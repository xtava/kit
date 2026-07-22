use crate::domain::{ClientDomain, ClientDomainConfig};
use crate::pane::ClientPane;
use anyhow::{anyhow, bail, Context};
use async_ossl::AsyncSslStream;
use async_trait::async_trait;
use codec::*;
use config::{configuration, SshDomain, TlsDomainClient, UnixDomain, UnixTarget};
use filedescriptor::FileDescriptor;
use futures::FutureExt;
use mux::client::ClientId;
use mux::connui::{ConnectionUI, ConnectionUi};
use mux::domain::DomainId;
use mux::pane::PaneId;
use mux::ssh::ssh_connect_with_ui;
use mux::Mux;
use openssl::ssl::{SslConnector, SslFiletype, SslMethod};
use openssl::x509::X509;
use portable_pty::Child;
use smol::channel::{bounded, Receiver, Sender};
use smol::prelude::*;
use smol::{block_on, Async};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::marker::Unpin;
use std::net::TcpStream;
use std::num::NonZeroU32;
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, AsSocket, BorrowedSocket, RawSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use thiserror::Error;
use wezterm_runtime_admission::{AdmissionError, CountClass, CountPermit, RuntimeAdmission};
use wezterm_uds::UnixStream;

#[derive(Error, Debug)]
#[error("Timeout")]
struct Timeout;

#[derive(Error, Debug)]
#[error("ChannelSendError")]
struct ChannelSendError;

enum ReaderMessage {
    SendPdu {
        pdu: Box<Pdu>,
        expected_response: PduTag,
        promise: Sender<anyhow::Result<AdmittedRpcResponse<Pdu>>>,
        _permit: CountPermit,
    },
    Readable,
}

