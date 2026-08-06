use std::fs::Metadata;
use std::future::Future;
use std::io::ErrorKind;
use std::num::NonZeroU32;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crossterm::event::{
    KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent, KeyEventKind,
    KeyModifiers as CrosstermModifiers, MediaKeyCode, ModifierKeyCode,
    MouseButton as CrosstermMouseButton, MouseEvent as CrosstermMouseEvent,
    MouseEventKind as CrosstermMouseEventKind,
};
use termwiz::input::{KeyCode, KeyEvent, Modifiers};
use tokio::sync::watch;
use wezterm_client::client::{Client, PaneControlStatus};
use wezterm_client::domain::ClientDomainConfig;
use wezterm_codec::{
    ControlLeaseAction, ControlLeaseRequest, ControlLeaseResult, EnvironmentFreeCommand,
    InputSerial, KillPane, Resize, SendKeyDown, SendMouseEvent, SendPaste, ServiceDrainAction,
    ServiceDrainRequest, SpawnV2, TabSpawnDomain, TabSpawnPlacement, TabTitleChanged,
};
use wezterm_config::UnixDomain;
use wezterm_mux::tab::PaneNode;
use wezterm_mux::{Mux, DEFAULT_WORKSPACE};
use wezterm_term::{
    Line, MouseButton as WeztermMouseButton, MouseEvent as WeztermMouseEvent,
    MouseEventKind as WeztermMouseEventKind, StableRowIndex, TerminalSize,
};

use super::activity::{self, ActivityTracker, AgentEvidence, AgentPresentation};
use super::perf_trace;

pub type SessionId = usize;

// This bounds the complete WezTerm attach, including bootstrap, ListPanes, and construction of the
// local render projection for every retained session. It is intentionally longer than an IPC-only
// socket deadline; a busy machine or a large session set must not be misclassified as disconnected.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REMOTE_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RPC_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PROJECTED_TERMINAL_ROWS: isize = 1_024;
const MAX_AGENT_DETECTION_ROWS: isize = 16;
const REMOTE_RECONNECT_ATTEMPTS: NonZeroU32 = NonZeroU32::new(8).unwrap();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionControl {
    Synchronizing,
    Uncontrolled,
    Controller,
    Observer,
}

pub use super::connection::{ConnectionHealth, ConnectionState};

use super::connection::{AttachmentPolicy, ConnectionHandle, ConnectionOwner};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalContentGeometry {
    origin_column: u16,
    origin_row: u16,
    cols: u16,
    rows: u16,
}

impl TerminalContentGeometry {
    pub const fn new(origin_column: u16, origin_row: u16, cols: u16, rows: u16) -> Self {
        Self { origin_column, origin_row, cols, rows }
    }

    fn relative_position(self, column: u16, row: u16) -> Option<(usize, i64)> {
        let column = column.checked_sub(self.origin_column)?;
        let row = row.checked_sub(self.origin_row)?;
        if column >= self.cols || row >= self.rows {
            return None;
        }
        Some((usize::from(column), i64::from(row)))
    }
}

#[derive(Clone, PartialEq)]
pub struct SessionView {
    pub id: SessionId,
    pub pane_id: usize,
    pub tab_id: usize,
    pub window_id: usize,
    pub title: String,
    pub control: SessionControl,
    pub agent: Option<AgentPresentation>,
    pane_title: String,
    foreground_process_name: Option<String>,
}

#[derive(PartialEq)]
pub struct TerminalView {
    pub pane_id: usize,
    pub title: String,
    pub first_row: StableRowIndex,
    pub cols: usize,
    pub rows: usize,
    pub cursor_x: usize,
    pub cursor_y: usize,
    /// Whether the authoritative remote render projection reports terminal mouse capture.
    pub mouse_reporting: bool,
    pub lines: Vec<Line>,
    pub(super) content_sequence: usize,
}

#[derive(PartialEq)]
pub struct ConsoleSnapshot {
    pub sessions: Vec<SessionView>,
    pub terminal: Option<TerminalView>,
}

