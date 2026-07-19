use std::{collections::HashSet, time::Instant};

use anyhow::{Context as _, Result};
use futures_util::future::join_all;
use reqwest::{Client, RequestBuilder};
use serde::Deserialize;
use serde_json::Value;

use super::{
    config::{
        HttpHealthSource, LokiSource, MonitorConfig, PrometheusSource, ServiceConfig, SourceAuth,
        SourceConfig,
    },
    model::{
        metric_health, summarize_health, CostSnapshot, CostSummary, DeploymentCollection,
        EnvironmentSnapshot, HealthState, LogCollection, LogEvent, MetricSample, MetricSnapshot,
        MetricValue, MonitorSnapshot, ServiceSnapshot, SourceSnapshot, SourceState,
        SNAPSHOT_SCHEMA_VERSION,
    },
};

#[derive(Clone, Debug)]
pub struct CollectionRequest {
    pub environment: String,
    pub include_logs: bool,
    pub log_service: Option<String>,
    pub log_lookback_secs: u64,
    pub log_limit: usize,
}

#[derive(Clone)]
pub struct Collector {
    backend: CollectorBackend,
}

#[derive(Clone)]
enum CollectorBackend {
    Live(Client),
    Demo,
}

impl Collector {
    pub fn new(config: &MonitorConfig) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(config.limits.request_timeout())
            .timeout(config.limits.request_timeout())
            .user_agent(concat!("kit-monitor/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build Monitor HTTP client")?;
        Ok(Self { backend: CollectorBackend::Live(client) })
    }

    pub fn demo() -> Self {
        Self { backend: CollectorBackend::Demo }
    }

