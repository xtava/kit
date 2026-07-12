//! record — operator shell for Modular Playwright recorder artifacts.
//!
//! Kit owns the operator loop; Modular owns the Playwright recorder, snapshots, and artifacts.

mod tui;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context as AnyhowContext, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tokio::process::Command as TokioCommand;

use crate::framework::{Context, Tool, ToolMeta};

const TOOL: &str = "record";
const DEFAULT_SCENARIO: &str = "workspace-layout-dnd";

/// Persistent config for `kit record`, stored at the framework config path (`record.toml`).
/// The repo has no built-in default — it is machine-specific — and an unset scenario falls back to
/// [`DEFAULT_SCENARIO`].
#[derive(Debug, Default, Deserialize, Serialize)]
struct RecordConfig {
    #[serde(default)]
    repo: Option<PathBuf>,
    #[serde(default)]
    scenario: Option<String>,
}

pub fn tool() -> RecordTool {
    RecordTool
}

pub struct RecordTool;

#[derive(Parser)]
#[command(
    name = "record",
    about = "Operate Modular Playwright recorder runs",
    long_about = "Starts, stops, replays, and inspects Modular Playwright recorder runs while keeping the recorder implementation in the Modular repo."
)]
struct RecordArgs {
    #[command(subcommand)]
    command: Option<RecordCommand>,

    /// Open the record REPL instead of running one command.
    #[arg(short, long)]
    interactive: bool,

    /// Modular checkout that owns pnpm record and recorder artifacts. Defaults to the `repo` set in
    /// `record.toml`.
    #[arg(long, global = true)]
    repo: Option<PathBuf>,

    /// Recorder scenario id. Defaults to the `scenario` in `record.toml`, else the built-in fallback.
    #[arg(long, global = true)]
    scenario: Option<String>,
}

#[derive(Subcommand)]
enum RecordCommand {
    /// Start recording. Use `kit record stop` or `stop` in the REPL to finish.
    Start {
        /// Output directory passed through to `pnpm record -- --out`.
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Ask the current recorder run to stop and flush artifacts.
    Stop,

    /// Cancel the current recorder run and close its Electron window without finalizing artifacts.
    Cancel,

    /// Replay the current or provided recording.
    Replay {
        /// Recording directory. Defaults to the current recording for this scenario.
        dir: Option<PathBuf>,
    },

    /// Show current run state and artifact location.
    Status,

    /// Summarize the physical-events.jsonl recording.
    Events,

    /// List files in the current recording artifact directory.
    Artifacts,

    /// Move the current recording into a stable saved recording directory.
    Rename {
        /// Saved recording name.
        name: String,
    },
}

#[async_trait]
impl Tool for RecordTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "record",
            about: "Operate Modular Playwright recorder runs",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        RecordArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = RecordArgs::from_arg_matches(matches)?;
        let config: RecordConfig = cx.config.load(TOOL)?;

        let repo = args.repo.or(config.repo).ok_or_else(|| {
            anyhow!(
                "no Modular repo configured — set `repo` in {} or pass --repo <path>",
                cx.config.path(TOOL).display()
            )
        })?;
        let repo = normalize_repo(repo)?;
        let scenario =
            args.scenario.or(config.scenario).unwrap_or_else(|| DEFAULT_SCENARIO.to_owned());

        if args.interactive
            || (args.command.is_none() && cx.term.interactive() && !cx.out.is_json())
        {
            return tui::run(repo, scenario).await;
        }

        let command = args.command.unwrap_or(RecordCommand::Status);
        dispatch_command(cx, &repo, &scenario, command).await
    }
}

async fn dispatch_command(
    cx: &Context,
    repo: &Path,
    scenario: &str,
    command: RecordCommand,
) -> Result<()> {
    match command {
        RecordCommand::Start { out } => {
            run_modular_command(repo, record_args(scenario, out.as_deref())).await
        }
        RecordCommand::Stop => run_modular_command(repo, stop_args(scenario)).await,
        RecordCommand::Cancel => run_modular_command(repo, cancel_args(scenario)).await,
        RecordCommand::Replay { dir } => {
            run_modular_command(repo, replay_args(scenario, dir.as_deref())).await
        }
        RecordCommand::Status => print_status(cx, repo, scenario),
        RecordCommand::Events => print_events(cx, repo, scenario),
        RecordCommand::Artifacts => print_artifacts(cx, repo, scenario),
        RecordCommand::Rename { name } => {
            let target = rename_current_recording(repo, scenario, &name)?;
            if cx.out.is_json() {
                cx.out.json(&serde_json::json!({ "savedDir": target }))?;
            } else {
                println!("saved recording: {}", target.display());
            }
            Ok(())
        }
    }
}