struct PreparedClientRequest {
    sender: Sender<ReaderMessage>,
    message: ReaderMessage,
    response: Receiver<anyhow::Result<AdmittedRpcResponse<Pdu>>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaneControlStatus {
    AwaitingSnapshot,
    Uncontrolled,
    Controller,
    Observer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlReduction {
    NotControl,
    Applied,
    Discarded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SequenceReduction {
    Applied,
    Discarded,
}

#[derive(Debug, Error, Eq, PartialEq)]
enum ControlTrackingError {
    #[error("received a control change before its initial snapshot")]
    ChangeBeforeSnapshot,
    #[error("control sequence gap: expected {expected}, received {actual}")]
    SequenceGap { expected: u64, actual: u64 },
    #[error("control sequence overflow after {current}, received {actual}")]
    SequenceOverflow { current: u64, actual: u64 },
}

#[derive(Default)]
struct AttachmentControlTracker {
    snapshot: Mutex<Option<ControlSnapshot>>,
}

impl AttachmentControlTracker {
    fn begin_connection(&self) {
        *self.snapshot.lock().unwrap() = None;
    }

    fn pane_status(&self, pane_id: PaneId) -> PaneControlStatus {
        let snapshot = self.snapshot.lock().unwrap();
        let Some(snapshot) = snapshot.as_ref() else {
            return PaneControlStatus::AwaitingSnapshot;
        };
        match snapshot
            .state
            .active
            .iter()
            .find(|lease| lease.pane_id == pane_id)
        {
            None => PaneControlStatus::Uncontrolled,
            Some(lease) if lease.controller == snapshot.attachment_identity => {
                PaneControlStatus::Controller
            }
            Some(_) => PaneControlStatus::Observer,
        }
    }

    fn reduce(&self, pdu: &Pdu) -> Result<ControlReduction, ControlTrackingError> {
        match pdu {
            Pdu::ControlSnapshot(snapshot) => self.reduce_snapshot(snapshot.clone()),
            Pdu::ControlChanged(changed) => self.reduce_change(changed),
            _ => Ok(ControlReduction::NotControl),
        }
    }

    fn reduce_snapshot(
        &self,
        incoming: ControlSnapshot,
    ) -> Result<ControlReduction, ControlTrackingError> {
        let mut current = self.snapshot.lock().unwrap();
        let Some(snapshot) = current.as_ref() else {
            *current = Some(incoming);
            return Ok(ControlReduction::Applied);
        };
        match next_control_sequence(snapshot.state.sequence, incoming.state.sequence)? {
            SequenceReduction::Discarded => Ok(ControlReduction::Discarded),
            SequenceReduction::Applied => {
                *current = Some(incoming);
                Ok(ControlReduction::Applied)
            }
        }
    }

    fn reduce_change(
        &self,
        incoming: &ControlChanged,
    ) -> Result<ControlReduction, ControlTrackingError> {
        let mut current = self.snapshot.lock().unwrap();
        let snapshot = current
            .as_mut()
            .ok_or(ControlTrackingError::ChangeBeforeSnapshot)?;
        match next_control_sequence(snapshot.state.sequence, incoming.state.sequence)? {
            SequenceReduction::Discarded => Ok(ControlReduction::Discarded),
            SequenceReduction::Applied => {
                snapshot.state = incoming.state.clone();
                Ok(ControlReduction::Applied)
            }
        }
    }

    fn reduce_authoritative_state(
        &self,
        incoming: ControlLeaseState,
    ) -> Result<ControlReduction, ControlTrackingError> {
        let mut current = self.snapshot.lock().unwrap();
        let snapshot = current
            .as_mut()
            .ok_or(ControlTrackingError::ChangeBeforeSnapshot)?;
        if incoming.sequence <= snapshot.state.sequence {
            return Ok(ControlReduction::Discarded);
        }
        snapshot.state = incoming;
        Ok(ControlReduction::Applied)
    }
}

fn next_control_sequence(
    current: u64,
    incoming: u64,
) -> Result<SequenceReduction, ControlTrackingError> {
    if current == u64::MAX {
        return if incoming == current {
            Ok(SequenceReduction::Discarded)
        } else {
            Err(ControlTrackingError::SequenceOverflow {
                current,
                actual: incoming,
            })
        };
    }
    if incoming <= current {
        return Ok(SequenceReduction::Discarded);
    }
    let expected = current + 1;
    if incoming != expected {
        return Err(ControlTrackingError::SequenceGap {
            expected,
            actual: incoming,
        });
    }
    Ok(SequenceReduction::Applied)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeadlessConnectionFailure {
    Transport,
    PromptRequired,
    LifecycleSaturated,
    LifecycleClosed,
    Runtime,
    RetryExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeadlessConnectionState {
    Attaching,
    Reconnecting { attempt: u32 },
    Ready,
    Failed(HeadlessConnectionFailure),
    Detached,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientRuntimeOutcome {
    Completed,
    Cancelled,
    Detached,
    Failed(HeadlessConnectionFailure),
    Panicked,
}

#[derive(Debug, Error)]
pub enum HeadlessLifecycleError {
    #[error("connection lifecycle admission failed")]
    Admission(#[source] AdmissionError),
    #[error("connection lifecycle queue is saturated")]
    Saturated,
    #[error("connection lifecycle receiver is closed")]
    Closed,
    #[error("connection lifecycle queue is empty")]
    Empty,
}

struct AdmittedLifecycleState {
    state: HeadlessConnectionState,
    _permit: CountPermit,
}

#[derive(Clone)]
pub(crate) struct HeadlessLifecycleReporter {
    admission: Arc<RuntimeAdmission>,
    sender: Sender<AdmittedLifecycleState>,
    terminal: Arc<AtomicBool>,
    reconnect_attempt_limit: Option<NonZeroU32>,
}

impl HeadlessLifecycleReporter {
    fn publish(&self, state: HeadlessConnectionState) -> Result<(), HeadlessLifecycleError> {
        let is_terminal = matches!(
            state,
            HeadlessConnectionState::Failed(_) | HeadlessConnectionState::Detached
        );
        if is_terminal {
            if self.terminal.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
        } else if self.terminal.load(Ordering::Acquire) {
            return Err(HeadlessLifecycleError::Closed);
        }
        let permit = self
            .admission
            .try_count(CountClass::LifecycleEvent, 1)
            .map_err(|error| match error {
                AdmissionError::CapacityExceeded { .. } => HeadlessLifecycleError::Saturated,
                error => HeadlessLifecycleError::Admission(error),
            })?;
        self.sender
            .try_send(AdmittedLifecycleState {
                state,
                _permit: permit,
            })
            .map_err(|error| {
                if error.is_full() {
                    HeadlessLifecycleError::Saturated
                } else {
                    HeadlessLifecycleError::Closed
                }
            })
    }
}

#[derive(Debug, Error)]
#[error("headless connection requires interactive input")]
struct HeadlessPromptRequired;

fn run_and_report_connection_error<T, F>(ui: &dyn ConnectionUi, operation: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T>,
{
    match operation() {
        Ok(value) => Ok(value),
        Err(error) => {
            ui.report_error(&error);
            Err(error)
        }
    }
}

impl ConnectionUi for HeadlessLifecycleReporter {
    fn output(&self, _changes: Vec<termwiz::surface::Change>) {}

    fn input(&self, _prompt: &str) -> anyhow::Result<String> {
        Err(HeadlessPromptRequired.into())
    }

    fn password(&self, _prompt: &str) -> anyhow::Result<String> {
        Err(HeadlessPromptRequired.into())
    }

    fn report_error(&self, _error: &anyhow::Error) {}
}

pub struct HeadlessConnectionLifecycle {
    reporter: HeadlessLifecycleReporter,
    receiver: Receiver<AdmittedLifecycleState>,
}

impl HeadlessConnectionLifecycle {
    pub fn new(admission: Arc<RuntimeAdmission>) -> Self {
        Self::with_reconnect_attempt_limit(admission, None)
    }

    pub fn with_reconnect_attempt_limit(
        admission: Arc<RuntimeAdmission>,
        reconnect_attempt_limit: Option<NonZeroU32>,
    ) -> Self {
        let (sender, receiver) = bounded(wezterm_runtime_admission::MAX_LIFECYCLE_EVENTS);
        Self {
            reporter: HeadlessLifecycleReporter {
                admission,
                sender,
                terminal: Arc::new(AtomicBool::new(false)),
                reconnect_attempt_limit,
            },
            receiver,
        }
    }

    pub fn admission(&self) -> &Arc<RuntimeAdmission> {
        &self.reporter.admission
    }

    pub fn uses_admission(&self, admission: &Arc<RuntimeAdmission>) -> bool {
        Arc::ptr_eq(self.admission(), admission)
    }

    pub async fn recv(&self) -> Result<HeadlessConnectionState, HeadlessLifecycleError> {
        self.receiver
            .recv()
            .await
            .map(|event| event.state)
            .map_err(|_| HeadlessLifecycleError::Closed)
    }

    pub fn try_recv(&self) -> Result<HeadlessConnectionState, HeadlessLifecycleError> {
        self.receiver
            .try_recv()
            .map(|event| event.state)
            .map_err(|error| {
                if error.is_empty() {
                    HeadlessLifecycleError::Empty
                } else {
                    HeadlessLifecycleError::Closed
                }
            })
    }

    pub(crate) fn reporter(&self) -> HeadlessLifecycleReporter {
        self.reporter.clone()
    }

    pub(crate) fn publish_attaching(&self) -> Result<(), HeadlessLifecycleError> {
        self.reporter.publish(HeadlessConnectionState::Attaching)
    }

    pub(crate) fn publish_ready(&self) -> Result<(), HeadlessLifecycleError> {
        self.reporter.publish(HeadlessConnectionState::Ready)
    }

    pub(crate) fn publish_failed(
        &self,
        failure: HeadlessConnectionFailure,
    ) -> Result<(), HeadlessLifecycleError> {
        self.reporter
            .publish(HeadlessConnectionState::Failed(failure))
    }
}

#[derive(Clone)]
enum ConnectionPresentation {
    Interactive(ConnectionUI),
    Headless(HeadlessLifecycleReporter),
}

impl ConnectionPresentation {
    fn publish(&self, state: HeadlessConnectionState) -> Result<(), HeadlessLifecycleError> {
        match self {
            Self::Interactive(_) => Ok(()),
            Self::Headless(reporter) => reporter.publish(state),
        }
    }

    fn is_headless(&self) -> bool {
        matches!(self, Self::Headless(_))
    }

    fn reconnect_attempt_limit(&self) -> Option<NonZeroU32> {
        match self {
            Self::Interactive(_) => None,
            Self::Headless(reporter) => reporter.reconnect_attempt_limit,
        }
    }

    fn for_reconnect(&self) -> Self {
        match self {
            Self::Interactive(_) => Self::Interactive(ConnectionUI::new()),
            Self::Headless(reporter) => Self::Headless(reporter.clone()),
        }
    }

    fn close(&self) {
        if let Self::Interactive(ui) = self {
            ui.close();
        }
    }
}

impl ConnectionUi for ConnectionPresentation {
    fn output(&self, changes: Vec<termwiz::surface::Change>) {
        match self {
            Self::Interactive(ui) => ConnectionUi::output(ui, changes),
            Self::Headless(reporter) => ConnectionUi::output(reporter, changes),
        }
    }

    fn input(&self, prompt: &str) -> anyhow::Result<String> {
        match self {
            Self::Interactive(ui) => ConnectionUi::input(ui, prompt),
            Self::Headless(reporter) => ConnectionUi::input(reporter, prompt),
        }
    }

    fn password(&self, prompt: &str) -> anyhow::Result<String> {
        match self {
            Self::Interactive(ui) => ConnectionUi::password(ui, prompt),
            Self::Headless(reporter) => ConnectionUi::password(reporter, prompt),
        }
    }

    fn report_error(&self, error: &anyhow::Error) {
        match self {
            Self::Interactive(ui) => ConnectionUi::report_error(ui, error),
            Self::Headless(reporter) => ConnectionUi::report_error(reporter, error),
        }
    }
}

struct ClientCancellation {
    requested: Arc<AtomicBool>,
    sender: Sender<()>,
}

struct ClientCancelWaiter {
    requested: Arc<AtomicBool>,
    receiver: Receiver<()>,
}

impl ClientCancellation {
    fn pair() -> (Self, ClientCancelWaiter) {
        let requested = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = bounded(1);
        (
            Self {
                requested: Arc::clone(&requested),
                sender,
            },
            ClientCancelWaiter {
                requested,
                receiver,
            },
        )
    }

    fn cancel(&self) {
        self.requested.store(true, Ordering::Release);
        let _ = self.sender.try_send(());
    }
}

impl ClientCancelWaiter {
    fn is_cancelled(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

struct ClientRuntimeState {
    worker: Option<thread::JoinHandle<ClientRuntimeOutcome>>,
    outcome: Option<ClientRuntimeOutcome>,
}

struct ClientRuntime {
    cancellation: ClientCancellation,
    state: Mutex<ClientRuntimeState>,
}

impl ClientRuntime {
    fn spawn<F>(admission: &Arc<RuntimeAdmission>, worker: F) -> anyhow::Result<Arc<Self>>
    where
        F: FnOnce(ClientCancelWaiter) -> ClientRuntimeOutcome + Send + 'static,
    {
        let runnable = admission
            .try_count(CountClass::ExecutorRunnable, 1)
            .context("client runtime runnable admission")?;
        let (cancellation, waiter) = ClientCancellation::pair();
        let worker = thread::Builder::new()
            .name("wezterm-client-runtime".to_string())
            .spawn(move || {
                let _runnable = runnable;
                worker(waiter)
            })
            .context("spawn owned client runtime")?;
        Ok(Arc::new(Self {
            cancellation,
            state: Mutex::new(ClientRuntimeState {
                worker: Some(worker),
                outcome: None,
            }),
        }))
    }

    fn shutdown_and_join(&self) -> ClientRuntimeOutcome {
        self.cancellation.cancel();
        let mut state = self.state.lock().unwrap();
        if let Some(outcome) = state.outcome {
            return outcome;
        }
        let outcome = match state.worker.take() {
            Some(worker) => worker.join().unwrap_or(ClientRuntimeOutcome::Panicked),
            None => ClientRuntimeOutcome::Completed,
        };
        state.outcome = Some(outcome);
        outcome
    }
}

impl Drop for ClientRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown_and_join();
    }
}

impl PreparedClientRequest {
    async fn execute(self) -> anyhow::Result<AdmittedRpcResponse<Pdu>> {
        let Self {
            sender,
            message,
            response,
        } = self;
        sender
            .send(message)
            .await
            .map_err(|_| ChannelSendError)
            .context("send_pdu send")?;
        response.recv().await.context("send_pdu recv")?
    }
}

#[derive(Clone)]
pub struct Client {
    sender: Sender<ReaderMessage>,
    runtime: Arc<ClientRuntime>,
    admission: Arc<RuntimeAdmission>,
    control: Arc<AttachmentControlTracker>,
    presentation: ConnectionPresentation,
    local_domain_id: Option<DomainId>,
    pub client_id: ClientId,
    initial_server_version: Arc<OnceLock<GetCodecVersionResponse>>,
    client_domain_config: ClientDomainConfig,
    pub is_reconnectable: bool,
    pub is_local: bool,
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "Please install the same version of wezterm on both the client and server!\n\
     The server version is {} (codec version {}),\n\
     which is not compatible with our version \n\
     {} (codec version {}).",
    version,
    codec_vers,
    config::wezterm_version(),
    CODEC_VERSION
)]
pub struct IncompatibleVersionError {
    pub version: String,
    pub codec_vers: usize,
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error("server build identity mismatch: expected {expected:?}, received {actual:?}")]
pub struct BuildIdentityMismatch {
    pub expected: BuildIdentity,
    pub actual: BuildIdentity,
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error("server rejected the connection because its attachment capacity is exhausted")]
pub struct AttachmentRejectedError;

macro_rules! rpc {
    ($method_name:ident, $request_type:ident, $response_type:ident) => {
        pub fn $method_name(
            &self,
            pdu: $request_type,
        ) -> impl std::future::Future<Output = anyhow::Result<AdmittedRpcResponse<$response_type>>>
               + Send
               + 'static {
            let start = std::time::Instant::now();
            let request = self.send_pdu(Pdu::$request_type(pdu));
            async move {
                let result = request.await;
                let elapsed = start.elapsed();
                metrics::histogram!("rpc", "method" => stringify!($method_name)).record(elapsed);
                metrics::counter!("rpc.count", "method" => stringify!($method_name)).increment(1);
                match result {
                    Ok(response) => response.try_map(|pdu| match pdu {
                        Pdu::$response_type(res) => Ok(res),
                        unexpected => bail!("unexpected response {unexpected:?}"),
                    }),
                    Err(err) => Err(err),
                }
            }
        }
    };

    // This variant allows omitting the request parameter; this is useful
    // in the case where the struct is empty and present only for the purpose
    // of typing the request.
    ($method_name:ident, $request_type:ident=(), $response_type:ident) => {
        #[allow(dead_code)]
        pub fn $method_name(
            &self,
        ) -> impl std::future::Future<Output = anyhow::Result<AdmittedRpcResponse<$response_type>>>
               + Send
               + 'static {
            let start = std::time::Instant::now();
            let request = self.send_pdu(Pdu::$request_type($request_type {}));
            async move {
                let result = request.await;
                let elapsed = start.elapsed();
                metrics::histogram!("rpc", "method" => stringify!($method_name)).record(elapsed);
                metrics::counter!("rpc.count", "method" => stringify!($method_name)).increment(1);
                match result {
                    Ok(response) => response.try_map(|pdu| match pdu {
                        Pdu::$response_type(res) => Ok(res),
                        unexpected => bail!("unexpected response {unexpected:?}"),
                    }),
                    Err(err) => Err(err),
                }
            }
        }
    };
}

fn spawn_client_invalidation<F>(future: F) -> anyhow::Result<()>
where
    F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    Mux::get().try_spawn_client_invalidation(future)
}

async fn process_unilateral_async(
    local_domain_id: DomainId,
    notification: AdmittedNotification,
) -> anyhow::Result<()> {
    let mux = match Mux::try_get() {
        Some(mux) => mux,
        None => return Ok(()),
    };
    let client_domain = mux
        .get_domain(local_domain_id)
        .ok_or_else(|| anyhow!("no such domain {}", local_domain_id))?;
    let client_domain = client_domain
        .downcast_ref::<ClientDomain>()
        .ok_or_else(|| anyhow!("domain {} is not a ClientDomain instance", local_domain_id))?;

    match notification.pdu() {
        Pdu::WindowWorkspaceChanged(WindowWorkspaceChanged {
            window_id,
            workspace,
        }) => {
            let local_window_id = client_domain
                .remote_to_local_window_id(*window_id)
                .ok_or_else(|| anyhow!("no local window for remote window id {}", window_id))?;
            if let Some(mut window) = mux.get_window_mut(local_window_id) {
                window.set_workspace(workspace);
            }
            return Ok(());
        }
        Pdu::WindowTitleChanged(WindowTitleChanged { window_id, title }) => {
            client_domain.process_remote_window_title_change(*window_id, title.to_string());
            return Ok(());
        }
        Pdu::RenameWorkspace(RenameWorkspace {
            old_workspace,
            new_workspace,
        }) => {
            log::debug!("got a rename {old_workspace} -> {new_workspace}");
            mux.rename_workspace(old_workspace, new_workspace);
            return Ok(());
        }
        Pdu::TabTitleChanged(TabTitleChanged { tab_id, title }) => {
            client_domain.process_remote_tab_title_change(*tab_id, title.to_string());
            return Ok(());
        }
        Pdu::TabResized(_) | Pdu::TabAddedToWindow(_) => {
            log::trace!("resync due to {:?}", notification.pdu());
            return client_domain.resync().await;
        }
        _ => {}
    }

    let pane_id = notification
        .pdu()
        .pane_id()
        .ok_or_else(|| anyhow!("don't know how to handle {notification:?}"))?;

    // If we get a push for a pane that we don't yet know about, another
    // client changed the topology. Resync before resolving the final owner.
    let local_pane_id = match client_domain.remote_to_local_pane_id(pane_id) {
        Some(pane_id) => pane_id,
        None => {
            log::debug!("got {notification:?}, pane not found locally, resync");
            client_domain.resync().await?;
            client_domain
                .remote_to_local_pane_id(pane_id)
                .ok_or_else(|| {
                    anyhow!("remote pane id {} does not have a local pane id", pane_id)
                })?
        }
    };

    let pane = match mux.get_pane(local_pane_id) {
        Some(pane) => pane,
        None => {
            log::debug!(
                "got {notification:?}, but local pane {local_pane_id} no longer exists; resync"
            );
            client_domain.resync().await?;
            let local_pane_id =
                client_domain
                    .remote_to_local_pane_id(pane_id)
                    .ok_or_else(|| {
                        anyhow!("remote pane id {} does not have a local pane id", pane_id)
                    })?;
            mux.get_pane(local_pane_id)
                .ok_or_else(|| anyhow!("local pane {local_pane_id} not found"))?
        }
    };
    let client_pane = pane.downcast_ref::<ClientPane>().ok_or_else(|| {
        anyhow!(
            "received unilateral PDU for pane {} which is not a ClientPane: {:?}",
            local_pane_id,
            notification.pdu()
        )
    })?;
    client_pane.process_unilateral(notification).await
}

fn process_unilateral(
    local_domain_id: Option<DomainId>,
    control: &AttachmentControlTracker,
    notification: AdmittedNotification,
) -> anyhow::Result<()> {
    if control.reduce(notification.pdu())? != ControlReduction::NotControl {
        return Ok(());
    }
    let Some(local_domain_id) = local_domain_id else {
        log::trace!(
            "client doesn't have a real local domain, so unilateral message cannot be processed"
        );
        return Ok(());
    };
    spawn_client_invalidation(process_unilateral_async(local_domain_id, notification))
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
enum NotReconnectableError {
    #[error("Client was destroyed")]
    ClientWasDestroyed,
}

#[derive(Error, Debug)]
#[error("client runtime was cancelled")]
struct ClientCancelled;

fn reserve_client_request(admission: &RuntimeAdmission) -> anyhow::Result<CountPermit> {
    admission
        .try_count(CountClass::ClientRequest, 1)
        .context("client request admission")
}

fn prepare_client_request(
    sender: &Sender<ReaderMessage>,
    admission: &RuntimeAdmission,
    pdu: Pdu,
) -> anyhow::Result<PreparedClientRequest> {
    let expected_response = pdu.expected_response_tag().ok_or_else(|| {
        anyhow!(
            "PDU {} cannot be sent as a correlated client request",
            pdu.pdu_name()
        )
    })?;
    let permit = reserve_client_request(admission)?;
    let (promise, response) = bounded(1);
    Ok(PreparedClientRequest {
        sender: sender.clone(),
        message: ReaderMessage::SendPdu {
            pdu: Box::new(pdu),
            expected_response,
            promise,
            _permit: permit,
        },
        response,
    })
}

struct ClientBootstrap {
    server_version: GetCodecVersionResponse,
    next_serial: u64,
    resume_token: AttachmentResumeToken,
    control_snapshot: ControlSnapshot,
}

async fn read_server_pdu_async<R, F>(
    stream: &mut R,
    expected_response: F,
    admission: &RuntimeAdmission,
) -> anyhow::Result<AdmittedDecodedPdu>
where
    R: Unpin + AsyncRead + std::fmt::Debug,
    F: FnOnce(u64) -> Option<PduTag>,
{
    let header = Pdu::read_header_async(stream).await?;
    let context = if header.serial() == 0 {
        DecodeContext::server_to_client_notification()
    } else {
        DecodeContext::server_to_client_response(expected_response(header.serial()))
    };
    let body = header.validate(context, admission)?;
    Pdu::decode_body_async(stream, body, admission).await
}

async fn bootstrap_request<R>(
    stream: &mut R,
    serial: u64,
    pdu: Pdu,
    admission: &RuntimeAdmission,
    _permit: &CountPermit,
) -> anyhow::Result<AdmittedRpcResponse<Pdu>>
where
    R: Unpin + AsyncRead + AsyncWrite + std::fmt::Debug,
{
    let expected_response = pdu.expected_response_tag().ok_or_else(|| {
        anyhow!(
            "PDU {} cannot be sent as a correlated bootstrap request",
            pdu.pdu_name()
        )
    })?;
    pdu.encode_async(stream, serial, admission)
        .await
        .context("encoding a bootstrap PDU")?;
    stream.flush().await.context("flushing a bootstrap PDU")?;
    let response = read_server_pdu_async(
        stream,
        |response_serial| (response_serial == serial).then_some(expected_response),
        admission,
    )
    .await?;
    if matches!(response.pdu(), Pdu::AttachRejected(_)) {
        return Err(AttachmentRejectedError.into());
    }
    response.into_rpc_response()
}

async fn bootstrap_client_stream_async<R>(
    stream: &mut R,
    client_id: &ClientId,
    resume_token: &AttachmentResumeToken,
    ui: &dyn ConnectionUi,
    admission: &RuntimeAdmission,
    permit: &CountPermit,
    expected_build_identity: Option<&BuildIdentity>,
) -> anyhow::Result<ClientBootstrap>
where
    R: Unpin + AsyncRead + AsyncWrite + std::fmt::Debug,
{
    ui.output_str("Checking server version\n");
    let info = match bootstrap_request(
        stream,
        1,
        Pdu::GetCodecVersion(GetCodecVersion {}),
        admission,
        permit,
    )
    .or(async {
        smol::Timer::after(Duration::from_secs(60)).await;
        Err(Timeout).context("Timeout")
    })
    .await
    {
        Ok(response) => response.try_map(|pdu| match pdu {
            Pdu::GetCodecVersionResponse(info) => Ok(info),
            unexpected => bail!("unexpected response {unexpected:?}"),
        })?,
        Err(err) => {
            log::trace!("{:?}", err);
            if err.root_cause().is::<AttachmentRejectedError>() {
                ui.output_str(&err.to_string());
                return Err(err);
            }
            let msg = if err.root_cause().is::<Timeout>() {
                "Timed out while parsing the response from the server. \
                 This may be due to network connectivity issues"
                    .to_string()
            } else if err.root_cause().is::<CorruptResponse>() {
                "Received an implausible and likely corrupt response from \
                 the server. This can happen if the remote host outputs \
                 to stdout prior to running commands. \
                 Check your shell startup!"
                    .to_string()
            } else {
                format!(
                    "Please install the same version of wezterm on both \
                     the client and server! \
                     The server reported error '{err}' while being asked for its \
                     version.  This likely means that the server is older \
                     than the client, but it could also happen if the remote \
                     host outputs to stdout prior to running commands. \
                     Check your shell startup!",
                )
            };
            ui.output_str(&msg);
            bail!("{}", msg);
        }
    };

    if info.value().codec_vers != CODEC_VERSION {
        let err = IncompatibleVersionError {
            version: info.value().version_string.clone(),
            codec_vers: info.value().codec_vers,
        };
        ui.output_str(&err.to_string());
        log::error!("{:?}", err);
        return Err(err.into());
    }
    log::trace!(
        "Server version is {} (codec version {})",
        info.value().version_string,
        info.value().codec_vers
    );

    let build = bootstrap_request(
        stream,
        2,
        Pdu::GetBuildIdentity(GetBuildIdentity {}),
        admission,
        permit,
    )
    .await?
    .try_map(|pdu| match pdu {
        Pdu::GetBuildIdentityResponse(build) => Ok(build),
        unexpected => bail!("unexpected response {unexpected:?}"),
    })?;
    log::trace!("Server build identity is {:?}", build.value().identity);
    if let Some(expected) = expected_build_identity {
        if &build.value().identity != expected {
            return Err(BuildIdentityMismatch {
                expected: expected.clone(),
                actual: build.value().identity.clone(),
            }
            .into());
        }
    }

    let registration = bootstrap_request(
        stream,
        3,
        Pdu::SetClientId(SetClientId {
            client_id: client_id.clone(),
            is_proxy: false,
            resume_token: Some(resume_token.clone()),
        }),
        admission,
        permit,
    )
    .await?
    .try_map(|pdu| match pdu {
        Pdu::SetClientIdResponse(response) => Ok(response),
        unexpected => bail!("unexpected response {unexpected:?}"),
    })?
    .into_inner();
    let confirmed_resume_token = registration
        .resume_token
        .ok_or_else(|| anyhow!("server registration omitted the attachment resume capability"))?;
    let control_snapshot = registration
        .control_snapshot
        .ok_or_else(|| anyhow!("server registration omitted the initial control snapshot"))?;
    anyhow::ensure!(
        confirmed_resume_token.eq(resume_token),
        "server confirmed a different attachment resume capability"
    );

    ui.output_str("Version check OK!\n");
    Ok(ClientBootstrap {
        server_version: info.into_inner(),
        next_serial: 4,
        resume_token: confirmed_resume_token,
        control_snapshot,
    })
}

fn client_thread(
    reconnectable: &mut Reconnectable,
    local_domain_id: Option<DomainId>,
    control: &AttachmentControlTracker,
    rx: &mut Receiver<ReaderMessage>,
    cancellation: &ClientCancelWaiter,
    next_serial: u64,
    admission: &RuntimeAdmission,
) -> anyhow::Result<()> {
    block_on(client_thread_async(
        reconnectable,
        local_domain_id,
        control,
        rx,
        cancellation,
        next_serial,
        admission,
    ))
}

async fn client_thread_async(
    reconnectable: &mut Reconnectable,
    local_domain_id: Option<DomainId>,
    control: &AttachmentControlTracker,
    rx: &mut Receiver<ReaderMessage>,
    cancellation: &ClientCancelWaiter,
    mut next_serial: u64,
    admission: &RuntimeAdmission,
) -> anyhow::Result<()> {
    struct Promises {
        map: HashMap<u64, PendingRequest>,
    }

    struct PendingRequest {
        expected_response: PduTag,
        promise: Sender<anyhow::Result<AdmittedRpcResponse<Pdu>>>,
        _permit: CountPermit,
    }

    impl Promises {
        fn fail_all(&mut self, reason: &str) {
            log::trace!("failing all promises: {}", reason);
            for (_, pending) in self.map.drain() {
                let _ = pending.promise.try_send(Err(anyhow!("{}", reason)));
            }
        }
    }

    impl Drop for Promises {
        fn drop(&mut self) {
            self.fail_all("Client was destroyed");
        }
    }
    let mut promises = Promises {
        map: HashMap::new(),
    };

    let mut stream = reconnectable.take_stream().unwrap();

    loop {
        if cancellation.is_cancelled() {
            return Err(ClientCancelled.into());
        }

        enum ClientThreadEvent {
            Cancelled,
            Io(anyhow::Result<ReaderMessage>),
        }

        let rx_msg = rx
            .recv()
            .map(|result| result.map_err(|_| anyhow!("client request queue closed")));
        let wait_for_read = stream
            .wait_for_readable()
            .map(|result| result.map(|_| ReaderMessage::Readable));
        let io = smol::future::or(rx_msg, wait_for_read).map(ClientThreadEvent::Io);
        let cancel = cancellation
            .receiver
            .recv()
            .map(|_| ClientThreadEvent::Cancelled);

        match smol::future::or(cancel, io).await {
            ClientThreadEvent::Cancelled => return Err(ClientCancelled.into()),
            ClientThreadEvent::Io(Ok(ReaderMessage::SendPdu {
                pdu,
                expected_response,
                promise,
                _permit,
            })) => {
                let serial = next_serial;
                next_serial = next_serial
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("client request serial space exhausted"))?;
                promises.map.insert(
                    serial,
                    PendingRequest {
                        expected_response,
                        promise,
                        _permit,
                    },
                );

                pdu.encode_async(&mut stream, serial, admission)
                    .await
                    .context("encoding a PDU to send to the server")?;
                stream.flush().await.context("flushing PDU to server")?;
            }
            ClientThreadEvent::Io(Ok(ReaderMessage::Readable)) => {
                let decoded = read_server_pdu_async(
                    &mut stream,
                    |serial| {
                        promises
                            .map
                            .get(&serial)
                            .map(|pending| pending.expected_response)
                    },
                    admission,
                )
                .await;
                match decoded {
                    Ok(decoded) => {
                        log::debug!(
                            "decoded serial {} {}",
                            decoded.serial(),
                            decoded.pdu().pdu_name()
                        );
                        if decoded.serial() == 0 {
                            let notification = decoded.into_notification()?;
                            process_unilateral(local_domain_id, control, notification)
                                .context("processing unilateral PDU from server")
                                .map_err(|e| {
                                    log::error!("process_unilateral: {:?}", e);
                                    e
                                })?;
                        } else {
                            let response = decoded.into_rpc_response()?;
                            let serial = response.serial();
                            let Some(pending) = promises.map.remove(&serial) else {
                                let reason =
                                    format!("got serial {serial} without a corresponding promise");
                                promises.fail_all(&reason);
                                anyhow::bail!("{reason}");
                            };
                            if pending.promise.try_send(Ok(response)).is_err() {
                                return Err(NotReconnectableError::ClientWasDestroyed.into());
                            }
                        }
                    }
                    Err(err) => {
                        let reason = format!("Error while decoding response pdu: {:#}", err);
                        log::error!("{}", reason);
                        promises.fail_all(&reason);
                        return Err(err).context("Error while decoding response pdu");
                    }
                }
            }
            ClientThreadEvent::Io(Err(_)) => {
                return Err(NotReconnectableError::ClientWasDestroyed.into());
            }
        }
    }
}

pub fn unix_connect_with_retry(
    target: &UnixTarget,
    just_spawned: bool,
    max_attempts: Option<u64>,
) -> anyhow::Result<UnixStream> {
    let mut error = None;

    if just_spawned {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    let max_attempts = max_attempts.unwrap_or(10);

    for iter in 0..max_attempts {
        if iter > 0 {
            std::thread::sleep(std::time::Duration::from_millis(iter * 50));
        }
        match target {
            UnixTarget::Socket(path) => match UnixStream::connect(path) {
                Ok(stream) => return Ok(stream),
                Err(err) => {
                    error =
                        Some(Err(err).with_context(|| format!("connecting to {}", path.display())))
                }
            },
            UnixTarget::Proxy(argv) => {
                let mut cmd = std::process::Command::new(&argv[0]);
                cmd.args(&argv[1..]);

                let (a, b) = filedescriptor::socketpair()?;

                cmd.stdin(b.as_stdio()?);
                cmd.stdout(b.as_stdio()?);
                cmd.stderr(std::process::Stdio::inherit());
                let mut child = cmd
                    .spawn()
                    .with_context(|| format!("spawning proxy command {:?}", cmd))?;

                error.take();

                // Grace period to detect whether connection failed
                for _ in 0..5 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            error = Some(Err(anyhow!(
                                "{:?} exited already with status {:?}",
                                cmd,
                                status
                            )));
                            continue;
                        }
                        Ok(None) => {
                            error.take();
                        }
                        Err(err) => {
                            error =
                                Some(Err(err).context(format!("spawning proxy command {:?}", cmd)));
                            continue;
                        }
                    }
                }

                if error.is_none() {
                    #[cfg(unix)]
                    unsafe {
                        use std::os::unix::io::{FromRawFd, IntoRawFd};
                        return Ok(UnixStream::from_raw_fd(a.into_raw_fd()));
                    }
                    #[cfg(windows)]
                    unsafe {
                        use std::os::windows::io::{FromRawSocket, IntoRawSocket};
                        return Ok(UnixStream::from_raw_socket(a.into_raw_socket()));
                    }
                }
            }
        }
    }

    error.expect("only get here after at least one unix fail")
}

#[async_trait(?Send)]
pub trait AsyncReadAndWrite: Unpin + AsyncRead + AsyncWrite + std::fmt::Debug + Send {
    async fn wait_for_readable(&self) -> anyhow::Result<()>;
}

#[async_trait(?Send)]
impl<T> AsyncReadAndWrite for Async<T>
where
    T: std::fmt::Debug,
    T: std::io::Write,
    T: std::io::Read,
    T: Send,
    T: async_io::IoSafe,
{
    async fn wait_for_readable(&self) -> anyhow::Result<()> {
        Ok(self.readable().await?)
    }
}

#[derive(Debug)]
struct Reconnectable {
    config: ClientDomainConfig,
    stream: Option<Box<dyn AsyncReadAndWrite>>,
    tls_creds: Option<GetTlsCredsResponse>,
    admission: Arc<RuntimeAdmission>,
    expected_build_identity: Option<BuildIdentity>,
}

struct SshStream {
    stdin: FileDescriptor,
    stdout: FileDescriptor,
}

unsafe impl async_io::IoSafe for SshStream {}

impl std::fmt::Debug for SshStream {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(fmt, "SshStream {{...}}")
    }
}

#[cfg(unix)]
impl AsFd for SshStream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stdout.as_fd()
    }
}

