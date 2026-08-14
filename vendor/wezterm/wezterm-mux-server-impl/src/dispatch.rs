use crate::authorization::ServerPolicy;
use crate::sessionhandler::{PduSender, SessionHandler};
use anyhow::Context;
use async_ossl::AsyncSslStream;
use codec::{AttachmentIdentity, AttachmentResumeToken, Pdu};
use futures::FutureExt;
use mux::{Mux, MuxNotification};
use promise::spawn::MainThreadExecutorHandle;
use smol::prelude::*;
use smol::Async;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fmt;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver as SyncReceiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wezterm_runtime_admission::{
    AdmissionError, AttachmentPermit, CombinedPermit, CountClass, CountPermit, RuntimeAdmission,
    PROTOCOL_BOOTSTRAP_TIMEOUT_MS,
};
use wezterm_uds::UnixStream;

const ATTACHMENT_DISCONNECT_GRACE: Duration = Duration::from_secs(5);

#[cfg(unix)]
pub trait AsRawDesc: std::os::unix::io::AsRawFd + std::os::fd::AsFd {}
#[cfg(windows)]
pub trait AsRawDesc: std::os::windows::io::AsRawSocket + std::os::windows::io::AsSocket {}

impl AsRawDesc for UnixStream {}
impl AsRawDesc for AsyncSslStream {}

#[derive(Debug)]
enum Item {
    Notif(MuxNotification),
    WritePdu { pdu: Box<Pdu>, serial: u64 },
}

#[derive(Debug)]
struct AdmittedItem {
    item: Item,
    _permit: CombinedPermit,
}

enum DispatchEvent {
    Queued(AdmittedItem),
    AttachmentSuperseded,
    PaneOutput,
    Readable,
    Cancelled,
    TaskCompleted,
    InboundTimedOut,
    BootstrapTimedOut,
}

#[derive(Clone, Copy, Debug)]
struct ProtocolDeadlines {
    bootstrap: Option<Instant>,
    inbound: Option<Instant>,
}

impl ProtocolDeadlines {
    fn new(now: Instant) -> Self {
        Self {
            bootstrap: Some(now + Duration::from_millis(PROTOCOL_BOOTSTRAP_TIMEOUT_MS)),
            inbound: None,
        }
    }

    fn mark_inbound_saturated(&mut self, now: Instant) {
        self.inbound
            .get_or_insert(now + Duration::from_millis(PROTOCOL_BOOTSTRAP_TIMEOUT_MS));
    }

    fn task_completed(&mut self) {
        self.inbound = None;
    }

    fn bootstrap_completed(&mut self) {
        self.bootstrap = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionOverflow {
    OutputSaturated,
}

impl fmt::Display for ProjectionOverflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputSaturated => write!(f, "server output projection is saturated"),
        }
    }
}

impl std::error::Error for ProjectionOverflow {}

#[derive(Debug)]
enum DispatchEnqueueError {
    ProjectionOverflow(ProjectionOverflow),
    Closed,
}

impl fmt::Display for DispatchEnqueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectionOverflow(reason) => reason.fmt(f),
            Self::Closed => write!(f, "server output dispatch is closed"),
        }
    }
}

impl std::error::Error for DispatchEnqueueError {}

pub struct AttachmentConnection {
    fence: AttachmentFence,
    registry: Weak<AttachmentRegistry>,
    cancellation: smol::channel::Receiver<()>,
}

impl AttachmentConnection {
    async fn cancelled(&self) {
        let _ = self.cancellation.recv().await;
    }
}

impl Drop for AttachmentConnection {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.detach(self.fence);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AttachmentFence {
    pub(crate) identity: AttachmentIdentity,
    epoch: NonZeroU64,
}

pub(crate) struct EstablishedAttachment {
    pub(crate) fence: AttachmentFence,
    pub(crate) client_id: Arc<mux::client::ClientId>,
    pub(crate) resume_token: AttachmentResumeToken,
    pub(crate) connection: AttachmentConnection,
    pub(crate) is_new: bool,
}

struct AttachmentRecord {
    token: AttachmentResumeToken,
    epoch: NonZeroU64,
    client_id: Arc<mux::client::ClientId>,
    cancellation: Option<smol::channel::Sender<()>>,
}

struct AttachmentRegistryState {
    attachments: BTreeMap<AttachmentIdentity, AttachmentRecord>,
    tokens: BTreeMap<AttachmentResumeToken, AttachmentIdentity>,
}

pub struct AttachmentRegistry {
    admission: Mutex<Option<Arc<RuntimeAdmission>>>,
    next_identity: AtomicU64,
    state: Mutex<AttachmentRegistryState>,
    policy: Mutex<Weak<ServerPolicy>>,
    lifecycle_sender: Mutex<Option<SyncSender<AttachmentLifecycleEvent>>>,
    lifecycle_worker: Mutex<Option<JoinHandle<()>>>,
}

struct AttachmentLifecycleEvent {
    fence: AttachmentFence,
    _permit: CountPermit,
}

enum GraceExpiry {
    Stale,
    Expired {
        identity: AttachmentIdentity,
        client_id: Arc<mux::client::ClientId>,
    },
}

impl AttachmentRegistry {
    pub fn new() -> Arc<Self> {
        let (lifecycle_sender, lifecycle_receiver) =
            sync_channel(wezterm_runtime_admission::MAX_GRACE_TIMERS_TOTAL);
        let registry = Arc::new(Self {
            admission: Mutex::new(None),
            next_identity: AtomicU64::new(1),
            state: Mutex::new(AttachmentRegistryState {
                attachments: BTreeMap::new(),
                tokens: BTreeMap::new(),
            }),
            policy: Mutex::new(Weak::new()),
            lifecycle_sender: Mutex::new(Some(lifecycle_sender)),
            lifecycle_worker: Mutex::new(None),
        });
        let weak = Arc::downgrade(&registry);
        let worker = std::thread::Builder::new()
            .name("wezterm-attachment-lifecycle".to_string())
            .spawn(move || attachment_lifecycle_worker(weak, lifecycle_receiver))
            .expect("attachment lifecycle worker must start");
        *registry.lifecycle_worker.lock().unwrap() = Some(worker);
        registry
    }