    pub async fn collect(
        &self,
        config: &MonitorConfig,
        request: &CollectionRequest,
    ) -> Result<MonitorSnapshot> {
        if matches!(self.backend, CollectorBackend::Demo) {
            return Ok(super::demo::snapshot(config, request));
        }
        let started = Instant::now();
        let observed_at_secs = unix_time_secs();
        let environment = config
            .environments
            .iter()
            .find(|environment| environment.id == request.environment)
            .with_context(|| format!("environment '{}' is not configured", request.environment))?;
        let required =
            environment.required_sources.iter().map(String::as_str).collect::<HashSet<_>>();
        let sources = config
            .sources
            .iter()
            .filter(|source| source.environment() == environment.id)
            .collect::<Vec<_>>();
        let source_snapshots = join_all(sources.iter().map(|source| {
            self.probe_source(source, required.contains(source.id()), observed_at_secs)
        }))
        .await;

        let services = config
            .services
            .iter()
            .filter(|service| service.environment == environment.id)
            .map(|service| service_snapshot(service, &source_snapshots, observed_at_secs))
            .collect::<Vec<_>>();

        let metric_futures = config
            .services
            .iter()
            .filter(|service| service.environment == environment.id)
            .flat_map(|service| service.metrics.iter().map(move |metric| (service, metric)))
            .map(|(service, metric)| {
                let metric_sources = sources.clone();
                async move {
                    let source = metric_sources.iter().find_map(|source| match source {
                        SourceConfig::Prometheus(source) if source.id == metric.source => {
                            Some(source)
                        }
                        _ => None,
                    });
                    match source {
                        Some(source) => {
                            self.collect_metric(service, metric, source, observed_at_secs).await
                        }
                        None => MetricSnapshot {
                            id: metric.id.clone(),
                            service_id: service.id.clone(),
                            name: metric.name.clone(),
                            source_id: metric.source.clone(),
                            unit: metric.unit,
                            state: HealthState::Unknown,
                            value: MetricValue::Unavailable("source is not configured".to_owned()),
                            samples: Vec::new(),
                            observed_at_secs,
                            latency_ms: 0,
                        },
                    }
                }
            });
        let performance = join_all(metric_futures).await;

        let logs = if request.include_logs {
            match sources.iter().find_map(|source| match source {
                SourceConfig::Loki(source) => Some(source),
                _ => None,
            }) {
                Some(source) => {
                    self.collect_logs(
                        source,
                        request,
                        observed_at_secs,
                        config.limits.max_log_events,
                    )
                    .await
                }
                None => LogCollection {
                    state: SourceState::Unavailable,
                    events: Vec::new(),
                    detail: "no Loki source is configured for this environment".to_owned(),
                    truncated: false,
                    limit: request.log_limit.min(config.limits.max_log_events),
                },
            }
        } else {
            LogCollection {
                state: SourceState::Unavailable,
                events: Vec::new(),
                detail: "logs were not requested for this projection".to_owned(),
                truncated: false,
                limit: request.log_limit.min(config.limits.max_log_events),
            }
        };

        let cost_items = config
            .costs
            .iter()
            .filter(|cost| cost.environment == environment.id)
            .map(|cost| CostSnapshot {
                id: cost.id.clone(),
                name: cost.name.clone(),
                provider: cost.provider.clone(),
                service_id: cost.service.clone(),
                monthly_usd: cost.monthly_usd,
                confidence: cost.confidence,
            })
            .collect::<Vec<_>>();
        let costs = if cost_items.is_empty() {
            CostSummary {
                currency: "USD".to_owned(),
                monthly_total: 0.0,
                monthly_budget: environment.monthly_budget_usd,
                budget_percent: None,
                state: SourceState::Unavailable,
                items: cost_items,
                detail: "no cost items are configured".to_owned(),
            }
        } else {
            let monthly_total = cost_items.iter().map(|item| item.monthly_usd).sum::<f64>();
            CostSummary {
                currency: "USD".to_owned(),
                monthly_total,
                monthly_budget: environment.monthly_budget_usd,
                budget_percent: environment
                    .monthly_budget_usd
                    .map(|budget| monthly_total / budget * 100.0),
                state: SourceState::Partial,
                items: cost_items,
                detail: "configured recurring costs only; provider billing is not connected"
                    .to_owned(),
            }
        };
        let deployments = DeploymentCollection {
            state: SourceState::Unavailable,
            entries: Vec::new(),
            detail: "no read-only deployment source is configured".to_owned(),
            truncated: false,
        };
        let health = summarize_health(&services, &source_snapshots);
        let warnings = source_snapshots
            .iter()
            .filter(|source| source.state != SourceState::Ready)
            .map(|source| format!("{}: {}", source.name, source.detail))
            .chain((costs.state == SourceState::Partial).then(|| costs.detail.clone()))
            .collect();

        Ok(MonitorSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            environment: EnvironmentSnapshot {
                id: environment.id.clone(),
                name: environment.name.clone(),
            },
            observed_at_secs,
            collection_duration_ms: elapsed_ms(started),
            health,
            services,
            performance,
            logs,
            deployments,
            costs,
            sources: source_snapshots,
            warnings,
        })
    }

    async fn probe_source(
        &self,
        source: &SourceConfig,
        required: bool,
        observed_at_secs: u64,
    ) -> SourceSnapshot {
        let started = Instant::now();
        let result = match source {
            SourceConfig::HttpHealth(source) => self.probe_http_health(source).await,
            SourceConfig::Prometheus(source) => self.probe_prometheus(source).await,
            SourceConfig::Loki(source) => self.probe_loki(source).await,
        };
        let (state, detail) = match result {
            Ok(detail) => (SourceState::Ready, detail),
            Err(FetchFailure::Unauthorized(detail)) => (SourceState::Unauthorized, detail),
            Err(FetchFailure::Unavailable(detail)) => (SourceState::Unavailable, detail),
            Err(FetchFailure::Failed(detail)) => (SourceState::Error, detail),
        };
        SourceSnapshot {
            id: source.id().to_owned(),
            name: source.name().to_owned(),
            kind: source.kind().to_owned(),
            required,
            state,
            detail,
            observed_at_secs,
            latency_ms: elapsed_ms(started),
        }
    }

    async fn probe_http_health(&self, source: &HttpHealthSource) -> Result<String, FetchFailure> {
        let response = self
            .authorized(self.client().get(source.url.clone()), source.auth.as_ref())?
            .send()
            .await
            .map_err(safe_transport_error)?;
        let status = response.status().as_u16();
        if status == source.expected_status {
            Ok(format!("HTTP {status}"))
        } else if response.status() == reqwest::StatusCode::UNAUTHORIZED
            || response.status() == reqwest::StatusCode::FORBIDDEN
        {
            Err(FetchFailure::Unauthorized(format!("HTTP {status}")))
        } else {
            Err(FetchFailure::Failed(format!("HTTP {status}; expected {}", source.expected_status)))
        }
    }

    async fn probe_prometheus(&self, source: &PrometheusSource) -> Result<String, FetchFailure> {
        self.probe_ready(endpoint(&source.url, "api/v1/status/buildinfo"), source.auth.as_ref())
            .await
    }

    async fn probe_loki(&self, source: &LokiSource) -> Result<String, FetchFailure> {
        self.probe_ready(endpoint(&source.url, "ready"), source.auth.as_ref()).await
    }

    async fn probe_ready(
        &self,
        url: url::Url,
        auth: Option<&SourceAuth>,
    ) -> Result<String, FetchFailure> {
        let response = self
            .authorized(self.client().get(url), auth)?
            .send()
            .await
            .map_err(safe_transport_error)?;
        if response.status().is_success() {
            Ok(format!("HTTP {}", response.status().as_u16()))
        } else if matches!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            Err(FetchFailure::Unauthorized(format!("HTTP {}", response.status().as_u16())))
        } else {
            Err(FetchFailure::Failed(format!("HTTP {}", response.status().as_u16())))
        }
    }

    async fn collect_metric(
        &self,
        service: &ServiceConfig,
        metric: &super::config::MetricConfig,
        source: &PrometheusSource,
        observed_at_secs: u64,
    ) -> MetricSnapshot {
        let started = Instant::now();
        let result = self.query_prometheus(source, &metric.query).await;
        let (value, state) = match result {
            Ok(Some(value)) => (
                MetricValue::Available(value),
                metric_health(value, metric.warning_above, metric.critical_above),
            ),
            Ok(None) => (MetricValue::Empty, HealthState::Unknown),
            Err(error) => (MetricValue::Unavailable(error.detail()), HealthState::Unknown),
        };
        let samples = match &value {
            MetricValue::Available(value) => {
                vec![MetricSample { observed_at_secs, value: *value }]
            }
            MetricValue::Empty | MetricValue::Unavailable(_) => Vec::new(),
        };
        MetricSnapshot {
            id: metric.id.clone(),
            service_id: service.id.clone(),
            name: metric.name.clone(),
            source_id: source.id.clone(),
            unit: metric.unit,
            state,
            value,
            samples,
            observed_at_secs,
            latency_ms: elapsed_ms(started),
        }
    }

    async fn query_prometheus(
        &self,
        source: &PrometheusSource,
        query: &str,
    ) -> Result<Option<f64>, FetchFailure> {
        let url = endpoint(&source.url, "api/v1/query");
        let response = self
            .authorized(self.client().get(url).query(&[("query", query)]), source.auth.as_ref())?
            .send()
            .await
            .map_err(safe_transport_error)?;
        if !response.status().is_success() {
            return Err(status_failure(response.status()));
        }
        let payload = response
            .json::<PrometheusResponse>()
            .await
            .map_err(|_| FetchFailure::Failed("Prometheus returned an invalid response".into()))?;
        if payload.status != "success" {
            return Err(FetchFailure::Failed("Prometheus query failed".into()));
        }
        Ok(payload
            .data
            .result
            .first()
            .and_then(|sample| sample.value.get(1))
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite()))
    }

    async fn collect_logs(
        &self,
        source: &LokiSource,
        request: &CollectionRequest,
        observed_at_secs: u64,
        configured_limit: usize,
    ) -> LogCollection {
        let limit = request.log_limit.min(configured_limit);
        let end_ns = observed_at_secs.saturating_mul(1_000_000_000);
        let start_ns =
            end_ns.saturating_sub(request.log_lookback_secs.saturating_mul(1_000_000_000));
        let query = match &request.log_service {
            Some(service) => format!("{} |= {:?}", source.default_query, service),
            None => source.default_query.clone(),
        };
        match self.query_loki(source, &query, start_ns, end_ns, limit).await {
            Ok(mut events) => {
                let truncated = events.len() >= limit;
                for event in &mut events {
                    if event.service_id.is_empty() {
                        event.service_id =
                            request.log_service.clone().unwrap_or_else(|| "unknown".into());
                    }
                }
                LogCollection {
                    state: SourceState::Ready,
                    detail: if truncated {
                        format!("showing the newest {limit} events")
                    } else {
                        format!("{} events", events.len())
                    },
                    events,
                    truncated,
                    limit,
                }
            }
            Err(error) => LogCollection {
                state: match error {
                    FetchFailure::Unauthorized(_) => SourceState::Unauthorized,
                    FetchFailure::Unavailable(_) => SourceState::Unavailable,
                    FetchFailure::Failed(_) => SourceState::Error,
                },
                events: Vec::new(),
                detail: error.detail(),
                truncated: false,
                limit,
            },
        }
    }

    async fn query_loki(
        &self,
        source: &LokiSource,
        query: &str,
        start_ns: u64,
        end_ns: u64,
        limit: usize,
    ) -> Result<Vec<LogEvent>, FetchFailure> {
        let url = endpoint(&source.url, "loki/api/v1/query_range");
        let response = self
            .authorized(
                self.client().get(url).query(&[
                    ("query", query.to_owned()),
                    ("start", start_ns.to_string()),
                    ("end", end_ns.to_string()),
                    ("limit", limit.to_string()),
                    ("direction", "backward".to_owned()),
                ]),
                source.auth.as_ref(),
            )?
            .send()
            .await
            .map_err(safe_transport_error)?;
        if !response.status().is_success() {
            return Err(status_failure(response.status()));
        }
        let payload = response
            .json::<LokiResponse>()
            .await
            .map_err(|_| FetchFailure::Failed("Loki returned an invalid response".into()))?;
        if payload.status != "success" {
            return Err(FetchFailure::Failed("Loki query failed".into()));
        }
        let sensitive = sensitive_fields(source);
        let mut events = Vec::new();
        for stream in payload.data.result {
            for (timestamp_ns, raw) in stream.values {
                let (message, service_id, level, redacted_fields) =
                    sanitize_log_line(&raw, &stream.stream, &sensitive, source.allow_unstructured);
                let id = format!("{}:{timestamp_ns}:{}", source.id, events.len());
                events.push(LogEvent {
                    id,
                    source_id: source.id.clone(),
                    timestamp_ns,
                    service_id,
                    level,
                    message,
                    redacted_fields,
                });
            }
        }
        events.sort_by(|left, right| right.timestamp_ns.cmp(&left.timestamp_ns));
        events.truncate(limit);
        Ok(events)
    }

    fn authorized(
        &self,
        request: RequestBuilder,
        auth: Option<&SourceAuth>,
    ) -> Result<RequestBuilder, FetchFailure> {
        match auth {
            Some(SourceAuth::Bearer { token_env }) => {
                Ok(request.bearer_auth(credential_value(token_env)?))
            }
            Some(SourceAuth::Basic { username, password_env }) => {
                Ok(request.basic_auth(username, Some(credential_value(password_env)?)))
            }
            None => Ok(request),
        }
    }

    fn client(&self) -> &Client {
        match &self.backend {
            CollectorBackend::Live(client) => client,
            CollectorBackend::Demo => unreachable!("demo collection does not perform HTTP"),
        }
    }
}