#[cfg(unix)]
impl AsRawFd for SshStream {
    fn as_raw_fd(&self) -> RawFd {
        self.stdout.as_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawSocket for SshStream {
    fn as_raw_socket(&self) -> RawSocket {
        self.stdout.as_raw_socket()
    }
}

#[cfg(windows)]
impl AsSocket for SshStream {
    fn as_socket(&self) -> BorrowedSocket {
        self.stdout.as_socket()
    }
}

impl Read for SshStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        self.stdout.read(buf)
    }
}

impl Write for SshStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, std::io::Error> {
        self.stdin.write(buf)
    }
    fn flush(&mut self) -> Result<(), std::io::Error> {
        self.stdin.flush()
    }
}

impl Reconnectable {
    fn new(
        config: ClientDomainConfig,
        stream: Option<Box<dyn AsyncReadAndWrite>>,
        admission: Arc<RuntimeAdmission>,
    ) -> Self {
        Self {
            config,
            stream,
            tls_creds: None,
            admission,
            expected_build_identity: None,
        }
    }

    fn expect_build_identity(mut self, identity: Option<BuildIdentity>) -> Self {
        self.expected_build_identity = identity;
        self
    }

    fn tls_creds_path(&self) -> anyhow::Result<PathBuf> {
        let path = config::pki_dir()?.join(self.config.name());
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn tls_creds_ca_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.tls_creds_path()?.join("ca.pem"))
    }

    fn tls_creds_cert_path(&self) -> anyhow::Result<PathBuf> {
        Ok(self.tls_creds_path()?.join("cert.pem"))
    }

    fn take_stream(&mut self) -> Option<Box<dyn AsyncReadAndWrite>> {
        self.stream.take()
    }

    fn is_local(&self) -> bool {
        matches!(&self.config, ClientDomainConfig::Unix(_))
    }

    fn reconnectable(&self, presentation: &ConnectionPresentation) -> bool {
        if presentation.is_headless() {
            return true;
        }
        match &self.config {
            // It doesn't make sense to reconnect to a unix socket; we only
            // get disconnected it it dies, so respawning it would not preserve
            // the set of tabs and we'd have confusing and inconsistent state
            ClientDomainConfig::Unix(_) => false,
            ClientDomainConfig::Tls(_) => true,
            // It *does* make sense to reconnect with an ssh session, but we
            // need to grow some smarts about whether the disconnect was because
            // we sent CTRL-D to close the last session, or whether it was a network
            // level disconnect, because we will otherwise throw up authentication
            // dialogs that would be annoying
            ClientDomainConfig::Ssh(_) => false,
        }
    }

    fn connect(
        &mut self,
        initial: bool,
        ui: &dyn ConnectionUi,
        no_auto_start: bool,
    ) -> anyhow::Result<()> {
        match self.config.clone() {
            ClientDomainConfig::Unix(unix_dom) => {
                self.unix_connect(unix_dom, initial, ui, no_auto_start)
            }
            ClientDomainConfig::Tls(tls) => self.tls_connect(tls, initial, ui),
            ClientDomainConfig::Ssh(ssh) => self.ssh_connect(ssh, initial, ui),
        }
    }

    /// Resolve the path to wezterm for the remote system.
    /// We can't simply derive this from the current executable because
    /// we are being asked to produce a path for the remote system and
    /// we don't really know anything about it.
    /// `path` comes from the SshDoman::remote_wezterm_path option; if set
    /// then the user has told us where to look.
    /// Otherwise, we have to rely on the `PATH` environment for the remote
    /// system, and we don't know if it is even running unix, or whether
    /// any given shell syntax will help us provide a more meaningful
    /// message to the user.
    fn wezterm_bin_path(path: &Option<String>) -> String {
        path.as_deref().unwrap_or("wezterm").to_string()
    }

    fn ssh_connect(
        &mut self,
        ssh_dom: SshDomain,
        initial: bool,
        ui: &dyn ConnectionUi,
    ) -> anyhow::Result<()> {
        let ssh_config = mux::ssh::ssh_domain_to_ssh_config(&ssh_dom)?;

        let sess = ssh_connect_with_ui(ssh_config, ui)?;
        let proxy_bin = Self::wezterm_bin_path(&ssh_dom.remote_wezterm_path);

        let cmd = if let Some(cmd) = ssh_dom.override_proxy_command.clone() {
            cmd
        } else if initial {
            format!("{} cli --prefer-mux proxy", proxy_bin)
        } else {
            format!("{} cli --prefer-mux --no-auto-start proxy", proxy_bin)
        };
        ui.output_str(&format!("Running: {}\n", cmd));
        log::debug!("going to run {}", cmd);

        let exec = smol::block_on(sess.exec(&cmd, None))?;

        let mut stderr = exec.stderr;
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while let Ok(len) = stderr.read(&mut buf) {
                if len == 0 {
                    break;
                } else {
                    let stderr = &buf[0..len];
                    log::error!("ssh stderr: {}", String::from_utf8_lossy(stderr));
                }
            }
        });

        // This is a bit gross, but it helps to surface errors in running
        // the proxy, and prevents us from hanging forever after the process
        // has died
        let mut child = exec.child;
        std::thread::spawn(move || match child.wait() {
            Err(err) => log::error!("waiting on {} failed: {:#}", cmd, err),
            Ok(status) if !status.success() => log::error!("{}: {}", cmd, status),
            _ => {}
        });

        let stream: Box<dyn AsyncReadAndWrite> = Box::new(Async::new(SshStream {
            stdin: exec.stdin,
            stdout: exec.stdout,
        })?);
        self.stream.replace(stream);
        Ok(())
    }

    fn unix_connect(
        &mut self,
        unix_dom: UnixDomain,
        initial: bool,
        ui: &dyn ConnectionUi,
        no_auto_start: bool,
    ) -> anyhow::Result<()> {
        let target = unix_dom.target();
        ui.output_str(&format!("Connect to {:?}\n", target));
        log::trace!("connect to {:?}", target);

        let max_attempts = if no_auto_start { Some(1) } else { None };

        let stream = match unix_connect_with_retry(&target, false, max_attempts) {
            Ok(stream) => stream,
            Err(e) => {
                if no_auto_start || unix_dom.no_serve_automatically || !initial {
                    bail!("failed to connect to {:?}: {}", target, e);
                }
                log::warn!(
                    "While connecting to {:?}: {}.  Will try spawning the server.",
                    target,
                    e
                );
                ui.output_str(&format!("Error: {}.  Will try spawning server.\n", e));

                let argv = unix_dom.serve_command()?;

                let mut cmd = std::process::Command::new(&argv[0]);
                cmd.args(&argv[1..]);

                #[cfg(unix)]
                if let Some(mask) = umask::UmaskSaver::saved_umask() {
                    unsafe {
                        cmd.pre_exec(move || {
                            libc::umask(mask);
                            Ok(())
                        });
                    }
                }

                log::warn!("Running: {:?}", cmd);
                ui.output_str(&format!("Running: {:?}\n", cmd));

                let child = cmd
                    .spawn()
                    .with_context(|| format!("while spawning {:?}", cmd))?;
                std::thread::spawn(move || match child.wait_with_output() {
                    Ok(out) => {
                        if let Ok(stdout) = std::str::from_utf8(&out.stdout) {
                            if !stdout.is_empty() {
                                log::warn!("stdout: {}", stdout);
                            }
                        }
                        if let Ok(stderr) = std::str::from_utf8(&out.stderr) {
                            if !stderr.is_empty() {
                                log::warn!("stderr: {}", stderr);
                            }
                        }
                    }
                    Err(err) => {
                        log::error!("spawn: {:#}", err);
                    }
                });

                unix_connect_with_retry(&target, true, None).with_context(|| {
                    format!("(after spawning server) failed to connect to {:?}", target)
                })?
            }
        };

        ui.output_str("Connected!\n");
        stream.set_read_timeout(Some(unix_dom.read_timeout))?;
        stream.set_write_timeout(Some(unix_dom.write_timeout))?;
        let stream: Box<dyn AsyncReadAndWrite> = Box::new(Async::new(stream)?);
        self.stream.replace(stream);
        Ok(())
    }

    pub fn tls_connect(
        &mut self,
        tls_client: TlsDomainClient,
        _initial: bool,
        ui: &dyn ConnectionUi,
    ) -> anyhow::Result<()> {
        openssl::init();

        let remote_address = &tls_client.remote_address;

        let remote_host_name = remote_address.split(':').next().ok_or_else(|| {
            anyhow!(
                "expected mux_server_remote_address to have the form 'host:port', but have {}",
                remote_address
            )
        })?;

        // If we are reconnecting and already bootstrapped via SSH, let's see if
        // we can connect using those same credentials and avoid running through
        // the SSH authentication flow.
        if let Some(Ok(_)) = tls_client.ssh_parameters() {
            match self.try_connect(&tls_client, ui, remote_address, remote_host_name) {
                Ok(stream) => {
                    self.stream.replace(stream);
                    return Ok(());
                }
                Err(err) => {
                    if let Some(ioerr) = err.root_cause().downcast_ref::<std::io::Error>() {
                        match ioerr.kind() {
                            std::io::ErrorKind::ConnectionRefused => {
                                // Server isn't up yet; let's proceed with bootstrap
                            }
                            _ => {
                                // If it is an IO error that implies that we had an issue
                                // reaching or otherwise talking to the remote host.
                                // Re-attempting the SSH bootstrap most likely will not
                                // succeed so we let this bubble up.
                                return Err(err);
                            }
                        }
                    }
                    ui.output_str(&format!(
                        "Failed to reuse creds: {:?}\nWill retry bootstrap via SSH\n",
                        err
                    ));
                }
            }
        }

        if let Some(Ok(ssh_params)) = tls_client.ssh_parameters() {
            if self.tls_creds.is_none() {
                // We need to bootstrap via an ssh session

                let mut ssh_config = wezterm_ssh::Config::new();
                ssh_config.add_default_config_files();

                let mut fields = ssh_params.host_and_port.split(':');
                let host = fields
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("no host component somehow"))?;
                let port = fields.next();

                let mut ssh_config = ssh_config.for_host(host);
                if let Some(username) = &ssh_params.username {
                    ssh_config.insert("user".to_string(), username.to_string());
                }
                if let Some(port) = port {
                    ssh_config.insert("port".to_string(), port.to_string());
                }

                let sess = ssh_connect_with_ui(ssh_config, ui)?;

                let creds = run_and_report_connection_error(ui, || {
                    // The `tlscreds` command will start the server if needed and then
                    // obtain client credentials that we can use for tls.
                    let cmd = format!(
                        "{} cli tlscreds",
                        Self::wezterm_bin_path(&tls_client.remote_wezterm_path)
                    );

                    ui.output_str(&format!("Running: {}\n", cmd));
                    let mut exec = smol::block_on(sess.exec(&cmd, None))
                        .with_context(|| format!("executing `{}` on remote host", cmd))?;

                    log::debug!("waiting for command to finish");
                    let status = exec.child.wait()?;
                    if !status.success() {
                        anyhow::bail!("{} failed", cmd);
                    }

                    drop(exec.stdin);

                    let mut stderr = exec.stderr;
                    thread::spawn(move || {
                        // stderr is ideally empty
                        let mut err = String::new();
                        let _ = stderr.read_to_string(&mut err);
                        if !err.is_empty() {
                            log::error!("remote: `{}` stderr -> `{}`", cmd, err);
                        }
                    });

                    let creds = Pdu::decode(
                        exec.stdout,
                        DecodeContext::server_to_client_response(Some(PduTag::GetTlsCredsResponse)),
                        &self.admission,
                    )
                    .context("reading tlscreds response")?
                    .into_rpc_response()?
                    .try_map(|pdu| match pdu {
                        Pdu::GetTlsCredsResponse(creds) => Ok(creds),
                        unexpected => bail!("unexpected response to tlscreds: {unexpected:?}"),
                    })?;

                    // Save the credentials to disk, as that is currently the easiest
                    // way to get them into openssl.  Ideally we'd keep these entirely
                    // in memory.
                    std::fs::write(
                        &self.tls_creds_ca_path()?,
                        creds.value().ca_cert_pem.as_bytes(),
                    )?;
                    std::fs::write(
                        &self.tls_creds_cert_path()?,
                        creds.value().client_cert_pem.as_bytes(),
                    )?;
                    log::info!("got TLS creds");
                    Ok(creds.into_inner())
                })?;
                self.tls_creds.replace(creds);
            }
        }

        let stream = run_and_report_connection_error(ui, || {
            self.try_connect(&tls_client, ui, remote_address, remote_host_name)
        })?;
        self.stream.replace(stream);
        Ok(())
    }

    fn try_connect(
        &mut self,
        tls_client: &TlsDomainClient,
        ui: &dyn ConnectionUi,
        remote_address: &str,
        remote_host_name: &str,
    ) -> anyhow::Result<Box<dyn AsyncReadAndWrite>> {
        let mut connector = SslConnector::builder(SslMethod::tls())?;

        let cert_file = match tls_client.pem_cert.clone() {
            Some(cert) => cert,
            None => self.tls_creds_cert_path()?,
        };

        connector
            .set_certificate_file(&cert_file, SslFiletype::PEM)
            .context(format!(
                "set_certificate_file to {} for TLS client",
                cert_file.display()
            ))?;

        if let Some(chain_file) = tls_client.pem_ca.as_ref() {
            connector
                .set_certificate_chain_file(chain_file)
                .context(format!(
                    "set_certificate_chain_file to {} for TLS client",
                    chain_file.display()
                ))?;
        }

        let key_file = match tls_client.pem_private_key.clone() {
            Some(key) => key,
            None => self.tls_creds_cert_path()?,
        };
        connector
            .set_private_key_file(&key_file, SslFiletype::PEM)
            .context(format!(
                "set_private_key_file to {} for TLS client",
                key_file.display()
            ))?;

        fn load_cert(name: &Path) -> anyhow::Result<X509> {
            let cert_bytes = std::fs::read(name)?;
            log::trace!("loaded {}", name.display());
            Ok(X509::from_pem(&cert_bytes)?)
        }
        for name in &tls_client.pem_root_certs {
            if name.is_dir() {
                for entry in std::fs::read_dir(name)? {
                    if let Ok(cert) = load_cert(&entry?.path()) {
                        connector.cert_store_mut().add_cert(cert).ok();
                    }
                }
            } else {
                connector.cert_store_mut().add_cert(load_cert(name)?)?;
            }
        }

        if let Ok(ca_path) = self.tls_creds_ca_path() {
            if ca_path.exists() {
                connector.cert_store_mut().add_cert(load_cert(&ca_path)?)?;
            }
        }

        let connector = connector.build();
        let connector = connector
            .configure()?
            .verify_hostname(!tls_client.accept_invalid_hostnames);

        ui.output_str(&format!("Connecting to {} using TLS\n", remote_address));
        let stream = TcpStream::connect(remote_address)
            .with_context(|| format!("connecting to {}", remote_address))?;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(tls_client.write_timeout))?;
        stream.set_read_timeout(Some(tls_client.read_timeout))?;

        let stream = Box::new(Async::new(AsyncSslStream::new(
            connector
                .connect(
                    tls_client
                        .expected_cn
                        .as_deref()
                        .unwrap_or(remote_host_name),
                    stream,
                )
                .with_context(|| {
                    format!(
                        "SslConnector for {} with host name {}",
                        remote_address, remote_host_name,
                    )
                })?,
        ))?);
        ui.output_str("TLS Connected!\n");
        Ok(stream)
    }
}

