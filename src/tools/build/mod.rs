//! `kit build` is a thin client for a repository-owned build provider.
//!
//! Kit owns the supervised provider process, transcripts, request publication, and protocol
//! validation. The repository provider owns every build decision: graph, cache, artifacts,
//! verification, concurrency, and resume policy.

mod evidence;
mod manifest;
mod protocol;
mod tui;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsString,
    io::Write as _,
    num::{NonZeroU64, NonZeroUsize},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Args, Command, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde::Serialize;
use tokio::sync::{mpsc, watch};

use crate::framework::{
    process::{
        CommandSpec, CompletionCause, ContainmentRequirement, ControlAcknowledgement,
        EnvironmentBase, InputPolicy, LeaderExit, LeaderExitObservation, OutputPolicy,
        OutputReport, PartialOutputReport, ProcessDeadline, ProcessEnvironment, ProcessLabel,
        ProcessOutputError, ProcessOutputHandle, ProcessSpec, RecordAvailability,
        RecordDisposition, RecordLimit, RecordOverflow, RecordPolicy, RecordTailEvent,
        RecordedOutputTail, TerminationPolicy, UnavailableOutput,
    },
    AtomicFileWriter, Context, Tool, ToolMeta, WorktreeRoot,
};

use manifest::{LoadedBuildManifest, Workflow};
use protocol::{
    provider_schema, read_final_result, BuildEvent, BuildEventKind, BuildEventStreamReader,
    BuildFinalResult, BuildOutcome, BuildRequest, StageOutcome, BUILD_PROTOCOL_VERSION,
};

const EVENT_READ_INTERVAL: Duration = Duration::from_millis(100);
const FAILURE_TRANSCRIPT_TAIL_BYTES: usize = 16 * 1024;
const MAX_DURABLE_TRANSCRIPT_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_BUILD_DEADLINE: Duration = Duration::from_secs(2 * 60 * 60);
const TERMINATION_GRACE: Duration = Duration::from_secs(10);
const BUILD_UPDATE_CAPACITY: usize = 256;
const BUILD_EVENT_PRESENTATION_BATCH: usize = 256;

pub fn tool() -> BuildTool {
    BuildTool
}

pub struct BuildTool;

#[derive(Parser)]
#[command(
    name = "build",
    about = "Run a repository-owned build provider under Kit supervision",
    long_about = "Run one named workflow from the nearest .kit/build.toml without crossing the canonical Git worktree boundary. Kit supervises the provider and validates its versioned JSON protocol; the repository provider owns the build graph, cache, artifacts, verification, and concurrency."
)]
struct BuildArgs {
    #[command(subcommand)]
    command: Option<BuildCommand>,
}

#[derive(Subcommand)]
enum BuildCommand {
    /// Run one operator-visible workflow through the repository's provider.
    Run(BuildRunArgs),
    /// List, inspect, or deliberately forget retained failed-build evidence.
    Evidence(BuildEvidenceArgs),
    /// Print the versioned provider protocol JSON Schema.
    Schema,
}

#[derive(Args)]
struct BuildEvidenceArgs {
    #[command(subcommand)]
    command: BuildEvidenceCommand,
}

#[derive(Subcommand)]
enum BuildEvidenceCommand {
    /// List retained and incomplete Build evidence records.
    List,
    /// Inspect one retained Build evidence record.
    Inspect { run_id: String },
    /// Deliberately remove one retained or incomplete Build evidence record.
    Forget { run_id: String },
}

#[derive(Args)]
struct BuildRunArgs {
    /// Workflow ID from .kit/build.toml.
    workflow: String,

    /// Start manifest discovery here instead of the current directory.
    #[arg(long, value_name = "PATH")]
    project: Option<PathBuf>,

    /// Terminate the provider after this many seconds (default: 7200).
    #[arg(long, value_name = "SECONDS")]
    deadline: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(super) struct BuildRunOutput {
    pub(super) run_id: String,
    pub(super) workflow_id: String,
    pub(super) workflow_label: String,
    pub(super) outcome: BuildOutcome,
    pub(super) event_count: u32,
    pub(super) events: Vec<BuildEvent>,
}

#[derive(Clone, Debug)]
pub(super) struct BuildSuccess {
    pub(super) run_id: String,
    pub(super) workflow_id: String,
    pub(super) workflow_label: String,
    pub(super) event_count: u32,
}

impl From<&BuildRunOutput> for BuildSuccess {
    fn from(output: &BuildRunOutput) -> Self {
        Self {
            run_id: output.run_id.clone(),
            workflow_id: output.workflow_id.clone(),
            workflow_label: output.workflow_label.clone(),
            event_count: output.event_count,
        }
    }
}

/// A single resolved Build invocation. Every presenter feeds this same execution engine; no
/// interactive path shells out to `kit build run` or reparses the provider protocol.
#[derive(Clone)]
pub(super) struct BuildInvocation {
    pub(super) root: WorktreeRoot,
    pub(super) manifest: LoadedBuildManifest,
    pub(super) workflow: Workflow,
    pub(super) deadline_seconds: Option<u64>,
}

#[derive(Clone, Debug)]
pub(super) enum BuildRuntimeAvailability {
    Ready,
    Unavailable { reason: String },
}

impl BuildRuntimeAvailability {
    async fn detect(cx: &Context) -> Self {
        match cx.processes.probe_prepared_complete_tree().await {
            Ok(()) => Self::Ready,
            Err(error) => Self::Unavailable {
                reason: format!(
                    "Build requires private prepared-process storage and complete-tree containment: {error}"
                ),
            },
        }
    }