pub(super) struct ActivityRefresh {
    pub changed: bool,
    pub revisit: bool,
    pub ready: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConsoleSocketProbe {
    Missing { path: PathBuf },
    WrongOwner { path: PathBuf, expected_uid: u32, actual_uid: u32 },
    Rejected { path: PathBuf, detail: String },
    Ready,
}

pub struct ConsoleClient {
    connection: ConnectionHandle,
    remote_status: Option<Arc<Mutex<watch::Receiver<Option<super::service::ConsoleStatus>>>>>,
    activity: Mutex<ActivityTracker>,
}

pub(crate) fn console_runtime_dir() -> Result<PathBuf> {
    super::runtime::directory()
}

pub(crate) fn console_socket_path() -> Result<PathBuf> {
    Ok(console_runtime_dir()?.join("agent.sock"))
}

pub(crate) fn console_lock_path() -> Result<PathBuf> {
    Ok(console_runtime_dir()?.join("agent.lock"))
}

pub(crate) fn unix_domain() -> Result<UnixDomain> {
    Ok(UnixDomain {
        name: "kit-console".to_owned(),
        socket_path: Some(console_socket_path()?),
        no_serve_automatically: true,
        ..Default::default()
    })
}

pub(crate) fn connection_owner() -> Result<ConnectionOwner> {
    ConnectionOwner::start()
}

fn remote_domain(socket_path: PathBuf) -> ClientDomainConfig {
    ClientDomainConfig::Unix(UnixDomain {
        name: "kit-console-remote".to_owned(),
        socket_path: Some(socket_path),
        no_serve_automatically: true,
        ..Default::default()
    })
}

fn validate_owned_private_socket(path: &Path, metadata: &Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() {
        bail!("Console agent socket {} must not be a symlink", path.display());
    }
    if !metadata.file_type().is_socket() {
        bail!("Console agent path {} is not a Unix socket", path.display());
    }
    validate_owned_mode(path, metadata, 0o022, "group/other write access is forbidden")
}

fn validate_owned_mode(
    path: &Path,
    metadata: &Metadata,
    forbidden_mode: u32,
    reason: &str,
) -> Result<()> {
    let effective_user = unsafe { libc::geteuid() };
    if metadata.uid() != effective_user {
        bail!(
            "Console path {} is owned by uid {}, expected uid {}",
            path.display(),
            metadata.uid(),
            effective_user
        );
    }
    let mode = metadata.mode();
    if mode & forbidden_mode != 0 {
        bail!(
            "Console path {} has insecure permissions {:o}; {reason}",
            path.display(),
            mode & 0o7777
        );
    }
    Ok(())
}

fn inspect_console_socket(runtime_dir: &Path, socket_path: &Path) -> ConsoleSocketProbe {
    let expected_uid = unsafe { libc::geteuid() };
    match std::fs::symlink_metadata(runtime_dir) {
        Ok(metadata) if metadata.uid() != expected_uid => {
            return ConsoleSocketProbe::WrongOwner {
                path: runtime_dir.to_path_buf(),
                expected_uid,
                actual_uid: metadata.uid(),
            }
        }
        Ok(metadata) => {
            if let Err(error) =
                super::runtime::validate_owned_private_directory(runtime_dir, &metadata)
            {
                return ConsoleSocketProbe::Rejected {
                    path: runtime_dir.to_path_buf(),
                    detail: format!("{error:#}"),
                };
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return ConsoleSocketProbe::Missing { path: socket_path.to_path_buf() };
        }
        Err(error) => {
            return ConsoleSocketProbe::Rejected {
                path: runtime_dir.to_path_buf(),
                detail: format!(
                    "inspecting Console runtime directory {}: {error}",
                    runtime_dir.display()
                ),
            };
        }
    }

    match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) if metadata.uid() != expected_uid => ConsoleSocketProbe::WrongOwner {
            path: socket_path.to_path_buf(),
            expected_uid,
            actual_uid: metadata.uid(),
        },
        Ok(metadata) => match validate_owned_private_socket(socket_path, &metadata) {
            Ok(()) => ConsoleSocketProbe::Ready,
            Err(error) => ConsoleSocketProbe::Rejected {
                path: socket_path.to_path_buf(),
                detail: format!("{error:#}"),
            },
        },
        Err(error) if error.kind() == ErrorKind::NotFound => {
            ConsoleSocketProbe::Missing { path: socket_path.to_path_buf() }
        }
        Err(error) => ConsoleSocketProbe::Rejected {
            path: socket_path.to_path_buf(),
            detail: format!("inspecting Console agent socket {}: {error}", socket_path.display()),
        },
    }
}

pub(crate) fn probe_console_socket() -> Result<ConsoleSocketProbe> {
    let runtime_dir = console_runtime_dir()?;
    let socket_path = runtime_dir.join("agent.sock");
    Ok(inspect_console_socket(&runtime_dir, &socket_path))
}

pub(crate) fn remove_stale_console_socket() -> Result<()> {
    match probe_console_socket()? {
        ConsoleSocketProbe::Missing { .. } => Ok(()),
        ConsoleSocketProbe::Ready => {
            let path = console_socket_path()?;
            std::fs::remove_file(&path)
                .with_context(|| format!("removing stale Console socket {}", path.display()))
        }
        ConsoleSocketProbe::WrongOwner { path, expected_uid, actual_uid } => bail!(
            "refusing to remove Console path {} owned by uid {}; expected uid {}",
            path.display(),
            actual_uid,
            expected_uid
        ),
        ConsoleSocketProbe::Rejected { path, detail } => {
            bail!("refusing to remove rejected Console path {}: {detail}", path.display())
        }
    }
}

impl ConsoleClient {
    pub(crate) async fn connect(owner: &ConnectionOwner) -> Result<Self> {
        match probe_console_socket()? {
            ConsoleSocketProbe::Ready => {
                Self::connect_once(
                    owner,
                    ClientDomainConfig::Unix(unix_domain()?),
                    Some(super::build_identity()?),
                    CONNECT_TIMEOUT,
                    None,
                )
                .await
            }
            ConsoleSocketProbe::Missing { .. } => bail!("the local Console agent is not running"),
            ConsoleSocketProbe::WrongOwner { path, expected_uid, actual_uid } => bail!(
                "Console path {} is owned by uid {}, expected uid {}",
                path.display(),
                actual_uid,
                expected_uid
            ),
            ConsoleSocketProbe::Rejected { detail, .. } => bail!("{detail}"),
        }
    }

