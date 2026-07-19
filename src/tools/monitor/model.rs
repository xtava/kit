use serde::Serialize;

use super::config::{CostConfidence, MetricUnit};

pub const SNAPSHOT_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MonitorView {
    Overview,
    Services,
    Performance,
    Logs,
    Deployments,
    Costs,
    Sources,
}

impl MonitorView {
    pub const ALL: [Self; 7] = [
        Self::Overview,
        Self::Services,
        Self::Performance,
        Self::Logs,
        Self::Deployments,
        Self::Costs,
        Self::Sources,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Services => "Services",
            Self::Performance => "Performance",
            Self::Logs => "Logs",
            Self::Deployments => "Deployments",
            Self::Costs => "Costs",
            Self::Sources => "Sources",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HealthState {
    Healthy,
    Degraded,
    Incident,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceState {
    Ready,
    Partial,
    Unavailable,
    Unauthorized,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct MonitorSnapshot {
    pub schema_version: u32,
    pub environment: EnvironmentSnapshot,
    pub observed_at_secs: u64,
    pub collection_duration_ms: u64,
    pub health: HealthSummary,
    pub services: Vec<ServiceSnapshot>,
    pub performance: Vec<MetricSnapshot>,
    pub logs: LogCollection,
    pub deployments: DeploymentCollection,
    pub costs: CostSummary,
    pub sources: Vec<SourceSnapshot>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EnvironmentSnapshot {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct HealthSummary {
    pub state: HealthState,
    pub healthy_services: usize,
    pub degraded_services: usize,
    pub incident_services: usize,
    pub unknown_services: usize,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ServiceSnapshot {
    pub id: String,
    pub name: String,
    pub state: HealthState,
    pub reason: String,
    pub source_id: String,
    pub observed_at_secs: u64,
    pub latency_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricSnapshot {
    pub id: String,
    pub service_id: String,
    pub name: String,
    pub source_id: String,
    pub unit: MetricUnit,
    pub state: HealthState,
    pub value: MetricValue,
    pub samples: Vec<MetricSample>,
    pub observed_at_secs: u64,
    pub latency_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricSample {
    pub observed_at_secs: u64,
    pub value: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", content = "value", rename_all = "kebab-case")]
pub enum MetricValue {
    Available(f64),
    Empty,
    Unavailable(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct SourceSnapshot {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub required: bool,
    pub state: SourceState,
    pub detail: String,
    pub observed_at_secs: u64,
    pub latency_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogCollection {
    pub state: SourceState,
    pub events: Vec<LogEvent>,
    pub detail: String,
    pub truncated: bool,
    pub limit: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogEvent {
    pub id: String,
    pub source_id: String,
    pub timestamp_ns: String,
    pub service_id: String,
    pub level: String,
    pub message: String,
    pub redacted_fields: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeploymentCollection {
    pub state: SourceState,
    pub entries: Vec<DeploymentSnapshot>,
    pub detail: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct DeploymentSnapshot {
    pub id: String,
    pub service_id: String,
    pub version: String,
    pub status: String,
    pub deployed_at_secs: u64,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CostSummary {
    pub currency: String,
    pub monthly_total: f64,
    pub monthly_budget: Option<f64>,
    pub budget_percent: Option<f64>,
    pub state: SourceState,
    pub items: Vec<CostSnapshot>,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CostSnapshot {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub service_id: Option<String>,
    pub monthly_usd: f64,
    pub confidence: CostConfidence,
}

pub fn summarize_health(services: &[ServiceSnapshot], sources: &[SourceSnapshot]) -> HealthSummary {
    let healthy_services =
        services.iter().filter(|service| service.state == HealthState::Healthy).count();
    let degraded_services =
        services.iter().filter(|service| service.state == HealthState::Degraded).count();
    let incident_services =
        services.iter().filter(|service| service.state == HealthState::Incident).count();
    let unknown_services =
        services.iter().filter(|service| service.state == HealthState::Unknown).count();
    let mut reasons = services
        .iter()
        .filter(|service| service.state != HealthState::Healthy)
        .map(|service| format!("{}: {}", service.name, service.reason))
        .collect::<Vec<_>>();
    reasons.extend(
        sources
            .iter()
            .filter(|source| source.required && source.state != SourceState::Ready)
            .map(|source| format!("{}: {}", source.name, source.detail)),
    );
    let state = if services.is_empty() {
        reasons.push("no services are configured for this environment".to_owned());
        HealthState::Unknown
    } else if incident_services > 0 {
        HealthState::Incident
    } else if degraded_services > 0 {
        HealthState::Degraded
    } else if unknown_services > 0
        || sources.iter().any(|source| source.required && source.state != SourceState::Ready)
    {
        HealthState::Unknown
    } else {
        HealthState::Healthy
    };
    HealthSummary {
        state,
        healthy_services,
        degraded_services,
        incident_services,
        unknown_services,
        reasons,
    }
}

pub fn metric_health(value: f64, warning: Option<f64>, critical: Option<f64>) -> HealthState {
    if critical.is_some_and(|threshold| value >= threshold) {
        HealthState::Incident
    } else if warning.is_some_and(|threshold| value >= threshold) {
        HealthState::Degraded
    } else {
        HealthState::Healthy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_source_failure_prevents_healthy_rollup() {
        let service = ServiceSnapshot {
            id: "api".into(),
            name: "API".into(),
            state: HealthState::Healthy,
            reason: "HTTP 200".into(),
            source_id: "api".into(),
            observed_at_secs: 1,
            latency_ms: 2,
        };
        let source = SourceSnapshot {
            id: "metrics".into(),
            name: "Metrics".into(),
            kind: "prometheus".into(),
            required: true,
            state: SourceState::Error,
            detail: "request failed".into(),
            observed_at_secs: 1,
            latency_ms: 2,
        };
        assert_eq!(summarize_health(&[service], &[source]).state, HealthState::Unknown);
    }
}