    pub(crate) fn bind_policy(&self, policy: Weak<ServerPolicy>) {
        *self.policy.lock().unwrap() = policy;
    }

    pub fn bind_admission(&self, admission: &Arc<RuntimeAdmission>) -> anyhow::Result<()> {
        let mut bound = self.admission.lock().unwrap();
        match bound.as_ref() {
            Some(current) if !Arc::ptr_eq(current, admission) => {
                anyhow::bail!("attachment registry admission must be process-global")
            }
            Some(_) => Ok(()),
            None => {
                *bound = Some(Arc::clone(admission));
                Ok(())
            }
        }
    }

    fn issue_identity(&self) -> anyhow::Result<AttachmentIdentity> {
        let sequence = self
            .next_identity
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("server attachment identity space exhausted"))?;
        let sequence = NonZeroU64::new(sequence)
            .ok_or_else(|| anyhow::anyhow!("server attachment identity space exhausted"))?;
        Ok(AttachmentIdentity::from_server_sequence(sequence))
    }

    pub(crate) fn establish(
        self: &Arc<Self>,
        client_id: Arc<mux::client::ClientId>,
        resume_token: AttachmentResumeToken,
    ) -> anyhow::Result<EstablishedAttachment> {
        let _admission = self
            .admission
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("attachment registry admission is not bound"))?;
        let (cancellation, cancelled) = smol::channel::bounded(1);
        let mut state = self.state.lock().unwrap();
        let (identity, epoch, established_client_id, is_new) =
            if let Some(identity) = state.tokens.get(&resume_token).copied() {
                let record = state
                    .attachments
                    .get(&identity)
                    .ok_or_else(|| anyhow::anyhow!("attachment resume state is unavailable"))?;
                let epoch =
                    NonZeroU64::new(
                        record.epoch.get().checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("attachment connection epoch exhausted")
                        })?,
                    )
                    .expect("a checked non-zero epoch increment remains non-zero");
                (identity, epoch, Arc::clone(&record.client_id), false)
            } else {
                let identity = self.issue_identity()?;
                (identity, NonZeroU64::new(1).unwrap(), client_id, true)
            };
        let fence = AttachmentFence { identity, epoch };
        if is_new {
            state.tokens.insert(resume_token.clone(), identity);
            state.attachments.insert(
                identity,
                AttachmentRecord {
                    token: resume_token.clone(),
                    epoch,
                    client_id: Arc::clone(&established_client_id),
                    cancellation: Some(cancellation),
                },
            );
        } else {
            let record = state
                .attachments
                .get_mut(&identity)
                .expect("validated resume attachment remains present while state is locked");
            if let Some(previous) = record.cancellation.replace(cancellation) {
                previous.close();
            }
            record.epoch = epoch;
        }
        Ok(EstablishedAttachment {
            fence,
            client_id: established_client_id,
            resume_token,
            connection: AttachmentConnection {
                fence,
                registry: Arc::downgrade(self),
                cancellation: cancelled,
            },
            is_new,
        })
    }

    pub(crate) fn is_current(&self, fence: AttachmentFence) -> bool {
        self.state
            .lock()
            .unwrap()
            .attachments
            .get(&fence.identity)
            .is_some_and(|record| record.epoch == fence.epoch)
    }

    fn expire_grace(&self, fence: AttachmentFence) -> GraceExpiry {
        let mut state = self.state.lock().unwrap();
        let Some(record) = state.attachments.get(&fence.identity) else {
            return GraceExpiry::Stale;
        };
        if record.epoch != fence.epoch || record.cancellation.is_some() {
            return GraceExpiry::Stale;
        }
        let record = state
            .attachments
            .remove(&fence.identity)
            .expect("validated attachment remains present while state is locked");
        state.tokens.remove(&record.token);
        GraceExpiry::Expired {
            identity: fence.identity,
            client_id: record.client_id,
        }
    }

    fn expire_grace_and_finalize(&self, fence: AttachmentFence) -> GraceExpiry {
        let expiry = self.expire_grace(fence);
        if let GraceExpiry::Expired {
            identity,
            client_id,
        } = &expiry
        {
            if let Some(mux) = Mux::try_get() {
                mux.unregister_client(client_id);
            }
            if let Some(policy) = self.policy.lock().unwrap().upgrade() {
                policy.attachment_expired(*identity);
            }
        }
        expiry
    }

    fn detach(&self, fence: AttachmentFence) {
        {
            let mut state = self.state.lock().unwrap();
            let Some(record) = state.attachments.get_mut(&fence.identity) else {
                return;
            };
            if record.epoch != fence.epoch {
                return;
            }
            record.cancellation = None;
        }
        let admission = self.admission.lock().unwrap().clone();
        let Some(admission) = admission else {
            return;
        };
        let Ok(permit) = admission.try_count(CountClass::GraceTimer, 1) else {
            let _ = self.expire_grace_and_finalize(fence);
            return;
        };
        let event = AttachmentLifecycleEvent {
            fence,
            _permit: permit,
        };
        let sent = self
            .lifecycle_sender
            .lock()
            .unwrap()
            .as_ref()
            .map(|sender| sender.try_send(event));
        if !matches!(sent, Some(Ok(()))) {
            let _ = self.expire_grace_and_finalize(fence);
        }
    }
}

