use std::{
    fs::DirBuilder,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use directories::ProjectDirs;
use thiserror::Error;
use tokio::{process::Command, sync::mpsc, task::JoinHandle};
use uuid::Uuid;

use crate::framework::AtomicFileWriter;

use super::{
    output::{
        create_private_record_file, earliest_observed_failure, partial_from_output_report,
        recorded_output_path, spawn_once_input, spawn_output_pump, unavailable_partial_output,
        ObservedProcessFailure, OutputPumpCompletion, ProcessInputHandle, ProcessInputWriter,
        ProcessOutputHandle, RecordedOutputPath,
    },
    platform::attached::{AttachedGroup, TerminationRequest},
    report::{
        leader_exit, CompletionCause, DescendantDisposition, LeaderExit, LeaderExitObservation,
        OutputReport, PartialOutputReport, ProcessFailureKind, ProcessFailureReport, ProcessReport,
        ProcessRunId, ProcessStream, TerminationDisposition,
    },
    session::{
        process_session, ContainmentStrength, ControlAcknowledgement, ControlRequest,
        ProcessCompletion, StartedProcess,
    },
    spec::{
        CommandSpec, ContainmentRequirement, EnvironmentBase, InputPolicy, OutputPolicy,
        ProcessDeadline, ProcessSpec, TerminationPolicy,
    },
};

const OUTPUT_DRAIN_CONFIRMATION: Duration = Duration::from_secs(30);
const MAX_PROCESS_DURATION: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);
const PREPARED_COMPLETE_TREE_PROBE_TIMEOUT: Duration = Duration::from_secs(40);
const ATTACHED_RUN_MARKER: &str = "attached-run.json";
const ATTACHED_RUN_LOCK: &str = "attached-run.lock";
const DETACHED_LAUNCH_MARKER: &str = "detached-launch.json";
const ATTACHED_RUN_MARKER_BYTES: &[u8] = br#"{"version":"v1","kind":"attached"}"#;

#[derive(Clone, Debug)]
pub struct ProcessSupervisor {
    inner: Arc<SupervisorInner>,
}

#[derive(Debug)]
struct SupervisorInner {
    instance_id: Uuid,
    state_root: PathBuf,
    private_storage: bool,
    prepared_complete_tree_ready: tokio::sync::Mutex<bool>,
}

