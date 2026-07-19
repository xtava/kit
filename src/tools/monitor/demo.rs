//! Deterministic Monitor scenario used to develop every projection without production access.

use super::{
    config::{CostConfidence, MetricUnit, MonitorConfig},
    model::{
        summarize_health, CostSnapshot, CostSummary, DeploymentCollection, DeploymentSnapshot,
        EnvironmentSnapshot, HealthState, LogCollection, LogEvent, MetricSample, MetricSnapshot,
        MetricValue, MonitorSnapshot, ServiceSnapshot, SourceSnapshot, SourceState,
        SNAPSHOT_SCHEMA_VERSION,
    },
    sources::CollectionRequest,
};

pub(super) const CONFIG: &str = r#"
version = 1
default_environment = "demo"

[[environments]]
id = "demo"
name = "Production Demo"
monthly_budget_usd = 300.0
required_sources = ["api-health", "web-health", "worker-health", "database-health", "metrics"]

[[sources]]
type = "http-health"
id = "api-health"
name = "API health"
environment = "demo"
url = "https://api.example.test/health"

[[sources]]
type = "http-health"
id = "web-health"
name = "Web health"
environment = "demo"
url = "https://web.example.test/health"

[[sources]]
type = "http-health"
id = "worker-health"
name = "Worker health"
environment = "demo"
url = "https://worker.example.test/health"

[[sources]]
type = "http-health"
id = "database-health"
name = "Database health"
environment = "demo"
url = "https://database.example.test/health"

[[sources]]
type = "prometheus"
id = "metrics"
name = "Prometheus"
environment = "demo"
url = "https://metrics.example.test"

[[sources]]
type = "loki"
id = "logs"
name = "Loki"
environment = "demo"
url = "https://logs.example.test"
default_query = "{environment=\"production\"}"

[[services]]
id = "api"
name = "API"
environment = "demo"
health_source = "api-health"

[[services.metrics]]
id = "request-rate"
name = "Request rate"
source = "metrics"
query = "sum(rate(http_requests_total[5m]))"
unit = "requests-per-second"

[[services.metrics]]
id = "latency-p95"
name = "Request p95"
source = "metrics"
query = "histogram_quantile(0.95, rate(http_request_duration_seconds_bucket[5m]))"
unit = "milliseconds"
warning_above = 250.0
critical_above = 1000.0

[[services]]
id = "web"
name = "Web"
environment = "demo"
health_source = "web-health"

[[services.metrics]]
id = "error-rate"
name = "Error rate"
source = "metrics"
query = "sum(rate(http_requests_total{status=~\"5..\"}[5m]))"
unit = "percent"
warning_above = 0.02
critical_above = 0.05

[[services]]
id = "worker"
name = "Background jobs"
environment = "demo"
health_source = "worker-health"

[[services.metrics]]
id = "queue-depth"
name = "Queue depth"
source = "metrics"
query = "job_queue_depth"
unit = "count"
warning_above = 100.0
critical_above = 500.0

[[services]]
id = "database"
name = "Database"
environment = "demo"
health_source = "database-health"

[[services.metrics]]
id = "connections"
name = "Connection use"
source = "metrics"
query = "database_connection_utilization"
unit = "percent"
warning_above = 0.75
critical_above = 0.90

[[costs]]
id = "compute"
name = "Production compute"
provider = "Hetzner"
environment = "demo"
service = "api"
monthly_usd = 84.40
confidence = "fixed"

[[costs]]
id = "database"
name = "Managed database"
provider = "Neon"
environment = "demo"
service = "database"
monthly_usd = 69.00
confidence = "billed"

[[costs]]
id = "edge"
name = "Edge and DNS"
provider = "Cloudflare"
environment = "demo"
service = "web"
monthly_usd = 20.00
confidence = "fixed"

[[costs]]
id = "observability"
name = "Logs and metrics"
provider = "Grafana Cloud"
environment = "demo"
monthly_usd = 14.00
confidence = "estimated"
"#;