fn credential_value(name: &str) -> Result<String, FetchFailure> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        Ok(_) | Err(std::env::VarError::NotPresent) => Err(FetchFailure::Unauthorized(format!(
            "credential environment variable {name} is not set"
        ))),
        Err(std::env::VarError::NotUnicode(_)) => Err(FetchFailure::Unauthorized(format!(
            "credential environment variable {name} is not valid text"
        ))),
    }
}

fn service_snapshot(
    service: &ServiceConfig,
    sources: &[SourceSnapshot],
    observed_at_secs: u64,
) -> ServiceSnapshot {
    let source = sources.iter().find(|source| source.id == service.health_source);
    let (state, reason, latency_ms) = match source {
        Some(source) => (
            match source.state {
                SourceState::Ready => HealthState::Healthy,
                SourceState::Partial => HealthState::Degraded,
                SourceState::Error => HealthState::Incident,
                SourceState::Unavailable | SourceState::Unauthorized => HealthState::Unknown,
            },
            source.detail.clone(),
            source.latency_ms,
        ),
        None => (HealthState::Unknown, "health source is unavailable".to_owned(), 0),
    };
    ServiceSnapshot {
        id: service.id.clone(),
        name: service.name.clone(),
        state,
        reason,
        source_id: service.health_source.clone(),
        observed_at_secs,
        latency_ms,
    }
}