impl Drop for AttachmentRegistry {
    fn drop(&mut self) {
        self.lifecycle_sender.lock().unwrap().take();
        let worker = self.lifecycle_worker.lock().unwrap().take();
        if let Some(worker) = worker {
            if worker.thread().id() != std::thread::current().id() {
                let _ = worker.join();
            }
        }
    }
}

fn attachment_lifecycle_worker(
    registry: Weak<AttachmentRegistry>,
    receiver: SyncReceiver<AttachmentLifecycleEvent>,
) {
    let mut pending: BTreeMap<AttachmentIdentity, (Instant, AttachmentFence, CountPermit)> =
        BTreeMap::new();
    loop {
        let now = Instant::now();
        let expired: Vec<_> = pending
            .iter()
            .filter_map(|(&identity, (deadline, _, _))| (*deadline <= now).then_some(identity))
            .collect();
        for identity in expired {
            let Some(registry) = registry.upgrade() else {
                return;
            };
            let fence = pending
                .get(&identity)
                .map(|(_, fence, _)| *fence)
                .expect("pending grace identity remains present");
            match registry.expire_grace_and_finalize(fence) {
                GraceExpiry::Stale => {
                    pending.remove(&identity);
                }
                GraceExpiry::Expired { identity, .. } => {
                    pending.remove(&identity);
                }
            }
        }
        let timeout = pending
            .values()
            .map(|(deadline, _, _)| deadline.saturating_duration_since(Instant::now()))
            .min()
            .unwrap_or(Duration::from_secs(60));
        match receiver.recv_timeout(timeout) {
            Ok(event) => {
                pending.insert(
                    event.fence.identity,
                    (
                        Instant::now() + ATTACHMENT_DISCONNECT_GRACE,
                        event.fence,
                        event._permit,
                    ),
                );
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[derive(Default)]
struct PendingPaneOutput {
    pane_ids: VecDeque<(mux::pane::PaneId, CombinedPermit)>,
    members: HashSet<mux::pane::PaneId>,
}

struct DispatchQueue {
    attachment: Arc<AttachmentPermit>,
    reliable_tx: smol::channel::Sender<AdmittedItem>,
    reliable_drain_rx: smol::channel::Receiver<AdmittedItem>,
    pane_output_wake_tx: smol::channel::Sender<()>,
    pane_output: Mutex<PendingPaneOutput>,
    failure: Mutex<Option<ProjectionOverflow>>,
    closed: AtomicBool,
}

struct DispatchReceiver {
    reliable_rx: smol::channel::Receiver<AdmittedItem>,
    pane_output_wake_rx: smol::channel::Receiver<()>,
}

struct DispatchSubscriptionGuard(Arc<DispatchQueue>);

impl Drop for DispatchSubscriptionGuard {
    fn drop(&mut self) {
        self.0.close();
        // `Mux::subscribe` removes a subscriber only when its callback returns
        // false. Prompt one content-free notification so a clean disconnect does
        // not leave the attachment queue retained until unrelated pane activity.
        if let Some(mux) = Mux::try_get() {
            mux.notify(MuxNotification::Empty);
        }
    }
}

impl DispatchQueue {
    fn new(attachment: Arc<AttachmentPermit>) -> (Arc<Self>, DispatchReceiver) {
        let (reliable_tx, reliable_rx) = smol::channel::bounded(
            wezterm_runtime_admission::MAX_SERVER_OUTPUT_ITEMS_PER_ATTACHMENT,
        );
        let (pane_output_wake_tx, pane_output_wake_rx) = smol::channel::bounded(1);
        (
            Arc::new(Self {
                attachment,
                reliable_tx,
                reliable_drain_rx: reliable_rx.clone(),
                pane_output_wake_tx,
                pane_output: Mutex::new(PendingPaneOutput::default()),
                failure: Mutex::new(None),
                closed: AtomicBool::new(false),
            }),
            DispatchReceiver {
                reliable_rx,
                pane_output_wake_rx,
            },
        )
    }

    fn failure(&self) -> Option<ProjectionOverflow> {
        *self.failure.lock().unwrap()
    }

    fn fail(&self, reason: ProjectionOverflow) -> DispatchEnqueueError {
        let mut failure = self.failure.lock().unwrap();
        let reason = *failure.get_or_insert(reason);
        drop(failure);

        // Closing both channels is the non-lossy wakeup. A receiver blocked in either
        // future is woken even when the reliable queue was full or global admission,
        // rather than the local channel, caused the overflow.
        self.close();
        DispatchEnqueueError::ProjectionOverflow(reason)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.reliable_tx.close();
        self.pane_output_wake_tx.close();
        while self.reliable_drain_rx.try_recv().is_ok() {}
        let mut pending = self.pane_output.lock().unwrap();
        pending.members.clear();
        pending.pane_ids.clear();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn try_output_permit(&self) -> Result<CombinedPermit, DispatchEnqueueError> {
        self.attachment
            .try_output()
            .map_err(|_| self.fail(ProjectionOverflow::OutputSaturated))
    }

    fn enqueue_reliable(&self, item: Item) -> Result<(), DispatchEnqueueError> {
        if self.is_closed() {
            return Err(DispatchEnqueueError::Closed);
        }

        let admitted = AdmittedItem {
            item,
            _permit: self.try_output_permit()?,
        };
        match self.reliable_tx.try_send(admitted) {
            Ok(()) => Ok(()),
            Err(smol::channel::TrySendError::Full(_)) => {
                Err(self.fail(ProjectionOverflow::OutputSaturated))
            }
            Err(smol::channel::TrySendError::Closed(_)) => match self.failure() {
                Some(reason) => Err(DispatchEnqueueError::ProjectionOverflow(reason)),
                None => Err(DispatchEnqueueError::Closed),
            },
        }
    }

    fn enqueue_pane_output(&self, pane_id: mux::pane::PaneId) -> Result<(), DispatchEnqueueError> {
        if let Some(reason) = self.failure() {
            return Err(DispatchEnqueueError::ProjectionOverflow(reason));
        }
        if self.is_closed() {
            return Err(DispatchEnqueueError::Closed);
        }

        let permit = self.try_output_permit()?;
        let mut pending = self.pane_output.lock().unwrap();
        if self.is_closed() {
            return Err(DispatchEnqueueError::Closed);
        }
        if pending.members.contains(&pane_id) {
            return Ok(());
        }
        pending.members.insert(pane_id);
        pending.pane_ids.push_back((pane_id, permit));
        drop(pending);

        match self.pane_output_wake_tx.try_send(()) {
            Ok(()) | Err(smol::channel::TrySendError::Full(_)) => Ok(()),
            Err(smol::channel::TrySendError::Closed(_)) => match self.failure() {
                Some(reason) => Err(DispatchEnqueueError::ProjectionOverflow(reason)),
                None => Err(DispatchEnqueueError::Closed),
            },
        }
    }

    fn enqueue_notification(
        &self,
        notification: MuxNotification,
    ) -> Result<(), DispatchEnqueueError> {
        match notification {
            MuxNotification::PaneOutput(pane_id) => self.enqueue_pane_output(pane_id),
            MuxNotification::PaneRemoved(pane_id) => {
                self.discard_pane_output(pane_id);
                self.enqueue_reliable(Item::Notif(MuxNotification::PaneRemoved(pane_id)))
            }
            reliable => self.enqueue_reliable(Item::Notif(reliable)),
        }
    }

    fn discard_pane_output(&self, pane_id: mux::pane::PaneId) {
        let mut pending = self.pane_output.lock().unwrap();
        if pending.members.remove(&pane_id) {
            pending
                .pane_ids
                .retain(|(pending_id, _)| *pending_id != pane_id);
        }
    }

    fn take_pane_output(&self) -> Vec<(mux::pane::PaneId, CombinedPermit)> {
        let mut pending = self.pane_output.lock().unwrap();
        pending.members.clear();
        pending.pane_ids.drain(..).collect()
    }
}

#[derive(Clone, Debug)]
pub struct DispatchCancel {
    sender: smol::channel::Sender<()>,
    receiver: smol::channel::Receiver<()>,
}

impl DispatchCancel {
    pub fn new() -> Self {
        let (sender, receiver) = smol::channel::bounded(1);
        Self { sender, receiver }
    }

    pub fn cancel(&self) {
        self.sender.close();
    }

    pub fn is_cancelled(&self) -> bool {
        self.sender.is_closed()
    }

    async fn cancelled(&self) {
        let _ = self.receiver.recv().await;
    }
}

impl Default for DispatchCancel {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn process<T>(
    stream: T,
    cancel: DispatchCancel,
    policy: Arc<ServerPolicy>,
    attachment: Arc<AttachmentPermit>,
    executor: MainThreadExecutorHandle,
) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: AsRawDesc,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    let stream = smol::Async::new(stream)?;
    process_async(stream, cancel, policy, attachment, executor).await
}

pub async fn process_async<T>(
    mut stream: Async<T>,
    cancel: DispatchCancel,
    policy: Arc<ServerPolicy>,
    attachment: Arc<AttachmentPermit>,
    executor: MainThreadExecutorHandle,
) -> anyhow::Result<()>
where
    T: 'static,
    T: std::io::Read,
    T: std::io::Write,
    T: std::fmt::Debug,
    T: async_io::IoSafe,
{
    log::trace!("process_async called");

    let admission = Arc::clone(attachment.admission());
    let (dispatch, dispatch_rx) = DispatchQueue::new(Arc::clone(&attachment));

    let pdu_sender = PduSender::new({
        let dispatch = Arc::clone(&dispatch);
        move |pdu, serial| {
            dispatch
                .enqueue_reliable(Item::WritePdu {
                    pdu: Box::new(pdu),
                    serial,
                })
                .map_err(Into::into)
        }
    });
    let mut handler = SessionHandler::new(pdu_sender, policy, Arc::clone(&admission), executor)?;
    let mut attachment_connection: Option<AttachmentConnection> = None;

    {
        let mux = Mux::get();
        let dispatch = Arc::clone(&dispatch);
        mux.subscribe(
            move |notification| match dispatch.enqueue_notification(notification) {
                Ok(()) => true,
                Err(DispatchEnqueueError::ProjectionOverflow(reason)) => {
                    log::warn!("disconnecting saturated mux projection: {reason}");
                    false
                }
                Err(DispatchEnqueueError::Closed) => false,
            },
        );
    }
    let _subscription = DispatchSubscriptionGuard(Arc::clone(&dispatch));

    let mut deadlines = ProtocolDeadlines::new(Instant::now());
    let result: anyhow::Result<()> = async {
        loop {
            if let Some(reason) = dispatch.failure() {
                return Err(reason.into());
            }

            let rx_msg = dispatch_rx.reliable_rx.recv().map(|result| {
                result
                    .map(DispatchEvent::Queued)
                    .map_err(anyhow::Error::from)
            });
            let pane_output = dispatch_rx.pane_output_wake_rx.recv().map(|result| {
                result
                    .map(|()| DispatchEvent::PaneOutput)
                    .map_err(anyhow::Error::from)
            });
            let attachment_superseded = async {
                match attachment_connection.as_ref() {
                    Some(connection) => {
                        connection.cancelled().await;
                        Ok(DispatchEvent::AttachmentSuperseded)
                    }
                    None => futures::future::pending::<anyhow::Result<DispatchEvent>>().await,
                }
            };
            let wait_for_read = async {
                if deadlines.inbound.is_some() {
                    return futures::future::pending::<anyhow::Result<DispatchEvent>>().await;
                }
                stream
                    .readable()
                    .await
                    .map(|_| DispatchEvent::Readable)
                    .map_err(anyhow::Error::from)
            };
            let wait_for_cancel = cancel
                .cancelled()
                .map(|()| Ok::<DispatchEvent, anyhow::Error>(DispatchEvent::Cancelled));
            let wait_for_task = handler
                .wait_for_task()
                .map(|result| result.map(|()| DispatchEvent::TaskCompleted));
            let deadline = deadlines.inbound;
            let wait_for_inbound_deadline = async move {
                match deadline {
                    Some(deadline) => {
                        smol::Timer::after(deadline.saturating_duration_since(Instant::now()))
                            .await;
                        Ok::<DispatchEvent, anyhow::Error>(DispatchEvent::InboundTimedOut)
                    }
                    None => futures::future::pending::<anyhow::Result<DispatchEvent>>().await,
                }
            };
            let deadline = deadlines.bootstrap;
            let wait_for_bootstrap_deadline = async move {
                match deadline {
                    Some(deadline) => {
                        smol::Timer::after(deadline.saturating_duration_since(Instant::now()))
                            .await;
                        Ok::<DispatchEvent, anyhow::Error>(DispatchEvent::BootstrapTimedOut)
                    }
                    None => futures::future::pending::<anyhow::Result<DispatchEvent>>().await,
                }
            };

            match smol::future::or(
                wait_for_cancel,
                smol::future::or(
                    wait_for_bootstrap_deadline,
                    smol::future::or(
                        wait_for_inbound_deadline,
                        smol::future::or(
                            wait_for_task,
                            smol::future::or(
                                smol::future::or(
                                    smol::future::or(rx_msg, pane_output),
                                    attachment_superseded,
                                ),
                                wait_for_read,
                            ),
                        ),
                    ),
                ),
            )
            .await
            {
                Ok(DispatchEvent::Cancelled) => return Ok(()),
                Ok(DispatchEvent::PaneOutput) => {
                    for (pane_id, _output_permit) in dispatch.take_pane_output() {
                        handler.schedule_pane_push(pane_id)?;
                    }
                }
                Ok(DispatchEvent::TaskCompleted) => {
                    deadlines.task_completed();
                }
                Ok(DispatchEvent::AttachmentSuperseded) => return Ok(()),
                Ok(DispatchEvent::InboundTimedOut) => {
                    return Err(anyhow::anyhow!(
                        "inbound request admission did not drain before its deadline"
                    ));
                }
                Ok(DispatchEvent::BootstrapTimedOut) => {
                    return Err(anyhow::anyhow!(
                        "client protocol bootstrap did not complete before its deadline"
                    ));
                }
                Ok(DispatchEvent::Readable) => {
                    let inbound = match attachment.try_inbound() {
                        Ok(inbound) => inbound,
                        Err(AdmissionError::CapacityExceeded { .. }) => {
                            deadlines.mark_inbound_saturated(Instant::now());
                            continue;
                        }
                        Err(err) => return Err(err.into()),
                    };
                    let request_phase = handler.client_request_phase();
                    let decoded = match async {
                        let header = Pdu::read_header_async(&mut stream).await?;
                        let body = header.validate(
                            codec::DecodeContext::client_to_server_request(request_phase),
                            &admission,
                        )?;
                        Pdu::decode_body_async(&mut stream, body, &admission).await
                    }
                    .await
                    {
                        Ok(data) => data,
                        Err(err) => {
                            if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                                if err.kind() == std::io::ErrorKind::UnexpectedEof {
                                    // Client disconnected: no need to make a noise
                                    return Ok(());
                                }
                            }
                            return Err(err).context("reading Pdu from client");
                        }
                    };
                    match handler.admit_request(decoded, inbound) {
                        Ok(request) => handler.process_one(request)?,
                        Err(rejected) => handler.reject_request(rejected)?,
                    }
                    if handler.client_request_phase() == codec::ClientRequestPhase::Established {
                        deadlines.bootstrap_completed();
                    }
                }
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::WritePdu { pdu, serial },
                    _permit: _output_permit,
                })) => {
                    let activates_attachment = matches!(
                        pdu.as_ref(),
                        Pdu::SetClientIdResponse(response)
                            if response.resume_token.is_some()
                    );
                    match pdu.encode_async(&mut stream, serial, &admission).await {
                        Ok(()) => {}
                        Err(err) => {
                            if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
                                if err.kind() == std::io::ErrorKind::BrokenPipe {
                                    // Client disconnected: no need to make a noise
                                    return Ok(());
                                }
                            }
                            return Err(err).context("encoding PDU to client");
                        }
                    };
                    match stream.flush().await {
                        Ok(()) => {}
                        Err(err) => {
                            if err.kind() == std::io::ErrorKind::BrokenPipe {
                                // Client disconnected: no need to make a noise
                                return Ok(());
                            }
                            return Err(err).context("flushing PDU to client");
                        }
                    }
                    if activates_attachment {
                        let connection = handler.take_attachment_connection().ok_or_else(|| {
                            anyhow::anyhow!(
                                "attachment bootstrap completed without its connection guard"
                            )
                        })?;
                        if attachment_connection.replace(connection).is_some() {
                            return Err(anyhow::anyhow!(
                                "connection attempted to replace its attachment guard"
                            ));
                        }
                    }
                }
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::PaneOutput(_)),
                    _permit: _output_permit,
                })) => unreachable!("pane output bypassed coalescing"),
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::PaneAdded(_pane_id)),
                    _permit: _output_permit,
                })) => {}
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::PaneRemoved(pane_id)),
                    _permit: _output_permit,
                })) => {
                    handler.remove_pane_projection(pane_id);
                    Pdu::PaneRemoved(codec::PaneRemoved { pane_id })
                        .encode_async(&mut stream, 0, &admission)
                        .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::Alert { pane_id, alert }),
                    _permit: _output_permit,
                })) => {
                    handler.schedule_pane_alert(pane_id, alert)?;
                }
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::SaveToDownloads { .. }),
                    _permit: _output_permit,
                })) => {}
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item:
                        Item::Notif(MuxNotification::AssignClipboard {
                            pane_id,
                            selection,
                            clipboard,
                        }),
                    _permit: _output_permit,
                })) => {
                    Pdu::SetClipboard(codec::SetClipboard {
                        pane_id,
                        clipboard,
                        selection,
                    })
                    .encode_async(&mut stream, 0, &admission)
                    .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::TabAddedToWindow { tab_id, window_id }),
                    _permit: _output_permit,
                })) => {
                    Pdu::TabAddedToWindow(codec::TabAddedToWindow { tab_id, window_id })
                        .encode_async(&mut stream, 0, &admission)
                        .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::WindowRemoved(_window_id)),
                    _permit: _output_permit,
                })) => {}
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::WindowCreated(_window_id)),
                    _permit: _output_permit,
                })) => {}
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::WindowInvalidated(_window_id)),
                    _permit: _output_permit,
                })) => {}
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::WindowWorkspaceChanged(window_id)),
                    _permit: _output_permit,
                })) => {
                    let workspace = {
                        let mux = Mux::get();
                        mux.get_window(window_id)
                            .map(|w| w.get_workspace().to_string())
                    };
                    if let Some(workspace) = workspace {
                        Pdu::WindowWorkspaceChanged(codec::WindowWorkspaceChanged {
                            window_id,
                            workspace,
                        })
                        .encode_async(&mut stream, 0, &admission)
                        .await?;
                        stream.flush().await.context("flushing PDU to client")?;
                    }
                }
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::PaneFocused(pane_id)),
                    _permit: _output_permit,
                })) => {
                    Pdu::PaneFocused(codec::PaneFocused { pane_id })
                        .encode_async(&mut stream, 0, &admission)
                        .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::TabResized(tab_id)),
                    _permit: _output_permit,
                })) => {
                    Pdu::TabResized(codec::TabResized { tab_id })
                        .encode_async(&mut stream, 0, &admission)
                        .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::TabTitleChanged { tab_id, title }),
                    _permit: _output_permit,
                })) => {
                    Pdu::TabTitleChanged(codec::TabTitleChanged { tab_id, title })
                        .encode_async(&mut stream, 0, &admission)
                        .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::WindowTitleChanged { window_id, title }),
                    _permit: _output_permit,
                })) => {
                    Pdu::WindowTitleChanged(codec::WindowTitleChanged { window_id, title })
                        .encode_async(&mut stream, 0, &admission)
                        .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item:
                        Item::Notif(MuxNotification::WorkspaceRenamed {
                            old_workspace,
                            new_workspace,
                        }),
                    _permit: _output_permit,
                })) => {
                    Pdu::RenameWorkspace(codec::RenameWorkspace {
                        old_workspace,
                        new_workspace,
                    })
                    .encode_async(&mut stream, 0, &admission)
                    .await?;
                    stream.flush().await.context("flushing PDU to client")?;
                }
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::ActiveWorkspaceChanged(_)),
                    _permit: _output_permit,
                })) => {}
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::Notif(MuxNotification::Empty),
                    _permit: _output_permit,
                })) => {}
                Err(err) => {
                    if let Some(reason) = dispatch.failure() {
                        return Err(reason.into());
                    }
                    return Err(err).context("waiting for the next dispatch event");
                }
            }
        }
    }
    .await;

    let cleanup = handler.cancel_and_join_tasks().await;
    dispatch.close();
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(err), Ok(())) => Err(err),
        (Ok(()), Err(cleanup)) => Err(cleanup.context("joining dispatch-owned tasks")),
        (Err(err), Err(cleanup)) => Err(err.context(format!(
            "dispatch cleanup also failed while joining owned tasks: {cleanup:#}"
        ))),
    }
}