    fn require(&self) -> Result<()> {
        match self {
            Self::Ready => Ok(()),
            Self::Unavailable { reason } => Err(anyhow::anyhow!(reason.clone())),
        }
    }
}

pub(super) async fn build_runtime_availability(cx: &Context) -> BuildRuntimeAvailability {
    BuildRuntimeAvailability::detect(cx).await
}

/// The closed control vocabulary accepted while a Build is active.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BuildControl {
    Cancel,
}

/// Validated lifecycle facts emitted by the Build engine to its presenter.
#[derive(Clone, Debug)]
pub(super) enum BuildUpdate {
    Started { run_id: String, workflow_id: String, workflow_label: String },
    Events(Vec<BuildEvent>),
    Terminal(BuildTerminal),
}

/// The latest bounded supervisor tails. `Arc<str>` keeps a revision cheap to fan out: changing
/// stdout does not copy stderr (and vice versa), while the watch channel coalesces revisions when
/// the terminal cannot redraw as quickly as a provider writes.
#[derive(Clone, Debug, Default)]
pub(super) struct BuildTranscriptTails {
    pub(super) stdout: Arc<str>,
    pub(super) stderr: Arc<str>,
}

#[derive(Clone)]
pub(super) struct BuildPresentationSender {
    updates: mpsc::Sender<BuildUpdate>,
    transcripts: watch::Sender<BuildTranscriptTails>,
}

pub(super) struct BuildPresentationReceiver {
    pub(super) updates: mpsc::Receiver<BuildUpdate>,
    pub(super) transcripts: watch::Receiver<BuildTranscriptTails>,
}

/// A terminal Build fact. `InfrastructureFailure` means the supervisor could not produce a
/// completed process report; a provider result is never allowed to mask that distinction.
#[derive(Clone, Debug)]
pub(super) enum BuildTerminal {
    Succeeded(BuildSuccess),
    Failed { message: String, evidence: BuildFailureEvidence },
    InfrastructureFailure { message: String, evidence: BuildFailureEvidence },
}

/// Whether a failed interactive run actually published durable evidence. Presentation consumes
/// this fact directly instead of inferring it from a formatted error message.
#[derive(Clone, Debug)]
pub(super) enum BuildFailureEvidence {
    Retained { run_id: String },
    NotRetained { reason: String },
    NotCreated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuildFailureClass {
    Failed,
    InfrastructureFailure,
}

struct BuildFailureState {
    class: BuildFailureClass,
    evidence: BuildFailureEvidence,
}

impl Default for BuildFailureState {
    fn default() -> Self {
        Self {
            class: BuildFailureClass::InfrastructureFailure,
            evidence: BuildFailureEvidence::NotCreated,
        }
    }
}

struct ProtocolArtifacts {
    request: PathBuf,
    events: PathBuf,
    result: PathBuf,
}

impl ProtocolArtifacts {
    fn in_workspace(workspace: &Path) -> Self {
        Self {
            request: workspace.join("request.json"),
            events: workspace.join("events.jsonl"),
            result: workspace.join("result.json"),
        }
    }
}

#[async_trait]
impl Tool for BuildTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "build",
            about: "Run a repository-owned build provider under Kit supervision",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        BuildArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = BuildArgs::from_arg_matches(matches)?;
        match args.command {
            Some(BuildCommand::Run(args)) => run_build(cx, args).await,
            Some(BuildCommand::Evidence(args)) => match args.command {
                BuildEvidenceCommand::List => evidence::list(cx),
                BuildEvidenceCommand::Inspect { run_id } => evidence::inspect(cx, &run_id),
                BuildEvidenceCommand::Forget { run_id } => evidence::forget(cx, &run_id),
            },
            Some(BuildCommand::Schema) => print_schema(cx),
            None => {
                if cx.out.is_json() {
                    bail!("bare `kit build` is interactive; use `kit build run WORKFLOW` for JSON output");
                }
                if !cx.term.interactive() {
                    bail!("bare `kit build` requires an interactive terminal; use `kit build run WORKFLOW`");
                }
                tui::run(cx).await
            }
        }
    }
}

fn print_schema(cx: &Context) -> Result<()> {
    let schema = provider_schema()?;
    if cx.out.is_json() {
        return cx.out.json(&schema);
    }
    let rendered = serde_json::to_string_pretty(&schema)?;
    write_plain_line(&rendered)
}

async fn run_build(cx: &Context, args: BuildRunArgs) -> Result<()> {
    let invocation = resolve_build_invocation(cx, args).await?;
    if cx.out.is_json() {
        let (_control_sender, controls) = mpsc::channel(1);
        let output = execute_build(cx, invocation, None, controls, false).await?;
        return cx.out.json(&output);
    }

    let (updates, receiver) = build_presentation_channel();
    let (_control_sender, controls) = mpsc::channel(1);
    let execution = execute_build(cx, invocation, Some(updates), controls, false);
    let presentation = present_plain(receiver.updates);
    let (execution, presentation) = tokio::join!(execution, presentation);
    execution?;
    presentation
}