    pub(crate) async fn connect_for_service_management(owner: &ConnectionOwner) -> Result<Self> {
        match probe_console_socket()? {
            ConsoleSocketProbe::Ready => {
                Self::connect_once(
                    owner,
                    ClientDomainConfig::Unix(unix_domain()?),
                    None,
                    CONNECT_TIMEOUT,
                    None,
                )
                .await
            }
            ConsoleSocketProbe::Missing { .. } => bail!("the local Console agent is not running"),
            ConsoleSocketProbe::WrongOwner { path, expected_uid, actual_uid } => bail!(
                "Console path {} is owned by uid {}, expected uid {}",
                path.display(),
                actual_uid,
                expected_uid
            ),
            ConsoleSocketProbe::Rejected { detail, .. } => bail!("{detail}"),
        }
    }

    async fn connect_once(
        owner: &ConnectionOwner,
        domain: ClientDomainConfig,
        expected_build_identity: Option<wezterm_codec::BuildIdentity>,
        timeout: Duration,
        remote_status: Option<watch::Receiver<Option<super::service::ConsoleStatus>>>,
    ) -> Result<Self> {
        let reconnect_attempt_limit = remote_status.as_ref().map(|_| REMOTE_RECONNECT_ATTEMPTS);
        let connection = owner
            .attach(AttachmentPolicy::new(
                domain,
                expected_build_identity,
                timeout,
                reconnect_attempt_limit,
            ))
            .await?;
        Ok(Self {
            connection,
            remote_status: remote_status.map(|receiver| Arc::new(Mutex::new(receiver))),
            activity: Mutex::new(ActivityTracker::default()),
        })
    }

    pub fn drain_connection_health(&self) -> Result<Option<ConnectionHealth>> {
        self.connection.drain_health()
    }

    pub async fn retry(&self) -> Result<()> {
        self.connection.retry().await
    }

    pub fn drain_remote_status(&self) -> Option<Option<super::service::ConsoleStatus>> {
        let receiver = self.remote_status.as_ref()?;
        let mut receiver = receiver.lock().unwrap();
        match receiver.has_changed() {
            Ok(true) => Some(receiver.borrow_and_update().clone()),
            Ok(false) | Err(_) => None,
        }
    }

    pub(crate) async fn connect_to_relay(
        owner: &ConnectionOwner,
        socket_path: PathBuf,
        remote_status: watch::Receiver<Option<super::service::ConsoleStatus>>,
    ) -> Result<Self> {
        Self::connect_once(
            owner,
            remote_domain(socket_path),
            Some(super::build_identity()?),
            REMOTE_CONNECT_TIMEOUT,
            Some(remote_status),
        )
        .await
    }

    pub async fn snapshot(&self, selected: Option<SessionId>) -> Result<ConsoleSnapshot> {
        perf_trace::record_snapshot();
        let mut sessions = self.list_sessions().await?;
        let selected_pane = selected
            .and_then(|id| sessions.iter().find(|session| session.id == id))
            .map(|session| session.pane_id);
        let terminal =
            selected_pane.map(|pane_id| self.project_terminal(pane_id)).transpose()?.flatten();
        let _ = self.refresh_activity(&mut sessions, selected, terminal.as_ref())?;
        Ok(ConsoleSnapshot { sessions, terminal })
    }

    pub fn refresh_activity(
        &self,
        sessions: &mut [SessionView],
        selected: Option<SessionId>,
        selected_terminal: Option<&TerminalView>,
    ) -> Result<ActivityRefresh> {
        let mut tracker = self.activity.lock().unwrap();
        let mut changed = false;
        let mut revisit = false;
        let mut ready = false;
        let now = Instant::now();
        for session in sessions.iter_mut() {
            let terminal = selected_terminal.filter(|terminal| terminal.pane_id == session.pane_id);
            let observation = self.observe_agent_activity(
                &mut tracker,
                session,
                selected == Some(session.id),
                terminal,
                now,
            )?;
            let agent = observation.presentation;
            revisit |= observation.revisit;
            ready |= matches!(observation.transition, Some(activity::AgentTransition::Ready(_)));
            changed |= session.agent != agent;
            session.agent = agent;
        }
        tracker.retain(|session_id| {
            sessions.binary_search_by_key(&session_id, |session| session.id).is_ok()
        });
        Ok(ActivityRefresh { changed, revisit, ready })
    }

    pub fn project_terminal(&self, remote_pane_id: usize) -> Result<Option<TerminalView>> {
        self.terminal_view(remote_pane_id)
    }

    pub fn local_pane_id(&self, remote_pane_id: usize) -> Result<Option<usize>> {
        Ok(self.connection.domain()?.remote_to_local_pane_id(remote_pane_id))
    }