#[cfg(test)]
mod dispatch_queue_tests {
    use super::*;
    use wezterm_runtime_admission::{
        CountClass, RuntimeAdmission, RuntimeRole, MAX_ATTACHMENTS,
        MAX_INBOUND_REQUESTS_PER_ATTACHMENT, MAX_INBOUND_REQUESTS_TOTAL,
        MAX_SERVER_OUTPUT_ITEMS_PER_ATTACHMENT, MAX_SERVER_OUTPUT_ITEMS_TOTAL,
    };

    fn queue() -> (Arc<RuntimeAdmission>, Arc<DispatchQueue>, DispatchReceiver) {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let attachment = Arc::new(admission.try_attachment().unwrap());
        let (queue, receiver) = DispatchQueue::new(attachment);
        (admission, queue, receiver)
    }

    fn attachment_registry() -> (Arc<RuntimeAdmission>, Arc<AttachmentRegistry>) {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let registry = AttachmentRegistry::new();
        registry.bind_admission(&admission).unwrap();
        (admission, registry)
    }

    fn attachment(registry: &Arc<AttachmentRegistry>) -> EstablishedAttachment {
        static NEXT_TEST_TOKEN: AtomicU64 = AtomicU64::new(1);
        let sequence = NEXT_TEST_TOKEN.fetch_add(1, Ordering::Relaxed);
        let mut token = [0u8; 32];
        token[..8].copy_from_slice(&sequence.to_le_bytes());
        registry
            .establish(
                Arc::new(mux::client::ClientId::new()),
                AttachmentResumeToken::from_random_bytes(token),
            )
            .unwrap()
    }

