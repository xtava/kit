use super::{
    config::MetricUnit,
    model::{HealthState, MetricValue, MonitorSnapshot, SourceState},
};

pub fn overview(snapshot: &MonitorSnapshot) {
    println!(
        "{}  {}  {}/{} sources  collected in {}ms",
        snapshot.environment.name,
        health_label(snapshot.health.state),
        snapshot.sources.iter().filter(|source| source.state == SourceState::Ready).count(),
        snapshot.sources.len(),
        snapshot.collection_duration_ms,
    );
    for reason in &snapshot.health.reasons {
        println!("  ! {reason}");
    }
    println!("\nSERVICES");
    services(snapshot, None, usize::MAX);
    println!(
        "\nCOST  ${:.2}/month{}  {}",
        snapshot.costs.monthly_total,
        snapshot
            .costs
            .monthly_budget
            .map(|budget| format!(" / ${budget:.2} budget"))
            .unwrap_or_default(),
        snapshot.costs.detail
    );
}

pub fn services(snapshot: &MonitorSnapshot, state: Option<HealthState>, limit: usize) {
    println!("{:<10} {:<24} {:<12} DETAIL", "STATE", "SERVICE", "LATENCY");
    for service in snapshot
        .services
        .iter()
        .filter(|service| state.is_none_or(|state| service.state == state))
        .take(limit)
    {
        println!(
            "{:<10} {:<24} {:>8}ms  {}",
            health_label(service.state),
            service.name,
            service.latency_ms,
            service.reason
        );
    }
}

pub fn performance(snapshot: &MonitorSnapshot, service: Option<&str>, limit: usize) {
    println!("{:<24} {:<24} {:>14}  STATE", "SERVICE", "METRIC", "VALUE");
    for metric in snapshot
        .performance
        .iter()
        .filter(|metric| service.is_none_or(|service| metric.service_id == service))
        .take(limit)
    {
        println!(
            "{:<24} {:<24} {:>14}  {}",
            metric.service_id,
            metric.name,
            metric_value(&metric.value, metric.unit),
            health_label(metric.state),
        );
    }
}

pub fn logs(snapshot: &MonitorSnapshot, minimum_level: Option<&str>) {
    println!("LOGS  {}  {}", source_label(snapshot.logs.state), snapshot.logs.detail);
    for event in snapshot.logs.events.iter().filter(|event| {
        minimum_level.is_none_or(|minimum| level_severity(&event.level) >= level_severity(minimum))
    }) {
        println!(
            "{}  {:<7} {:<16} {}{}",
            event.timestamp_ns,
            event.level,
            event.service_id,
            event.message,
            if event.redacted_fields > 0 {
                format!("  [redacted:{}]", event.redacted_fields)
            } else {
                String::new()
            }
        );
    }
}

pub fn explain(snapshot: &MonitorSnapshot, service_id: &str) {
    let service = snapshot
        .services
        .iter()
        .find(|service| service.id == service_id)
        .expect("validated service must exist in the snapshot");
    println!("{}  {}  {}", service.name, health_label(service.state), service.reason);
    for metric in snapshot.performance.iter().filter(|metric| metric.service_id == service_id) {
        println!(
            "  {:<24} {:>14}  {}",
            metric.name,
            metric_value(&metric.value, metric.unit),
            health_label(metric.state)
        );
    }
    for warning in &snapshot.warnings {
        println!("  ! {warning}");
    }
}

pub fn deployments(snapshot: &MonitorSnapshot) {
    println!(
        "DEPLOYMENTS  {}  {}",
        source_label(snapshot.deployments.state),
        snapshot.deployments.detail
    );
    for deployment in &snapshot.deployments.entries {
        println!(
            "{}  {:<16} {:<12} {}",
            deployment.deployed_at_secs,
            deployment.service_id,
            deployment.status,
            deployment.version
        );
    }
}

pub fn costs(snapshot: &MonitorSnapshot) {
    println!(
        "COSTS  ${:.2}/month{}  {}",
        snapshot.costs.monthly_total,
        snapshot
            .costs
            .monthly_budget
            .map(|budget| format!(" / ${budget:.2} budget"))
            .unwrap_or_default(),
        snapshot.costs.detail,
    );
    println!("{:<20} {:<20} {:>10}  CONFIDENCE", "PROVIDER", "ITEM", "MONTHLY");
    for item in &snapshot.costs.items {
        println!(
            "{:<20} {:<20} ${:>9.2}  {:?}",
            item.provider, item.name, item.monthly_usd, item.confidence
        );
    }
}

pub fn sources(snapshot: &MonitorSnapshot) {
    println!("{:<10} {:<20} {:<14} {:>10}  DETAIL", "STATE", "SOURCE", "TYPE", "LATENCY");
    for source in &snapshot.sources {
        println!(
            "{:<10} {:<20} {:<14} {:>7}ms  {}{}",
            source_label(source.state),
            source.name,
            source.kind,
            source.latency_ms,
            source.detail,
            if source.required { "  [required]" } else { "" }
        );
    }
}

pub fn health_label(state: HealthState) -> &'static str {
    match state {
        HealthState::Healthy => "HEALTHY",
        HealthState::Degraded => "DEGRADED",
        HealthState::Incident => "INCIDENT",
        HealthState::Unknown => "UNKNOWN",
    }
}

pub fn source_label(state: SourceState) -> &'static str {
    match state {
        SourceState::Ready => "READY",
        SourceState::Partial => "PARTIAL",
        SourceState::Unavailable => "UNAVAILABLE",
        SourceState::Unauthorized => "UNAUTHORIZED",
        SourceState::Error => "ERROR",
    }
}

pub fn metric_value(value: &MetricValue, unit: MetricUnit) -> String {
    match value {
        MetricValue::Available(value) => match unit {
            MetricUnit::Percent => format!("{:.1}%", value * 100.0),
            MetricUnit::Bytes => format_bytes(*value),
            MetricUnit::Milliseconds => format!("{value:.1}ms"),
            MetricUnit::Count => format!("{value:.0}"),
            MetricUnit::RequestsPerSecond => format!("{value:.1}/s"),
        },
        MetricValue::Empty => "—".to_owned(),
        MetricValue::Unavailable(_) => "unavailable".to_owned(),
    }
}

fn format_bytes(value: f64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = value.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1}{}", UNITS[unit])
}

fn level_severity(level: &str) -> u8 {
    match level.to_ascii_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "warn" | "warning" => 3,
        "error" => 4,
        "fatal" => 5,
        _ => 2,
    }
}