pub(super) fn snapshot(config: &MonitorConfig, request: &CollectionRequest) -> MonitorSnapshot {
    let now = unix_time_secs();
    let environment = config
        .environments
        .iter()
        .find(|environment| environment.id == request.environment)
        .expect("demo requests are validated against the built-in config");
    let sources = vec![
        source(
            "api-health",
            "API health",
            "http-health",
            true,
            SourceState::Ready,
            "HTTP 200",
            42,
            now,
        ),
        source(
            "web-health",
            "Web health",
            "http-health",
            true,
            SourceState::Ready,
            "HTTP 200",
            28,
            now,
        ),
        source(
            "worker-health",
            "Worker health",
            "http-health",
            true,
            SourceState::Partial,
            "queue depth elevated",
            96,
            now,
        ),
        source(
            "database-health",
            "Database health",
            "http-health",
            true,
            SourceState::Error,
            "connection saturation",
            184,
            now,
        ),
        source(
            "metrics",
            "Prometheus",
            "prometheus",
            true,
            SourceState::Ready,
            "HTTP 200",
            51,
            now,
        ),
        source("logs", "Loki", "loki", false, SourceState::Ready, "HTTP 200", 63, now),
    ];
    let services = vec![
        service("api", "API", HealthState::Healthy, "HTTP 200", "api-health", 42, now),
        service("web", "Web", HealthState::Healthy, "HTTP 200", "web-health", 28, now),
        service(
            "worker",
            "Background jobs",
            HealthState::Degraded,
            "queue depth elevated",
            "worker-health",
            96,
            now,
        ),
        service(
            "database",
            "Database",
            HealthState::Incident,
            "connection saturation",
            "database-health",
            184,
            now,
        ),
    ];
    let performance = vec![
        metric(
            "request-rate",
            "api",
            "Request rate",
            MetricUnit::RequestsPerSecond,
            &[82.0, 88.0, 94.0, 91.0, 103.0, 109.0, 117.0, 114.0, 121.0, 126.0, 132.0, 128.4],
            HealthState::Healthy,
            47,
            now,
        ),
        metric(
            "latency-p95",
            "api",
            "Request p95",
            MetricUnit::Milliseconds,
            &[112.0, 124.0, 119.0, 137.0, 142.0, 151.0, 148.0, 164.0, 172.0, 168.0, 179.0, 184.0],
            HealthState::Healthy,
            53,
            now,
        ),
        metric(
            "error-rate",
            "web",
            "Error rate",
            MetricUnit::Percent,
            &[0.003, 0.004, 0.003, 0.005, 0.004, 0.006, 0.005, 0.007, 0.006, 0.009, 0.007, 0.008],
            HealthState::Healthy,
            45,
            now,
        ),
        metric(
            "queue-depth",
            "worker",
            "Queue depth",
            MetricUnit::Count,
            &[42.0, 48.0, 55.0, 61.0, 74.0, 83.0, 96.0, 118.0, 137.0, 149.0, 171.0, 182.0],
            HealthState::Degraded,
            58,
            now,
        ),
        metric(
            "connections",
            "database",
            "Connection use",
            MetricUnit::Percent,
            &[0.63, 0.66, 0.68, 0.71, 0.74, 0.77, 0.79, 0.83, 0.86, 0.89, 0.92, 0.94],
            HealthState::Incident,
            71,
            now,
        ),
    ];
    let logs = demo_logs(request, now);
    let deployments = DeploymentCollection {
        state: SourceState::Ready,
        entries: vec![
            deployment("deploy-api-142", "api", "v1.42.0", "succeeded", now - 1_320, 48_200),
            deployment("deploy-web-318", "web", "web-318", "succeeded", now - 5_640, 31_800),
            deployment(
                "deploy-worker-87",
                "worker",
                "worker-87",
                "rolled-back",
                now - 12_400,
                74_100,
            ),
        ],
        detail: "3 recent deployments from the demo scenario".to_owned(),
        truncated: false,
    };
    let items = vec![
        cost("compute", "Production compute", "Hetzner", Some("api"), 84.40, CostConfidence::Fixed),
        cost(
            "database",
            "Managed database",
            "Neon",
            Some("database"),
            69.00,
            CostConfidence::Billed,
        ),
        cost("edge", "Edge and DNS", "Cloudflare", Some("web"), 20.00, CostConfidence::Fixed),
        cost(
            "observability",
            "Logs and metrics",
            "Grafana Cloud",
            None,
            14.00,
            CostConfidence::Estimated,
        ),
    ];
    let monthly_total = items.iter().map(|item| item.monthly_usd).sum::<f64>();
    let monthly_budget = environment.monthly_budget_usd;
    let costs = CostSummary {
        currency: "USD".to_owned(),
        monthly_total,
        monthly_budget,
        budget_percent: monthly_budget.map(|budget| monthly_total / budget * 100.0),
        state: SourceState::Partial,
        items,
        detail: "mixed billed, fixed, and estimated demo costs".to_owned(),
    };
    let health = summarize_health(&services, &sources);
    MonitorSnapshot {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        environment: EnvironmentSnapshot {
            id: environment.id.clone(),
            name: environment.name.clone(),
        },
        observed_at_secs: now,
        collection_duration_ms: 7,
        health,
        services,
        performance,
        logs,
        deployments,
        costs,
        sources,
        warnings: vec![
            "Demo evidence only; no production systems were queried".to_owned(),
            "Database connection saturation requires attention".to_owned(),
        ],
    }
}

