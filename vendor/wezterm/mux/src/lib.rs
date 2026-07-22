use crate::client::{ClientId, ClientInfo};
use crate::pane::{CachePolicy, Pane, PaneId};
use crate::ssh_agent::AgentProxy;
use crate::tab::{SplitRequest, Tab, TabId};
use crate::window::{Window, WindowId};
use anyhow::{anyhow, bail, Context, Error};
use config::keyassignment::SpawnTabDomain;
use config::{configuration, ExitBehavior, GuiPosition};
use domain::{Domain, DomainId, DomainState, SplitSource};
use filedescriptor::{poll, pollfd, socketpair, AsRawSocketDescriptor, FileDescriptor, POLLIN};
#[cfg(unix)]
use libc::{c_int, SOL_SOCKET, SO_RCVBUF, SO_SNDBUF};
use log::error;
use metrics::histogram;
use parking_lot::{
    MappedRwLockReadGuard, MappedRwLockWriteGuard, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use percent_encoding::percent_decode_str;
use portable_pty::{CommandBuilder, ExitStatus, PtySize};
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::future::Future;
use std::io::{Read, Write};
#[cfg(windows)]
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver as SyncReceiver, SyncSender, TrySendError};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};
use termwiz::escape::csi::{DecPrivateMode, DecPrivateModeCode, Device, Mode};
use termwiz::escape::{Action, CSI};
use thiserror::*;
use wezterm_runtime_admission::{
    ByteClass, BytePermit, MAX_PANE_INPUT_BYTES_PER_PANE, MAX_PANE_INPUT_ITEMS_PER_PANE,
};
pub use wezterm_runtime_admission::{
    CountClass, CountPermit, PaneAdmissionPermit, RuntimeAdmission, RuntimeRole,
    TabAdmissionPermit, MAX_PANES,
};
use wezterm_term::{Clipboard, ClipboardSelection, DownloadHandler, TerminalSize};
#[cfg(windows)]
use winapi::um::winsock2::{SOL_SOCKET, SO_RCVBUF, SO_SNDBUF};

pub mod activity;
pub mod client;
pub mod connui;
pub mod domain;
pub mod localpane;
pub mod pane;
pub mod renderable;
pub mod ssh;
pub mod ssh_agent;
pub mod tab;
pub mod termwiztermtab;
pub mod tmux;
pub mod tmux_commands;
mod tmux_pty;
pub mod window;

use crate::activity::Activity;

pub const DEFAULT_WORKSPACE: &str = "default";

#[derive(Clone, Debug)]
pub enum MuxNotification {
    PaneOutput(PaneId),
    PaneAdded(PaneId),
    PaneRemoved(PaneId),
    WindowCreated(WindowId),
    WindowRemoved(WindowId),
    WindowInvalidated(WindowId),
    WindowWorkspaceChanged(WindowId),
    ActiveWorkspaceChanged(Arc<ClientId>),
    Alert {
        pane_id: PaneId,
        alert: wezterm_term::Alert,
    },
    Empty,
    AssignClipboard {
        pane_id: PaneId,
        selection: ClipboardSelection,
        clipboard: Option<String>,
    },
    SaveToDownloads {
        name: Option<String>,
        data: Arc<Vec<u8>>,
    },
    TabAddedToWindow {
        tab_id: TabId,
        window_id: WindowId,
    },
    PaneFocused(PaneId),
    TabResized(TabId),
    TabTitleChanged {
        tab_id: TabId,
        title: String,
    },
    WindowTitleChanged {
        window_id: WindowId,
        title: String,
    },
    WorkspaceRenamed {
        old_workspace: String,
        new_workspace: String,
    },
}

static LAST_SUBSCRIBER_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaneRemoval {
    Kill,
    Unregister,
}