struct InitialConnectionRequest {
    initial: bool,
    no_auto_start: bool,
}

trait ClientRuntimeConnection: Send {
    fn connect_for_runtime(
        &mut self,
        initial: bool,
        presentation: &dyn ConnectionUi,
        no_auto_start: bool,
        cancellation: &ClientCancelWaiter,
    ) -> anyhow::Result<()>;

    fn bootstrap_connected_stream(
        &mut self,
        client_id: &ClientId,
        resume_token: &AttachmentResumeToken,
        presentation: &dyn ConnectionUi,
        cancellation: &ClientCancelWaiter,
        permit: &CountPermit,
    ) -> anyhow::Result<ClientBootstrap>;

    fn run_connected_session(
        &mut self,
        local_domain_id: Option<DomainId>,
        control: &AttachmentControlTracker,
        receiver: &mut Receiver<ReaderMessage>,
        cancellation: &ClientCancelWaiter,
        next_serial: u64,
    ) -> anyhow::Result<()>;

    fn reconnectable_for_runtime(&self, presentation: &ConnectionPresentation) -> bool;
}

impl ClientRuntimeConnection for Reconnectable {
    fn connect_for_runtime(
        &mut self,
        initial: bool,
        presentation: &dyn ConnectionUi,
        no_auto_start: bool,
        cancellation: &ClientCancelWaiter,
    ) -> anyhow::Result<()> {
        if cancellation.is_cancelled() {
            return Err(ClientCancelled.into());
        }
        let result = Reconnectable::connect(self, initial, presentation, no_auto_start);
        if cancellation.is_cancelled() {
            return Err(ClientCancelled.into());
        }
        result
    }

    fn bootstrap_connected_stream(
        &mut self,
        client_id: &ClientId,
        resume_token: &AttachmentResumeToken,
        presentation: &dyn ConnectionUi,
        cancellation: &ClientCancelWaiter,
        permit: &CountPermit,
    ) -> anyhow::Result<ClientBootstrap> {
        if cancellation.is_cancelled() {
            return Err(ClientCancelled.into());
        }
        let admission = Arc::clone(&self.admission);
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| anyhow!("connected client has no stream to bootstrap"))?;
        block_on(smol::future::or(
            bootstrap_client_stream_async(
                stream,
                client_id,
                resume_token,
                presentation,
                &admission,
                permit,
                self.expected_build_identity.as_ref(),
            ),
            async {
                let _ = cancellation.receiver.recv().await;
                Err(ClientCancelled.into())
            },
        ))
    }

    fn run_connected_session(
        &mut self,
        local_domain_id: Option<DomainId>,
        control: &AttachmentControlTracker,
        receiver: &mut Receiver<ReaderMessage>,
        cancellation: &ClientCancelWaiter,
        next_serial: u64,
    ) -> anyhow::Result<()> {
        let admission = Arc::clone(&self.admission);
        client_thread(
            self,
            local_domain_id,
            control,
            receiver,
            cancellation,
            next_serial,
            &admission,
        )
    }

    fn reconnectable_for_runtime(&self, presentation: &ConnectionPresentation) -> bool {
        self.reconnectable(presentation)
    }
}

trait ClientRuntimeHost {
    fn schedule_reattach(
        &self,
        domain_id: DomainId,
        presentation: ConnectionPresentation,
        cancelled: Arc<AtomicBool>,
    ) -> anyhow::Result<()>;

    fn schedule_cleanup(&self, domain_id: DomainId) -> anyhow::Result<()>;
}

struct MuxClientRuntimeHost;

impl ClientRuntimeHost for MuxClientRuntimeHost {
    fn schedule_reattach(
        &self,
        domain_id: DomainId,
        presentation: ConnectionPresentation,
        cancelled: Arc<AtomicBool>,
    ) -> anyhow::Result<()> {
        schedule_domain_reattach(domain_id, presentation, cancelled)
    }

    fn schedule_cleanup(&self, domain_id: DomainId) -> anyhow::Result<()> {
        schedule_domain_cleanup(domain_id)
    }
}

#[derive(Clone, Copy)]
struct ReconnectBackoff {
    initial: Duration,
    maximum: Duration,
}

impl ReconnectBackoff {
    const STANDARD: Self = Self {
        initial: Duration::from_secs(1),
        maximum: Duration::from_secs(10),
    };
}

fn connection_failure(error: &anyhow::Error) -> HeadlessConnectionFailure {
    if error.root_cause().is::<HeadlessPromptRequired>() {
        HeadlessConnectionFailure::PromptRequired
    } else {
        HeadlessConnectionFailure::Transport
    }
}

fn should_reconnect(
    connection: &dyn ClientRuntimeConnection,
    presentation: &ConnectionPresentation,
    local_domain_id: Option<DomainId>,
    error: &anyhow::Error,
) -> bool {
    local_domain_id.is_some()
        && connection.reconnectable_for_runtime(presentation)
        && !error.root_cause().is::<ClientCancelled>()
        && !error.root_cause().is::<NotReconnectableError>()
}

fn lifecycle_error_outcome(error: HeadlessLifecycleError) -> ClientRuntimeOutcome {
    ClientRuntimeOutcome::Failed(match error {
        HeadlessLifecycleError::Saturated => HeadlessConnectionFailure::LifecycleSaturated,
        HeadlessLifecycleError::Closed | HeadlessLifecycleError::Empty => {
            HeadlessConnectionFailure::LifecycleClosed
        }
        HeadlessLifecycleError::Admission(_) => HeadlessConnectionFailure::Runtime,
    })
}

fn publish_terminal_failure(
    presentation: &ConnectionPresentation,
    failure: HeadlessConnectionFailure,
) -> ClientRuntimeOutcome {
    match presentation.publish(HeadlessConnectionState::Failed(failure)) {
        Ok(()) => ClientRuntimeOutcome::Failed(failure),
        Err(error) => lifecycle_error_outcome(error),
    }
}

fn wait_for_reconnect_backoff(cancellation: &ClientCancelWaiter, duration: Duration) -> bool {
    if cancellation.is_cancelled() {
        return false;
    }
    block_on(smol::future::or(
        async {
            let _ = cancellation.receiver.recv().await;
            false
        },
        async {
            smol::Timer::after(duration).await;
            true
        },
    ))
}

fn schedule_domain_reattach(
    domain_id: DomainId,
    presentation: ConnectionPresentation,
    cancelled: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let mux = Mux::try_get().ok_or_else(|| anyhow!("no mux for client domain reattach"))?;
    mux.try_spawn_client_invalidation(async move {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        match ClientDomain::reattach(domain_id).await {
            Ok(()) => {
                presentation.close();
                Ok(())
            }
            Err(error) => {
                let publish = presentation
                    .publish(HeadlessConnectionState::Failed(
                        HeadlessConnectionFailure::Transport,
                    ))
                    .map_err(anyhow::Error::from);
                presentation.close();
                publish?;
                Err(error)
            }
        }
    })
}

fn schedule_domain_cleanup(domain_id: DomainId) -> anyhow::Result<()> {
    let mux = Mux::try_get().ok_or_else(|| anyhow!("no mux for client domain cleanup"))?;
    mux.try_spawn_client_invalidation(async move {
        let mux = Mux::try_get().ok_or_else(|| anyhow!("no mux for client domain cleanup"))?;
        let Some(domain) = mux.get_domain(domain_id) else {
            return Ok(());
        };
        let domain = domain
            .downcast_ref::<ClientDomain>()
            .ok_or_else(|| anyhow!("domain {} is not a ClientDomain instance", domain_id))?;
        domain.perform_detach();
        Ok(())
    })
}

fn fail_runtime_and_schedule_cleanup(
    host: &dyn ClientRuntimeHost,
    domain_id: DomainId,
    presentation: &ConnectionPresentation,
    failure: HeadlessConnectionFailure,
) -> ClientRuntimeOutcome {
    if host.schedule_cleanup(domain_id).is_err() {
        return publish_terminal_failure(presentation, HeadlessConnectionFailure::Runtime);
    }
    publish_terminal_failure(presentation, failure)
}

