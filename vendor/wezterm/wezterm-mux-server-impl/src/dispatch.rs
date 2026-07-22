use crate::authorization::ServerPolicy;
use crate::sessionhandler::{PduSender, SessionHandler};
use anyhow::Context;
use async_ossl::AsyncSslStream;
use codec::{
    ActiveControlLease, ConnectionIdentity, ControlChanged, ControlLeaseAction, ControlLeaseResult,
    ControlLeaseState, ControlSnapshot, Pdu,
};
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
    MAX_CONTROL_NOTIFICATIONS_PER_ATTACHMENT, MAX_PANES, PROTOCOL_BOOTSTRAP_TIMEOUT_MS,
};
use wezterm_uds::UnixStream;

const CONTROL_DISCONNECT_GRACE: Duration = Duration::from_secs(5);

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
    Control(ControlPublication),
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

#[derive(Debug)]
enum ControlPublication {
    Snapshot(ControlSnapshot),
    Changed(ControlChanged),
}

#[derive(Debug)]
struct AdmittedControlPublication {
    publication: ControlPublication,
    _permit: CountPermit,
}

pub struct ControlSubscription {
    identity: ConnectionIdentity,
    publisher: Weak<ControlPublisher>,
    receiver: smol::channel::Receiver<AdmittedControlPublication>,
}

impl ControlSubscription {
    pub fn identity(&self) -> ConnectionIdentity {
        self.identity
    }

    async fn recv(&self) -> Result<ControlPublication, smol::channel::RecvError> {
        self.receiver
            .recv()
            .await
            .map(|delivery| delivery.publication)
    }
}

impl Drop for ControlSubscription {
    fn drop(&mut self) {
        if let Some(publisher) = self.publisher.upgrade() {
            publisher.detach(self.identity);
        }
    }
}

struct ControlPublisherState {
    sequence: u64,
    active: BTreeMap<mux::pane::PaneId, ConnectionIdentity>,
    subscribers: BTreeMap<ConnectionIdentity, smol::channel::Sender<AdmittedControlPublication>>,
}

impl ControlPublisherState {
    fn snapshot(&self) -> ControlLeaseState {
        ControlLeaseState {
            sequence: self.sequence,
            active: self
                .active
                .iter()
                .map(|(&pane_id, &controller)| ActiveControlLease {
                    pane_id,
                    controller,
                })
                .collect(),
        }
    }
}

pub struct ControlPublisher {
    admission: Mutex<Option<Arc<RuntimeAdmission>>>,
    next_identity: AtomicU64,
    state: Mutex<ControlPublisherState>,
    lifecycle_sender: Mutex<Option<SyncSender<ControlLifecycleEvent>>>,
    lifecycle_worker: Mutex<Option<JoinHandle<()>>>,
}

struct ControlLifecycleEvent {
    identity: ConnectionIdentity,
    _permit: CountPermit,
}

impl ControlPublisher {
    pub fn new() -> Arc<Self> {
        let (lifecycle_sender, lifecycle_receiver) =
            sync_channel(wezterm_runtime_admission::MAX_GRACE_TIMERS_TOTAL);
        let publisher = Arc::new(Self {
            admission: Mutex::new(None),
            next_identity: AtomicU64::new(1),
            state: Mutex::new(ControlPublisherState {
                sequence: 0,
                active: BTreeMap::new(),
                subscribers: BTreeMap::new(),
            }),
            lifecycle_sender: Mutex::new(Some(lifecycle_sender)),
            lifecycle_worker: Mutex::new(None),
        });
        let weak = Arc::downgrade(&publisher);
        let worker = std::thread::Builder::new()
            .name("wezterm-control-lifecycle".to_string())
            .spawn(move || control_lifecycle_worker(weak, lifecycle_receiver))
            .expect("control lifecycle worker must start");
        *publisher.lifecycle_worker.lock().unwrap() = Some(worker);
        publisher
    }