async fn run_modular_command(repo: &Path, args: Vec<String>) -> Result<()> {
    let status = TokioCommand::new("pnpm")
        .current_dir(repo)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("failed to run pnpm in {}", repo.display()))?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("pnpm command exited with {status}"))
    }
}

fn record_args(scenario: &str, out: Option<&Path>) -> Vec<String> {
    let mut args =
        vec!["record".to_owned(), "--".to_owned(), "--scenario".to_owned(), scenario.to_owned()];
    if let Some(out) = out {
        args.push("--out".to_owned());
        args.push(out.display().to_string());
    }
    args
}

fn stop_args(scenario: &str) -> Vec<String> {
    vec!["record-stop".to_owned(), "--".to_owned(), "--scenario".to_owned(), scenario.to_owned()]
}

fn cancel_args(scenario: &str) -> Vec<String> {
    vec!["record-cancel".to_owned(), "--".to_owned(), "--scenario".to_owned(), scenario.to_owned()]
}

fn replay_args(scenario: &str, dir: Option<&Path>) -> Vec<String> {
    let mut args = vec![
        "record".to_owned(),
        "--".to_owned(),
        "--scenario".to_owned(),
        scenario.to_owned(),
        "--replay".to_owned(),
    ];
    if let Some(dir) = dir {
        args.push(dir.display().to_string());
    }
    args
}

fn normalize_repo(repo: PathBuf) -> Result<PathBuf> {
    let repo = repo
        .canonicalize()
        .with_context(|| format!("could not resolve repo path {}", repo.display()))?;
    let package_json = repo.join("package.json");
    if !package_json.exists() {
        return Err(anyhow!("{} is not a Modular repo root with package.json", repo.display()));
    }
    Ok(repo)
}

fn current_recording_dir(repo: &Path, scenario: &str) -> PathBuf {
    repo.join("artifacts")
        .join("e2e-recordings")
        .join("current")
        .join(format!("instance-{}", resolve_instance_id(repo)))
        .join(scenario)
}

fn saved_recording_root(repo: &Path) -> PathBuf {
    repo.join("artifacts")
        .join("e2e-recordings")
        .join("saved")
        .join(format!("instance-{}", resolve_instance_id(repo)))
}

fn rename_current_recording(repo: &Path, scenario: &str, name: &str) -> Result<PathBuf> {
    let name = sanitize_recording_name(name)?;
    let source = current_recording_dir(repo, scenario);
    if !source.exists() {
        return Err(anyhow!("current recording does not exist: {}", source.display()));
    }

    let target = saved_recording_root(repo).join(name);
    if target.exists() {
        return Err(anyhow!("saved recording already exists: {}", target.display()));
    }

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    fs::rename(&source, &target)
        .with_context(|| format!("failed to move {} to {}", source.display(), target.display()))?;
    Ok(target)
}

fn sanitize_recording_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("recording name cannot be empty"));
    }
    if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(anyhow!("recording name must be a single path segment"));
    }
    Ok(name.to_owned())
}

fn resolve_instance_id(repo: &Path) -> u32 {
    if let Ok(raw) = std::env::var("INSTANCE_ID") {
        if let Ok(id) = raw.parse::<u32>() {
            return id;
        }
    }

    let Ok(contents) = fs::read_to_string(repo.join(".worktree-env")) else {
        return 0;
    };

    for line in contents.lines() {
        let line = line.trim();
        let Some(raw) = line.strip_prefix("INSTANCE_ID=") else {
            continue;
        };
        if let Ok(id) = raw.trim_matches('"').parse::<u32>() {
            return id;
        }
    }

    0
}

fn print_status(cx: &Context, repo: &Path, scenario: &str) -> Result<()> {
    if cx.out.is_json() {
        cx.out.json(&status_report(repo, scenario)?)?;
    } else {
        print_status_text(repo, scenario)?;
    }
    Ok(())
}