    #[test]
    fn lost_registration_response_resumes_identity_and_fences_the_old_connection() {
        let (_admission, registry) = attachment_registry();
        let original = attachment(&registry);
        let original_identity = original.fence.identity;
        let original_fence = original.fence;
        let resume_token = original.resume_token.clone();
        assert!(original.is_new);
        assert!(registry.is_current(original_fence));

        let reconnected = registry
            .establish(Arc::new(mux::client::ClientId::new()), resume_token.clone())
            .unwrap();

        assert!(!reconnected.is_new);
        assert_eq!(reconnected.resume_token, resume_token);
        assert_eq!(reconnected.fence.identity, original_identity);
        assert!(!registry.is_current(original_fence));
        assert!(registry.is_current(reconnected.fence));
        smol::block_on(original.connection.cancelled());
    }

    #[test]
    fn attachment_identity_exhaustion_is_permanently_terminal() {
        let (_admission, registry) = attachment_registry();
        registry.next_identity.store(u64::MAX, Ordering::Relaxed);

        assert!(registry.issue_identity().is_err());
        assert!(registry.issue_identity().is_err());
        assert_eq!(registry.next_identity.load(Ordering::Relaxed), u64::MAX);
    }

    fn repeated_pane_output_uses_one_slot_and_one_wake() {
        let (admission, queue, receiver) = queue();

        queue
            .enqueue_notification(MuxNotification::PaneOutput(7))
            .unwrap();
        queue
            .enqueue_notification(MuxNotification::PaneOutput(7))
            .unwrap();

        assert_eq!(admission.count_usage(CountClass::ServerOutput), 1);
        assert_eq!(receiver.pane_output_wake_rx.try_recv(), Ok(()));
        assert!(receiver.pane_output_wake_rx.try_recv().is_err());
        assert_eq!(
            queue
                .take_pane_output()
                .into_iter()
                .map(|(pane_id, _)| pane_id)
                .collect::<Vec<_>>(),
            vec![7]
        );
        assert_eq!(admission.count_usage(CountClass::ServerOutput), 0);
    }

