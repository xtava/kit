//! Deterministic, independently observable Codex swarm orchestration.
//!
//! The immutable spec and append-only event Journal are the only run authorities. The detached
//! owner writes events; headless and interactive clients only replay them and publish control
//! requests.

pub mod codex;
pub mod limiter;
pub mod model;
pub mod prompts;
pub mod report;
pub mod runner;
pub mod store;
pub mod tree;
mod tui;

use std::{io, path::PathBuf, time::Duration};

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde::{Deserialize, Serialize};

use crate::framework::process::{ProcessRunId, ProcessSupervisor};
use crate::framework::{Context, Tool, ToolMeta};

use model::{DebatePolicy, ReasoningEffort, RunStatus, Stage, SwarmId, SwarmProjection};
use runner::{RunOwner, SwarmLauncher};
use store::{DiscoveredRun, NewSwarmSpec, SwarmStore};

pub fn tool() -> SwarmTool {
    SwarmTool
}

pub struct SwarmTool;

#[derive(Parser)]
#[command(name = "swarm", about = "Deterministic, independently observable Codex swarms")]
struct SwarmArgs {
    #[command(subcommand)]
    command: Option<SwarmCommand>,
}

#[derive(Subcommand)]
enum SwarmCommand {
    /// Start a swarm and wait for its result by default.
    Run {
        /// Prompt text. When omitted, read the complete prompt from stdin.
        prompt: Option<String>,
        /// Return after the detached owner publishes RunStarted.
        #[arg(long)]
        detach: bool,
        /// Codex model override. Omit to use configured default routing.
        #[arg(long)]
        model: Option<String>,
        /// Model reasoning effort.
        #[arg(long, default_value = "high", value_parser = parse_reasoning)]
        reasoning: ReasoningEffort,
        /// Skip same-thread peer rebuttals.
        #[arg(long)]
        no_debate: bool,
        /// Number of retries after the first failed attempt.
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(0..=5))]
        retry_limit: u8,
        /// Working directory visible to read-only Codex turns.
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// List every persisted swarm without exposing full prompts or streams.
    List,
    /// Show one complete replayed run projection.
    Show { id: String },
    /// Print canonical event records as JSONL.
    Events {
        id: String,
        /// Continue following new complete records until the run is terminal or orphaned.
        #[arg(long)]
        follow: bool,
    },
    /// Wait for a run to become terminal or orphaned.
    Wait { id: String },
    /// Request cancellation and wait for the owner to acknowledge and terminate.
    Cancel { id: String },
    /// Delete one terminal or orphaned run.
    Delete { id: String },
    #[command(name = "__drive", hide = true)]
    Drive { id: String },
}