fn demo_logs(request: &CollectionRequest, now: u64) -> LogCollection {
    if !request.include_logs {
        return LogCollection {
            state: SourceState::Unavailable,
            events: Vec::new(),
            detail: "logs were not requested for this projection".to_owned(),
            truncated: false,
            limit: request.log_limit,
        };
    }
    let mut events = vec![
        log("log-db-1", now - 18, "database", "error", "connection pool exhausted; retrying", 0),
        log(
            "log-api-1",
            now - 41,
            "api",
            "warn",
            "request latency exceeded the 250ms objective",
            0,
        ),
        log("log-worker-1", now - 73, "worker", "warn", "queue depth reached 182 jobs", 0),
        log("log-web-1", now - 109, "web", "info", "asset rollout completed", 0),
        log(
            "log-api-2",
            now - 144,
            "api",
            "error",
            "upstream request failed; authorization=[REDACTED]",
            1,
        ),
        log("log-worker-2", now - 201, "worker", "info", "processed batch of 50 jobs", 0),
    ];
    if let Some(service) = &request.log_service {
        events.retain(|event| event.service_id == *service);
    }
    let truncated = events.len() > request.log_limit;
    events.truncate(request.log_limit);
    LogCollection {
        state: SourceState::Ready,
        detail: if truncated {
            format!("showing the newest {} demo events", request.log_limit)
        } else {
            format!("{} demo events", events.len())
        },
        events,
        truncated,
        limit: request.log_limit,
    }
}

fn source(
    id: &str,
    name: &str,
    kind: &str,
    required: bool,
    state: SourceState,
    detail: &str,
    latency_ms: u64,
    observed_at_secs: u64,
) -> SourceSnapshot {
    SourceSnapshot {
        id: id.to_owned(),
        name: name.to_owned(),
        kind: kind.to_owned(),
        required,
        state,
        detail: detail.to_owned(),
        observed_at_secs,
        latency_ms,
    }
}

fn service(
    id: &str,
    name: &str,
    state: HealthState,
    reason: &str,
    source_id: &str,
    latency_ms: u64,
    observed_at_secs: u64,
) -> ServiceSnapshot {
    ServiceSnapshot {
        id: id.to_owned(),
        name: name.to_owned(),
        state,
        reason: reason.to_owned(),
        source_id: source_id.to_owned(),
        observed_at_secs,
        latency_ms,
    }
}