    pub(super) fn connection_mux(&self) -> Result<Arc<Mux>> {
        self.connection.mux()
    }

    pub async fn create_session(&self, cols: u16, rows: u16) -> Result<SessionId> {
        let client = self.connection.client()?;
        let response = bounded_rpc(
            "creating a session",
            client.spawn_v2(SpawnV2 {
                domain: TabSpawnDomain::DefaultDomain,
                placement: TabSpawnPlacement::NewWindow {
                    size: terminal_size(cols, rows),
                    workspace: DEFAULT_WORKSPACE.to_owned(),
                },
                command: EnvironmentFreeCommand::DefaultLoginShell,
                command_dir: None,
            }),
        )
        .await?
        .into_inner();
        self.require_control(response.pane_id).await?;
        Ok(response.tab_id)
    }

    pub async fn begin_service_drain(&self) -> Result<()> {
        let client = self.connection.client()?;
        let result = bounded_rpc(
            "beginning Console service drain",
            client.service_drain(ServiceDrainRequest { action: ServiceDrainAction::Begin }),
        )
        .await?
        .into_inner();
        if !result.draining {
            bail!("Console agent did not enter service drain mode");
        }
        Ok(())
    }

    pub async fn cancel_service_drain(&self) -> Result<()> {
        let client = self.connection.client()?;
        let result = bounded_rpc(
            "cancelling Console service drain",
            client.service_drain(ServiceDrainRequest { action: ServiceDrainAction::Cancel }),
        )
        .await?
        .into_inner();
        if result.draining {
            bail!("Console agent did not leave service drain mode");
        }
        Ok(())
    }

    pub async fn close_pane(&self, pane_id: usize) -> Result<()> {
        self.require_control(pane_id).await?;
        let client = self.connection.client()?;
        bounded_rpc("closing a session", client.kill_pane(KillPane { pane_id })).await?;
        Ok(())
    }

    pub async fn rename_session(&self, id: SessionId, title: String) -> Result<()> {
        let session = self.find_session(id).await?;
        self.require_control(session.pane_id).await?;
        let client = self.connection.client()?;
        bounded_rpc(
            "renaming a session",
            client.set_tab_title(TabTitleChanged { tab_id: session.tab_id, title }),
        )
        .await?;
        Ok(())
    }

    pub async fn release_control(&self, id: SessionId) -> Result<()> {
        let session = self.find_session(id).await?;
        match self.apply_control(session.pane_id, ControlLeaseAction::Release).await? {
            SessionControl::Uncontrolled | SessionControl::Observer => Ok(()),
            SessionControl::Synchronizing => {
                bail!("Console control state is still synchronizing for session {id}")
            }
            SessionControl::Controller => {
                bail!("Console agent retained control after releasing session {id}")
            }
        }
    }

    pub async fn take_control(&self, id: SessionId) -> Result<()> {
        let session = self.find_session(id).await?;
        match self.apply_control(session.pane_id, ControlLeaseAction::Take).await? {
            SessionControl::Controller => Ok(()),
            SessionControl::Observer => {
                bail!("Console agent did not transfer control of session {id}")
            }
            SessionControl::Synchronizing | SessionControl::Uncontrolled => {
                bail!("Console agent did not establish control of session {id}")
            }
        }
    }

    pub async fn acquire_control(&self, id: SessionId) -> Result<()> {
        let session = self.find_session(id).await?;
        match self.apply_control(session.pane_id, ControlLeaseAction::Acquire).await? {
            SessionControl::Controller => Ok(()),
            SessionControl::Observer => {
                bail!("this Console attachment is observing the session; take control to edit it")
            }
            SessionControl::Synchronizing | SessionControl::Uncontrolled => {
                bail!("Console agent did not establish control of session {id}")
            }
        }
    }