#[async_trait]
impl Tool for SwarmTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "swarm",
            about: "Deterministic, independently observable Codex swarms",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        SwarmArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = SwarmArgs::from_arg_matches(matches)?;
        match args.command {
            Some(SwarmCommand::Run {
                prompt,
                detach,
                model,
                reasoning,
                no_debate,
                retry_limit,
                cwd,
            }) => {
                let prompt = read_prompt(prompt)?;
                let working_directory = match cwd {
                    Some(path) => path
                        .canonicalize()
                        .with_context(|| format!("resolve working directory {}", path.display()))?,
                    None => std::env::current_dir().context("resolve current working directory")?,
                };
                let store = SwarmStore::bootstrap()?;
                let spec = store.create(NewSwarmSpec {
                    prompt,
                    working_directory,
                    model,
                    reasoning,
                    debate: if no_debate { DebatePolicy::Disabled } else { DebatePolicy::Enabled },
                    retry_limit,
                })?;
                let owner = SwarmLauncher::installed(store.clone(), cx.processes.clone())?
                    .launch(&spec.id)
                    .await?;
                if detach {
                    print_launch(cx, &spec.id, owner)?;
                    return Ok(());
                }
                let projection = wait_for_terminal(&store, &cx.processes, &spec.id, None).await?;
                print_projection(cx, &projection)?;
                terminal_exit(&projection)
            }
            Some(SwarmCommand::List) => {
                let store = SwarmStore::bootstrap()?;
                let mut entries = Vec::new();
                for run in store.discover()? {
                    entries.push(match run {
                        DiscoveredRun::Valid(spec) => {
                            match store.inspect(&cx.processes, &spec.id).await {
                                Ok(projection) => ListEntry::from_projection(&projection),
                                Err(error) => ListEntry {
                                    id: spec.id,
                                    status: ListStatus::Corrupt,
                                    created_at_ms: spec.created_at_ms,
                                    last_sequence: 0,
                                    active_stage: None,
                                    agents: 0,
                                    error: Some(error.to_string()),
                                },
                            }
                        }
                        DiscoveredRun::Corrupt { id, error } => ListEntry {
                            id,
                            status: ListStatus::Corrupt,
                            created_at_ms: 0,
                            last_sequence: 0,
                            active_stage: None,
                            agents: 0,
                            error: Some(error),
                        },
                    });
                }
                if cx.out.is_json() {
                    cx.out.json(&entries)?;
                } else if entries.is_empty() {
                    println!("No swarms.");
                } else {
                    for entry in entries {
                        println!(
                            "{:<12} {:<11} seq {:<6} agents {}",
                            entry.id, entry.status, entry.last_sequence, entry.agents
                        );
                    }
                }
                Ok(())
            }
            Some(SwarmCommand::Show { id }) => {
                let id = SwarmId::new(id)?;
                let projection = SwarmStore::bootstrap()?.inspect(&cx.processes, &id).await?;
                print_projection(cx, &projection)
            }
            Some(SwarmCommand::Events { id, follow }) => {
                let id = SwarmId::new(id)?;
                events(&SwarmStore::bootstrap()?, &cx.processes, &id, follow).await
            }
            Some(SwarmCommand::Wait { id }) => {
                let id = SwarmId::new(id)?;
                let projection =
                    wait_for_terminal(&SwarmStore::bootstrap()?, &cx.processes, &id, None).await?;
                print_projection(cx, &projection)?;
                terminal_exit(&projection)
            }
            Some(SwarmCommand::Cancel { id }) => {
                let id = SwarmId::new(id)?;
                let store = SwarmStore::bootstrap()?;
                let current = store.inspect(&cx.processes, &id).await?;
                if current.status.is_terminal()
                    || matches!(current.status, RunStatus::Orphaned | RunStatus::Unavailable)
                {
                    bail!("swarm {id} is already {:?}", current.status);
                }
                store.request_cancellation(&id)?;
                let projection =
                    wait_for_terminal(&store, &cx.processes, &id, Some(Duration::from_secs(15)))
                        .await?;
                print_projection(cx, &projection)?;
                if projection.status == RunStatus::Cancelled {
                    Ok(())
                } else {
                    terminal_exit(&projection)
                }
            }
            Some(SwarmCommand::Delete { id }) => {
                let id = SwarmId::new(id)?;
                let store = SwarmStore::bootstrap()?;
                store.inspect(&cx.processes, &id).await?;
                store.delete(&id)?;
                if cx.out.is_json() {
                    cx.out.json(&DeleteOutput { id, deleted: true })?;
                } else {
                    println!("Deleted {id}.");
                }
                Ok(())
            }
            Some(SwarmCommand::Drive { id }) => {
                let id = SwarmId::new(id)?;
                let store = SwarmStore::bootstrap()?;
                let spec = store.load_spec(&id)?;
                RunOwner::installed(store, spec.working_directory.clone(), cx.processes.clone())?
                    .drive_detached(&id)
                    .await?;
                Ok(())
            }
            None => {
                if cx.out.is_json() || !cx.term.interactive() {
                    bail!("kit swarm requires an interactive terminal; use a headless subcommand")
                }
                tui::run(SwarmStore::bootstrap()?, cx.processes.clone()).await
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListEntry {
    id: SwarmId,
    status: ListStatus,
    created_at_ms: u64,
    last_sequence: u64,
    active_stage: Option<Stage>,
    agents: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LaunchOutput {
    id: SwarmId,
    status: LaunchStatus,
    process_run_id: ProcessRunId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DeleteOutput {
    id: SwarmId,
    deleted: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LaunchStatus {
    Running,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ListStatus {
    Queued,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    Orphaned,
    Unavailable,
    Corrupt,
}

impl From<RunStatus> for ListStatus {
    fn from(status: RunStatus) -> Self {
        match status {
            RunStatus::Queued => Self::Queued,
            RunStatus::Running => Self::Running,
            RunStatus::Cancelling => Self::Cancelling,
            RunStatus::Succeeded => Self::Succeeded,
            RunStatus::Failed => Self::Failed,
            RunStatus::Cancelled => Self::Cancelled,
            RunStatus::Orphaned => Self::Orphaned,
            RunStatus::Unavailable => Self::Unavailable,
        }
    }
}

impl std::fmt::Display for ListStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Orphaned => "orphaned",
            Self::Unavailable => "unavailable",
            Self::Corrupt => "corrupt",
        })
    }
}

impl ListEntry {
    fn from_projection(projection: &SwarmProjection) -> Self {
        Self {
            id: projection.spec.id.clone(),
            status: projection.status.into(),
            created_at_ms: projection.spec.created_at_ms,
            last_sequence: projection.last_sequence,
            active_stage: projection.active_stage,
            agents: projection.nodes.len(),
            error: None,
        }
    }
}

fn parse_reasoning(value: &str) -> Result<ReasoningEffort, String> {
    match value {
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "high" => Ok(ReasoningEffort::High),
        "xhigh" => Ok(ReasoningEffort::Xhigh),
        _ => Err("expected one of: low, medium, high, xhigh".to_owned()),
    }
}

fn read_prompt(argument: Option<String>) -> Result<String> {
    let prompt = match argument {
        Some(prompt) => prompt,
        None => io::read_to_string(io::stdin()).context("read swarm prompt from stdin")?,
    };
    if prompt.trim().is_empty() {
        bail!("swarm prompt must not be empty");
    }
    Ok(prompt)
}

fn print_launch(cx: &Context, id: &SwarmId, process_run_id: ProcessRunId) -> Result<()> {
    if cx.out.is_json() {
        cx.out.json(&LaunchOutput {
            id: id.clone(),
            status: LaunchStatus::Running,
            process_run_id,
        })?;
    } else {
        println!("Started {id} (owner run {process_run_id}).");
    }
    Ok(())
}

fn print_projection(cx: &Context, projection: &SwarmProjection) -> Result<()> {
    if cx.out.is_json() {
        cx.out.json(projection)?;
    } else {
        println!(
            "{}  {}  sequence {}",
            projection.spec.id,
            status_name(projection.status),
            projection.last_sequence
        );
        if let Some(result) = projection.result.as_ref() {
            println!("\n{}", result.answer);
        }
        if let Some(failure) = projection.failure.as_ref() {
            println!("\n{failure}");
        }
    }
    Ok(())
}

async fn events(
    store: &SwarmStore,
    processes: &ProcessSupervisor,
    id: &SwarmId,
    follow: bool,
) -> Result<()> {
    let initial = store.read_journal(id)?;
    for record in &initial.records {
        println!("{}", serde_json::to_string(record)?);
    }
    if !follow || initial.projection.status.is_terminal() {
        return Ok(());
    }
    let mut tail = store.tail(id)?;
    loop {
        for record in tail.refresh()? {
            println!("{}", serde_json::to_string(&record)?);
        }
        if tail.projection().status.is_terminal()
            || matches!(
                store.inspect(processes, id).await?.status,
                RunStatus::Orphaned | RunStatus::Unavailable
            )
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_terminal(
    store: &SwarmStore,
    processes: &ProcessSupervisor,
    id: &SwarmId,
    timeout: Option<Duration>,
) -> Result<SwarmProjection> {
    let started = tokio::time::Instant::now();
    loop {
        let projection = store.inspect(processes, id).await?;
        if projection.status.is_terminal()
            || matches!(projection.status, RunStatus::Orphaned | RunStatus::Unavailable)
        {
            return Ok(projection);
        }
        if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
            bail!("timed out waiting for swarm {id} to become terminal");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn terminal_exit(projection: &SwarmProjection) -> Result<()> {
    match projection.status {
        RunStatus::Succeeded => Ok(()),
        RunStatus::Failed => bail!("swarm {} failed", projection.spec.id),
        RunStatus::Cancelled => bail!("swarm {} was cancelled", projection.spec.id),
        RunStatus::Orphaned => {
            bail!("swarm {} owner exited without a terminal event", projection.spec.id)
        }
        RunStatus::Unavailable => {
            bail!("swarm {} owner status is currently unavailable", projection.spec.id)
        }
        RunStatus::Queued | RunStatus::Running | RunStatus::Cancelling => {
            bail!("swarm {} is not terminal", projection.spec.id)
        }
    }
}

fn status_name(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::Cancelling => "cancelling",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::Orphaned => "orphaned",
        RunStatus::Unavailable => "unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_grammar_covers_headless_and_interactive_entrypoints() {
        assert!(SwarmArgs::try_parse_from(["swarm"]).unwrap().command.is_none());
        assert!(matches!(
            SwarmArgs::try_parse_from([
                "swarm",
                "run",
                "question",
                "--detach",
                "--reasoning",
                "xhigh",
                "--no-debate",
                "--retry-limit",
                "3",
            ])
            .unwrap()
            .command,
            Some(SwarmCommand::Run {
                prompt: Some(prompt),
                detach: true,
                reasoning: ReasoningEffort::Xhigh,
                no_debate: true,
                retry_limit: 3,
                ..
            }) if prompt == "question"
        ));
        for command in ["list", "show", "events", "wait", "cancel", "delete", "__drive"] {
            let arguments = match command {
                "list" => vec!["swarm", command],
                _ => vec!["swarm", command, "swarm-1"],
            };
            assert!(SwarmArgs::try_parse_from(arguments).is_ok(), "{command}");
        }
    }

    #[test]
    fn prompt_argument_is_validated_without_shell_or_argv_projection() {
        assert_eq!(read_prompt(Some("full prompt".to_owned())).unwrap(), "full prompt");
        assert!(read_prompt(Some("   ".to_owned())).is_err());
    }

    #[test]
    fn headless_summary_contracts_are_named_closed_and_stable() {
        let id = SwarmId::new("swarm-7").unwrap();
        let process_run_id = ProcessRunId::new();
        let launch = LaunchOutput { id: id.clone(), status: LaunchStatus::Running, process_run_id };
        let launch_json = serde_json::to_value(&launch).unwrap();
        assert_eq!(
            launch_json,
            serde_json::json!({
                "id": "swarm-7",
                "status": "running",
                "process_run_id": process_run_id.to_string()
            })
        );
        assert_eq!(serde_json::from_value::<LaunchOutput>(launch_json).unwrap(), launch);

        let deleted = DeleteOutput { id, deleted: true };
        let deleted_json = serde_json::to_value(&deleted).unwrap();
        assert_eq!(deleted_json, serde_json::json!({ "id": "swarm-7", "deleted": true }));
        assert_eq!(serde_json::from_value::<DeleteOutput>(deleted_json).unwrap(), deleted);
        assert!(serde_json::from_value::<DeleteOutput>(serde_json::json!({
            "id": "swarm-7",
            "deleted": true,
            "unexpected": 1
        }))
        .is_err());
    }
}