async fn resolve_build_invocation(cx: &Context, args: BuildRunArgs) -> Result<BuildInvocation> {
    let start = match args.project {
        Some(path) => path,
        None => std::env::current_dir().context("resolve current directory")?,
    };
    if !start.is_dir() {
        bail!("build project path must be a directory: {}", start.display());
    }
    let root = cx
        .repositories
        .nearest_worktree_root(&start)
        .with_context(|| format!("locate Git worktree for {}", start.display()))?;
    let manifest = LoadedBuildManifest::load(&root, &start)?;
    let workflow = manifest.workflow(&args.workflow)?.clone();
    if !workflow.supports_current_platform() {
        bail!("build workflow '{}' is not eligible for this platform", workflow.id);
    }
    build_runtime_availability(cx).await.require()?;
    Ok(BuildInvocation { root, manifest, workflow, deadline_seconds: args.deadline })
}

pub(super) fn load_interactive_manifest(
    cx: &Context,
) -> Result<(WorktreeRoot, LoadedBuildManifest)> {
    let start = std::env::current_dir().context("resolve current directory")?;
    if !start.is_dir() {
        bail!("build project path must be a directory: {}", start.display());
    }
    let root = cx
        .repositories
        .nearest_worktree_root(&start)
        .with_context(|| format!("locate Git worktree for {}", start.display()))?;
    let manifest = LoadedBuildManifest::load(&root, &start)?;
    Ok((root, manifest))
}

pub(super) fn build_presentation_channel() -> (BuildPresentationSender, BuildPresentationReceiver) {
    let (updates, update_receiver) = mpsc::channel(BUILD_UPDATE_CAPACITY);
    let (transcripts, transcript_receiver) = watch::channel(BuildTranscriptTails::default());
    (
        BuildPresentationSender { updates, transcripts },
        BuildPresentationReceiver { updates: update_receiver, transcripts: transcript_receiver },
    )
}

async fn present_plain(mut updates: mpsc::Receiver<BuildUpdate>) -> Result<()> {
    while let Some(update) = updates.recv().await {
        match update {
            BuildUpdate::Events(events) => render_build_events(&events)?,
            BuildUpdate::Terminal(BuildTerminal::Succeeded(output)) => {
                write_plain_line(&format!(
                    "build {} ({}) succeeded - {} protocol events (run {})",
                    output.workflow_id, output.workflow_label, output.event_count, output.run_id
                ))?;
            }
            BuildUpdate::Terminal(BuildTerminal::Failed { .. })
            | BuildUpdate::Terminal(BuildTerminal::InfrastructureFailure { .. })
            | BuildUpdate::Started { .. } => {}
        }
    }
    Ok(())
}

pub(super) async fn execute_build(
    cx: &Context,
    invocation: BuildInvocation,
    updates: Option<BuildPresentationSender>,
    controls: mpsc::Receiver<BuildControl>,
    present_live_transcript_tails: bool,
) -> Result<BuildRunOutput> {
    let mut failure = BuildFailureState::default();
    let result = execute_build_inner(
        cx,
        invocation,
        updates.clone(),
        controls,
        present_live_transcript_tails,
        &mut failure,
    )
    .await;
    if let Some(updates) = updates {
        let terminal = match &result {
            Ok(output) => BuildTerminal::Succeeded(BuildSuccess::from(output)),
            Err(error) => match failure.class {
                BuildFailureClass::Failed => BuildTerminal::Failed {
                    message: format!("{error:#}"),
                    evidence: failure.evidence.clone(),
                },
                BuildFailureClass::InfrastructureFailure => BuildTerminal::InfrastructureFailure {
                    message: format!("{error:#}"),
                    evidence: failure.evidence.clone(),
                },
            },
        };
        emit_update(&Some(updates), BuildUpdate::Terminal(terminal)).await;
    }
    result
}