    #[test]
    fn pane_output_can_be_queued_again_after_the_prior_batch_is_taken() {
        let (_admission, queue, receiver) = queue();

        queue.enqueue_pane_output(11).unwrap();
        assert_eq!(receiver.pane_output_wake_rx.try_recv(), Ok(()));
        assert_eq!(
            queue
                .take_pane_output()
                .into_iter()
                .map(|(pane_id, _)| pane_id)
                .collect::<Vec<_>>(),
            vec![11]
        );

        queue.enqueue_pane_output(11).unwrap();
        assert_eq!(receiver.pane_output_wake_rx.try_recv(), Ok(()));
        assert_eq!(
            queue
                .take_pane_output()
                .into_iter()
                .map(|(pane_id, _)| pane_id)
                .collect::<Vec<_>>(),
            vec![11]
        );
    }

    #[test]
    fn pane_removal_discards_coalesced_output_for_only_that_pane() {
        let (admission, queue, receiver) = queue();

        queue.enqueue_pane_output(7).unwrap();
        queue.enqueue_pane_output(8).unwrap();
        queue
            .enqueue_notification(MuxNotification::PaneRemoved(7))
            .unwrap();

        assert_eq!(receiver.pane_output_wake_rx.try_recv(), Ok(()));
        let remaining = queue.take_pane_output();
        assert_eq!(
            remaining
                .iter()
                .map(|(pane_id, _)| *pane_id)
                .collect::<Vec<_>>(),
            vec![8]
        );
        let removed = receiver.reliable_rx.try_recv().unwrap();
        assert!(matches!(
            removed.item,
            Item::Notif(MuxNotification::PaneRemoved(7))
        ));
        drop(remaining);
        drop(removed);
        assert_eq!(admission.count_usage(CountClass::ServerOutput), 0);
    }