fn publish_detached_and_schedule_cleanup(
    host: &dyn ClientRuntimeHost,
    local_domain_id: Option<DomainId>,
    presentation: &ConnectionPresentation,
    outcome: ClientRuntimeOutcome,
) -> ClientRuntimeOutcome {
    if let Some(domain_id) = local_domain_id {
        if host.schedule_cleanup(domain_id).is_err() {
            return publish_terminal_failure(presentation, HeadlessConnectionFailure::Runtime);
        }
    }
    match presentation.publish(HeadlessConnectionState::Detached) {
        Ok(()) => outcome,
        Err(error) => lifecycle_error_outcome(error),
    }
}

struct ClientRuntimeRun<'a> {
    local_domain_id: Option<DomainId>,
    control: Arc<AttachmentControlTracker>,
    receiver: Receiver<ReaderMessage>,
    presentation: ConnectionPresentation,
    cancellation: ClientCancelWaiter,
    initial_connection: Option<InitialConnectionRequest>,
    initial_ready: Sender<anyhow::Result<()>>,
    client_id: ClientId,
    attachment_resume_token: AttachmentResumeToken,
    initial_server_version: Arc<OnceLock<GetCodecVersionResponse>>,
    bootstrap_request_permit: CountPermit,
    host: &'a dyn ClientRuntimeHost,
    reconnect_backoff: ReconnectBackoff,
}