impl PaneRemoval {
    fn should_kill(self) -> bool {
        self == Self::Kill
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientPaneTaskKind {
    Request,
    Fetch,
    Poll,
    Close,
}

impl ClientPaneTaskKind {
    fn admission_class(self) -> Option<CountClass> {
        match self {
            Self::Request | Self::Close => None,
            Self::Fetch => Some(CountClass::ClientFetchJob),
            Self::Poll => Some(CountClass::ClientPollJob),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaneTaskKind {
    Input { bytes: usize },
    Write { bytes: usize },
    Refresh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OwnedPaneTaskKind {
    Client(ClientPaneTaskKind),
    Pane(PaneTaskKind),
}

impl OwnedPaneTaskKind {
    fn cancel_on_removal(self, removal: PaneRemoval) -> bool {
        match self {
            Self::Client(ClientPaneTaskKind::Close) => removal == PaneRemoval::Unregister,
            Self::Client(_) | Self::Pane(_) => true,
        }
    }
}

struct OwnedPaneTask {
    kind: OwnedPaneTaskKind,
    task: promise::spawn::AdmittedTask<anyhow::Result<()>>,
}

#[derive(Default)]
struct PaneTaskPermits {
    _pane_input: Option<PaneInputPermit>,
    _input_item: Option<CountPermit>,
    _job: Option<CountPermit>,
    _input_bytes: Option<BytePermit>,
}

#[derive(Default)]
struct PaneInputAdmission {
    items: AtomicUsize,
    bytes: AtomicUsize,
}

struct PaneInputPermit {
    admission: Arc<PaneInputAdmission>,
    bytes: usize,
}

impl PaneInputAdmission {
    fn try_reserve(
        counter: &AtomicUsize,
        amount: usize,
        maximum: usize,
        name: &str,
    ) -> anyhow::Result<()> {
        let mut used = counter.load(Ordering::Acquire);
        loop {
            let next = used
                .checked_add(amount)
                .ok_or_else(|| anyhow!("{name} admission overflow"))?;
            if next > maximum {
                anyhow::bail!("{name} admission is full ({used} + {amount} > {maximum})");
            }
            match counter.compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(()),
                Err(observed) => used = observed,
            }
        }
    }

    fn try_input(self: &Arc<Self>, bytes: usize) -> anyhow::Result<PaneInputPermit> {
        Self::try_reserve(
            &self.items,
            1,
            MAX_PANE_INPUT_ITEMS_PER_PANE,
            "pane input item",
        )?;
        if let Err(error) = Self::try_reserve(
            &self.bytes,
            bytes,
            MAX_PANE_INPUT_BYTES_PER_PANE,
            "pane input byte",
        ) {
            self.items.fetch_sub(1, Ordering::AcqRel);
            return Err(error);
        }
        Ok(PaneInputPermit {
            admission: Arc::clone(self),
            bytes,
        })
    }
}

impl Drop for PaneInputPermit {
    fn drop(&mut self) {
        let prior_items = self.admission.items.fetch_sub(1, Ordering::AcqRel);
        let prior_bytes = self.admission.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(prior_items >= 1, "pane input item admission underflow");
        debug_assert!(
            prior_bytes >= self.bytes,
            "pane input byte admission underflow"
        );
    }
}

impl PaneTaskPermits {
    fn admit(
        admission: &RuntimeAdmission,
        pane_input: &Arc<PaneInputAdmission>,
        kind: PaneTaskKind,
    ) -> anyhow::Result<Self> {
        match kind {
            PaneTaskKind::Input { bytes } => Ok(Self {
                _pane_input: Some(pane_input.try_input(bytes)?),
                _input_item: Some(
                    admission
                        .try_count(CountClass::PaneInputItem, 1)
                        .context("admit pane input item")?,
                ),
                _input_bytes: Some(
                    admission
                        .try_bytes(ByteClass::PaneInput, bytes)
                        .context("admit pane input bytes")?,
                ),
                _job: None,
            }),
            PaneTaskKind::Write { bytes } => Ok(Self {
                _pane_input: Some(pane_input.try_input(bytes)?),
                _input_item: Some(
                    admission
                        .try_count(CountClass::PaneInputItem, 1)
                        .context("admit pane write input item")?,
                ),
                _job: Some(
                    admission
                        .try_count(CountClass::PaneWriteJob, 1)
                        .context("admit pane write job")?,
                ),
                _input_bytes: Some(
                    admission
                        .try_bytes(ByteClass::PaneInput, bytes)
                        .context("admit pane write bytes")?,
                ),
            }),
            PaneTaskKind::Refresh => Ok(Self {
                _job: Some(
                    admission
                        .try_count(CountClass::PaneRefreshJob, 1)
                        .context("admit pane refresh job")?,
                ),
                ..Self::default()
            }),
        }
    }
}

struct RetiringPaneTasks {
    pane_id: PaneId,
    tasks: Vec<OwnedPaneTask>,
    _pane_permit: PaneAdmissionPermit,
}

#[derive(Default)]
struct HeadlessPaneOutputTasks {
    pending: HashSet<PaneId>,
    tasks: Vec<promise::spawn::AdmittedTask<anyhow::Result<()>>>,
}

fn retire_pane_tasks(
    pane_id: PaneId,
    removal: PaneRemoval,
    tasks: Vec<OwnedPaneTask>,
    pane_permit: PaneAdmissionPermit,
) -> RetiringPaneTasks {
    for owned in &tasks {
        if owned.kind.cancel_on_removal(removal) {
            owned.task.cancel();
        }
    }

    RetiringPaneTasks {
        pane_id,
        tasks,
        _pane_permit: pane_permit,
    }
}

fn reap_retiring_pane_tasks(retiring: &mut Vec<RetiringPaneTasks>) -> anyhow::Result<()> {
    let mut first_error = None;
    let mut index = 0;
    while index < retiring.len() {
        if let Err(err) = reap_owned_pane_tasks(&mut retiring[index].tasks) {
            if first_error.is_none() {
                first_error = Some(err);
            }
        }
        if retiring[index].tasks.is_empty() {
            retiring.swap_remove(index);
        } else {
            index += 1;
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

pub struct Mux {
    admission: Arc<RuntimeAdmission>,
    headless_executor: Option<Arc<promise::spawn::SimpleExecutor>>,
    lifecycle: Mutex<RuntimeLifecycle>,
    tab_permits: Mutex<HashMap<TabId, TabAdmissionPermit>>,
    pane_workers: Mutex<HashMap<PaneId, PaneWorkerSet>>,
    retiring_pane_tasks: Mutex<Vec<RetiringPaneTasks>>,
    headless_runtime_tasks: Mutex<Vec<promise::spawn::AdmittedTask<anyhow::Result<()>>>>,
    client_invalidation_tasks: Mutex<Vec<promise::spawn::AdmittedTask<anyhow::Result<()>>>>,
    headless_pane_output_tasks: Mutex<HeadlessPaneOutputTasks>,
    headless_task_error: Mutex<Option<anyhow::Error>>,
    pane_lifecycle: Mutex<Option<PaneLifecycleCoordinator>>,
    tabs: RwLock<HashMap<TabId, Arc<Tab>>>,
    panes: RwLock<HashMap<PaneId, Arc<dyn Pane>>>,
    windows: RwLock<HashMap<WindowId, Window>>,
    default_domain: RwLock<Option<Arc<dyn Domain>>>,
    domains: RwLock<HashMap<DomainId, Arc<dyn Domain>>>,
    domains_by_name: RwLock<HashMap<String, Arc<dyn Domain>>>,
    subscribers: RwLock<HashMap<usize, Box<dyn Fn(MuxNotification) -> bool + Send + Sync>>>,
    banner: RwLock<Option<String>>,
    clients: RwLock<HashMap<ClientId, ClientInfo>>,
    identity: RwLock<Option<Arc<ClientId>>>,
    num_panes_by_workspace: RwLock<HashMap<String, usize>>,
    main_thread_id: std::thread::ThreadId,
    agent: Option<AgentProxy>,
}

#[derive(Debug)]
struct RuntimeLifecycle {
    shutting_down: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PaneLifecycleAction {
    Prune,
    Remove(PaneId),
}

struct PaneLifecycleEvent {
    action: PaneLifecycleAction,
    _permit: CountPermit,
}

struct PaneLifecycleCoordinator {
    sender: Option<SyncSender<PaneLifecycleEvent>>,
    worker: Option<thread::JoinHandle<anyhow::Result<()>>>,
}

struct PaneWorkerSet {
    pane: Arc<dyn Pane>,
    pane_permit: Option<PaneAdmissionPermit>,
    cancel: Arc<AtomicBool>,
    reader: Option<thread::JoinHandle<anyhow::Result<()>>>,
    parser: Option<thread::JoinHandle<()>>,
    has_reader: bool,
    input_admission: Arc<PaneInputAdmission>,
    tasks: Vec<OwnedPaneTask>,
    stopped: bool,
}

impl Drop for Mux {
    fn drop(&mut self) {
        self.shutdown_runtime();
    }
}

impl PaneWorkerSet {
    fn start(
        pane: Arc<dyn Pane>,
        banner: Option<String>,
        reader: Option<Box<dyn std::io::Read + Send>>,
        admission: Arc<RuntimeAdmission>,
        lifecycle: SyncSender<PaneLifecycleEvent>,
        pane_permit: PaneAdmissionPermit,
    ) -> anyhow::Result<Self> {
        let Some(reader) = reader else {
            return Ok(Self {
                pane,
                pane_permit: Some(pane_permit),
                cancel: Arc::new(AtomicBool::new(false)),
                reader: None,
                parser: None,
                has_reader: false,
                input_admission: Arc::new(PaneInputAdmission::default()),
                tasks: Vec::new(),
                stopped: false,
            });
        };

        let (tx, rx) = allocate_socketpair()?;
        let cancel = Arc::new(AtomicBool::new(false));
        let parser_permit = admission
            .try_count(CountClass::ExecutorRunnable, 1)
            .context("admit pane parser worker")?;
        let reader_permit = admission
            .try_count(CountClass::ExecutorRunnable, 1)
            .context("admit pane reader worker")?;
        let parser_cancel = Arc::clone(&cancel);
        let parser_pane = Arc::downgrade(&pane);
        let pane_id = pane.pane_id();
        let parser = thread::Builder::new()
            .name(format!("mux-pane-parser-{pane_id}"))
            .spawn(move || {
                let _permit = parser_permit;
                parse_buffered_data(parser_pane, &parser_cancel, rx)
            })
            .context("spawn pane parser worker")?;

        let reader_cancel = Arc::clone(&cancel);
        let reader_pane = Arc::downgrade(&pane);
        let reader_admission = Arc::clone(&admission);
        let reader = match thread::Builder::new()
            .name(format!("mux-pane-reader-{pane_id}"))
            .spawn(move || {
                let action = read_from_pane_pty(reader_pane, banner, reader, tx, reader_cancel);
                drop(reader_permit);
                if let Some(action) = action {
                    if let Err(err) = enqueue_pane_lifecycle(&reader_admission, &lifecycle, action)
                    {
                        reader_admission.begin_shutdown();
                        return Err(err);
                    }
                }
                Ok(())
            }) {
            Ok(reader) => reader,
            Err(err) => {
                cancel.store(true, Ordering::Release);
                if parser.join().is_err() {
                    log::error!("pane parser worker panicked after reader spawn failure");
                }
                return Err(err).context("spawn pane reader worker");
            }
        };

        Ok(Self {
            pane,
            pane_permit: Some(pane_permit),
            cancel,
            reader: Some(reader),
            parser: Some(parser),
            has_reader: true,
            input_admission: Arc::new(PaneInputAdmission::default()),
            tasks: Vec::new(),
            stopped: false,
        })
    }

    fn take_pane_permit(&mut self) -> PaneAdmissionPermit {
        self.pane_permit
            .take()
            .expect("registered pane worker must own its admission permit")
    }

    fn shutdown(&mut self, kill_process: bool) -> anyhow::Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;

        // Mark this as an owned shutdown before killing the child. The child waiter can wake the
        // PTY reader immediately after process exit; setting the flag first prevents that wake from
        // being mistaken for a natural EOF that should schedule another pane-removal task.
        self.cancel.store(true, Ordering::Release);
        if kill_process {
            self.pane.kill();
        }

        let mut first_error = None;
        if self.has_reader {
            if let Err(err) = self.pane.cancel_reader() {
                first_error = Some(err.context("cancel pane reader"));
            }
        }
        if let Some(reader) = self.reader.take() {
            match reader.join() {
                Ok(Ok(())) => {}
                Ok(Err(err)) if first_error.is_none() => {
                    first_error = Some(err.context("pane reader worker failed"));
                }
                Ok(Err(_)) => {}
                Err(_) if first_error.is_none() => {
                    first_error = Some(anyhow!("pane reader worker panicked"));
                }
                Err(_) => {}
            }
        }
        if let Some(parser) = self.parser.take() {
            if parser.join().is_err() && first_error.is_none() {
                first_error = Some(anyhow!("pane parser worker panicked"));
            }
        }
        if let Err(err) = self.pane.join_child_waiter() {
            if first_error.is_none() {
                first_error = Some(err.context("join pane child waiter"));
            }
        }

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

impl Drop for PaneWorkerSet {
    fn drop(&mut self) {
        debug_assert!(
            self.tasks.is_empty(),
            "pane tasks must be transferred to their join owner"
        );
        if let Err(err) = self.shutdown(true) {
            log::error!("pane worker shutdown failed: {err:#}");
        }
    }
}

fn observe_owned_task(
    task: promise::spawn::AdmittedTask<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    match promise::spawn::block_on(task) {
        Ok(result) => result,
        Err(err) if err.is_cancelled() => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn reap_admitted_tasks(
    tasks: &mut Vec<promise::spawn::AdmittedTask<anyhow::Result<()>>>,
) -> anyhow::Result<()> {
    let mut first_error = None;
    let mut index = 0;
    while index < tasks.len() {
        if tasks[index].is_finished() {
            let task = tasks.swap_remove(index);
            if let Err(err) = observe_owned_task(task) {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        } else {
            index += 1;
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn reap_owned_pane_tasks(tasks: &mut Vec<OwnedPaneTask>) -> anyhow::Result<()> {
    let mut first_error = None;
    let mut index = 0;
    while index < tasks.len() {
        if tasks[index].task.is_finished() {
            let task = tasks.swap_remove(index).task;
            if let Err(err) = observe_owned_task(task) {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        } else {
            index += 1;
        }
    }
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

const BUFSIZE: usize = 1024 * 1024;

fn bounded_parser_buffer_size(configured: usize) -> usize {
    configured.clamp(
        1,
        wezterm_runtime_admission::MAX_PANE_PARSER_INPUT_BYTES_PER_BATCH,
    )
}

fn parser_buffer_size() -> usize {
    bounded_parser_buffer_size(configuration().mux_output_parser_buffer_size)
}

/// This function applies parsed actions to the pane and notifies any
/// mux subscribers about the output event
fn send_actions_to_mux(pane: &Weak<dyn Pane>, dead: &Arc<AtomicBool>, actions: Vec<Action>) {
    let start = Instant::now();
    match pane.upgrade() {
        Some(pane) => {
            pane.perform_actions(actions);
            histogram!("send_actions_to_mux.perform_actions.latency").record(start.elapsed());
            Mux::notify_pane_output_from_any_thread(pane.pane_id());
        }
        None => {
            // Something else removed the pane from
            // the mux, so signal that we should stop
            // trying to process it in read_from_pane_pty.
            dead.store(true, Ordering::Relaxed);
        }
    }
    histogram!("send_actions_to_mux.rate").record(1.);
}

/// This is the parsing loop for the given pane.
/// It reads all data sent to `rx` (from pane PTY) and handles all terminal events for this pane.
fn parse_buffered_data(pane: Weak<dyn Pane>, dead: &Arc<AtomicBool>, mut rx: FileDescriptor) {
    let mut buf = vec![0; parser_buffer_size()];
    let mut parser = termwiz::escape::parser::Parser::new();
    let mut actions = vec![];
    let mut hold = false;
    let mut action_size: usize = 0;
    let mut delay = Duration::from_millis(configuration().mux_output_parser_coalesce_delay_ms);
    let mut deadline = None;

    loop {
        match rx.read(&mut buf) {
            Ok(size) if size == 0 => {
                dead.store(true, Ordering::Relaxed);
                break;
            }
            Err(_) => {
                dead.store(true, Ordering::Relaxed);
                break;
            }
            Ok(size) => {
                parser.parse(&buf[0..size], |action| {
                    let mut flush = false;
                    match &action {
                        Action::CSI(CSI::Mode(Mode::SetDecPrivateMode(DecPrivateMode::Code(
                            DecPrivateModeCode::SynchronizedOutput,
                        )))) => {
                            // Synchronized output frame started:
                            // => We hold off ~all actions that applies changes to the terminal.
                            hold = true;

                            // => We also flush prior actions
                            flush = true;
                        }
                        Action::CSI(CSI::Mode(Mode::ResetDecPrivateMode(
                            DecPrivateMode::Code(DecPrivateModeCode::SynchronizedOutput),
                        ))) => {
                            // Synchronized output frame ended:
                            // => We flush out all pending actions to the terminal.
                            hold = false;
                            flush = true;
                        }
                        Action::CSI(CSI::Device(dev)) if matches!(**dev, Device::SoftReset) => {
                            // Soft reset requested
                            hold = false;
                            flush = true;
                        }
                        _ => {}
                    };
                    action.append_to(&mut actions);

                    if flush && !actions.is_empty() {
                        send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                        action_size = 0;
                    }
                });
                action_size = action_size.saturating_add(size);
                if !actions.is_empty()
                    && action_size
                        >= wezterm_runtime_admission::MAX_PANE_PARSER_INPUT_BYTES_PER_BATCH
                {
                    // Synchronized output may delay presentation, but it must not retain an
                    // unbounded action vector. Applying a bounded chunk preserves the terminal's
                    // synchronized-output mode while releasing parser-owned allocations.
                    send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                    deadline = None;
                    action_size = 0;
                } else if !actions.is_empty() && !hold {
                    // If we haven't accumulated too much data,
                    // pause for a short while to increase the chances
                    // that we coalesce a full "frame" from an unoptimized
                    // TUI program
                    if action_size < buf.len() {
                        let poll_delay = match deadline {
                            None => {
                                deadline.replace(Instant::now() + delay);
                                Some(delay)
                            }
                            Some(target) => target.checked_duration_since(Instant::now()),
                        };
                        if poll_delay.is_some() {
                            let mut pfd = [pollfd {
                                fd: rx.as_socket_descriptor(),
                                events: POLLIN,
                                revents: 0,
                            }];
                            if let Ok(1) = poll(&mut pfd, poll_delay) {
                                // We can read now without blocking, so accumulate
                                // more data into actions
                                continue;
                            }

                            // Not readable in time: let the data we have flow into
                            // the terminal model
                        }
                    }

                    send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
                    deadline = None;
                    action_size = 0;
                }

                let config = configuration();
                buf.resize(
                    bounded_parser_buffer_size(config.mux_output_parser_buffer_size),
                    0,
                );
                delay = Duration::from_millis(config.mux_output_parser_coalesce_delay_ms);
            }
        }
    }

    // Don't forget to send anything that we might have buffered
    // to be displayed before we return from here; this is important
    // for very short lived commands so that we don't forget to
    // display what they displayed.
    if !actions.is_empty() {
        send_actions_to_mux(&pane, &dead, std::mem::take(&mut actions));
    }
}

fn set_socket_buffer(fd: &mut FileDescriptor, option: i32, size: usize) -> anyhow::Result<()> {
    let size = size as c_int;
    let socklen = std::mem::size_of_val(&size);
    unsafe {
        let res = libc::setsockopt(
            fd.as_socket_descriptor(),
            SOL_SOCKET,
            option,
            &size as *const c_int as *const _,
            socklen as _,
        );
        if res == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error()).context("setsockopt")
        }
    }
}

fn allocate_socketpair() -> anyhow::Result<(FileDescriptor, FileDescriptor)> {
    let (mut tx, mut rx) = socketpair().context("socketpair")?;
    set_socket_buffer(&mut tx, SO_SNDBUF, BUFSIZE)
        .context("SO_SNDBUF")
        .ok();
    set_socket_buffer(&mut rx, SO_RCVBUF, BUFSIZE)
        .context("SO_RCVBUF")
        .ok();
    Ok((tx, rx))
}

fn enqueue_pane_lifecycle(
    admission: &RuntimeAdmission,
    sender: &SyncSender<PaneLifecycleEvent>,
    action: PaneLifecycleAction,
) -> anyhow::Result<()> {
    let permit = admission
        .try_count(CountClass::PaneLifecycleEvent, 1)
        .context("admit pane lifecycle event")?;
    let event = PaneLifecycleEvent {
        action,
        _permit: permit,
    };
    sender.try_send(event).map_err(|err| match err {
        TrySendError::Full(_) => anyhow!("pane lifecycle queue is saturated"),
        TrySendError::Disconnected(_) => anyhow!("pane lifecycle coordinator is stopped"),
    })
}

/// This function is run in a separate thread; its purpose is to perform
/// blocking reads from the pty (non-blocking reads are not portable to
/// all platforms and pty/tty types), parse the escape sequences and
/// relay the actions to the mux thread to apply them to the pane.
fn read_from_pane_pty(
    pane: Weak<dyn Pane>,
    banner: Option<String>,
    mut reader: Box<dyn std::io::Read>,
    mut tx: FileDescriptor,
    dead: Arc<AtomicBool>,
) -> Option<PaneLifecycleAction> {
    let mut buf = vec![0; BUFSIZE];

    let (pane_id, exit_behavior) = match pane.upgrade() {
        Some(pane) => (pane.pane_id(), pane.exit_behavior()),
        None => return None,
    };

    if let Some(banner) = banner {
        tx.write_all(banner.as_bytes()).ok();
    }

    // A cancellation-aware reader drains every byte that was already readable before it reports
    // EOF. Do not stop merely because shutdown was requested: doing so would preserve only the
    // first ready read and discard the remainder of the PTY's final output.
    loop {
        match reader.read(&mut buf) {
            Ok(size) if size == 0 => {
                log::trace!("read_pty EOF: pane_id {}", pane_id);
                break;
            }
            Err(err) => {
                error!("read_pty failed: pane {} {:?}", pane_id, err);
                break;
            }
            Ok(size) => {
                histogram!("read_from_pane_pty.bytes.rate").record(size as f64);
                log::trace!("read_pty pane {pane_id} read {size} bytes");
                // Send received data to this pane parser thread.
                if let Err(err) = tx.write_all(&buf[..size]) {
                    error!(
                        "read_pty failed to write to parser for pane {}: {:?}",
                        pane_id, err
                    );
                    break;
                }
            }
        }
    }

    let was_cancelled = dead.swap(true, Ordering::AcqRel);
    drop(tx);
    if was_cancelled {
        return None;
    }

    Some(
        match exit_behavior.unwrap_or_else(|| configuration().exit_behavior) {
            ExitBehavior::Hold | ExitBehavior::CloseOnCleanExit => PaneLifecycleAction::Prune,
            ExitBehavior::Close => PaneLifecycleAction::Remove(pane_id),
        },
    )
}

enum ProcessMux {
    Vacant,
    Active(Arc<Mux>),
    Shutdown,
}

lazy_static::lazy_static! {
    static ref MUX: Mutex<ProcessMux> = Mutex::new(ProcessMux::Vacant);
}

pub struct MuxWindowBuilder {
    window_id: WindowId,
    activity: Option<Activity>,
    notified: bool,
}

impl MuxWindowBuilder {
    fn notify(&mut self) {
        if self.notified {
            return;
        }
        self.notified = true;
        let activity = self.activity.take().unwrap();
        let window_id = self.window_id;
        let mux = Mux::get();
        if mux.is_main_thread() {
            // If we're already on the mux thread, just send the notification
            // immediately.
            // This is super important for Wayland; if we push it to the
            // spawn queue below then the extra milliseconds of delay
            // causes it to get confused and shutdown the connection!?
            mux.notify(MuxNotification::WindowCreated(window_id));
        } else {
            let weak_mux = Arc::downgrade(&mux);
            if let Err(err) =
                mux.try_spawn_runtime_task("schedule window-created notification", async move {
                    if let Some(mux) = weak_mux.upgrade() {
                        mux.notify(MuxNotification::WindowCreated(window_id));
                        drop(activity);
                    }
                    Ok(())
                })
            {
                log::error!("failed to schedule window-created notification: {err:#}");
            }
        }
    }
}

impl Drop for MuxWindowBuilder {
    fn drop(&mut self) {
        self.notify();
    }
}

impl std::ops::Deref for MuxWindowBuilder {
    type Target = WindowId;

    fn deref(&self) -> &WindowId {
        &self.window_id
    }
}

impl Mux {
    pub fn new(default_domain: Option<Arc<dyn Domain>>, admission: Arc<RuntimeAdmission>) -> Self {
        Self::new_with_executor(default_domain, admission, None)
    }

    /// Construct a mux whose async lifecycle is owned by a bounded, explicitly injected executor.
    /// GUI callers use [`Mux::new`] and their existing event-loop scheduler; headless runtimes must
    /// use this constructor and retain the matching [`promise::spawn::SimpleExecutor`].
    pub fn new_headless(
        default_domain: Option<Arc<dyn Domain>>,
        admission: Arc<RuntimeAdmission>,
        executor: Arc<promise::spawn::SimpleExecutor>,
    ) -> Self {
        Self::new_with_executor(default_domain, admission, Some(executor))
    }

    fn new_with_executor(
        default_domain: Option<Arc<dyn Domain>>,
        admission: Arc<RuntimeAdmission>,
        headless_executor: Option<Arc<promise::spawn::SimpleExecutor>>,
    ) -> Self {
        let mut domains = HashMap::new();
        let mut domains_by_name = HashMap::new();
        if let Some(default_domain) = default_domain.as_ref() {
            domains.insert(default_domain.domain_id(), Arc::clone(default_domain));

            domains_by_name.insert(
                default_domain.domain_name().to_string(),
                Arc::clone(default_domain),
            );
        }

        let agent = if config::configuration().mux_enable_ssh_agent {
            Some(AgentProxy::new())
        } else {
            None
        };

        Self {
            admission,
            headless_executor,
            lifecycle: Mutex::new(RuntimeLifecycle {
                shutting_down: false,
            }),
            tab_permits: Mutex::new(HashMap::new()),
            pane_workers: Mutex::new(HashMap::new()),
            retiring_pane_tasks: Mutex::new(Vec::new()),
            headless_runtime_tasks: Mutex::new(Vec::new()),
            client_invalidation_tasks: Mutex::new(Vec::new()),
            headless_pane_output_tasks: Mutex::new(HeadlessPaneOutputTasks::default()),
            headless_task_error: Mutex::new(None),
            pane_lifecycle: Mutex::new(None),
            tabs: RwLock::new(HashMap::new()),
            panes: RwLock::new(HashMap::new()),
            windows: RwLock::new(HashMap::new()),
            default_domain: RwLock::new(default_domain),
            domains_by_name: RwLock::new(domains_by_name),
            domains: RwLock::new(domains),
            subscribers: RwLock::new(HashMap::new()),
            banner: RwLock::new(None),
            clients: RwLock::new(HashMap::new()),
            identity: RwLock::new(None),
            num_panes_by_workspace: RwLock::new(HashMap::new()),
            main_thread_id: std::thread::current().id(),
            agent,
        }
    }

    pub fn admission(&self) -> &Arc<RuntimeAdmission> {
        &self.admission
    }

    pub fn headless_executor(&self) -> anyhow::Result<promise::spawn::SimpleExecutorHandle> {
        self.headless_executor
            .as_ref()
            .map(|executor| executor.handle())
            .ok_or_else(|| anyhow!("this mux is not owned by a headless executor"))
    }

    pub fn is_headless_runtime(&self) -> bool {
        self.headless_executor.is_some()
    }

    pub fn try_spawn_client_pane_task<F>(
        &self,
        pane_id: PaneId,
        kind: ClientPaneTaskKind,
        future: F,
    ) -> anyhow::Result<()>
    where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        if self.admission.is_shutting_down() {
            anyhow::bail!("mux is shutting down");
        }

        let Some(executor) = self.headless_executor.as_ref() else {
            promise::spawn::spawn(async move {
                if let Err(err) = future.await {
                    log::error!("client pane task failed: {err:#}");
                }
            })
            .detach();
            return Ok(());
        };

        if let Err(err) = self.reap_headless_tasks() {
            log::error!("observed completed task before scheduling pane work: {err:#}");
            self.record_headless_task_error(err);
        }
        let mut pane_workers = self.pane_workers.lock();
        let workers = pane_workers
            .get_mut(&pane_id)
            .ok_or_else(|| anyhow!("pane {pane_id} has no registered task owner"))?;
        if kind == ClientPaneTaskKind::Poll
            && workers
                .tasks
                .iter()
                .any(|owned| owned.kind == OwnedPaneTaskKind::Client(ClientPaneTaskKind::Poll))
        {
            anyhow::bail!("pane {pane_id} already has an active poll task");
        }
        let producer_permit = match kind.admission_class() {
            Some(class) => Some(
                self.admission
                    .try_count(class, 1)
                    .with_context(|| format!("admit {kind:?} client pane task"))?,
            ),
            None => None,
        };
        let task = executor.handle().try_spawn(async move {
            let _producer_permit = producer_permit;
            future.await
        })?;
        workers.tasks.push(OwnedPaneTask {
            kind: OwnedPaneTaskKind::Client(kind),
            task,
        });
        Ok(())
    }

    /// Schedule one admitted pane mutation or refresh on the headless owner executor.
    ///
    /// The pane worker set retains the task and its permits through completion, pane removal, or
    /// runtime shutdown. Interactive runtimes deliberately use their synchronous pane path instead.
    pub fn try_spawn_pane_task_local<F>(
        &self,
        pane_id: PaneId,
        kind: PaneTaskKind,
        future: F,
    ) -> anyhow::Result<()>
    where
        F: Future<Output = anyhow::Result<()>> + 'static,
    {
        if self.admission.is_shutting_down() {
            anyhow::bail!("mux is shutting down");
        }
        if !self.is_main_thread() {
            anyhow::bail!("pane tasks must be scheduled on the mux owner thread");
        }
        let executor = self
            .headless_executor
            .as_ref()
            .ok_or_else(|| anyhow!("pane task scheduling requires a headless mux runtime"))?;

        if let Err(err) = self.reap_headless_tasks() {
            self.record_headless_task_error(err);
        }
        let mut pane_workers = self.pane_workers.lock();
        let workers = pane_workers
            .get_mut(&pane_id)
            .ok_or_else(|| anyhow!("pane {pane_id} has no registered task owner"))?;
        if kind == PaneTaskKind::Refresh
            && workers
                .tasks
                .iter()
                .any(|owned| owned.kind == OwnedPaneTaskKind::Pane(PaneTaskKind::Refresh))
        {
            return Ok(());
        }

        let permits = PaneTaskPermits::admit(&self.admission, &workers.input_admission, kind)?;
        let task = executor.handle().local().try_spawn_local(async move {
            let _permits = permits;
            future.await
        })?;
        workers.tasks.push(OwnedPaneTask {
            kind: OwnedPaneTaskKind::Pane(kind),
            task,
        });
        Ok(())
    }

    pub fn try_spawn_client_invalidation<F>(&self, future: F) -> anyhow::Result<()>
    where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let lifecycle = self.lifecycle.lock();
        if lifecycle.shutting_down {
            anyhow::bail!("mux is shutting down");
        }

        let Some(executor) = self.headless_executor.as_ref() else {
            promise::spawn::spawn_into_main_thread(async move {
                if let Err(err) = future.await {
                    log::error!("client invalidation task failed: {err:#}");
                }
            })
            .detach();
            return Ok(());
        };

        if let Err(err) = self.reap_headless_tasks() {
            log::error!("observed completed client task before scheduling invalidation: {err:#}");
            self.record_headless_task_error(err);
        }
        let permit = self
            .admission
            .try_count(CountClass::ClientInvalidation, 1)
            .context("admit client invalidation task")?;
        let task = executor.handle().try_spawn(async move {
            let _permit = permit;
            future.await
        })?;
        self.client_invalidation_tasks.lock().push(task);
        Ok(())
    }

    /// Schedule mux-owner work on the configured main-thread executor. Headless runtimes retain
    /// every task for error observation, cancellation, and shutdown join; interactive runtimes keep
    /// using their event-loop scheduler.
    pub fn try_spawn_runtime_task<F>(
        &self,
        schedule_context: &'static str,
        future: F,
    ) -> anyhow::Result<()>
    where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        self.try_spawn_runtime_task_with_priority(schedule_context, future, false)
    }

    pub fn try_spawn_runtime_task_low_priority<F>(
        &self,
        schedule_context: &'static str,
        future: F,
    ) -> anyhow::Result<()>
    where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        self.try_spawn_runtime_task_with_priority(schedule_context, future, true)
    }

    fn try_spawn_runtime_task_with_priority<F>(
        &self,
        schedule_context: &'static str,
        future: F,
        low_priority: bool,
    ) -> anyhow::Result<()>
    where
        F: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        if self.lifecycle.lock().shutting_down {
            anyhow::bail!("mux is shutting down");
        }

        let Some(executor) = self.headless_executor.as_ref() else {
            let task = async move {
                if let Err(err) = future.await {
                    log::error!("mux runtime task failed: {err:#}");
                }
            };
            if low_priority {
                promise::spawn::spawn_into_main_thread_with_low_priority(task).detach();
            } else {
                promise::spawn::spawn_into_main_thread(task).detach();
            }
            return Ok(());
        };

        if let Err(err) = self.reap_headless_tasks() {
            self.record_headless_task_error(err);
        }
        // The headless SimpleExecutor intentionally has one bounded FIFO: its legacy normal and
        // low-priority schedulers already share this same queue. Interactive runtimes preserve the
        // requested priority above through their distinct event-loop schedulers.
        let task = match executor.handle().try_spawn(future) {
            Ok(task) => task,
            Err(error) => {
                self.record_headless_task_error(
                    anyhow::Error::new(error.clone()).context(schedule_context),
                );
                return Err(anyhow::Error::new(error).context(schedule_context));
            }
        };
        self.headless_runtime_tasks.lock().push(task);
        Ok(())
    }

    /// Main-thread-only variant for futures that intentionally capture non-Send GUI/Lua state.
    pub fn try_spawn_runtime_task_local<F>(
        &self,
        schedule_context: &'static str,
        future: F,
    ) -> anyhow::Result<()>
    where
        F: Future<Output = anyhow::Result<()>> + 'static,
    {
        if self.lifecycle.lock().shutting_down {
            anyhow::bail!("mux is shutting down");
        }

        let Some(executor) = self.headless_executor.as_ref() else {
            promise::spawn::spawn(async move {
                if let Err(err) = future.await {
                    log::error!("mux runtime task failed: {err:#}");
                }
            })
            .detach();
            return Ok(());
        };
        if !self.is_main_thread() {
            anyhow::bail!("local mux runtime tasks must be scheduled on the owner thread");
        }

        if let Err(err) = self.reap_headless_tasks() {
            self.record_headless_task_error(err);
        }
        let task = match executor.handle().local().try_spawn_local(future) {
            Ok(task) => task,
            Err(error) => {
                self.record_headless_task_error(
                    anyhow::Error::new(error.clone()).context(schedule_context),
                );
                return Err(anyhow::Error::new(error).context(schedule_context));
            }
        };
        self.headless_runtime_tasks.lock().push(task);
        Ok(())
    }

    fn try_enqueue_headless_pane_output(self: &Arc<Self>, pane_id: PaneId) -> anyhow::Result<()> {
        if self.lifecycle.lock().shutting_down {
            anyhow::bail!("mux is shutting down");
        }
        let executor = self
            .headless_executor
            .as_ref()
            .ok_or_else(|| anyhow!("this mux is not owned by a headless executor"))?;
        let mut output_tasks = self.headless_pane_output_tasks.lock();
        if !output_tasks.pending.insert(pane_id) {
            return Ok(());
        }

        let permit = match self.admission.try_count(CountClass::PaneLifecycleEvent, 1) {
            Ok(permit) => permit,
            Err(err) => {
                output_tasks.pending.remove(&pane_id);
                return Err(err).context("admit headless pane-output notification");
            }
        };
        let mux = Arc::downgrade(self);
        let task = match executor.handle().try_spawn(async move {
            let _permit = permit;
            if let Some(mux) = mux.upgrade() {
                mux.headless_pane_output_tasks
                    .lock()
                    .pending
                    .remove(&pane_id);
                mux.notify(MuxNotification::PaneOutput(pane_id));
            }
            Ok(())
        }) {
            Ok(task) => task,
            Err(err) => {
                output_tasks.pending.remove(&pane_id);
                return Err(err).context("schedule headless pane-output notification");
            }
        };
        output_tasks.tasks.push(task);
        Ok(())
    }

    pub fn tick_headless(&self) -> anyhow::Result<()> {
        if !self.is_main_thread() {
            anyhow::bail!("headless mux executor must be ticked on its owner thread");
        }
        if let Some(err) = self.headless_task_error.lock().take() {
            return Err(err.context("owned headless task failed"));
        }
        self.reap_headless_tasks()?;
        self.headless_executor
            .as_ref()
            .ok_or_else(|| anyhow!("this mux is not owned by a headless executor"))?
            .tick()?;
        self.reap_headless_tasks()
    }

    fn record_headless_task_error(&self, error: anyhow::Error) {
        let mut first = self.headless_task_error.lock();
        if first.is_none() {
            *first = Some(error);
        }
    }

    fn reap_headless_tasks(&self) -> anyhow::Result<()> {
        let mut first_error = None;
        for workers in self.pane_workers.lock().values_mut() {
            if let Err(err) = reap_owned_pane_tasks(&mut workers.tasks) {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }

        if let Err(err) = reap_retiring_pane_tasks(&mut self.retiring_pane_tasks.lock()) {
            if first_error.is_none() {
                first_error = Some(err);
            }
        }

        if let Err(err) = reap_admitted_tasks(&mut self.client_invalidation_tasks.lock()) {
            if first_error.is_none() {
                first_error = Some(err);
            }
        }
        if let Err(err) = reap_admitted_tasks(&mut self.headless_runtime_tasks.lock()) {
            if first_error.is_none() {
                first_error = Some(err);
            }
        }
        if let Err(err) = reap_admitted_tasks(&mut self.headless_pane_output_tasks.lock().tasks) {
            if first_error.is_none() {
                first_error = Some(err);
            }
        }

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn get_default_workspace(&self) -> String {
        let config = configuration();
        config
            .default_workspace
            .as_deref()
            .unwrap_or(DEFAULT_WORKSPACE)
            .to_string()
    }

    pub fn is_main_thread(&self) -> bool {
        std::thread::current().id() == self.main_thread_id
    }

    fn recompute_pane_count(&self) {
        let mut count = HashMap::new();
        for window in self.windows.read().values() {
            let workspace = window.get_workspace();
            for tab in window.iter() {
                *count.entry(workspace.to_string()).or_insert(0) += match tab.count_panes() {
                    Some(n) => n,
                    None => {
                        // Busy: abort this and we'll retry later
                        return;
                    }
                };
            }
        }
        *self.num_panes_by_workspace.write() = count;
    }

    pub fn client_had_input(&self, client_id: &ClientId) {
        if let Some(info) = self.clients.write().get_mut(client_id) {
            info.update_last_input();
        }
        if let Some(agent) = &self.agent {
            agent.update_target();
        }
    }

    pub fn record_input_for_current_identity(&self) {
        if let Some(ident) = self.identity.read().as_ref() {
            self.client_had_input(ident);
        }
    }

    pub fn record_focus_for_current_identity(&self, pane_id: PaneId) {
        if let Some(ident) = self.identity.read().as_ref() {
            self.record_focus_for_client(ident, pane_id);
        }
    }

    pub fn resolve_focused_pane(
        &self,
        client_id: &ClientId,
    ) -> Option<(DomainId, WindowId, TabId, PaneId)> {
        let pane_id = self.clients.read().get(client_id)?.focused_pane_id?;
        let (domain, window, tab) = self.resolve_pane_id(pane_id)?;
        Some((domain, window, tab, pane_id))
    }

    pub fn record_focus_for_client(&self, client_id: &ClientId, pane_id: PaneId) {
        let mut prior = None;
        if let Some(info) = self.clients.write().get_mut(client_id) {
            prior = info.focused_pane_id;
            info.update_focused_pane(pane_id);
        }

        if prior == Some(pane_id) {
            return;
        }
        // Synthesize focus events
        if let Some(prior_id) = prior {
            if let Some(pane) = self.get_pane(prior_id) {
                pane.focus_changed(false);
            }
        }
        if let Some(pane) = self.get_pane(pane_id) {
            pane.focus_changed(true);
        }
    }

    /// Called by PaneFocused event handlers to reconcile a remote
    /// pane focus event and apply its effects locally
    pub fn focus_pane_and_containing_tab(&self, pane_id: PaneId) -> anyhow::Result<()> {
        let pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {pane_id} not found"))?;

        let (_domain, window_id, tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow::anyhow!("can't find {pane_id} in the mux"))?;

        // Focus/activate the containing tab within its window
        {
            let mut win = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow::anyhow!("window_id {window_id} not found"))?;
            let tab_idx = win
                .idx_by_id(tab_id)
                .ok_or_else(|| anyhow::anyhow!("tab {tab_id} not in {window_id}"))?;
            win.save_and_then_set_active(tab_idx);
        }

        // Focus/activate the pane locally
        let tab = self
            .get_tab(tab_id)
            .ok_or_else(|| anyhow::anyhow!("tab {tab_id} not found"))?;

        tab.set_active_pane(&pane);

        Ok(())
    }

    pub fn register_client(&self, client_id: Arc<ClientId>) {
        self.clients
            .write()
            .insert((*client_id).clone(), ClientInfo::new(client_id));
    }

    pub fn iter_clients(&self) -> Vec<ClientInfo> {
        self.clients
            .read()
            .values()
            .map(|info| info.clone())
            .collect()
    }

    /// Returns a list of the unique workspace names known to the mux.
    /// This is taken from all known windows.
    pub fn iter_workspaces(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .windows
            .read()
            .values()
            .map(|w| w.get_workspace().to_string())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Generate a new unique workspace name
    pub fn generate_workspace_name(&self) -> String {
        let used = self.iter_workspaces();
        for candidate in names::Generator::default() {
            if !used.contains(&candidate) {
                return candidate;
            }
        }
        unreachable!();
    }

    /// Returns the effective active workspace name
    pub fn active_workspace(&self) -> String {
        self.identity
            .read()
            .as_ref()
            .and_then(|ident| {
                self.clients
                    .read()
                    .get(&ident)
                    .and_then(|info| info.active_workspace.clone())
            })
            .unwrap_or_else(|| self.get_default_workspace())
    }

    /// Returns the effective active workspace name for a given client
    pub fn active_workspace_for_client(&self, ident: &Arc<ClientId>) -> String {
        self.clients
            .read()
            .get(&ident)
            .and_then(|info| info.active_workspace.clone())
            .unwrap_or_else(|| self.get_default_workspace())
    }

    pub fn set_active_workspace_for_client(&self, ident: &Arc<ClientId>, workspace: &str) {
        let mut clients = self.clients.write();
        if let Some(info) = clients.get_mut(&ident) {
            info.active_workspace.replace(workspace.to_string());
            self.notify(MuxNotification::ActiveWorkspaceChanged(ident.clone()));
        }
    }

    /// Assigns the active workspace name for the current identity
    pub fn set_active_workspace(&self, workspace: &str) {
        if let Some(ident) = self.identity.read().clone() {
            self.set_active_workspace_for_client(&ident, workspace);
        }
    }

    pub fn rename_workspace(&self, old_workspace: &str, new_workspace: &str) {
        if old_workspace == new_workspace {
            return;
        }
        self.notify(MuxNotification::WorkspaceRenamed {
            old_workspace: old_workspace.to_string(),
            new_workspace: new_workspace.to_string(),
        });

        for window in self.windows.write().values_mut() {
            if window.get_workspace() == old_workspace {
                window.set_workspace(new_workspace);
            }
        }
        self.recompute_pane_count();
        for client in self.clients.write().values_mut() {
            if client.active_workspace.as_deref() == Some(old_workspace) {
                client.active_workspace.replace(new_workspace.to_string());
                self.notify(MuxNotification::ActiveWorkspaceChanged(
                    client.client_id.clone(),
                ));
            }
        }
    }

    /// Overrides the current client identity.
    /// Returns `IdentityHolder` which will restore the prior identity
    /// when it is dropped.
    /// This can be used to change the identity for the duration of a block.
    pub fn with_identity(&self, id: Option<Arc<ClientId>>) -> IdentityHolder {
        let prior = self.replace_identity(id);
        IdentityHolder { prior }
    }

    /// Replace the identity, returning the prior identity
    pub fn replace_identity(&self, id: Option<Arc<ClientId>>) -> Option<Arc<ClientId>> {
        std::mem::replace(&mut *self.identity.write(), id)
    }

    /// Returns the active identity
    pub fn active_identity(&self) -> Option<Arc<ClientId>> {
        self.identity.read().clone()
    }

    pub fn unregister_client(&self, client_id: &ClientId) {
        self.clients.write().remove(client_id);
    }

    pub fn subscribe<F>(&self, subscriber: F)
    where
        F: Fn(MuxNotification) -> bool + 'static + Send + Sync,
    {
        let sub_id = LAST_SUBSCRIBER_ID.fetch_add(1, Ordering::Relaxed);
        self.subscribers
            .write()
            .insert(sub_id, Box::new(subscriber));
    }

    pub fn notify(&self, notification: MuxNotification) {
        let mut subscribers = self.subscribers.write();
        subscribers.retain(|_, notify| notify(notification.clone()));
    }

    fn notify_pane_output_from_any_thread(pane_id: PaneId) {
        if let Some(mux) = Mux::try_get() {
            if mux.is_main_thread() {
                mux.notify(MuxNotification::PaneOutput(pane_id));
                return;
            }
            if mux.headless_executor.is_some() {
                if mux.lifecycle.lock().shutting_down {
                    return;
                }
                if let Err(err) = mux.try_enqueue_headless_pane_output(pane_id) {
                    mux.record_headless_task_error(
                        err.context("queue off-thread headless pane-output notification"),
                    );
                }
                return;
            }
        }
        promise::spawn::spawn_into_main_thread(async move {
            if let Some(mux) = Mux::try_get() {
                mux.notify(MuxNotification::PaneOutput(pane_id));
            }
        })
        .detach();
    }

    pub fn default_domain(&self) -> Arc<dyn Domain> {
        self.default_domain.read().as_ref().map(Arc::clone).unwrap()
    }

    pub fn set_default_domain(&self, domain: &Arc<dyn Domain>) {
        *self.default_domain.write() = Some(Arc::clone(domain));
    }

    pub fn get_domain(&self, id: DomainId) -> Option<Arc<dyn Domain>> {
        self.domains.read().get(&id).cloned()
    }

    pub fn get_domain_by_name(&self, name: &str) -> Option<Arc<dyn Domain>> {
        self.domains_by_name.read().get(name).cloned()
    }

    pub fn add_domain(&self, domain: &Arc<dyn Domain>) {
        if self.default_domain.read().is_none() {
            *self.default_domain.write() = Some(Arc::clone(domain));
        }
        self.domains
            .write()
            .insert(domain.domain_id(), Arc::clone(domain));
        self.domains_by_name
            .write()
            .insert(domain.domain_name().to_string(), Arc::clone(domain));
    }

    pub fn set_mux(mux: &Arc<Mux>) -> anyhow::Result<()> {
        let mut process_mux = MUX.lock();
        match &*process_mux {
            ProcessMux::Vacant => {}
            ProcessMux::Active(_) => bail!("the process-global mux is already initialized"),
            ProcessMux::Shutdown => {
                bail!("the process-global mux was shut down and cannot be restarted")
            }
        }

        mux.start_pane_lifecycle()?;
        *process_mux = ProcessMux::Active(Arc::clone(mux));
        Ok(())
    }

    pub fn shutdown() {
        let mux = {
            let mut process_mux = MUX.lock();
            match std::mem::replace(&mut *process_mux, ProcessMux::Shutdown) {
                ProcessMux::Active(mux) => Some(mux),
                ProcessMux::Vacant | ProcessMux::Shutdown => None,
            }
        };
        if let Some(mux) = mux {
            mux.shutdown_runtime();
        }
    }

    fn shutdown_runtime(&self) {
        let (
            mut workers,
            mut pane_lifecycle,
            tab_permits,
            mut retiring,
            mut runtime_tasks,
            mut invalidations,
            mut pane_output_tasks,
        ) = {
            let mut lifecycle = self.lifecycle.lock();
            if lifecycle.shutting_down {
                return;
            }
            lifecycle.shutting_down = true;

            (
                std::mem::take(&mut *self.pane_workers.lock()),
                self.pane_lifecycle.lock().take(),
                std::mem::take(&mut *self.tab_permits.lock()),
                std::mem::take(&mut *self.retiring_pane_tasks.lock()),
                std::mem::take(&mut *self.headless_runtime_tasks.lock()),
                std::mem::take(&mut *self.client_invalidation_tasks.lock()),
                std::mem::take(&mut *self.headless_pane_output_tasks.lock()),
            )
        };
        if let Some(coordinator) = pane_lifecycle.as_mut() {
            coordinator.sender.take();
        }

        for worker_set in workers.values() {
            for owned in &worker_set.tasks {
                owned.task.cancel();
            }
        }
        for task in &invalidations {
            task.cancel();
        }
        for task in &runtime_tasks {
            task.cancel();
        }
        for task in &pane_output_tasks.tasks {
            task.cancel();
        }
        for retired in &retiring {
            for owned in &retired.tasks {
                owned.task.cancel();
            }
        }

        while !runtime_tasks.is_empty()
            || !invalidations.is_empty()
            || !retiring.is_empty()
            || !pane_output_tasks.tasks.is_empty()
            || workers
                .values()
                .any(|worker_set| !worker_set.tasks.is_empty())
        {
            if let Err(err) = reap_admitted_tasks(&mut runtime_tasks) {
                log::error!("mux runtime task failed during shutdown: {err:#}");
            }
            if let Err(err) = reap_admitted_tasks(&mut invalidations) {
                log::error!("client invalidation task failed during mux shutdown: {err:#}");
            }
            if let Err(err) = reap_admitted_tasks(&mut pane_output_tasks.tasks) {
                log::error!("pane-output task failed during mux shutdown: {err:#}");
            }
            for (pane_id, worker_set) in &mut workers {
                if let Err(err) = reap_owned_pane_tasks(&mut worker_set.tasks) {
                    log::error!("pane {pane_id} task failed during mux shutdown: {err:#}");
                }
            }
            let mut index = 0;
            while index < retiring.len() {
                if let Err(err) = reap_owned_pane_tasks(&mut retiring[index].tasks) {
                    log::error!(
                        "pane {} task failed during mux shutdown: {err:#}",
                        retiring[index].pane_id
                    );
                }
                if retiring[index].tasks.is_empty() {
                    retiring.swap_remove(index);
                } else {
                    index += 1;
                }
            }
            if runtime_tasks.is_empty()
                && invalidations.is_empty()
                && retiring.is_empty()
                && pane_output_tasks.tasks.is_empty()
                && workers
                    .values()
                    .all(|worker_set| worker_set.tasks.is_empty())
            {
                break;
            }
            let Some(executor) = self.headless_executor.as_ref() else {
                log::error!("headless tasks have no executor join owner");
                break;
            };
            if let Err(err) = executor.tick() {
                log::error!("headless executor failed while joining mux tasks: {err:#}");
            }
        }

        self.admission.begin_shutdown();
        for (pane_id, worker_set) in &mut workers {
            if let Err(err) = worker_set.shutdown(true) {
                log::error!("pane {pane_id} worker shutdown failed during mux shutdown: {err:#}");
            }
        }
        drop(workers);

        if let Some(mut coordinator) = pane_lifecycle {
            if let Some(worker) = coordinator.worker.take() {
                match worker.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        log::error!("pane lifecycle coordinator failed: {err:#}");
                    }
                    Err(_) => log::error!("pane lifecycle coordinator panicked"),
                }
            }
        }

        // Pane and tab admission remains held until every corresponding worker has stopped.
        drop(tab_permits);
    }

    fn start_pane_lifecycle(self: &Arc<Self>) -> anyhow::Result<()> {
        let mut slot = self.pane_lifecycle.lock();
        if slot.is_some() {
            return Ok(());
        }

        let coordinator_permit = self
            .admission
            .try_count(CountClass::ExecutorRunnable, 1)
            .context("admit pane lifecycle coordinator")?;
        let (sender, receiver): (
            SyncSender<PaneLifecycleEvent>,
            SyncReceiver<PaneLifecycleEvent>,
        ) = sync_channel(MAX_PANES);
        let mux = Arc::downgrade(self);
        let worker = thread::Builder::new()
            .name("mux-pane-lifecycle".to_string())
            .spawn(move || {
                let _permit = coordinator_permit;
                while let Ok(event) = receiver.recv() {
                    let Some(mux) = mux.upgrade() else {
                        break;
                    };
                    if mux.admission.is_shutting_down() {
                        continue;
                    }
                    match event.action {
                        PaneLifecycleAction::Prune => mux.prune_dead_windows(),
                        PaneLifecycleAction::Remove(pane_id) => mux.remove_pane(pane_id),
                    }
                }
                Ok(())
            })
            .context("spawn pane lifecycle coordinator")?;
        *slot = Some(PaneLifecycleCoordinator {
            sender: Some(sender),
            worker: Some(worker),
        });
        Ok(())
    }

    fn pane_lifecycle_sender(&self) -> anyhow::Result<SyncSender<PaneLifecycleEvent>> {
        self.pane_lifecycle
            .lock()
            .as_ref()
            .and_then(|coordinator| coordinator.sender.as_ref())
            .cloned()
            .ok_or_else(|| anyhow!("pane lifecycle coordinator is not running"))
    }

    pub fn get() -> Arc<Mux> {
        Self::try_get().unwrap()
    }

    pub fn try_get() -> Option<Arc<Mux>> {
        match &*MUX.lock() {
            ProcessMux::Active(mux) => Some(Arc::clone(mux)),
            ProcessMux::Vacant | ProcessMux::Shutdown => None,
        }
    }

    pub fn get_pane(&self, pane_id: PaneId) -> Option<Arc<dyn Pane>> {
        self.panes.read().get(&pane_id).map(Arc::clone)
    }

    pub fn get_tab(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        self.tabs.read().get(&tab_id).map(Arc::clone)
    }

    pub fn add_pane(&self, pane: &Arc<dyn Pane>) -> Result<(), Error> {
        let lifecycle = self.lifecycle.lock();
        if lifecycle.shutting_down {
            anyhow::bail!("mux is shutting down");
        }
        let added = self.add_pane_locked(pane)?;
        drop(lifecycle);

        if added {
            self.recompute_pane_count();
            self.notify(MuxNotification::PaneAdded(pane.pane_id()));
        }
        Ok(())
    }

    /// Adds a pane while the caller holds `self.lifecycle`.
    fn add_pane_locked(&self, pane: &Arc<dyn Pane>) -> Result<bool, Error> {
        if self.panes.read().contains_key(&pane.pane_id()) {
            return Ok(false);
        }

        let permit = self.admission.try_pane().context("pane admission")?;

        let clipboard: Arc<dyn Clipboard> = Arc::new(MuxClipboard {
            pane_id: pane.pane_id(),
        });
        pane.set_clipboard(&clipboard);

        let downloader: Arc<dyn DownloadHandler> = Arc::new(MuxDownloader {});
        pane.set_download_handler(&downloader);

        let reader = pane.reader()?;
        let lifecycle = self.pane_lifecycle_sender()?;
        self.panes.write().insert(pane.pane_id(), Arc::clone(pane));
        let pane_id = pane.pane_id();
        let banner = self.banner.read().clone();
        match PaneWorkerSet::start(
            Arc::clone(pane),
            banner,
            reader,
            Arc::clone(&self.admission),
            lifecycle,
            permit,
        ) {
            Ok(workers) => {
                self.pane_workers.lock().insert(pane_id, workers);
            }
            Err(err) => {
                self.panes.write().remove(&pane_id);
                pane.kill();
                if let Err(cancel_err) = pane.cancel_reader() {
                    log::error!(
                        "failed to cancel pane {pane_id} reader after worker startup failed: \
                         {cancel_err:#}"
                    );
                }
                if let Err(join_err) = pane.join_child_waiter() {
                    log::error!(
                        "failed to join pane {pane_id} child after worker startup failed: \
                         {join_err:#}"
                    );
                }
                return Err(err.context("start pane workers"));
            }
        }
        Ok(true)
    }

    pub fn add_tab_no_panes(&self, tab: &Arc<Tab>) -> Result<(), Error> {
        let lifecycle = self.lifecycle.lock();
        if lifecycle.shutting_down {
            anyhow::bail!("mux is shutting down");
        }
        let added = self.add_tab_no_panes_locked(tab)?;
        drop(lifecycle);
        if added {
            self.recompute_pane_count();
        }
        Ok(())
    }

    /// Adds a tab while the caller holds `self.lifecycle`; returns whether this call owns it.
    fn add_tab_no_panes_locked(&self, tab: &Arc<Tab>) -> Result<bool, Error> {
        if self.tabs.read().contains_key(&tab.tab_id()) {
            return Ok(false);
        }
        let permit = self.admission.try_tab().context("tab admission")?;
        self.tabs.write().insert(tab.tab_id(), Arc::clone(tab));
        self.tab_permits.lock().insert(tab.tab_id(), permit);
        Ok(true)
    }

    pub fn add_tab_and_active_pane(&self, tab: &Arc<Tab>) -> Result<(), Error> {
        let lifecycle = self.lifecycle.lock();
        if lifecycle.shutting_down {
            anyhow::bail!("mux is shutting down");
        }
        let added_tab = self.add_tab_no_panes_locked(tab)?;
        let pane = match tab.get_active_pane() {
            Some(pane) => pane,
            None => {
                if added_tab {
                    self.tabs.write().remove(&tab.tab_id());
                    self.tab_permits.lock().remove(&tab.tab_id());
                }
                anyhow::bail!("tab MUST have an active pane");
            }
        };
        let added_pane = match self.add_pane_locked(&pane) {
            Ok(added) => added,
            Err(err) => {
                if added_tab {
                    self.tabs.write().remove(&tab.tab_id());
                    self.tab_permits.lock().remove(&tab.tab_id());
                }
                return Err(err);
            }
        };
        drop(lifecycle);

        self.recompute_pane_count();
        if added_pane {
            self.notify(MuxNotification::PaneAdded(pane.pane_id()));
        }
        Ok(())
    }

    fn remove_pane_internal(&self, pane_id: PaneId, removal: PaneRemoval) {
        log::debug!("removing pane {}", pane_id);
        let lifecycle = self.lifecycle.lock();
        let mut pane_workers = self.pane_workers.lock();
        let mut changed = false;
        let mut worker_to_stop = None;
        let mut orphaned_pane = None;
        // Bind the lookup before entering the branch so its read guard is released before removal.
        // An `if let` scrutinee temporary lives through the body in Rust 2021.
        let pane = self.panes.read().get(&pane_id).cloned();
        if let Some(pane) = pane {
            let kill_process = removal.should_kill() || pane.kill_process_on_unregister();
            if kill_process {
                pane.kill();
            }
            self.panes.write().remove(&pane_id);
            if let Some(mut workers) = pane_workers.remove(&pane_id) {
                let pane_tasks = std::mem::take(&mut workers.tasks);
                let retiring_tasks =
                    retire_pane_tasks(pane_id, removal, pane_tasks, workers.take_pane_permit());
                self.retiring_pane_tasks.lock().push(retiring_tasks);
                worker_to_stop = Some(workers);
            } else if kill_process {
                orphaned_pane = Some(pane);
            }
            changed = true;
        }

        drop(lifecycle);
        drop(pane_workers);

        // Pane workers can publish their final parsed output while stopping. Join them only after
        // releasing the lifecycle locks needed by that notification path.
        if let Some(mut workers) = worker_to_stop {
            if let Err(err) = workers.shutdown(false) {
                log::error!("pane {pane_id} worker shutdown failed: {err:#}");
            }
        } else if let Some(pane) = orphaned_pane {
            log::debug!("joining pane {} without a registered worker set", pane_id);
            if let Err(err) = pane.join_child_waiter() {
                log::error!("pane {pane_id} child-waiter join failed: {err:#}");
            }
        }

        if changed {
            self.notify(MuxNotification::PaneRemoved(pane_id));
            if let Err(err) = reap_retiring_pane_tasks(&mut self.retiring_pane_tasks.lock()) {
                self.record_headless_task_error(
                    err.context("reap pane cleanup after removal publication"),
                );
            }
            self.recompute_pane_count();
        }
    }

    fn remove_tab_internal(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        log::debug!("remove_tab_internal tab {}", tab_id);

        let tab = self.tabs.write().remove(&tab_id)?;

        if let Some(mut windows) = self.windows.try_write() {
            for w in windows.values_mut() {
                w.remove_by_id(tab_id);
            }
        }

        let mut pane_ids = vec![];
        for pos in tab.iter_panes_ignoring_zoom() {
            pane_ids.push(pos.pane.pane_id());
        }
        log::debug!("panes to remove: {pane_ids:?}");
        for pane_id in pane_ids {
            self.remove_pane_internal(pane_id, PaneRemoval::Kill);
        }
        self.tab_permits.lock().remove(&tab_id);
        self.recompute_pane_count();

        Some(tab)
    }

    fn remove_window_internal(&self, window_id: WindowId) {
        log::debug!("remove_window_internal {}", window_id);

        let window = self.windows.write().remove(&window_id);
        if let Some(window) = window {
            // Gather all the domains referenced by this window
            let mut domains_of_window = HashSet::new();
            for tab in window.iter() {
                for pane in tab.iter_panes_ignoring_zoom() {
                    domains_of_window.insert(pane.pane.domain_id());
                }
            }

            for domain_id in domains_of_window {
                if let Some(domain) = self.get_domain(domain_id) {
                    if domain.detachable() {
                        log::info!("detaching domain");
                        if let Err(err) = domain.detach() {
                            log::error!(
                                "while detaching domain {domain_id} {}: {err:#}",
                                domain.domain_name()
                            );
                        }
                    }
                }
            }

            for tab in window.iter() {
                self.remove_tab_internal(tab.tab_id());
            }
            self.notify(MuxNotification::WindowRemoved(window_id));
        }
        self.recompute_pane_count();
    }

    pub fn remove_pane(&self, pane_id: PaneId) {
        self.remove_pane_internal(pane_id, PaneRemoval::Kill);
        self.prune_dead_windows();
    }

    pub fn unregister_pane(&self, pane_id: PaneId) {
        self.remove_pane_internal(pane_id, PaneRemoval::Unregister);
        self.prune_dead_windows();
    }

    pub fn remove_tab(&self, tab_id: TabId) -> Option<Arc<Tab>> {
        let tab = self.remove_tab_internal(tab_id);
        self.prune_dead_windows();
        tab
    }

    pub fn prune_dead_windows(&self) {
        if Activity::count() > 0 {
            log::trace!("prune_dead_windows: Activity::count={}", Activity::count());
            return;
        }
        let live_tab_ids: Vec<TabId> = self.tabs.read().keys().cloned().collect();
        let mut dead_windows = vec![];
        let mut dead_pane_ids = vec![];
        let dead_tab_ids: Vec<TabId>;

        {
            let mut windows = match self.windows.try_write() {
                Some(w) => w,
                None => {
                    // It's ok if our caller already locked it; we can prune later.
                    log::trace!("prune_dead_windows: self.windows already borrowed");
                    return;
                }
            };
            for (window_id, win) in windows.iter_mut() {
                dead_pane_ids.extend(win.prune_dead_tabs(&live_tab_ids));
                if win.is_empty() {
                    log::trace!("prune_dead_windows: window is now empty");
                    dead_windows.push(*window_id);
                }
            }

            dead_tab_ids = self
                .tabs
                .read()
                .iter()
                .filter_map(|(&id, tab)| if tab.is_dead() { Some(id) } else { None })
                .collect();
        }

        for pane_id in dead_pane_ids {
            self.remove_pane_internal(pane_id, PaneRemoval::Kill);
        }

        for tab_id in dead_tab_ids {
            log::trace!("tab {} is dead", tab_id);
            self.remove_tab_internal(tab_id);
        }

        for window_id in dead_windows {
            log::trace!("window {} is dead", window_id);
            self.remove_window_internal(window_id);
        }

        if self.is_empty() {
            log::trace!("prune_dead_windows: is_empty, send MuxNotification::Empty");
            self.notify(MuxNotification::Empty);
        } else {
            log::trace!("prune_dead_windows: not empty");
        }
    }

    pub fn kill_window(&self, window_id: WindowId) {
        self.remove_window_internal(window_id);
        self.prune_dead_windows();
    }

    pub fn get_window(&self, window_id: WindowId) -> Option<MappedRwLockReadGuard<'_, Window>> {
        if !self.windows.read().contains_key(&window_id) {
            return None;
        }
        Some(RwLockReadGuard::map(self.windows.read(), |windows| {
            windows.get(&window_id).unwrap()
        }))
    }

    pub fn get_window_mut(
        &self,
        window_id: WindowId,
    ) -> Option<MappedRwLockWriteGuard<'_, Window>> {
        if !self.windows.read().contains_key(&window_id) {
            return None;
        }
        Some(RwLockWriteGuard::map(self.windows.write(), |windows| {
            windows.get_mut(&window_id).unwrap()
        }))
    }

    pub fn get_active_tab_for_window(&self, window_id: WindowId) -> Option<Arc<Tab>> {
        let window = self.get_window(window_id)?;
        window.get_active().map(Arc::clone)
    }

    pub fn new_empty_window(
        &self,
        workspace: Option<String>,
        position: Option<GuiPosition>,
    ) -> MuxWindowBuilder {
        let window = Window::new(workspace, position);
        let window_id = window.window_id();
        self.windows.write().insert(window_id, window);
        MuxWindowBuilder {
            window_id,
            activity: Some(Activity::new()),
            notified: false,
        }
    }

    pub fn add_tab_to_window(&self, tab: &Arc<Tab>, window_id: WindowId) -> anyhow::Result<()> {
        let tab_id = tab.tab_id();
        {
            let mut window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("add_tab_to_window: no such window_id {}", window_id))?;
            window.push(tab);
        }
        self.recompute_pane_count();
        self.notify(MuxNotification::TabAddedToWindow { tab_id, window_id });
        Ok(())
    }

    /// Returns the ID of the window containing the given tab ID, if any.
    pub fn window_containing_tab(&self, tab_id: TabId) -> Option<WindowId> {
        for w in self.windows.read().values() {
            for t in w.iter() {
                if t.tab_id() == tab_id {
                    return Some(w.window_id());
                }
            }
        }
        None
    }

    pub fn is_empty(&self) -> bool {
        self.panes.read().is_empty()
    }

    pub fn is_workspace_empty(&self, workspace: &str) -> bool {
        *self
            .num_panes_by_workspace
            .read()
            .get(workspace)
            .unwrap_or(&0)
            == 0
    }

    pub fn is_active_workspace_empty(&self) -> bool {
        let workspace = self.active_workspace();
        self.is_workspace_empty(&workspace)
    }

    pub fn iter_panes(&self) -> Vec<Arc<dyn Pane>> {
        self.panes
            .read()
            .iter()
            .map(|(_, v)| Arc::clone(v))
            .collect()
    }

    pub fn iter_windows_in_workspace(&self, workspace: &str) -> Vec<WindowId> {
        let mut windows: Vec<WindowId> = self
            .windows
            .read()
            .iter()
            .filter_map(|(k, w)| {
                if w.get_workspace() == workspace {
                    Some(k)
                } else {
                    None
                }
            })
            .cloned()
            .collect();
        windows.sort();
        windows
    }

    pub fn iter_windows(&self) -> Vec<WindowId> {
        self.windows.read().keys().cloned().collect()
    }

    pub fn iter_domains(&self) -> Vec<Arc<dyn Domain>> {
        self.domains.read().values().cloned().collect()
    }

    pub fn resolve_pane_id(&self, pane_id: PaneId) -> Option<(DomainId, WindowId, TabId)> {
        let mut ids = None;
        for tab in self.tabs.read().values() {
            for p in tab.iter_panes_ignoring_zoom() {
                if p.pane.pane_id() == pane_id {
                    ids = Some((tab.tab_id(), p.pane.domain_id()));
                    break;
                }
            }
        }
        let (tab_id, domain_id) = ids?;
        let window_id = self.window_containing_tab(tab_id)?;
        Some((domain_id, window_id, tab_id))
    }

    pub fn domain_was_detached(&self, domain: DomainId) {
        let mut dead_panes = vec![];
        for pane in self.panes.read().values() {
            if pane.domain_id() == domain {
                dead_panes.push(pane.pane_id());
            }
        }

        {
            let mut windows = self.windows.write();
            for (_, win) in windows.iter_mut() {
                for tab in win.iter() {
                    tab.kill_panes_in_domain(domain);
                }
            }
        }

        log::info!("domain detached panes: {:?}", dead_panes);
        for pane_id in dead_panes {
            self.remove_pane_internal(pane_id, PaneRemoval::Unregister);
        }

        self.prune_dead_windows();
    }

    pub fn set_banner(&self, banner: Option<String>) {
        *self.banner.write() = banner;
    }

    pub fn resolve_spawn_tab_domain(
        &self,
        // TODO: disambiguate with TabId
        pane_id: Option<PaneId>,
        domain: &config::keyassignment::SpawnTabDomain,
    ) -> anyhow::Result<Arc<dyn Domain>> {
        let domain = match domain {
            SpawnTabDomain::DefaultDomain => self.default_domain(),
            SpawnTabDomain::CurrentPaneDomain => match pane_id {
                Some(pane_id) => {
                    let (pane_domain_id, _window_id, _tab_id) = self
                        .resolve_pane_id(pane_id)
                        .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;
                    self.get_domain(pane_domain_id)
                        .expect("resolve_pane_id to give valid domain_id")
                }
                None => self.default_domain(),
            },
            SpawnTabDomain::DomainId(domain_id) => self
                .get_domain(*domain_id)
                .ok_or_else(|| anyhow!("domain id {} is invalid", domain_id))?,
            SpawnTabDomain::DomainName(name) => {
                self.get_domain_by_name(&name).ok_or_else(|| {
                    let names: Vec<String> = self
                        .domains_by_name
                        .read()
                        .keys()
                        .map(|name| format!("\"{name}\""))
                        .collect();
                    anyhow!(
                        "domain name \"{name}\" is invalid. Possible names are {}.",
                        names.join(", ")
                    )
                })?
            }
        };
        Ok(domain)
    }

    fn resolve_cwd(
        &self,
        command_dir: Option<String>,
        pane: Option<Arc<dyn Pane>>,
        target_domain: DomainId,
        policy: CachePolicy,
    ) -> Option<String> {
        command_dir.or_else(|| {
            match pane {
                Some(pane) if pane.domain_id() == target_domain => pane
                    .get_current_working_dir(policy)
                    .and_then(|url| {
                        percent_decode_str(url.path())
                            .decode_utf8()
                            .ok()
                            .map(|path| path.into_owned())
                    })
                    .map(|path| {
                        // On Windows the file URI can produce a path like:
                        // `/C:\Users` which is valid in a file URI, but the leading slash
                        // is not liked by the windows file APIs, so we strip it off here.
                        let bytes = path.as_bytes();
                        if bytes.len() > 2 && bytes[0] == b'/' && bytes[2] == b':' {
                            path[1..].to_owned()
                        } else {
                            path
                        }
                    }),
                _ => None,
            }
        })
    }

    pub async fn split_pane(
        &self,
        // TODO: disambiguate with TabId
        pane_id: PaneId,
        request: SplitRequest,
        source: SplitSource,
        domain: config::keyassignment::SpawnTabDomain,
    ) -> anyhow::Result<(Arc<dyn Pane>, TerminalSize)> {
        let (_pane_domain_id, window_id, tab_id) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} invalid", pane_id))?;

        let domain = self
            .resolve_spawn_tab_domain(Some(pane_id), &domain)
            .context("resolve_spawn_tab_domain")?;

        if domain.state() == DomainState::Detached {
            domain.attach(Some(window_id)).await?;
        }

        let current_pane = self
            .get_pane(pane_id)
            .ok_or_else(|| anyhow!("pane_id {} is invalid", pane_id))?;
        let term_config = current_pane.get_config();

        let source = match source {
            SplitSource::Spawn {
                command,
                command_dir,
            } => SplitSource::Spawn {
                command,
                command_dir: self.resolve_cwd(
                    command_dir,
                    Some(Arc::clone(&current_pane)),
                    domain.domain_id(),
                    CachePolicy::FetchImmediate,
                ),
            },
            other => other,
        };

        let pane = domain.split_pane(source, tab_id, pane_id, request).await?;
        if let Some(config) = term_config {
            pane.set_config(config);
        }

        // FIXME: clipboard

        let dims = pane.get_dimensions();

        let size = TerminalSize {
            cols: dims.cols,
            rows: dims.viewport_rows,
            pixel_height: 0, // FIXME: split pane pixel dimensions
            pixel_width: 0,
            dpi: dims.dpi,
        };

        Ok((pane, size))
    }

    pub async fn move_pane_to_new_tab(
        &self,
        pane_id: PaneId,
        window_id: Option<WindowId>,
        workspace_for_new_window: Option<String>,
    ) -> anyhow::Result<(Arc<Tab>, WindowId)> {
        let (domain_id, _src_window, src_tab) = self
            .resolve_pane_id(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {} not found", pane_id))?;

        let domain = self
            .get_domain(domain_id)
            .ok_or_else(|| anyhow::anyhow!("domain {domain_id} of pane {pane_id} not found"))?;

        if let Some((tab, window_id)) = domain
            .move_pane_to_new_tab(pane_id, window_id, workspace_for_new_window.clone())
            .await?
        {
            return Ok((tab, window_id));
        }

        let src_tab = match self.get_tab(src_tab) {
            Some(t) => t,
            None => anyhow::bail!("Invalid tab id {}", src_tab),
        };

        let window_builder;
        let (window_id, size) = if let Some(window_id) = window_id {
            let window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
            let tab = window
                .get_active()
                .ok_or_else(|| anyhow!("window {} has no tabs", window_id))?;
            let size = tab.get_size();

            (window_id, size)
        } else {
            window_builder = self.new_empty_window(workspace_for_new_window, None);
            (*window_builder, src_tab.get_size())
        };

        let pane = src_tab
            .remove_pane(pane_id)
            .ok_or_else(|| anyhow::anyhow!("pane {} wasn't in its containing tab!?", pane_id))?;

        let tab = Arc::new(Tab::new(&size));
        tab.assign_pane(&pane);
        pane.resize(size)?;
        self.add_tab_and_active_pane(&tab)?;
        self.add_tab_to_window(&tab, window_id)?;

        if src_tab.is_dead() {
            self.remove_tab(src_tab.tab_id());
        }

        Ok((tab, window_id))
    }

    pub async fn spawn_tab_or_window(
        &self,
        window_id: Option<WindowId>,
        domain: SpawnTabDomain,
        command: Option<CommandBuilder>,
        command_dir: Option<String>,
        size: TerminalSize,
        current_pane_id: Option<PaneId>,
        workspace_for_new_window: String,
        window_position: Option<GuiPosition>,
    ) -> anyhow::Result<(Arc<Tab>, Arc<dyn Pane>, WindowId)> {
        let domain = self
            .resolve_spawn_tab_domain(current_pane_id, &domain)
            .context("resolve_spawn_tab_domain")?;

        let window_builder;
        let term_config;

        let (window_id, size) = if let Some(window_id) = window_id {
            let window = self
                .get_window_mut(window_id)
                .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
            let tab = window
                .get_active()
                .ok_or_else(|| anyhow!("window {} has no tabs", window_id))?;
            let pane = tab
                .get_active_pane()
                .ok_or_else(|| anyhow!("active tab in window {} has no panes", window_id))?;
            term_config = pane.get_config();

            let size = tab.get_size();

            (window_id, size)
        } else {
            term_config = None;
            window_builder = self.new_empty_window(Some(workspace_for_new_window), window_position);
            (*window_builder, size)
        };

        if domain.state() == DomainState::Detached {
            domain.attach(Some(window_id)).await?;
        }

        let cwd = self.resolve_cwd(
            command_dir,
            match current_pane_id {
                Some(id) => {
                    // Only use the cwd from the current pane if the domain
                    // is the same as the one we are spawning into
                    let (current_domain_id, _, _) = self
                        .resolve_pane_id(id)
                        .ok_or_else(|| anyhow!("pane_id {} invalid", id))?;
                    if current_domain_id == domain.domain_id() {
                        self.get_pane(id)
                    } else {
                        None
                    }
                }
                None => None,
            },
            domain.domain_id(),
            CachePolicy::FetchImmediate,
        );

        let tab = domain
            .spawn(size, command.clone(), cwd.clone(), window_id)
            .await
            .with_context(|| {
                format!(
                    "Spawning in domain `{}`: {size:?} command={command:?} cwd={cwd:?}",
                    domain.domain_name()
                )
            })?;

        let pane = tab
            .get_active_pane()
            .ok_or_else(|| anyhow!("missing active pane on tab!?"))?;

        if let Some(config) = term_config {
            pane.set_config(config);
        }

        // FIXME: clipboard?

        let mut window = self
            .get_window_mut(window_id)
            .ok_or_else(|| anyhow!("no such window!?"))?;
        if let Some(idx) = window.idx_by_id(tab.tab_id()) {
            window.save_and_then_set_active(idx);
        }

        Ok((tab, pane, window_id))
    }
}

pub struct IdentityHolder {
    prior: Option<Arc<ClientId>>,
}

impl Drop for IdentityHolder {
    fn drop(&mut self) {
        if let Some(mux) = Mux::try_get() {
            mux.replace_identity(self.prior.take());
        }
    }
}

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum SessionTerminated {
    #[error("Process exited: {:?}", status)]
    ProcessStatus { status: ExitStatus },
    #[error("Error: {:?}", err)]
    Error { err: Error },
    #[error("Window Closed")]
    WindowClosed,
}

pub(crate) fn terminal_size_to_pty_size(size: TerminalSize) -> anyhow::Result<PtySize> {
    Ok(PtySize {
        rows: size.rows.try_into()?,
        cols: size.cols.try_into()?,
        pixel_height: size.pixel_height.try_into()?,
        pixel_width: size.pixel_width.try_into()?,
    })
}

struct MuxClipboard {
    pane_id: PaneId,
}

impl Clipboard for MuxClipboard {
    fn set_contents(
        &self,
        selection: ClipboardSelection,
        clipboard: Option<String>,
    ) -> anyhow::Result<()> {
        let mux =
            Mux::try_get().ok_or_else(|| anyhow::anyhow!("MuxClipboard::set_contents: no Mux?"))?;
        mux.notify(MuxNotification::AssignClipboard {
            pane_id: self.pane_id,
            selection,
            clipboard,
        });
        Ok(())
    }
}

struct MuxDownloader {}

impl wezterm_term::DownloadHandler for MuxDownloader {
    fn save_to_downloads(&self, name: Option<String>, data: Vec<u8>) {
        if let Some(mux) = Mux::try_get() {
            mux.notify(MuxNotification::SaveToDownloads {
                name,
                data: Arc::new(data),
            });
        }
    }
}

#[cfg(test)]
mod pane_removal_tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn parser_buffer_size_is_never_zero_or_larger_than_its_retained_batch_bound() {
        let maximum = wezterm_runtime_admission::MAX_PANE_PARSER_INPUT_BYTES_PER_BATCH;
        assert_eq!(bounded_parser_buffer_size(0), 1);
        assert_eq!(bounded_parser_buffer_size(1), 1);
        assert_eq!(bounded_parser_buffer_size(maximum), maximum);
        assert_eq!(bounded_parser_buffer_size(maximum + 1), maximum);
        assert_eq!(bounded_parser_buffer_size(usize::MAX), maximum);
    }

    #[test]
    fn pane_task_permits_charge_the_real_input_write_and_refresh_pools_until_drop() {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let pane_input = Arc::new(PaneInputAdmission::default());

        let input =
            PaneTaskPermits::admit(&admission, &pane_input, PaneTaskKind::Input { bytes: 17 })
                .unwrap();
        assert_eq!(admission.count_usage(CountClass::PaneInputItem), 1);
        assert_eq!(admission.byte_usage(ByteClass::PaneInput), 17);
        drop(input);
        assert_eq!(admission.count_usage(CountClass::PaneInputItem), 0);
        assert_eq!(admission.byte_usage(ByteClass::PaneInput), 0);

        let write =
            PaneTaskPermits::admit(&admission, &pane_input, PaneTaskKind::Write { bytes: 23 })
                .unwrap();
        assert_eq!(admission.count_usage(CountClass::PaneInputItem), 1);
        assert_eq!(admission.count_usage(CountClass::PaneWriteJob), 1);
        assert_eq!(admission.byte_usage(ByteClass::PaneInput), 23);
        drop(write);
        assert_eq!(admission.count_usage(CountClass::PaneInputItem), 0);
        assert_eq!(admission.count_usage(CountClass::PaneWriteJob), 0);
        assert_eq!(admission.byte_usage(ByteClass::PaneInput), 0);

        let refresh =
            PaneTaskPermits::admit(&admission, &pane_input, PaneTaskKind::Refresh).unwrap();
        assert_eq!(admission.count_usage(CountClass::PaneRefreshJob), 1);
        drop(refresh);
        assert_eq!(admission.count_usage(CountClass::PaneRefreshJob), 0);

        let item_permits = (0..MAX_PANE_INPUT_ITEMS_PER_PANE)
            .map(|_| {
                PaneTaskPermits::admit(&admission, &pane_input, PaneTaskKind::Input { bytes: 0 })
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(
            PaneTaskPermits::admit(&admission, &pane_input, PaneTaskKind::Input { bytes: 0 })
                .is_err()
        );
        drop(item_permits);

        let too_large = MAX_PANE_INPUT_BYTES_PER_PANE + 1;
        assert!(PaneTaskPermits::admit(
            &admission,
            &pane_input,
            PaneTaskKind::Write { bytes: too_large }
        )
        .is_err());
        assert_eq!(admission.count_usage(CountClass::PaneInputItem), 0);
        assert_eq!(admission.count_usage(CountClass::PaneWriteJob), 0);
        assert_eq!(admission.byte_usage(ByteClass::PaneInput), 0);
    }

    fn test_mux() -> Arc<Mux> {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let mux = Arc::new(Mux::new(None, admission));
        mux.start_pane_lifecycle().unwrap();
        mux
    }

    fn headless_test_mux() -> Arc<Mux> {
        let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
        let executor = Arc::new(promise::spawn::SimpleExecutor::new(Arc::clone(&admission)));
        let mux = Arc::new(Mux::new_headless(None, admission, executor));
        mux.start_pane_lifecycle().unwrap();
        mux
    }

    #[test]
    fn process_mux_cannot_be_replaced_or_restarted_after_shutdown() {
        const HELPER: &str = "WEZTERM_MUX_SINGLETON_TEST_HELPER";
        if std::env::var_os(HELPER).is_some() {
            let new_mux = || {
                Arc::new(Mux::new(
                    None,
                    RuntimeAdmission::new(RuntimeRole::Server).unwrap(),
                ))
            };
            let active = new_mux();
            Mux::set_mux(&active).unwrap();

            let replacement = new_mux();
            assert!(Mux::set_mux(&replacement)
                .unwrap_err()
                .to_string()
                .contains("already initialized"));

            Mux::shutdown();
            let restarted = new_mux();
            assert!(Mux::set_mux(&restarted)
                .unwrap_err()
                .to_string()
                .contains("cannot be restarted"));
            return;
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "pane_removal_tests::process_mux_cannot_be_replaced_or_restarted_after_shutdown",
                "--nocapture",
            ])
            .env(HELPER, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "singleton subprocess failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn stale_client_pane_unregister_never_requests_remote_kill() {
        assert!(!PaneRemoval::Unregister.should_kill());
        assert!(PaneRemoval::Kill.should_kill());
    }

    #[test]
    fn tab_without_active_pane_rolls_back_only_the_insertion_it_owns() {
        let mux = test_mux();
        let new_tab = Arc::new(Tab::new(&TerminalSize::default()));
        assert!(mux.add_tab_and_active_pane(&new_tab).is_err());
        assert!(mux.get_tab(new_tab.tab_id()).is_none());

        let existing_tab = Arc::new(Tab::new(&TerminalSize::default()));
        mux.add_tab_no_panes(&existing_tab).unwrap();
        assert!(mux.add_tab_and_active_pane(&existing_tab).is_err());
        assert!(mux.get_tab(existing_tab.tab_id()).is_some());
        mux.shutdown_runtime();
    }

    #[test]
    fn duplicate_tab_add_is_atomic_and_shutdown_closes_registration() {
        let mux = test_mux();
        let tab = Arc::new(Tab::new(&TerminalSize::default()));
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let mux = Arc::clone(&mux);
            let tab = Arc::clone(&tab);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                mux.add_tab_no_panes(&tab)
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        assert!(mux.get_tab(tab.tab_id()).is_some());

        mux.shutdown_runtime();
        let after_shutdown = Arc::new(Tab::new(&TerminalSize::default()));
        assert!(mux.add_tab_no_panes(&after_shutdown).is_err());
        assert!(mux.get_tab(after_shutdown.tab_id()).is_none());
        assert_eq!(mux.admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn headless_invalidation_is_reaped_by_the_mux_executor_owner() {
        let mux = headless_test_mux();
        mux.try_spawn_client_invalidation(async { Ok(()) }).unwrap();
        assert_eq!(mux.admission.count_usage(CountClass::ClientInvalidation), 1);
        assert_eq!(mux.admission.count_usage(CountClass::ExecutorRunnable), 2);

        mux.tick_headless().unwrap();
        assert_eq!(mux.admission.count_usage(CountClass::ClientInvalidation), 0);
        assert_eq!(mux.admission.count_usage(CountClass::ExecutorRunnable), 1);
        mux.shutdown_runtime();
        assert_eq!(mux.admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn headless_runtime_task_is_retained_reaped_and_joined() {
        let mux = headless_test_mux();
        let completed = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&completed);
        mux.try_spawn_runtime_task("schedule headless runtime task test", async move {
            observed.fetch_add(1, Ordering::Release);
            Ok(())
        })
        .unwrap();

        assert_eq!(mux.headless_runtime_tasks.lock().len(), 1);
        assert_eq!(mux.admission.count_usage(CountClass::ExecutorRunnable), 2);
        mux.tick_headless().unwrap();
        assert_eq!(completed.load(Ordering::Acquire), 1);
        assert!(mux.headless_runtime_tasks.lock().is_empty());
        assert_eq!(mux.admission.count_usage(CountClass::ExecutorRunnable), 1);

        mux.try_spawn_runtime_task(
            "schedule pending headless runtime task test",
            std::future::pending::<anyhow::Result<()>>(),
        )
        .unwrap();
        mux.shutdown_runtime();
        assert!(mux.headless_runtime_tasks.lock().is_empty());
        assert_eq!(mux.admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn headless_pane_output_is_coalesced_reaped_and_joined_on_shutdown() {
        let mux = headless_test_mux();
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&notifications);
        mux.subscribe(move |notification| {
            if matches!(notification, MuxNotification::PaneOutput(42)) {
                observed.fetch_add(1, Ordering::Relaxed);
            }
            true
        });

        mux.try_enqueue_headless_pane_output(42).unwrap();
        mux.try_enqueue_headless_pane_output(42).unwrap();
        assert_eq!(mux.headless_pane_output_tasks.lock().pending.len(), 1);
        assert_eq!(mux.headless_pane_output_tasks.lock().tasks.len(), 1);
        assert_eq!(mux.admission.count_usage(CountClass::PaneLifecycleEvent), 1);
        assert_eq!(mux.admission.count_usage(CountClass::ExecutorRunnable), 2);

        mux.tick_headless().unwrap();
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert!(mux.headless_pane_output_tasks.lock().pending.is_empty());
        assert!(mux.headless_pane_output_tasks.lock().tasks.is_empty());
        assert_eq!(mux.admission.count_usage(CountClass::PaneLifecycleEvent), 0);
        assert_eq!(mux.admission.count_usage(CountClass::ExecutorRunnable), 1);

        mux.try_enqueue_headless_pane_output(42).unwrap();
        mux.shutdown_runtime();
        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert!(mux.admission.is_shutting_down());
        assert_eq!(mux.admission.count_usage(CountClass::PaneLifecycleEvent), 0);
        assert_eq!(mux.admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn pane_kill_retires_close_after_cancelling_sibling_tasks_and_retains_admission() {
        fn pending_task(
            executor: &promise::spawn::SimpleExecutorHandle,
            kind: ClientPaneTaskKind,
            producer_permit: Option<CountPermit>,
        ) -> OwnedPaneTask {
            let task = executor
                .try_spawn(async move {
                    let _producer_permit = producer_permit;
                    std::future::pending::<()>().await;
                    Ok(())
                })
                .unwrap();
            OwnedPaneTask {
                kind: OwnedPaneTaskKind::Client(kind),
                task,
            }
        }

        let mux = headless_test_mux();
        let admission = Arc::clone(mux.admission());
        let executor = mux.headless_executor().unwrap();
        let pane_id = 42;
        let request = pending_task(
            &executor,
            ClientPaneTaskKind::Request,
            Some(admission.try_count(CountClass::ClientRequest, 1).unwrap()),
        );
        let fetch = pending_task(
            &executor,
            ClientPaneTaskKind::Fetch,
            Some(admission.try_count(CountClass::ClientFetchJob, 1).unwrap()),
        );
        let poll = pending_task(
            &executor,
            ClientPaneTaskKind::Poll,
            Some(admission.try_count(CountClass::ClientPollJob, 1).unwrap()),
        );
        let input_kind = PaneTaskKind::Input { bytes: 31 };
        let pane_input = Arc::new(PaneInputAdmission::default());
        let input_permits = PaneTaskPermits::admit(&admission, &pane_input, input_kind).unwrap();
        let input_task = executor
            .local()
            .try_spawn_local(async move {
                let _permits = input_permits;
                std::future::pending::<()>().await;
                Ok(())
            })
            .unwrap();
        let input = OwnedPaneTask {
            kind: OwnedPaneTaskKind::Pane(input_kind),
            task: input_task,
        };
        let close_permit = admission.try_count(CountClass::ClientRequest, 1).unwrap();
        let (close_tx, close_rx) = smol::channel::bounded(1);
        let close_task = executor
            .try_spawn(async move {
                let _close_permit = close_permit;
                close_rx.recv().await.context("await close completion")?;
                Ok(())
            })
            .unwrap();
        let close = OwnedPaneTask {
            kind: OwnedPaneTaskKind::Client(ClientPaneTaskKind::Close),
            task: close_task,
        };

        let retired = retire_pane_tasks(
            pane_id,
            PaneRemoval::Kill,
            vec![request, fetch, poll, input, close],
            admission.try_pane().unwrap(),
        );
        mux.retiring_pane_tasks.lock().push(retired);

        for _ in 0..8 {
            mux.tick_headless().unwrap();
            if mux.retiring_pane_tasks.lock()[0].tasks.len() == 1 {
                break;
            }
        }
        let retiring = mux.retiring_pane_tasks.lock();
        assert_eq!(retiring.len(), 1);
        assert_eq!(retiring[0].pane_id, pane_id);
        assert_eq!(retiring[0].tasks.len(), 1);
        assert_eq!(
            retiring[0].tasks[0].kind,
            OwnedPaneTaskKind::Client(ClientPaneTaskKind::Close)
        );
        assert!(!retiring[0].tasks[0].task.is_finished());
        drop(retiring);
        assert_eq!(admission.count_usage(CountClass::ClientRequest), 1);
        assert_eq!(admission.count_usage(CountClass::ClientFetchJob), 0);
        assert_eq!(admission.count_usage(CountClass::ClientPollJob), 0);
        assert_eq!(admission.count_usage(CountClass::PaneInputItem), 0);
        assert_eq!(admission.byte_usage(ByteClass::PaneInput), 0);

        let other_pane_permits = (1..MAX_PANES)
            .map(|_| admission.try_pane().unwrap())
            .collect::<Vec<_>>();
        assert!(admission.try_pane().is_err());

        close_tx.try_send(()).unwrap();
        mux.tick_headless().unwrap();
        assert!(mux.retiring_pane_tasks.lock().is_empty());
        assert_eq!(admission.count_usage(CountClass::ClientRequest), 0);
        let reclaimed_pane_permit = admission.try_pane().unwrap();
        drop(reclaimed_pane_permit);
        drop(other_pane_permits);

        mux.shutdown_runtime();
        assert!(admission.is_shutting_down());
        assert_eq!(admission.count_usage(CountClass::ExecutorRunnable), 0);
    }

    #[test]
    fn empty_pane_cleanup_retains_admission_until_publication_reap() {
        let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
        let mut retiring = vec![retire_pane_tasks(
            42,
            PaneRemoval::Unregister,
            Vec::new(),
            admission.try_pane().unwrap(),
        )];
        let other_panes = (1..MAX_PANES)
            .map(|_| admission.try_pane().unwrap())
            .collect::<Vec<_>>();

        assert!(admission.try_pane().is_err());
        reap_retiring_pane_tasks(&mut retiring).unwrap();
        assert!(retiring.is_empty());
        assert!(admission.try_pane().is_ok());
        drop(other_panes);
    }

    #[test]
    fn headless_shutdown_cancels_and_joins_pending_invalidations_before_admission_closes() {
        let mux = headless_test_mux();
        mux.try_spawn_client_invalidation(async {
            std::future::pending::<()>().await;
            Ok(())
        })
        .unwrap();

        mux.shutdown_runtime();
        assert!(mux.admission.is_shutting_down());
        assert_eq!(mux.admission.count_usage(CountClass::ClientInvalidation), 0);
        assert_eq!(mux.admission.count_usage(CountClass::ExecutorRunnable), 0);
    }
}