    #[test]
    fn reliable_notifications_preserve_exact_fifo_order() {
        let (_admission, queue, receiver) = queue();

        queue
            .enqueue_notification(MuxNotification::PaneRemoved(3))
            .unwrap();
        queue
            .enqueue_notification(MuxNotification::TabResized(5))
            .unwrap();
        queue
            .enqueue_notification(MuxNotification::PaneFocused(8))
            .unwrap();

        let mut observed = vec![];
        for _ in 0..3 {
            let admitted = receiver.reliable_rx.try_recv().unwrap();
            observed.push(match admitted.item {
                Item::Notif(MuxNotification::PaneRemoved(pane_id)) => ("pane-removed", pane_id),
                Item::Notif(MuxNotification::TabResized(tab_id)) => ("tab-resized", tab_id),
                Item::Notif(MuxNotification::PaneFocused(pane_id)) => ("pane-focused", pane_id),
                other => panic!("unexpected reliable item: {:?}", other),
            });
        }

        assert_eq!(
            observed,
            vec![("pane-removed", 3), ("tab-resized", 5), ("pane-focused", 8)]
        );
    }

    #[test]
    fn dequeued_output_keeps_its_permit_until_delivery_finishes() {
        let (admission, queue, receiver) = queue();
        queue
            .enqueue_notification(MuxNotification::PaneRemoved(3))
            .unwrap();

        let admitted = receiver.reliable_rx.try_recv().unwrap();
        assert_eq!(admission.count_usage(CountClass::ServerOutput), 1);
        assert!(matches!(
            &admitted.item,
            Item::Notif(MuxNotification::PaneRemoved(3))
        ));
        assert_eq!(admission.count_usage(CountClass::ServerOutput), 1);
        drop(admitted);
        assert_eq!(admission.count_usage(CountClass::ServerOutput), 0);
    }

