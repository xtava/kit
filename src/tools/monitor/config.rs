use std::{collections::HashSet, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

pub const PROJECT_CONFIG: &str = ".kit/monitor.toml";
pub const SCHEMA_VERSION: u32 = 1;

pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: MonitorConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorConfig {
    pub version: u32,
    pub default_environment: String,
    pub environments: Vec<EnvironmentConfig>,
    pub sources: Vec<SourceConfig>,
    pub services: Vec<ServiceConfig>,
    #[serde(default)]
    pub costs: Vec<CostConfig>,
    #[serde(default)]
    pub limits: LimitsConfig,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentConfig {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub monthly_budget_usd: Option<f64>,
    #[serde(default)]
    pub required_sources: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SourceConfig {
    HttpHealth(HttpHealthSource),
    Prometheus(PrometheusSource),
    Loki(LokiSource),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpHealthSource {
    pub id: String,
    pub name: String,
    pub environment: String,
    pub url: Url,
    #[serde(default = "default_expected_status")]
    pub expected_status: u16,
    #[serde(default)]
    pub auth: Option<SourceAuth>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrometheusSource {
    pub id: String,
    pub name: String,
    pub environment: String,
    pub url: Url,
    #[serde(default)]
    pub auth: Option<SourceAuth>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LokiSource {
    pub id: String,
    pub name: String,
    pub environment: String,
    pub url: Url,
    pub default_query: String,
    #[serde(default)]
    pub auth: Option<SourceAuth>,
    #[serde(default)]
    pub allow_unstructured: bool,
    #[serde(default)]
    pub sensitive_fields: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum SourceAuth {
    Bearer { token_env: String },
    Basic { username: String, password_env: String },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub id: String,
    pub name: String,
    pub environment: String,
    pub health_source: String,
    #[serde(default)]
    pub metrics: Vec<MetricConfig>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetricUnit {
    Percent,
    Bytes,
    Milliseconds,
    Count,
    RequestsPerSecond,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricConfig {
    pub id: String,
    pub name: String,
    pub source: String,
    pub query: String,
    pub unit: MetricUnit,
    #[serde(default)]
    pub warning_above: Option<f64>,
    #[serde(default)]
    pub critical_above: Option<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CostConfidence {
    Billed,
    Estimated,
    Fixed,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostConfig {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub environment: String,
    #[serde(default)]
    pub service: Option<String>,
    pub monthly_usd: f64,
    pub confidence: CostConfidence,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    pub request_timeout_ms: u64,
    pub max_log_events: usize,
    pub max_lookback_hours: u64,
    pub stale_after_secs: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            request_timeout_ms: 8_000,
            max_log_events: 500,
            max_lookback_hours: 24 * 7,
            stale_after_secs: 90,
        }
    }
}

impl LimitsConfig {
    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.request_timeout_ms)
    }
}

impl SourceConfig {
    pub fn id(&self) -> &str {
        match self {
            Self::HttpHealth(source) => &source.id,
            Self::Prometheus(source) => &source.id,
            Self::Loki(source) => &source.id,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::HttpHealth(source) => &source.name,
            Self::Prometheus(source) => &source.name,
            Self::Loki(source) => &source.name,
        }
    }

    pub fn environment(&self) -> &str {
        match self {
            Self::HttpHealth(source) => &source.environment,
            Self::Prometheus(source) => &source.environment,
            Self::Loki(source) => &source.environment,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::HttpHealth(_) => "http-health",
            Self::Prometheus(_) => "prometheus",
            Self::Loki(_) => "loki",
        }
    }

    pub fn auth(&self) -> Option<&SourceAuth> {
        match self {
            Self::HttpHealth(source) => source.auth.as_ref(),
            Self::Prometheus(source) => source.auth.as_ref(),
            Self::Loki(source) => source.auth.as_ref(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(
        "no monitor config found; searched:\n{searched}\ncreate .kit/monitor.toml or pass --config <path>"
    )]
    Missing { searched: String },
    #[error("read monitor config {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse monitor config {}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error("invalid monitor config {}:\n{}", path.display(), issues.join("\n"))]
    Invalid { path: PathBuf, issues: Vec<String> },
}

impl LoadedConfig {
    pub fn demo() -> Result<Self, ConfigError> {
        let path = PathBuf::from("<built-in monitor demo>");
        let config = toml::from_str::<MonitorConfig>(super::demo::CONFIG).map_err(|source| {
            ConfigError::Parse { path: path.clone(), message: source.message().to_owned() }
        })?;
        let issues = validate(&config);
        if issues.is_empty() {
            Ok(Self { path, config })
        } else {
            Err(ConfigError::Invalid { path, issues })
        }
    }

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
        let config = toml::from_str::<MonitorConfig>(&raw).map_err(|source| {
            ConfigError::Parse { path: path.clone(), message: source.message().to_owned() }
        })?;
        let issues = validate(&config);
        if issues.is_empty() {
            Ok(Self { path, config })
        } else {
            Err(ConfigError::Invalid { path, issues })
        }
    }
}

fn validate(config: &MonitorConfig) -> Vec<String> {
    let mut issues = Vec::new();
    if config.version != SCHEMA_VERSION {
        issues.push(format!(
            "- version {} is unsupported; expected {SCHEMA_VERSION}",
            config.version
        ));
    }
    validate_limits(&config.limits, &mut issues);

    let environment_ids = unique_ids(
        config.environments.iter().map(|environment| environment.id.as_str()),
        "environment",
        &mut issues,
    );
    if !environment_ids.contains(config.default_environment.as_str()) {
        issues.push(format!(
            "- default_environment '{}' does not reference an environment",
            config.default_environment
        ));
    }

    let source_ids = unique_ids(config.sources.iter().map(SourceConfig::id), "source", &mut issues);
    let prometheus_ids = config
        .sources
        .iter()
        .filter_map(|source| match source {
            SourceConfig::Prometheus(source) => Some(source.id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    for (index, source) in config.sources.iter().enumerate() {
        if !environment_ids.contains(source.environment()) {
            issues.push(format!(
                "- sources[{index}].environment '{}' does not reference an environment",
                source.environment()
            ));
        }
        validate_source(index, source, &mut issues);
    }
    for (index, environment) in config.environments.iter().enumerate() {
        if environment.name.trim().is_empty() {
            issues.push(format!("- environments[{index}].name must not be empty"));
        }
        if environment.monthly_budget_usd.is_some_and(|budget| !budget.is_finite() || budget <= 0.0)
        {
            issues.push(format!(
                "- environments[{index}].monthly_budget_usd must be finite and greater than zero"
            ));
        }
        for source in &environment.required_sources {
            if !source_ids.contains(source.as_str()) {
                issues.push(format!(
                    "- environments[{index}].required_sources references unknown source '{source}'"
                ));
            } else if config
                .sources
                .iter()
                .find(|candidate| candidate.id() == source)
                .is_some_and(|candidate| candidate.environment() != environment.id)
            {
                issues.push(format!(
                    "- environments[{index}].required_sources source '{source}' belongs to another environment"
                ));
            }
        }
    }

    let service_ids = unique_ids(
        config.services.iter().map(|service| service.id.as_str()),
        "service",
        &mut issues,
    );
    for (index, service) in config.services.iter().enumerate() {
        if service.name.trim().is_empty() {
            issues.push(format!("- services[{index}].name must not be empty"));
        }
        if !environment_ids.contains(service.environment.as_str()) {
            issues.push(format!(
                "- services[{index}].environment '{}' does not reference an environment",
                service.environment
            ));
        }
        if !source_ids.contains(service.health_source.as_str()) {
            issues.push(format!(
                "- services[{index}].health_source '{}' does not reference a source",
                service.health_source
            ));
        } else if config
            .sources
            .iter()
            .find(|source| source.id() == service.health_source)
            .is_some_and(|source| source.environment() != service.environment)
        {
            issues.push(format!(
                "- services[{index}].health_source '{}' belongs to another environment",
                service.health_source
            ));
        }
        let mut metric_ids = HashSet::new();
        for (metric_index, metric) in service.metrics.iter().enumerate() {
            let label = format!("services[{index}].metrics[{metric_index}]");
            if !valid_id(&metric.id) || !metric_ids.insert(metric.id.as_str()) {
                issues.push(format!("- {label}.id must be valid and unique within its service"));
            }
            if metric.name.trim().is_empty() || metric.query.trim().is_empty() {
                issues.push(format!("- {label}.name and query must not be empty"));
            }
            if !prometheus_ids.contains(metric.source.as_str()) {
                issues.push(format!(
                    "- {label}.source '{}' must reference a prometheus source",
                    metric.source
                ));
            } else if config
                .sources
                .iter()
                .find(|source| source.id() == metric.source)
                .is_some_and(|source| source.environment() != service.environment)
            {
                issues.push(format!(
                    "- {label}.source '{}' belongs to another environment",
                    metric.source
                ));
            }
            if let (Some(warning), Some(critical)) = (metric.warning_above, metric.critical_above) {
                if !warning.is_finite() || !critical.is_finite() || warning > critical {
                    issues.push(format!(
                        "- {label} thresholds must be finite and warning_above <= critical_above"
                    ));
                }
            }
        }
    }

    unique_ids(config.costs.iter().map(|cost| cost.id.as_str()), "cost", &mut issues);
    for (index, cost) in config.costs.iter().enumerate() {
        if !environment_ids.contains(cost.environment.as_str()) {
            issues.push(format!(
                "- costs[{index}].environment '{}' does not reference an environment",
                cost.environment
            ));
        }
        if let Some(service) = &cost.service {
            if !service_ids.contains(service.as_str()) {
                issues.push(format!(
                    "- costs[{index}].service '{service}' does not reference a service"
                ));
            } else if config
                .services
                .iter()
                .find(|candidate| candidate.id == *service)
                .is_some_and(|candidate| candidate.environment != cost.environment)
            {
                issues.push(format!(
                    "- costs[{index}].service '{service}' belongs to another environment"
                ));
            }
        }
        if cost.name.trim().is_empty()
            || cost.provider.trim().is_empty()
            || !cost.monthly_usd.is_finite()
            || cost.monthly_usd < 0.0
        {
            issues.push(format!(
                "- costs[{index}] requires a name, provider, and finite non-negative monthly_usd"
            ));
        }
    }
    issues
}

fn validate_source(index: usize, source: &SourceConfig, issues: &mut Vec<String>) {
    if !valid_id(source.id()) || source.name().trim().is_empty() {
        issues.push(format!("- sources[{index}] requires a valid id and non-empty name"));
    }
    let url = match source {
        SourceConfig::HttpHealth(source) => {
            if !(100..=599).contains(&source.expected_status) {
                issues.push(format!("- sources[{index}].expected_status must be 100..599"));
            }
            &source.url
        }
        SourceConfig::Prometheus(source) => &source.url,
        SourceConfig::Loki(source) => {
            if source.default_query.trim().is_empty() {
                issues.push(format!("- sources[{index}].default_query must not be empty"));
            }
            &source.url
        }
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        issues.push(format!(
            "- sources[{index}].url must be an http(s) origin/path without credentials, query, or fragment"
        ));
    }
    if let Some(auth) = source.auth() {
        match auth {
            SourceAuth::Bearer { token_env } => {
                validate_credential_name(index, "auth.token_env", token_env, issues);
            }
            SourceAuth::Basic { username, password_env } => {
                if username.trim().is_empty() {
                    issues.push(format!("- sources[{index}].auth.username must not be empty"));
                }
                validate_credential_name(index, "auth.password_env", password_env, issues);
            }
        }
    }
}

fn validate_credential_name(
    source_index: usize,
    field: &str,
    name: &str,
    issues: &mut Vec<String>,
) {
    if !valid_environment_name(name) {
        issues.push(format!(
            "- sources[{source_index}].{field} must be a valid environment variable name"
        ));
    }
}

fn validate_limits(limits: &LimitsConfig, issues: &mut Vec<String>) {
    if !(250..=60_000).contains(&limits.request_timeout_ms) {
        issues.push("- limits.request_timeout_ms must be between 250 and 60000".to_owned());
    }
    if !(1..=5_000).contains(&limits.max_log_events) {
        issues.push("- limits.max_log_events must be between 1 and 5000".to_owned());
    }
    if !(1..=24 * 31).contains(&limits.max_lookback_hours) {
        issues.push("- limits.max_lookback_hours must be between 1 and 744".to_owned());
    }
    if !(5..=3_600).contains(&limits.stale_after_secs) {
        issues.push("- limits.stale_after_secs must be between 5 and 3600".to_owned());
    }
}

fn unique_ids<'a>(
    ids: impl Iterator<Item = &'a str>,
    label: &str,
    issues: &mut Vec<String>,
) -> HashSet<&'a str> {
    let mut out = HashSet::new();
    for id in ids {
        if !valid_id(id) {
            issues.push(format!(
                "- {label} id '{id}' must use only letters, numbers, '.', '_' or '-'"
            ));
        } else if !out.insert(id) {
            issues.push(format!("- duplicate {label} id '{id}'"));
        }
    }
    out
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

const fn default_expected_status() -> u16 {
    200
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
version = 1
default_environment = "production"

[[environments]]
id = "production"
name = "Production"
required_sources = ["api", "metrics"]

[[sources]]
type = "http-health"
id = "api"
name = "API health"
environment = "production"
url = "https://api.example.test/health"

[[sources]]
type = "prometheus"
id = "metrics"
name = "Prometheus"
environment = "production"
url = "https://metrics.example.test"

[[services]]
id = "api"
name = "API"
environment = "production"
health_source = "api"

[[services.metrics]]
id = "cpu"
name = "CPU"
source = "metrics"
query = "process_cpu_utilization"
unit = "percent"
warning_above = 0.7
critical_above = 0.9

[[costs]]
id = "server"
name = "Server"
provider = "Hetzner"
environment = "production"
monthly_usd = 24.0
confidence = "fixed"
"#;

    #[test]
    fn validates_closed_monitor_config() {
        let config = toml::from_str::<MonitorConfig>(VALID).unwrap();
        assert!(validate(&config).is_empty());
    }

    #[test]
    fn rejects_secret_bearing_urls_and_bad_references() {
        let raw = VALID
            .replace("https://metrics.example.test", "https://user:secret@example.test?token=x")
            .replace("health_source = \"api\"", "health_source = \"missing\"");
        let config = toml::from_str::<MonitorConfig>(&raw).unwrap();
        let issues = validate(&config).join("\n");
        assert!(issues.contains("without credentials"));
        assert!(issues.contains("does not reference a source"));
        assert!(!issues.contains("secret"));
    }
}