#[derive(Debug, Error)]
pub enum ProcessSupervisorBootstrapError {
    #[error("resolve Kit process state directory")]
    StateDirectory,
    #[error("create Kit process state directory {}: {source}", path.display())]
    CreateStateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Kit process state path {} is not an owner-only directory", path.display())]
    UnsafeStateDirectory { path: PathBuf },
    #[error("inspect Kit process state directory {}: {source}", path.display())]
    InspectStateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainmentAvailability {
    CompleteTree,
    ProcessGroupOnly,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessPrivateStorageAvailability {
    Available,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessSupervisorCapabilities {
    private_storage: ProcessPrivateStorageAvailability,
    maximum_attached_containment: ContainmentAvailability,
}

impl ProcessSupervisorCapabilities {
    pub fn private_storage(self) -> ProcessPrivateStorageAvailability {
        self.private_storage
    }

    pub fn maximum_attached_containment(self) -> ContainmentAvailability {
        self.maximum_attached_containment
    }

    pub fn prepared_complete_tree(self) -> Result<(), PreparedCompleteTreeUnavailable> {
        if self.private_storage != ProcessPrivateStorageAvailability::Available {
            return Err(PreparedCompleteTreeUnavailable::PrivateStorageUnavailable);
        }
        if self.maximum_attached_containment != ContainmentAvailability::CompleteTree {
            return Err(PreparedCompleteTreeUnavailable::CompleteTreeUnavailable);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum PreparedCompleteTreeUnavailable {
    #[error("private prepared-process storage is unavailable")]
    PrivateStorageUnavailable,
    #[error("complete-tree attached process containment is unavailable")]
    CompleteTreeUnavailable,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum PreparedCompleteTreeReadinessError {
    #[error("{0}")]
    Capability(PreparedCompleteTreeUnavailable),
    #[error("complete-tree containment readiness failed: {message}")]
    RuntimeUnavailable { message: String },
    #[error("complete-tree containment readiness timed out")]
    TimedOut,
    #[error("complete-tree containment readiness cleanup was not confirmed")]
    CleanupUnconfirmed,
}

#[derive(Debug, Error)]
pub enum ProcessPrepareError {
    #[error("create private process run directory: {message}")]
    CreateRunDirectory { message: String },
    #[error("private process storage is unavailable on this platform")]
    PrivateStorageUnavailable,
}

#[derive(Debug, Error)]
pub enum ProcessWorkspaceError {
    #[error("create private process workspace")]
    Create,
}

#[derive(Clone)]
pub struct ProcessWorkspace {
    path: PathBuf,
    _retention: Arc<RunDirectoryLease>,
}

impl ProcessWorkspace {
    pub fn as_path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for ProcessWorkspace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_tuple("ProcessWorkspace").field(&self.path).finish()
    }
}

impl PartialEq for ProcessWorkspace {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Eq for ProcessWorkspace {}

pub struct PreparedProcessRun {
    supervisor_id: Uuid,
    run_id: ProcessRunId,
    run_dir: PathBuf,
    retention: Arc<RunDirectoryLease>,
}

impl PreparedProcessRun {
    pub fn run_id(&self) -> ProcessRunId {
        self.run_id
    }

    pub(crate) fn into_run_directory(self) -> RunDirectory {
        RunDirectory { retention: self.retention }
    }

    pub fn create_workspace(&self) -> Result<ProcessWorkspace, ProcessWorkspaceError> {
        let path = self.run_dir.join("workspace");
        create_private_run_directory(&path).map_err(|_| ProcessWorkspaceError::Create)?;
        let directory =
            std::fs::File::open(&self.run_dir).map_err(|_| ProcessWorkspaceError::Create)?;
        directory.sync_all().map_err(|_| ProcessWorkspaceError::Create)?;
        Ok(ProcessWorkspace { path, _retention: Arc::clone(&self.retention) })
    }
}

pub(crate) struct RunDirectoryLease {
    path: PathBuf,
    retain: AtomicBool,
    _attached_run: Option<AttachedRunLease>,
}

impl RunDirectoryLease {
    #[cfg(target_os = "linux")]
    pub(crate) fn retain(&self) {
        self.retain.store(true, Ordering::Release);
    }
}

pub(crate) struct RunDirectory {
    retention: Arc<RunDirectoryLease>,
}

impl RunDirectory {
    pub(crate) fn path(&self) -> &Path {
        &self.retention.path
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn retain(&mut self) {
        self.retention.retain();
    }

    pub(crate) fn retention(&self) -> Arc<RunDirectoryLease> {
        Arc::clone(&self.retention)
    }
}

struct AttachedRunLease {
    _lock: std::fs::File,
}

impl Drop for RunDirectoryLease {
    fn drop(&mut self) {
        if !self.retain.load(Ordering::Acquire) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[derive(Debug, Error)]
pub enum ProcessStartError {
    #[error("prepared process run belongs to a different supervisor")]
    ForeignPreparedRun,
    #[error("process working directory is unavailable")]
    WorkingDirectoryUnavailable,
    #[error("required process containment is unavailable: required {required:?}, available {available:?}")]
    ContainmentUnavailable { required: ContainmentRequirement, available: ContainmentAvailability },
    #[error("set up process containment: {message}")]
    ContainmentSetupFailed { message: String },
    #[error("prepare process output")]
    OutputSetupFailed,
    #[error("recorded attached output requires private process storage")]
    PrivateStorageUnavailable,
    #[error("start process: {message}")]
    SpawnFailed { message: String },
    #[error("spawned process did not expose its configured {stream:?} pipe")]
    MissingPipe { stream: ProcessStream },
    #[error("failed to terminate and reap a process after startup failed")]
    StartupRollbackFailed,
    #[error("process deadline exceeds the supported duration")]
    DeadlineOutOfRange,
    #[error("process termination grace period exceeds the supported duration")]
    TerminationGraceOutOfRange,
}

impl ProcessSupervisor {
    pub fn bootstrap() -> Result<Self, ProcessSupervisorBootstrapError> {
        let project = ProjectDirs::from("", "", "kit")
            .ok_or(ProcessSupervisorBootstrapError::StateDirectory)?;
        let base = project.state_dir().unwrap_or_else(|| project.data_local_dir());
        Self::from_state_root(base.join("processes"))
    }

    #[cfg(test)]
    pub(crate) fn for_test(state_root: PathBuf) -> Result<Self, ProcessSupervisorBootstrapError> {
        Self::from_state_root(state_root)
    }

    fn from_state_root(state_root: PathBuf) -> Result<Self, ProcessSupervisorBootstrapError> {
        #[cfg(not(windows))]
        let state_root = {
            ensure_private_state_root(&state_root)?;
            let canonical = std::fs::canonicalize(&state_root).map_err(|source| {
                ProcessSupervisorBootstrapError::InspectStateDirectory {
                    path: state_root.clone(),
                    source,
                }
            })?;
            let metadata = std::fs::symlink_metadata(&canonical).map_err(|source| {
                ProcessSupervisorBootstrapError::InspectStateDirectory {
                    path: canonical.clone(),
                    source,
                }
            })?;
            validate_private_directory(&canonical, &metadata)?;
            recover_abandoned_attached_runs(&canonical)?;
            canonical
        };
        Ok(Self {
            inner: Arc::new(SupervisorInner {
                instance_id: Uuid::new_v4(),
                state_root,
                private_storage: !cfg!(windows),
                prepared_complete_tree_ready: tokio::sync::Mutex::new(false),
            }),
        })
    }

    #[cfg(target_os = "linux")]
    pub(super) fn state_root(&self) -> &Path {
        &self.inner.state_root
    }

    pub fn capabilities(&self) -> ProcessSupervisorCapabilities {
        ProcessSupervisorCapabilities {
            private_storage: if self.inner.private_storage {
                ProcessPrivateStorageAvailability::Available
            } else {
                ProcessPrivateStorageAvailability::Unavailable
            },
            maximum_attached_containment: if cfg!(any(target_os = "linux", windows)) {
                ContainmentAvailability::CompleteTree
            } else if cfg!(unix) {
                ContainmentAvailability::ProcessGroupOnly
            } else {
                ContainmentAvailability::Unavailable
            },
        }
    }

    pub async fn probe_prepared_complete_tree(
        &self,
    ) -> Result<(), PreparedCompleteTreeReadinessError> {
        let mut ready = self.inner.prepared_complete_tree_ready.lock().await;
        if *ready {
            return Ok(());
        }
        self.capabilities()
            .prepared_complete_tree()
            .map_err(PreparedCompleteTreeReadinessError::Capability)?;
        let probe = async {
            let mut containment = AttachedGroup::create(
                ContainmentRequirement::CompleteTree,
                TerminationPolicy::new(Duration::from_millis(250)),
            )
            .await
            .map_err(|error| {
                PreparedCompleteTreeReadinessError::RuntimeUnavailable {
                    message: error.to_string(),
                }
            })?;
            containment
                .disarm_guardian()
                .await
                .map_err(|()| PreparedCompleteTreeReadinessError::CleanupUnconfirmed)?;
            wait_for_empty_containment(&containment)
                .await
                .map_err(|_| PreparedCompleteTreeReadinessError::CleanupUnconfirmed)
        };
        tokio::time::timeout(PREPARED_COMPLETE_TREE_PROBE_TIMEOUT, probe)
            .await
            .map_err(|_| PreparedCompleteTreeReadinessError::TimedOut)??;
        *ready = true;
        Ok(())
    }

    pub fn prepare(&self) -> Result<PreparedProcessRun, ProcessPrepareError> {
        self.prepare_run(true)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn prepare_detached(&self) -> Result<PreparedProcessRun, ProcessPrepareError> {
        self.prepare_run(false)
    }

    fn prepare_run(&self, attached: bool) -> Result<PreparedProcessRun, ProcessPrepareError> {
        if !self.inner.private_storage {
            return Err(ProcessPrepareError::PrivateStorageUnavailable);
        }
        loop {
            let run_id = ProcessRunId::new();
            let run_dir = self.inner.state_root.join(run_id.to_string());
            match create_private_run_directory(&run_dir) {
                Ok(()) => {
                    let attached_run = if attached {
                        match prepare_attached_run_lease(&run_dir) {
                            Ok(lease) => Some(lease),
                            Err(error) => {
                                let _ = std::fs::remove_dir_all(&run_dir);
                                return Err(ProcessPrepareError::CreateRunDirectory {
                                    message: error.to_string(),
                                });
                            }
                        }
                    } else {
                        None
                    };
                    let retention = Arc::new(RunDirectoryLease {
                        path: run_dir.clone(),
                        retain: AtomicBool::new(false),
                        _attached_run: attached_run,
                    });
                    return Ok(PreparedProcessRun {
                        supervisor_id: self.inner.instance_id,
                        run_id,
                        run_dir,
                        retention,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(ProcessPrepareError::CreateRunDirectory {
                        message: error.to_string(),
                    });
                }
            }
        }
    }

    pub async fn spawn(&self, spec: ProcessSpec) -> Result<StartedProcess, ProcessStartError> {
        if self.inner.private_storage {
            let prepared = self
                .prepare()
                .map_err(|error| ProcessStartError::SpawnFailed { message: error.to_string() })?;
            return self.spawn_prepared(prepared, spec).await;
        }
        spawn_attached(ProcessRunId::new(), None, spec).await
    }

    pub async fn spawn_prepared(
        &self,
        prepared: PreparedProcessRun,
        spec: ProcessSpec,
    ) -> Result<StartedProcess, ProcessStartError> {
        if prepared.supervisor_id != self.inner.instance_id {
            return Err(ProcessStartError::ForeignPreparedRun);
        }
        let run_id = prepared.run_id;
        spawn_attached(run_id, Some(prepared.into_run_directory()), spec).await
    }
}

async fn spawn_attached(
    run_id: ProcessRunId,
    run_directory: Option<RunDirectory>,
    spec: ProcessSpec,
) -> Result<StartedProcess, ProcessStartError> {
    if !spec.command.working_directory.is_dir() {
        return Err(ProcessStartError::WorkingDirectoryUnavailable);
    }
    validate_timing(&spec)?;

    let stdout_policy = spec.stdout;
    let stderr_policy = spec.stderr;
    let input_policy = spec.input;
    let deadline = match spec.deadline {
        ProcessDeadline::Unlimited => None,
        ProcessDeadline::After(duration) => Some(
            tokio::time::Instant::now()
                .checked_add(duration)
                .expect("process deadline was validated before containment"),
        ),
    };
    let stdout_record =
        prepare_record_output(stdout_policy, ProcessStream::Stdout, run_directory.as_ref())?;
    let stderr_record =
        prepare_record_output(stderr_policy, ProcessStream::Stderr, run_directory.as_ref())?;
    let mut containment = AttachedGroup::create(spec.containment, spec.termination).await?;
    let containment_strength = containment.strength();
    let mut command = tokio_command(&spec.command);
    command
        .stdin(input_stdio(&input_policy))
        .stdout(output_stdio(stdout_policy))
        .stderr(output_stdio(stderr_policy));
    let mut child = containment.spawn(command)?;
    if let Some(stream) =
        missing_configured_pipe(&child, &input_policy, stdout_policy, stderr_policy)
    {
        rollback_failed_spawn(&mut child, &mut containment).await?;
        return Err(ProcessStartError::MissingPipe { stream });
    }
    let (input, input_task) = match input_policy {
        InputPolicy::Closed => (ProcessInputHandle::Closed, None),
        InputPolicy::Once(bytes) => {
            let stdin =
                child.stdin.take().expect("configured stdin pipe was validated after spawn");
            let (completion, task) = spawn_once_input(stdin, bytes);
            (ProcessInputHandle::Once(completion), Some(task))
        }
        InputPolicy::Writable => {
            let stdin =
                child.stdin.take().expect("configured stdin pipe was validated after spawn");
            (ProcessInputHandle::Writable(ProcessInputWriter::new(stdin)), None)
        }
    };

    let observation_sequence = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (stdout, stdout_owner, stdout_failures) = prepare_output(
        child.stdout.take(),
        stdout_policy,
        ProcessStream::Stdout,
        stdout_record,
        observation_sequence.clone(),
    );
    let (stderr, stderr_owner, stderr_failures) = prepare_output(
        child.stderr.take(),
        stderr_policy,
        ProcessStream::Stderr,
        stderr_record,
        observation_sequence,
    );

    let fallback = fallback_failure(
        run_id,
        stdout_policy,
        stderr_policy,
        run_directory.as_ref(),
        ProcessFailureKind::OwnerTaskFailed,
    );
    let (session, controls, completion) = process_session(run_id, containment_strength, fallback);
    let (output_failure_sender, output_failures) = mpsc::unbounded_channel();
    forward_output_failures(stdout_failures, output_failure_sender.clone());
    forward_output_failures(stderr_failures, output_failure_sender.clone());
    drop(output_failure_sender);
    let owner = AttachedOwner {
        run_id,
        child,
        containment,
        containment_strength,
        deadline,
        grace: spec.termination.grace_period,
        controls: Some(controls),
        completion: Some(completion),
        input_task,
        stdout: stdout_owner,
        stderr: stderr_owner,
        output_failures,
        started_at: Instant::now(),
        _run_directory: run_directory,
    };
    tokio::spawn(owner.run());

    Ok(StartedProcess::new(session, input, stdout, stderr))
}

fn input_stdio(policy: &InputPolicy) -> Stdio {
    match policy {
        InputPolicy::Closed => Stdio::null(),
        InputPolicy::Once(_) | InputPolicy::Writable => Stdio::piped(),
    }
}

fn output_stdio(policy: OutputPolicy) -> Stdio {
    match policy {
        OutputPolicy::Inherit => Stdio::inherit(),
        OutputPolicy::Discard => Stdio::null(),
        OutputPolicy::Capture(_) | OutputPolicy::Stream(_) | OutputPolicy::Record(_) => {
            Stdio::piped()
        }
    }
}

fn prepare_record_output(
    policy: OutputPolicy,
    stream: ProcessStream,
    run_directory: Option<&RunDirectory>,
) -> Result<Option<(std::fs::File, RecordedOutputPath)>, ProcessStartError> {
    if !matches!(policy, OutputPolicy::Record(_)) {
        return Ok(None);
    }
    let run_directory = run_directory.ok_or(ProcessStartError::PrivateStorageUnavailable)?;
    let path = recorded_output_path(run_directory.path(), stream);
    let file =
        create_private_record_file(&path).map_err(|_| ProcessStartError::OutputSetupFailed)?;
    Ok(Some((file, RecordedOutputPath::retained(path, run_directory.retention()))))
}

fn missing_configured_pipe(
    child: &tokio::process::Child,
    input: &InputPolicy,
    stdout: OutputPolicy,
    stderr: OutputPolicy,
) -> Option<ProcessStream> {
    if !matches!(input, InputPolicy::Closed) && child.stdin.is_none() {
        return Some(ProcessStream::Stdin);
    }
    if matches!(
        stdout,
        OutputPolicy::Capture(_) | OutputPolicy::Stream(_) | OutputPolicy::Record(_)
    ) && child.stdout.is_none()
    {
        return Some(ProcessStream::Stdout);
    }
    if matches!(
        stderr,
        OutputPolicy::Capture(_) | OutputPolicy::Stream(_) | OutputPolicy::Record(_)
    ) && child.stderr.is_none()
    {
        return Some(ProcessStream::Stderr);
    }
    None
}

async fn rollback_failed_spawn(
    child: &mut tokio::process::Child,
    containment: &mut AttachedGroup,
) -> Result<(), ProcessStartError> {
    let _ = containment.force_kill();
    let _ = child.start_kill();
    let reaped = child.wait().await.is_ok();
    let guardian_reaped = containment.reap_guardian_after_kill().await.is_ok();
    let empty = wait_for_empty_containment(containment).await.is_ok();
    if reaped && guardian_reaped && empty {
        Ok(())
    } else {
        Err(ProcessStartError::StartupRollbackFailed)
    }
}

enum OutputOwner {
    Complete(OutputReport),
    Pump { task: JoinHandle<OutputPumpCompletion>, unavailable: PartialOutputReport },
}

fn prepare_output(
    reader: Option<impl tokio::io::AsyncRead + Unpin + Send + 'static>,
    policy: OutputPolicy,
    stream: ProcessStream,
    prepared_record: Option<(std::fs::File, RecordedOutputPath)>,
    observation_sequence: Arc<std::sync::atomic::AtomicU64>,
) -> (ProcessOutputHandle, OutputOwner, Option<mpsc::UnboundedReceiver<ObservedProcessFailure>>) {
    match policy {
        OutputPolicy::Inherit => {
            (ProcessOutputHandle::Inherited, OutputOwner::Complete(OutputReport::Inherited), None)
        }
        OutputPolicy::Discard => {
            (ProcessOutputHandle::Discarded, OutputOwner::Complete(OutputReport::Discarded), None)
        }
        policy => {
            let reader = reader.expect("configured output pipe was validated after spawn");
            let recorded_path = prepared_record.as_ref().map(|(_, path)| path.clone());
            let (handle, task, failures) =
                spawn_output_pump(reader, policy, stream, prepared_record, observation_sequence);
            (
                handle,
                OutputOwner::Pump {
                    task,
                    unavailable: unavailable_partial_output(policy, recorded_path),
                },
                Some(failures),
            )
        }
    }
}

struct AttachedOwner {
    run_id: ProcessRunId,
    child: tokio::process::Child,
    containment: AttachedGroup,
    containment_strength: ContainmentStrength,
    deadline: Option<tokio::time::Instant>,
    grace: Duration,
    controls: Option<mpsc::UnboundedReceiver<ControlRequest>>,
    completion: Option<tokio::sync::oneshot::Sender<ProcessCompletion>>,
    input_task: Option<JoinHandle<Result<(), super::output::ProcessInputError>>>,
    stdout: OutputOwner,
    stderr: OutputOwner,
    output_failures: mpsc::UnboundedReceiver<ObservedProcessFailure>,
    started_at: Instant,
    _run_directory: Option<RunDirectory>,
}

#[derive(Clone, Copy)]
enum StopReason {
    Cancelled,
    DeadlineExceeded,
    OwnerDropped,
    Infrastructure(InfrastructureTrigger),
}

#[derive(Clone, Copy)]
enum InfrastructureTrigger {
    Input,
    Output(ObservedProcessFailure),
}

impl InfrastructureTrigger {
    fn kind(self) -> ProcessFailureKind {
        match self {
            Self::Input => ProcessFailureKind::InputIo,
            Self::Output(failure) => failure.kind,
        }
    }
}

impl AttachedOwner {
    async fn run(mut self) {
        let outcome = self.run_to_completion().await;
        let completion = self.completion.take().expect("attached owner has one completion sender");
        let mut controls = self.controls.take().expect("attached owner has one control receiver");
        let _ = completion.send(outcome);
        drop(self);
        while let Some(request) = controls.recv().await {
            match request {
                ControlRequest::Cancel { acknowledgement }
                | ControlRequest::ForceKill { acknowledgement } => {
                    let _ = acknowledgement.send(ControlAcknowledgement::AlreadyCompleted);
                }
                ControlRequest::OwnerDropped => {}
            }
        }
    }

    async fn run_to_completion(&mut self) -> ProcessCompletion {
        let mut stop_reason = None;
        let mut termination = TerminationDisposition::NotRequested;
        let mut grace_deadline = None;
        let mut output_failures_open = true;
        let deadline = self.deadline;

        let status = loop {
            tokio::select! {
                status = self.child.wait() => {
                    match status {
                        Ok(status) => break status,
                        Err(_) => {
                            return self.failure(
                                ProcessFailureKind::OwnerTaskFailed,
                                LeaderExitObservation::NotObserved,
                            ).await;
                        }
                    }
                }
                request = self.controls.as_mut().expect("controls exist while owner is live").recv() => {
                    let Some(request) = request else {
                        if stop_reason.is_none() {
                            stop_reason = Some(StopReason::OwnerDropped);
                            termination = self.request_termination();
                            grace_deadline = grace_timer(termination, self.grace);
                        }
                        continue;
                    };
                    match request {
                        ControlRequest::Cancel { acknowledgement } => {
                            let ack = if stop_reason.is_some() {
                                ControlAcknowledgement::AlreadyStopping
                            } else {
                                stop_reason = Some(StopReason::Cancelled);
                                termination = self.request_termination();
                                grace_deadline = grace_timer(termination, self.grace);
                                ControlAcknowledgement::Accepted
                            };
                            let _ = acknowledgement.send(ack);
                        }
                        ControlRequest::ForceKill { acknowledgement } => {
                            let ack = if termination == TerminationDisposition::Forced {
                                ControlAcknowledgement::AlreadyStopping
                            } else {
                                if stop_reason.is_none() {
                                    stop_reason = Some(StopReason::Cancelled);
                                }
                                match self.containment.force_kill() {
                                    Ok(()) => termination = TerminationDisposition::Forced,
                                    Err(_) => {
                                        return self.failure(
                                            ProcessFailureKind::TerminationUnconfirmed,
                                            LeaderExitObservation::NotObserved,
                                        ).await;
                                    }
                                }
                                grace_deadline = grace_timer(termination, self.grace);
                                ControlAcknowledgement::Accepted
                            };
                            let _ = acknowledgement.send(ack);
                        }
                        ControlRequest::OwnerDropped => {
                            if stop_reason.is_none() {
                                stop_reason = Some(StopReason::OwnerDropped);
                                termination = self.request_termination();
                                grace_deadline = grace_timer(termination, self.grace);
                            }
                        }
                    }
                }
                () = wait_until(deadline), if deadline.is_some() && stop_reason.is_none() => {
                    stop_reason = Some(StopReason::DeadlineExceeded);
                    termination = self.request_termination();
                    grace_deadline = grace_timer(termination, self.grace);
                }
                () = wait_until(grace_deadline), if grace_deadline.is_some() => {
                    if termination == TerminationDisposition::Forced {
                        return self.failure(
                            ProcessFailureKind::TerminationUnconfirmed,
                            LeaderExitObservation::NotObserved,
                        ).await;
                    }
                    if self.containment.force_kill().is_err() {
                        return self.failure(
                            ProcessFailureKind::TerminationUnconfirmed,
                            LeaderExitObservation::NotObserved,
                        ).await;
                    }
                    termination = TerminationDisposition::Forced;
                    grace_deadline = grace_timer(termination, self.grace);
                }
                input = wait_input(&mut self.input_task), if self.input_task.is_some() => {
                    self.input_task = None;
                    if !matches!(input, Ok(Ok(()))) && stop_reason.is_none() {
                        stop_reason = Some(StopReason::Infrastructure(InfrastructureTrigger::Input));
                        termination = self.request_termination();
                        grace_deadline = grace_timer(termination, self.grace);
                    }
                }
                failure = self.output_failures.recv(), if output_failures_open => {
                    match failure {
                        Some(failure) if stop_reason.is_none() => {
                            stop_reason = Some(StopReason::Infrastructure(
                                InfrastructureTrigger::Output(failure),
                            ));
                            termination = self.request_termination();
                            grace_deadline = grace_timer(termination, self.grace);
                        }
                        Some(failure) => {
                            if let Some(StopReason::Infrastructure(
                                InfrastructureTrigger::Output(first),
                            )) = stop_reason
                            {
                                let first = earliest_observed_failure(Some(first), Some(failure))
                                    .expect("two observed output failures have an earliest value");
                                stop_reason = Some(StopReason::Infrastructure(
                                    InfrastructureTrigger::Output(first),
                                ));
                            }
                        }
                        None => output_failures_open = false,
                    }
                }
                guardian = self.containment.guardian_exited(), if self.containment.has_guardian() => {
                    self.containment.acknowledge_guardian_exit();
                    if guardian.is_err() || termination != TerminationDisposition::Forced {
                        return self.failure(
                            ProcessFailureKind::ContainmentLost,
                            LeaderExitObservation::NotObserved,
                        ).await;
                    }
                }
            }
        };

        let leader_exit = leader_exit(status);
        if let Some(input_task) = self.input_task.take() {
            if !matches!(
                tokio::time::timeout(OUTPUT_DRAIN_CONFIRMATION, input_task).await,
                Ok(Ok(Ok(())))
            ) {
                return self
                    .failure(
                        ProcessFailureKind::InputIo,
                        LeaderExitObservation::Observed(leader_exit),
                    )
                    .await;
            }
        }
        let mut descendants = DescendantDisposition::EmptyWhenObserved;
        match self.containment.target_members() {
            Ok(members) if !members.is_empty() => {
                descendants = DescendantDisposition::TerminatedAfterLeaderExit;
                if self.containment.force_kill().is_err() {
                    return self
                        .failure(
                            ProcessFailureKind::TerminationUnconfirmed,
                            LeaderExitObservation::Observed(leader_exit),
                        )
                        .await;
                }
                termination = TerminationDisposition::Forced;
                if let Err(failure) = wait_for_empty_containment(&self.containment).await {
                    return self
                        .failure(failure, LeaderExitObservation::Observed(leader_exit))
                        .await;
                }
            }
            Ok(_) => {}
            Err(_) => {
                return self
                    .failure(
                        ProcessFailureKind::ContainmentLost,
                        LeaderExitObservation::Observed(leader_exit),
                    )
                    .await;
            }
        }

        if termination == TerminationDisposition::Forced {
            if self.containment.reap_guardian_after_kill().await.is_err() {
                return self
                    .failure(
                        ProcessFailureKind::TerminationUnconfirmed,
                        LeaderExitObservation::Observed(leader_exit),
                    )
                    .await;
            }
        } else if self.containment.disarm_guardian().await.is_err() {
            return self
                .failure(
                    ProcessFailureKind::ContainmentLost,
                    LeaderExitObservation::Observed(leader_exit),
                )
                .await;
        }
        if let Err(failure) = wait_for_empty_containment(&self.containment).await {
            return self.failure(failure, LeaderExitObservation::Observed(leader_exit)).await;
        }

        let outputs = finish_outputs(&mut self.stdout, &mut self.stderr).await;
        let first_infrastructure = match stop_reason {
            Some(StopReason::Infrastructure(failure)) => Some(failure),
            _ => None,
        };
        let (stdout, stderr, output_failure) = match outputs {
            FinishedOutputs::Complete { stdout, stderr, failure } => (stdout, stderr, failure),
            incomplete @ FinishedOutputs::Incomplete { .. } => {
                let (failure, stdout, stderr) = incomplete.into_failure_evidence();
                let later_failure = failure.expect("incomplete output collection has a failure");
                let failure = first_infrastructure
                    .map(InfrastructureTrigger::kind)
                    .map(|first| preferred_failure(first, Some(later_failure)))
                    .unwrap_or(later_failure);
                return Err(ProcessFailureReport {
                    run_id: self.run_id,
                    failure,
                    leader_exit: LeaderExitObservation::Observed(leader_exit),
                    termination,
                    stdout,
                    stderr,
                });
            }
        };
        let infrastructure_failure = match first_infrastructure {
            Some(InfrastructureTrigger::Input) => Some(ProcessFailureKind::InputIo),
            Some(InfrastructureTrigger::Output(first)) => {
                earliest_observed_failure(Some(first), output_failure).map(|failure| failure.kind)
            }
            None => output_failure.map(|failure| failure.kind),
        };
        if let Some(failure) = infrastructure_failure {
            return Err(ProcessFailureReport {
                run_id: self.run_id,
                failure,
                leader_exit: LeaderExitObservation::Observed(leader_exit),
                termination,
                stdout: partial_from_report(stdout),
                stderr: partial_from_report(stderr),
            });
        }

        let completion = match stop_reason {
            None if matches!(leader_exit, LeaderExit::Signal(_)) => {
                CompletionCause::ExternalTermination
            }
            None => CompletionCause::Natural,
            Some(StopReason::Cancelled) => CompletionCause::Cancelled,
            Some(StopReason::DeadlineExceeded) => CompletionCause::DeadlineExceeded,
            Some(StopReason::OwnerDropped) => CompletionCause::OwnerDropped,
            Some(StopReason::Infrastructure(_)) => unreachable!(),
        };
        Ok(ProcessReport {
            run_id: self.run_id,
            completion,
            leader_exit: LeaderExitObservation::Observed(leader_exit),
            containment: self.containment_strength,
            descendants,
            termination,
            stdout,
            stderr,
            elapsed: self.started_at.elapsed(),
        })
    }

    fn request_termination(&mut self) -> TerminationDisposition {
        match self.containment.terminate() {
            Ok(TerminationRequest::Graceful) => TerminationDisposition::Graceful,
            Ok(TerminationRequest::Forced) | Err(_) => {
                let _ = self.containment.force_kill();
                let _ = self.child.start_kill();
                TerminationDisposition::Forced
            }
        }
    }

    async fn failure(
        &mut self,
        failure: ProcessFailureKind,
        exit_observation: LeaderExitObservation,
    ) -> ProcessCompletion {
        let _ = self.containment.force_kill();
        let _ = self.child.start_kill();
        let mut observed_exit = exit_observation;
        let mut terminal_failure = failure;
        match tokio::time::timeout(OUTPUT_DRAIN_CONFIRMATION, self.child.wait()).await {
            Ok(Ok(status)) => {
                observed_exit = LeaderExitObservation::Observed(leader_exit(status));
            }
            Ok(Err(_)) => {}
            Err(_) => terminal_failure = ProcessFailureKind::TerminationUnconfirmed,
        }
        if self.containment.reap_guardian_after_kill().await.is_err() {
            terminal_failure = ProcessFailureKind::TerminationUnconfirmed;
        }
        if let Err(containment_failure) = wait_for_empty_containment(&self.containment).await {
            terminal_failure = containment_failure;
        }
        let (output_failure, stdout, stderr) =
            finish_outputs(&mut self.stdout, &mut self.stderr).await.into_failure_evidence();
        Err(ProcessFailureReport {
            run_id: self.run_id,
            failure: preferred_failure(terminal_failure, output_failure),
            leader_exit: observed_exit,
            termination: TerminationDisposition::Forced,
            stdout,
            stderr,
        })
    }
}

fn grace_timer(
    termination: TerminationDisposition,
    grace: Duration,
) -> Option<tokio::time::Instant> {
    match termination {
        TerminationDisposition::Graceful => Some(
            tokio::time::Instant::now()
                .checked_add(grace)
                .expect("termination grace was validated before containment"),
        ),
        TerminationDisposition::Forced => Some(
            tokio::time::Instant::now()
                .checked_add(OUTPUT_DRAIN_CONFIRMATION)
                .expect("fixed output drain confirmation fits the platform clock"),
        ),
        TerminationDisposition::NotRequested => None,
    }
}

async fn wait_for_empty_containment(containment: &AttachedGroup) -> Result<(), ProcessFailureKind> {
    let wait = async {
        loop {
            match containment.members() {
                Ok(members) if members.is_empty() => return Ok(()),
                Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                Err(_) => return Err(ProcessFailureKind::ContainmentLost),
            }
        }
    };
    tokio::time::timeout(OUTPUT_DRAIN_CONFIRMATION, wait)
        .await
        .map_err(|_| ProcessFailureKind::TerminationUnconfirmed)?
}

async fn wait_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn wait_input(
    input: &mut Option<JoinHandle<Result<(), super::output::ProcessInputError>>>,
) -> Result<Result<(), super::output::ProcessInputError>, tokio::task::JoinError> {
    match input {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}

enum FinishedOutputs {
    Complete {
        stdout: OutputReport,
        stderr: OutputReport,
        failure: Option<ObservedProcessFailure>,
    },
    Incomplete {
        failure: ProcessFailureKind,
        observed_failure: Option<ObservedProcessFailure>,
        stdout: PartialOutputReport,
        stderr: PartialOutputReport,
    },
}

impl FinishedOutputs {
    fn into_failure_evidence(
        self,
    ) -> (Option<ProcessFailureKind>, PartialOutputReport, PartialOutputReport) {
        match self {
            Self::Complete { stdout, stderr, failure } => (
                failure.map(|failure| failure.kind),
                partial_from_output_report(stdout),
                partial_from_output_report(stderr),
            ),
            Self::Incomplete { failure, observed_failure, stdout, stderr } => {
                let failure = observed_failure
                    .map(|observed| preferred_failure(observed.kind, Some(failure)))
                    .unwrap_or(failure);
                (Some(failure), stdout, stderr)
            }
        }
    }
}

async fn finish_outputs(stdout: &mut OutputOwner, stderr: &mut OutputOwner) -> FinishedOutputs {
    let mut stdout_result = completed_output(stdout);
    let mut stderr_result = completed_output(stderr);
    let mut timed_out = false;
    let deadline = tokio::time::sleep(OUTPUT_DRAIN_CONFIRMATION);
    tokio::pin!(deadline);
    while stdout_result.is_none() || stderr_result.is_none() {
        tokio::select! {
            result = finish_output(stdout), if stdout_result.is_none() => {
                stdout_result = Some(result);
            }
            result = finish_output(stderr), if stderr_result.is_none() => {
                stderr_result = Some(result);
            }
            () = &mut deadline => {
                timed_out = true;
                break;
            }
        }
    }
    if stdout_result.is_none() {
        abort_output(stdout).await;
    }
    if stderr_result.is_none() {
        abort_output(stderr).await;
    }

    if !timed_out
        && matches!(stdout_result.as_ref(), Some(Ok(_)))
        && matches!(stderr_result.as_ref(), Some(Ok(_)))
    {
        if let (Some(Ok(stdout)), Some(Ok(stderr))) = (stdout_result.take(), stderr_result.take()) {
            return FinishedOutputs::Complete {
                stdout: stdout.report,
                stderr: stderr.report,
                failure: earliest_observed_failure(stdout.failure, stderr.failure),
            };
        }
    }

    let failure = if timed_out {
        ProcessFailureKind::TerminationUnconfirmed
    } else {
        stdout_result
            .as_ref()
            .and_then(|result| result.as_ref().err().copied())
            .or_else(|| stderr_result.as_ref().and_then(|result| result.as_ref().err().copied()))
            .expect("an incomplete output collection has a failed output owner")
    };
    FinishedOutputs::Incomplete {
        failure,
        observed_failure: earliest_observed_failure(
            completed_failure(&stdout_result),
            completed_failure(&stderr_result),
        ),
        stdout: partial_output_result(stdout_result, stdout),
        stderr: partial_output_result(stderr_result, stderr),
    }
}

fn completed_failure(
    result: &Option<Result<OutputPumpCompletion, ProcessFailureKind>>,
) -> Option<ObservedProcessFailure> {
    result.as_ref().and_then(|result| result.as_ref().ok()?.failure)
}

async fn abort_output(owner: &mut OutputOwner) {
    if let OutputOwner::Pump { task, .. } = owner {
        task.abort();
        let _ = task.await;
    }
}

fn completed_output(
    owner: &OutputOwner,
) -> Option<Result<OutputPumpCompletion, ProcessFailureKind>> {
    match owner {
        OutputOwner::Complete(report) => {
            Some(Ok(OutputPumpCompletion { report: report.clone(), failure: None }))
        }
        OutputOwner::Pump { .. } => None,
    }
}

async fn finish_output(
    owner: &mut OutputOwner,
) -> Result<OutputPumpCompletion, ProcessFailureKind> {
    match owner {
        OutputOwner::Complete(report) => {
            Ok(OutputPumpCompletion { report: report.clone(), failure: None })
        }
        OutputOwner::Pump { task, .. } => {
            task.await.map_err(|_| ProcessFailureKind::OwnerTaskFailed)
        }
    }
}

fn partial_output_result(
    result: Option<Result<OutputPumpCompletion, ProcessFailureKind>>,
    owner: &OutputOwner,
) -> PartialOutputReport {
    match result {
        Some(Ok(completion)) => partial_from_output_report(completion.report),
        Some(Err(_)) | None => match owner {
            OutputOwner::Complete(report) => partial_from_output_report(report.clone()),
            OutputOwner::Pump { unavailable, .. } => unavailable.clone(),
        },
    }
}

fn preferred_failure(
    first_failure: ProcessFailureKind,
    later_failure: Option<ProcessFailureKind>,
) -> ProcessFailureKind {
    match later_failure {
        Some(
            failure @ (ProcessFailureKind::ContainmentLost
            | ProcessFailureKind::TerminationUnconfirmed),
        ) => failure,
        _ => first_failure,
    }
}

fn fallback_failure(
    run_id: ProcessRunId,
    stdout: OutputPolicy,
    stderr: OutputPolicy,
    run_directory: Option<&RunDirectory>,
    failure: ProcessFailureKind,
) -> ProcessFailureReport {
    ProcessFailureReport {
        run_id,
        failure,
        leader_exit: LeaderExitObservation::NotObserved,
        termination: TerminationDisposition::NotRequested,
        stdout: unavailable_partial_output(
            stdout,
            recorded_artifact(stdout, run_directory, ProcessStream::Stdout),
        ),
        stderr: unavailable_partial_output(
            stderr,
            recorded_artifact(stderr, run_directory, ProcessStream::Stderr),
        ),
    }
}

fn recorded_artifact(
    policy: OutputPolicy,
    run_directory: Option<&RunDirectory>,
    stream: ProcessStream,
) -> Option<RecordedOutputPath> {
    if !matches!(policy, OutputPolicy::Record(_)) {
        return None;
    }
    let run_directory = run_directory
        .expect("recorded attached output was rejected without private process storage");
    Some(RecordedOutputPath::retained(
        recorded_output_path(run_directory.path(), stream),
        run_directory.retention(),
    ))
}

fn partial_from_report(report: OutputReport) -> PartialOutputReport {
    partial_from_output_report(report)
}

fn validate_timing(spec: &ProcessSpec) -> Result<(), ProcessStartError> {
    let now = tokio::time::Instant::now();
    if let ProcessDeadline::After(duration) = spec.deadline {
        if duration > MAX_PROCESS_DURATION || now.checked_add(duration).is_none() {
            return Err(ProcessStartError::DeadlineOutOfRange);
        }
    }
    if spec.termination.grace_period > MAX_PROCESS_DURATION
        || now.checked_add(spec.termination.grace_period).is_none()
    {
        return Err(ProcessStartError::TerminationGraceOutOfRange);
    }
    Ok(())
}

pub(in crate::framework) fn tokio_command(spec: &CommandSpec) -> Command {
    let mut command = Command::new(&spec.program);
    command.args(&spec.arguments).current_dir(&spec.working_directory);
    if spec.environment.base == EnvironmentBase::Empty {
        command.env_clear();
    }
    command.envs(&spec.environment.values);
    for name in &spec.environment.removals {
        command.env_remove(name);
    }
    command
}

fn forward_output_failures(
    receiver: Option<mpsc::UnboundedReceiver<ObservedProcessFailure>>,
    sender: mpsc::UnboundedSender<ObservedProcessFailure>,
) {
    let Some(mut receiver) = receiver else {
        return;
    };
    tokio::spawn(async move {
        while let Some(failure) = receiver.recv().await {
            if sender.send(failure).is_err() {
                return;
            }
        }
    });
}

fn ensure_private_state_root(path: &Path) -> Result<(), ProcessSupervisorBootstrapError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_directory(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path).map_err(|source| {
                ProcessSupervisorBootstrapError::CreateStateDirectory {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
            set_private_permissions(path).map_err(|source| {
                ProcessSupervisorBootstrapError::CreateStateDirectory {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
            let metadata = std::fs::symlink_metadata(path).map_err(|source| {
                ProcessSupervisorBootstrapError::InspectStateDirectory {
                    path: path.to_path_buf(),
                    source,
                }
            })?;
            validate_private_directory(path, &metadata)
        }
        Err(source) => Err(ProcessSupervisorBootstrapError::InspectStateDirectory {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn prepare_attached_run_lease(run_dir: &Path) -> Result<AttachedRunLease, std::io::Error> {
    let writer = AtomicFileWriter::new(run_dir, ATTACHED_RUN_LOCK, ".attached-run");
    let lock = writer.lock().map_err(std::io::Error::other)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        lock.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    lock.sync_all()?;
    writer
        .replace(&run_dir.join(ATTACHED_RUN_MARKER), ATTACHED_RUN_MARKER_BYTES)
        .map_err(std::io::Error::other)?;
    Ok(AttachedRunLease { _lock: lock })
}

#[cfg(unix)]
fn recover_abandoned_attached_runs(
    state_root: &Path,
) -> Result<(), ProcessSupervisorBootstrapError> {
    use std::{
        fs::{OpenOptions, TryLockError},
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };

    let entries = std::fs::read_dir(state_root).map_err(|source| {
        ProcessSupervisorBootstrapError::InspectStateDirectory {
            path: state_root.to_path_buf(),
            source,
        }
    })?;
    for entry in entries {
        let entry =
            entry.map_err(|source| ProcessSupervisorBootstrapError::InspectStateDirectory {
                path: state_root.to_path_buf(),
                source,
            })?;
        let run_dir = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if Uuid::parse_str(&name).is_err() {
            continue;
        }
        let Ok(directory_metadata) = std::fs::symlink_metadata(&run_dir) else {
            continue;
        };
        if validate_private_directory(&run_dir, &directory_metadata).is_err()
            || std::fs::symlink_metadata(run_dir.join(DETACHED_LAUNCH_MARKER)).is_ok()
            || !valid_attached_run_marker(&run_dir)
        {
            continue;
        }

        let lock_path = run_dir.join(ATTACHED_RUN_LOCK);
        let lock = match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&lock_path)
        {
            Ok(lock) => lock,
            Err(_) => continue,
        };
        let Ok(lock_metadata) = lock.metadata() else {
            continue;
        };
        if !lock_metadata.file_type().is_file()
            || lock_metadata.uid() != rustix::process::getuid().as_raw()
            || lock_metadata.mode() & 0o077 != 0
        {
            continue;
        }
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(_)) => continue,
        }
        if std::fs::symlink_metadata(run_dir.join(DETACHED_LAUNCH_MARKER)).is_ok()
            || !valid_attached_run_marker(&run_dir)
        {
            continue;
        }
        std::fs::remove_dir_all(&run_dir).map_err(|source| {
            ProcessSupervisorBootstrapError::InspectStateDirectory { path: run_dir.clone(), source }
        })?;
        std::fs::File::open(state_root).and_then(|directory| directory.sync_all()).map_err(
            |source| ProcessSupervisorBootstrapError::InspectStateDirectory {
                path: state_root.to_path_buf(),
                source,
            },
        )?;
    }
    Ok(())
}

#[cfg(unix)]
fn valid_attached_run_marker(run_dir: &Path) -> bool {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };

    let marker = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(run_dir.join(ATTACHED_RUN_MARKER))
    {
        Ok(marker) => marker,
        Err(_) => return false,
    };
    let Ok(metadata) = marker.metadata() else {
        return false;
    };
    if !metadata.file_type().is_file()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.len() != ATTACHED_RUN_MARKER_BYTES.len() as u64
    {
        return false;
    }
    let mut bytes = Vec::with_capacity(ATTACHED_RUN_MARKER_BYTES.len());
    marker.take(ATTACHED_RUN_MARKER_BYTES.len() as u64 + 1).read_to_end(&mut bytes).is_ok()
        && bytes == ATTACHED_RUN_MARKER_BYTES
}

#[cfg(not(unix))]
fn recover_abandoned_attached_runs(
    _state_root: &Path,
) -> Result<(), ProcessSupervisorBootstrapError> {
    Ok(())
}

fn validate_private_directory(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), ProcessSupervisorBootstrapError> {
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(ProcessSupervisorBootstrapError::UnsafeStateDirectory {
            path: path.to_path_buf(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != rustix::process::getuid().as_raw() || metadata.mode() & 0o077 != 0 {
            return Err(ProcessSupervisorBootstrapError::UnsafeStateDirectory {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(())
}

fn create_private_run_directory(path: &Path) -> Result<(), std::io::Error> {
    let mut builder = DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    set_private_permissions(path)
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}