    #[test]
    fn saturation_records_typed_overflow_and_closes_both_wake_lanes() {
        let (_admission, queue, receiver) = queue();

        for pane_id in 0..MAX_SERVER_OUTPUT_ITEMS_PER_ATTACHMENT {
            queue
                .enqueue_notification(MuxNotification::PaneRemoved(pane_id))
                .unwrap();
        }

        assert!(matches!(
            queue.enqueue_notification(MuxNotification::PaneRemoved(
                MAX_SERVER_OUTPUT_ITEMS_PER_ATTACHMENT
            )),
            Err(DispatchEnqueueError::ProjectionOverflow(
                ProjectionOverflow::OutputSaturated
            ))
        ));
        assert_eq!(queue.failure(), Some(ProjectionOverflow::OutputSaturated));
        assert!(receiver.reliable_rx.is_closed());
        assert!(receiver.pane_output_wake_rx.is_closed());
    }

    #[test]
    fn pane_output_coalescing_does_not_hide_reliable_overflow() {
        let (_admission, queue, receiver) = queue();

        queue.enqueue_pane_output(usize::MAX).unwrap();
        for pane_id in 0..MAX_SERVER_OUTPUT_ITEMS_PER_ATTACHMENT - 1 {
            queue
                .enqueue_notification(MuxNotification::PaneRemoved(pane_id))
                .unwrap();
        }

        assert!(matches!(
            queue.enqueue_notification(MuxNotification::TabResized(99)),
            Err(DispatchEnqueueError::ProjectionOverflow(
                ProjectionOverflow::OutputSaturated
            ))
        ));
        assert!(receiver.reliable_rx.is_closed());
        assert!(receiver.pane_output_wake_rx.is_closed());
    }

    #[test]
    fn every_attachment_and_aggregate_output_slot_is_exactly_released() {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let mut queues = Vec::new();

        for attachment_idx in 0..MAX_ATTACHMENTS {
            let attachment = Arc::new(admission.try_attachment().unwrap());
            let (queue, receiver) = DispatchQueue::new(attachment);
            for item_idx in 0..MAX_SERVER_OUTPUT_ITEMS_PER_ATTACHMENT {
                let pane_id = attachment_idx * MAX_SERVER_OUTPUT_ITEMS_PER_ATTACHMENT + item_idx;
                queue
                    .enqueue_notification(MuxNotification::PaneRemoved(pane_id))
                    .unwrap();
            }
            queues.push((queue, receiver));
        }

        assert_eq!(
            admission.count_usage(CountClass::ServerOutput),
            MAX_SERVER_OUTPUT_ITEMS_TOTAL
        );
        assert!(admission.try_attachment().is_err());

        for (queue, receiver) in queues {
            queue.close();
            drop(receiver);
            drop(queue);
        }
        assert_eq!(admission.count_usage(CountClass::ServerOutput), 0);
        assert_eq!(admission.count_usage(CountClass::Attachment), 0);
    }

    #[test]
    fn inbound_admission_stops_at_each_attachment_and_aggregate_boundary() {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let mut attachments = Vec::new();
        let mut permits = Vec::new();

        for _ in 0..MAX_ATTACHMENTS {
            let attachment = admission.try_attachment().unwrap();
            for _ in 0..MAX_INBOUND_REQUESTS_PER_ATTACHMENT {
                permits.push(attachment.try_inbound().unwrap());
            }
            assert!(attachment.try_inbound().is_err());
            attachments.push(attachment);
        }

        assert_eq!(
            admission.count_usage(CountClass::InboundRequest),
            MAX_INBOUND_REQUESTS_TOTAL
        );
        drop(permits);
        assert_eq!(admission.count_usage(CountClass::InboundRequest), 0);
        drop(attachments);
        assert_eq!(admission.count_usage(CountClass::Attachment), 0);
    }

    #[test]
    fn explicit_close_refuses_new_work_and_releases_queued_permits() {
        let (admission, queue, receiver) = queue();
        queue.enqueue_pane_output(1).unwrap();
        queue
            .enqueue_notification(MuxNotification::PaneRemoved(2))
            .unwrap();
        assert_eq!(admission.count_usage(CountClass::ServerOutput), 2);

        queue.close();
        assert!(matches!(
            queue.enqueue_notification(MuxNotification::PaneRemoved(3)),
            Err(DispatchEnqueueError::Closed)
        ));
        assert_eq!(admission.count_usage(CountClass::ServerOutput), 0);
        drop(receiver);
        assert_eq!(admission.count_usage(CountClass::ServerOutput), 0);
        drop(queue);
    }

    #[test]
    fn cancellation_wakes_every_waiter_without_a_polling_timer() {
        let cancel = DispatchCancel::new();
        let waiter_a = cancel.clone();
        let waiter_b = cancel.clone();
        let joined_a = std::thread::spawn(move || smol::block_on(waiter_a.cancelled()));
        let joined_b = std::thread::spawn(move || smol::block_on(waiter_b.cancelled()));

        cancel.cancel();
        joined_a.join().unwrap();
        joined_b.join().unwrap();
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn completed_request_task_does_not_extend_or_clear_bootstrap_deadline() {
        let now = Instant::now();
        let mut deadlines = ProtocolDeadlines::new(now);
        let bootstrap = deadlines.bootstrap;

        deadlines.mark_inbound_saturated(now);
        assert!(deadlines.inbound.is_some());
        deadlines.task_completed();

        assert_eq!(deadlines.bootstrap, bootstrap);
        assert!(deadlines.inbound.is_none());
    }

    #[test]
    fn established_identity_clears_only_the_bootstrap_deadline() {
        let now = Instant::now();
        let mut deadlines = ProtocolDeadlines::new(now);
        deadlines.mark_inbound_saturated(now);
        let inbound = deadlines.inbound;

        deadlines.bootstrap_completed();

        assert!(deadlines.bootstrap.is_none());
        assert_eq!(deadlines.inbound, inbound);
    }
}
