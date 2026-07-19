//! monitor — config-driven, read-only production operations console.

mod config;
mod contributions;
mod demo;
mod model;
mod report;
mod sources;
mod tui;

use std::{path::PathBuf, time::Duration};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use serde::Serialize;

use crate::framework::{Context, Tool, ToolMeta};

use config::LoadedConfig;
use model::{HealthState, MonitorSnapshot, SNAPSHOT_SCHEMA_VERSION};
use sources::{CollectionRequest, Collector};

pub fn tool() -> MonitorTool {
    MonitorTool
}

pub struct MonitorTool;

#[derive(Parser)]
#[command(
    name = "monitor",
    about = "Read-only production health, performance, logs, deployments, and cost console"
)]
struct MonitorArgs {
    /// Use deterministic built-in data for visualization and workflow development.
    #[arg(long, conflicts_with_all = ["config", "environment"])]
    demo: bool,

    /// Load this monitor configuration instead of project-local or XDG configuration.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Select an environment from monitor.toml.
    #[arg(long, value_name = "ID")]
    environment: Option<String>,

    /// Keep terminal mouse reporting disabled; keyboard controls remain available.
    #[arg(long)]
    no_mouse: bool,

    #[command(subcommand)]
    view: Option<MonitorViewArgs>,
}

#[derive(Clone, Debug, Subcommand)]
enum MonitorViewArgs {
    /// Production briefing: health, services, sources, and recurring cost.
    Overview,
    /// Service health rows.
    Services {
        #[arg(long)]
        state: Option<HealthFilter>,
        #[arg(long, default_value_t = 100, value_parser = parse_limit)]
        limit: usize,
    },
    /// Configured Prometheus metrics.
    Performance {
        #[arg(long)]
        service: Option<String>,
        #[arg(long, default_value_t = 100, value_parser = parse_limit)]
        limit: usize,
    },
    /// Bounded Loki events when a Loki source is configured.
    Logs {
        #[arg(long)]
        service: Option<String>,
        #[arg(long, value_enum)]
        level: Option<LogLevelFilter>,
        #[arg(long, default_value = "30m", value_parser = parse_duration)]
        since: Duration,
        #[arg(long, default_value_t = 200, value_parser = parse_limit)]
        limit: usize,
    },
    /// Read-only deployment history when a deployment source is available.
    Deployments {
        #[arg(long)]
        service: Option<String>,
        #[arg(long, default_value_t = 100, value_parser = parse_limit)]
        limit: usize,
    },
    /// Configured recurring costs and confidence.
    Costs,
    /// Monitoring source readiness, authentication, and freshness.
    Sources,
    /// Explain one service's health from collected evidence.
    Explain {
        #[arg(long)]
        service: String,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum HealthFilter {
    Healthy,
    Degraded,
    Incident,
    Unknown,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LogLevelFilter {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl LogLevelFilter {
    fn includes(self, level: &str) -> bool {
        log_severity(level) >= self.severity()
    }

    const fn severity(self) -> u8 {
        match self {
            Self::Trace => 0,
            Self::Debug => 1,
            Self::Info => 2,
            Self::Warn => 3,
            Self::Error => 4,
            Self::Fatal => 5,
        }
    }
}

impl From<HealthFilter> for HealthState {
    fn from(value: HealthFilter) -> Self {
        match value {
            HealthFilter::Healthy => Self::Healthy,
            HealthFilter::Degraded => Self::Degraded,
            HealthFilter::Incident => Self::Incident,
            HealthFilter::Unknown => Self::Unknown,
        }
    }
}

#[async_trait]
impl Tool for MonitorTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "monitor",
            about: "Read-only production operations console",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        MonitorArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = MonitorArgs::from_arg_matches(matches)?;
        let project_dir = std::env::current_dir().context("resolve current directory")?;
        let loaded = if args.demo {
            LoadedConfig::demo()?
        } else {
            LoadedConfig::load(args.config, project_dir, cx.config.path("monitor"))?
        };
        let environment =
            args.environment.unwrap_or_else(|| loaded.config.default_environment.clone());
        let collector = if args.demo { Collector::demo() } else { Collector::new(&loaded.config)? };

        if args.view.is_none() && cx.term.interactive() && !cx.out.is_json() {
            return tui::run(loaded, environment, collector, !args.no_mouse).await;
        }

        let view = args.view.unwrap_or(MonitorViewArgs::Overview);
        let request = collection_request(&loaded.config, &environment, &view)?;
        let snapshot = collector.collect(&loaded.config, &request).await?;
        if cx.out.is_json() {
            emit_json(cx, &snapshot, &view)?;
        } else {
            emit_text(&snapshot, &view);
        }
        Ok(())
    }
}

fn collection_request(
    config: &config::MonitorConfig,
    environment: &str,
    view: &MonitorViewArgs,
) -> Result<CollectionRequest> {
    if !config.environments.iter().any(|candidate| candidate.id == environment) {
        anyhow::bail!("environment '{environment}' is not configured");
    }
    let selected_service = match view {
        MonitorViewArgs::Performance { service, .. }
        | MonitorViewArgs::Logs { service, .. }
        | MonitorViewArgs::Deployments { service, .. } => service.as_deref(),
        MonitorViewArgs::Explain { service } => Some(service.as_str()),
        _ => None,
    };
    if let Some(service) = selected_service {
        if !config
            .services
            .iter()
            .any(|candidate| candidate.environment == environment && candidate.id == service)
        {
            anyhow::bail!("service '{service}' is not configured in environment '{environment}'");
        }
    }
    let (include_logs, log_service, lookback, limit) = match view {
        MonitorViewArgs::Logs { service, since, limit, .. } => {
            if since.as_secs() > config.limits.max_lookback_hours * 3_600 {
                anyhow::bail!(
                    "--since exceeds configured maximum of {} hours",
                    config.limits.max_lookback_hours
                );
            }
            (true, service.clone(), since.as_secs(), *limit)
        }
        _ => (false, None, 30 * 60, config.limits.max_log_events.min(200)),
    };
    Ok(CollectionRequest {
        environment: environment.to_owned(),
        include_logs,
        log_service,
        log_lookback_secs: lookback,
        log_limit: limit.min(config.limits.max_log_events),
    })
}

fn emit_text(snapshot: &MonitorSnapshot, view: &MonitorViewArgs) {
    match view {
        MonitorViewArgs::Overview => report::overview(snapshot),
        MonitorViewArgs::Services { state, limit } => {
            report::services(snapshot, state.map(HealthState::from), *limit)
        }
        MonitorViewArgs::Performance { service, limit } => {
            report::performance(snapshot, service.as_deref(), *limit)
        }
        MonitorViewArgs::Logs { level, .. } => {
            report::logs(snapshot, level.map(|level| level.as_str()))
        }
        MonitorViewArgs::Deployments { .. } => report::deployments(snapshot),
        MonitorViewArgs::Costs => report::costs(snapshot),
        MonitorViewArgs::Sources => report::sources(snapshot),
        MonitorViewArgs::Explain { service } => report::explain(snapshot, service),
    }
}

fn emit_json(cx: &Context, snapshot: &MonitorSnapshot, view: &MonitorViewArgs) -> Result<()> {
    let projection = match view {
        MonitorViewArgs::Overview => MonitorProjection::Overview(OverviewProjection {
            health: &snapshot.health,
            services: &snapshot.services,
            performance: &snapshot.performance,
            costs: &snapshot.costs,
            sources: &snapshot.sources,
            deployments: &snapshot.deployments,
        }),
        MonitorViewArgs::Services { state, limit } => MonitorProjection::Services(
            snapshot
                .services
                .iter()
                .filter(|service| {
                    state.is_none_or(|state| service.state == HealthState::from(state))
                })
                .take(*limit)
                .collect(),
        ),
        MonitorViewArgs::Performance { service, limit } => MonitorProjection::Performance(
            snapshot
                .performance
                .iter()
                .filter(|metric| {
                    service.as_ref().is_none_or(|service| &metric.service_id == service)
                })
                .take(*limit)
                .collect(),
        ),
        MonitorViewArgs::Logs { level, .. } => MonitorProjection::Logs(LogProjection {
            state: snapshot.logs.state,
            detail: &snapshot.logs.detail,
            events: snapshot
                .logs
                .events
                .iter()
                .filter(|event| level.is_none_or(|level| level.includes(&event.level)))
                .collect(),
            truncated: snapshot.logs.truncated,
            limit: snapshot.logs.limit,
        }),
        MonitorViewArgs::Deployments { service, limit } => MonitorProjection::Deployments(
            snapshot
                .deployments
                .entries
                .iter()
                .filter(|deployment| {
                    service.as_ref().is_none_or(|service| &deployment.service_id == service)
                })
                .take(*limit)
                .collect(),
        ),
        MonitorViewArgs::Costs => MonitorProjection::Costs(&snapshot.costs),
        MonitorViewArgs::Sources => MonitorProjection::Sources(&snapshot.sources),
        MonitorViewArgs::Explain { service } => MonitorProjection::Explain(ExplainProjection {
            service: snapshot.services.iter().find(|candidate| candidate.id == *service),
            metrics: snapshot
                .performance
                .iter()
                .filter(|metric| metric.service_id == *service)
                .collect(),
            warnings: &snapshot.warnings,
        }),
    };
    cx.out.json(&MonitorEnvelope {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        environment: &snapshot.environment.id,
        observed_at_secs: snapshot.observed_at_secs,
        warnings: &snapshot.warnings,
        data: projection,
    })
}

#[derive(Serialize)]
struct MonitorEnvelope<'a> {
    schema_version: u32,
    environment: &'a str,
    observed_at_secs: u64,
    warnings: &'a [String],
    data: MonitorProjection<'a>,
}

#[derive(Serialize)]
#[serde(tag = "view", content = "value", rename_all = "kebab-case")]
enum MonitorProjection<'a> {
    Overview(OverviewProjection<'a>),
    Services(Vec<&'a model::ServiceSnapshot>),
    Performance(Vec<&'a model::MetricSnapshot>),
    Logs(LogProjection<'a>),
    Deployments(Vec<&'a model::DeploymentSnapshot>),
    Costs(&'a model::CostSummary),
    Sources(&'a [model::SourceSnapshot]),
    Explain(ExplainProjection<'a>),
}

#[derive(Serialize)]
struct OverviewProjection<'a> {
    health: &'a model::HealthSummary,
    services: &'a [model::ServiceSnapshot],
    performance: &'a [model::MetricSnapshot],
    costs: &'a model::CostSummary,
    sources: &'a [model::SourceSnapshot],
    deployments: &'a model::DeploymentCollection,
}

#[derive(Serialize)]
struct LogProjection<'a> {
    state: model::SourceState,
    detail: &'a str,
    events: Vec<&'a model::LogEvent>,
    truncated: bool,
    limit: usize,
}

#[derive(Serialize)]
struct ExplainProjection<'a> {
    service: Option<&'a model::ServiceSnapshot>,
    metrics: Vec<&'a model::MetricSnapshot>,
    warnings: &'a [String],
}

fn parse_duration(raw: &str) -> Result<Duration, String> {
    if raw.len() < 2 {
        return Err("duration must use s, m, h, or d (for example 30m)".to_owned());
    }
    let (number, suffix) = raw.split_at(raw.len() - 1);
    let value = number.parse::<u64>().map_err(|_| "duration must start with an integer")?;
    if value == 0 {
        return Err("duration must be greater than zero".to_owned());
    }
    let multiplier = match suffix {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err("duration must use s, m, h, or d (for example 30m)".to_owned()),
    };
    value
        .checked_mul(multiplier)
        .map(Duration::from_secs)
        .ok_or_else(|| "duration is too large".to_owned())
}

fn parse_limit(raw: &str) -> Result<usize, String> {
    let value = raw.parse::<usize>().map_err(|_| "limit must be an integer")?;
    if (1..=5_000).contains(&value) {
        Ok(value)
    } else {
        Err("limit must be between 1 and 5000".to_owned())
    }
}

impl LogLevelFilter {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Fatal => "fatal",
        }
    }
}

fn log_severity(level: &str) -> u8 {
    match level.to_ascii_lowercase().as_str() {
        "trace" => 0,
        "debug" => 1,
        "warn" | "warning" => 3,
        "error" => 4,
        "fatal" => 5,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bounded_duration_vocabulary() {
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1_800));
        assert!(parse_duration("30").is_err());
        assert!(parse_duration("0m").is_err());
    }
}