async fn execute_build_inner(
    cx: &Context,
    invocation: BuildInvocation,
    mut updates: Option<BuildPresentationSender>,
    mut controls: mpsc::Receiver<BuildControl>,
    present_live_transcript_tails: bool,
    failure: &mut BuildFailureState,
) -> Result<BuildRunOutput> {
    let BuildInvocation { root, manifest, workflow, deadline_seconds } = invocation;
    build_runtime_availability(cx).await.require()?;
    // Give an interactive presenter one scheduling turn to publish an immediate cancel before
    // Kit allocates process artifacts. The explicit checks below also cover already-queued
    // controls from non-interactive callers.
    tokio::task::yield_now().await;
    observe_prelaunch_cancellation(&mut controls, failure, "preparing build artifacts")?;
    let prepared = cx.processes.prepare().context("prepare supervised build run")?;
    let run_id = prepared.run_id().to_string();
    let workspace =
        prepared.create_workspace().context("create private build process workspace")?;
    let artifacts = ProtocolArtifacts::in_workspace(workspace.as_path());
    let request = BuildRequest {
        protocol_version: BUILD_PROTOCOL_VERSION,
        run_id: run_id.clone(),
        workflow_id: workflow.id.clone(),
        repository_root: absolute_utf8(root.as_path(), "repository root")?,
        events_path: absolute_utf8(&artifacts.events, "build event artifact")?,
        result_path: absolute_utf8(&artifacts.result, "build result artifact")?,
    };
    request.validate()?;
    publish_request(&artifacts.request, &request)?;

    let mut environment_values = BTreeMap::new();
    environment_values
        .insert(OsString::from("KIT_BUILD_REQUEST"), artifacts.request.clone().into_os_string());
    let environment =
        ProcessEnvironment::new(EnvironmentBase::Inherit, environment_values, BTreeSet::new())
            .context("construct build provider environment")?;
    let command = CommandSpec::new(
        manifest.provider.program,
        manifest.provider.arguments,
        root.as_path().to_path_buf(),
        environment,
        ProcessLabel::new(format!("build {}", workflow.id)).context("label build provider")?,
    )
    .context("construct build provider command")?;
    let record_policy = RecordPolicy::new(
        NonZeroUsize::new(FAILURE_TRANSCRIPT_TAIL_BYTES)
            .context("build transcript final-tail limit")?,
        RecordLimit::Bytes(
            NonZeroU64::new(MAX_DURABLE_TRANSCRIPT_BYTES)
                .context("build durable transcript limit")?,
        ),
        RecordOverflow::DrainWithTruncationEvidence,
    );
    let deadline = process_deadline(deadline_seconds)?;
    let spec = ProcessSpec::new(
        command,
        InputPolicy::Closed,
        OutputPolicy::Record(record_policy),
        OutputPolicy::Record(record_policy),
        ContainmentRequirement::CompleteTree,
        deadline,
        TerminationPolicy::new(TERMINATION_GRACE),
    );
    tokio::task::yield_now().await;
    observe_prelaunch_cancellation(&mut controls, failure, "starting the build provider")?;
    let starting = cx.processes.spawn_prepared(prepared, spec);
    tokio::pin!(starting);
    let mut controls_open = true;
    let started = loop {
        tokio::select! {
            biased;
            requested = controls.recv(), if controls_open => match requested {
                Some(BuildControl::Cancel) => {
                    failure.class = BuildFailureClass::Failed;
                    failure.evidence = BuildFailureEvidence::NotCreated;
                    bail!("build cancelled by operator before the provider was started");
                }
                None => controls_open = false,
            },
            started = &mut starting => break started,
        }
    }
    .context("start build provider")?;
    let crate::framework::process::StartedProcess { session, stdout, stderr, .. } = started;
    let mut stdout_tail = match (present_live_transcript_tails, stdout) {
        (true, ProcessOutputHandle::Recorded(tail)) => Some(tail),
        (true, _) => bail!("build supervisor did not expose the required recorded stdout tail"),
        (false, _) => None,
    };
    let mut stderr_tail = match (present_live_transcript_tails, stderr) {
        (true, ProcessOutputHandle::Recorded(tail)) => Some(tail),
        (true, _) => bail!("build supervisor did not expose the required recorded stderr tail"),
        (false, _) => None,
    };
    failure.class = BuildFailureClass::Failed;
    emit_update(
        &updates,
        BuildUpdate::Started {
            run_id: run_id.clone(),
            workflow_id: workflow.id.clone(),
            workflow_label: workflow.label.as_str().to_owned(),
        },
    )
    .await;
    let control = session.control();
    let completion = session.wait();
    tokio::pin!(completion);
    let interruption = termination_signal();
    tokio::pin!(interruption);
    let mut event_reader = BuildEventStreamReader::new(&artifacts.events, &run_id, &workflow.id);
    let mut emitted_event_count = 0usize;
    let mut pending_presentation_events = VecDeque::new();
    let mut live_stdout: Arc<str> = Arc::from("");
    let mut live_stderr: Arc<str> = Arc::from("");
    let mut event_interval = tokio::time::interval(EVENT_READ_INTERVAL);
    event_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (report, interruption_error) = loop {
        tokio::select! {
            report = &mut completion => match report {
                Ok(report) => break (report, None),
                Err(process_failure) => {
                    failure.class = BuildFailureClass::InfrastructureFailure;
                    let error = anyhow::anyhow!(
                        "build process infrastructure failed: {:?}",
                        process_failure.failure
                    );
                    flush_pending_event_updates(&updates, &mut pending_presentation_events).await;
                    return Err(failure_with_partial_evidence(error, &process_failure, failure));
                }
            },
            interrupt = &mut interruption => {
                let mut errors = Vec::new();
                if let Err(error) = interrupt {
                    errors.push(format!("listen for build interruption: {error}"));
                }
                if let Err(error) = control.cancel().await {
                    errors.push(format!("request build cancellation: {error}"));
                }
                let report = match completion.await {
                    Ok(report) => report,
                    Err(process_failure) => {
                        failure.class = BuildFailureClass::InfrastructureFailure;
                        errors.push(format!(
                            "build process infrastructure failed: {:?}",
                            process_failure.failure
                        ));
                        flush_pending_event_updates(&updates, &mut pending_presentation_events)
                            .await;
                        return Err(failure_with_partial_evidence(
                            anyhow::anyhow!(errors.join("; ")),
                            &process_failure,
                            failure,
                        ));
                    }
                };
                let error = (!errors.is_empty()).then(|| anyhow::anyhow!(errors.join("; ")));
                break (report, error);
            }
            requested = controls.recv(), if controls_open => {
                match requested {
                    Some(BuildControl::Cancel) => {
                        let cancellation = control.cancel().await;
                        let report = match completion.await {
                            Ok(report) => report,
                            Err(process_failure) => {
                                failure.class = BuildFailureClass::InfrastructureFailure;
                                let error = match cancellation {
                                    Ok(ControlAcknowledgement::Accepted) => anyhow::anyhow!(
                                        "build process infrastructure failed after operator cancellation: {:?}",
                                        process_failure.failure
                                    ),
                                    Ok(ControlAcknowledgement::AlreadyStopping) => anyhow::anyhow!(
                                        "build process infrastructure failed while the process was already stopping when operator cancellation was requested: {:?}",
                                        process_failure.failure
                                    ),
                                    Ok(ControlAcknowledgement::AlreadyCompleted) => anyhow::anyhow!(
                                        "build process infrastructure failed after it had already completed when operator cancellation was requested: {:?}",
                                        process_failure.failure
                                    ),
                                    Err(cancel_error) => anyhow::anyhow!(
                                        "request operator build cancellation: {cancel_error}; build process infrastructure also failed: {:?}",
                                        process_failure.failure
                                    ),
                                };
                                flush_pending_event_updates(
                                    &updates,
                                    &mut pending_presentation_events,
                                )
                                .await;
                                return Err(failure_with_partial_evidence(
                                    error,
                                    &process_failure,
                                    failure,
                                ));
                            }
                        };
                        match cancellation {
                            Ok(ControlAcknowledgement::Accepted) => break (
                                report,
                                Some(anyhow::anyhow!("build cancelled by operator")),
                            ),
                            Ok(
                                ControlAcknowledgement::AlreadyStopping
                                | ControlAcknowledgement::AlreadyCompleted,
                            ) => break (report, None),
                            Err(cancel_error) => break (
                                report,
                                Some(anyhow::anyhow!(
                                    "build cancellation was requested by operator but the supervisor rejected it: {cancel_error}"
                                )),
                            ),
                        }
                    }
                    None => controls_open = false,
                }
            }
            permit = reserve_event_update(
                updates.as_ref().map(|sender| sender.updates.clone()),
            ), if updates.is_some() && !pending_presentation_events.is_empty() => {
                if let Some(permit) = permit {
                    let batch_len = pending_presentation_events
                        .len()
                        .min(BUILD_EVENT_PRESENTATION_BATCH);
                    let batch = pending_presentation_events.drain(..batch_len).collect();
                    permit.send(BuildUpdate::Events(batch));
                    emitted_event_count = emitted_event_count
                        .checked_add(batch_len)
                        .context("count rendered build events")?;
                } else {
                    emitted_event_count = emitted_event_count
                        .checked_add(pending_presentation_events.len())
                        .context("count skipped build presentation events")?;
                    pending_presentation_events.clear();
                    updates = None;
                }
            }
            tail = next_live_tail(&mut stdout_tail) => {
                match tail {
                    Ok(RecordTailEvent::Revision(revision)) => {
                        live_stdout = Arc::from(escape_terminal_controls(&String::from_utf8_lossy(&revision.bytes)));
                        emit_transcript_update(&updates, &live_stdout, &live_stderr);
                    }
                    Ok(RecordTailEvent::End) => stdout_tail = None,
                    Err(error) => {
                        live_stdout = Arc::from(format!(
                            "<live stdout tail unavailable: {error}; final recorded tail will be shown at completion>"
                        ));
                        stdout_tail = None;
                        emit_transcript_update(&updates, &live_stdout, &live_stderr);
                    }
                }
            }
            tail = next_live_tail(&mut stderr_tail) => {
                match tail {
                    Ok(RecordTailEvent::Revision(revision)) => {
                        live_stderr = Arc::from(escape_terminal_controls(&String::from_utf8_lossy(&revision.bytes)));
                        emit_transcript_update(&updates, &live_stdout, &live_stderr);
                    }
                    Ok(RecordTailEvent::End) => stderr_tail = None,
                    Err(error) => {
                        live_stderr = Arc::from(format!(
                            "<live stderr tail unavailable: {error}; final recorded tail will be shown at completion>"
                        ));
                        stderr_tail = None;
                        emit_transcript_update(&updates, &live_stdout, &live_stderr);
                    }
                }
            }
            _ = event_interval.tick() => {
                match event_reader.read_available() {
                    Ok(events) => {
                        let new_event_count = events.len();
                        if updates.is_some() {
                            pending_presentation_events.extend(events.iter().cloned());
                        } else {
                            emitted_event_count = emitted_event_count
                                .checked_add(new_event_count)
                                .context("count skipped build presentation events")?;
                        }
                    }
                    Err(error) => {
                        let error = match control.cancel().await {
                            Ok(_) => anyhow::anyhow!("validate live build event stream: {error:#}"),
                            Err(cancel_error) => anyhow::anyhow!(
                                "validate live build event stream: {error:#}; request build cancellation: {cancel_error}"
                            ),
                        };
                        let report = match completion.await {
                            Ok(report) => report,
                            Err(process_failure) => {
                                failure.class = BuildFailureClass::InfrastructureFailure;
                                let error = anyhow::anyhow!(
                                    "{error:#}; build process infrastructure also failed: {:?}",
                                    process_failure.failure
                                );
                                flush_pending_event_updates(
                                    &updates,
                                    &mut pending_presentation_events,
                                )
                                .await;
                                return Err(failure_with_partial_evidence(
                                    error,
                                    &process_failure,
                                    failure,
                                ));
                            }
                        };
                        let failure = failure_after_completed_process(
                            error,
                            &workflow,
                            &artifacts,
                            &report,
                            failure,
                        );
                        emit_completed_transcript_tail(
                            &updates,
                            &report.stdout,
                            &report.stderr,
                            present_live_transcript_tails,
                        );
                        flush_pending_event_updates(
                            &updates,
                            &mut pending_presentation_events,
                        )
                        .await;
                        return Err(failure);
                    }
                }
            }
        }
    };
    if let Some(error) = interruption_error {
        flush_pending_event_updates(&updates, &mut pending_presentation_events).await;
        let failure_error =
            failure_after_completed_process(error, &workflow, &artifacts, &report, failure);
        emit_completed_transcript_tail(
            &updates,
            &report.stdout,
            &report.stderr,
            present_live_transcript_tails,
        );
        return Err(failure_error);
    }

    let validation = (|| {
        if report.completion != CompletionCause::Natural {
            bail!(
                "build provider did not complete naturally ({:?}); its final result cannot override Kit process evidence",
                report.completion
            );
        }
        event_reader.read_available()?;
        let events = event_reader.finish()?;
        let result = read_final_result(&artifacts.result, &run_id, &workflow.id, &events)?;
        validate_exit_consistency(&report.leader_exit, &result)?;
        Ok::<_, anyhow::Error>((events, result))
    })();
    let (events, result) = match validation {
        Ok(validated) => validated,
        Err(error) => {
            let failure_error =
                failure_after_completed_process(error, &workflow, &artifacts, &report, failure);
            emit_completed_transcript_tail(
                &updates,
                &report.stdout,
                &report.stderr,
                present_live_transcript_tails,
            );
            flush_pending_event_updates(&updates, &mut pending_presentation_events).await;
            return Err(failure_error);
        }
    };

    let event_count = events.event_count();
    let events = events.into_events();
    let output = BuildRunOutput {
        run_id,
        workflow_id: workflow.id.clone(),
        workflow_label: workflow.label.as_str().to_owned(),
        outcome: result.outcome,
        event_count,
        events,
    };
    match result.outcome {
        BuildOutcome::Succeeded => {
            emit_completed_transcript_tail(
                &updates,
                &report.stdout,
                &report.stderr,
                present_live_transcript_tails,
            );
            emit_event_updates(&updates, &output.events[emitted_event_count..]).await;
            Ok(output)
        }
        BuildOutcome::Failed => {
            let failure = failure_after_completed_process(
                anyhow::anyhow!("build workflow '{}' reported failure", output.workflow_id),
                &workflow,
                &artifacts,
                &report,
                failure,
            );
            // Failed evidence is captured before a presenter can publish final protocol
            // records; a closed human-output pipe cannot erase a retained failure record.
            emit_completed_transcript_tail(
                &updates,
                &report.stdout,
                &report.stderr,
                present_live_transcript_tails,
            );
            emit_event_updates(&updates, &output.events[emitted_event_count..]).await;
            Err(failure)
        }
    }
}

