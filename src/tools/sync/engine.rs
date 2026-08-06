use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env,
    ffi::OsString,
    num::NonZeroUsize,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    time::Duration,
};

#[cfg(test)]
use std::ffi::OsStr;

use crate::framework::process::{
    CaptureOverflow, CapturePolicy, CommandSpec, ContainmentRequirement, EnvironmentBase,
    InputPolicy, LeaderExit, LeaderExitObservation, OutputPolicy, OutputReport, ProcessByteEvent,
    ProcessByteStream, ProcessControlError, ProcessDeadline, ProcessEnvironment,
    ProcessFailureKind, ProcessLabel, ProcessOutputError, ProcessOutputHandle, ProcessSession,
    ProcessSpec, ProcessStartError, ProcessSupervisor, StreamPolicy, TerminationPolicy,
};
use crate::framework::AtomicFileWriter;
use crate::tailscale::prepare_mutagen_ssh_directory;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::model::{ProjectId, ProjectLifecycle, SyncedProject};

pub(super) const SUPPORTED_VERSION: &str = "0.18.1";
const SESSION_LABEL: &str = "kit.synced-project";
const REMOTE_NODE_LABEL: &str = "kit.remote-node";
const SYNC_PROFILE_LABEL: &str = "kit.sync-profile";
const SYNC_PROFILE_VERSION: &str = "1";
const JSON_TEMPLATE: &str = "{{ json . }}";
const CAPTURE_BYTES: usize = 8 * 1024 * 1024;
const STREAM_BYTES: usize = 1024 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
const MAX_SESSION_ID_BYTES: usize = 512;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const CREATE_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const CANCEL_GRACE: Duration = Duration::from_secs(2);
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const LINUX_WATCH_POLLING_INTERVAL_SECONDS: u32 = 1;
const SSH_TRANSPORT_GENERATION: &str = "ssh-transport-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EndpointPlatform {
    Linux,
    Macos,
}

impl EndpointPlatform {
    const fn local() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Macos
        }
    }

    fn append_watch_configuration(self, endpoint: &str, arguments: &mut Vec<OsString>) {
        if self == Self::Linux {
            arguments.push(
                format!(
                    "--watch-polling-interval-{endpoint}={LINUX_WATCH_POLLING_INTERVAL_SECONDS}"
                )
                .into(),
            );
        }
    }
}

#[derive(Clone)]
pub(super) struct MutagenClient {
    processes: ProcessSupervisor,
    executable: Option<OsString>,
    working_directory: PathBuf,
    data_directory: PathBuf,
    ssh_directory: PathBuf,
    transport_marker: PathBuf,
}

impl MutagenClient {
    pub(super) fn new(
        processes: ProcessSupervisor,
        working_directory: PathBuf,
    ) -> Result<Self, MutagenError> {
        let executable = resolve_executable(&working_directory);
        let data_directory = ProjectDirs::from("", "", "kit")
            .ok_or(MutagenError::DataDirectoryUnavailable)?
            .data_dir()
            .join("mutagen");
        let ssh_directory = prepare_mutagen_ssh_directory()?;
        Self::with_resolved_executable(
            processes,
            working_directory,
            data_directory,
            ssh_directory,
            executable.map(OsString::from),
        )
    }

    #[cfg(test)]
    pub(super) fn with_executable(
        processes: ProcessSupervisor,
        working_directory: PathBuf,
        data_directory: PathBuf,
        executable: impl AsRef<OsStr>,
    ) -> Result<Self, MutagenError> {
        let ssh_directory = data_directory.join("ssh");
        std::fs::create_dir_all(&ssh_directory).map_err(|source| {
            MutagenError::CreateDataDirectory { path: ssh_directory.clone(), source }
        })?;
        std::fs::write(data_directory.join(SSH_TRANSPORT_GENERATION), b"1\n").map_err(
            |source| MutagenError::CreateDataDirectory { path: data_directory.clone(), source },
        )?;
        Self::with_resolved_executable(
            processes,
            working_directory,
            data_directory,
            ssh_directory,
            Some(executable.as_ref().to_owned()),
        )
    }

