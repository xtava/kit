use std::{
    future::Future,
    num::NonZeroUsize,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
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
    CaptureOverflow, CapturePolicy, CommandSpec, ContainmentRequirement, InputPolicy,
    LeaderExitObservation, OutputPolicy, ProcessByteEvent, ProcessByteStream, ProcessDeadline,
    ProcessFailureKind, ProcessFailureReport, ProcessInputHandle, ProcessInputWriter,
    ProcessOutputHandle, ProcessReport, ProcessSession, ProcessSpec, ProcessSupervisor,
    StreamPolicy, TerminationPolicy,
};

use super::protocol::{ClientRequest, GatewayErrorCode, ServerResponse, MAX_RESPONSE_LINE_BYTES};

const COPY_BUFFER_BYTES: usize = 32 * 1024;
const STREAM_BUDGET: NonZeroUsize = NonZeroUsize::new(256 * 1024).unwrap();
const TERMINATION_GRACE: Duration = Duration::from_secs(3);
pub(super) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RelayEpochOutcome {
    pub(crate) epoch: u64,
    pub(crate) kind: RelayEpochOutcomeKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RelayEpochOutcomeKind {
    Ready { build: wezterm_codec::BuildIdentity },
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
    GatewayDenied,
    GatewayUnavailable,
    Protocol,
    Supervision(ProcessFailureKind),
}

#[derive(Debug, Error)]
pub(crate) enum TailnetRelayError {
    #[error("the initial Console relay preflight failed")]
    InitialPreflight,
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
    command: CommandSpec,
}

impl PreparedRelayEpoch {
    pub(crate) fn new(command: CommandSpec) -> Self {
        Self { command }
    }
}

/// One stable, private relay listener whose owner serializes tailnet transport epochs.
pub(crate) struct TailnetRelay {
    socket_path: PathBuf,
    outcomes: watch::Receiver<Option<RelayEpochOutcome>>,
    gateway_build: watch::Receiver<Option<wezterm_codec::BuildIdentity>>,
    cancel: watch::Sender<bool>,
    owner: Option<JoinHandle<Result<(), TailnetRelayError>>>,
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
}

impl Drop for RelaySocketLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl TailnetRelay {
    pub(crate) async fn start(
        processes: &ProcessSupervisor,
        provider: Arc<dyn RelayEpochProvider>,
    ) -> Result<Self, TailnetRelayError> {
        let initial_epoch =
            provider.prepare().await.map_err(|_| TailnetRelayError::InitialPreflight)?;

        let listener = bind_relay_socket()?;
        let socket_path = listener.socket.path.clone();

        let (cancel, cancel_receiver) = watch::channel(false);
        let (outcome_sender, outcomes) = watch::channel(None);
        let (gateway_build_sender, gateway_build) = watch::channel(None);
        let process_owner = processes.clone();
        let owner = tokio::spawn(async move {
            let owner = RelayOwner { processes: process_owner, provider, initial_epoch };
            run_owner(listener, owner, cancel_receiver, outcome_sender, gateway_build_sender).await
        });

        Ok(Self { socket_path, outcomes, gateway_build, cancel, owner: Some(owner) })
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub(crate) fn outcome_receiver(&self) -> watch::Receiver<Option<RelayEpochOutcome>> {
        self.outcomes.clone()
    }

    pub(crate) fn gateway_build(&self) -> Option<wezterm_codec::BuildIdentity> {
        self.gateway_build.borrow().clone()
    }

    pub(crate) async fn shutdown(mut self) -> Result<(), TailnetRelayError> {
        let _ = self.cancel.send(true);
        let owner = self.owner.take().expect("a live relay owns one owner task");
        let result = owner.await.map_err(|error| TailnetRelayError::Owner(error.to_string()))?;
        let _last_outcome = self.outcomes.borrow_and_update().clone();
        result
    }
}

impl Drop for TailnetRelay {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
    }
}

async fn run_owner(
    relay: RelayListener,
    owner: RelayOwner,
    mut cancel: watch::Receiver<bool>,
    outcomes: watch::Sender<Option<RelayEpochOutcome>>,
    gateway_build: watch::Sender<Option<wezterm_codec::BuildIdentity>>,
) -> Result<(), TailnetRelayError> {
    let RelayOwner { processes, provider, initial_epoch } = owner;
    let RelayListener { listener, socket } = relay;
    let _socket = socket;
    let mut epoch = 0u64;
    let mut initial_epoch = Some(initial_epoch);
    loop {
        let socket = tokio::select! {
            accepted = listener.accept() => accepted.map_err(TailnetRelayError::Accept)?.0,
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
            prepared.command,
            epoch,
            &outcomes,
            &gateway_build,
            &mut cancel,
        )
        .await;
        let cancelled = matches!(&kind, RelayEpochOutcomeKind::Cancelled);
        outcomes.send_replace(Some(RelayEpochOutcome { epoch, kind }));
        if cancelled {
            return Ok(());
        }
    }
}

fn bind_relay_socket() -> Result<RelayListener, TailnetRelayError> {
    let runtime_dir = super::super::runtime::directory()
        .map_err(|error| TailnetRelayError::Runtime(error.to_string()))?;
    super::super::runtime::prepare(&runtime_dir)
        .map_err(|error| TailnetRelayError::Runtime(error.to_string()))?;
    let socket_path = runtime_dir.join(format!("r-{}.sock", uuid::Uuid::new_v4().simple()));
    super::super::runtime::validate_socket_path(&socket_path)
        .map_err(|error| TailnetRelayError::Runtime(error.to_string()))?;
    let socket = RelaySocketLease { path: socket_path.clone() };
    let listener = UnixListener::bind(&socket_path)
        .map_err(|source| TailnetRelayError::Bind { path: socket_path.clone(), source })?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        .map_err(|source| TailnetRelayError::Protect { path: socket_path, source })?;
    Ok(RelayListener { listener, socket })
}

async fn run_epoch(
    processes: &ProcessSupervisor,
    socket: UnixStream,
    command: CommandSpec,
    epoch: u64,
    outcomes: &watch::Sender<Option<RelayEpochOutcome>>,
    gateway_build: &watch::Sender<Option<wezterm_codec::BuildIdentity>>,
    cancel: &mut watch::Receiver<bool>,
) -> RelayEpochOutcomeKind {
    gateway_build.send_replace(None);
    let spec = tailnet_spec(command);
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
    let handshake = tokio::select! {
        result = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            gateway_handshake(
                input.as_mut().expect("a configured relay owns a writable transport input"),
                &mut output,
            ),
        ) => match result {
            Ok(result) => result,
            Err(_) => Err(RelayEpochFailure::GatewayUnavailable),
        },
        _ = cancelled(cancel) => {
            let _ = control.cancel().await;
            drop(input.take());
            drain_and_reap(&mut output, wait.as_mut()).await;
            return RelayEpochOutcomeKind::Cancelled;
        }
    };
    let handshake = match handshake {
        Ok(handshake) => handshake,
        Err(failure) => {
            let _ = control.cancel().await;
            drop(input.take());
            drain_and_reap(&mut output, wait.as_mut()).await;
            return RelayEpochOutcomeKind::Failed(failure);
        }
    };
    gateway_build.send_replace(Some(handshake.build.clone()));
    outcomes.send_replace(Some(RelayEpochOutcome {
        epoch,
        kind: RelayEpochOutcomeKind::Ready { build: handshake.build },
    }));
    let (mut local_read, mut local_write) = socket.into_split();
    if !handshake.buffered_remote.is_empty()
        && local_write.write_all(&handshake.buffered_remote).await.is_err()
    {
        let _ = control.cancel().await;
        drop(input.take());
        drain_and_reap(&mut output, wait.as_mut()).await;
        return RelayEpochOutcomeKind::LocalDisconnected;
    }
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
                Ok(report) => {
                    let exit = report.leader_exit;
                    match drain_remote_to_eof(&mut output, &mut local_write, cancel).await {
                        Ok(()) => {
                            let _ = local_write.shutdown().await;
                            break EpochEnd::Exited(exit);
                        }
                        Err(EpochEnd::LocalDisconnected) => {
                            return RelayEpochOutcomeKind::LocalDisconnected;
                        }
                        Err(EpochEnd::Cancelled) => return RelayEpochOutcomeKind::Cancelled,
                        Err(EpochEnd::Failed(failure)) => {
                            return RelayEpochOutcomeKind::Failed(failure);
                        }
                        Err(EpochEnd::Exited(_)) => {
                            unreachable!("draining a completed transport cannot reap it twice")
                        }
                    }
                }
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

async fn drain_remote_to_eof(
    output: &mut ProcessByteStream,
    local_write: &mut tokio::net::unix::OwnedWriteHalf,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), EpochEnd> {
    loop {
        let event = tokio::select! {
            event = output.next() => event,
            _ = cancelled(cancel) => return Err(EpochEnd::Cancelled),
        };
        match event {
            Ok(ProcessByteEvent::Chunk { bytes, .. }) => {
                tokio::select! {
                    written = local_write.write_all(bytes.as_ref()) => {
                        if written.is_err() {
                            return Err(EpochEnd::LocalDisconnected);
                        }
                    }
                    _ = cancelled(cancel) => return Err(EpochEnd::Cancelled),
                }
                tokio::select! {
                    flushed = local_write.flush() => {
                        if flushed.is_err() {
                            return Err(EpochEnd::LocalDisconnected);
                        }
                    }
                    _ = cancelled(cancel) => return Err(EpochEnd::Cancelled),
                }
            }
            Ok(ProcessByteEvent::End) => return Ok(()),
            Err(_) => return Err(EpochEnd::Failed(RelayEpochFailure::TransportOutput)),
        }
    }
}

async fn gateway_handshake(
    input: &mut ProcessInputWriter,
    output: &mut ProcessByteStream,
) -> Result<GatewayHandshake, RelayEpochFailure> {
    input
        .write(&ClientRequest::Connect.encode())
        .await
        .map_err(|_| RelayEpochFailure::TransportInput)?;
    input.flush().await.map_err(|_| RelayEpochFailure::TransportInput)?;

    let mut line = Vec::new();
    loop {
        match output.next().await {
            Ok(ProcessByteEvent::Chunk { bytes, .. }) => {
                let newline = bytes.iter().position(|byte| *byte == b'\n');
                let line_bytes = newline.map_or(bytes.len(), |index| index + 1);
                if line.len().saturating_add(line_bytes) > MAX_RESPONSE_LINE_BYTES {
                    return Err(RelayEpochFailure::Protocol);
                }
                line.extend_from_slice(&bytes[..line_bytes]);
                let Some(newline) = newline else {
                    continue;
                };
                return match ServerResponse::parse_line(&line) {
                    Ok(ServerResponse::Ready { build }) if build.product == "kit-console" => {
                        Ok(GatewayHandshake {
                            build,
                            buffered_remote: bytes[newline + 1..].to_vec(),
                        })
                    }
                    Ok(ServerResponse::Ready { .. }) => Err(RelayEpochFailure::Protocol),
                    Ok(ServerResponse::Error(GatewayErrorCode::Auth)) => {
                        Err(RelayEpochFailure::GatewayDenied)
                    }
                    Ok(ServerResponse::Error(GatewayErrorCode::Unavailable)) => {
                        Err(RelayEpochFailure::GatewayUnavailable)
                    }
                    Ok(ServerResponse::Error(GatewayErrorCode::Protocol)) | Err(_) => {
                        Err(RelayEpochFailure::Protocol)
                    }
                };
            }
            Ok(ProcessByteEvent::End) | Err(_) => return Err(RelayEpochFailure::TransportOutput),
        }
    }
}

struct GatewayHandshake {
    build: wezterm_codec::BuildIdentity,
    buffered_remote: Vec<u8>,
}

async fn drain_and_reap(
    output: &mut ProcessByteStream,
    mut wait: Pin<
        &mut (dyn Future<Output = std::result::Result<ProcessReport, ProcessFailureReport>> + Send),
    >,
) {
    loop {
        tokio::select! {
            event = output.next() => {
                if !matches!(event, Ok(ProcessByteEvent::Chunk { .. })) {
                    let _ = wait.as_mut().await;
                    return;
                }
            }
            _ = wait.as_mut() => return,
        }
    }
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

fn tailnet_spec(command: CommandSpec) -> ProcessSpec {
    ProcessSpec::new(
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
    )
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
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    use anyhow::Result;
    use async_trait::async_trait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wezterm_codec::BuildIdentity;

    use super::{PreparedRelayEpoch, RelayEpochFailure, RelayEpochProvider, TailnetRelay};
    use crate::{
        framework::process::{
            CommandSpec, EnvironmentBase, ProcessEnvironment, ProcessLabel, ProcessSupervisor,
        },
        tools::console::transport::protocol::ServerResponse,
    };

    struct StaticTarget {
        preparations: Arc<AtomicUsize>,
        program: PathBuf,
        working_directory: PathBuf,
    }

    #[async_trait]
    impl RelayEpochProvider for StaticTarget {
        async fn prepare(&self) -> Result<PreparedRelayEpoch, RelayEpochFailure> {
            self.preparations.fetch_add(1, Ordering::AcqRel);
            let environment = ProcessEnvironment::new(
                EnvironmentBase::Inherit,
                Default::default(),
                Default::default(),
            )
            .map_err(|_| RelayEpochFailure::Start)?;
            let command = CommandSpec::new(
                self.program.clone().into_os_string(),
                Vec::new(),
                self.working_directory.clone(),
                environment,
                ProcessLabel::new("test Console tailnet relay".to_owned())
                    .map_err(|_| RelayEpochFailure::Start)?,
            )
            .map_err(|_| RelayEpochFailure::Start)?;
            Ok(PreparedRelayEpoch::new(command))
        }
    }

    #[tokio::test]
    async fn stable_listener_relays_two_joined_bounded_epochs() -> Result<()> {
        let suffix = uuid::Uuid::new_v4().as_u128() as u32;
        let root = std::env::temp_dir().join(format!("kcr-{suffix:x}"));
        let state = root.join("state");
        std::fs::create_dir_all(&state)?;
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700))?;
        let ready = ServerResponse::Ready {
            build: BuildIdentity {
                product: "kit-console".to_owned(),
                version: "test".to_owned(),
                source_revision: Some("a".repeat(40)),
                source_dirty: Some(false),
                embedded_wezterm_revision: Some("b".repeat(40)),
            },
        }
        .encode()?;
        let ready = String::from_utf8(ready)?;
        assert!(!ready.contains('\''));
        let fake_tailnet = root.join("tailscale-nc");
        std::fs::write(
            &fake_tailnet,
            format!("#!/bin/sh\nIFS= read -r _hello\nprintf '%s' '{ready}'\ncat\n"),
        )?;
        std::fs::set_permissions(&fake_tailnet, std::fs::Permissions::from_mode(0o700))?;
        let processes = ProcessSupervisor::for_test(state)?;
        let preparations = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn RelayEpochProvider> = Arc::new(StaticTarget {
            preparations: Arc::clone(&preparations),
            program: fake_tailnet,
            working_directory: root.clone(),
        });
        let relay = TailnetRelay::start(&processes, provider).await?;
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
        assert_eq!(
            relay.gateway_build().as_ref().map(|build| build.product.as_str()),
            Some("kit-console")
        );

        relay.shutdown().await?;
        assert!(!socket_path.exists());
        assert_eq!(preparations.load(Ordering::Acquire), 2);
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[tokio::test]
    async fn transport_exit_drains_final_remote_bytes_before_local_eof() -> Result<()> {
        let suffix = uuid::Uuid::new_v4().as_u128() as u32;
        let root = std::env::temp_dir().join(format!("kcr-exit-{suffix:x}"));
        let state = root.join("state");
        std::fs::create_dir_all(&state)?;
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700))?;
        let ready = ServerResponse::Ready {
            build: BuildIdentity {
                product: "kit-console".to_owned(),
                version: "test".to_owned(),
                source_revision: Some("a".repeat(40)),
                source_dirty: Some(false),
                embedded_wezterm_revision: Some("b".repeat(40)),
            },
        }
        .encode()?;
        let ready = String::from_utf8(ready)?;
        let fake_tailnet = root.join("tailscale-nc");
        std::fs::write(
            &fake_tailnet,
            format!(
                "#!/bin/sh\nIFS= read -r _hello\nprintf '%s' '{ready}'\nsleep 0.05\nprintf 'final remote bytes'\n"
            ),
        )?;
        std::fs::set_permissions(&fake_tailnet, std::fs::Permissions::from_mode(0o700))?;
        let processes = ProcessSupervisor::for_test(state)?;
        let provider: Arc<dyn RelayEpochProvider> = Arc::new(StaticTarget {
            preparations: Arc::new(AtomicUsize::new(0)),
            program: fake_tailnet,
            working_directory: root.clone(),
        });
        let relay = TailnetRelay::start(&processes, provider).await?;
        let mut client = tokio::net::UnixStream::connect(relay.socket_path()).await?;
        let mut received = Vec::new();
        client.read_to_end(&mut received).await?;

        assert_eq!(received, b"final remote bytes");
        relay.shutdown().await?;
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }
}
