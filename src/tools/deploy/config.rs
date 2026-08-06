use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};

use serde::Deserialize;
use thiserror::Error;

use crate::onepassword::{EnvironmentFileError, OpEnvironment};

pub const PROJECT_CONFIG: &str = ".kit/deploy.toml";
pub const SCHEMA_VERSION: u32 = 1;

/// One validated deployment plan loaded from disk.
#[derive(Clone, Debug)]
pub struct LoadedPlan {
    pub path: PathBuf,
    pub base_dir: PathBuf,
    pub plan: DeploymentPlan,
    pub environments: BTreeMap<String, OpEnvironment>,
}

/// The complete, versioned deploy configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentPlan {
    pub version: u32,
    pub targets: Vec<DeployTarget>,
}

/// One operator-selectable deployment outcome.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployTarget {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub source_roots: Vec<PathBuf>,
    pub env_file: Option<PathBuf>,
    pub artifact: Option<ArtifactSpec>,
    pub steps: Vec<DeployStep>,
    pub backend: Option<TargetBackend>,
    pub rollback: Option<RollbackStrategy>,
}

/// A typed, non-secret artifact identity emitted by a successful deployment.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ArtifactSpec {
    ContainerImage,
}

/// The external platform that owns a Target's Versions and rollback operation.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum TargetBackend {
    CloudflarePages { account_id: String, project: String, token_env: String },
}

/// One named, ordered unit of deployment work.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployStep {
    pub name: String,
    pub working_dir: Option<PathBuf>,
    pub action: DeployAction,
}

/// The executable shape of a Step.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum DeployAction {
    Command {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Shell {
        script: String,
    },
}

/// How a Target restores a selected journaled Version.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum RollbackStrategy {
    Steps { steps: Vec<DeployStep> },
    Redeploy,
}

impl DeployTarget {
    pub fn rollback_steps(&self) -> Option<Vec<DeployStep>> {
        match &self.rollback {
            Some(RollbackStrategy::Steps { steps }) => Some(steps.clone()),
            Some(RollbackStrategy::Redeploy) => Some(self.steps.clone()),
            None => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("no deploy config found; searched:\n{searched}\ncopy examples/deploy.toml to .kit/deploy.toml or pass --config <path>")]
    Missing { searched: String },
    #[error("read deploy config {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse deploy config {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid deploy config {}:\n{}", path.display(), issues.join("\n"))]
    Invalid { path: PathBuf, issues: Vec<String> },
    #[error("load env_file for Target '{target}': {source}")]
    Environment {
        target: String,
        #[source]
        source: EnvironmentFileError,
    },
}

impl LoadedPlan {
    pub fn load(
        explicit: Option<PathBuf>,
        project_dir: PathBuf,
        xdg_path: PathBuf,
    ) -> Result<Self, ConfigError> {
        let path = match explicit {
            Some(path) => path,
            None => {
                let project_path = project_dir.join(PROJECT_CONFIG);
                if project_path.is_file() {
                    project_path
                } else if xdg_path.is_file() {
                    xdg_path
                } else {
                    let searched = [project_path, xdg_path]
                        .into_iter()
                        .map(|path| format!("  - {}", path.display()))
                        .collect::<Vec<_>>()
                        .join("\n");
                    return Err(ConfigError::Missing { searched });
                }
            }
        };

        let raw = std::fs::read_to_string(&path)
            .map_err(|source| ConfigError::Read { path: path.clone(), source })?;
        let plan = parse_and_validate(&raw, path.clone())?;
        let base_dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| project_dir.clone());

        let mut environments = BTreeMap::new();
        for target in &plan.targets {
            let environment = match target.env_file.as_deref() {
                Some(env_file) => {
                    let resolved = if env_file.is_absolute() {
                        env_file.to_path_buf()
                    } else {
                        base_dir.join(env_file)
                    };
                    OpEnvironment::load(&resolved).map_err(|source| ConfigError::Environment {
                        target: target.id.clone(),
                        source,
                    })?
                }
                None => OpEnvironment::default(),
            };
            environments.insert(target.id.clone(), environment);
        }

        Ok(Self { path, base_dir, plan, environments })
    }
}