fn render_build_events(events: &[BuildEvent]) -> Result<()> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    for event in events {
        let sequence = event.sequence;
        match &event.event {
            BuildEventKind::RunStarted {} => {}
            BuildEventKind::StageStarted { stage_id, parent_stage_id } => {
                let stage_id = escape_inline_terminal_controls(stage_id);
                if let Some(parent_stage_id) = parent_stage_id {
                    writeln!(
                        output,
                        "[{sequence}] stage {stage_id} started (parent {})",
                        escape_inline_terminal_controls(parent_stage_id)
                    )?;
                } else {
                    writeln!(output, "[{sequence}] stage {stage_id} started")?;
                }
            }
            BuildEventKind::StageProgress { stage_id, message } => {
                writeln!(
                    output,
                    "[{sequence}] stage {}: {}",
                    escape_inline_terminal_controls(stage_id),
                    escape_inline_terminal_controls(message)
                )?;
            }
            BuildEventKind::StageFinished { stage_id, outcome } => {
                let outcome = match outcome {
                    StageOutcome::Succeeded => "succeeded",
                    StageOutcome::Failed => "failed",
                };
                writeln!(
                    output,
                    "[{sequence}] stage {} {outcome}",
                    escape_inline_terminal_controls(stage_id)
                )?;
            }
            BuildEventKind::ArtifactReported { path, label } => {
                writeln!(
                    output,
                    "[{sequence}] artifact {}: {}",
                    escape_inline_terminal_controls(label),
                    escape_inline_terminal_controls(path)
                )?;
            }
            BuildEventKind::EvidenceReported { path, label } => {
                writeln!(
                    output,
                    "[{sequence}] evidence {}: {}",
                    escape_inline_terminal_controls(label),
                    escape_inline_terminal_controls(path)
                )?;
            }
        }
    }
    output.flush().context("flush Build output")?;
    Ok(())
}