fn metric(
    id: &str,
    service_id: &str,
    name: &str,
    unit: MetricUnit,
    values: &[f64],
    state: HealthState,
    latency_ms: u64,
    observed_at_secs: u64,
) -> MetricSnapshot {
    let value = *values.last().expect("demo metric series must not be empty");
    let sample_count = u64::try_from(values.len()).unwrap_or(u64::MAX);
    let samples = values
        .iter()
        .enumerate()
        .map(|(index, value)| MetricSample {
            observed_at_secs: observed_at_secs.saturating_sub(
                sample_count.saturating_sub(index as u64 + 1).saturating_mul(5 * 60),
            ),
            value: *value,
        })
        .collect();
    MetricSnapshot {
        id: id.to_owned(),
        service_id: service_id.to_owned(),
        name: name.to_owned(),
        source_id: "metrics".to_owned(),
        unit,
        state,
        value: MetricValue::Available(value),
        samples,
        observed_at_secs,
        latency_ms,
    }
}

fn log(
    id: &str,
    observed_at_secs: u64,
    service_id: &str,
    level: &str,
    message: &str,
    redacted_fields: usize,
) -> LogEvent {
    LogEvent {
        id: id.to_owned(),
        source_id: "logs".to_owned(),
        timestamp_ns: observed_at_secs.saturating_mul(1_000_000_000).to_string(),
        service_id: service_id.to_owned(),
        level: level.to_owned(),
        message: message.to_owned(),
        redacted_fields,
    }
}

fn deployment(
    id: &str,
    service_id: &str,
    version: &str,
    status: &str,
    deployed_at_secs: u64,
    duration_ms: u64,
) -> DeploymentSnapshot {
    DeploymentSnapshot {
        id: id.to_owned(),
        service_id: service_id.to_owned(),
        version: version.to_owned(),
        status: status.to_owned(),
        deployed_at_secs,
        duration_ms,
    }
}

fn cost(
    id: &str,
    name: &str,
    provider: &str,
    service_id: Option<&str>,
    monthly_usd: f64,
    confidence: CostConfidence,
) -> CostSnapshot {
    CostSnapshot {
        id: id.to_owned(),
        name: name.to_owned(),
        provider: provider.to_owned(),
        service_id: service_id.map(str::to_owned),
        monthly_usd,
        confidence,
    }
}

fn unix_time_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::monitor::config::LoadedConfig;

    fn request(include_logs: bool, log_limit: usize) -> CollectionRequest {
        CollectionRequest {
            environment: "demo".to_owned(),
            include_logs,
            log_service: None,
            log_lookback_secs: 1_800,
            log_limit,
        }
    }

    #[test]
    fn built_in_scenario_exercises_every_projection() {
        let loaded = LoadedConfig::demo().unwrap();
        let snapshot = snapshot(&loaded.config, &request(true, 200));
        assert_eq!(snapshot.health.state, HealthState::Incident);
        assert!(!snapshot.services.is_empty());
        assert!(!snapshot.performance.is_empty());
        assert!(snapshot.performance.iter().all(|metric| metric.samples.len() == 12));
        assert!(!snapshot.logs.events.is_empty());
        assert!(!snapshot.deployments.entries.is_empty());
        assert!(!snapshot.costs.items.is_empty());
        assert!(!snapshot.sources.is_empty());
    }

    #[test]
    fn demo_logs_honor_service_filter_and_limit() {
        let loaded = LoadedConfig::demo().unwrap();
        let mut request = request(true, 1);
        request.log_service = Some("api".to_owned());
        let snapshot = snapshot(&loaded.config, &request);
        assert_eq!(snapshot.logs.events.len(), 1);
        assert_eq!(snapshot.logs.events[0].service_id, "api");
        assert!(snapshot.logs.truncated);
    }
}