fn parse_and_validate(raw: &str, path: PathBuf) -> Result<DeploymentPlan, ConfigError> {
    let plan = toml::from_str::<DeploymentPlan>(raw)
        .map_err(|source| ConfigError::Parse { path: path.clone(), source })?;
    let issues = validate(&plan);
    if issues.is_empty() {
        Ok(plan)
    } else {
        Err(ConfigError::Invalid { path, issues })
    }
}

fn validate(plan: &DeploymentPlan) -> Vec<String> {
    let mut issues = Vec::new();
    if plan.version != SCHEMA_VERSION {
        issues
            .push(format!("- version {} is unsupported; expected {SCHEMA_VERSION}", plan.version));
    }
    if plan.targets.is_empty() {
        issues.push("- targets must contain at least one Target".to_owned());
    }

    let mut ids = HashSet::new();
    for (target_index, target) in plan.targets.iter().enumerate() {
        let target_label = format!("targets[{target_index}]");
        if !valid_id(&target.id) {
            issues.push(format!(
                "- {target_label}.id must use only letters, numbers, '.', '_' or '-'"
            ));
        } else if !ids.insert(target.id.as_str()) {
            issues.push(format!("- duplicate Target id '{}'", target.id));
        }
        if target.name.trim().is_empty() {
            issues.push(format!("- {target_label}.name must not be empty"));
        }
        if target.steps.is_empty() {
            issues.push(format!("- Target '{}' must contain at least one Step", target.id));
        }
        if target
            .env_file
            .as_ref()
            .is_some_and(|path| path.as_os_str().to_string_lossy().trim().is_empty())
        {
            issues.push(format!("- {target_label}.env_file must not be empty"));
        }
        for (source_index, source_root) in target.source_roots.iter().enumerate() {
            if source_root.as_os_str().to_string_lossy().trim().is_empty() {
                issues.push(format!(
                    "- {target_label}.source_roots[{source_index}] must not be empty"
                ));
            }
        }
        if target.artifact.is_some() && target.backend.is_some() {
            issues.push(format!(
                "- Target '{}' cannot declare both artifact capture and a platform Backend",
                target.id
            ));
        }

        if let Some(TargetBackend::CloudflarePages { account_id, project, token_env }) =
            &target.backend
        {
            for (field, value) in [
                ("account_id", account_id.as_str()),
                ("project", project.as_str()),
                ("token_env", token_env.as_str()),
            ] {
                if value.trim().is_empty() {
                    issues.push(format!("- {target_label}.backend.{field} must not be empty"));
                }
            }
            if target.rollback.is_some() {
                issues.push(format!(
                    "- Target '{}' uses a Cloudflare Pages backend; remove its local rollback strategy because Cloudflare owns rollback",
                    target.id
                ));
            }
        }

        for (step_index, step) in target.steps.iter().enumerate() {
            let step_label = format!("{target_label}.steps[{step_index}]");
            if step.name.trim().is_empty() {
                issues.push(format!("- {step_label}.name must not be empty"));
            }
            match &step.action {
                DeployAction::Command { program, .. } if program.trim().is_empty() => {
                    issues.push(format!("- {step_label}.action.program must not be empty"));
                }
                DeployAction::Shell { script } if script.trim().is_empty() => {
                    issues.push(format!("- {step_label}.action.script must not be empty"));
                }
                DeployAction::Command { .. } | DeployAction::Shell { .. } => {}
            }
        }

        match &target.rollback {
            Some(RollbackStrategy::Steps { steps }) if steps.is_empty() => {
                issues.push(format!(
                    "- Target '{}' rollback.steps must contain at least one Step",
                    target.id
                ));
            }
            Some(RollbackStrategy::Steps { steps }) => {
                validate_steps(steps, &format!("{target_label}.rollback.steps"), &mut issues);
            }
            Some(RollbackStrategy::Redeploy)
                if !target.steps.iter().any(|step| action_has_version_template(&step.action)) =>
            {
                issues.push(format!(
                    "- Target '{}' uses rollback type 'redeploy' but no deploy Action references {{{{version}}}} or {{{{ref}}}}",
                    target.id
                ));
            }
            Some(RollbackStrategy::Redeploy) | None => {}
        }
    }

    issues
}