fn run_client_runtime<C: ClientRuntimeConnection>(
    mut connection: C,
    run: ClientRuntimeRun<'_>,
) -> ClientRuntimeOutcome {
    let ClientRuntimeRun {
        local_domain_id,
        control,
        mut receiver,
        presentation,
        cancellation,
        initial_connection,
        initial_ready,
        client_id,
        attachment_resume_token,
        initial_server_version,
        bootstrap_request_permit,
        host,
        reconnect_backoff,
    } = run;
    if let Some(initial_connection) = initial_connection {
        if cancellation.is_cancelled() {
            let _ = initial_ready.try_send(Err(ClientCancelled.into()));
            return publish_detached_and_schedule_cleanup(
                host,
                local_domain_id,
                &presentation,
                ClientRuntimeOutcome::Cancelled,
            );
        }
        if let Err(error) = connection.connect_for_runtime(
            initial_connection.initial,
            &presentation,
            initial_connection.no_auto_start,
            &cancellation,
        ) {
            let cancelled = error.root_cause().is::<ClientCancelled>();
            let failure = connection_failure(&error);
            let _ = initial_ready.try_send(Err(error));
            if cancelled {
                return publish_detached_and_schedule_cleanup(
                    host,
                    local_domain_id,
                    &presentation,
                    ClientRuntimeOutcome::Cancelled,
                );
            }
            return match local_domain_id {
                Some(domain_id) => {
                    fail_runtime_and_schedule_cleanup(host, domain_id, &presentation, failure)
                }
                None => publish_terminal_failure(&presentation, failure),
            };
        }
    }

    if cancellation.is_cancelled() {
        let _ = initial_ready.try_send(Err(ClientCancelled.into()));
        return publish_detached_and_schedule_cleanup(
            host,
            local_domain_id,
            &presentation,
            ClientRuntimeOutcome::Cancelled,
        );
    }

    let initial_bootstrap = {
        let mut attempt = 0u32;
        let mut backoff = reconnect_backoff.initial;
        loop {
            match connection.bootstrap_connected_stream(
                &client_id,
                &attachment_resume_token,
                &presentation,
                &cancellation,
                &bootstrap_request_permit,
            ) {
                Ok(bootstrap) => break bootstrap,
                Err(error)
                    if should_reconnect(&connection, &presentation, local_domain_id, &error)
                        && !error.root_cause().is::<IncompatibleVersionError>()
                        && !error.root_cause().is::<BuildIdentityMismatch>() =>
                {
                    attempt = attempt.saturating_add(1);
                    if presentation
                        .reconnect_attempt_limit()
                        .is_some_and(|limit| attempt > limit.get())
                    {
                        let _ = initial_ready.try_send(Err(error));
                        let domain_id =
                            local_domain_id.expect("reconnect requires a client domain");
                        return fail_runtime_and_schedule_cleanup(
                            host,
                            domain_id,
                            &presentation,
                            HeadlessConnectionFailure::RetryExhausted,
                        );
                    }
                    if let Err(error) =
                        presentation.publish(HeadlessConnectionState::Reconnecting { attempt })
                    {
                        let _ = initial_ready.try_send(Err(error.into()));
                        let domain_id =
                            local_domain_id.expect("reconnect requires a client domain");
                        return fail_runtime_and_schedule_cleanup(
                            host,
                            domain_id,
                            &presentation,
                            HeadlessConnectionFailure::Runtime,
                        );
                    }
                    if !wait_for_reconnect_backoff(&cancellation, backoff) {
                        let _ = initial_ready.try_send(Err(ClientCancelled.into()));
                        return publish_detached_and_schedule_cleanup(
                            host,
                            local_domain_id,
                            &presentation,
                            ClientRuntimeOutcome::Cancelled,
                        );
                    }
                    match connection.connect_for_runtime(false, &presentation, true, &cancellation)
                    {
                        Ok(()) => {}
                        Err(error) if error.root_cause().is::<ClientCancelled>() => {
                            let _ = initial_ready.try_send(Err(error));
                            return publish_detached_and_schedule_cleanup(
                                host,
                                local_domain_id,
                                &presentation,
                                ClientRuntimeOutcome::Cancelled,
                            );
                        }
                        Err(error) if error.root_cause().is::<HeadlessPromptRequired>() => {
                            let _ = initial_ready.try_send(Err(error));
                            let domain_id =
                                local_domain_id.expect("reconnect requires a client domain");
                            return fail_runtime_and_schedule_cleanup(
                                host,
                                domain_id,
                                &presentation,
                                HeadlessConnectionFailure::PromptRequired,
                            );
                        }
                        Err(_) => {}
                    }
                    backoff = (backoff + backoff).min(reconnect_backoff.maximum);
                }
                Err(error) => {
                    let cancelled = error.root_cause().is::<ClientCancelled>();
                    let _ = initial_ready.try_send(Err(error));
                    if cancelled {
                        return publish_detached_and_schedule_cleanup(
                            host,
                            local_domain_id,
                            &presentation,
                            ClientRuntimeOutcome::Cancelled,
                        );
                    }
                    return match local_domain_id {
                        Some(domain_id) => fail_runtime_and_schedule_cleanup(
                            host,
                            domain_id,
                            &presentation,
                            HeadlessConnectionFailure::Runtime,
                        ),
                        None => publish_terminal_failure(
                            &presentation,
                            HeadlessConnectionFailure::Runtime,
                        ),
                    };
                }
            }
        }
    };
    let ClientBootstrap {
        server_version,
        mut next_serial,
        mut resume_token,
        control_snapshot,
    } = initial_bootstrap;
    if initial_server_version.set(server_version).is_err() {
        let _ = initial_ready.try_send(Err(anyhow!(
            "initial client bootstrap result was already published"
        )));
        return match local_domain_id {
            Some(domain_id) => fail_runtime_and_schedule_cleanup(
                host,
                domain_id,
                &presentation,
                HeadlessConnectionFailure::Runtime,
            ),
            None => publish_terminal_failure(&presentation, HeadlessConnectionFailure::Runtime),
        };
    }
    control.begin_connection();
    if let Err(error) = control.reduce_snapshot(control_snapshot) {
        let _ = initial_ready.try_send(Err(error.into()));
        return match local_domain_id {
            Some(domain_id) => fail_runtime_and_schedule_cleanup(
                host,
                domain_id,
                &presentation,
                HeadlessConnectionFailure::Runtime,
            ),
            None => publish_terminal_failure(&presentation, HeadlessConnectionFailure::Runtime),
        };
    }
    if initial_ready.try_send(Ok(())).is_err() {
        return publish_detached_and_schedule_cleanup(
            host,
            local_domain_id,
            &presentation,
            ClientRuntimeOutcome::Cancelled,
        );
    }

    let mut backoff = reconnect_backoff.initial;
    loop {
        match connection.run_connected_session(
            local_domain_id,
            &control,
            &mut receiver,
            &cancellation,
            next_serial,
        ) {
            Ok(()) => return ClientRuntimeOutcome::Completed,
            Err(error) if error.root_cause().is::<ClientCancelled>() => {
                return publish_detached_and_schedule_cleanup(
                    host,
                    local_domain_id,
                    &presentation,
                    ClientRuntimeOutcome::Cancelled,
                );
            }
            Err(error) if error.root_cause().is::<NotReconnectableError>() => {
                return publish_detached_and_schedule_cleanup(
                    host,
                    local_domain_id,
                    &presentation,
                    ClientRuntimeOutcome::Detached,
                );
            }
            Err(error)
                if !should_reconnect(&connection, &presentation, local_domain_id, &error) =>
            {
                if let Some(domain_id) = local_domain_id {
                    return fail_runtime_and_schedule_cleanup(
                        host,
                        domain_id,
                        &presentation,
                        connection_failure(&error),
                    );
                }
                return publish_terminal_failure(&presentation, connection_failure(&error));
            }
            Err(error) => {
                log::warn!("client connection ended; reconnecting: {error:#}");
            }
        }

        let domain_id = local_domain_id.expect("reconnect requires a client domain");
        let reconnect_presentation = presentation.for_reconnect();
        let mut attempt = 1u32;
        loop {
            if reconnect_presentation
                .reconnect_attempt_limit()
                .is_some_and(|limit| attempt > limit.get())
            {
                return fail_runtime_and_schedule_cleanup(
                    host,
                    domain_id,
                    &reconnect_presentation,
                    HeadlessConnectionFailure::RetryExhausted,
                );
            }
            if let Err(error) =
                reconnect_presentation.publish(HeadlessConnectionState::Reconnecting { attempt })
            {
                let outcome = lifecycle_error_outcome(error);
                let failure = match outcome {
                    ClientRuntimeOutcome::Failed(failure) => failure,
                    _ => HeadlessConnectionFailure::Runtime,
                };
                return fail_runtime_and_schedule_cleanup(
                    host,
                    domain_id,
                    &reconnect_presentation,
                    failure,
                );
            }
            if !wait_for_reconnect_backoff(&cancellation, backoff) {
                return publish_detached_and_schedule_cleanup(
                    host,
                    local_domain_id,
                    &presentation,
                    ClientRuntimeOutcome::Cancelled,
                );
            }

            let initial = false;
            let no_auto_start = true;
            match connection.connect_for_runtime(
                initial,
                &reconnect_presentation,
                no_auto_start,
                &cancellation,
            ) {
                Ok(()) => {
                    if cancellation.is_cancelled() {
                        return publish_detached_and_schedule_cleanup(
                            host,
                            local_domain_id,
                            &reconnect_presentation,
                            ClientRuntimeOutcome::Cancelled,
                        );
                    }
                    let bootstrap = match connection.bootstrap_connected_stream(
                        &client_id,
                        &resume_token,
                        &reconnect_presentation,
                        &cancellation,
                        &bootstrap_request_permit,
                    ) {
                        Ok(bootstrap) => bootstrap,
                        Err(error) if error.root_cause().is::<ClientCancelled>() => {
                            return publish_detached_and_schedule_cleanup(
                                host,
                                local_domain_id,
                                &reconnect_presentation,
                                ClientRuntimeOutcome::Cancelled,
                            );
                        }
                        Err(error) if error.root_cause().is::<IncompatibleVersionError>() => {
                            return fail_runtime_and_schedule_cleanup(
                                host,
                                domain_id,
                                &reconnect_presentation,
                                HeadlessConnectionFailure::Runtime,
                            );
                        }
                        Err(_error) => {
                            backoff = (backoff + backoff).min(reconnect_backoff.maximum);
                            attempt = attempt.saturating_add(1);
                            continue;
                        }
                    };
                    next_serial = bootstrap.next_serial;
                    resume_token = bootstrap.resume_token;
                    control.begin_connection();
                    if control.reduce_snapshot(bootstrap.control_snapshot).is_err() {
                        return fail_runtime_and_schedule_cleanup(
                            host,
                            domain_id,
                            &reconnect_presentation,
                            HeadlessConnectionFailure::Runtime,
                        );
                    }
                    if let Err(_error) = host.schedule_reattach(
                        domain_id,
                        reconnect_presentation.clone(),
                        Arc::clone(&cancellation.requested),
                    ) {
                        return fail_runtime_and_schedule_cleanup(
                            host,
                            domain_id,
                            &reconnect_presentation,
                            HeadlessConnectionFailure::Runtime,
                        );
                    }
                    backoff = reconnect_backoff.initial;
                    break;
                }
                Err(error) if error.root_cause().is::<ClientCancelled>() => {
                    return publish_detached_and_schedule_cleanup(
                        host,
                        local_domain_id,
                        &reconnect_presentation,
                        ClientRuntimeOutcome::Cancelled,
                    );
                }
                Err(error) if error.root_cause().is::<HeadlessPromptRequired>() => {
                    return fail_runtime_and_schedule_cleanup(
                        host,
                        domain_id,
                        &reconnect_presentation,
                        HeadlessConnectionFailure::PromptRequired,
                    );
                }
                Err(_error) => {
                    backoff = (backoff + backoff).min(reconnect_backoff.maximum);
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }
}

impl Client {
    fn new(
        local_domain_id: Option<DomainId>,
        reconnectable: Reconnectable,
        admission: Arc<RuntimeAdmission>,
        presentation: ConnectionPresentation,
        initial_connection: Option<InitialConnectionRequest>,
        client_id: ClientId,
    ) -> anyhow::Result<(Self, Receiver<anyhow::Result<()>>)> {
        if !Arc::ptr_eq(&admission, &reconnectable.admission) {
            bail!("client runtime and connection must share one runtime admission owner");
        }
        if let ConnectionPresentation::Headless(reporter) = &presentation {
            if !Arc::ptr_eq(&admission, &reporter.admission) {
                bail!("headless lifecycle and client must share one runtime admission owner");
            }
        }
        let client_domain_config = reconnectable.config.clone();
        let is_reconnectable = reconnectable.reconnectable(&presentation);
        let is_local = reconnectable.is_local();
        let (sender, receiver) = bounded(wezterm_runtime_admission::MAX_CLIENT_REQUESTS);
        let worker_client_id = client_id.clone();
        let mut resume_token_bytes = [0u8; 32];
        getrandom::fill(&mut resume_token_bytes)
            .map_err(|error| anyhow!("generating attachment resume capability: {error}"))?;
        let worker_attachment_resume_token =
            AttachmentResumeToken::from_random_bytes(resume_token_bytes);
        let control = Arc::new(AttachmentControlTracker::default());
        let worker_control = Arc::clone(&control);
        let initial_server_version = Arc::new(OnceLock::new());
        let worker_initial_server_version = Arc::clone(&initial_server_version);
        let bootstrap_request_permit = reserve_client_request(&admission)?;
        let (initial_ready, initial_ready_receiver) = bounded(1);
        let worker_presentation = presentation.clone();
        let runtime = ClientRuntime::spawn(&admission, move |cancellation| {
            let host = MuxClientRuntimeHost;
            run_client_runtime(
                reconnectable,
                ClientRuntimeRun {
                    local_domain_id,
                    control: worker_control,
                    receiver,
                    presentation: worker_presentation,
                    cancellation,
                    initial_connection,
                    initial_ready,
                    client_id: worker_client_id,
                    attachment_resume_token: worker_attachment_resume_token,
                    initial_server_version: worker_initial_server_version,
                    bootstrap_request_permit,
                    host: &host,
                    reconnect_backoff: ReconnectBackoff::STANDARD,
                },
            )
        })?;

        Ok((
            Self {
                sender,
                runtime,
                admission,
                control,
                presentation,
                local_domain_id,
                is_reconnectable,
                is_local,
                client_id,
                initial_server_version,
                client_domain_config,
            },
            initial_ready_receiver,
        ))
    }

    async fn await_initial_bootstrap(
        self,
        initial_ready: Receiver<anyhow::Result<()>>,
    ) -> anyhow::Result<Self> {
        match initial_ready.recv().await {
            Ok(Ok(())) => Ok(self),
            Ok(Err(error)) => {
                let _ = self.shutdown_and_join();
                Err(error)
            }
            Err(_) => {
                let outcome = self.shutdown_and_join();
                bail!("client runtime ended before initial bootstrap: {outcome:?}")
            }
        }
    }

    pub fn into_client_domain_config(self) -> ClientDomainConfig {
        self.client_domain_config
    }

    pub fn shutdown_and_join(&self) -> ClientRuntimeOutcome {
        self.runtime.shutdown_and_join()
    }

    pub fn pane_control_status(&self, pane_id: PaneId) -> PaneControlStatus {
        self.control.pane_status(pane_id)
    }

    pub(crate) fn publish_ready(&self) -> Result<(), HeadlessLifecycleError> {
        self.presentation.publish(HeadlessConnectionState::Ready)
    }

    pub fn initial_server_version(&self) -> &GetCodecVersionResponse {
        self.initial_server_version
            .get()
            .expect("Client constructors complete bootstrap before returning")
    }

    #[allow(dead_code)]
    pub fn local_domain_id(&self) -> Option<DomainId> {
        self.local_domain_id
    }

    pub fn admission(&self) -> &Arc<RuntimeAdmission> {
        &self.admission
    }

    fn compute_unix_domain(
        prefer_mux: bool,
        class_name: &str,
    ) -> anyhow::Result<config::UnixDomain> {
        match std::env::var_os("WEZTERM_UNIX_SOCKET") {
            Some(path) if !path.is_empty() => Ok(config::UnixDomain {
                socket_path: Some(path.into()),
                ..Default::default()
            }),
            Some(_) | None => {
                if !prefer_mux {
                    if let Ok(gui) = crate::discovery::resolve_gui_sock_path(class_name) {
                        return Ok(config::UnixDomain {
                            socket_path: Some(gui),
                            no_serve_automatically: true,
                            ..Default::default()
                        });
                    }
                }

                let config = configuration();
                Ok(config
                    .unix_domains
                    .first()
                    .ok_or_else(|| {
                        anyhow!(
                            "no default unix domain is configured and WEZTERM_UNIX_SOCKET \
                             is not set in the environment"
                        )
                    })?
                    .clone())
            }
        }
    }

    pub fn new_default_unix_domain(
        admission: Arc<RuntimeAdmission>,
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
        prefer_mux: bool,
        class_name: &str,
    ) -> anyhow::Result<Self> {
        let unix_dom = Self::compute_unix_domain(prefer_mux, class_name)?;
        Self::new_unix_domain(admission, None, &unix_dom, initial, ui, no_auto_start)
    }

    pub fn new_unix_domain(
        admission: Arc<RuntimeAdmission>,
        local_domain_id: Option<DomainId>,
        unix_dom: &UnixDomain,
        initial: bool,
        ui: &mut ConnectionUI,
        no_auto_start: bool,
    ) -> anyhow::Result<Self> {
        let mut reconnectable = Reconnectable::new(
            ClientDomainConfig::Unix(unix_dom.clone()),
            None,
            Arc::clone(&admission),
        );
        reconnectable.connect(initial, ui, no_auto_start)?;
        let (client, initial_ready) = Self::new(
            local_domain_id,
            reconnectable,
            admission,
            ConnectionPresentation::Interactive(ui.clone()),
            None,
            ClientId::new(),
        )?;
        block_on(client.await_initial_bootstrap(initial_ready))
    }

    pub fn new_tls(
        admission: Arc<RuntimeAdmission>,
        local_domain_id: DomainId,
        tls_client: &TlsDomainClient,
        ui: &mut ConnectionUI,
    ) -> anyhow::Result<Self> {
        let mut reconnectable = Reconnectable::new(
            ClientDomainConfig::Tls(tls_client.clone()),
            None,
            Arc::clone(&admission),
        );
        let no_auto_start = true;
        reconnectable.connect(true, ui, no_auto_start)?;
        let (client, initial_ready) = Self::new(
            Some(local_domain_id),
            reconnectable,
            admission,
            ConnectionPresentation::Interactive(ui.clone()),
            None,
            ClientId::new(),
        )?;
        block_on(client.await_initial_bootstrap(initial_ready))
    }

    pub fn new_ssh(
        admission: Arc<RuntimeAdmission>,
        local_domain_id: DomainId,
        ssh_dom: &SshDomain,
        ui: &mut ConnectionUI,
    ) -> anyhow::Result<Self> {
        let mut reconnectable = Reconnectable::new(
            ClientDomainConfig::Ssh(ssh_dom.clone()),
            None,
            Arc::clone(&admission),
        );
        let no_auto_start = true;
        reconnectable.connect(true, ui, no_auto_start)?;
        let (client, initial_ready) = Self::new(
            Some(local_domain_id),
            reconnectable,
            admission,
            ConnectionPresentation::Interactive(ui.clone()),
            None,
            ClientId::new(),
        )?;
        block_on(client.await_initial_bootstrap(initial_ready))
    }

    pub async fn new_headless(
        local_domain_id: Option<DomainId>,
        config: ClientDomainConfig,
        admission: Arc<RuntimeAdmission>,
        lifecycle: &HeadlessConnectionLifecycle,
        expected_build_identity: Option<BuildIdentity>,
        client_id: ClientId,
        initial: bool,
        no_auto_start: bool,
    ) -> anyhow::Result<Self> {
        if !lifecycle.uses_admission(&admission) {
            bail!("headless lifecycle and client must share one runtime admission owner");
        }
        let reconnectable = Reconnectable::new(config, None, Arc::clone(&admission))
            .expect_build_identity(expected_build_identity);
        let (client, initial_ready) = match Self::new(
            local_domain_id,
            reconnectable,
            admission,
            ConnectionPresentation::Headless(lifecycle.reporter()),
            Some(InitialConnectionRequest {
                initial,
                no_auto_start,
            }),
            client_id,
        ) {
            Ok(client) => client,
            Err(error) => {
                let _ = lifecycle.publish_failed(HeadlessConnectionFailure::Runtime);
                return Err(error);
            }
        };
        client.await_initial_bootstrap(initial_ready).await
    }

    pub async fn new_default_unix_domain_headless(
        admission: Arc<RuntimeAdmission>,
        lifecycle: &HeadlessConnectionLifecycle,
        initial: bool,
        no_auto_start: bool,
        prefer_mux: bool,
        class_name: &str,
    ) -> anyhow::Result<Self> {
        let unix = Self::compute_unix_domain(prefer_mux, class_name)?;
        Self::new_unix_domain_headless(
            admission,
            lifecycle,
            None,
            &unix,
            None,
            ClientId::new(),
            initial,
            no_auto_start,
        )
        .await
    }

    pub async fn new_unix_domain_headless(
        admission: Arc<RuntimeAdmission>,
        lifecycle: &HeadlessConnectionLifecycle,
        local_domain_id: Option<DomainId>,
        unix: &UnixDomain,
        expected_build_identity: Option<BuildIdentity>,
        client_id: ClientId,
        initial: bool,
        no_auto_start: bool,
    ) -> anyhow::Result<Self> {
        Self::new_headless(
            local_domain_id,
            ClientDomainConfig::Unix(unix.clone()),
            admission,
            lifecycle,
            expected_build_identity,
            client_id,
            initial,
            no_auto_start,
        )
        .await
    }

    fn prepare_pdu(&self, pdu: Pdu) -> anyhow::Result<PreparedClientRequest> {
        prepare_client_request(&self.sender, &self.admission, pdu)
    }

    fn send_pdu(
        &self,
        pdu: Pdu,
    ) -> impl std::future::Future<Output = anyhow::Result<AdmittedRpcResponse<Pdu>>> + Send + 'static
    {
        // Reserve request capacity before the generated RPC future can be polled.
        // The permit then moves through ReaderMessage and the serial promise map.
        let prepared = self.prepare_pdu(pdu);
        async move { prepared?.execute().await }
    }

    pub async fn resolve_pane_id(&self, pane_id: Option<PaneId>) -> anyhow::Result<PaneId> {
        let pane_id: PaneId = match pane_id {
            Some(p) => p,
            None => {
                if let Ok(pane) = std::env::var("WEZTERM_PANE") {
                    pane.parse()?
                } else {
                    let mut clients = self.list_clients().await?.into_inner().clients;
                    clients.retain(|client| client.focused_pane_id.is_some());
                    clients.sort_by_key(|client| std::cmp::Reverse(client.last_input));
                    if clients.is_empty() {
                        anyhow::bail!(
                            "--pane-id was not specified and $WEZTERM_PANE
                         is not set in the environment, and I couldn't
                         determine which pane was currently focused"
                        );
                    }

                    clients[0]
                        .focused_pane_id
                        .expect("to have filtered out above")
                }
            }
        };
        Ok(pane_id)
    }

    rpc!(ping, Ping = (), Pong);
    pub fn control_lease(
        &self,
        pdu: ControlLeaseRequest,
    ) -> impl std::future::Future<Output = anyhow::Result<AdmittedRpcResponse<ControlLeaseResult>>>
           + Send
           + 'static {
        let start = std::time::Instant::now();
        let request = self.send_pdu(Pdu::ControlLeaseRequest(pdu));
        let control = Arc::clone(&self.control);
        async move {
            let response = request.await;
            let elapsed = start.elapsed();
            metrics::histogram!("rpc", "method" => "control_lease").record(elapsed);
            metrics::counter!("rpc.count", "method" => "control_lease").increment(1);
            match response {
                Ok(response) => response.try_map(move |pdu| match pdu {
                    Pdu::ControlLeaseResult(result) => {
                        let state = match &result {
                            ControlLeaseResult::Acquired(state)
                            | ControlLeaseResult::AlreadyController(state)
                            | ControlLeaseResult::Observing(state)
                            | ControlLeaseResult::Taken(state)
                            | ControlLeaseResult::Released(state)
                            | ControlLeaseResult::NotController(state) => Some(state),
                            ControlLeaseResult::Overloaded => None,
                        };
                        if let Some(state) = state {
                            control.reduce_authoritative_state(state.clone())?;
                        }
                        Ok(result)
                    }
                    unexpected => bail!("unexpected response {unexpected:?}"),
                }),
                Err(error) => Err(error),
            }
        }
    }
    rpc!(service_drain, ServiceDrainRequest, ServiceDrainResult);
    rpc!(list_panes, ListPanes = (), ListPanesResponse);
    rpc!(spawn_v2, SpawnV2, SpawnResponse);
    rpc!(split_pane, SplitPane, SpawnResponse);
    rpc!(
        move_pane_to_new_tab,
        MovePaneToNewTab,
        MovePaneToNewTabResponse
    );
    rpc!(write_to_pane, WriteToPane, UnitResponse);
    rpc!(send_paste, SendPaste, UnitResponse);
    rpc!(key_down, SendKeyDown, UnitResponse);
    rpc!(mouse_event, SendMouseEvent, UnitResponse);
    rpc!(resize, Resize, UnitResponse);
    rpc!(set_zoomed, SetPaneZoomed, UnitResponse);
    rpc!(activate_pane_direction, ActivatePaneDirection, UnitResponse);
    rpc!(
        get_pane_render_changes,
        GetPaneRenderChanges,
        LivenessResponse
    );
    rpc!(get_lines, GetLines, GetLinesResponse);
    rpc!(
        get_dimensions,
        GetPaneRenderableDimensions,
        GetPaneRenderableDimensionsResponse
    );
    rpc!(get_tls_creds, GetTlsCreds = (), GetTlsCredsResponse);
    rpc!(
        search_scrollback,
        SearchScrollbackRequest,
        SearchScrollbackResponse
    );
    rpc!(kill_pane, KillPane, UnitResponse);
    rpc!(list_clients, GetClientList = (), GetClientListResponse);
    rpc!(set_window_workspace, SetWindowWorkspace, UnitResponse);
    rpc!(set_focused_pane_id, SetFocusedPane, UnitResponse);
    rpc!(get_image_cell, GetImageCell, GetImageCellResponse);
    rpc!(set_configured_palette_for_pane, SetPalette, UnitResponse);
    rpc!(set_tab_title, TabTitleChanged, UnitResponse);
    rpc!(set_window_title, WindowTitleChanged, UnitResponse);
    rpc!(rename_workspace, RenameWorkspace, UnitResponse);
    rpc!(erase_scrollback, EraseScrollbackRequest, UnitResponse);
    rpc!(
        get_pane_direction,
        GetPaneDirection,
        GetPaneDirectionResponse
    );
    rpc!(adjust_pane_size, AdjustPaneSize, UnitResponse);
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use std::io::Cursor;
    use std::num::NonZeroU64;
    use std::sync::atomic::AtomicUsize;
    use wezterm_runtime_admission::RuntimeRole;

    fn attachment_identity(value: u64) -> AttachmentIdentity {
        AttachmentIdentity::from_server_sequence(NonZeroU64::new(value).unwrap())
    }

    fn control_state(
        sequence: u64,
        active: impl IntoIterator<Item = (PaneId, AttachmentIdentity)>,
    ) -> ControlLeaseState {
        ControlLeaseState {
            sequence,
            active: active
                .into_iter()
                .map(|(pane_id, controller)| ActiveControlLease {
                    pane_id,
                    controller,
                })
                .collect(),
        }
    }

    fn control_snapshot(
        identity: AttachmentIdentity,
        sequence: u64,
        active: impl IntoIterator<Item = (PaneId, AttachmentIdentity)>,
    ) -> Pdu {
        Pdu::ControlSnapshot(ControlSnapshot {
            attachment_identity: identity,
            state: control_state(sequence, active),
        })
    }

    fn control_change(
        sequence: u64,
        active: impl IntoIterator<Item = (PaneId, AttachmentIdentity)>,
    ) -> Pdu {
        Pdu::ControlChanged(ControlChanged {
            state: control_state(sequence, active),
        })
    }

    #[test]
    fn delayed_baseline_after_live_switch_is_discarded() {
        let tracker = AttachmentControlTracker::default();
        let first = attachment_identity(1);
        let second = attachment_identity(2);
        tracker.begin_connection();
        assert_eq!(
            tracker.reduce(&control_snapshot(first, 5, [(7, first)])),
            Ok(ControlReduction::Applied)
        );
        assert_eq!(
            tracker.reduce(&control_snapshot(second, 6, [(7, second)])),
            Ok(ControlReduction::Applied)
        );
        assert_eq!(
            tracker.reduce(&control_snapshot(first, 5, [(7, second)])),
            Ok(ControlReduction::Discarded)
        );
        assert_eq!(tracker.pane_status(7), PaneControlStatus::Controller);
    }

    #[test]
    fn duplicate_control_change_is_discarded_without_replacing_state() {
        let tracker = AttachmentControlTracker::default();
        let first = attachment_identity(1);
        let second = attachment_identity(2);
        tracker.begin_connection();
        tracker
            .reduce(&control_snapshot(first, 1, std::iter::empty()))
            .unwrap();
        assert_eq!(
            tracker.reduce(&control_change(2, [(7, first)])),
            Ok(ControlReduction::Applied)
        );
        assert_eq!(
            tracker.reduce(&control_change(2, [(7, second)])),
            Ok(ControlReduction::Discarded)
        );
        assert_eq!(tracker.pane_status(7), PaneControlStatus::Controller);
    }

    #[test]
    fn authoritative_rpc_state_may_skip_notifications_and_discards_late_changes() {
        let tracker = AttachmentControlTracker::default();
        let identity = attachment_identity(1);
        tracker.begin_connection();
        tracker
            .reduce(&control_snapshot(identity, 1, std::iter::empty()))
            .unwrap();

        assert_eq!(
            tracker.reduce_authoritative_state(control_state(4, [(7, identity)])),
            Ok(ControlReduction::Applied)
        );
        assert_eq!(tracker.pane_status(7), PaneControlStatus::Controller);
        assert_eq!(
            tracker.reduce(&control_change(2, std::iter::empty())),
            Ok(ControlReduction::Discarded)
        );
        assert_eq!(tracker.pane_status(7), PaneControlStatus::Controller);
    }

    #[test]
    fn forward_control_sequence_gap_fails_the_session() {
        let tracker = AttachmentControlTracker::default();
        let identity = attachment_identity(1);
        tracker.begin_connection();
        tracker
            .reduce(&control_snapshot(identity, 10, std::iter::empty()))
            .unwrap();
        assert_eq!(
            tracker.reduce(&control_change(12, std::iter::empty())),
            Err(ControlTrackingError::SequenceGap {
                expected: 11,
                actual: 12
            })
        );
    }

    #[test]
    fn wrapped_control_sequence_fails_as_overflow() {
        let tracker = AttachmentControlTracker::default();
        let identity = attachment_identity(1);
        tracker.begin_connection();
        tracker
            .reduce(&control_snapshot(identity, u64::MAX, std::iter::empty()))
            .unwrap();
        assert_eq!(
            tracker.reduce(&control_change(0, std::iter::empty())),
            Err(ControlTrackingError::SequenceOverflow {
                current: u64::MAX,
                actual: 0
            })
        );
    }

    #[test]
    fn takeover_updates_typed_controller_status() {
        let tracker = AttachmentControlTracker::default();
        let first = attachment_identity(1);
        let second = attachment_identity(2);
        tracker.begin_connection();
        tracker
            .reduce(&control_snapshot(first, 0, [(7, first)]))
            .unwrap();
        assert_eq!(tracker.pane_status(7), PaneControlStatus::Controller);
        tracker.reduce(&control_change(1, [(7, second)])).unwrap();
        assert_eq!(tracker.pane_status(7), PaneControlStatus::Observer);
        tracker
            .reduce(&control_change(2, std::iter::empty()))
            .unwrap();
        assert_eq!(tracker.pane_status(7), PaneControlStatus::Uncontrolled);
    }

    #[test]
    fn reconnect_preserves_attachment_identity_and_control() {
        let tracker = AttachmentControlTracker::default();
        let identity = attachment_identity(1);
        tracker.begin_connection();
        tracker
            .reduce(&control_snapshot(identity, 9, [(7, identity)]))
            .unwrap();
        assert_eq!(tracker.pane_status(7), PaneControlStatus::Controller);

        tracker.begin_connection();
        assert_eq!(tracker.pane_status(7), PaneControlStatus::AwaitingSnapshot);
        tracker
            .reduce(&control_snapshot(identity, 9, [(7, identity)]))
            .unwrap();
        assert_eq!(tracker.pane_status(7), PaneControlStatus::Controller);
    }

    #[derive(Debug)]
    struct ScriptedBootstrapStream {
        readable: Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl AsyncRead for ScriptedBootstrapStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(std::io::Read::read(&mut self.get_mut().readable, buf))
        }
    }

    impl AsyncWrite for ScriptedBootstrapStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            this.written.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[derive(Clone, Copy)]
    enum FakeConnectBehavior {
        Immediate,
        Fail,
        WaitForCancellation,
    }

    #[derive(Clone, Copy)]
    enum FakeSessionBehavior {
        Complete,
        WaitForCancellation,
        EofThenComplete,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum RuntimeEvent {
        Connect,
        Bootstrap,
        Reattach,
        Session,
    }

    struct FakeRuntimeConnection {
        connect_behavior: FakeConnectBehavior,
        session_behavior: FakeSessionBehavior,
        connects: Arc<AtomicUsize>,
        bootstraps: Arc<AtomicUsize>,
        sessions: Arc<AtomicUsize>,
        bootstrap_client_ids: Arc<Mutex<Vec<ClientId>>>,
        bootstrap_resume_tokens: Arc<Mutex<Vec<Option<AttachmentResumeToken>>>>,
        events: Arc<Mutex<Vec<RuntimeEvent>>>,
        connect_started: Option<Sender<()>>,
        bootstrap_failures_remaining: usize,
    }

    impl FakeRuntimeConnection {
        fn new(
            connect_behavior: FakeConnectBehavior,
            session_behavior: FakeSessionBehavior,
        ) -> Self {
            Self {
                connect_behavior,
                session_behavior,
                connects: Arc::new(AtomicUsize::new(0)),
                bootstraps: Arc::new(AtomicUsize::new(0)),
                sessions: Arc::new(AtomicUsize::new(0)),
                bootstrap_client_ids: Arc::new(Mutex::new(Vec::new())),
                bootstrap_resume_tokens: Arc::new(Mutex::new(Vec::new())),
                events: Arc::new(Mutex::new(Vec::new())),
                connect_started: None,
                bootstrap_failures_remaining: 0,
            }
        }
    }

    impl ClientRuntimeConnection for FakeRuntimeConnection {
        fn connect_for_runtime(
            &mut self,
            _initial: bool,
            _presentation: &dyn ConnectionUi,
            _no_auto_start: bool,
            cancellation: &ClientCancelWaiter,
        ) -> anyhow::Result<()> {
            self.connects.fetch_add(1, Ordering::AcqRel);
            self.events.lock().unwrap().push(RuntimeEvent::Connect);
            if let Some(started) = &self.connect_started {
                let _ = started.try_send(());
            }
            match self.connect_behavior {
                FakeConnectBehavior::Immediate => Ok(()),
                FakeConnectBehavior::Fail => {
                    Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused).into())
                }
                FakeConnectBehavior::WaitForCancellation => {
                    let _ = block_on(cancellation.receiver.recv());
                    Err(ClientCancelled.into())
                }
            }
        }

        fn bootstrap_connected_stream(
            &mut self,
            client_id: &ClientId,
            resume_token: &AttachmentResumeToken,
            _presentation: &dyn ConnectionUi,
            _cancellation: &ClientCancelWaiter,
            _permit: &CountPermit,
        ) -> anyhow::Result<ClientBootstrap> {
            self.bootstraps.fetch_add(1, Ordering::AcqRel);
            self.bootstrap_client_ids
                .lock()
                .unwrap()
                .push(client_id.clone());
            self.bootstrap_resume_tokens
                .lock()
                .unwrap()
                .push(Some(resume_token.clone()));
            self.events.lock().unwrap().push(RuntimeEvent::Bootstrap);
            if self.bootstrap_failures_remaining > 0 {
                self.bootstrap_failures_remaining -= 1;
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
            }
            Ok(ClientBootstrap {
                server_version: GetCodecVersionResponse {
                    codec_vers: CODEC_VERSION,
                    version_string: config::wezterm_version().to_string(),
                    executable_path: PathBuf::new(),
                    config_file_path: None,
                },
                next_serial: 4,
                resume_token: resume_token.clone(),
                control_snapshot: ControlSnapshot {
                    attachment_identity: attachment_identity(1),
                    state: control_state(0, std::iter::empty()),
                },
            })
        }

        fn run_connected_session(
            &mut self,
            _local_domain_id: Option<DomainId>,
            _control: &AttachmentControlTracker,
            _receiver: &mut Receiver<ReaderMessage>,
            cancellation: &ClientCancelWaiter,
            next_serial: u64,
        ) -> anyhow::Result<()> {
            assert_eq!(next_serial, 4);
            self.events.lock().unwrap().push(RuntimeEvent::Session);
            let session = self.sessions.fetch_add(1, Ordering::AcqRel);
            match self.session_behavior {
                FakeSessionBehavior::Complete => Ok(()),
                FakeSessionBehavior::WaitForCancellation => {
                    let _ = block_on(cancellation.receiver.recv());
                    Err(ClientCancelled.into())
                }
                FakeSessionBehavior::EofThenComplete if session == 0 => {
                    Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into())
                }
                FakeSessionBehavior::EofThenComplete => Ok(()),
            }
        }

        fn reconnectable_for_runtime(&self, _presentation: &ConnectionPresentation) -> bool {
            true
        }
    }

    #[derive(Clone, Default)]
    struct FakeRuntimeHost {
        reattachments: Arc<AtomicUsize>,
        cleanups: Arc<AtomicUsize>,
        events: Arc<Mutex<Vec<RuntimeEvent>>>,
    }

    impl ClientRuntimeHost for FakeRuntimeHost {
        fn schedule_reattach(
            &self,
            _domain_id: DomainId,
            presentation: ConnectionPresentation,
            _cancelled: Arc<AtomicBool>,
        ) -> anyhow::Result<()> {
            self.reattachments.fetch_add(1, Ordering::AcqRel);
            self.events.lock().unwrap().push(RuntimeEvent::Reattach);
            presentation
                .publish(HeadlessConnectionState::Ready)
                .map_err(anyhow::Error::from)
        }

        fn schedule_cleanup(&self, _domain_id: DomainId) -> anyhow::Result<()> {
            self.cleanups.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    const ZERO_BACKOFF: ReconnectBackoff = ReconnectBackoff {
        initial: Duration::ZERO,
        maximum: Duration::ZERO,
    };

    fn resume_token(byte: u8) -> AttachmentResumeToken {
        AttachmentResumeToken::from_random_bytes([byte; 32])
    }

    #[test]
    fn stream_bootstrap_sends_the_exact_registration_sequence_once() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let mut readable = Vec::new();
        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
            codec_vers: CODEC_VERSION,
            version_string: "test-version".to_string(),
            executable_path: PathBuf::from("/test/wezterm"),
            config_file_path: None,
        })
        .encode(&mut readable, 1, &admission)
        .unwrap();
        let expected_build_identity = BuildIdentity {
            product: "wezterm".to_string(),
            version: "test-version".to_string(),
            source_revision: None,
            source_dirty: None,
            embedded_wezterm_revision: None,
        };
        Pdu::GetBuildIdentityResponse(GetBuildIdentityResponse {
            identity: expected_build_identity.clone(),
        })
        .encode(&mut readable, 2, &admission)
        .unwrap();
        let issued_resume_token = resume_token(11);
        Pdu::SetClientIdResponse(SetClientIdResponse {
            resume_token: Some(issued_resume_token.clone()),
            control_snapshot: Some(ControlSnapshot {
                attachment_identity: attachment_identity(1),
                state: control_state(0, std::iter::empty()),
            }),
        })
        .encode(&mut readable, 3, &admission)
        .unwrap();

        let mut stream = ScriptedBootstrapStream {
            readable: Cursor::new(readable),
            written: Vec::new(),
        };
        let lifecycle = HeadlessConnectionLifecycle::new(Arc::clone(&admission));
        let reporter = lifecycle.reporter();
        let permit = reserve_client_request(&admission).unwrap();
        let client_id = ClientId::new();
        let bootstrap = block_on(bootstrap_client_stream_async(
            &mut stream,
            &client_id,
            &issued_resume_token,
            &reporter,
            &admission,
            &permit,
            Some(&expected_build_identity),
        ))
        .unwrap();

        assert_eq!(bootstrap.server_version.codec_vers, CODEC_VERSION);
        assert_eq!(bootstrap.next_serial, 4);
        assert_eq!(bootstrap.resume_token, issued_resume_token);
        assert_eq!(
            bootstrap.control_snapshot.attachment_identity,
            attachment_identity(1)
        );
        assert_eq!(bootstrap.control_snapshot.state.sequence, 0);
        let mut written = stream.written.as_slice();
        for (serial, expected_tag) in [
            (1, PduTag::GetCodecVersion),
            (2, PduTag::GetBuildIdentity),
            (3, PduTag::SetClientId),
        ] {
            let decoded = Pdu::decode(
                &mut written,
                DecodeContext::client_to_server_request(ClientRequestPhase::Bootstrap),
                &admission,
            )
            .unwrap();
            assert_eq!(decoded.serial(), serial);
            assert_eq!(decoded.pdu().tag(), Some(expected_tag));
            if let Pdu::SetClientId(registration) = decoded.pdu() {
                assert_eq!(registration.client_id, client_id);
                assert_eq!(
                    registration.resume_token.as_ref(),
                    Some(&issued_resume_token)
                );
            }
        }
        assert!(written.is_empty());
    }

    #[test]
    fn stream_bootstrap_reports_typed_attachment_rejection() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let mut readable = Vec::new();
        Pdu::AttachRejected(AttachRejected {})
            .encode(&mut readable, 0, &admission)
            .unwrap();
        let mut stream = ScriptedBootstrapStream {
            readable: Cursor::new(readable),
            written: Vec::new(),
        };
        let lifecycle = HeadlessConnectionLifecycle::new(Arc::clone(&admission));
        let reporter = lifecycle.reporter();
        let permit = reserve_client_request(&admission).unwrap();

        let error = block_on(bootstrap_client_stream_async(
            &mut stream,
            &ClientId::new(),
            &resume_token(12),
            &reporter,
            &admission,
            &permit,
            None,
        ))
        .err()
        .expect("attachment rejection must fail bootstrap");

        assert!(error.root_cause().is::<AttachmentRejectedError>());
    }

    #[test]
    fn stream_bootstrap_rejects_build_mismatch_before_registration() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let actual = BuildIdentity {
            product: "kit-console".to_string(),
            version: "1.0.0".to_string(),
            source_revision: Some("a".repeat(40)),
            source_dirty: Some(false),
            embedded_wezterm_revision: Some("b".repeat(40)),
        };
        let mut expected = actual.clone();
        expected.source_dirty = Some(true);

        let mut readable = Vec::new();
        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
            codec_vers: CODEC_VERSION,
            version_string: "test-version".to_string(),
            executable_path: PathBuf::from("/test/kit"),
            config_file_path: None,
        })
        .encode(&mut readable, 1, &admission)
        .unwrap();
        Pdu::GetBuildIdentityResponse(GetBuildIdentityResponse {
            identity: actual.clone(),
        })
        .encode(&mut readable, 2, &admission)
        .unwrap();

        let mut stream = ScriptedBootstrapStream {
            readable: Cursor::new(readable),
            written: Vec::new(),
        };
        let lifecycle = HeadlessConnectionLifecycle::new(Arc::clone(&admission));
        let reporter = lifecycle.reporter();
        let permit = reserve_client_request(&admission).unwrap();
        let error = block_on(bootstrap_client_stream_async(
            &mut stream,
            &ClientId::new(),
            &resume_token(13),
            &reporter,
            &admission,
            &permit,
            Some(&expected),
        ))
        .err()
        .expect("mismatched build identity must fail bootstrap");
        let mismatch = error.downcast_ref::<BuildIdentityMismatch>().unwrap();
        assert_eq!(mismatch.expected, expected);
        assert_eq!(mismatch.actual, actual);

        let mut written = stream.written.as_slice();
        for (serial, expected_tag) in [(1, PduTag::GetCodecVersion), (2, PduTag::GetBuildIdentity)]
        {
            let decoded = Pdu::decode(
                &mut written,
                DecodeContext::client_to_server_request(ClientRequestPhase::Bootstrap),
                &admission,
            )
            .unwrap();
            assert_eq!(decoded.serial(), serial);
            assert_eq!(decoded.pdu().tag(), Some(expected_tag));
        }
        assert!(written.is_empty());
    }

    #[test]
    fn bounded_client_request_permits_exhaust_and_release() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let permits = (0..wezterm_runtime_admission::MAX_CLIENT_REQUESTS)
            .map(|_| reserve_client_request(&admission).unwrap())
            .collect::<Vec<_>>();
        assert!(reserve_client_request(&admission).is_err());
        drop(permits);
        assert!(reserve_client_request(&admission).is_ok());
    }

    #[test]
    fn prepared_requests_reserve_capacity_without_being_polled() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let (sender, _receiver) = bounded(wezterm_runtime_admission::MAX_CLIENT_REQUESTS);
        let prepared = (0..wezterm_runtime_admission::MAX_CLIENT_REQUESTS)
            .map(|_| prepare_client_request(&sender, &admission, Pdu::Ping(Ping {})).unwrap())
            .collect::<Vec<_>>();

        assert!(prepare_client_request(&sender, &admission, Pdu::Ping(Ping {})).is_err());
        drop(prepared);
        assert_eq!(admission.count_usage(CountClass::ClientRequest), 0);
    }

    #[test]
    fn prepared_request_carries_its_manifest_expected_response_tag() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let (sender, _receiver) = bounded(1);
        let prepared = prepare_client_request(&sender, &admission, Pdu::Ping(Ping {})).unwrap();
        match prepared.message {
            ReaderMessage::SendPdu {
                expected_response, ..
            } => {
                assert_eq!(expected_response, PduTag::Pong)
            }
            ReaderMessage::Readable => panic!("prepared request became a readability event"),
        }

        assert!(prepare_client_request(
            &sender,
            &admission,
            Pdu::PaneRemoved(PaneRemoved { pane_id: 1 }),
        )
        .is_err());
    }

    #[test]
    fn headless_initial_presentation_is_ui_free_and_admitted() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let lifecycle = HeadlessConnectionLifecycle::new(Arc::clone(&admission));

        lifecycle.publish_attaching().unwrap();
        assert_eq!(admission.count_usage(CountClass::LifecycleEvent), 1);
        assert_eq!(
            lifecycle.try_recv().unwrap(),
            HeadlessConnectionState::Attaching
        );
        assert_eq!(admission.count_usage(CountClass::LifecycleEvent), 0);

        let reporter = lifecycle.reporter();
        ConnectionUi::output(
            &reporter,
            vec![termwiz::surface::Change::Text("not observable".to_string())],
        );
        assert!(matches!(
            lifecycle.try_recv(),
            Err(HeadlessLifecycleError::Empty)
        ));
        assert!(ConnectionUi::input(&reporter, "prompt").is_err());
        assert!(ConnectionUi::password(&reporter, "password").is_err());
    }

    #[test]
    fn relay_eof_is_reconnectable_but_explicit_shutdown_is_terminal() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let lifecycle = HeadlessConnectionLifecycle::new(Arc::clone(&admission));
        let presentation = ConnectionPresentation::Headless(lifecycle.reporter());
        let reconnectable = Reconnectable::new(
            ClientDomainConfig::Unix(UnixDomain::default()),
            None,
            Arc::clone(&admission),
        );
        let eof = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        assert!(should_reconnect(
            &reconnectable,
            &presentation,
            Some(1),
            &eof,
        ));

        let cancelled = anyhow::Error::from(ClientCancelled);
        assert!(!should_reconnect(
            &reconnectable,
            &presentation,
            Some(1),
            &cancelled,
        ));
    }

    #[test]
    fn lost_initial_registration_response_retries_with_the_same_resume_capability() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let lifecycle = HeadlessConnectionLifecycle::new(Arc::clone(&admission));
        let presentation = ConnectionPresentation::Headless(lifecycle.reporter());
        let mut connection = FakeRuntimeConnection::new(
            FakeConnectBehavior::Immediate,
            FakeSessionBehavior::Complete,
        );
        connection.bootstrap_failures_remaining = 1;
        let bootstraps = Arc::clone(&connection.bootstraps);
        let connects = Arc::clone(&connection.connects);
        let bootstrap_resume_tokens = Arc::clone(&connection.bootstrap_resume_tokens);
        let host = FakeRuntimeHost::default();
        let (_sender, receiver) = bounded(1);
        let (_cancellation, waiter) = ClientCancellation::pair();
        let (initial_ready, initial_ready_receiver) = bounded(1);
        let attachment_resume_token = resume_token(20);
        let control = Arc::new(AttachmentControlTracker::default());
        let bootstrap_request_permit = reserve_client_request(&admission).unwrap();

        assert_eq!(
            run_client_runtime(
                connection,
                ClientRuntimeRun {
                    local_domain_id: Some(1),
                    control: Arc::clone(&control),
                    receiver,
                    presentation,
                    cancellation: waiter,
                    initial_connection: None,
                    initial_ready,
                    client_id: ClientId::new(),
                    attachment_resume_token: attachment_resume_token.clone(),
                    initial_server_version: Arc::new(OnceLock::new()),
                    bootstrap_request_permit,
                    host: &host,
                    reconnect_backoff: ZERO_BACKOFF,
                },
            ),
            ClientRuntimeOutcome::Completed
        );
        assert!(block_on(initial_ready_receiver.recv()).unwrap().is_ok());
        assert_eq!(bootstraps.load(Ordering::Acquire), 2);
        assert_eq!(connects.load(Ordering::Acquire), 1);
        assert_eq!(control.pane_status(7), PaneControlStatus::Uncontrolled);
        assert_eq!(
            bootstrap_resume_tokens.lock().unwrap().as_slice(),
            &[
                Some(attachment_resume_token.clone()),
                Some(attachment_resume_token)
            ]
        );
    }

    #[test]
    fn relay_eof_reconnects_once_and_schedules_one_resync() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let lifecycle = HeadlessConnectionLifecycle::new(Arc::clone(&admission));
        let presentation = ConnectionPresentation::Headless(lifecycle.reporter());
        let connection = FakeRuntimeConnection::new(
            FakeConnectBehavior::Immediate,
            FakeSessionBehavior::EofThenComplete,
        );
        let bootstraps = Arc::clone(&connection.bootstraps);
        let sessions = Arc::clone(&connection.sessions);
        let bootstrap_client_ids = Arc::clone(&connection.bootstrap_client_ids);
        let bootstrap_resume_tokens = Arc::clone(&connection.bootstrap_resume_tokens);
        let events = Arc::clone(&connection.events);
        let mut host = FakeRuntimeHost::default();
        host.events = Arc::clone(&events);
        let reattachments = Arc::clone(&host.reattachments);
        let (_sender, receiver) = bounded(1);
        let (_cancellation, waiter) = ClientCancellation::pair();
        let (initial_ready, initial_ready_receiver) = bounded(1);
        let initial_server_version = Arc::new(OnceLock::new());
        let client_id = ClientId::new();
        let attachment_resume_token = resume_token(21);
        let bootstrap_request_permit = reserve_client_request(&admission).unwrap();

        assert_eq!(
            run_client_runtime(
                connection,
                ClientRuntimeRun {
                    local_domain_id: Some(1),
                    control: Arc::new(AttachmentControlTracker::default()),
                    receiver,
                    presentation,
                    cancellation: waiter,
                    initial_connection: None,
                    initial_ready,
                    client_id: client_id.clone(),
                    attachment_resume_token: attachment_resume_token.clone(),
                    initial_server_version: Arc::clone(&initial_server_version),
                    bootstrap_request_permit,
                    host: &host,
                    reconnect_backoff: ZERO_BACKOFF,
                },
            ),
            ClientRuntimeOutcome::Completed
        );
        assert!(block_on(initial_ready_receiver.recv()).unwrap().is_ok());
        assert_eq!(
            initial_server_version.get().unwrap().codec_vers,
            CODEC_VERSION
        );
        assert_eq!(bootstraps.load(Ordering::Acquire), 2);
        assert_eq!(sessions.load(Ordering::Acquire), 2);
        assert_eq!(
            bootstrap_resume_tokens.lock().unwrap().as_slice(),
            &[
                Some(attachment_resume_token.clone()),
                Some(attachment_resume_token)
            ]
        );
        assert_eq!(reattachments.load(Ordering::Acquire), 1);
        assert_eq!(
            bootstrap_client_ids.lock().unwrap().as_slice(),
            &[client_id.clone(), client_id]
        );
        assert_eq!(
            events.lock().unwrap().as_slice(),
            &[
                RuntimeEvent::Bootstrap,
                RuntimeEvent::Session,
                RuntimeEvent::Connect,
                RuntimeEvent::Bootstrap,
                RuntimeEvent::Reattach,
                RuntimeEvent::Session,
            ]
        );
        assert_eq!(
            lifecycle.try_recv().unwrap(),
            HeadlessConnectionState::Reconnecting { attempt: 1 }
        );
        assert_eq!(
            lifecycle.try_recv().unwrap(),
            HeadlessConnectionState::Ready
        );
    }

    #[test]
    fn bounded_headless_reconnects_publish_retry_exhaustion_once() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let lifecycle = HeadlessConnectionLifecycle::with_reconnect_attempt_limit(
            Arc::clone(&admission),
            NonZeroU32::new(2),
        );
        let presentation = ConnectionPresentation::Headless(lifecycle.reporter());
        let connection = FakeRuntimeConnection::new(
            FakeConnectBehavior::Fail,
            FakeSessionBehavior::EofThenComplete,
        );
        let connects = Arc::clone(&connection.connects);
        let host = FakeRuntimeHost::default();
        let cleanups = Arc::clone(&host.cleanups);
        let (_sender, receiver) = bounded(1);
        let (_cancellation, waiter) = ClientCancellation::pair();
        let (initial_ready, initial_ready_receiver) = bounded(1);
        let bootstrap_request_permit = reserve_client_request(&admission).unwrap();

        assert_eq!(
            run_client_runtime(
                connection,
                ClientRuntimeRun {
                    local_domain_id: Some(1),
                    control: Arc::new(AttachmentControlTracker::default()),
                    receiver,
                    presentation,
                    cancellation: waiter,
                    initial_connection: None,
                    initial_ready,
                    client_id: ClientId::new(),
                    attachment_resume_token: resume_token(22),
                    initial_server_version: Arc::new(OnceLock::new()),
                    bootstrap_request_permit,
                    host: &host,
                    reconnect_backoff: ZERO_BACKOFF,
                },
            ),
            ClientRuntimeOutcome::Failed(HeadlessConnectionFailure::RetryExhausted)
        );
        assert!(block_on(initial_ready_receiver.recv()).unwrap().is_ok());
        assert_eq!(connects.load(Ordering::Acquire), 2);
        assert_eq!(cleanups.load(Ordering::Acquire), 1);
        assert_eq!(
            lifecycle.try_recv().unwrap(),
            HeadlessConnectionState::Reconnecting { attempt: 1 }
        );
        assert_eq!(
            lifecycle.try_recv().unwrap(),
            HeadlessConnectionState::Reconnecting { attempt: 2 }
        );
        assert_eq!(
            lifecycle.try_recv().unwrap(),
            HeadlessConnectionState::Failed(HeadlessConnectionFailure::RetryExhausted)
        );
    }

    #[test]
    fn shutdown_interrupts_initial_connect_and_joins_cleanup() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let lifecycle = HeadlessConnectionLifecycle::new(Arc::clone(&admission));
        let presentation = ConnectionPresentation::Headless(lifecycle.reporter());
        let (connect_started, connect_waiter) = bounded(1);
        let mut connection = FakeRuntimeConnection::new(
            FakeConnectBehavior::WaitForCancellation,
            FakeSessionBehavior::WaitForCancellation,
        );
        connection.connect_started = Some(connect_started);
        let host = FakeRuntimeHost::default();
        let cleanups = Arc::clone(&host.cleanups);
        let (_sender, receiver) = bounded(1);
        let (initial_ready, connected) = bounded(1);
        let initial_server_version = Arc::new(OnceLock::new());
        let client_id = ClientId::new();
        let bootstrap_request_permit = reserve_client_request(&admission).unwrap();
        let runtime = ClientRuntime::spawn(&admission, move |cancellation| {
            run_client_runtime(
                connection,
                ClientRuntimeRun {
                    local_domain_id: Some(1),
                    control: Arc::new(AttachmentControlTracker::default()),
                    receiver,
                    presentation,
                    cancellation,
                    initial_connection: Some(InitialConnectionRequest {
                        initial: true,
                        no_auto_start: true,
                    }),
                    initial_ready,
                    client_id,
                    attachment_resume_token: resume_token(23),
                    initial_server_version,
                    bootstrap_request_permit,
                    host: &host,
                    reconnect_backoff: ZERO_BACKOFF,
                },
            )
        })
        .unwrap();

        block_on(connect_waiter.recv()).unwrap();
        assert_eq!(runtime.shutdown_and_join(), ClientRuntimeOutcome::Cancelled);
        assert!(block_on(connected.recv()).unwrap().is_err());
        assert_eq!(cleanups.load(Ordering::Acquire), 1);
        assert_eq!(
            lifecycle.try_recv().unwrap(),
            HeadlessConnectionState::Detached
        );
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn explicit_detach_cancels_and_joins_the_owned_worker() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let runtime = ClientRuntime::spawn(&admission, |cancellation| {
            block_on(cancellation.receiver.recv()).unwrap();
            ClientRuntimeOutcome::Cancelled
        })
        .unwrap();
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 1);
        assert_eq!(runtime.shutdown_and_join(), ClientRuntimeOutcome::Cancelled);
        assert_eq!(runtime.shutdown_and_join(), ClientRuntimeOutcome::Cancelled);
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn shutdown_interrupts_backoff_and_releases_the_runnable_permit() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let runtime = ClientRuntime::spawn(&admission, |cancellation| {
            assert!(!wait_for_reconnect_backoff(
                &cancellation,
                Duration::from_secs(60)
            ));
            ClientRuntimeOutcome::Cancelled
        })
        .unwrap();
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 1);
        assert_eq!(runtime.shutdown_and_join(), ClientRuntimeOutcome::Cancelled);
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn lifecycle_saturation_is_typed_content_free_and_exactly_released() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let lifecycle = HeadlessConnectionLifecycle::new(Arc::clone(&admission));
        for attempt in 0..wezterm_runtime_admission::MAX_LIFECYCLE_EVENTS {
            lifecycle
                .reporter
                .publish(HeadlessConnectionState::Reconnecting {
                    attempt: attempt as u32,
                })
                .unwrap();
        }
        assert_eq!(
            admission.count_usage(CountClass::LifecycleEvent),
            wezterm_runtime_admission::MAX_LIFECYCLE_EVENTS
        );
        assert!(matches!(
            lifecycle.reporter.publish(HeadlessConnectionState::Ready),
            Err(HeadlessLifecycleError::Saturated)
        ));
        for _ in 0..wezterm_runtime_admission::MAX_LIFECYCLE_EVENTS {
            lifecycle.try_recv().unwrap();
        }
        assert_eq!(admission.count_usage(CountClass::LifecycleEvent), 0);
    }

    #[test]
    fn runtime_join_classifies_success_error_and_panic() {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();

        let completed =
            ClientRuntime::spawn(&admission, |_| ClientRuntimeOutcome::Completed).unwrap();
        assert_eq!(
            completed.shutdown_and_join(),
            ClientRuntimeOutcome::Completed
        );

        let failed = ClientRuntime::spawn(&admission, |_| {
            ClientRuntimeOutcome::Failed(HeadlessConnectionFailure::Transport)
        })
        .unwrap();
        assert_eq!(
            failed.shutdown_and_join(),
            ClientRuntimeOutcome::Failed(HeadlessConnectionFailure::Transport)
        );

        let panicked = ClientRuntime::spawn(&admission, |_| panic!("test panic")).unwrap();
        assert_eq!(panicked.shutdown_and_join(), ClientRuntimeOutcome::Panicked);
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }
}
