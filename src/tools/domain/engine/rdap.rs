use std::{
    fs,
    path::PathBuf,
    sync::OnceLock,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use tokio::time::sleep;

use super::{DomainRecord, Outcome};

const BOOTSTRAP_URL: &str = "https://data.iana.org/rdap/dns.json";
const CACHE_FILE: &str = "domain-rdap-bootstrap.json";
const CACHE_TTL: Duration = Duration::from_secs(60 * 60 * 24);
const RDAP_BACKOFF: Duration = Duration::from_millis(1200);

static RDAP_BOOTSTRAP: OnceLock<Bootstrap> = OnceLock::new();

pub(crate) enum BootstrapLoad {
    Ready(&'static Bootstrap),
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bootstrap {
    services: Vec<BootstrapService>,
}

impl Bootstrap {
    pub fn endpoints_for_domain(&self, domain: &str) -> Vec<String> {
        let labels = domain
            .trim_end_matches('.')
            .to_lowercase()
            .split('.')
            .filter(|label| !label.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();

        for start in 0..labels.len() {
            let suffix = labels[start..].join(".");
            let mut endpoints = Vec::new();

            for service in self
                .services
                .iter()
                .filter(|service| service.labels.iter().any(|label| label == &suffix))
            {
                for endpoint in &service.endpoints {
                    push_unique(&mut endpoints, endpoint.clone());
                }
            }

            if endpoints.is_empty() {
                continue;
            }

            endpoints.sort_by_key(|endpoint| !is_https(endpoint));
            return endpoints;
        }

        Vec::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BootstrapService {
    labels: Vec<String>,
    endpoints: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct BootstrapDocument {
    services: Vec<BootstrapServiceEntry>,
}

#[derive(Debug, Deserialize)]
struct BootstrapServiceEntry(Vec<String>, Vec<String>);

#[derive(Debug)]
pub(crate) struct RdapCheck {
    pub(crate) outcome: Option<Outcome>,
    pub(crate) record: Option<DomainRecord>,
    pub(crate) evidence: String,
}

pub(crate) async fn load(client: &Client) -> BootstrapLoad {
    if let Some(bootstrap) = RDAP_BOOTSTRAP.get() {
        return BootstrapLoad::Ready(bootstrap);
    }

    match load_uncached(client).await {
        Ok(bootstrap) => {
            let _ = RDAP_BOOTSTRAP.set(bootstrap);
            match RDAP_BOOTSTRAP.get() {
                Some(bootstrap) => BootstrapLoad::Ready(bootstrap),
                None => BootstrapLoad::Failed("RDAP bootstrap initialized but unavailable".into()),
            }
        }
        Err(error) => BootstrapLoad::Failed(format!("{error:#}")),
    }
}

async fn load_uncached(client: &Client) -> Result<Bootstrap> {
    if let Some(raw) = read_fresh_cache()? {
        if let Ok(bootstrap) = parse_bootstrap(&raw) {
            return Ok(bootstrap);
        }
    }

    let raw = client
        .get(BOOTSTRAP_URL)
        .send()
        .await
        .context("fetch IANA RDAP bootstrap")?
        .error_for_status()
        .context("IANA RDAP bootstrap status")?
        .text()
        .await
        .context("read IANA RDAP bootstrap body")?;

    let bootstrap = parse_bootstrap(&raw)?;
    let _ = fs::write(cache_path(), raw);
    Ok(bootstrap)
}

fn read_fresh_cache() -> Result<Option<String>> {
    let path = cache_path();
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read RDAP bootstrap cache metadata"),
    };

    if !is_fresh(metadata.modified().ok()) {
        return Ok(None);
    }

    fs::read_to_string(path).map(Some).context("read RDAP bootstrap cache")
}

fn is_fresh(modified: Option<SystemTime>) -> bool {
    modified.and_then(|modified| modified.elapsed().ok()).is_some_and(|age| age <= CACHE_TTL)
}

fn cache_path() -> PathBuf {
    std::env::temp_dir().join(CACHE_FILE)
}

fn parse_bootstrap(raw: &str) -> Result<Bootstrap> {
    let document =
        serde_json::from_str::<BootstrapDocument>(raw).context("parse RDAP bootstrap")?;
    let services = document
        .services
        .into_iter()
        .filter_map(|entry| {
            let labels = entry
                .0
                .into_iter()
                .map(|label| label.trim().trim_start_matches('.').to_lowercase())
                .filter(|label| !label.is_empty())
                .collect::<Vec<_>>();
            let endpoints = entry
                .1
                .into_iter()
                .map(|endpoint| endpoint.trim().trim_end_matches('/').to_owned())
                .filter(|endpoint| !endpoint.is_empty())
                .collect::<Vec<_>>();

            (!labels.is_empty() && !endpoints.is_empty())
                .then_some(BootstrapService { labels, endpoints })
        })
        .collect();

    Ok(Bootstrap { services })
}

pub(crate) async fn check(client: &Client, domain: &str, endpoints: &[String]) -> RdapCheck {
    let mut evidence = Vec::new();

    for endpoint in endpoints {
        match check_endpoint(client, domain, endpoint).await {
            EndpointCheck::Conclusive { outcome, record } => {
                return RdapCheck {
                    evidence: outcome.evidence.clone(),
                    outcome: Some(outcome),
                    record,
                };
            }
            EndpointCheck::Inconclusive(detail) => evidence.push(detail),
        }
    }

    RdapCheck {
        outcome: None,
        record: None,
        evidence: if evidence.is_empty() {
            "registry RDAP unavailable".to_owned()
        } else {
            evidence.join("; ")
        },
    }
}

async fn check_endpoint(client: &Client, domain: &str, endpoint: &str) -> EndpointCheck {
    let mut last_detail = None;

    for attempt in 0..=1 {
        match request_endpoint(client, domain, endpoint).await {
            conclusive @ EndpointCheck::Conclusive { .. } => return conclusive,
            EndpointCheck::Inconclusive(detail) if attempt == 0 && is_retryable_detail(&detail) => {
                last_detail = Some(detail);
                sleep(RDAP_BACKOFF).await;
            }
            EndpointCheck::Inconclusive(detail) => return EndpointCheck::Inconclusive(detail),
        }
    }

    EndpointCheck::Inconclusive(last_detail.unwrap_or_else(|| "registry RDAP retried".to_owned()))
}

async fn request_endpoint(client: &Client, domain: &str, endpoint: &str) -> EndpointCheck {
    let url = format!("{}/domain/{domain}", endpoint.trim_end_matches('/'));
    let response = match client.get(&url).header("accept", "application/rdap+json").send().await {
        Ok(response) => response,
        Err(error) => {
            return EndpointCheck::Inconclusive(format!(
                "registry RDAP request failed at {endpoint}: {error}"
            ));
        }
    };

    let status = response.status();
    match status {
        StatusCode::NOT_FOUND => EndpointCheck::Conclusive {
            outcome: Outcome::available("registry RDAP 404"),
            record: None,
        },
        StatusCode::OK => {
            let body = response.text().await.unwrap_or_default();
            let record = parse_record(&body);
            let evidence = match record.as_ref().and_then(|record| record.expires_on.as_deref()) {
                Some(expiration) => format!("registry RDAP 200, exp {expiration}"),
                None => "registry RDAP 200".to_owned(),
            };
            EndpointCheck::Conclusive { outcome: Outcome::taken(evidence), record }
        }
        status => {
            EndpointCheck::Inconclusive(format!("registry RDAP {} at {endpoint}", status.as_u16()))
        }
    }
}

fn is_retryable_detail(detail: &str) -> bool {
    [" 429 ", " 500 ", " 502 ", " 503 "].iter().any(|needle| detail.contains(needle))
}

#[derive(Debug)]
enum EndpointCheck {
    Conclusive { outcome: Outcome, record: Option<DomainRecord> },
    Inconclusive(String),
}

#[derive(Debug, Deserialize)]
struct RdapDomain {
    #[serde(default)]
    events: Vec<RdapEvent>,
    #[serde(default)]
    status: Vec<String>,
    #[serde(default)]
    nameservers: Vec<RdapNameserver>,
    #[serde(default)]
    entities: Vec<RdapEntity>,
}

#[derive(Debug, Deserialize)]
struct RdapEvent {
    #[serde(rename = "eventAction")]
    event_action: String,
    #[serde(rename = "eventDate")]
    event_date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RdapNameserver {
    #[serde(rename = "ldhName")]
    ldh_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RdapEntity {
    #[serde(default)]
    roles: Vec<String>,
    #[serde(rename = "vcardArray")]
    vcard_array: Option<Value>,
}

fn parse_record(body: &str) -> Option<DomainRecord> {
    let domain = serde_json::from_str::<RdapDomain>(body).ok()?;

    let statuses = domain
        .status
        .iter()
        .map(|status| status.trim().to_lowercase())
        .filter(|status| !status.is_empty())
        .collect::<Vec<_>>();

    let nameservers = domain
        .nameservers
        .iter()
        .filter_map(|nameserver| nameserver.ldh_name.as_deref())
        .map(|nameserver| nameserver.trim_end_matches('.').to_lowercase())
        .filter(|nameserver| !nameserver.is_empty())
        .collect::<Vec<_>>();

    Some(DomainRecord {
        registrar: registrar_name(&domain.entities),
        created_on: event_date(&domain.events, "registration"),
        expires_on: event_date(&domain.events, "expiration"),
        statuses,
        nameservers,
    })
}

fn event_date(events: &[RdapEvent], action: &str) -> Option<String> {
    events
        .iter()
        .find(|event| event.event_action.eq_ignore_ascii_case(action))?
        .event_date
        .as_deref()?
        .get(..10)
        .map(ToOwned::to_owned)
}

fn registrar_name(entities: &[RdapEntity]) -> Option<String> {
    let registrar = entities
        .iter()
        .find(|entity| entity.roles.iter().any(|role| role.eq_ignore_ascii_case("registrar")))?;
    vcard_full_name(registrar.vcard_array.as_ref()?)
}

fn vcard_full_name(vcard: &Value) -> Option<String> {
    let entries = vcard.as_array()?.get(1)?.as_array()?;
    for entry in entries {
        let Some(fields) = entry.as_array() else {
            continue;
        };
        let is_full_name = fields
            .first()
            .and_then(Value::as_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("fn"));
        if !is_full_name {
            continue;
        }
        if let Some(value) = fields.get(3).and_then(Value::as_str) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

fn is_https(endpoint: &str) -> bool {
    endpoint.get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
}

fn push_unique(out: &mut Vec<String>, value: String) {
    if !out.iter().any(|existing| existing == &value) {
        out.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iana_bootstrap_to_tld_endpoint_map() -> Result<()> {
        let bootstrap = parse_bootstrap(
            r#"{
                "services": [
                    [["com", "net"], ["https://rdap.example/", "http://rdap.example/"]],
                    [["ai"], ["https://rdap.ai.example"]]
                ]
            }"#,
        )?;

        assert_eq!(
            bootstrap.endpoints_for_domain("modkit.com"),
            vec!["https://rdap.example".to_owned(), "http://rdap.example".to_owned()]
        );
        assert_eq!(
            bootstrap.endpoints_for_domain("modkit.ai"),
            vec!["https://rdap.ai.example".to_owned()]
        );
        assert!(bootstrap.endpoints_for_domain("modkit.invalid").is_empty());

        Ok(())
    }

    #[test]
    fn chooses_longest_label_match() -> Result<()> {
        let bootstrap = parse_bootstrap(
            r#"{
                "services": [
                    [["foo"], ["https://rdap.foo"]],
                    [["bar.foo"], ["https://rdap.bar.foo"]]
                ]
            }"#,
        )?;

        assert_eq!(
            bootstrap.endpoints_for_domain("name.bar.foo"),
            vec!["https://rdap.bar.foo".to_owned()]
        );

        Ok(())
    }

    #[test]
    fn rejects_invalid_bootstrap_json() {
        let error = parse_bootstrap("not json").expect_err("invalid bootstrap must fail");

        assert!(format!("{error:#}").contains("parse RDAP bootstrap"));
    }

    #[test]
    fn parses_record_fields_from_rdap_body() {
        let body = r#"{
            "status": ["client transfer prohibited", "Redemption Period"],
            "events": [
                { "eventAction": "registration", "eventDate": "2020-01-01T00:00:00Z" },
                { "eventAction": "expiration", "eventDate": "2030-01-01T00:00:00Z" }
            ],
            "nameservers": [
                { "ldhName": "NS1.EXAMPLE.COM." },
                { "ldhName": "ns2.example.com" }
            ],
            "entities": [
                {
                    "roles": ["registrar"],
                    "vcardArray": ["vcard", [
                        ["version", {}, "text", "4.0"],
                        ["fn", {}, "text", "Example Registrar, LLC"]
                    ]]
                }
            ]
        }"#;

        let record = parse_record(body).expect("record parses");
        assert_eq!(record.created_on.as_deref(), Some("2020-01-01"));
        assert_eq!(record.expires_on.as_deref(), Some("2030-01-01"));
        assert_eq!(record.registrar.as_deref(), Some("Example Registrar, LLC"));
        assert_eq!(
            record.statuses,
            vec!["client transfer prohibited".to_owned(), "redemption period".to_owned()]
        );
        assert_eq!(
            record.nameservers,
            vec!["ns1.example.com".to_owned(), "ns2.example.com".to_owned()]
        );
    }
}