fn write_plain_line(line: &str) -> Result<()> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    writeln!(output, "{line}").context("write Build output")?;
    output.flush().context("flush Build output")
}

fn escape_inline_terminal_controls(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            escaped.extend(character.escape_unicode());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn process_deadline(seconds: Option<u64>) -> Result<ProcessDeadline> {
    let duration = match seconds {
        Some(seconds) => Duration::from_secs(seconds),
        None => DEFAULT_BUILD_DEADLINE,
    };
    tokio::time::Instant::now()
        .checked_add(duration)
        .context("build deadline is too large for the platform clock")?;
    Ok(ProcessDeadline::After(duration))
}

fn failure_after_completed_process(
    error: anyhow::Error,
    workflow: &Workflow,
    artifacts: &ProtocolArtifacts,
    report: &crate::framework::process::ProcessReport,
    failure: &mut BuildFailureState,
) -> anyhow::Error {
    let primary = format!("{error:#}");
    let mut output = vec![primary.clone()];
    match evidence::capture(workflow, &primary, artifacts, report) {
        Ok(evidence::CaptureOutcome::Stored(stored)) => {
            failure.evidence = BuildFailureEvidence::Retained { run_id: stored.run_id.clone() };
            output.push(format!(
                "build evidence retained for run {} ({} bytes)\ndirectory: {}\ninspect: kit build evidence inspect {}\nforget: kit build evidence forget {}",
                stored.run_id,
                stored.bytes,
                escape_terminal_controls(&stored.directory.display().to_string()),
                stored.run_id,
                stored.run_id
            ));
        }
        Ok(evidence::CaptureOutcome::AtCapacity(capacity)) => {
            let reason = capacity.render();
            failure.evidence = BuildFailureEvidence::NotRetained { reason: reason.clone() };
            output.push(format!(
                "build evidence was not retained: {}. Existing evidence was left untouched; use `kit build evidence list` and `kit build evidence forget RUN_ID` to free capacity",
                reason
            ));
        }
        Err(capture_error) => {
            let reason = format!("Build could not publish its evidence record: {capture_error:#}");
            failure.evidence = BuildFailureEvidence::NotRetained { reason: reason.clone() };
            output.push(format!("build evidence was not retained because {reason}"));
        }
    }
    output.push(render_complete_process_evidence(&report.stdout, &report.stderr));
    anyhow::anyhow!(output.join("\n"))
}

fn failure_with_partial_evidence(
    error: anyhow::Error,
    report: &crate::framework::process::ProcessFailureReport,
    failure: &mut BuildFailureState,
) -> anyhow::Error {
    let reason = "the process supervisor returned an infrastructure failure report, not a completed process report";
    failure.evidence = BuildFailureEvidence::NotRetained { reason: reason.to_owned() };
    anyhow::anyhow!(
        "{error:#}\nbuild evidence was not retained: {reason}\n{}",
        render_partial_process_evidence(&report.stdout, &report.stderr)
    )
}

fn render_complete_process_evidence(stdout: &OutputReport, stderr: &OutputReport) -> String {
    let mut output = Vec::new();
    append_complete_output_evidence(&mut output, "stdout", stdout);
    append_complete_output_evidence(&mut output, "stderr", stderr);
    output.join("\n")
}

fn completed_output_tail(report: &OutputReport) -> String {
    match report {
        OutputReport::Recorded(report) => {
            escape_terminal_controls(&String::from_utf8_lossy(&report.final_tail))
        }
        OutputReport::Inherited
        | OutputReport::Discarded
        | OutputReport::Captured(_)
        | OutputReport::Streamed(_) => String::new(),
    }
}

fn emit_completed_transcript_tail(
    updates: &Option<BuildPresentationSender>,
    stdout: &OutputReport,
    stderr: &OutputReport,
    enabled: bool,
) {
    if !enabled {
        return;
    }
    let stdout: Arc<str> = Arc::from(completed_output_tail(stdout));
    let stderr: Arc<str> = Arc::from(completed_output_tail(stderr));
    emit_transcript_update(updates, &stdout, &stderr);
}

fn render_partial_process_evidence(
    stdout: &PartialOutputReport,
    stderr: &PartialOutputReport,
) -> String {
    let mut output = Vec::new();
    append_partial_output_evidence(&mut output, "stdout", stdout);
    append_partial_output_evidence(&mut output, "stderr", stderr);
    output.join("\n")
}

fn append_complete_output_evidence(output: &mut Vec<String>, label: &str, report: &OutputReport) {
    if let OutputReport::Recorded(report) = report {
        append_recorded_output_evidence(
            output,
            label,
            report.observed_bytes,
            report.retained_bytes,
            report.disposition,
            report.availability,
            &report.final_tail,
        );
    }
}

fn append_partial_output_evidence(
    output: &mut Vec<String>,
    label: &str,
    report: &PartialOutputReport,
) {
    match report {
        PartialOutputReport::Recorded(report) => {
            append_recorded_output_evidence(
                output,
                label,
                report.observed_bytes,
                report.retained_bytes,
                report.disposition,
                report.availability,
                &report.final_tail,
            );
        }
        PartialOutputReport::Unavailable(UnavailableOutput::Record { .. }) => {
            output.push(format!("{label} recorded output unavailable"));
            output.push(format!("{label} final tail unavailable"));
        }
        PartialOutputReport::Inherited
        | PartialOutputReport::Discarded
        | PartialOutputReport::Captured(_)
        | PartialOutputReport::Streamed(_)
        | PartialOutputReport::Unavailable(
            UnavailableOutput::Capture | UnavailableOutput::Stream,
        ) => {}
    }
}

fn append_recorded_output_evidence(
    output: &mut Vec<String>,
    label: &str,
    observed_bytes: u64,
    retained_bytes: u64,
    disposition: RecordDisposition,
    availability: RecordAvailability,
    final_tail: &[u8],
) {
    let disposition = match disposition {
        RecordDisposition::Complete => "complete",
        RecordDisposition::Truncated => "capped",
        RecordDisposition::Interrupted => "interrupted",
    };
    let availability = match availability {
        RecordAvailability::Available => "available",
        RecordAvailability::Unavailable => "write unavailable",
    };
    output.push(format!(
        "{label} recorded output: retained {retained_bytes} of {observed_bytes} bytes; {disposition}; {availability}"
    ));
    let tail_start = final_tail.len().saturating_sub(FAILURE_TRANSCRIPT_TAIL_BYTES);
    let final_tail = &final_tail[tail_start..];
    if final_tail.is_empty() {
        output.push(format!("{label} final tail: <empty>"));
    } else {
        output.push(format!(
            "{label} final tail (last at most {FAILURE_TRANSCRIPT_TAIL_BYTES} bytes):\n{}",
            escape_terminal_controls(&String::from_utf8_lossy(final_tail))
        ));
    }
}

fn escape_terminal_controls(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' | '\t' => escaped.push(character),
            character if character.is_control() => {
                escaped.extend(character.escape_unicode());
            }
            character => escaped.push(character),
        }
    }
    escaped
}