fn validate_steps(steps: &[DeployStep], label: &str, issues: &mut Vec<String>) {
    for (step_index, step) in steps.iter().enumerate() {
        let step_label = format!("{label}[{step_index}]");
        if step.name.trim().is_empty() {
            issues.push(format!("- {step_label}.name must not be empty"));
        }
        match &step.action {
            DeployAction::Command { program, .. } if program.trim().is_empty() => {
                issues.push(format!("- {step_label}.action.program must not be empty"));
            }
            DeployAction::Shell { script } if script.trim().is_empty() => {
                issues.push(format!("- {step_label}.action.script must not be empty"));
            }
            DeployAction::Command { .. } | DeployAction::Shell { .. } => {}
        }
    }
}

fn action_has_version_template(action: &DeployAction) -> bool {
    let has_template = |value: &str| value.contains("{{version}}") || value.contains("{{ref}}");
    match action {
        DeployAction::Command { args, .. } => args.iter().any(|arg| has_template(arg)),
        DeployAction::Shell { script } => has_template(script),
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const VALID: &str = r#"
version = 1

[[targets]]
id = "preview"
name = "Preview"
description = "Publish a preview"
working_dir = "../app"
source_roots = ["../shared"]

[[targets.steps]]
name = "Build"
action = { type = "command", program = "builder", args = ["release"] }

[[targets.steps]]
name = "Publish"
working_dir = "scripts"
action = { type = "shell", script = "./publish.sh" }

[targets.rollback]
type = "steps"
steps = [{ name = "Restore", action = { type = "command", program = "restore", args = ["{{version}}"] } }]
"#;

    #[test]
    fn parses_typed_command_and_shell_actions() -> Result<(), ConfigError> {
        let plan = parse_and_validate(VALID, PathBuf::from("deploy.toml"))?;

        assert_eq!(plan.version, SCHEMA_VERSION);
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].source_roots, [PathBuf::from("../shared")]);
        assert!(matches!(
            &plan.targets[0].steps[0].action,
            DeployAction::Command { program, args }
                if program == "builder" && args == &["release"]
        ));
        assert!(matches!(
            &plan.targets[0].steps[1].action,
            DeployAction::Shell { script } if script == "./publish.sh"
        ));
        Ok(())
    }

    #[test]
    fn parses_cloudflare_pages_backend() -> Result<(), ConfigError> {
        let raw = VALID.replace(
            "[targets.rollback]\ntype = \"steps\"\nsteps = [{ name = \"Restore\", action = { type = \"command\", program = \"restore\", args = [\"{{version}}\"] } }]",
            "[targets.backend]\ntype = \"cloudflare-pages\"\naccount_id = \"<account-id>\"\nproject = \"<pages-project>\"\ntoken_env = \"CLOUDFLARE_API_TOKEN\"",
        );
        let plan = parse_and_validate(&raw, PathBuf::from("deploy.toml"))?;

        assert!(matches!(
            &plan.targets[0].backend,
            Some(TargetBackend::CloudflarePages { account_id, project, token_env })
                if account_id == "<account-id>"
                    && project == "<pages-project>"
                    && token_env == "CLOUDFLARE_API_TOKEN"
        ));
        Ok(())
    }

    #[test]
    fn rejects_incomplete_cloudflare_pages_backend() {
        let raw = VALID.replace(
            "[targets.rollback]\ntype = \"steps\"\nsteps = [{ name = \"Restore\", action = { type = \"command\", program = \"restore\", args = [\"{{version}}\"] } }]",
            "[targets.backend]\ntype = \"cloudflare-pages\"\naccount_id = \" \"\nproject = \"\"\ntoken_env = \" \"",
        );
        let error = parse_and_validate(&raw, PathBuf::from("deploy.toml"))
            .expect_err("incomplete Backend must fail");
        let message = error.to_string();

        assert!(message.contains("backend.account_id must not be empty"));
        assert!(message.contains("backend.project must not be empty"));
        assert!(message.contains("backend.token_env must not be empty"));
    }

    #[test]
    fn rejects_unknown_fields() {
        let raw = VALID.replace("name = \"Preview\"", "name = \"Preview\"\nhost = \"hidden\"");
        let error = parse_and_validate(&raw, PathBuf::from("deploy.toml"))
            .expect_err("unknown fields must fail");

        assert!(matches!(error, ConfigError::Parse { .. }));
        assert!(error.to_string().contains("unknown field `host`"));
    }

    #[test]
    fn reports_all_structural_validation_errors() {
        let raw = r#"
version = 2
[[targets]]
id = "bad id"
name = " "
steps = []

[[targets]]
id = "duplicate"
name = "One"
[[targets.steps]]
name = ""
action = { type = "command", program = "" }

[[targets]]
id = "duplicate"
name = "Two"
[[targets.steps]]
name = "Deploy"
action = { type = "shell", script = " " }
"#;
        let error = parse_and_validate(raw, PathBuf::from("deploy.toml"))
            .expect_err("invalid plan must fail");
        let message = error.to_string();

        assert!(message.contains("version 2 is unsupported"));
        assert!(message.contains("id must use only"));
        assert!(message.contains("name must not be empty"));
        assert!(message.contains("at least one Step"));
        assert!(message.contains("duplicate Target id 'duplicate'"));
        assert!(message.contains("action.program must not be empty"));
        assert!(message.contains("action.script must not be empty"));
    }

    #[test]
    fn project_config_precedes_xdg_config() -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!("kit-deploy-config-{}", std::process::id()));
        let project = root.join("project");
        let project_config = project.join(PROJECT_CONFIG);
        let xdg_config = root.join("xdg/deploy.toml");
        std::fs::create_dir_all(project_config.parent().ok_or("project config has no parent")?)?;
        std::fs::create_dir_all(xdg_config.parent().ok_or("xdg config has no parent")?)?;
        std::fs::write(&project_config, VALID)?;
        std::fs::write(&xdg_config, VALID.replace("Preview", "XDG"))?;

        let loaded = LoadedPlan::load(None, project, xdg_config)?;
        let _ = std::fs::remove_dir_all(root);

        assert_eq!(loaded.path, project_config);
        assert_eq!(loaded.plan.targets[0].name, "Preview");
        Ok(())
    }

    #[test]
    fn loads_env_file_relative_to_config_directory() -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!("kit-deploy-env-{}", std::process::id()));
        let config_dir = root.join("config");
        let config_path = config_dir.join("deploy.toml");
        let env_path = config_dir.join("secrets.env");
        std::fs::create_dir_all(&config_dir)?;
        std::fs::write(
            &config_path,
            VALID.replace(
                "working_dir = \"../app\"",
                "working_dir = \"../app\"\nenv_file = \"secrets.env\"",
            ),
        )?;
        std::fs::write(&env_path, "KIT_DEPLOY_RELATIVE_ENV_TEST=loaded")?;

        let loaded =
            LoadedPlan::load(Some(config_path), root.clone(), root.join("unused-xdg-deploy.toml"))?;
        let environment = loaded.environments.get("preview").expect("preview environment loaded");
        let value = environment
            .child_values()
            .find(|(name, _)| *name == "KIT_DEPLOY_RELATIVE_ENV_TEST")
            .map(|(_, value)| value);
        let _ = std::fs::remove_dir_all(root);

        assert_eq!(loaded.plan.targets[0].env_file.as_deref(), Some(Path::new("secrets.env")));
        assert_eq!(value, Some("loaded"));
        Ok(())
    }

    #[test]
    fn missing_config_lists_every_resolution_path() {
        let root =
            std::env::temp_dir().join(format!("kit-deploy-missing-config-{}", std::process::id()));
        let project = root.join("project");
        let xdg = root.join("state/deploy.toml");
        let error = LoadedPlan::load(None, project.clone(), xdg.clone())
            .expect_err("missing config must fail");
        let message = error.to_string();

        assert!(message.contains(&project.join(PROJECT_CONFIG).display().to_string()));
        assert!(message.contains(&xdg.display().to_string()));
        assert!(message.contains("examples/deploy.toml"));
    }

    #[test]
    fn redeploy_rollback_requires_a_version_template() {
        let raw = VALID.replace(
            "type = \"steps\"\nsteps = [{ name = \"Restore\", action = { type = \"command\", program = \"restore\", args = [\"{{version}}\"] } }]",
            "type = \"redeploy\"",
        );
        let error = parse_and_validate(&raw, PathBuf::from("deploy.toml"))
            .expect_err("unpinned redeploy rollback must fail");

        assert!(error.to_string().contains("no deploy Action references"));
    }
}