    pub fn bind_admission(&self, admission: &Arc<RuntimeAdmission>) -> anyhow::Result<()> {
        let mut bound = self.admission.lock().unwrap();
        match bound.as_ref() {
            Some(current) if !Arc::ptr_eq(current, admission) => {
                anyhow::bail!("control publisher admission must be process-global")
            }
            Some(_) => Ok(()),
            None => {
                *bound = Some(Arc::clone(admission));
                Ok(())
            }
        }
    }

    pub fn issue_identity(&self) -> anyhow::Result<ConnectionIdentity> {
        let sequence = self
            .next_identity
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("server connection identity space exhausted"))?;
        let sequence = NonZeroU64::new(sequence)
            .ok_or_else(|| anyhow::anyhow!("server connection identity space exhausted"))?;
        Ok(ConnectionIdentity::from_server_sequence(sequence))
    }

    pub fn subscribe(
        self: &Arc<Self>,
        identity: ConnectionIdentity,
    ) -> anyhow::Result<ControlSubscription> {
        let admission = self
            .admission
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("control publisher admission is not bound"))?;
        let permit = admission.try_count(CountClass::ControlNotificationDelivery, 1)?;
        let (sender, receiver) = smol::channel::bounded(MAX_CONTROL_NOTIFICATIONS_PER_ATTACHMENT);
        let mut state = self.state.lock().unwrap();
        if state.subscribers.contains_key(&identity) {
            anyhow::bail!("connection identity is already subscribed")
        }
        sender
            .try_send(AdmittedControlPublication {
                publication: ControlPublication::Snapshot(ControlSnapshot {
                    attachment_identity: identity,
                    state: state.snapshot(),
                }),
                _permit: permit,
            })
            .map_err(|_| anyhow::anyhow!("initial control snapshot queue is unavailable"))?;
        state.subscribers.insert(identity, sender);
        Ok(ControlSubscription {
            identity,
            publisher: Arc::downgrade(self),
            receiver,
        })
    }

    pub fn is_controller(&self, pane_id: mux::pane::PaneId, identity: ConnectionIdentity) -> bool {
        self.state.lock().unwrap().active.get(&pane_id) == Some(&identity)
    }

    pub fn apply(
        &self,
        pane_id: mux::pane::PaneId,
        identity: ConnectionIdentity,
        action: ControlLeaseAction,
    ) -> ControlLeaseResult {
        let mut state = self.state.lock().unwrap();
        let current = state.active.get(&pane_id).copied();
        match (action, current) {
            (ControlLeaseAction::Acquire, None) => self
                .commit_locked(&mut state, pane_id, Some(identity))
                .map(ControlLeaseResult::Acquired)
                .unwrap_or(ControlLeaseResult::Overloaded),
            (ControlLeaseAction::Acquire, Some(current)) if current == identity => {
                ControlLeaseResult::AlreadyController(state.snapshot())
            }
            (ControlLeaseAction::Acquire, Some(_)) => {
                ControlLeaseResult::Observing(state.snapshot())
            }
            (ControlLeaseAction::Take, Some(current)) if current == identity => {
                ControlLeaseResult::AlreadyController(state.snapshot())
            }
            (ControlLeaseAction::Take, _) => self
                .commit_locked(&mut state, pane_id, Some(identity))
                .map(ControlLeaseResult::Taken)
                .unwrap_or(ControlLeaseResult::Overloaded),
            (ControlLeaseAction::Release, Some(current)) if current == identity => self
                .commit_locked(&mut state, pane_id, None)
                .map(ControlLeaseResult::Released)
                .unwrap_or(ControlLeaseResult::Overloaded),
            (ControlLeaseAction::Release, _) => ControlLeaseResult::NotController(state.snapshot()),
        }
    }

    pub fn remove_pane(&self, pane_id: mux::pane::PaneId) -> bool {
        let mut state = self.state.lock().unwrap();
        if !state.active.contains_key(&pane_id) {
            return true;
        }
        self.commit_locked(&mut state, pane_id, None).is_some()
    }

    pub fn expire_grace(&self, identity: ConnectionIdentity) -> bool {
        let mut state = self.state.lock().unwrap();
        let mut active = state.active.clone();
        active.retain(|_, controller| *controller != identity);
        if active == state.active {
            return true;
        }
        self.commit_active_locked(&mut state, active).is_some()
    }

    fn detach(&self, identity: ConnectionIdentity) {
        self.state.lock().unwrap().subscribers.remove(&identity);
        let admission = self.admission.lock().unwrap().clone();
        let Some(admission) = admission else {
            return;
        };
        let Ok(permit) = admission.try_count(CountClass::GraceTimer, 1) else {
            let _ = self.expire_grace(identity);
            return;
        };
        let event = ControlLifecycleEvent {
            identity,
            _permit: permit,
        };
        let sent = self
            .lifecycle_sender
            .lock()
            .unwrap()
            .as_ref()
            .map(|sender| sender.try_send(event));
        if !matches!(sent, Some(Ok(()))) {
            let _ = self.expire_grace(identity);
        }
    }

    fn commit_locked(
        &self,
        state: &mut ControlPublisherState,
        pane_id: mux::pane::PaneId,
        controller: Option<ConnectionIdentity>,
    ) -> Option<ControlLeaseState> {
        let mut active = state.active.clone();
        match controller {
            Some(controller) => {
                active.insert(pane_id, controller);
            }
            None => {
                active.remove(&pane_id);
            }
        }
        self.commit_active_locked(state, active)
    }

    fn commit_active_locked(
        &self,
        state: &mut ControlPublisherState,
        active: BTreeMap<mux::pane::PaneId, ConnectionIdentity>,
    ) -> Option<ControlLeaseState> {
        if active.len() > MAX_PANES {
            return None;
        }
        let admission = self.admission.lock().unwrap().clone()?;
        let _event = admission.try_count(CountClass::ControlEvent, 1).ok()?;
        state.subscribers.retain(|_, sender| !sender.is_closed());
        if state
            .subscribers
            .values()
            .any(|sender| sender.len() >= MAX_CONTROL_NOTIFICATIONS_PER_ATTACHMENT)
        {
            return None;
        }
        let mut deliveries = Vec::with_capacity(state.subscribers.len());
        for identity in state.subscribers.keys().copied() {
            let permit = admission
                .try_count(CountClass::ControlNotificationDelivery, 1)
                .ok()?;
            deliveries.push((identity, permit));
        }
        let sequence = state.sequence.checked_add(1)?;
        state.active = active;
        state.sequence = sequence;
        let snapshot = state.snapshot();
        for (identity, permit) in deliveries {
            let Some(sender) = state.subscribers.get(&identity) else {
                continue;
            };
            let _ = sender.try_send(AdmittedControlPublication {
                publication: ControlPublication::Changed(ControlChanged {
                    state: snapshot.clone(),
                }),
                _permit: permit,
            });
        }
        Some(snapshot)
    }
}