async fn emit_update(sender: &Option<BuildPresentationSender>, update: BuildUpdate) {
    if let Some(sender) = sender {
        let _ignored = sender.updates.send(update).await;
    }
}

async fn reserve_event_update(
    sender: Option<mpsc::Sender<BuildUpdate>>,
) -> Option<mpsc::OwnedPermit<BuildUpdate>> {
    let sender = sender?;
    sender.reserve_owned().await.ok()
}

async fn flush_pending_event_updates(
    sender: &Option<BuildPresentationSender>,
    pending: &mut VecDeque<BuildEvent>,
) {
    while !pending.is_empty() {
        let batch_len = pending.len().min(BUILD_EVENT_PRESENTATION_BATCH);
        let batch = pending.drain(..batch_len).collect();
        emit_update(sender, BuildUpdate::Events(batch)).await;
    }
}

async fn emit_event_updates(sender: &Option<BuildPresentationSender>, events: &[BuildEvent]) {
    for events in events.chunks(BUILD_EVENT_PRESENTATION_BATCH) {
        emit_update(sender, BuildUpdate::Events(events.to_vec())).await;
    }
}

fn emit_transcript_update(
    sender: &Option<BuildPresentationSender>,
    stdout: &Arc<str>,
    stderr: &Arc<str>,
) {
    if let Some(sender) = sender {
        sender.transcripts.send_replace(BuildTranscriptTails {
            stdout: Arc::clone(stdout),
            stderr: Arc::clone(stderr),
        });
    }
}

