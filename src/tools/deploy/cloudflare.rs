use reqwest::{Client, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;

use crate::onepassword::{OpClient, OpEnvironment, OpError, SecretBytes};

use super::config::{DeployTarget, TargetBackend};

const API_ROOT: &str = "https://api.cloudflare.com/client/v4";

pub struct CloudflarePagesClient {
    http: Client,
    account_id: String,
    project: String,
    token: SecretBytes,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CloudflareDeployment {
    pub id: String,
    pub short_id: String,
    pub created_on: String,
    pub environment: CloudflareEnvironment,
    pub url: String,
    pub latest_stage: Option<CloudflareStage>,
    pub deployment_trigger: Option<CloudflareDeploymentTrigger>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CloudflareEnvironment {
    Production,
    Preview,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CloudflareStage {
    pub status: CloudflareStageStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CloudflareStageStatus {
    Success,
    Idle,
    Active,
    Failure,
    Canceled,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CloudflareDeploymentTrigger {
    pub metadata: Option<CloudflareDeploymentMetadata>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CloudflareDeploymentMetadata {
    pub commit_hash: Option<String>,
    pub branch: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CloudflareProject {
    pub production_branch: String,
    pub canonical_deployment: Option<CanonicalDeployment>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CanonicalDeployment {
    pub id: String,
}

/// The version history plus the pointers needed to render "which one is live".
#[derive(Clone, Debug)]
pub struct CloudflareVersions {
    pub deployments: Vec<CloudflareDeployment>,
    pub live_id: Option<String>,
    pub production_branch: String,
}

#[derive(Debug, Error)]
pub enum CloudflareError {
    #[error(
        "Cloudflare API token is unavailable; add '{token_env}=op://<vault>/<item>/<field>' to this Target's env_file"
    )]
    MissingToken { token_env: String },
    #[error("resolve Cloudflare API token from '{token_env}': {source}")]
    ResolveToken {
        token_env: String,
        #[source]
        source: OpError,
    },
    #[error("build Cloudflare API client: {0}")]
    BuildClient(#[source] reqwest::Error),
    #[error("build Cloudflare API URL")]
    InvalidUrl,
    #[error("Cloudflare API request failed: {0}")]
    Request(#[source] reqwest::Error),
    #[error("Cloudflare API returned HTTP {status}: {message}")]
    Http { status: StatusCode, message: String },
    #[error("Cloudflare API rejected the request: {0}")]
    Api(String),
    #[error("decode Cloudflare API response: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("Cloudflare API response did not contain a result")]
    MissingResult,
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    success: bool,
    result: Option<T>,
    #[serde(default)]
    errors: Vec<ApiError>,
    result_info: Option<ResultInfo>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct ResultInfo {
    total_pages: Option<u32>,
}

#[derive(Serialize)]
struct RollbackRequest {}

impl CloudflarePagesClient {
    pub async fn for_target(
        target: &DeployTarget,
        environment: &OpEnvironment,
        op: &OpClient,
    ) -> Result<Option<Self>, CloudflareError> {
        let Some(TargetBackend::CloudflarePages { account_id, project, token_env, .. }) =
            &target.backend
        else {
            return Ok(None);
        };
        let http = Client::builder()
            .user_agent(concat!("kit/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(CloudflareError::BuildClient)?;
        let reference = environment
            .reference(token_env)
            .ok_or_else(|| CloudflareError::MissingToken { token_env: token_env.to_owned() })?;
        let token = op.read_reference(reference).await.map_err(|source| {
            CloudflareError::ResolveToken { token_env: token_env.to_owned(), source }
        })?;
        Ok(Some(Self {
            http,
            account_id: account_id.to_owned(),
            project: project.to_owned(),
            token,
        }))
    }

    pub async fn list_deployments(&self) -> Result<Vec<CloudflareDeployment>, CloudflareError> {
        let token = self.token.as_str();
        let first = self.deployment_page(token, None).await?;
        let total_pages =
            first.result_info.as_ref().and_then(|info| info.total_pages).unwrap_or(1).max(1);
        let mut deployments = first.result.ok_or(CloudflareError::MissingResult)?;
        for page in 2..=total_pages {
            let response = self.deployment_page(token, Some(page)).await?;
            deployments.extend(response.result.ok_or(CloudflareError::MissingResult)?);
        }
        deployments.sort_by(|left, right| right.created_on.cmp(&left.created_on));
        Ok(deployments)
    }

    async fn deployment_page(
        &self,
        token: &str,
        page: Option<u32>,
    ) -> Result<ApiResponse<Vec<CloudflareDeployment>>, CloudflareError> {
        let url = self.deployment_page_url(page)?;
        let response =
            self.http.get(url).bearer_auth(token).send().await.map_err(CloudflareError::Request)?;
        decode(response).await
    }

    pub async fn rollback(
        &self,
        deployment_id: &str,
    ) -> Result<CloudflareDeployment, CloudflareError> {
        let token = self.token.as_str();
        let mut url = self.deployments_url()?;
        url.path_segments_mut()
            .map_err(|()| CloudflareError::InvalidUrl)?
            .push(deployment_id)
            .push("rollback");
        let response = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(&RollbackRequest {})
            .send()
            .await
            .map_err(CloudflareError::Request)?;
        let parsed: ApiResponse<CloudflareDeployment> = decode(response).await?;
        parsed.result.ok_or(CloudflareError::MissingResult)
    }

    pub async fn load_versions(&self) -> Result<CloudflareVersions, CloudflareError> {
        let (deployments, project) = tokio::try_join!(self.list_deployments(), self.get_project())?;
        Ok(CloudflareVersions {
            deployments,
            live_id: project.canonical_deployment.map(|canonical| canonical.id),
            production_branch: project.production_branch,
        })
    }

    pub async fn get_project(&self) -> Result<CloudflareProject, CloudflareError> {
        let token = self.token.as_str();
        let response = self
            .http
            .get(self.project_url()?)
            .bearer_auth(token)
            .send()
            .await
            .map_err(CloudflareError::Request)?;
        let parsed: ApiResponse<CloudflareProject> = decode(response).await?;
        parsed.result.ok_or(CloudflareError::MissingResult)
    }

    pub async fn delete_deployment(&self, deployment_id: &str) -> Result<(), CloudflareError> {
        let token = self.token.as_str();
        let mut url = self.deployments_url()?;
        url.path_segments_mut().map_err(|()| CloudflareError::InvalidUrl)?.push(deployment_id);
        url.query_pairs_mut().append_pair("force", "true");
        let response = self
            .http
            .delete(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(CloudflareError::Request)?;
        let _: ApiResponse<serde_json::Value> = decode(response).await?;
        Ok(())
    }

    fn deployments_url(&self) -> Result<Url, CloudflareError> {
        let mut url = Url::parse(API_ROOT).map_err(|_| CloudflareError::InvalidUrl)?;
        url.path_segments_mut().map_err(|()| CloudflareError::InvalidUrl)?.extend([
            "accounts",
            self.account_id.as_str(),
            "pages",
            "projects",
            self.project.as_str(),
            "deployments",
        ]);
        Ok(url)
    }

    fn project_url(&self) -> Result<Url, CloudflareError> {
        let mut url = Url::parse(API_ROOT).map_err(|_| CloudflareError::InvalidUrl)?;
        url.path_segments_mut().map_err(|()| CloudflareError::InvalidUrl)?.extend([
            "accounts",
            self.account_id.as_str(),
            "pages",
            "projects",
            self.project.as_str(),
        ]);
        Ok(url)
    }

    fn deployment_page_url(&self, page: Option<u32>) -> Result<Url, CloudflareError> {
        let mut url = self.deployments_url()?;
        if let Some(page) = page {
            url.query_pairs_mut().append_pair("page", &page.to_string());
        }
        Ok(url)
    }
}

impl CloudflareDeployment {
    pub fn commit_hash(&self) -> Option<&str> {
        self.deployment_trigger
            .as_ref()
            .and_then(|trigger| trigger.metadata.as_ref())
            .and_then(|metadata| metadata.commit_hash.as_deref())
    }

    pub fn branch(&self) -> Option<&str> {
        self.deployment_trigger
            .as_ref()
            .and_then(|trigger| trigger.metadata.as_ref())
            .and_then(|metadata| metadata.branch.as_deref())
    }

    pub fn is_production(&self) -> bool {
        self.environment == CloudflareEnvironment::Production
    }

    pub fn succeeded(&self) -> bool {
        self.latest_stage
            .as_ref()
            .is_some_and(|stage| stage.status == CloudflareStageStatus::Success)
    }

    pub fn rollback_eligible(&self) -> bool {
        self.is_production() && self.succeeded()
    }
}

async fn decode<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<ApiResponse<T>, CloudflareError> {
    let status = response.status();
    let body = response.text().await.map_err(CloudflareError::Request)?;
    let parsed = serde_json::from_str::<ApiResponse<T>>(&body).map_err(CloudflareError::Decode)?;
    let message = api_error_message(&parsed.errors);
    if !status.is_success() {
        return Err(CloudflareError::Http {
            status,
            message: if message.is_empty() { "request failed".to_owned() } else { message },
        });
    }
    if !parsed.success {
        return Err(CloudflareError::Api(if message.is_empty() {
            "request was not successful".to_owned()
        } else {
            message
        }));
    }
    Ok(parsed)
}

fn api_error_message(errors: &[ApiError]) -> String {
    errors
        .iter()
        .map(|error| format!("{}: {}", error.code, error.message))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        onepassword::parse_dotenv,
        tools::deploy::config::{DeployAction, DeployStep},
    };

    fn pages_target() -> DeployTarget {
        DeployTarget {
            id: "pages".to_owned(),
            name: "Pages".to_owned(),
            description: None,
            working_dir: None,
            source_roots: Vec::new(),
            env_file: None,
            steps: vec![DeployStep {
                name: "Publish".to_owned(),
                working_dir: None,
                action: DeployAction::Command {
                    program: "<your-pages-deploy-command>".to_owned(),
                    args: Vec::new(),
                },
            }],
            backend: Some(TargetBackend::CloudflarePages {
                account_id: "<account-id>".to_owned(),
                project: "<pages-project>".to_owned(),
                token_env: "KIT_DEPLOY_CLOUDFLARE_TOKEN_FROM_FILE_TEST".to_owned(),
            }),
            rollback: None,
        }
    }

    fn fake_op() -> OpClient {
        OpClient::with_executable(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-op"),
        )
    }

    #[test]
    fn parses_cloudflare_deployment_response_with_extra_fields() -> Result<(), serde_json::Error> {
        let fixture = r#"{
            "success": true,
            "errors": [],
            "messages": [],
            "result": [{
                "id": "deployment-id-placeholder",
                "short_id": "short-id",
                "created_on": "2026-01-02T03:04:05Z",
                "environment": "production",
                "url": "https://placeholder.pages.dev",
                "latest_stage": { "name": "deploy", "status": "success" },
                "deployment_trigger": {
                    "type": "ad_hoc",
                    "metadata": { "commit_hash": "abcdef123456", "branch": "main" }
                },
                "future_field": true
            }],
            "result_info": { "page": 1, "total_pages": 1 }
        }"#;

        let response = serde_json::from_str::<ApiResponse<Vec<CloudflareDeployment>>>(fixture)?;
        let deployment = response.result.as_ref().and_then(|items| items.first());

        assert!(response.success);
        assert!(deployment.is_some_and(CloudflareDeployment::rollback_eligible));
        assert_eq!(deployment.and_then(CloudflareDeployment::commit_hash), Some("abcdef123456"));
        Ok(())
    }

    #[tokio::test]
    async fn resolves_backend_token_from_target_environment(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let environment =
            parse_dotenv("KIT_DEPLOY_CLOUDFLARE_TOKEN_FROM_FILE_TEST=op://Tests/read/success")?;
        let client = CloudflarePagesClient::for_target(&pages_target(), &environment, &fake_op())
            .await?
            .ok_or("Pages Target did not create a Cloudflare client")?;

        assert_eq!(client.token.as_str(), "fixture-secret-value");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_a_literal_cloudflare_token_as_an_unmasked_secret_source(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let environment = parse_dotenv("KIT_DEPLOY_CLOUDFLARE_TOKEN_FROM_FILE_TEST=file-token")?;
        let result =
            CloudflarePagesClient::for_target(&pages_target(), &environment, &fake_op()).await;
        let Err(error) = result else { panic!("literal Cloudflare token unexpectedly succeeded") };

        assert!(matches!(error, CloudflareError::MissingToken { .. }));
        Ok(())
    }

    #[tokio::test]
    async fn resolves_only_the_backend_token_and_ignores_unrelated_references(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let environment = parse_dotenv(
            "KIT_DEPLOY_CLOUDFLARE_TOKEN_FROM_FILE_TEST=op://Tests/read/success\n\
             CLOUDFLARE_ACCOUNT_ID=op://Tests/read/failure",
        )?;
        let client = CloudflarePagesClient::for_target(&pages_target(), &environment, &fake_op())
            .await?
            .ok_or("Pages Target did not create a Cloudflare client")?;

        assert_eq!(client.token.as_str(), "fixture-secret-value");
        assert_eq!(client.account_id, "<account-id>");
        Ok(())
    }

    #[tokio::test]
    async fn first_page_uses_platform_defaults_and_later_pages_only_select_page(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let environment =
            parse_dotenv("KIT_DEPLOY_CLOUDFLARE_TOKEN_FROM_FILE_TEST=op://Tests/read/success")?;
        let client = CloudflarePagesClient::for_target(&pages_target(), &environment, &fake_op())
            .await?
            .ok_or("Pages Target did not create a Cloudflare client")?;

        assert_eq!(client.deployment_page_url(None)?.query(), None);
        assert_eq!(client.deployment_page_url(Some(2))?.query(), Some("page=2"));
        Ok(())
    }
}