impl Drop for ControlPublisher {
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

fn control_lifecycle_worker(
    publisher: Weak<ControlPublisher>,
    receiver: SyncReceiver<ControlLifecycleEvent>,
) {
    let mut pending = BTreeMap::new();
    loop {
        let now = Instant::now();
        let expired: Vec<_> = pending
            .iter()
            .filter_map(|(&identity, (deadline, _))| (*deadline <= now).then_some(identity))
            .collect();
        for identity in expired {
            let Some(publisher) = publisher.upgrade() else {
                return;
            };
            if publisher.expire_grace(identity) {
                pending.remove(&identity);
            } else if let Some((deadline, _)) = pending.get_mut(&identity) {
                *deadline = Instant::now() + Duration::from_millis(10);
            }
        }
        let timeout = pending
            .values()
            .map(|(deadline, _)| deadline.saturating_duration_since(Instant::now()))
            .min()
            .unwrap_or(Duration::from_secs(60));
        match receiver.recv_timeout(timeout) {
            Ok(event) => {
                pending.insert(
                    event.identity,
                    (Instant::now() + CONTROL_DISCONNECT_GRACE, event._permit),
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
    let control_policy = Arc::clone(&policy);
    let mut handler = SessionHandler::new(pdu_sender, policy, Arc::clone(&admission), executor)?;
    let mut control_subscription: Option<ControlSubscription> = None;

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
            let control = async {
                match control_subscription.as_ref() {
                    Some(subscription) => subscription
                        .recv()
                        .await
                        .map(DispatchEvent::Control)
                        .map_err(anyhow::Error::from),
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
                                smol::future::or(smol::future::or(rx_msg, pane_output), control),
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
                Ok(DispatchEvent::Control(publication)) => {
                    let pdu = match publication {
                        ControlPublication::Snapshot(snapshot) => Pdu::ControlSnapshot(snapshot),
                        ControlPublication::Changed(changed) => Pdu::ControlChanged(changed),
                    };
                    pdu.encode_async(&mut stream, 0, &admission).await?;
                    stream
                        .flush()
                        .await
                        .context("flushing control publication")?;
                }
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
                    if let Some(subscription) = handler.take_control_subscription() {
                        if control_subscription.replace(subscription).is_some() {
                            return Err(anyhow::anyhow!(
                                "connection attempted to replace its control subscription"
                            ));
                        }
                    }
                    if handler.client_request_phase() == codec::ClientRequestPhase::Established {
                        deadlines.bootstrap_completed();
                    }
                }
                Ok(DispatchEvent::Queued(AdmittedItem {
                    item: Item::WritePdu { pdu, serial },
                    _permit: _output_permit,
                })) => {
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
                    if !control_policy.remove_controlled_pane(pane_id) {
                        return Err(ProjectionOverflow::OutputSaturated.into());
                    }
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

    fn control_publisher() -> (Arc<RuntimeAdmission>, Arc<ControlPublisher>) {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let publisher = ControlPublisher::new();
        publisher.bind_admission(&admission).unwrap();
        (admission, publisher)
    }

    fn state(publication: ControlPublication) -> ControlLeaseState {
        match publication {
            ControlPublication::Snapshot(snapshot) => snapshot.state,
            ControlPublication::Changed(changed) => changed.state,
        }
    }

    fn snapshot(publication: ControlPublication) -> ControlSnapshot {
        match publication {
            ControlPublication::Snapshot(snapshot) => snapshot,
            ControlPublication::Changed(_) => panic!("expected initial control snapshot"),
        }
    }

    #[test]
    fn control_lease_rules_and_takeover_are_exact() {
        let (_admission, publisher) = control_publisher();
        let first = publisher.issue_identity().unwrap();
        let second = publisher.issue_identity().unwrap();
        assert_ne!(first, second);
        let first_rx = publisher.subscribe(first).unwrap();
        let second_rx = publisher.subscribe(second).unwrap();
        let first_snapshot = snapshot(smol::block_on(first_rx.recv()).unwrap());
        let second_snapshot = snapshot(smol::block_on(second_rx.recv()).unwrap());
        assert_eq!(first_snapshot.attachment_identity, first);
        assert_eq!(second_snapshot.attachment_identity, second);
        assert_ne!(
            first_snapshot.attachment_identity,
            second_snapshot.attachment_identity
        );
        assert_eq!(first_snapshot.state.sequence, 0);
        assert_eq!(second_snapshot.state.sequence, 0);

        let acquired = publisher.apply(7, first, ControlLeaseAction::Acquire);
        assert!(matches!(acquired, ControlLeaseResult::Acquired(_)));
        assert!(publisher.is_controller(7, first));
        assert!(matches!(
            publisher.apply(7, first, ControlLeaseAction::Acquire),
            ControlLeaseResult::AlreadyController(_)
        ));
        assert!(matches!(
            publisher.apply(7, second, ControlLeaseAction::Acquire),
            ControlLeaseResult::Observing(_)
        ));
        assert!(matches!(
            publisher.apply(7, second, ControlLeaseAction::Release),
            ControlLeaseResult::NotController(_)
        ));

        let taken = publisher.apply(7, second, ControlLeaseAction::Take);
        assert!(matches!(taken, ControlLeaseResult::Taken(_)));
        assert!(!publisher.is_controller(7, first));
        assert!(publisher.is_controller(7, second));
        assert!(matches!(
            publisher.apply(7, second, ControlLeaseAction::Release),
            ControlLeaseResult::Released(_)
        ));
        assert!(!publisher.is_controller(7, second));

        let first_states = (0..3)
            .map(|_| state(smol::block_on(first_rx.recv()).unwrap()))
            .collect::<Vec<_>>();
        let second_states = (0..3)
            .map(|_| state(smol::block_on(second_rx.recv()).unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(first_states, second_states);
        assert_eq!(
            first_states
                .iter()
                .map(|state| state.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(first_states[0].active[0].controller, first);
        assert_eq!(first_states[1].active[0].controller, second);
        assert!(first_states[2].active.is_empty());
    }

    #[test]
    fn reconnect_receives_a_new_attachment_identity_and_authoritative_state() {
        let (_admission, publisher) = control_publisher();
        let original = publisher.issue_identity().unwrap();
        let original_rx = publisher.subscribe(original).unwrap();
        let original_snapshot = snapshot(smol::block_on(original_rx.recv()).unwrap());
        assert_eq!(original_snapshot.attachment_identity, original);
        assert!(matches!(
            publisher.apply(7, original, ControlLeaseAction::Acquire),
            ControlLeaseResult::Acquired(_)
        ));
        drop(original_rx);

        let reconnected = publisher.issue_identity().unwrap();
        let reconnected_rx = publisher.subscribe(reconnected).unwrap();
        let reconnected_snapshot = snapshot(smol::block_on(reconnected_rx.recv()).unwrap());
        assert_ne!(reconnected_snapshot.attachment_identity, original);
        assert_eq!(reconnected_snapshot.attachment_identity, reconnected);
        assert_eq!(reconnected_snapshot.state.sequence, 1);
        assert_eq!(
            reconnected_snapshot.state.active,
            vec![ActiveControlLease {
                pane_id: 7,
                controller: original,
            }]
        );

        assert!(matches!(
            publisher.apply(7, reconnected, ControlLeaseAction::Take),
            ControlLeaseResult::Taken(_)
        ));
        let changed = state(smol::block_on(reconnected_rx.recv()).unwrap());
        assert_eq!(changed.sequence, 2);
        assert_eq!(
            changed.active,
            vec![ActiveControlLease {
                pane_id: 7,
                controller: reconnected,
            }]
        );
    }

    #[test]
    fn publisher_overload_does_not_advance_sequence_and_retry_succeeds() {
        let (_admission, publisher) = control_publisher();
        let first = publisher.issue_identity().unwrap();
        let second = publisher.issue_identity().unwrap();
        let first_rx = publisher.subscribe(first).unwrap();
        let second_rx = publisher.subscribe(second).unwrap();
        smol::block_on(first_rx.recv()).unwrap();
        smol::block_on(second_rx.recv()).unwrap();

        for index in 0..MAX_CONTROL_NOTIFICATIONS_PER_ATTACHMENT {
            let identity = if index % 2 == 0 { first } else { second };
            assert!(matches!(
                publisher.apply(11, identity, ControlLeaseAction::Take),
                ControlLeaseResult::Taken(_)
            ));
        }
        assert!(matches!(
            publisher.apply(11, first, ControlLeaseAction::Take),
            ControlLeaseResult::Overloaded
        ));
        assert_eq!(publisher.state.lock().unwrap().sequence, 256);

        smol::block_on(first_rx.recv()).unwrap();
        smol::block_on(second_rx.recv()).unwrap();
        assert!(matches!(
            publisher.apply(11, first, ControlLeaseAction::Take),
            ControlLeaseResult::Taken(_)
        ));
        assert_eq!(publisher.state.lock().unwrap().sequence, 257);
    }

    #[test]
    fn pane_removal_and_grace_expiry_publish_contiguous_changes() {
        let (_admission, publisher) = control_publisher();
        let identity = publisher.issue_identity().unwrap();
        let rx = publisher.subscribe(identity).unwrap();
        smol::block_on(rx.recv()).unwrap();
        assert!(matches!(
            publisher.apply(3, identity, ControlLeaseAction::Acquire),
            ControlLeaseResult::Acquired(_)
        ));
        assert!(matches!(
            publisher.apply(4, identity, ControlLeaseAction::Acquire),
            ControlLeaseResult::Acquired(_)
        ));
        assert!(publisher.remove_pane(3));
        assert!(publisher.expire_grace(identity));

        let sequences = (0..4)
            .map(|_| state(smol::block_on(rx.recv()).unwrap()).sequence)
            .collect::<Vec<_>>();
        assert_eq!(sequences, vec![1, 2, 3, 4]);
        assert!(publisher.state.lock().unwrap().active.is_empty());
    }

    #[test]
    fn connection_identity_exhaustion_is_permanently_terminal() {
        let (_admission, publisher) = control_publisher();
        publisher.next_identity.store(u64::MAX, Ordering::Relaxed);

        assert!(publisher.issue_identity().is_err());
        assert!(publisher.issue_identity().is_err());
        assert_eq!(publisher.next_identity.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn active_control_leases_are_bounded_and_repeated_acquire_does_not_grow_state() {
        let (_admission, publisher) = control_publisher();
        let identity = publisher.issue_identity().unwrap();
        for pane_id in 0..MAX_PANES {
            assert!(matches!(
                publisher.apply(pane_id, identity, ControlLeaseAction::Acquire),
                ControlLeaseResult::Acquired(_)
            ));
        }
        let sequence = publisher.state.lock().unwrap().sequence;
        assert!(matches!(
            publisher.apply(0, identity, ControlLeaseAction::Acquire),
            ControlLeaseResult::AlreadyController(_)
        ));
        assert_eq!(publisher.state.lock().unwrap().sequence, sequence);
        assert!(matches!(
            publisher.apply(MAX_PANES, identity, ControlLeaseAction::Acquire),
            ControlLeaseResult::Overloaded
        ));
        let state = publisher.state.lock().unwrap();
        assert_eq!(state.active.len(), MAX_PANES);
        assert_eq!(state.sequence, sequence);
    }

    #[test]
    fn grace_expiry_is_atomic_and_non_mutating_when_publication_is_full() {
        let (_admission, publisher) = control_publisher();
        let expiring = publisher.issue_identity().unwrap();
        let other = publisher.issue_identity().unwrap();
        let rx = publisher.subscribe(expiring).unwrap();
        smol::block_on(rx.recv()).unwrap();
        assert!(matches!(
            publisher.apply(1, expiring, ControlLeaseAction::Acquire),
            ControlLeaseResult::Acquired(_)
        ));
        assert!(matches!(
            publisher.apply(2, expiring, ControlLeaseAction::Acquire),
            ControlLeaseResult::Acquired(_)
        ));
        for index in 0..(MAX_CONTROL_NOTIFICATIONS_PER_ATTACHMENT - 2) {
            let identity = if index % 2 == 0 { expiring } else { other };
            assert!(matches!(
                publisher.apply(3, identity, ControlLeaseAction::Take),
                ControlLeaseResult::Taken(_)
            ));
        }
        let before = publisher.state.lock().unwrap().snapshot();
        assert!(!publisher.expire_grace(expiring));
        assert_eq!(publisher.state.lock().unwrap().snapshot(), before);

        smol::block_on(rx.recv()).unwrap();
        assert!(publisher.expire_grace(expiring));
        let after = publisher.state.lock().unwrap().snapshot();
        assert_eq!(after.sequence, before.sequence + 1);
        assert!(after
            .active
            .iter()
            .all(|lease| lease.controller != expiring));
    }

    #[test]
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