fn print_status_text(repo: &Path, scenario: &str) -> Result<()> {
    let report = status_report(repo, scenario)?;
    println!("artifact dir: {}", report.artifact_dir.display());
    match report.run_state {
        Some(state) => {
            let status = state.status.unwrap_or_else(|| "unknown".to_owned());
            let status = if status == "running" && report.process_alive == Some(false) {
                "running (stale pid)".to_owned()
            } else {
                status
            };
            println!("status: {status}");
            if let Some(pid) = state.pid {
                println!("pid: {pid}");
            }
            if let Some(started_at) = state.started_at {
                println!("started: {started_at}");
            }
            if let Some(finished_at) = state.finished_at {
                println!("finished: {finished_at}");
            }
            if let Some(error) = state.error {
                println!("error: {error}");
            }
        }
        None => println!("run-state: missing"),
    }
    Ok(())
}

fn print_events(cx: &Context, repo: &Path, scenario: &str) -> Result<()> {
    if cx.out.is_json() {
        cx.out.json(&events_summary(repo, scenario)?)?;
    } else {
        print_events_text(repo, scenario)?;
    }
    Ok(())
}

fn print_events_text(repo: &Path, scenario: &str) -> Result<()> {
    let summary = events_summary(repo, scenario)?;
    println!("events: {}", summary.path.display());
    println!("total: {}", summary.total);
    for (event_type, count) in summary.counts {
        println!("  {event_type:<16} {count}");
    }
    Ok(())
}

fn print_artifacts(cx: &Context, repo: &Path, scenario: &str) -> Result<()> {
    if cx.out.is_json() {
        cx.out.json(&artifacts_report(repo, scenario)?)?;
    } else {
        print_artifacts_text(repo, scenario)?;
    }
    Ok(())
}

fn print_artifacts_text(repo: &Path, scenario: &str) -> Result<()> {
    let report = artifacts_report(repo, scenario)?;
    println!("artifact dir: {}", report.dir.display());
    if report.files.is_empty() {
        println!("no files");
    }
    for file in report.files {
        println!("{:>10}  {}", file.bytes, file.name);
    }
    Ok(())
}

fn status_report(repo: &Path, scenario: &str) -> Result<StatusReport> {
    let artifact_dir = current_recording_dir(repo, scenario);
    let run_state_path = artifact_dir.join("run-state.json");
    let run_state = if run_state_path.exists() {
        let contents = fs::read_to_string(&run_state_path)
            .with_context(|| format!("failed to read {}", run_state_path.display()))?;
        Some(
            serde_json::from_str::<RunState>(&contents)
                .with_context(|| format!("failed to parse {}", run_state_path.display()))?,
        )
    } else {
        None
    };
    let process_alive = run_state.as_ref().and_then(|state| state.pid).map(is_process_alive);

    Ok(StatusReport { artifact_dir, run_state, process_alive })
}

fn events_summary(repo: &Path, scenario: &str) -> Result<EventsSummary> {
    let path = current_recording_dir(repo, scenario).join("physical-events.jsonl");
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;

    let mut total = 0usize;
    let mut counts = BTreeMap::new();
    for (index, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str::<RecordedEvent>(line)
            .with_context(|| format!("failed to parse {} line {}", path.display(), index + 1))?;
        total += 1;
        *counts.entry(event.event_type).or_insert(0) += 1;
    }

    Ok(EventsSummary { path, total, counts })
}

fn artifacts_report(repo: &Path, scenario: &str) -> Result<ArtifactsReport> {
    let dir = current_recording_dir(repo, scenario);
    let mut files = Vec::new();
    if dir.exists() {
        for entry in
            fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))?
        {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_file() {
                files.push(ArtifactFile {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    bytes: metadata.len(),
                });
            }
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(ArtifactsReport { dir, files })
}

#[derive(Debug, Serialize)]
struct StatusReport {
    artifact_dir: PathBuf,
    run_state: Option<RunState>,
    process_alive: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunState {
    pid: Option<u32>,
    scenario_id: Option<String>,
    output_dir: Option<PathBuf>,
    instance_id: Option<u32>,
    status: Option<String>,
    started_at: Option<String>,
    finished_at: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct EventsSummary {
    path: PathBuf,
    total: usize,
    counts: BTreeMap<String, usize>,
}

#[derive(Debug, Deserialize)]
struct RecordedEvent {
    #[serde(rename = "type")]
    event_type: String,
}

#[derive(Debug, Serialize)]
struct ArtifactsReport {
    dir: PathBuf,
    files: Vec<ArtifactFile>,
}

#[derive(Debug, Serialize)]
struct ArtifactFile {
    name: String,
    bytes: u64,
}

fn is_process_alive(pid: u32) -> bool {
    if unsafe { libc::kill(pid as i32, 0) == 0 } {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