    pub async fn send_key(&self, pane_id: usize, event: CrosstermKeyEvent) -> Result<()> {
        if event.kind == KeyEventKind::Release {
            return Ok(());
        }
        let Some((key, modifiers)) = map_key(event) else {
            return Ok(());
        };
        let client = self.connection.client()?;
        bounded_rpc(
            "sending terminal input",
            client.key_down(SendKeyDown {
                pane_id,
                event: KeyEvent { key, modifiers },
                input_serial: InputSerial::now(),
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn paste(&self, pane_id: usize, text: String) -> Result<()> {
        let client = self.connection.client()?;
        bounded_rpc("pasting into a session", client.send_paste(SendPaste { pane_id, data: text }))
            .await?;
        Ok(())
    }

    /// Forward a terminal-content mouse event through WezTerm's canonical mouse PDU.
    ///
    /// Returns `false` when the event is outside the current terminal content rectangle. The
    /// caller retains ownership of sidebar, borders, resize handles, and other Kit UI regions.
    pub async fn send_mouse(
        &self,
        pane_id: usize,
        event: CrosstermMouseEvent,
        geometry: TerminalContentGeometry,
    ) -> Result<bool> {
        let Some(event) = map_mouse(event, geometry) else {
            return Ok(false);
        };
        let client = self.connection.client()?;
        bounded_rpc(
            "sending terminal mouse input",
            client.mouse_event(SendMouseEvent { pane_id, event }),
        )
        .await?;
        Ok(true)
    }

    pub async fn resize(&self, tab_id: usize, pane_id: usize, cols: u16, rows: u16) -> Result<()> {
        let client = self.connection.client()?;
        bounded_rpc(
            "resizing a session",
            client.resize(Resize {
                containing_tab_id: tab_id,
                pane_id,
                size: terminal_size(cols, rows),
            }),
        )
        .await?;
        Ok(())
    }

    async fn require_control(&self, pane_id: usize) -> Result<()> {
        match self.apply_control(pane_id, ControlLeaseAction::Acquire).await? {
            SessionControl::Controller => Ok(()),
            SessionControl::Observer => {
                bail!("this Console attachment is observing the session; take control to edit it")
            }
            SessionControl::Synchronizing | SessionControl::Uncontrolled => {
                bail!("the Console agent did not establish control of pane {pane_id}")
            }
        }
    }

    async fn apply_control(
        &self,
        pane_id: usize,
        action: ControlLeaseAction,
    ) -> Result<SessionControl> {
        let client = self.connection.client()?;
        let result = bounded_rpc(
            "updating terminal control",
            client.control_lease(ControlLeaseRequest { pane_id, action }),
        )
        .await?
        .into_inner();
        match result {
            ControlLeaseResult::Acquired(_) | ControlLeaseResult::AlreadyController(_) => {
                Ok(SessionControl::Controller)
            }
            ControlLeaseResult::Observing(_) => Ok(SessionControl::Observer),
            ControlLeaseResult::Taken(_) if action == ControlLeaseAction::Take => {
                Ok(SessionControl::Controller)
            }
            ControlLeaseResult::Released(_) if action == ControlLeaseAction::Release => {
                Ok(SessionControl::Uncontrolled)
            }
            ControlLeaseResult::NotController(state) => {
                if state.active.iter().any(|lease| lease.pane_id == pane_id) {
                    Ok(SessionControl::Observer)
                } else {
                    Ok(SessionControl::Uncontrolled)
                }
            }
            ControlLeaseResult::Overloaded => {
                bail!("Console control state is busy; retry the operation")
            }
            unexpected => bail!("Console agent returned {unexpected:?} for {action:?}"),
        }
    }

    async fn find_session(&self, id: SessionId) -> Result<SessionView> {
        self.list_sessions()
            .await?
            .into_iter()
            .find(|session| session.id == id)
            .ok_or_else(|| anyhow!("Console session {id} no longer exists"))
    }

    async fn list_sessions(&self) -> Result<Vec<SessionView>> {
        perf_trace::record_list_panes();
        let client = self.connection.client()?;
        let panes = bounded_rpc("listing sessions", client.list_panes()).await?.into_inner();
        let mut sessions = Vec::new();
        for (root, tab_title) in panes.tabs.into_iter().zip(panes.tab_titles) {
            flatten_panes(&client, root, &tab_title, &mut sessions);
        }
        sessions.sort_by_key(|session| session.id);
        Ok(sessions)
    }

    fn terminal_view(&self, remote_pane_id: usize) -> Result<Option<TerminalView>> {
        perf_trace::record_terminal_projection();
        let Some(local_pane_id) = self.local_pane_id(remote_pane_id)? else {
            return Ok(None);
        };
        let Some(pane) = self.connection.mux()?.get_pane(local_pane_id) else {
            return Ok(None);
        };
        let dimensions = pane.get_dimensions();
        let end = dimensions
            .physical_top
            .checked_add(dimensions.viewport_rows as isize)
            .context("computing Console viewport range")?;
        let start =
            dimensions.scrollback_top.max(end.saturating_sub(MAX_PROJECTED_TERMINAL_ROWS)).min(end);
        // Drive the projected pane's canonical render-change request before reading its state.
        // The request is owned and deduplicated by ClientPane; a later snapshot observes it.
        let content_sequence = pane.get_current_seqno();
        let _ = pane.get_changed_since(start..end, content_sequence);
        let (first_row, lines) = pane.get_lines(start..end);
        let cursor = pane.get_cursor_position();
        let cursor_y = cursor.y.saturating_sub(dimensions.physical_top).max(0) as usize;
        Ok(Some(TerminalView {
            pane_id: remote_pane_id,
            title: pane.get_title(),
            first_row,
            cols: dimensions.cols,
            rows: dimensions.viewport_rows,
            cursor_x: cursor.x,
            cursor_y,
            mouse_reporting: pane.is_mouse_grabbed(),
            lines,
            content_sequence,
        }))
    }

    fn observe_agent_activity(
        &self,
        tracker: &mut ActivityTracker,
        session: &SessionView,
        selected: bool,
        terminal: Option<&TerminalView>,
        now: Instant,
    ) -> Result<activity::ActivityObservation> {
        if let Some(terminal) = terminal {
            return Ok(tracker.observe_with(
                session.id,
                activity::AgentFingerprint {
                    content_sequence: Some(terminal.content_sequence),
                    foreground_process_name: session.foreground_process_name.as_deref(),
                    title: &terminal.title,
                },
                selected,
                now,
                || {
                    let screen =
                        terminal.lines.iter().map(Line::as_str).collect::<Vec<_>>().join("\n");
                    activity::detect(AgentEvidence {
                        foreground_process_name: session.foreground_process_name.as_deref(),
                        title: &terminal.title,
                        screen: &screen,
                    })
                },
            ));
        }
        let Some(local_pane_id) = self.local_pane_id(session.pane_id)? else {
            return Ok(tracker.observe_with(
                session.id,
                activity::AgentFingerprint {
                    content_sequence: None,
                    foreground_process_name: session.foreground_process_name.as_deref(),
                    title: &session.pane_title,
                },
                selected,
                now,
                || {
                    activity::detect(AgentEvidence {
                        foreground_process_name: session.foreground_process_name.as_deref(),
                        title: &session.pane_title,
                        screen: "",
                    })
                },
            ));
        };
        let Some(pane) = self.connection.mux()?.get_pane(local_pane_id) else {
            return Ok(activity::ActivityObservation {
                presentation: None,
                revisit: false,
                transition: None,
            });
        };
        let content_sequence = pane.get_current_seqno();
        let title = pane.get_title();
        let dimensions = pane.get_dimensions();
        let end = dimensions
            .physical_top
            .checked_add(dimensions.viewport_rows as isize)
            .context("computing Console agent-detection range")?;
        let start =
            dimensions.physical_top.max(end.saturating_sub(MAX_AGENT_DETECTION_ROWS)).min(end);
        Ok(tracker.observe_with(
            session.id,
            activity::AgentFingerprint {
                content_sequence: Some(content_sequence),
                foreground_process_name: session.foreground_process_name.as_deref(),
                title: &title,
            },
            selected,
            now,
            || {
                let _ = pane.get_changed_since(start..end, content_sequence);
                let (_, lines) = pane.get_lines(start..end);
                perf_trace::record_activity_screen_read();
                let screen = lines.iter().map(Line::as_str).collect::<Vec<_>>().join("\n");
                activity::detect(AgentEvidence {
                    foreground_process_name: session.foreground_process_name.as_deref(),
                    title: &title,
                    screen: &screen,
                })
            },
        ))
    }
}

async fn bounded_rpc<T>(
    operation: &'static str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(RPC_TIMEOUT, future)
        .await
        .with_context(|| format!("Console agent timed out while {operation}"))?
}

fn flatten_panes(
    client: &Client,
    root: PaneNode,
    tab_title: &str,
    sessions: &mut Vec<SessionView>,
) {
    match root {
        PaneNode::Empty => {}
        PaneNode::Split { left, right, .. } => {
            flatten_panes(client, *left, tab_title, sessions);
            flatten_panes(client, *right, tab_title, sessions);
        }
        PaneNode::Leaf(pane) => sessions.push(SessionView {
            id: pane.tab_id,
            pane_id: pane.pane_id,
            tab_id: pane.tab_id,
            window_id: pane.window_id,
            title: if tab_title.is_empty() { pane.title.clone() } else { tab_title.to_owned() },
            control: session_control(client.pane_control_status(pane.pane_id)),
            agent: None,
            pane_title: pane.title,
            foreground_process_name: pane.foreground_process_name,
        }),
    }
}

fn session_control(status: PaneControlStatus) -> SessionControl {
    match status {
        PaneControlStatus::AwaitingSnapshot => SessionControl::Synchronizing,
        PaneControlStatus::Uncontrolled => SessionControl::Uncontrolled,
        PaneControlStatus::Controller => SessionControl::Controller,
        PaneControlStatus::Observer => SessionControl::Observer,
    }
}

fn terminal_size(cols: u16, rows: u16) -> TerminalSize {
    TerminalSize {
        rows: rows as usize,
        cols: cols as usize,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    }
}

fn map_key(event: CrosstermKeyEvent) -> Option<(KeyCode, Modifiers)> {
    // GUI terminals conventionally interpret Super+Backspace as deleting to the start of the
    // line. The embedded mux cannot safely process that host-only modifier combination, so encode
    // its portable terminal equivalent instead.
    if event.code == CrosstermKeyCode::Backspace
        && event.modifiers.contains(CrosstermModifiers::SUPER)
    {
        return Some((KeyCode::Char('u'), Modifiers::CTRL));
    }

    let mut modifiers = map_modifiers(event.modifiers);

    let key = match event.code {
        CrosstermKeyCode::Backspace => KeyCode::Backspace,
        CrosstermKeyCode::Enter => KeyCode::Enter,
        CrosstermKeyCode::Left => KeyCode::LeftArrow,
        CrosstermKeyCode::Right => KeyCode::RightArrow,
        CrosstermKeyCode::Up => KeyCode::UpArrow,
        CrosstermKeyCode::Down => KeyCode::DownArrow,
        CrosstermKeyCode::Home => KeyCode::Home,
        CrosstermKeyCode::End => KeyCode::End,
        CrosstermKeyCode::PageUp => KeyCode::PageUp,
        CrosstermKeyCode::PageDown => KeyCode::PageDown,
        CrosstermKeyCode::Tab => KeyCode::Tab,
        CrosstermKeyCode::BackTab => {
            modifiers |= Modifiers::SHIFT;
            KeyCode::Tab
        }
        CrosstermKeyCode::Delete => KeyCode::Delete,
        CrosstermKeyCode::Insert => KeyCode::Insert,
        CrosstermKeyCode::F(number) => KeyCode::Function(number),
        CrosstermKeyCode::Char(character) => KeyCode::Char(character),
        CrosstermKeyCode::Null => KeyCode::Char('\0'),
        CrosstermKeyCode::Esc => KeyCode::Escape,
        CrosstermKeyCode::CapsLock => KeyCode::CapsLock,
        CrosstermKeyCode::ScrollLock => KeyCode::ScrollLock,
        CrosstermKeyCode::NumLock => KeyCode::NumLock,
        CrosstermKeyCode::PrintScreen => KeyCode::PrintScreen,
        CrosstermKeyCode::Pause => KeyCode::Pause,
        CrosstermKeyCode::Menu => KeyCode::Menu,
        CrosstermKeyCode::KeypadBegin => KeyCode::KeyPadBegin,
        CrosstermKeyCode::Media(media) => match media {
            MediaKeyCode::Play | MediaKeyCode::Pause | MediaKeyCode::PlayPause => {
                KeyCode::MediaPlayPause
            }
            MediaKeyCode::Stop => KeyCode::MediaStop,
            MediaKeyCode::TrackNext | MediaKeyCode::FastForward => KeyCode::MediaNextTrack,
            MediaKeyCode::TrackPrevious | MediaKeyCode::Reverse | MediaKeyCode::Rewind => {
                KeyCode::MediaPrevTrack
            }
            MediaKeyCode::LowerVolume => KeyCode::VolumeDown,
            MediaKeyCode::RaiseVolume => KeyCode::VolumeUp,
            MediaKeyCode::MuteVolume => KeyCode::VolumeMute,
            MediaKeyCode::Record => return None,
        },
        CrosstermKeyCode::Modifier(modifier) => match modifier {
            ModifierKeyCode::LeftShift => KeyCode::LeftShift,
            ModifierKeyCode::RightShift => KeyCode::RightShift,
            ModifierKeyCode::LeftControl => KeyCode::LeftControl,
            ModifierKeyCode::RightControl => KeyCode::RightControl,
            ModifierKeyCode::LeftAlt => KeyCode::LeftAlt,
            ModifierKeyCode::RightAlt => KeyCode::RightAlt,
            ModifierKeyCode::LeftSuper => KeyCode::LeftWindows,
            ModifierKeyCode::RightSuper => KeyCode::RightWindows,
            ModifierKeyCode::LeftHyper | ModifierKeyCode::RightHyper => KeyCode::Hyper,
            ModifierKeyCode::LeftMeta | ModifierKeyCode::RightMeta => KeyCode::Meta,
            ModifierKeyCode::IsoLevel3Shift | ModifierKeyCode::IsoLevel5Shift => return None,
        },
    };
    Some((key, modifiers))
}

fn map_modifiers(modifiers: CrosstermModifiers) -> Modifiers {
    let mut mapped = Modifiers::NONE;
    if modifiers.contains(CrosstermModifiers::SHIFT) {
        mapped |= Modifiers::SHIFT;
    }
    if modifiers.contains(CrosstermModifiers::CONTROL) {
        mapped |= Modifiers::CTRL;
    }
    if modifiers.intersects(CrosstermModifiers::ALT | CrosstermModifiers::META) {
        mapped |= Modifiers::ALT;
    }
    if modifiers.intersects(CrosstermModifiers::SUPER | CrosstermModifiers::HYPER) {
        mapped |= Modifiers::SUPER;
    }
    mapped
}

fn map_mouse(
    event: CrosstermMouseEvent,
    geometry: TerminalContentGeometry,
) -> Option<WeztermMouseEvent> {
    let (x, y) = geometry.relative_position(event.column, event.row)?;
    let (kind, button) = match event.kind {
        CrosstermMouseEventKind::Down(button) => {
            (WeztermMouseEventKind::Press, map_mouse_button(button))
        }
        CrosstermMouseEventKind::Up(button) => {
            (WeztermMouseEventKind::Release, map_mouse_button(button))
        }
        CrosstermMouseEventKind::Drag(button) => {
            (WeztermMouseEventKind::Move, map_mouse_button(button))
        }
        CrosstermMouseEventKind::Moved => (WeztermMouseEventKind::Move, WeztermMouseButton::None),
        CrosstermMouseEventKind::ScrollDown => {
            (WeztermMouseEventKind::Press, WeztermMouseButton::WheelDown(1))
        }
        CrosstermMouseEventKind::ScrollUp => {
            (WeztermMouseEventKind::Press, WeztermMouseButton::WheelUp(1))
        }
        CrosstermMouseEventKind::ScrollLeft => {
            (WeztermMouseEventKind::Press, WeztermMouseButton::WheelLeft(1))
        }
        CrosstermMouseEventKind::ScrollRight => {
            (WeztermMouseEventKind::Press, WeztermMouseButton::WheelRight(1))
        }
    };
    Some(WeztermMouseEvent {
        kind,
        x,
        y,
        x_pixel_offset: 0,
        y_pixel_offset: 0,
        button,
        modifiers: map_modifiers(event.modifiers),
    })
}

fn map_mouse_button(button: CrosstermMouseButton) -> WeztermMouseButton {
    match button {
        CrosstermMouseButton::Left => WeztermMouseButton::Left,
        CrosstermMouseButton::Right => WeztermMouseButton::Right,
        CrosstermMouseButton::Middle => WeztermMouseButton::Middle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

    struct SocketFixture {
        runtime_dir: PathBuf,
        socket_path: PathBuf,
    }

    impl SocketFixture {
        fn new() -> Self {
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let runtime_dir = std::env::temp_dir()
                .join(format!("kit-console-client-{}-{sequence}", std::process::id()));
            std::fs::create_dir(&runtime_dir).unwrap();
            std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            let socket_path = runtime_dir.join("agent.sock");
            Self { runtime_dir, socket_path }
        }

        fn inspect(&self) -> ConsoleSocketProbe {
            inspect_console_socket(&self.runtime_dir, &self.socket_path)
        }
    }

    impl Drop for SocketFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.runtime_dir);
        }
    }

    #[test]
    fn super_backspace_uses_portable_line_delete() {
        let event = CrosstermKeyEvent::new(CrosstermKeyCode::Backspace, CrosstermModifiers::SUPER);

        assert_eq!(map_key(event), Some((KeyCode::Char('u'), Modifiers::CTRL)));
    }

    #[test]
    fn plain_backspace_remains_plain_backspace() {
        let event = CrosstermKeyEvent::new(CrosstermKeyCode::Backspace, CrosstermModifiers::NONE);

        assert_eq!(map_key(event), Some((KeyCode::Backspace, Modifiers::NONE)));
    }

    #[test]
    fn terminal_mouse_coordinates_are_relative_and_bounded() {
        let geometry = TerminalContentGeometry::new(12, 4, 80, 24);
        let event = CrosstermMouseEvent {
            kind: CrosstermMouseEventKind::Down(CrosstermMouseButton::Left),
            column: 17,
            row: 7,
            modifiers: CrosstermModifiers::CONTROL,
        };

        assert_eq!(
            map_mouse(event, geometry),
            Some(WeztermMouseEvent {
                kind: WeztermMouseEventKind::Press,
                x: 5,
                y: 3,
                x_pixel_offset: 0,
                y_pixel_offset: 0,
                button: WeztermMouseButton::Left,
                modifiers: Modifiers::CTRL,
            })
        );

        let outside = CrosstermMouseEvent { column: 11, ..event };
        assert_eq!(map_mouse(outside, geometry), None);
        let right_edge = CrosstermMouseEvent { column: 92, ..event };
        assert_eq!(map_mouse(right_edge, geometry), None);
    }

    #[test]
    fn socket_inspection_accepts_only_owned_private_unix_sockets() {
        let fixture = SocketFixture::new();
        assert_eq!(
            fixture.inspect(),
            ConsoleSocketProbe::Missing { path: fixture.socket_path.clone() }
        );

        let listener = UnixListener::bind(&fixture.socket_path).unwrap();
        std::fs::set_permissions(&fixture.socket_path, std::fs::Permissions::from_mode(0o700))
            .unwrap();
        assert_eq!(fixture.inspect(), ConsoleSocketProbe::Ready);

        std::fs::set_permissions(&fixture.socket_path, std::fs::Permissions::from_mode(0o777))
            .unwrap();
        assert!(matches!(
            fixture.inspect(),
            ConsoleSocketProbe::Rejected { detail, .. } if detail.contains("insecure permissions")
        ));
        drop(listener);
    }

    #[test]
    fn socket_inspection_rejects_symlinks_and_non_sockets() {
        let fixture = SocketFixture::new();
        std::os::unix::fs::symlink("missing", &fixture.socket_path).unwrap();
        assert!(matches!(
            fixture.inspect(),
            ConsoleSocketProbe::Rejected { detail, .. } if detail.contains("must not be a symlink")
        ));
        std::fs::remove_file(&fixture.socket_path).unwrap();

        std::fs::write(&fixture.socket_path, b"not a socket").unwrap();
        assert!(matches!(
            fixture.inspect(),
            ConsoleSocketProbe::Rejected { detail, .. } if detail.contains("not a Unix socket")
        ));
    }

    #[test]
    fn socket_inspection_rejects_insecure_runtime_directory() {
        let fixture = SocketFixture::new();
        std::fs::set_permissions(&fixture.runtime_dir, std::fs::Permissions::from_mode(0o777))
            .unwrap();

        assert!(matches!(
            fixture.inspect(),
            ConsoleSocketProbe::Rejected { detail, .. } if detail.contains("insecure permissions")
        ));
    }
}