fn observe_prelaunch_cancellation(
    controls: &mut mpsc::Receiver<BuildControl>,
    failure: &mut BuildFailureState,
    boundary: &str,
) -> Result<()> {
    match controls.try_recv() {
        Ok(BuildControl::Cancel) => {
            failure.class = BuildFailureClass::Failed;
            failure.evidence = BuildFailureEvidence::NotCreated;
            bail!("build cancelled by operator before {boundary}");
        }
        Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => Ok(()),
    }
}

async fn next_live_tail(
    tail: &mut Option<RecordedOutputTail>,
) -> Result<RecordTailEvent, ProcessOutputError> {
    match tail {
        Some(tail) => tail.next().await,
        None => std::future::pending().await,
    }
}

#[cfg(unix)]
async fn termination_signal() -> std::io::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        interrupt = tokio::signal::ctrl_c() => interrupt,
        received = terminate.recv() => received
            .map(|_| ())
            .ok_or_else(|| std::io::Error::other("termination signal stream closed")),
    }
}

#[cfg(not(unix))]
async fn termination_signal() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}

fn publish_request(path: &std::path::Path, request: &BuildRequest) -> Result<()> {
    let directory = path.parent().context("build request artifact has no parent directory")?;
    let run_suffix = request.run_id.replace('-', "");
    let writer = AtomicFileWriter::new(
        directory,
        format!(".build-request-{run_suffix}.lock"),
        format!(".build-request-{run_suffix}"),
    );
    let serialized = serde_json::to_vec(request).context("serialize build request")?;
    let _lock = writer.lock().context("lock build request artifact")?;
    writer
        .replace(path, &serialized)
        .with_context(|| format!("atomically publish build request {}", path.display()))
}

fn validate_exit_consistency(
    exit: &LeaderExitObservation,
    result: &BuildFinalResult,
) -> Result<()> {
    let LeaderExitObservation::Observed(exit) = exit else {
        bail!("build provider completed without an observed leader exit")
    };
    match (result.outcome, exit) {
        (BuildOutcome::Succeeded, LeaderExit::Code(0)) => Ok(()),
        (BuildOutcome::Succeeded, LeaderExit::Code(code)) => {
            bail!("build provider reported success but exited with status {code}")
        }
        (BuildOutcome::Succeeded, LeaderExit::Signal(signal)) => {
            bail!("build provider reported success but exited from signal {}", signal.get())
        }
        (BuildOutcome::Failed, LeaderExit::Code(0)) => {
            bail!("build provider reported failure but exited with status 0")
        }
        (BuildOutcome::Failed, LeaderExit::Code(_))
        | (BuildOutcome::Failed, LeaderExit::Signal(_)) => Ok(()),
    }
}

fn absolute_utf8(path: &std::path::Path, label: &str) -> Result<String> {
    if !path.is_absolute() {
        bail!("{label} must be absolute");
    }
    path.to_str().map(ToOwned::to_owned).with_context(|| format!("{label} is not valid UTF-8"))
}