#[derive(Debug)]
enum FetchFailure {
    Unauthorized(String),
    Unavailable(String),
    Failed(String),
}

impl FetchFailure {
    fn detail(&self) -> String {
        match self {
            Self::Unauthorized(detail) | Self::Unavailable(detail) | Self::Failed(detail) => {
                detail.clone()
            }
        }
    }
}

fn safe_transport_error(error: reqwest::Error) -> FetchFailure {
    if error.is_timeout() {
        FetchFailure::Unavailable("request timed out".to_owned())
    } else if error.is_connect() {
        FetchFailure::Unavailable("connection failed".to_owned())
    } else {
        FetchFailure::Failed("request failed".to_owned())
    }
}

fn status_failure(status: reqwest::StatusCode) -> FetchFailure {
    if matches!(status, reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN) {
        FetchFailure::Unauthorized(format!("HTTP {}", status.as_u16()))
    } else {
        FetchFailure::Failed(format!("HTTP {}", status.as_u16()))
    }
}

fn endpoint(base: &url::Url, suffix: &str) -> url::Url {
    let mut endpoint = base.clone();
    let path = format!("{}/{}", base.path().trim_end_matches('/'), suffix.trim_start_matches('/'));
    endpoint.set_path(&path);
    endpoint
}

#[derive(Deserialize)]
struct PrometheusResponse {
    status: String,
    data: PrometheusData,
}

