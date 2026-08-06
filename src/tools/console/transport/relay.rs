use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    num::NonZeroUsize,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::watch,
    task::JoinHandle,
};

use crate::framework::process::{
    CaptureOverflow, CapturePolicy, CommandSpec, ContainmentRequirement, EnvironmentBase,
    InputPolicy, LeaderExitObservation, OutputPolicy, ProcessByteEvent, ProcessDeadline,
    ProcessEnvironment, ProcessFailureKind, ProcessInputHandle, ProcessLabel, ProcessOutputHandle,
    ProcessSession, ProcessSpec, ProcessSupervisor, StreamPolicy, TerminationPolicy,
};

const COPY_BUFFER_BYTES: usize = 32 * 1024;
const STREAM_BUDGET: NonZeroUsize = NonZeroUsize::new(256 * 1024).unwrap();
const TERMINATION_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RelayEpochOutcome {
    epoch: u64,
    kind: RelayEpochOutcomeKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelayEpochOutcomeKind {
    TransportExited { exit: LeaderExitObservation },
    LocalDisconnected,
    Cancelled,
    Failed(RelayEpochFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RelayEpochFailure {
    Preflight,
    Start,
    LocalIo,
    TransportInput,
    TransportOutput,
    Supervision(ProcessFailureKind),
}

#[derive(Debug, Error)]
pub(crate) enum SshRelayError {
    #[error("the initial Console relay preflight failed")]
    InitialPreflight,
    #[error("prepare private Console relay storage: {0}")]
    Prepare(String),
    #[error("create private Console relay workspace: {0}")]
    Workspace(String),
    #[error("prepare private Console relay runtime: {0}")]
    Runtime(String),
    #[error("bind private Console relay socket {}: {source}", path.display())]
    Bind {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("protect private Console relay socket {}: {source}", path.display())]
    Protect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("accept a Console relay connection: {0}")]
    Accept(#[source] std::io::Error),
    #[error("Console relay owner task failed: {0}")]
    Owner(String),
}

#[async_trait]
pub(crate) trait RelayEpochProvider: Send + Sync {
    async fn prepare(&self) -> Result<PreparedRelayEpoch, RelayEpochFailure>;
}

pub(crate) struct PreparedRelayEpoch {
    arguments: Vec<OsString>,
}

impl PreparedRelayEpoch {
    pub(crate) fn new(arguments: Vec<OsString>) -> Self {
        Self { arguments }
    }
}

/// One stable, private relay listener whose owner serializes OpenSSH transport epochs.
pub(crate) struct SshRelay {
    socket_path: PathBuf,
    outcomes: watch::Receiver<Option<RelayEpochOutcome>>,
    cancel: watch::Sender<bool>,
    owner: Option<JoinHandle<Result<(), SshRelayError>>>,
}

struct RelaySocketLease {
    path: PathBuf,
}

struct RelayListener {
    listener: UnixListener,
    socket: RelaySocketLease,
}

struct RelayOwner {
    processes: ProcessSupervisor,
    provider: Arc<dyn RelayEpochProvider>,
    initial_epoch: PreparedRelayEpoch,
    program: OsString,
    workspace: crate::framework::process::ProcessWorkspace,
}

impl Drop for RelaySocketLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl SshRelay {
    pub(crate) async fn start(
        processes: &ProcessSupervisor,
        provider: Arc<dyn RelayEpochProvider>,
    ) -> Result<Self, SshRelayError> {
        Self::start_with_program(processes, provider, OsString::from("ssh")).await
    }

    async fn start_with_program(
        processes: &ProcessSupervisor,
        provider: Arc<dyn RelayEpochProvider>,
        program: OsString,
    ) -> Result<Self, SshRelayError> {
        let initial_epoch =
            provider.prepare().await.map_err(|_| SshRelayError::InitialPreflight)?;
        let prepared =
            processes.prepare().map_err(|error| SshRelayError::Prepare(error.to_string()))?;
        let workspace = prepared
            .create_workspace()
            .map_err(|error| SshRelayError::Workspace(error.to_string()))?;
        drop(prepared);

        let listener = bind_relay_socket()?;
        let socket_path = listener.socket.path.clone();

        let (cancel, cancel_receiver) = watch::channel(false);
        let (outcome_sender, outcomes) = watch::channel(None);
        let process_owner = processes.clone();
        let owner = tokio::spawn(async move {
            let owner = RelayOwner {
                processes: process_owner,
                provider,
                initial_epoch,
                program,
                workspace,
            };
            run_owner(listener, owner, cancel_receiver, outcome_sender).await
        });

        Ok(Self { socket_path, outcomes, cancel, owner: Some(owner) })
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub(crate) async fn shutdown(mut self) -> Result<(), SshRelayError> {
        let _ = self.cancel.send(true);
        let owner = self.owner.take().expect("a live relay owns one owner task");
        let result = owner.await.map_err(|error| SshRelayError::Owner(error.to_string()))?;
        let _last_outcome = *self.outcomes.borrow_and_update();
        result
    }
}

impl Drop for SshRelay {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
    }
}

async fn run_owner(
    relay: RelayListener,
    owner: RelayOwner,
    mut cancel: watch::Receiver<bool>,
    outcomes: watch::Sender<Option<RelayEpochOutcome>>,
) -> Result<(), SshRelayError> {
    let RelayOwner { processes, provider, initial_epoch, program, workspace } = owner;
    let RelayListener { listener, socket } = relay;
    let working_directory = workspace.as_path().to_owned();
    let _workspace = workspace;
    let _socket = socket;
    let mut epoch = 0u64;
    let mut initial_epoch = Some(initial_epoch);
    loop {
        let socket = tokio::select! {
            accepted = listener.accept() => accepted.map_err(SshRelayError::Accept)?.0,
            _ = cancelled(&mut cancel) => return Ok(()),
        };
        epoch = epoch.saturating_add(1);
        let prepared = if let Some(prepared) = initial_epoch.take() {
            prepared
        } else {
            let preparation = tokio::select! {
                prepared = provider.prepare() => prepared,
                _ = cancelled(&mut cancel) => return Ok(()),
            };
            match preparation {
                Ok(prepared) => prepared,
                Err(failure) => {
                    outcomes.send_replace(Some(RelayEpochOutcome {
                        epoch,
                        kind: RelayEpochOutcomeKind::Failed(failure),
                    }));
                    drop(socket);
                    continue;
                }
            }
        };
        let kind = run_epoch(
            &processes,
            socket,
            program.clone(),
            prepared.arguments,
            &working_directory,
            &mut cancel,
        )
        .await;
        outcomes.send_replace(Some(RelayEpochOutcome { epoch, kind }));
        if matches!(kind, RelayEpochOutcomeKind::Cancelled) {
            return Ok(());
        }
    }
}

fn bind_relay_socket() -> Result<RelayListener, SshRelayError> {
    let runtime_dir = super::super::runtime::directory()
        .map_err(|error| SshRelayError::Runtime(error.to_string()))?;
    super::super::runtime::prepare(&runtime_dir)
        .map_err(|error| SshRelayError::Runtime(error.to_string()))?;
    let socket_path = runtime_dir.join(format!("r-{}.sock", uuid::Uuid::new_v4().simple()));
    super::super::runtime::validate_socket_path(&socket_path)
        .map_err(|error| SshRelayError::Runtime(error.to_string()))?;
    let socket = RelaySocketLease { path: socket_path.clone() };
    let listener = UnixListener::bind(&socket_path)
        .map_err(|source| SshRelayError::Bind { path: socket_path.clone(), source })?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        .map_err(|source| SshRelayError::Protect { path: socket_path, source })?;
    Ok(RelayListener { listener, socket })
}

async fn run_epoch(
    processes: &ProcessSupervisor,
    socket: UnixStream,
    program: OsString,
    arguments: Vec<OsString>,
    working_directory: &Path,
    cancel: &mut watch::Receiver<bool>,
) -> RelayEpochOutcomeKind {
    let spec = match ssh_spec(program, arguments, working_directory) {
        Ok(spec) => spec,
        Err(_) => return RelayEpochOutcomeKind::Failed(RelayEpochFailure::Start),
    };
    let started = match processes.spawn(spec).await {
        Ok(started) => started,
        Err(_) => return RelayEpochOutcomeKind::Failed(RelayEpochFailure::Start),
    };
    let (mut input, mut output) = match (started.input, started.stdout) {
        (ProcessInputHandle::Writable(input), ProcessOutputHandle::Stream(output)) => {
            (Some(input), output)
        }
        (input, output) => return reap_misconfigured(started.session, input, output).await,
    };

    let control = started.session.control();
    let wait = started.session.wait();
    tokio::pin!(wait);
    let (mut local_read, mut local_write) = socket.into_split();
    let mut buffer = [0u8; COPY_BUFFER_BYTES];
    let mut local_input_open = true;
    let mut remote_output_open = true;
    let end = loop {
        tokio::select! {
            read = local_read.read(&mut buffer), if local_input_open => match read {
                Ok(0) => {
                    local_input_open = false;
                    let writer = input.take().expect("an open relay input owns its writer");
                    tokio::select! {
                        closed = writer.close() => {
                            if closed.is_err() {
                                break EpochEnd::Failed(RelayEpochFailure::TransportInput);
                            }
                        }
                        _ = cancelled(cancel) => break EpochEnd::Cancelled,
                    }
                }
                Ok(count) => {
                    let writer = input.as_mut().expect("an open relay input owns its writer");
                    tokio::select! {
                        written = writer.write(&buffer[..count]) => {
                            if written.is_err() {
                                break EpochEnd::Failed(RelayEpochFailure::TransportInput);
                            }
                        }
                        _ = cancelled(cancel) => break EpochEnd::Cancelled,
                    }
                    tokio::select! {
                        flushed = writer.flush() => {
                            if flushed.is_err() {
                                break EpochEnd::Failed(RelayEpochFailure::TransportInput);
                            }
                        }
                        _ = cancelled(cancel) => break EpochEnd::Cancelled,
                    }
                }
                Err(_) => break EpochEnd::Failed(RelayEpochFailure::LocalIo),
            },
            event = output.next(), if remote_output_open => match event {
                Ok(ProcessByteEvent::Chunk { bytes, .. }) => {
                    tokio::select! {
                        written = local_write.write_all(bytes.as_ref()) => {
                            if written.is_err() {
                                break EpochEnd::LocalDisconnected;
                            }
                        }
                        _ = cancelled(cancel) => break EpochEnd::Cancelled,
                    }
                    tokio::select! {
                        flushed = local_write.flush() => {
                            if flushed.is_err() {
                                break EpochEnd::LocalDisconnected;
                            }
                        }
                        _ = cancelled(cancel) => break EpochEnd::Cancelled,
                    }
                }
                Ok(ProcessByteEvent::End) => {
                    remote_output_open = false;
                    let _ = local_write.shutdown().await;
                }
                Err(_) => break EpochEnd::Failed(RelayEpochFailure::TransportOutput),
            },
            report = &mut wait => match report {
                Ok(report) => break EpochEnd::Exited(report.leader_exit),
                Err(failure) => break EpochEnd::Failed(
                    RelayEpochFailure::Supervision(failure.failure),
                ),
            },
            _ = cancelled(cancel) => break EpochEnd::Cancelled,
        }
    };

    let kind = match end {
        EpochEnd::Exited(exit) => return RelayEpochOutcomeKind::TransportExited { exit },
        EpochEnd::LocalDisconnected => RelayEpochOutcomeKind::LocalDisconnected,
        EpochEnd::Cancelled => RelayEpochOutcomeKind::Cancelled,
        EpochEnd::Failed(failure) => RelayEpochOutcomeKind::Failed(failure),
    };

    let _ = control.cancel().await;
    drop(input.take());
    let mut reaped = false;
    while remote_output_open && !reaped {
        tokio::select! {
            event = output.next() => {
                remote_output_open = matches!(event, Ok(ProcessByteEvent::Chunk { .. }));
            }
            _ = &mut wait => reaped = true,
        }
    }
    if !reaped {
        let _ = wait.await;
    }
    kind
}

enum EpochEnd {
    Exited(crate::framework::process::LeaderExitObservation),
    LocalDisconnected,
    Cancelled,
    Failed(RelayEpochFailure),
}

async fn reap_misconfigured(
    session: ProcessSession,
    input: ProcessInputHandle,
    output: ProcessOutputHandle,
) -> RelayEpochOutcomeKind {
    let control = session.control();
    let _ = control.cancel().await;
    if let ProcessInputHandle::Writable(input) = input {
        let _ = input.close().await;
    }
    let wait = session.wait();
    tokio::pin!(wait);
    if let ProcessOutputHandle::Stream(mut output) = output {
        loop {
            tokio::select! {
                event = output.next() => {
                    if !matches!(event, Ok(ProcessByteEvent::Chunk { .. })) {
                        let _ = wait.await;
                        break;
                    }
                }
                _ = &mut wait => break,
            }
        }
    } else {
        let _ = wait.await;
    }
    RelayEpochOutcomeKind::Failed(RelayEpochFailure::Start)
}

fn ssh_spec(
    program: OsString,
    arguments: Vec<OsString>,
    working_directory: &Path,
) -> Result<ProcessSpec, SshRelayError> {
    let environment =
        ProcessEnvironment::new(EnvironmentBase::Inherit, BTreeMap::new(), BTreeSet::new())
            .map_err(|error| SshRelayError::Prepare(error.to_string()))?;
    let command = CommandSpec::new(
        program,
        arguments,
        working_directory.to_owned(),
        environment,
        ProcessLabel::new("connect Console relay".to_owned())
            .map_err(|error| SshRelayError::Prepare(error.to_string()))?,
    )
    .map_err(|error| SshRelayError::Prepare(error.to_string()))?;
    Ok(ProcessSpec::new(
        command,
        InputPolicy::Writable,
        OutputPolicy::Stream(StreamPolicy::new(STREAM_BUDGET)),
        OutputPolicy::Capture(CapturePolicy::new(
            STREAM_BUDGET,
            CaptureOverflow::TruncateWithEvidence,
        )),
        ContainmentRequirement::ExplicitProcessGroup,
        ProcessDeadline::Unlimited,
        TerminationPolicy::new(TERMINATION_GRACE),
    ))
}

async fn cancelled(receiver: &mut watch::Receiver<bool>) {
    loop {
        if *receiver.borrow() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        os::unix::fs::{MetadataExt, PermissionsExt},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    use anyhow::Result;
    use async_trait::async_trait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{PreparedRelayEpoch, RelayEpochFailure, RelayEpochProvider, SshRelay};
    use crate::framework::process::ProcessSupervisor;

    struct StaticTarget {
        preparations: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl RelayEpochProvider for StaticTarget {
        async fn prepare(&self) -> Result<PreparedRelayEpoch, RelayEpochFailure> {
            self.preparations.fetch_add(1, Ordering::AcqRel);
            Ok(PreparedRelayEpoch::new(Vec::<OsString>::new()))
        }
    }

    #[tokio::test]
    async fn stable_listener_relays_two_joined_bounded_epochs() -> Result<()> {
        let suffix = uuid::Uuid::new_v4().as_u128() as u32;
        let root = std::env::temp_dir().join(format!("kcr-{suffix:x}"));
        let state = root.join("state");
        std::fs::create_dir_all(&state)?;
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700))?;
        let fake_ssh = root.join("ssh");
        std::fs::write(&fake_ssh, "#!/bin/sh\ncat\n")?;
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o700))?;
        let processes = ProcessSupervisor::for_test(state)?;
        let preparations = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn RelayEpochProvider> =
            Arc::new(StaticTarget { preparations: Arc::clone(&preparations) });
        let relay =
            SshRelay::start_with_program(&processes, provider, fake_ssh.into_os_string()).await?;
        let socket_path = relay.socket_path().to_owned();
        assert!(socket_path.as_os_str().len() < 104);
        assert_eq!(std::fs::symlink_metadata(socket_path.parent().unwrap())?.mode() & 0o777, 0o700);
        assert_eq!(std::fs::symlink_metadata(&socket_path)?.mode() & 0o777, 0o600);

        for message in [b"first epoch".as_slice(), b"second epoch".as_slice()] {
            let mut client = tokio::net::UnixStream::connect(relay.socket_path()).await?;
            client.write_all(message).await?;
            client.shutdown().await?;
            let mut echoed = Vec::new();
            client.read_to_end(&mut echoed).await?;
            assert_eq!(echoed, message);
        }

        relay.shutdown().await?;
        assert!(!socket_path.exists());
        assert_eq!(preparations.load(Ordering::Acquire), 2);
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }
}