    fn with_resolved_executable(
        processes: ProcessSupervisor,
        working_directory: PathBuf,
        data_directory: PathBuf,
        ssh_directory: PathBuf,
        executable: Option<OsString>,
    ) -> Result<Self, MutagenError> {
        if !working_directory.is_absolute()
            || !data_directory.is_absolute()
            || !ssh_directory.is_absolute()
        {
            return Err(MutagenError::RelativeRuntimePath);
        }
        std::fs::create_dir_all(&data_directory).map_err(|source| {
            MutagenError::CreateDataDirectory { path: data_directory.clone(), source }
        })?;
        let transport_marker = data_directory.join(SSH_TRANSPORT_GENERATION);
        Ok(Self {
            processes,
            executable,
            working_directory,
            data_directory,
            ssh_directory,
            transport_marker,
        })
    }

    pub(super) async fn verify_installation(&self) -> Result<(), MutagenError> {
        self.ensure_transport_daemon().await?;
        self.verify_version().await?;
        let executable = self.executable.as_ref().ok_or(MutagenError::ExecutableUnavailable)?;
        let bundle = PathBuf::from(executable).with_file_name("mutagen-agents.tar.gz");
        let metadata = bundle
            .metadata()
            .map_err(|source| MutagenError::InspectAgentBundle { path: bundle.clone(), source })?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(MutagenError::InvalidAgentBundle { path: bundle });
        }
        Ok(())
    }

    async fn verify_version(&self) -> Result<(), MutagenError> {
        let output = self
            .capture("inspect Mutagen version", vec!["version".into()], COMMAND_TIMEOUT)
            .await?;
        let version =
            std::str::from_utf8(&output).map_err(MutagenError::VersionEncoding)?.trim().to_owned();
        if version == SUPPORTED_VERSION {
            Ok(())
        } else {
            Err(MutagenError::UnsupportedVersion { found: version })
        }
    }

    pub(super) async fn project_sessions(
        &self,
        project: ProjectId,
    ) -> Result<Vec<Session>, MutagenError> {
        let output = self
            .capture(
                "list Mutagen synchronization sessions",
                vec![
                    "sync".into(),
                    "list".into(),
                    format!("--label-selector={}", project_selector(project)).into(),
                    "--template".into(),
                    JSON_TEMPLATE.into(),
                ],
                COMMAND_TIMEOUT,
            )
            .await?;
        decode_sessions(&output)
    }

    pub(super) async fn session_inventory(
        &self,
    ) -> Result<HashMap<ProjectId, Vec<Session>>, MutagenError> {
        let output = self
            .capture(
                "inventory Mutagen synchronization sessions",
                vec![
                    "sync".into(),
                    "list".into(),
                    format!("--label-selector={SESSION_LABEL}").into(),
                    "--template".into(),
                    JSON_TEMPLATE.into(),
                ],
                COMMAND_TIMEOUT,
            )
            .await?;
        let mut inventory = HashMap::new();
        for session in decode_sessions(&output)? {
            let value =
                session.labels.get(SESSION_LABEL).ok_or(MutagenError::MissingProjectIdentity)?;
            let project = value
                .parse()
                .map_err(|source| MutagenError::ProjectIdentity { value: value.clone(), source })?;
            inventory.entry(project).or_insert_with(Vec::new).push(session);
        }
        Ok(inventory)
    }

    pub(super) async fn create(
        &self,
        project: &SyncedProject,
        remote_host: &str,
        remote_platform: EndpointPlatform,
    ) -> Result<(), MutagenError> {
        let remote_root =
            project.remote().root().to_str().ok_or(MutagenError::RemotePathEncoding)?;
        if remote_host.is_empty() || remote_host.chars().any(char::is_control) {
            return Err(MutagenError::RemoteHost);
        }
        let remote = format!("{}@{remote_host}:{remote_root}", project.remote().unix_user());
        let mut arguments = vec![
            "sync".into(),
            "create".into(),
            "--no-global-configuration".into(),
            format!("--name={}", project.name()).into(),
            format!("--label={}", project_selector(project.id())).into(),
            format!("--label={REMOTE_NODE_LABEL}={}", project.remote().stable_node_id()).into(),
            format!("--label={SYNC_PROFILE_LABEL}={SYNC_PROFILE_VERSION}").into(),
            "--mode=two-way-safe".into(),
            "--ignore-vcs".into(),
        ];
        EndpointPlatform::local().append_watch_configuration("alpha", &mut arguments);
        remote_platform.append_watch_configuration("beta", &mut arguments);
        arguments.extend(
            project
                .source()
                .excludes()
                .into_iter()
                .map(|pattern| format!("--ignore={pattern}").into()),
        );
        arguments.extend(
            project.source().includes().map(|pattern| format!("--ignore=!{pattern}").into()),
        );
        if project.lifecycle() == ProjectLifecycle::Paused {
            arguments.push("--paused".into());
        }
        arguments.push(project.local().root().as_os_str().to_owned());
        arguments.push(remote.into());
        self.capture("create Mutagen synchronization session", arguments, CREATE_TIMEOUT).await?;
        Ok(())
    }

    pub(super) async fn flush(&self, project: ProjectId) -> Result<(), MutagenError> {
        let mut monitor = self.monitor(project).await?;
        let mut last_status = None;
        let reconnect = tokio::time::timeout(RECONNECT_TIMEOUT, async {
            loop {
                let Some(sessions) = monitor.next().await? else {
                    return Err(MutagenError::MonitorEnded);
                };
                let session = match sessions.as_slice() {
                    [] => {
                        last_status = None;
                        continue;
                    }
                    [session] => session,
                    sessions => return Err(MutagenError::SessionMultiplicity(sessions.len())),
                };
                last_status = Some(session.status);
                if session.paused {
                    return Err(MutagenError::SessionPaused);
                }
                if matches!(
                    session.status,
                    SessionStatus::HaltedOnRootEmptied
                        | SessionStatus::HaltedOnRootDeletion
                        | SessionStatus::HaltedOnRootTypeChange
                ) {
                    return Err(MutagenError::SessionHalted { status: session.status });
                }
                if session.alpha.connected && session.beta.connected {
                    return Ok(());
                }
            }
        })
        .await;
        let stop = monitor.stop().await;
        match reconnect {
            Ok(Ok(())) => {
                stop?;
                self.control(Control::Flush, project).await
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(MutagenError::SessionReconnectTimeout {
                timeout_seconds: RECONNECT_TIMEOUT.as_secs(),
                status: last_status,
            }),
        }
    }

    pub(super) async fn pause(&self, project: ProjectId) -> Result<(), MutagenError> {
        self.control(Control::Pause, project).await
    }

    pub(super) async fn resume(&self, project: ProjectId) -> Result<(), MutagenError> {
        self.control(Control::Resume, project).await
    }

    pub(super) async fn terminate(&self, project: ProjectId) -> Result<(), MutagenError> {
        self.control(Control::Terminate, project).await
    }

    pub(super) async fn monitor(&self, project: ProjectId) -> Result<SessionMonitor, MutagenError> {
        self.ensure_transport_daemon().await?;
        let arguments = vec![
            "sync".into(),
            "monitor".into(),
            format!("--label-selector={}", project_selector(project)).into(),
            "--template".into(),
            JSON_TEMPLATE.into(),
        ];
        let spec = self.spec(
            "monitor Mutagen synchronization session",
            arguments,
            OutputPolicy::Stream(StreamPolicy::new(non_zero(STREAM_BYTES))),
            OutputPolicy::Capture(CapturePolicy::new(
                non_zero(CAPTURE_BYTES),
                CaptureOverflow::FailAndTerminate,
            )),
            ProcessDeadline::Unlimited,
        )?;
        let started = self.processes.spawn(spec).await?;
        let snapshots = match started.stdout {
            ProcessOutputHandle::Stream(stream) => stream,
            _ => return Err(MutagenError::OutputUnavailable),
        };
        Ok(SessionMonitor { session: started.session, snapshots, buffer: Vec::new() })
    }

    async fn control(&self, control: Control, project: ProjectId) -> Result<(), MutagenError> {
        self.capture(
            control.process_label(),
            vec![
                "sync".into(),
                control.command().into(),
                format!("--label-selector={}", project_selector(project)).into(),
            ],
            COMMAND_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    async fn capture(
        &self,
        process_label: &str,
        arguments: Vec<OsString>,
        timeout: Duration,
    ) -> Result<Vec<u8>, MutagenError> {
        self.ensure_transport_daemon().await?;
        self.capture_unchecked(process_label, arguments, timeout).await
    }

    async fn capture_unchecked(
        &self,
        process_label: &str,
        arguments: Vec<OsString>,
        timeout: Duration,
    ) -> Result<Vec<u8>, MutagenError> {
        let capture = OutputPolicy::Capture(CapturePolicy::new(
            non_zero(CAPTURE_BYTES),
            CaptureOverflow::FailAndTerminate,
        ));
        let spec =
            self.spec(process_label, arguments, capture, capture, ProcessDeadline::After(timeout))?;
        let report = self
            .processes
            .spawn(spec)
            .await?
            .session
            .wait()
            .await
            .map_err(|failure| MutagenError::Supervision(failure.failure))?;
        let stdout = captured(&report.stdout).ok_or(MutagenError::OutputUnavailable)?;
        let stderr = captured(&report.stderr).ok_or(MutagenError::OutputUnavailable)?;
        match report.leader_exit {
            LeaderExitObservation::Observed(LeaderExit::Code(0)) => Ok(stdout.to_vec()),
            exit => Err(MutagenError::CommandFailed {
                exit,
                detail: String::from_utf8_lossy(stderr).trim().to_owned(),
            }),
        }
    }

    async fn ensure_transport_daemon(&self) -> Result<(), MutagenError> {
        if self.transport_marker.is_file() {
            return Ok(());
        }
        let writer = AtomicFileWriter::new(&self.data_directory, ".transport.lock", ".transport");
        let lock = writer.lock()?;
        if self.transport_marker.is_file() {
            return Ok(());
        }
        self.capture_unchecked(
            "stop pre-hermetic Mutagen daemon",
            vec!["daemon".into(), "stop".into()],
            COMMAND_TIMEOUT,
        )
        .await?;
        writer.replace(&self.transport_marker, b"1\n")?;
        drop(lock);
        Ok(())
    }

    fn spec(
        &self,
        process_label: &str,
        arguments: Vec<OsString>,
        stdout: OutputPolicy,
        stderr: OutputPolicy,
        deadline: ProcessDeadline,
    ) -> Result<ProcessSpec, MutagenError> {
        let environment = ProcessEnvironment::new(
            EnvironmentBase::Inherit,
            BTreeMap::from([
                (
                    OsString::from("MUTAGEN_DATA_DIRECTORY"),
                    self.data_directory.as_os_str().to_owned(),
                ),
                (OsString::from("MUTAGEN_SSH_PATH"), self.ssh_directory.as_os_str().to_owned()),
            ]),
            BTreeSet::new(),
        )?;
        let command = CommandSpec::new(
            self.executable.clone().ok_or(MutagenError::ExecutableUnavailable)?,
            arguments,
            self.working_directory.clone(),
            environment,
            ProcessLabel::new(process_label.to_owned())?,
        )?;
        Ok(ProcessSpec::new(
            command,
            InputPolicy::Closed,
            stdout,
            stderr,
            ContainmentRequirement::ExplicitProcessGroup,
            deadline,
            TerminationPolicy::new(CANCEL_GRACE),
        ))
    }
}

pub(super) struct SessionMonitor {
    session: ProcessSession,
    snapshots: ProcessByteStream,
    buffer: Vec<u8>,
}

impl SessionMonitor {
    pub(super) async fn next(&mut self) -> Result<Option<Vec<Session>>, MutagenError> {
        loop {
            if let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let mut line = self.buffer.drain(..=end).collect::<Vec<_>>();
                line.pop();
                if line.is_empty() {
                    continue;
                }
                return decode_sessions(&line).map(Some);
            }
            match self.snapshots.next().await? {
                ProcessByteEvent::Chunk { bytes, .. } => {
                    self.buffer.extend_from_slice(&bytes);
                    if self.buffer.len() > MAX_SNAPSHOT_BYTES {
                        let _ = self.session.control().cancel().await;
                        return Err(MutagenError::SnapshotTooLarge);
                    }
                }
                ProcessByteEvent::End => {
                    if self.buffer.is_empty() {
                        return Ok(None);
                    }
                    let line = std::mem::take(&mut self.buffer);
                    return decode_sessions(&line).map(Some);
                }
            }
        }
    }

    pub(super) async fn stop(self) -> Result<(), MutagenError> {
        let control = self.session.control();
        control.cancel().await?;
        self.session.wait().await.map_err(|failure| MutagenError::Supervision(failure.failure))?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum SessionStatus {
    Disconnected,
    HaltedOnRootEmptied,
    HaltedOnRootDeletion,
    HaltedOnRootTypeChange,
    ConnectingAlpha,
    ConnectingBeta,
    Watching,
    Scanning,
    WaitingForRescan,
    Reconciling,
    StagingAlpha,
    StagingBeta,
    Transitioning,
    Saving,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum SynchronizationMode {
    TwoWaySafe,
    TwoWayResolved,
    OneWaySafe,
    OneWayReplica,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EndpointState {
    pub protocol: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub host: String,
    pub path: String,
    pub connected: bool,
    #[serde(default)]
    pub scanned: bool,
    #[serde(default)]
    pub scan_problems: Vec<Problem>,
    #[serde(default)]
    pub excluded_scan_problems: u64,
    #[serde(default)]
    pub transition_problems: Vec<Problem>,
    #[serde(default)]
    pub excluded_transition_problems: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Problem {
    pub path: String,
    pub error: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Change {
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Conflict {
    pub root: String,
    pub alpha_changes: Vec<Change>,
    pub beta_changes: Vec<Change>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Session {
    pub identifier: SessionId,
    pub creating_version: String,
    pub alpha: EndpointState,
    pub beta: EndpointState,
    pub mode: SynchronizationMode,
    pub name: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub paused: bool,
    pub status: SessionStatus,
    #[serde(default)]
    pub last_error: String,
    #[serde(default)]
    pub successful_cycles: u64,
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
    #[serde(default)]
    pub excluded_conflicts: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct SessionId(String);

impl SessionId {
    fn validate(&self) -> Result<(), MutagenError> {
        if self.0.is_empty()
            || self.0.len() > MAX_SESSION_ID_BYTES
            || !self.0.is_ascii()
            || !self
                .0
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            Err(MutagenError::SessionIdentity(self.0.clone()))
        } else {
            Ok(())
        }
    }
}

impl Session {
    fn validate(&self) -> Result<(), MutagenError> {
        self.identifier.validate()
    }

    pub(super) fn health(&self) -> SessionHealth {
        if self.paused {
            SessionHealth::Paused
        } else if !self.conflicts.is_empty() || self.excluded_conflicts > 0 {
            SessionHealth::Conflicted
        } else if self.status == SessionStatus::Disconnected
            || !self.alpha.connected
            || !self.beta.connected
        {
            SessionHealth::Offline
        } else if !self.last_error.is_empty()
            || matches!(
                self.status,
                SessionStatus::HaltedOnRootEmptied
                    | SessionStatus::HaltedOnRootDeletion
                    | SessionStatus::HaltedOnRootTypeChange
            )
        {
            SessionHealth::Error
        } else if self.status == SessionStatus::Watching {
            SessionHealth::Healthy
        } else {
            SessionHealth::Synchronizing
        }
    }

    pub(super) fn mark_paused(&mut self) {
        self.paused = true;
    }

    pub(super) fn mark_flushed(&mut self) {
        self.paused = false;
        self.alpha.connected = true;
        self.beta.connected = true;
        self.status = SessionStatus::Watching;
    }

    pub(super) fn matches_project(&self, project: &SyncedProject) -> bool {
        let project_id = project.id().to_string();
        self.mode == SynchronizationMode::TwoWaySafe
            && self.name == project.name()
            && self.alpha.protocol == "local"
            && self.beta.protocol == "ssh"
            && project.local().root().to_str().is_some_and(|path| self.alpha.path == path)
            && self.beta.user == project.remote().unix_user()
            && project.remote().root().to_str().is_some_and(|path| self.beta.path == path)
            && self.labels.get(SESSION_LABEL).map(String::as_str) == Some(project_id.as_str())
            && self.labels.get(REMOTE_NODE_LABEL).map(String::as_str)
                == Some(project.remote().stable_node_id())
            && self.labels.get(SYNC_PROFILE_LABEL).map(String::as_str) == Some(SYNC_PROFILE_VERSION)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum SessionHealth {
    Healthy,
    Synchronizing,
    Paused,
    Conflicted,
    Offline,
    Error,
}

impl SessionHealth {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Synchronizing => "syncing",
            Self::Paused => "paused",
            Self::Conflicted => "conflicted",
            Self::Offline => "offline",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Error)]
pub(super) enum MutagenError {
    #[error("Mutagen {SUPPORTED_VERSION} is not installed on PATH")]
    ExecutableUnavailable,
    #[error("inspect Mutagen agent bundle {}", path.display())]
    InspectAgentBundle {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Mutagen agent bundle {} is not a non-empty file", path.display())]
    InvalidAgentBundle { path: PathBuf },
    #[error("resolve Kit data directory for Mutagen")]
    DataDirectoryUnavailable,
    #[error("Mutagen working and data directories must be absolute")]
    RelativeRuntimePath,
    #[error("create Mutagen data directory {}", path.display())]
    CreateDataDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("start Mutagen process")]
    Start(#[from] ProcessStartError),
    #[error("Mutagen process supervision failed: {0:?}")]
    Supervision(ProcessFailureKind),
    #[error("Mutagen process output is unavailable")]
    OutputUnavailable,
    #[error("Mutagen command exited unsuccessfully ({exit:?}): {detail}")]
    CommandFailed { exit: LeaderExitObservation, detail: String },
    #[error("the Synced Project is paused; resume it before syncing")]
    SessionPaused,
    #[error("the Synced Project is halted ({status:?}); run `kit sync doctor`")]
    SessionHalted { status: SessionStatus },
    #[error(
        "the Synced Project did not reconnect within {timeout_seconds} seconds \
         (last status: {status:?})"
    )]
    SessionReconnectTimeout { timeout_seconds: u64, status: Option<SessionStatus> },
    #[error("Mutagen version output is not UTF-8")]
    VersionEncoding(#[source] std::str::Utf8Error),
    #[error("Mutagen {found:?} is unsupported; install exactly {SUPPORTED_VERSION}")]
    UnsupportedVersion { found: String },
    #[error("decode Mutagen synchronization session JSON")]
    DecodeSessions(#[source] serde_json::Error),
    #[error("Mutagen returned invalid synchronization session identity {0:?}")]
    SessionIdentity(String),
    #[error("Mutagen synchronization session has no Synced Project identity")]
    MissingProjectIdentity,
    #[error("Mutagen returned invalid Synced Project identity {value:?}")]
    ProjectIdentity {
        value: String,
        #[source]
        source: uuid::Error,
    },
    #[error("Mutagen monitor returned {0} sessions for one Synced Project")]
    SessionMultiplicity(usize),
    #[error("Mutagen monitor ended before the Synced Project reconnected")]
    MonitorEnded,
    #[error("Mutagen monitor snapshot exceeds {MAX_SNAPSHOT_BYTES} bytes")]
    SnapshotTooLarge,
    #[error("Mutagen monitor output failed")]
    MonitorOutput(#[from] ProcessOutputError),
    #[error("stop Mutagen monitor")]
    MonitorControl(#[from] ProcessControlError),
    #[error("remote synchronization path is not UTF-8")]
    RemotePathEncoding,
    #[error("remote synchronization host is empty or contains control characters")]
    RemoteHost,
    #[error(transparent)]
    Environment(#[from] crate::framework::process::ProcessEnvironmentError),
    #[error(transparent)]
    Command(#[from] crate::framework::process::CommandSpecError),
    #[error(transparent)]
    Label(#[from] crate::framework::process::ProcessLabelError),
    #[error(transparent)]
    TailscaleSshState(#[from] crate::tailscale::TailscaleSshStateError),
    #[error(transparent)]
    AtomicFile(#[from] crate::framework::AtomicFileError),
}

fn project_selector(project: ProjectId) -> String {
    format!("{SESSION_LABEL}={project}")
}

#[derive(Clone, Copy)]
enum Control {
    Flush,
    Pause,
    Resume,
    Terminate,
}

impl Control {
    const fn command(self) -> &'static str {
        match self {
            Self::Flush => "flush",
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Terminate => "terminate",
        }
    }

    const fn process_label(self) -> &'static str {
        match self {
            Self::Flush => "flush Mutagen synchronization session",
            Self::Pause => "pause Mutagen synchronization session",
            Self::Resume => "resume Mutagen synchronization session",
            Self::Terminate => "terminate Mutagen synchronization session",
        }
    }
}

fn resolve_executable(working_directory: &std::path::Path) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for directory in env::split_paths(&path) {
        let directory =
            if directory.is_absolute() { directory } else { working_directory.join(directory) };
        let candidate = directory.join("mutagen");
        let Ok(metadata) = candidate.metadata() else { continue };
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            return Some(candidate);
        }
    }
    None
}

fn captured(output: &OutputReport) -> Option<&[u8]> {
    match output {
        OutputReport::Captured(capture) => Some(&capture.bytes),
        _ => None,
    }
}

fn decode_sessions(bytes: &[u8]) -> Result<Vec<Session>, MutagenError> {
    let sessions: Vec<Session> =
        serde_json::from_slice(bytes).map_err(MutagenError::DecodeSessions)?;
    for session in &sessions {
        session.validate()?;
    }
    Ok(sessions)
}

fn non_zero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("process output bounds are non-zero")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{
        framework::process::test_support::{CommandFixture, CommandResponse},
        tools::sync::model::{LocalEndpoint, RemoteEndpoint, SourcePolicy, SyncedProject},
    };

    use super::*;

    const SESSION_JSON: &str = r#"[{"identifier":"sync_test","creatingVersion":"0.18.1","alpha":{"protocol":"local","path":"/work/project","connected":true,"scanned":true},"beta":{"protocol":"ssh","user":"remote-user","host":"remote-node.test.ts.net","path":"/workspace/project","connected":true,"scanned":true},"mode":"two-way-safe","name":"project","labels":{"kit.synced-project":"2b59d60f-6f50-43f1-957a-6a3603975f99","kit.remote-node":"node-remote"},"paused":false,"status":"watching","successfulCycles":3}]"#;

    fn project() -> SyncedProject {
        SyncedProject::new(
            "project",
            LocalEndpoint::new(PathBuf::from("/work/project")).unwrap(),
            RemoteEndpoint::new("node-remote", "remote-user", PathBuf::from("/workspace/project"))
                .unwrap(),
            SourcePolicy::new(vec!["target".to_owned()], vec!["target/schema".to_owned()]).unwrap(),
        )
        .unwrap()
    }

    fn client(fixture: &CommandFixture) -> MutagenClient {
        MutagenClient::with_executable(
            ProcessSupervisor::for_test(fixture.root().join("processes")).unwrap(),
            fixture.root().to_path_buf(),
            fixture.root().join("mutagen"),
            fixture.executable(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn adapter_uses_exact_version_structured_list_and_safe_create_contract() {
        let project = project();
        let selector = format!("--label-selector={}", project_selector(project.id()));
        let mut fixture = CommandFixture::new().unwrap();
        fixture.respond(["version"], CommandResponse::success().stdout("0.18.1\n")).unwrap();
        fixture
            .respond(
                ["sync", "list", selector.as_str(), "--template", JSON_TEMPLATE],
                CommandResponse::success().stdout(SESSION_JSON),
            )
            .unwrap();
        let mut create_arguments = vec![
            "sync".into(),
            "create".into(),
            "--no-global-configuration".into(),
            "--name=project".into(),
            format!("--label={}", project_selector(project.id())).into(),
            "--label=kit.remote-node=node-remote".into(),
            "--label=kit.sync-profile=1".into(),
            "--mode=two-way-safe".into(),
            "--ignore-vcs".into(),
        ];
        EndpointPlatform::local().append_watch_configuration("alpha", &mut create_arguments);
        EndpointPlatform::Macos.append_watch_configuration("beta", &mut create_arguments);
        create_arguments.extend(
            [
                "--ignore=node_modules",
                "--ignore=target",
                "--ignore=.venv",
                "--ignore=venv",
                "--ignore=dist",
                "--ignore=build",
                "--ignore=.cache",
                "--ignore=.next",
                "--ignore=.turbo",
                "--ignore=coverage",
                "--ignore=__pycache__",
                "--ignore=*.pyc",
                "--ignore=.DS_Store",
                "--ignore=.env",
                "--ignore=.env.local",
                "--ignore=.env.*.local",
                "--ignore=.direnv",
                "--ignore=.pytest_cache",
                "--ignore=!target/schema",
                "/work/project",
                "remote-user@kit-node-node-remote-remote-user:/workspace/project",
            ]
            .into_iter()
            .map(OsString::from),
        );
        fixture.respond(create_arguments, CommandResponse::success()).unwrap();
        let client = client(&fixture);

        client.verify_version().await.unwrap();
        let sessions = client.project_sessions(project.id()).await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Watching);
        assert_eq!(sessions[0].mode, SynchronizationMode::TwoWaySafe);
        client
            .create(&project, "kit-node-node-remote-remote-user", EndpointPlatform::Macos)
            .await
            .unwrap();
    }

    #[test]
    fn every_mutagen_process_is_bound_to_kits_private_runtime_and_ssh_transport() {
        let fixture = CommandFixture::new().unwrap();
        let client = client(&fixture);
        let spec = client
            .spec(
                "inspect Mutagen",
                vec!["version".into()],
                OutputPolicy::Discard,
                OutputPolicy::Discard,
                ProcessDeadline::After(COMMAND_TIMEOUT),
            )
            .unwrap();

        assert_eq!(
            spec.command
                .environment
                .values
                .get(OsStr::new("MUTAGEN_DATA_DIRECTORY"))
                .map(OsString::as_os_str),
            Some(client.data_directory.as_os_str())
        );
        assert_eq!(
            spec.command
                .environment
                .values
                .get(OsStr::new("MUTAGEN_SSH_PATH"))
                .map(OsString::as_os_str),
            Some(client.ssh_directory.as_os_str())
        );
    }

    #[tokio::test]
    async fn first_hermetic_client_stops_the_preexisting_daemon_once() {
        let mut fixture = CommandFixture::new().unwrap();
        fixture.respond(["daemon", "stop"], CommandResponse::success()).unwrap();
        let client = client(&fixture);
        std::fs::remove_file(&client.transport_marker).unwrap();

        client.ensure_transport_daemon().await.unwrap();
        client.ensure_transport_daemon().await.unwrap();

        assert_eq!(fixture.invocations().unwrap().len(), 1);
        assert_eq!(
            fixture.invocations().unwrap()[0].arguments,
            [OsString::from("daemon"), OsString::from("stop")]
        );
        assert_eq!(std::fs::read(&client.transport_marker).unwrap(), b"1\n");
    }

    #[tokio::test]
    async fn adapter_projects_nonzero_exit_and_malformed_json_as_typed_errors() {
        let project = project();
        let selector = format!("--label-selector={}", project_selector(project.id()));
        let mut fixture = CommandFixture::new().unwrap();
        fixture
            .respond(["version"], CommandResponse::exit(1).stderr("mutagen unavailable"))
            .unwrap();
        fixture
            .respond(
                ["sync", "list", selector.as_str(), "--template", JSON_TEMPLATE],
                CommandResponse::success().stdout("{"),
            )
            .unwrap();
        let client = client(&fixture);

        assert!(matches!(
            client.verify_version().await,
            Err(MutagenError::CommandFailed { detail, .. }) if detail == "mutagen unavailable"
        ));
        assert!(matches!(
            client.project_sessions(project.id()).await,
            Err(MutagenError::DecodeSessions(_))
        ));
    }
}