#[derive(Deserialize)]
struct PrometheusData {
    result: Vec<PrometheusSample>,
}

#[derive(Deserialize)]
struct PrometheusSample {
    value: Vec<Value>,
}

#[derive(Deserialize)]
struct LokiResponse {
    status: String,
    data: LokiData,
}

#[derive(Deserialize)]
struct LokiData {
    result: Vec<LokiStream>,
}

#[derive(Deserialize)]
struct LokiStream {
    #[serde(default)]
    stream: serde_json::Map<String, Value>,
    values: Vec<(String, String)>,
}

fn sensitive_fields(source: &LokiSource) -> HashSet<String> {
    [
        "authorization",
        "cookie",
        "set-cookie",
        "token",
        "secret",
        "password",
        "prompt",
        "environment",
        "env",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain(source.sensitive_fields.iter().map(|field| field.to_ascii_lowercase()))
    .collect()
}

fn sanitize_log_line(
    raw: &str,
    labels: &serde_json::Map<String, Value>,
    sensitive: &HashSet<String>,
    allow_unstructured: bool,
) -> (String, String, String, usize) {
    let service_id = label(labels, &["service", "app", "container", "job"]);
    let level = label(labels, &["level", "severity"]).unwrap_or_else(|| "info".to_owned());
    let mut parsed = match serde_json::from_str::<Value>(raw) {
        Ok(value) => value,
        Err(_) if allow_unstructured => {
            return (redact_bearer(raw), service_id.unwrap_or_default(), level, 0);
        }
        Err(_) => {
            return (
                "[unstructured log omitted by redaction policy]".to_owned(),
                service_id.unwrap_or_default(),
                level,
                1,
            );
        }
    };
    let mut redacted = 0;
    redact_value(&mut parsed, sensitive, &mut redacted);
    let parsed_service = object_string(&parsed, &["service", "app", "container"]);
    let parsed_level = object_string(&parsed, &["level", "severity"]);
    let message = object_string(&parsed, &["message", "msg"])
        .map(|message| redact_bearer(&message))
        .unwrap_or_else(|| serde_json::to_string(&parsed).unwrap_or_else(|_| "{}".to_owned()));
    (
        message,
        parsed_service.or(service_id).unwrap_or_default(),
        parsed_level.unwrap_or(level),
        redacted,
    )
}

fn redact_value(value: &mut Value, sensitive: &HashSet<String>, redacted: &mut usize) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if sensitive.contains(&key.to_ascii_lowercase()) {
                    *value = Value::String("[REDACTED]".to_owned());
                    *redacted += 1;
                } else {
                    redact_value(value, sensitive, redacted);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_value(value, sensitive, redacted);
            }
        }
        _ => {}
    }
}

fn label(labels: &serde_json::Map<String, Value>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| labels.get(*name).and_then(Value::as_str).map(str::to_owned))
}

fn object_string(value: &Value, names: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    names.iter().find_map(|name| object.get(*name).and_then(Value::as_str).map(str::to_owned))
}

fn redact_bearer(value: &str) -> String {
    let Some(start) = value.to_ascii_lowercase().find("bearer ") else {
        return value.to_owned();
    };
    let token_start = start + "bearer ".len();
    let suffix = &value[token_start..];
    let token_len = suffix.find(char::is_whitespace).unwrap_or(suffix.len());
    format!("{}[REDACTED]{}", &value[..token_start], &suffix[token_len..])
}

fn unix_time_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_logs_remove_sensitive_fields_before_projection() {
        let source = LokiSource {
            id: "logs".into(),
            name: "Logs".into(),
            environment: "production".into(),
            url: "https://logs.example.test".parse().unwrap(),
            default_query: "{job=\"api\"}".into(),
            auth: None,
            allow_unstructured: false,
            sensitive_fields: vec!["session".into()],
        };
        let raw = r#"{"service":"api","level":"warn","message":"failed","authorization":"Bearer abc","nested":{"session":"xyz"}}"#;
        let (message, service, level, count) =
            sanitize_log_line(raw, &serde_json::Map::new(), &sensitive_fields(&source), false);
        assert_eq!(message, "failed");
        assert_eq!(service, "api");
        assert_eq!(level, "warn");
        assert_eq!(count, 2);
    }

    #[test]
    fn unstructured_logs_are_omitted_by_default() {
        let (message, _, _, count) = sanitize_log_line(
            "Authorization: Bearer secret",
            &serde_json::Map::new(),
            &HashSet::new(),
            false,
        );
        assert!(message.contains("omitted"));
        assert_eq!(count, 1);
    }
}
