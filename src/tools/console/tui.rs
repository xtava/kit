use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Margin, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, List, ListItem, ListState, Paragraph, Widget},
    Frame,
};
use tokio::time::MissedTickBehavior;
use wezterm_term::{
    color::ColorAttribute, Blink, CellAttributes, Intensity, StableRowIndex, Underline,
};

use crate::tui::{
    render_split_divider, theme::NORD, ActionId, ActionInvocation, ActionRegistry,
    ActionRegistryBuilder, ActionSpec, ActionState, ActionUnavailable, CommandPalette,
    CommandPaletteLayout, CommandPaletteOutcome, CommandPalettePlacement, ContextMenu,
    ContextMenuLayout, ContextMenuOutcome, ContextMenuStyle, Direction, EventReader, KeyChord,
    Keybinding, KeybindingPlacement, KeybindingResolution, KeybindingState, LineEditor, MenuId,
    MenuPlacement, NavigationHistory, NavigationMap, NavigationRegion, Session, SessionOptions,
    SettingsEditor, SettingsFlow, SplitDividerStyle, SplitDrag, SplitFrame, SplitMinimums,
    SplitRatio,
};

use super::activity::{AgentActivity, AgentPresentation};
use super::client::{
    ConnectionHealth, ConnectionState, ConsoleClient, ConsoleSnapshot, SessionControl, SessionId,
    SessionView, TerminalContentGeometry, TerminalView,
};
use super::config::{Config, Keybindings, ReadyNotification};
use super::control_center::ConnectedSessionOutcome;
use super::interaction::{
    resolve_control, ControlIntent, ControlOperation, EffectiveLayout, InteractionDecision,
    LayoutPreference, SessionAccess, TerminalOnlyReason,
};
use super::invalidation::ConsoleInvalidations;
use super::panels::{
    close_panel_change, next_split_session, select_change, visible_panel_slots, ClosePanelChange,
    PanelSlot, SelectionChange,
};
use super::perf_trace::{self, InputKind};
use super::scroll::{ScrollMetrics, ScrollState};

const ACTIVITY_INTERVAL: Duration = Duration::from_millis(300);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(4);
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(350);
const WHEEL_SCROLL_ROWS: usize = 3;
const SIDEBAR_STEP: i16 = 40;
const SESSION_MENU: MenuId = MenuId::new("console.session.context");

const SELECT_SESSION: ActionId = ActionId::new("console.session.select");
const CREATE_SESSION: ActionId = ActionId::new("console.session.create");
const ACTIVATE: ActionId = ActionId::new("console.activate");
const DISMISS: ActionId = ActionId::new("console.dismiss");
const RENAME_SESSION: ActionId = ActionId::new("console.session.rename");
const CLOSE_SESSION: ActionId = ActionId::new("console.session.close");
const RELEASE_CONTROL: ActionId = ActionId::new("console.session.releaseControl");
const TAKE_CONTROL: ActionId = ActionId::new("console.session.takeControl");
const PRIMARY_CONTROL: ActionId = ActionId::new("console.session.primaryControl");
const COPY_VISIBLE: ActionId = ActionId::new("console.terminal.copyVisible");
const OPEN_SEARCH: ActionId = ActionId::new("console.terminal.search");
const SPLIT_PANEL: ActionId = ActionId::new("console.panel.split");
const CLOSE_PANEL: ActionId = ActionId::new("console.panel.close");
const FOCUS_OTHER_PANEL: ActionId = ActionId::new("console.panel.focusOther");
const SCROLL_UP: ActionId = ActionId::new("console.terminal.scrollUp");
const SCROLL_DOWN: ActionId = ActionId::new("console.terminal.scrollDown");
const SCROLL_LINE_UP: ActionId = ActionId::new("console.terminal.scrollLineUp");
const SCROLL_LINE_DOWN: ActionId = ActionId::new("console.terminal.scrollLineDown");
const SCROLL_TO_LIVE: ActionId = ActionId::new("console.terminal.scrollToLive");
const PREVIOUS_SESSION: ActionId = ActionId::new("console.session.previous");
const NEXT_SESSION: ActionId = ActionId::new("console.session.next");
const HISTORY_BACK: ActionId = ActionId::new("console.session.historyBack");
const HISTORY_FORWARD: ActionId = ActionId::new("console.session.historyForward");
const FOCUS_SESSIONS: ActionId = ActionId::new("console.focus.sessions");
const FOCUS_LEFT: ActionId = ActionId::new("console.focus.left");
const FOCUS_RIGHT: ActionId = ActionId::new("console.focus.right");
const FOCUS_NEXT: ActionId = ActionId::new("console.focus.next");
const FOCUS_PREVIOUS: ActionId = ActionId::new("console.focus.previous");
const NARROW_SIDEBAR: ActionId = ActionId::new("console.sidebar.narrow");
const WIDEN_SIDEBAR: ActionId = ActionId::new("console.sidebar.widen");
const RESIZE_SIDEBAR: ActionId = ActionId::new("console.sidebar.resize");
const TOGGLE_SIDEBAR: ActionId = ActionId::new("console.sidebar.toggle");
const OPEN_COMMAND_PALETTE: ActionId = ActionId::new("console.commandPalette.open");
const OPEN_SETTINGS: ActionId = ActionId::new("console.settings.open");
const RETRY_CONNECTION: ActionId = ActionId::new("console.connection.retry");
const QUIT: ActionId = ActionId::new("console.quit");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveRegion {
    Sessions,
    PrimaryTerminal,
    SecondaryTerminal,
}

impl PanelSlot {
    const fn region(self) -> ActiveRegion {
        match self {
            Self::Primary => ActiveRegion::PrimaryTerminal,
            Self::Secondary => ActiveRegion::SecondaryTerminal,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceKind {
    Normal,
    Rename,
    Search,
    CommandPalette,
    Settings,
}

enum Surface {
    Normal,
    Rename { id: SessionId, input: LineEditor },
    Search { input: LineEditor, current_match: Option<StableRowIndex> },
    CommandPalette(CommandPalette<ConsoleActionContext>),
    Settings(SettingsEditor),
}

impl Surface {
    fn kind(&self) -> SurfaceKind {
        match self {
            Self::Normal => SurfaceKind::Normal,
            Self::Rename { .. } => SurfaceKind::Rename,
            Self::Search { .. } => SurfaceKind::Search,
            Self::CommandPalette(_) => SurfaceKind::CommandPalette,
            Self::Settings(_) => SurfaceKind::Settings,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConsoleAction {
    SelectSession,
    CreateSession,
    Activate,
    Dismiss,
    RenameSession,
    CloseSession,
    ReleaseControl,
    TakeControl,
    PrimaryControl,
    CopyVisibleTerminal,
    OpenSearch,
    SplitPanel,
    ClosePanel,
    FocusOtherPanel,
    ScrollUp,
    ScrollDown,
    ScrollLineUp,
    ScrollLineDown,
    ScrollToLive,
    PreviousSession,
    NextSession,
    HistoryBack,
    HistoryForward,
    FocusSessions,
    FocusLeft,
    FocusRight,
    FocusNext,
    FocusPrevious,
    NarrowSidebar,
    WidenSidebar,
    ResizeSidebar,
    ToggleSidebar,
    OpenCommandPalette,
    OpenSettings,
    RetryConnection,
    Quit,
}

impl ConsoleAction {
    const ALL: [Self; 36] = [
        Self::SelectSession,
        Self::CreateSession,
        Self::Activate,
        Self::Dismiss,
        Self::RenameSession,
        Self::CloseSession,
        Self::ReleaseControl,
        Self::TakeControl,
        Self::PrimaryControl,
        Self::CopyVisibleTerminal,
        Self::OpenSearch,
        Self::SplitPanel,
        Self::ClosePanel,
        Self::FocusOtherPanel,
        Self::ScrollUp,
        Self::ScrollDown,
        Self::ScrollLineUp,
        Self::ScrollLineDown,
        Self::ScrollToLive,
        Self::PreviousSession,
        Self::NextSession,
        Self::HistoryBack,
        Self::HistoryForward,
        Self::FocusSessions,
        Self::FocusLeft,
        Self::FocusRight,
        Self::FocusNext,
        Self::FocusPrevious,
        Self::NarrowSidebar,
        Self::WidenSidebar,
        Self::ResizeSidebar,
        Self::ToggleSidebar,
        Self::OpenCommandPalette,
        Self::OpenSettings,
        Self::RetryConnection,
        Self::Quit,
    ];

    const fn id(self) -> ActionId {
        match self {
            Self::SelectSession => SELECT_SESSION,
            Self::CreateSession => CREATE_SESSION,
            Self::Activate => ACTIVATE,
            Self::Dismiss => DISMISS,
            Self::RenameSession => RENAME_SESSION,
            Self::CloseSession => CLOSE_SESSION,
            Self::ReleaseControl => RELEASE_CONTROL,
            Self::TakeControl => TAKE_CONTROL,
            Self::PrimaryControl => PRIMARY_CONTROL,
            Self::CopyVisibleTerminal => COPY_VISIBLE,
            Self::OpenSearch => OPEN_SEARCH,
            Self::SplitPanel => SPLIT_PANEL,
            Self::ClosePanel => CLOSE_PANEL,
            Self::FocusOtherPanel => FOCUS_OTHER_PANEL,
            Self::ScrollUp => SCROLL_UP,
            Self::ScrollDown => SCROLL_DOWN,
            Self::ScrollLineUp => SCROLL_LINE_UP,
            Self::ScrollLineDown => SCROLL_LINE_DOWN,
            Self::ScrollToLive => SCROLL_TO_LIVE,
            Self::PreviousSession => PREVIOUS_SESSION,
            Self::NextSession => NEXT_SESSION,
            Self::HistoryBack => HISTORY_BACK,
            Self::HistoryForward => HISTORY_FORWARD,
            Self::FocusSessions => FOCUS_SESSIONS,
            Self::FocusLeft => FOCUS_LEFT,
            Self::FocusRight => FOCUS_RIGHT,
            Self::FocusNext => FOCUS_NEXT,
            Self::FocusPrevious => FOCUS_PREVIOUS,
            Self::NarrowSidebar => NARROW_SIDEBAR,
            Self::WidenSidebar => WIDEN_SIDEBAR,
            Self::ResizeSidebar => RESIZE_SIDEBAR,
            Self::ToggleSidebar => TOGGLE_SIDEBAR,
            Self::OpenCommandPalette => OPEN_COMMAND_PALETTE,
            Self::OpenSettings => OPEN_SETTINGS,
            Self::RetryConnection => RETRY_CONNECTION,
            Self::Quit => QUIT,
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::SelectSession => "Select session",
            Self::CreateSession => "New session",
            Self::Activate => "Attach / confirm",
            Self::Dismiss => "Dismiss",
            Self::RenameSession => "Rename session",
            Self::CloseSession => "Close session",
            Self::ReleaseControl => "Detach / release control",
            Self::TakeControl => "Take control",
            Self::PrimaryControl => "Acquire / release / take control",
            Self::CopyVisibleTerminal => "Copy visible terminal",
            Self::OpenSearch => "Search terminal",
            Self::SplitPanel => "Split terminal panel",
            Self::ClosePanel => "Close terminal panel",
            Self::FocusOtherPanel => "Focus other terminal panel",
            Self::ScrollUp => "Scroll up",
            Self::ScrollDown => "Scroll down",
            Self::ScrollLineUp => "Scroll up one step",
            Self::ScrollLineDown => "Scroll down one step",
            Self::ScrollToLive => "Return to live output",
            Self::PreviousSession => "Previous session",
            Self::NextSession => "Next session",
            Self::HistoryBack => "Session history back",
            Self::HistoryForward => "Session history forward",
            Self::FocusSessions => "Focus sessions",
            Self::FocusLeft => "Focus left",
            Self::FocusRight => "Focus right",
            Self::FocusNext => "Focus next region",
            Self::FocusPrevious => "Focus previous region",
            Self::NarrowSidebar => "Narrow sidebar",
            Self::WidenSidebar => "Widen sidebar",
            Self::ResizeSidebar => "Resize sidebar",
            Self::ToggleSidebar => "Toggle sessions sidebar",
            Self::OpenCommandPalette => "Show command palette",
            Self::OpenSettings => "Open Console settings",
            Self::RetryConnection => "Retry connection",
            Self::Quit => "Quit Console",
        }
    }

    const fn command_palette(self) -> CommandPalettePlacement {
        let (group, group_order, order) = match self {
            Self::CreateSession => ("Session", 10, 10),
            Self::RenameSession => ("Session", 10, 20),
            Self::CloseSession => ("Session", 10, 30),
            Self::PreviousSession => ("Session", 10, 40),
            Self::NextSession => ("Session", 10, 50),
            Self::HistoryBack => ("Session", 10, 60),
            Self::HistoryForward => ("Session", 10, 70),
            Self::ReleaseControl => ("Control", 20, 10),
            Self::TakeControl => ("Control", 20, 20),
            Self::CopyVisibleTerminal => ("Terminal", 30, 10),
            Self::OpenSearch => ("Terminal", 30, 20),
            Self::SplitPanel => ("Terminal", 30, 30),
            Self::ClosePanel => ("Terminal", 30, 40),
            Self::FocusOtherPanel => ("Terminal", 30, 50),
            Self::ScrollUp => ("Terminal", 30, 60),
            Self::ScrollDown => ("Terminal", 30, 70),
            Self::ScrollToLive => ("Terminal", 30, 80),
            Self::FocusSessions => ("View", 40, 10),
            Self::FocusLeft => ("View", 40, 20),
            Self::FocusRight => ("View", 40, 30),
            Self::FocusNext => ("View", 40, 40),
            Self::FocusPrevious => ("View", 40, 50),
            Self::NarrowSidebar => ("View", 40, 60),
            Self::WidenSidebar => ("View", 40, 70),
            Self::ToggleSidebar => ("View", 40, 80),
            Self::RetryConnection => ("Console", 50, 10),
            Self::OpenSettings => ("Console", 50, 20),
            Self::OpenCommandPalette => ("Console", 50, 30),
            Self::Quit => ("Console", 50, 40),
            Self::SelectSession
            | Self::Activate
            | Self::Dismiss
            | Self::PrimaryControl
            | Self::ScrollLineUp
            | Self::ScrollLineDown
            | Self::ResizeSidebar => return CommandPalettePlacement::Hidden,
        };
        CommandPalettePlacement::Visible { group, group_order, order }
    }
}

#[derive(Clone, Copy)]
struct ConsoleActionContext {
    target: Option<SessionId>,
    target_access: Option<SessionAccess>,
    selected: Option<SessionId>,
    selected_index: Option<usize>,
    session_count: usize,
    sidebar_visible: bool,
    region: ActiveRegion,
    focus_left: Option<ActiveRegion>,
    focus_right: Option<ActiveRegion>,
    focus_next: Option<ActiveRegion>,
    focus_previous: Option<ActiveRegion>,
    surface: SurfaceKind,
    has_terminal: bool,
    has_secondary_panel: bool,
    can_split_panel: bool,
    visible_rows: usize,
    can_scroll_up: bool,
    can_scroll_down: bool,
    can_history_back: bool,
    can_history_forward: bool,
    create_cols: u16,
    create_rows: u16,
    requested_ratio: Option<SplitRatio>,
    connection_retryable: bool,
}

enum Effect {
    None,
    ProjectTerminal,
    ReconcileTopology,
    RetryConnection,
    Create { cols: u16, rows: u16 },
    Rename { id: SessionId, title: String },
    Close(SessionId),
    AcquireControl(SessionId),
    ReleaseControl(SessionId),
    TakeControl(SessionId),
    Copy(String),
    SendKey(KeyEvent),
    SendMouse { pane_id: usize, event: MouseEvent, geometry: TerminalContentGeometry },
    Paste(String),
    Quit,
}

#[derive(Clone, Copy)]
struct ActionHitTarget {
    area: Rect,
    action: ConsoleAction,
    target: Option<SessionId>,
}

#[derive(Clone, Copy)]
struct TerminalRegion {
    slot: PanelSlot,
    area: Rect,
    content: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalSelection {
    anchor_line: StableRowIndex,
    anchor_column: usize,
    focus_line: StableRowIndex,
    focus_column: usize,
}

impl TerminalSelection {
    fn point(line: StableRowIndex, column: usize) -> Self {
        Self { anchor_line: line, anchor_column: column, focus_line: line, focus_column: column }
    }

    fn update(&mut self, line: StableRowIndex, column: usize) {
        self.focus_line = line;
        self.focus_column = column;
    }

    fn ordered(self) -> ((StableRowIndex, usize), (StableRowIndex, usize)) {
        let anchor = (self.anchor_line, self.anchor_column);
        let focus = (self.focus_line, self.focus_column);
        if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        }
    }

    fn contains(self, line: StableRowIndex, column: usize) -> bool {
        let (start, end) = self.ordered();
        (line, column) >= start && (line, column) <= end
    }
}

fn selection_outside_projection(
    selection: Option<TerminalSelection>,
    terminal: &TerminalView,
) -> bool {
    let projected_end = terminal
        .first_row
        .saturating_add(StableRowIndex::try_from(terminal.lines.len()).unwrap_or_default());
    selection.is_some_and(|selection| {
        selection.anchor_line < terminal.first_row
            || selection.anchor_line >= projected_end
            || selection.focus_line < terminal.first_row
            || selection.focus_line >= projected_end
    })
}

#[derive(Default)]
struct UiRegions {
    split: Option<SplitFrame>,
    sessions: Option<Rect>,
    terminals: Vec<TerminalRegion>,
    session_rows: Vec<(Rect, SessionId)>,
    action_hits: Vec<ActionHitTarget>,
    context_menu: Option<ContextMenuLayout>,
    command_palette: Option<CommandPaletteLayout>,
}

impl UiRegions {
    fn navigation(&self) -> NavigationMap<ActiveRegion> {
        let mut regions =
            vec![NavigationRegion::new(ActiveRegion::Sessions, self.sessions.unwrap_or_default())];
        regions.extend(
            self.terminals
                .iter()
                .map(|terminal| NavigationRegion::new(terminal.slot.region(), terminal.area)),
        );
        NavigationMap::new(regions)
    }

    fn create_size(&self) -> (u16, u16) {
        self.terminals
            .first()
            .map(|terminal| (terminal.content.width.max(1), terminal.content.height.max(1)))
            .unwrap_or((80, 24))
    }

    fn terminal(&self, slot: PanelSlot) -> Option<TerminalRegion> {
        self.terminals.iter().find(|terminal| terminal.slot == slot).copied()
    }

    fn terminal_at(&self, position: Position) -> Option<TerminalRegion> {
        self.terminals.iter().find(|terminal| terminal.content.contains(position)).copied()
    }
}

struct SecondaryPanel {
    session_id: SessionId,
    terminal: Option<TerminalView>,
    scroll: ScrollState,
    selection: Option<TerminalSelection>,
    last_terminal_size: Option<(usize, u16, u16)>,
}

#[derive(Clone, Copy)]
struct CloseTransition {
    pane_id: usize,
    local_pane_id: usize,
}

struct App {
    client: ConsoleClient,
    config: Config,
    snapshot: ConsoleSnapshot,
    selected: Option<SessionId>,
    active_region: ActiveRegion,
    focused_panel: PanelSlot,
    surface: Surface,
    keybinding_state: KeybindingState,
    layout: LayoutPreference,
    split_drag: Option<SplitDrag<()>>,
    history: NavigationHistory<SessionId>,
    menu: Option<ContextMenu<ConsoleActionContext>>,
    registry: ActionRegistry<ConsoleActionContext, ConsoleAction>,
    last_terminal_size: Option<(usize, u16, u16)>,
    last_session_click: Option<(SessionId, Instant)>,
    scroll: ScrollState,
    selection: Option<TerminalSelection>,
    secondary: Option<SecondaryPanel>,
    notice: Option<String>,
    connection_generation: u64,
    connection: ConnectionState,
    connection_detail: Option<String>,
    close_transitions: Vec<CloseTransition>,
}

impl App {
    async fn new(client: ConsoleClient, config: Config) -> Result<Self> {
        let mut snapshot = client.snapshot(None).await?;
        let health = client.drain_connection_health()?;
        let connection_generation = health.map_or(0, |health| health.generation);
        let connection = health.map_or(ConnectionState::Ready, |health| health.state);
        let connection_detail = client.drain_remote_status().flatten().map(|status| status.text());
        let selected = snapshot.sessions.first().map(|session| session.id);
        let mut history = NavigationHistory::default();
        if let Some(id) = selected {
            history.visit(id);
            let pane_id = snapshot
                .sessions
                .iter()
                .find(|session| session.id == id)
                .map(|session| session.pane_id);
            snapshot.terminal =
                pane_id.map(|pane_id| client.project_terminal(pane_id)).transpose()?.flatten();
        }
        let registry = console_actions(config.keybindings())?;
        Ok(Self {
            layout: LayoutPreference::split(config.sidebar_split_ratio()),
            client,
            config,
            snapshot,
            selected,
            active_region: ActiveRegion::Sessions,
            focused_panel: PanelSlot::Primary,
            surface: Surface::Normal,
            keybinding_state: KeybindingState::default(),
            split_drag: None,
            history,
            menu: None,
            registry,
            last_terminal_size: None,
            last_session_click: None,
            scroll: ScrollState::default(),
            selection: None,
            secondary: None,
            notice: None,
            connection_generation,
            connection,
            connection_detail,
            close_transitions: Vec::new(),
        })
    }

    async fn reconcile_topology(&mut self) -> Result<bool> {
        let mut changed = false;
        if let Some(status) = self.client.drain_remote_status() {
            let detail = status.map(|status| status.text());
            changed |= self.connection_detail != detail;
            self.connection_detail = detail;
        }
        if let Some(health) = self.client.drain_connection_health()? {
            let previous = (self.connection_generation, self.connection);
            self.apply_connection_health(health);
            changed |= previous != (self.connection_generation, self.connection);
        }
        if matches!(
            self.connection,
            ConnectionState::Attaching | ConnectionState::Reconnecting { .. }
        ) {
            return Ok(changed);
        }
        let mut snapshot = match self.client.snapshot(self.selected).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if let Some(health) = self.client.drain_connection_health()? {
                    self.apply_connection_health(health);
                }
                if matches!(
                    self.connection,
                    ConnectionState::Attaching | ConnectionState::Reconnecting { .. }
                ) {
                    return Ok(true);
                }
                return Err(error);
            }
        };
        let remote_panes =
            snapshot.sessions.iter().map(|session| session.pane_id).collect::<Vec<_>>();
        snapshot.sessions.retain(|session| {
            !self.close_transitions.iter().any(|transition| transition.pane_id == session.pane_id)
        });
        self.close_transitions.retain(|transition| remote_panes.contains(&transition.pane_id));
        let selected = self.selected;
        changed |= self.reconcile(snapshot);
        if self.selected != selected {
            changed |= self.project_terminal()?;
        }
        Ok(changed)
    }

    async fn reconcile_or_notice(&mut self) -> bool {
        match self.reconcile_topology().await {
            Ok(changed) => changed,
            Err(error) => {
                if let Ok(Some(health)) = self.client.drain_connection_health() {
                    self.apply_connection_health(health);
                }
                let notice = format!("Could not refresh Console: {error:#}");
                let changed = self.notice.as_deref() != Some(&notice);
                self.notice = Some(notice);
                changed
            }
        }
    }

    async fn begin_close(&mut self, id: SessionId) -> Result<()> {
        let pane_id =
            self.session(id).map(|session| session.pane_id).context("session no longer exists")?;
        let local_pane_id = self
            .client
            .local_pane_id(pane_id)?
            .context("session has no local terminal projection")?;
        let closing_secondary = self.secondary.as_ref().is_some_and(|panel| panel.session_id == id);
        let restore_terminal_focus = self.focused_session_id() == Some(id)
            && matches!(
                self.active_region,
                ActiveRegion::PrimaryTerminal | ActiveRegion::SecondaryTerminal
            );
        self.client.close_pane(pane_id).await?;
        self.close_transitions.push(CloseTransition { pane_id, local_pane_id });
        self.snapshot.sessions.retain(|session| session.pane_id != pane_id);
        if closing_secondary {
            self.secondary = None;
            self.focused_panel = PanelSlot::Primary;
            self.active_region = ActiveRegion::PrimaryTerminal;
        }
        if self.selected == Some(id) {
            if let Some(secondary) = self.secondary.take() {
                self.selected = Some(secondary.session_id);
                self.snapshot.terminal = secondary.terminal;
                self.scroll = secondary.scroll;
                self.selection = secondary.selection;
                self.last_terminal_size = secondary.last_terminal_size;
                self.focused_panel = PanelSlot::Primary;
            } else {
                self.selected = self.snapshot.sessions.first().map(|session| session.id);
                self.scroll.reset();
                self.selection = None;
            }
            if let Some(selected) = self.selected {
                self.history.replace_current(selected);
            }
            self.project_terminal()?;
        }
        if !restore_terminal_focus {
            return Ok(());
        }
        let Some((replacement_id, replacement_access)) =
            self.focused_session().map(|session| (session.id, session_access(session)))
        else {
            self.active_region = ActiveRegion::Sessions;
            return Ok(());
        };
        match replacement_access {
            SessionAccess::Available | SessionAccess::Synchronizing => {
                self.client.acquire_control(replacement_id).await?;
                self.active_region = self.focused_panel.region();
                if let Some(session) =
                    self.snapshot.sessions.iter_mut().find(|session| session.id == replacement_id)
                {
                    session.control = super::client::SessionControl::Controller;
                }
            }
            SessionAccess::ControlledBySelf => {
                self.active_region = self.focused_panel.region();
            }
            SessionAccess::ControlledByOther => {
                self.active_region = ActiveRegion::Sessions;
                self.notice =
                    Some("Focused session closed; the replacement is controlled elsewhere".into());
            }
        }
        Ok(())
    }

    fn confirm_closed_panes(&mut self, local_pane_ids: &[usize]) -> bool {
        let before = self.close_transitions.len();
        self.close_transitions
            .retain(|transition| !local_pane_ids.contains(&transition.local_pane_id));
        self.close_transitions.len() != before
    }

    fn has_pending_closures(&self) -> bool {
        !self.close_transitions.is_empty()
    }

    fn project_terminal(&mut self) -> Result<bool> {
        let pane_id = self.primary_session().map(|session| session.pane_id);
        let terminal =
            pane_id.map(|pane_id| self.client.project_terminal(pane_id)).transpose()?.flatten();
        let changed = self.snapshot.terminal != terminal;
        self.snapshot.terminal = terminal;
        let secondary_terminal = self
            .secondary
            .as_ref()
            .and_then(|panel| self.session(panel.session_id))
            .map(|session| self.client.project_terminal(session.pane_id))
            .transpose()?
            .flatten();
        let secondary_changed =
            self.secondary.as_ref().is_some_and(|panel| panel.terminal != secondary_terminal);
        if let Some(panel) = self.secondary.as_mut() {
            panel.terminal = secondary_terminal;
        }
        if changed || secondary_changed {
            self.normalize_terminal_state();
        }
        Ok(changed || secondary_changed)
    }

    fn refresh_activity(&mut self) -> Result<super::client::ActivityRefresh> {
        let selected = match self.focused_panel {
            PanelSlot::Primary => self.selected,
            PanelSlot::Secondary => self.secondary.as_ref().map(|panel| panel.session_id),
        };
        let terminal = match self.focused_panel {
            PanelSlot::Primary => self.snapshot.terminal.as_ref(),
            PanelSlot::Secondary => {
                self.secondary.as_ref().and_then(|panel| panel.terminal.as_ref())
            }
        };
        self.client.refresh_activity(&mut self.snapshot.sessions, selected, terminal)
    }

    fn reconcile(&mut self, snapshot: ConsoleSnapshot) -> bool {
        let mut changed = self.snapshot != snapshot;
        self.snapshot = snapshot;
        let previous_selection = self.selected;
        let selection_exists = self
            .selected
            .is_some_and(|id| self.snapshot.sessions.iter().any(|session| session.id == id));
        if !selection_exists {
            self.selected = self.snapshot.sessions.first().map(|session| session.id);
            self.scroll.reset();
            if let Some(id) = self.selected {
                self.history.replace_current(id);
            }
        }
        if self.secondary.as_ref().is_some_and(|panel| {
            Some(panel.session_id) == self.selected || !self.has_session(Some(panel.session_id))
        }) {
            self.secondary = None;
            self.focused_panel = PanelSlot::Primary;
            if self.active_region == ActiveRegion::SecondaryTerminal {
                self.active_region = ActiveRegion::PrimaryTerminal;
            }
        }
        changed |= self.selected != previous_selection;
        if self.menu.as_ref().is_some_and(|menu| !self.has_session(menu.context().target)) {
            self.menu = None;
        }
        if let Surface::Rename { id, .. } = &self.surface {
            if !self.has_session(Some(*id)) {
                self.surface = Surface::Normal;
            }
        }
        self.normalize_terminal_state();
        changed
    }

    fn normalize_terminal_state(&mut self) {
        if let Some(terminal) = self.snapshot.terminal.as_ref() {
            let metrics =
                ScrollMetrics::new(terminal.first_row, terminal.lines.len(), terminal.rows);
            self.scroll.normalize(metrics);
            if selection_outside_projection(self.selection, terminal) {
                self.selection = None;
            }
        } else {
            self.scroll.reset();
            self.selection = None;
        }
        if let Some(panel) = self.secondary.as_mut() {
            if let Some(terminal) = panel.terminal.as_ref() {
                let metrics =
                    ScrollMetrics::new(terminal.first_row, terminal.lines.len(), terminal.rows);
                panel.scroll.normalize(metrics);
                if selection_outside_projection(panel.selection, terminal) {
                    panel.selection = None;
                }
            } else {
                panel.scroll.reset();
                panel.selection = None;
            }
        }
    }

    fn apply_connection_health(&mut self, health: ConnectionHealth) {
        if health.generation >= self.connection_generation {
            self.connection_generation = health.generation;
            self.connection = health.state;
        }
    }

    fn has_session(&self, id: Option<SessionId>) -> bool {
        id.is_some_and(|id| self.snapshot.sessions.iter().any(|session| session.id == id))
    }

    fn primary_session(&self) -> Option<&SessionView> {
        let selected = self.selected?;
        self.snapshot.sessions.iter().find(|session| session.id == selected)
    }

    fn focused_session_id(&self) -> Option<SessionId> {
        match self.focused_panel {
            PanelSlot::Primary => self.selected,
            PanelSlot::Secondary => self.secondary.as_ref().map(|panel| panel.session_id),
        }
    }

    fn panel_session_id(&self, slot: PanelSlot) -> Option<SessionId> {
        match slot {
            PanelSlot::Primary => self.selected,
            PanelSlot::Secondary => self.secondary.as_ref().map(|panel| panel.session_id),
        }
    }

    fn focused_session(&self) -> Option<&SessionView> {
        self.focused_session_id().and_then(|id| self.session(id))
    }

    fn panel_terminal(&self, slot: PanelSlot) -> Option<&TerminalView> {
        match slot {
            PanelSlot::Primary => self.snapshot.terminal.as_ref(),
            PanelSlot::Secondary => self.secondary.as_ref()?.terminal.as_ref(),
        }
    }

    fn panel_scroll(&self, slot: PanelSlot) -> ScrollState {
        match slot {
            PanelSlot::Primary => self.scroll,
            PanelSlot::Secondary => {
                self.secondary.as_ref().map_or(ScrollState::default(), |panel| panel.scroll)
            }
        }
    }

    fn panel_selection(&self, slot: PanelSlot) -> Option<TerminalSelection> {
        match slot {
            PanelSlot::Primary => self.selection,
            PanelSlot::Secondary => self.secondary.as_ref().and_then(|panel| panel.selection),
        }
    }

    fn focus_panel(&mut self, slot: PanelSlot) {
        if slot == PanelSlot::Secondary && self.secondary.is_none() {
            return;
        }
        self.focused_panel = slot;
        self.active_region = slot.region();
    }

    fn session(&self, id: SessionId) -> Option<&SessionView> {
        self.snapshot.sessions.iter().find(|session| session.id == id)
    }

    fn selected_index(&self) -> Option<usize> {
        let selected = self.focused_session_id()?;
        self.snapshot.sessions.iter().position(|session| session.id == selected)
    }

    fn select(&mut self, id: SessionId) -> bool {
        let changed = self.assign_session(id);
        if changed {
            self.history.visit(id);
        }
        changed
    }

    fn assign_session(&mut self, id: SessionId) -> bool {
        if !self.has_session(Some(id)) {
            return false;
        }
        let secondary = self.secondary.as_ref().map(|panel| panel.session_id);
        match select_change(self.selected, secondary, self.focused_panel, id) {
            SelectionChange::Unchanged => return false,
            SelectionChange::Focus(slot) => self.focused_panel = slot,
            SelectionChange::Replace(slot) => match slot {
                PanelSlot::Primary => {
                    self.selected = Some(id);
                    self.scroll.reset();
                    self.selection = None;
                }
                PanelSlot::Secondary => {
                    if let Some(panel) = self.secondary.as_mut() {
                        panel.session_id = id;
                        panel.scroll.reset();
                        panel.selection = None;
                        panel.last_terminal_size = None;
                    }
                }
            },
        }
        if self.active_region != ActiveRegion::Sessions {
            self.active_region = self.focused_panel.region();
        }
        true
    }

    fn move_selection(&mut self, delta: isize) -> bool {
        let len = self.snapshot.sessions.len();
        if len == 0 {
            return false;
        }
        let index = self.selected_index().unwrap_or_default();
        let next = (index as isize + delta).clamp(0, len as isize - 1) as usize;
        self.select(self.snapshot.sessions[next].id)
    }

    fn split_panel(&mut self) -> bool {
        if self.secondary.is_some() {
            self.focus_panel(PanelSlot::Secondary);
            return false;
        }
        let Some(session_id) = next_split_session(
            self.selected,
            self.secondary.as_ref().map(|panel| panel.session_id),
            self.snapshot.sessions.iter().map(|session| session.id),
        ) else {
            return false;
        };
        self.secondary = Some(SecondaryPanel {
            session_id,
            terminal: None,
            scroll: ScrollState::default(),
            selection: None,
            last_terminal_size: None,
        });
        self.focus_panel(PanelSlot::Secondary);
        self.history.visit(session_id);
        true
    }

    fn close_focused_panel(&mut self) -> bool {
        let Some(change) = close_panel_change(self.secondary.is_some(), self.focused_panel) else {
            return false;
        };
        let Some(secondary) = self.secondary.take() else {
            return false;
        };
        if change == ClosePanelChange::PromoteSecondary {
            self.selected = Some(secondary.session_id);
            self.snapshot.terminal = secondary.terminal;
            self.scroll = secondary.scroll;
            self.selection = secondary.selection;
            self.last_terminal_size = secondary.last_terminal_size;
        }
        self.focused_panel = PanelSlot::Primary;
        self.active_region = ActiveRegion::PrimaryTerminal;
        true
    }

    fn history_target_exists(&self, delta: isize) -> bool {
        let mut offset = delta;
        while let Some((_, id)) = self.history.target(offset) {
            if self.has_session(Some(*id)) {
                return true;
            }
            offset += delta;
        }
        false
    }

    fn navigate_history(&mut self, delta: isize) -> bool {
        let mut offset = delta;
        while let Some((cursor, id)) = self.history.target(offset).map(|(cursor, id)| (cursor, *id))
        {
            if self.has_session(Some(id)) {
                self.history.select(cursor);
                let _ = self.assign_session(id);
                return true;
            }
            offset += delta;
        }
        false
    }

    fn resize_requests(&mut self, regions: &UiRegions) -> Vec<(usize, usize, u16, u16)> {
        let mut requests = Vec::new();
        for region in &regions.terminals {
            let session_id = match region.slot {
                PanelSlot::Primary => self.selected,
                PanelSlot::Secondary => self.secondary.as_ref().map(|panel| panel.session_id),
            };
            let values = session_id
                .and_then(|id| self.session(id))
                .zip(self.panel_terminal(region.slot))
                .map(|(session, terminal)| (session.control, session.tab_id, terminal.pane_id));
            let last_terminal_size = match region.slot {
                PanelSlot::Primary => &mut self.last_terminal_size,
                PanelSlot::Secondary => {
                    let Some(panel) = self.secondary.as_mut() else {
                        continue;
                    };
                    &mut panel.last_terminal_size
                }
            };
            let Some((control, tab_id, pane_id)) = values else {
                *last_terminal_size = None;
                continue;
            };
            let content = region.content;
            if control != SessionControl::Controller || content.width == 0 || content.height == 0 {
                *last_terminal_size = None;
                continue;
            }
            let observed = (pane_id, content.width, content.height);
            if *last_terminal_size != Some(observed) {
                *last_terminal_size = Some(observed);
                requests.push((tab_id, pane_id, content.width, content.height));
            }
        }
        requests
    }

    fn action_context(
        &self,
        target: Option<SessionId>,
        regions: &UiRegions,
        requested_ratio: Option<SplitRatio>,
    ) -> ConsoleActionContext {
        let target = target.or(self.focused_session_id());
        let target_access = target.and_then(|id| self.session(id)).map(session_access);
        let (create_cols, create_rows) = regions.create_size();
        let visible_rows = regions
            .terminal(self.focused_panel)
            .map_or(0, |region| usize::from(region.content.height));
        let terminal = self.panel_terminal(self.focused_panel);
        let scroll = self.panel_scroll(self.focused_panel);
        let scroll_metrics = terminal.map(|terminal| {
            ScrollMetrics::new(terminal.first_row, terminal.lines.len(), visible_rows)
        });
        let navigation = regions.navigation();
        ConsoleActionContext {
            target,
            target_access,
            selected: self.focused_session_id(),
            selected_index: self.selected_index(),
            session_count: self.snapshot.sessions.len(),
            sidebar_visible: regions.sessions.is_some(),
            region: self.active_region,
            focus_left: navigation.neighbor(self.active_region, Direction::Left),
            focus_right: navigation.neighbor(self.active_region, Direction::Right),
            focus_next: navigation.next(self.active_region),
            focus_previous: navigation.previous(self.active_region),
            surface: self.surface.kind(),
            has_terminal: terminal.is_some(),
            has_secondary_panel: self.secondary.is_some(),
            can_split_panel: self.secondary.is_none() && self.snapshot.sessions.len() > 1,
            visible_rows,
            can_scroll_up: scroll_metrics.is_some_and(|metrics| scroll.can_scroll_up(metrics)),
            can_scroll_down: scroll.can_scroll_down(),
            can_history_back: self.history_target_exists(-1),
            can_history_forward: self.history_target_exists(1),
            create_cols,
            create_rows,
            requested_ratio,
            connection_retryable: matches!(
                self.connection,
                ConnectionState::Failed
                    | ConnectionState::RetryExhausted
                    | ConnectionState::Detached
            ),
        }
    }

    fn invoke_action(
        &mut self,
        action: ConsoleAction,
        target: Option<SessionId>,
        regions: &UiRegions,
    ) -> Result<Effect> {
        self.invoke(
            ActionInvocation::new(action.id(), self.action_context(target, regions, None)),
            regions,
        )
    }

    fn invoke(
        &mut self,
        invocation: ActionInvocation<ConsoleActionContext>,
        regions: &UiRegions,
    ) -> Result<Effect> {
        // Context menus intentionally retain only their semantic target. Every invocation is
        // rehydrated from the latest snapshot so a frame rendered as `release` cannot release a
        // lease that changed to observer before the click was dispatched.
        let context = self.action_context(
            invocation.context.target,
            regions,
            invocation.context.requested_ratio,
        );
        let invocation = ActionInvocation::new(invocation.action, context);
        let action = match self.registry.command_for(&invocation) {
            Ok(action) => action,
            Err(ActionUnavailable::Disabled { reason, .. }) => {
                self.notice = Some(reason.into_owned());
                return Ok(Effect::None);
            }
            Err(ActionUnavailable::Unknown { .. }) => {
                self.notice = Some("That Console action is no longer available".to_owned());
                return Ok(Effect::None);
            }
        };
        Ok(self.evaluate(action, invocation.context))
    }

    fn evaluate(&mut self, action: ConsoleAction, context: ConsoleActionContext) -> Effect {
        match action {
            ConsoleAction::SelectSession => {
                let _changed = context.target.is_some_and(|id| self.select(id));
                Effect::ProjectTerminal
            }
            ConsoleAction::CreateSession => {
                Effect::Create { cols: context.create_cols, rows: context.create_rows }
            }
            ConsoleAction::Activate => self.activate(context),
            ConsoleAction::Dismiss => {
                self.surface = Surface::Normal;
                Effect::None
            }
            ConsoleAction::RenameSession => {
                if let Some(session) = context.target.and_then(|id| self.session(id)) {
                    let id = session.id;
                    let title = session.title.clone();
                    let mut input = LineEditor::default();
                    input.set(title);
                    self.surface = Surface::Rename { id, input };
                }
                Effect::None
            }
            ConsoleAction::CloseSession => {
                context.target.map(Effect::Close).unwrap_or(Effect::None)
            }
            ConsoleAction::ReleaseControl => {
                self.control_intent(context.target, ControlIntent::Release)
            }
            ConsoleAction::TakeControl => self.control_intent(context.target, ControlIntent::Take),
            ConsoleAction::PrimaryControl => {
                self.control_intent(context.target, ControlIntent::Primary)
            }
            ConsoleAction::CopyVisibleTerminal => self
                .selected_or_visible_terminal_text(context.visible_rows)
                .map(Effect::Copy)
                .unwrap_or(Effect::None),
            ConsoleAction::OpenSearch => {
                self.surface =
                    Surface::Search { input: LineEditor::default(), current_match: None };
                Effect::None
            }
            ConsoleAction::SplitPanel => {
                let _ = self.split_panel();
                Effect::ProjectTerminal
            }
            ConsoleAction::ClosePanel => {
                let _ = self.close_focused_panel();
                Effect::None
            }
            ConsoleAction::FocusOtherPanel => {
                let slot = match self.focused_panel {
                    PanelSlot::Primary => PanelSlot::Secondary,
                    PanelSlot::Secondary => PanelSlot::Primary,
                };
                self.focus_panel(slot);
                Effect::None
            }
            ConsoleAction::ScrollUp => {
                let page = context.visible_rows.saturating_sub(1).max(1);
                self.scroll_up(page, context.visible_rows);
                self.clear_focused_selection();
                Effect::None
            }
            ConsoleAction::ScrollDown => {
                let page = context.visible_rows.saturating_sub(1).max(1);
                self.scroll_down(page, context.visible_rows);
                self.clear_focused_selection();
                Effect::None
            }
            ConsoleAction::ScrollLineUp => {
                self.scroll_up(WHEEL_SCROLL_ROWS, context.visible_rows);
                self.clear_focused_selection();
                Effect::None
            }
            ConsoleAction::ScrollLineDown => {
                self.scroll_down(WHEEL_SCROLL_ROWS, context.visible_rows);
                self.clear_focused_selection();
                Effect::None
            }
            ConsoleAction::ScrollToLive => {
                self.reset_focused_terminal_state();
                Effect::None
            }
            ConsoleAction::PreviousSession => {
                self.move_selection(-1);
                Effect::ProjectTerminal
            }
            ConsoleAction::NextSession => {
                self.move_selection(1);
                Effect::ProjectTerminal
            }
            ConsoleAction::HistoryBack => {
                self.navigate_history(-1);
                Effect::ProjectTerminal
            }
            ConsoleAction::HistoryForward => {
                self.navigate_history(1);
                Effect::ProjectTerminal
            }
            ConsoleAction::FocusSessions => {
                self.active_region = ActiveRegion::Sessions;
                Effect::None
            }
            ConsoleAction::FocusLeft => {
                if let Some(region) = context.focus_left {
                    self.active_region = region;
                    self.sync_focused_panel_from_region();
                }
                Effect::None
            }
            ConsoleAction::FocusRight => {
                if let Some(region) = context.focus_right {
                    self.active_region = region;
                    self.sync_focused_panel_from_region();
                }
                Effect::None
            }
            ConsoleAction::FocusNext => {
                if let Some(region) = context.focus_next {
                    self.active_region = region;
                    self.sync_focused_panel_from_region();
                }
                Effect::None
            }
            ConsoleAction::FocusPrevious => {
                if let Some(region) = context.focus_previous {
                    self.active_region = region;
                    self.sync_focused_panel_from_region();
                }
                Effect::None
            }
            ConsoleAction::NarrowSidebar => {
                self.persist_split_ratio(self.layout.restore_ratio().adjusted(-SIDEBAR_STEP));
                Effect::None
            }
            ConsoleAction::WidenSidebar => {
                self.persist_split_ratio(self.layout.restore_ratio().adjusted(SIDEBAR_STEP));
                Effect::None
            }
            ConsoleAction::ResizeSidebar => {
                if let Some(ratio) = context.requested_ratio {
                    self.layout = self.layout.with_ratio(ratio);
                }
                Effect::None
            }
            ConsoleAction::ToggleSidebar => {
                self.toggle_sidebar();
                Effect::None
            }
            ConsoleAction::OpenCommandPalette => {
                self.surface =
                    Surface::CommandPalette(CommandPalette::open(context, &self.registry));
                Effect::None
            }
            ConsoleAction::OpenSettings => {
                self.surface = Surface::Settings(SettingsEditor::open(
                    self.config.store(),
                    vec![super::config::settings()],
                    NORD,
                ));
                Effect::None
            }
            ConsoleAction::RetryConnection => Effect::RetryConnection,
            ConsoleAction::Quit => Effect::Quit,
        }
    }

    fn activate(&mut self, context: ConsoleActionContext) -> Effect {
        match &mut self.surface {
            Surface::Normal => self.control_intent(context.selected, ControlIntent::Activate),
            Surface::Rename { id, input } => {
                let id = *id;
                let Some(title) = validated_rename(input) else {
                    self.notice = Some("Session name cannot be empty".to_owned());
                    return Effect::None;
                };
                self.surface = Surface::Normal;
                Effect::Rename { id, title }
            }
            Surface::Search { .. } => {
                self.advance_search(context.visible_rows);
                Effect::None
            }
            Surface::CommandPalette(_) | Surface::Settings(_) => Effect::None,
        }
    }

    fn reload_config(&mut self) {
        let store = self.config.store();
        match Config::load(store).and_then(|config| {
            let registry = console_actions(config.keybindings())?;
            Ok((config, registry))
        }) {
            Ok((config, registry)) => {
                self.layout = self.layout.with_ratio(config.sidebar_split_ratio());
                self.config = config;
                self.registry = registry;
            }
            Err(error) => {
                self.notice = Some(format!("Could not reload Console settings: {error:#}"));
            }
        }
    }

    fn control_intent(&mut self, target: Option<SessionId>, intent: ControlIntent) -> Effect {
        let Some(id) = target else {
            return Effect::None;
        };
        let Some(access) = self.session(id).map(session_access) else {
            return Effect::ReconcileTopology;
        };
        match resolve_control(intent, access) {
            InteractionDecision::FocusTerminal => {
                self.active_region = self.focused_panel.region();
                Effect::None
            }
            InteractionDecision::Control(ControlOperation::Acquire) => Effect::AcquireControl(id),
            InteractionDecision::Control(ControlOperation::Take) => Effect::TakeControl(id),
            InteractionDecision::Control(ControlOperation::Release) => Effect::ReleaseControl(id),
            InteractionDecision::Wait => {
                self.notice = Some("Session control is synchronizing…".to_owned());
                Effect::None
            }
            InteractionDecision::Unavailable(reason) => {
                self.notice = Some(reason.to_owned());
                Effect::None
            }
        }
    }

    fn advance_search(&mut self, visible_rows: usize) {
        let Surface::Search { input, current_match } = &self.surface else {
            return;
        };
        let query = input.value().to_owned();
        let current_match = *current_match;
        if query.is_empty() {
            if let Surface::Search { current_match, .. } = &mut self.surface {
                *current_match = None;
            }
            return;
        }
        let slot = self.focused_panel;
        let Some(terminal) = self.panel_terminal(slot) else {
            return;
        };
        let metrics =
            ScrollMetrics::new(terminal.first_row, terminal.lines.len(), visible_rows.max(1));
        let matches = terminal
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.as_str().contains(&query))
            .map(|(index, _)| {
                terminal
                    .first_row
                    .saturating_add(StableRowIndex::try_from(index).unwrap_or_default())
            })
            .collect::<Vec<_>>();
        if matches.is_empty() {
            self.notice = Some(format!("No matches for {query:?}"));
            if let Surface::Search { current_match, .. } = &mut self.surface {
                *current_match = None;
            }
            return;
        }
        let next = current_match
            .and_then(|current| matches.iter().copied().find(|row| *row > current))
            .unwrap_or(matches[0]);
        if let Surface::Search { current_match, .. } = &mut self.surface {
            *current_match = Some(next);
        }
        match slot {
            PanelSlot::Primary => self.scroll.scroll_to_row(next, metrics),
            PanelSlot::Secondary => {
                if let Some(panel) = self.secondary.as_mut() {
                    panel.scroll.scroll_to_row(next, metrics);
                }
            }
        }
        self.notice = Some(format!(
            "Match {} of {}",
            matches.iter().position(|index| *index == next).unwrap_or_default() + 1,
            matches.len()
        ));
    }

    fn visible_terminal_text(&self, visible_rows: usize) -> Option<String> {
        let terminal = self.panel_terminal(self.focused_panel)?;
        let range = self.panel_scroll(self.focused_panel).visible_range(ScrollMetrics::new(
            terminal.first_row,
            terminal.lines.len(),
            visible_rows,
        ));
        let text = terminal.lines[range]
            .iter()
            .map(|line| line.as_str().trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n");
        (!text.is_empty()).then_some(text)
    }

    fn selected_or_visible_terminal_text(&self, visible_rows: usize) -> Option<String> {
        self.selection_text().or_else(|| self.visible_terminal_text(visible_rows))
    }

    fn selection_text(&self) -> Option<String> {
        let selection = self.panel_selection(self.focused_panel)?;
        let terminal = self.panel_terminal(self.focused_panel)?;
        if terminal.lines.is_empty() {
            return None;
        }
        let (start, end) = selection.ordered();
        let projected_end =
            terminal.first_row.saturating_add(StableRowIndex::try_from(terminal.lines.len()).ok()?);
        let first_selected = start.0.max(terminal.first_row);
        let last_selected = end.0.min(projected_end.saturating_sub(1));
        if first_selected > last_selected {
            return None;
        }
        let mut selected_lines = Vec::new();
        for stable_row in first_selected..=last_selected {
            let line_index = usize::try_from(stable_row.saturating_sub(terminal.first_row)).ok()?;
            let line = &terminal.lines[line_index];
            let mut text = String::new();
            for cell in line
                .visible_cells()
                .filter(|cell| selection.contains(stable_row, cell.cell_index()))
            {
                text.push_str(cell.str());
            }
            selected_lines.push(text.trim_end().to_owned());
        }
        let text = selected_lines.join("\n");
        (!text.is_empty()).then_some(text)
    }

    fn begin_selection(&mut self, position: Position, content: Rect) {
        let slot = self.focused_panel;
        let Some((line, column)) =
            terminal_point(self.panel_terminal(slot), content, self.panel_scroll(slot), position)
        else {
            return;
        };
        match slot {
            PanelSlot::Primary => self.selection = Some(TerminalSelection::point(line, column)),
            PanelSlot::Secondary => {
                if let Some(panel) = self.secondary.as_mut() {
                    panel.selection = Some(TerminalSelection::point(line, column));
                }
            }
        }
    }

    fn update_selection(&mut self, position: Position, content: Rect) {
        let slot = self.focused_panel;
        let Some((line, column)) =
            terminal_point(self.panel_terminal(slot), content, self.panel_scroll(slot), position)
        else {
            return;
        };
        match slot {
            PanelSlot::Primary => {
                if let Some(selection) = self.selection.as_mut() {
                    selection.update(line, column);
                }
            }
            PanelSlot::Secondary => {
                if let Some(selection) =
                    self.secondary.as_mut().and_then(|panel| panel.selection.as_mut())
                {
                    selection.update(line, column);
                }
            }
        }
    }

    fn scroll_metrics(&self, slot: PanelSlot, visible_rows: usize) -> Option<ScrollMetrics> {
        self.panel_terminal(slot).map(|terminal| {
            ScrollMetrics::new(terminal.first_row, terminal.lines.len(), visible_rows)
        })
    }

    fn scroll_up(&mut self, rows: usize, visible_rows: usize) {
        let slot = self.focused_panel;
        if let Some(metrics) = self.scroll_metrics(slot, visible_rows) {
            match slot {
                PanelSlot::Primary => self.scroll.scroll_up(rows, metrics),
                PanelSlot::Secondary => {
                    if let Some(panel) = self.secondary.as_mut() {
                        panel.scroll.scroll_up(rows, metrics);
                    }
                }
            }
        }
    }

    fn scroll_down(&mut self, rows: usize, visible_rows: usize) {
        let slot = self.focused_panel;
        if let Some(metrics) = self.scroll_metrics(slot, visible_rows) {
            match slot {
                PanelSlot::Primary => self.scroll.scroll_down(rows, metrics),
                PanelSlot::Secondary => {
                    if let Some(panel) = self.secondary.as_mut() {
                        panel.scroll.scroll_down(rows, metrics);
                    }
                }
            }
        }
    }

    fn clear_focused_selection(&mut self) {
        match self.focused_panel {
            PanelSlot::Primary => self.selection = None,
            PanelSlot::Secondary => {
                if let Some(panel) = self.secondary.as_mut() {
                    panel.selection = None;
                }
            }
        }
    }

    fn reset_focused_terminal_state(&mut self) {
        match self.focused_panel {
            PanelSlot::Primary => {
                self.scroll.reset();
                self.selection = None;
            }
            PanelSlot::Secondary => {
                if let Some(panel) = self.secondary.as_mut() {
                    panel.scroll.reset();
                    panel.selection = None;
                }
            }
        }
    }

    fn sync_focused_panel_from_region(&mut self) {
        match self.active_region {
            ActiveRegion::PrimaryTerminal => self.focused_panel = PanelSlot::Primary,
            ActiveRegion::SecondaryTerminal if self.secondary.is_some() => {
                self.focused_panel = PanelSlot::Secondary;
            }
            ActiveRegion::Sessions | ActiveRegion::SecondaryTerminal => {}
        }
    }

    fn terminal_input_enabled(&self) -> bool {
        self.focused_session()
            .is_some_and(|session| session_access(session).permits_terminal_input())
    }

    fn register_session_click(&mut self, id: SessionId, now: Instant) -> bool {
        let double = self.last_session_click.is_some_and(|(previous, at)| {
            previous == id && now.duration_since(at) <= DOUBLE_CLICK_INTERVAL
        });
        self.last_session_click = (!double).then_some((id, now));
        double
    }

    fn search_query(&self) -> Option<&str> {
        match &self.surface {
            Surface::Search { input, .. } if !input.value().is_empty() => Some(input.value()),
            _ => None,
        }
    }

    fn persist_split_ratio(&mut self, ratio: SplitRatio) {
        match self.config.set_sidebar_split_ratio(ratio) {
            Ok(()) => self.layout = self.layout.with_ratio(ratio),
            Err(error) => {
                self.layout = self.layout.with_ratio(self.config.sidebar_split_ratio());
                self.notice = Some(format!("Could not save sidebar width: {error:#}"));
            }
        }
    }

    fn toggle_sidebar(&mut self) {
        match (self.layout, self.active_region) {
            (LayoutPreference::TerminalOnly { .. }, _) => {
                self.layout = self.layout.split_view();
                self.active_region = ActiveRegion::Sessions;
            }
            (LayoutPreference::Split { .. }, ActiveRegion::Sessions) => {
                self.layout = self.layout.terminal_only();
                self.active_region = self.focused_panel.region();
                self.menu = None;
                self.split_drag = None;
            }
            (
                LayoutPreference::Split { .. },
                ActiveRegion::PrimaryTerminal | ActiveRegion::SecondaryTerminal,
            ) => {
                self.active_region = ActiveRegion::Sessions;
            }
        }
    }

    fn normalize_focus(&mut self, regions: &UiRegions) {
        self.active_region = regions
            .navigation()
            .normalize(self.active_region)
            .unwrap_or(ActiveRegion::PrimaryTerminal);
        self.sync_focused_panel_from_region();
        if regions.sessions.is_none() {
            self.menu = None;
            self.split_drag = None;
        }
    }
}

fn session_access(session: &SessionView) -> SessionAccess {
    session.control.into()
}

pub async fn run(client: ConsoleClient, config: Config) -> Result<ConnectedSessionOutcome> {
    perf_trace::initialize()?;
    let outcome = run_loop(client, config).await;
    let flush = perf_trace::flush();
    let outcome = outcome?;
    flush?;
    Ok(outcome)
}

async fn run_loop(client: ConsoleClient, config: Config) -> Result<ConnectedSessionOutcome> {
    let mux = client.connection_mux()?;
    let mut app = App::new(client, config).await?;
    let mut session = Session::open(SessionOptions { mouse_capture: true, bracketed_paste: true })?;
    let mut events = EventReader::start();
    let mut invalidations = ConsoleInvalidations::subscribe(mux);
    let now = tokio::time::Instant::now();
    let mut reconcile = tokio::time::interval_at(now + RECONCILE_INTERVAL, RECONCILE_INTERVAL);
    reconcile.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut activity = tokio::time::interval_at(now + ACTIVITY_INTERVAL, ACTIVITY_INTERVAL);
    activity.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut activity_dirty = false;
    let mut needs_draw = true;
    let mut regions = UiRegions::default();

    loop {
        if needs_draw {
            session.draw(|frame| regions = render(frame, &mut app))?;
            perf_trace::record_redraw();
            app.normalize_focus(&regions);
            needs_draw = false;

            for (tab_id, pane_id, cols, rows) in app.resize_requests(&regions) {
                let started = perf_trace::input_timer();
                let result = app.client.resize(tab_id, pane_id, cols, rows).await;
                perf_trace::record_input_latency(InputKind::Resize, started);
                if let Err(error) = result {
                    app.notice = Some(error.to_string());
                    needs_draw = true;
                }
            }
        }

        let effect = tokio::select! {
            invalidation = invalidations.recv() => {
                let Some(invalidation) = invalidation else {
                    return Ok(ConnectedSessionOutcome::ReturnToControlCenter);
                };
                let confirmed_close = app.confirm_closed_panes(&invalidation.removed_panes);
                if invalidation.pane_output {
                    activity_dirty = true;
                }
                let changed = if invalidation.topology
                    && !confirmed_close
                    && !app.has_pending_closures()
                {
                    app.reconcile_or_notice().await
                } else if invalidation.pane_output {
                    match app.project_terminal() {
                        Ok(changed) => changed,
                        Err(error) => {
                            app.notice = Some(format!("Could not project Console terminal: {error:#}"));
                            true
                        }
                    }
                } else {
                    false
                };
                needs_draw |= changed;
                continue;
            }
            _ = reconcile.tick() => {
                if !app.has_pending_closures() {
                    needs_draw |= app.reconcile_or_notice().await;
                }
                continue;
            }
            _ = activity.tick(), if activity_dirty => {
                match app.refresh_activity() {
                    Ok(refresh) => {
                        needs_draw |= refresh.changed;
                        activity_dirty = refresh.revisit;
                        if refresh.ready
                            && app.config.ready_notification() == ReadyNotification::TerminalBell
                        {
                            session.ring_bell()?;
                        }
                    }
                    Err(error) => {
                        activity_dirty = true;
                        app.notice = Some(format!("Could not refresh Console activity: {error:#}"));
                        needs_draw = true;
                    }
                }
                continue;
            }
            event = events.recv() => {
                let Some(event) = event else {
                    return Ok(ConnectedSessionOutcome::Quit);
                };
                handle_event(event, &mut app, &regions)?
            }
        };

        // WezTerm predicts keyboard and paste input in the local render cache, so those effects
        // need an immediate draw. Mouse reporting has no local prediction; pane output is the
        // authoritative redraw signal, and drawing here would render the same terminal twice.
        let redraw_after_effect = !matches!(&effect, Effect::SendMouse { .. });
        match apply_effect(effect, &mut app, &mut session).await {
            Ok(EffectFlow::Continue) => needs_draw |= redraw_after_effect,
            Ok(EffectFlow::Quit) => {
                return Ok(ConnectedSessionOutcome::ReturnToControlCenter);
            }
            Err(error) => {
                app.notice = Some(error.to_string());
                needs_draw |= app.reconcile_or_notice().await;
            }
        }
    }
}

enum EffectFlow {
    Continue,
    Quit,
}

async fn apply_effect(effect: Effect, app: &mut App, session: &mut Session) -> Result<EffectFlow> {
    match effect {
        Effect::None => {}
        Effect::ProjectTerminal => {
            app.project_terminal()?;
        }
        Effect::ReconcileTopology => {
            app.reconcile_topology().await?;
        }
        Effect::RetryConnection => {
            app.client.retry().await?;
            app.connection = ConnectionState::Attaching;
            app.connection_detail = None;
            app.reconcile_topology().await?;
        }
        Effect::Create { cols, rows } => {
            let id = app.client.create_session(cols, rows).await?;
            app.selected = Some(id);
            app.focused_panel = PanelSlot::Primary;
            app.history.visit(id);
            app.active_region = ActiveRegion::PrimaryTerminal;
            app.scroll.reset();
            app.selection = None;
            app.reconcile_topology().await?;
        }
        Effect::Rename { id, title } => {
            app.client.rename_session(id, title).await?;
            app.reconcile_topology().await?;
        }
        Effect::Close(id) => {
            app.begin_close(id).await?;
        }
        Effect::AcquireControl(id) => {
            app.client.acquire_control(id).await?;
            app.active_region = app.focused_panel.region();
            app.reconcile_topology().await?;
        }
        Effect::ReleaseControl(id) => {
            app.client.release_control(id).await?;
            app.active_region = ActiveRegion::Sessions;
            app.reconcile_topology().await?;
        }
        Effect::TakeControl(id) => {
            app.client.take_control(id).await?;
            app.active_region = app.focused_panel.region();
            app.reconcile_topology().await?;
        }
        Effect::Copy(text) => {
            session.copy(&text)?;
            app.notice = Some(format!("Copied {} visible lines", text.lines().count()));
        }
        Effect::SendKey(key) => {
            if let Some(pane_id) = app.focused_session().map(|session| session.pane_id) {
                let started = perf_trace::input_timer();
                app.client.send_key(pane_id, key).await?;
                perf_trace::record_input_latency(InputKind::Key, started);
            }
        }
        Effect::SendMouse { pane_id, event, geometry } => {
            let started = perf_trace::input_timer();
            let _dispatched = app.client.send_mouse(pane_id, event, geometry).await?;
            perf_trace::record_input_latency(InputKind::Mouse, started);
        }
        Effect::Paste(text) => {
            if let Some(pane_id) = app.focused_session().map(|session| session.pane_id) {
                let started = perf_trace::input_timer();
                app.client.paste(pane_id, text).await?;
                perf_trace::record_input_latency(InputKind::Paste, started);
            }
        }
        Effect::Quit => return Ok(EffectFlow::Quit),
    }
    Ok(EffectFlow::Continue)
}

fn handle_event(event: Event, app: &mut App, regions: &UiRegions) -> Result<Effect> {
    app.notice = None;
    if matches!(app.surface, Surface::CommandPalette(_)) {
        let Some(layout) = regions.command_palette.as_ref() else {
            app.surface = Surface::Normal;
            return Ok(Effect::None);
        };
        let outcome = match &mut app.surface {
            Surface::CommandPalette(command_palette) => command_palette.on_event(event, layout),
            _ => unreachable!("palette surface checked above"),
        };
        return match outcome {
            CommandPaletteOutcome::Captured => Ok(Effect::None),
            CommandPaletteOutcome::Dismissed => {
                app.surface = Surface::Normal;
                Ok(Effect::None)
            }
            CommandPaletteOutcome::Invoke(invocation) => {
                app.surface = Surface::Normal;
                app.invoke(invocation, regions)
            }
        };
    }
    if matches!(app.surface, Surface::Settings(_)) {
        let flow = match (&mut app.surface, event) {
            (Surface::Settings(editor), Event::Key(key)) => editor.on_key(key),
            (Surface::Settings(editor), Event::Mouse(mouse)) => editor.on_mouse(mouse),
            _ => SettingsFlow::Continue,
        };
        if flow == SettingsFlow::Exit {
            app.surface = Surface::Normal;
            app.reload_config();
        }
        return Ok(Effect::None);
    }
    if app.menu.is_some() {
        let Some(layout) = regions.context_menu.as_ref() else {
            app.menu = None;
            return Ok(Effect::None);
        };
        let outcome = app.menu.as_mut().expect("menu checked above").on_event(event, layout);
        return match outcome {
            ContextMenuOutcome::Captured => Ok(Effect::None),
            ContextMenuOutcome::Dismissed => {
                app.menu = None;
                Ok(Effect::None)
            }
            ContextMenuOutcome::Unavailable { reason, .. } => {
                app.menu = None;
                app.notice = Some(reason.into_owned());
                Ok(Effect::None)
            }
            ContextMenuOutcome::Invoke(invocation) => {
                app.menu = None;
                app.invoke(invocation, regions)
            }
        };
    }

    match event {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            handle_key(key, app, regions)
        }
        Event::Mouse(mouse) => {
            app.keybinding_state.cancel();
            handle_mouse(mouse, app, regions)
        }
        Event::Paste(text)
            if matches!(
                app.active_region,
                ActiveRegion::PrimaryTerminal | ActiveRegion::SecondaryTerminal
            ) && app.surface.kind() == SurfaceKind::Normal
                && app.terminal_input_enabled() =>
        {
            app.keybinding_state.cancel();
            Ok(Effect::Paste(text))
        }
        Event::Resize(_, _) => {
            app.keybinding_state.cancel();
            if let Some(drag) = app.split_drag.take() {
                let ratio = app.layout.restore_ratio();
                if drag.changed(ratio) {
                    app.persist_split_ratio(ratio);
                }
            }
            Ok(Effect::None)
        }
        _ => Ok(Effect::None),
    }
}

fn handle_key(key: KeyEvent, app: &mut App, regions: &UiRegions) -> Result<Effect> {
    let Some(chord) = KeyChord::from_event(key) else {
        return Ok(Effect::None);
    };
    let context = app.action_context(None, regions, None);
    match app.registry.resolve_keybinding(&mut app.keybinding_state, chord, context) {
        KeybindingResolution::Invoke(invocation) => return app.invoke(invocation, regions),
        KeybindingResolution::Pending => return Ok(Effect::None),
        KeybindingResolution::UnmatchedSequence { prefix, chord }
            if prefix == chord
                && matches!(
                    app.active_region,
                    ActiveRegion::PrimaryTerminal | ActiveRegion::SecondaryTerminal
                )
                && app.surface.kind() == SurfaceKind::Normal
                && app.terminal_input_enabled() =>
        {
            return Ok(Effect::SendKey(key));
        }
        KeybindingResolution::UnmatchedSequence { .. } => return Ok(Effect::None),
        KeybindingResolution::Unmatched => {}
    }

    match &mut app.surface {
        Surface::Rename { input, .. } => {
            input.apply_key(key);
            Ok(Effect::None)
        }
        Surface::Search { input, current_match } => {
            input.apply_key(key);
            *current_match = None;
            app.reset_focused_terminal_state();
            Ok(Effect::None)
        }
        Surface::Normal
            if matches!(
                app.active_region,
                ActiveRegion::PrimaryTerminal | ActiveRegion::SecondaryTerminal
            ) =>
        {
            if app.terminal_input_enabled() {
                Ok(Effect::SendKey(key))
            } else {
                Ok(Effect::None)
            }
        }
        Surface::Normal | Surface::CommandPalette(_) | Surface::Settings(_) => Ok(Effect::None),
    }
}

fn handle_mouse(mouse: MouseEvent, app: &mut App, regions: &UiRegions) -> Result<Effect> {
    let position = Position { x: mouse.column, y: mouse.row };

    if let Some(region) = regions.terminal_at(position) {
        let terminal = app.panel_terminal(region.slot);
        let session = app.panel_session_id(region.slot).and_then(|id| app.session(id));
        if let (Some(terminal), Some(session)) = (terminal, session) {
            if terminal.mouse_reporting
                && session_access(session).permits_terminal_input()
                && !mouse.modifiers.contains(KeyModifiers::SHIFT)
            {
                let pane_id = session.pane_id;
                app.focus_panel(region.slot);
                return Ok(Effect::SendMouse {
                    pane_id,
                    event: mouse,
                    geometry: TerminalContentGeometry::new(
                        region.content.x,
                        region.content.y,
                        region.content.width,
                        region.content.height,
                    ),
                });
            }
        }
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.split_drag = regions.split.and_then(|split| {
                SplitDrag::begin((), split, app.layout.restore_ratio(), mouse.column, mouse.row)
            });
            if app.split_drag.is_some() {
                return Ok(Effect::None);
            }
            if let Some(hit) = action_at(regions, position) {
                if let Some(id) = hit.target {
                    let _ = app.invoke_action(ConsoleAction::SelectSession, Some(id), regions)?;
                }
                return app.invoke_action(hit.action, hit.target, regions);
            }
            if let Some(id) = session_at(regions, position) {
                let _ = app.invoke_action(ConsoleAction::FocusSessions, None, regions)?;
                let double = app.register_session_click(id, Instant::now());
                let select = app.invoke_action(ConsoleAction::SelectSession, Some(id), regions)?;
                if double {
                    let _ = app.invoke_action(ConsoleAction::RenameSession, Some(id), regions)?;
                }
                return Ok(select);
            }
            if let Some(terminal) = regions.terminal_at(position) {
                app.focus_panel(terminal.slot);
                app.begin_selection(position, terminal.content);
                return app.invoke_action(ConsoleAction::Activate, None, regions);
            }
            if let Some(region) = regions.navigation().hit_test(mouse.column, mouse.row) {
                return match region {
                    ActiveRegion::Sessions => {
                        app.invoke_action(ConsoleAction::FocusSessions, None, regions)
                    }
                    ActiveRegion::PrimaryTerminal | ActiveRegion::SecondaryTerminal => {
                        app.active_region = region;
                        app.sync_focused_panel_from_region();
                        app.invoke_action(ConsoleAction::Activate, None, regions)
                    }
                };
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            app.split_drag = None;
            if let Some(id) = session_at(regions, position) {
                let _ = app.invoke_action(ConsoleAction::FocusSessions, None, regions)?;
                let _ = app.invoke_action(ConsoleAction::SelectSession, Some(id), regions)?;
                let context = app.action_context(Some(id), regions, None);
                let items = app.registry.resolve_menu(SESSION_MENU, &context);
                app.menu = ContextMenu::open(position, context, items);
                return Ok(Effect::ProjectTerminal);
            }
            if let Some(terminal) = regions.terminal_at(position) {
                app.focus_panel(terminal.slot);
                let context = app.action_context(app.focused_session_id(), regions, None);
                let items = app.registry.resolve_menu(SESSION_MENU, &context);
                app.menu = ContextMenu::open(position, context, items);
                return Ok(Effect::None);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(ratio) = app.split_drag.and_then(|drag| {
                regions.split.and_then(|split| drag.ratio_for_column((), split, mouse.column))
            }) {
                let context = app.action_context(None, regions, Some(ratio));
                return app.invoke(ActionInvocation::new(RESIZE_SIDEBAR, context), regions);
            }
            if let Some(terminal) = regions.terminal_at(position) {
                app.focus_panel(terminal.slot);
                app.update_selection(position, terminal.content);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(drag) = app.split_drag.take() {
                let ratio = app.layout.restore_ratio();
                if drag.changed(ratio) {
                    app.persist_split_ratio(ratio);
                }
            }
        }
        MouseEventKind::ScrollUp
            if regions.sessions.is_some_and(|area| area.contains(position)) =>
        {
            return app.invoke_action(ConsoleAction::PreviousSession, None, regions);
        }
        MouseEventKind::ScrollDown
            if regions.sessions.is_some_and(|area| area.contains(position)) =>
        {
            return app.invoke_action(ConsoleAction::NextSession, None, regions);
        }
        MouseEventKind::ScrollUp if regions.terminal_at(position).is_some() => {
            app.focus_panel(regions.terminal_at(position).expect("terminal checked above").slot);
            return app.invoke_action(ConsoleAction::ScrollLineUp, None, regions);
        }
        MouseEventKind::ScrollDown if regions.terminal_at(position).is_some() => {
            app.focus_panel(regions.terminal_at(position).expect("terminal checked above").slot);
            return app.invoke_action(ConsoleAction::ScrollLineDown, None, regions);
        }
        _ => {}
    }
    Ok(Effect::None)
}

fn session_at(regions: &UiRegions, position: Position) -> Option<SessionId> {
    regions.session_rows.iter().find(|(area, _)| area.contains(position)).map(|(_, id)| *id)
}

fn action_at(regions: &UiRegions, position: Position) -> Option<ActionHitTarget> {
    regions.action_hits.iter().find(|hit| hit.area.contains(position)).copied()
}

fn console_actions(
    keybindings: &Keybindings,
) -> Result<ActionRegistry<ConsoleActionContext, ConsoleAction>> {
    let mut builder = ActionRegistryBuilder::new();
    for action in ConsoleAction::ALL {
        let enablement = match action {
            ConsoleAction::SelectSession => has_target,
            ConsoleAction::CreateSession
            | ConsoleAction::FocusSessions
            | ConsoleAction::NarrowSidebar
            | ConsoleAction::WidenSidebar
            | ConsoleAction::ResizeSidebar
            | ConsoleAction::ToggleSidebar
            | ConsoleAction::OpenCommandPalette
            | ConsoleAction::OpenSettings
            | ConsoleAction::Quit => enabled,
            ConsoleAction::RetryConnection => can_retry_connection,
            ConsoleAction::Activate => can_activate,
            ConsoleAction::Dismiss => can_dismiss,
            ConsoleAction::RenameSession | ConsoleAction::CloseSession => can_mutate,
            ConsoleAction::ReleaseControl => can_release,
            ConsoleAction::TakeControl => can_take,
            ConsoleAction::PrimaryControl => can_primary_control,
            ConsoleAction::CopyVisibleTerminal | ConsoleAction::OpenSearch => has_terminal,
            ConsoleAction::SplitPanel => can_split_panel,
            ConsoleAction::ClosePanel | ConsoleAction::FocusOtherPanel => can_close_panel,
            ConsoleAction::ScrollUp | ConsoleAction::ScrollLineUp => can_scroll_up,
            ConsoleAction::ScrollDown
            | ConsoleAction::ScrollLineDown
            | ConsoleAction::ScrollToLive => can_scroll_down,
            ConsoleAction::PreviousSession => has_previous_session,
            ConsoleAction::NextSession => has_next_session,
            ConsoleAction::HistoryBack => can_history_back,
            ConsoleAction::HistoryForward => can_history_forward,
            ConsoleAction::FocusLeft => can_focus_left,
            ConsoleAction::FocusRight => can_focus_right,
            ConsoleAction::FocusNext => can_focus_next,
            ConsoleAction::FocusPrevious => can_focus_previous,
        };
        builder.register_action(ActionSpec {
            id: action.id(),
            title: action.title(),
            command: action,
            enablement,
            command_palette: action.command_palette(),
        });
    }

    for (action, order) in [(ConsoleAction::CreateSession, 10), (ConsoleAction::ToggleSidebar, 20)]
    {
        builder.place_menu(MenuPlacement {
            menu: SESSION_MENU,
            action: action.id(),
            group: "workspace",
            group_order: 5,
            order,
            when: sidebar_hidden,
        });
    }

    for (action, group, group_order, order) in [
        (ConsoleAction::Activate, "navigation", 10, 10),
        (ConsoleAction::RenameSession, "session", 20, 10),
        // Context menus can outlive their rendered frame. Keep one semantic primary intent here
        // so the latest snapshot chooses acquire, release, or take at dispatch time rather than
        // leaving a stale `releaseControl` menu item unavailable.
        (ConsoleAction::PrimaryControl, "control", 30, 10),
        (ConsoleAction::CopyVisibleTerminal, "terminal", 40, 10),
        (ConsoleAction::OpenSearch, "terminal", 40, 20),
        (ConsoleAction::CloseSession, "destructive", 50, 10),
    ] {
        builder.place_menu(MenuPlacement {
            menu: SESSION_MENU,
            action: action.id(),
            group,
            group_order,
            order,
            when: always,
        });
    }

    for (code, modifiers, action, when) in [
        (
            KeyCode::Char('w'),
            KeyModifiers::CONTROL,
            ConsoleAction::CloseSession,
            sidebar_normal as fn(&ConsoleActionContext) -> bool,
        ),
        (KeyCode::F(2), KeyModifiers::NONE, ConsoleAction::RenameSession, sidebar_normal),
        (KeyCode::Char('r'), KeyModifiers::NONE, ConsoleAction::RenameSession, sidebar_normal),
        (KeyCode::Char('d'), KeyModifiers::NONE, ConsoleAction::ReleaseControl, sidebar_normal),
        (
            KeyCode::Char('t'),
            KeyModifiers::NONE,
            ConsoleAction::TakeControl,
            take_control_available,
        ),
        (
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ConsoleAction::CopyVisibleTerminal,
            local_tools_available,
        ),
        (
            KeyCode::Char('f'),
            KeyModifiers::CONTROL,
            ConsoleAction::OpenSearch,
            local_tools_available,
        ),
        (KeyCode::Char('/'), KeyModifiers::NONE, ConsoleAction::OpenSearch, local_tools_available),
        (KeyCode::PageUp, KeyModifiers::NONE, ConsoleAction::ScrollUp, local_tools_available),
        (KeyCode::PageDown, KeyModifiers::NONE, ConsoleAction::ScrollDown, local_tools_available),
        (KeyCode::End, KeyModifiers::NONE, ConsoleAction::ScrollToLive, local_tools_available),
        (KeyCode::Up, KeyModifiers::NONE, ConsoleAction::PreviousSession, sidebar_normal),
        (KeyCode::Down, KeyModifiers::NONE, ConsoleAction::NextSession, sidebar_normal),
        (KeyCode::Left, KeyModifiers::NONE, ConsoleAction::HistoryBack, sidebar_normal),
        (KeyCode::Right, KeyModifiers::NONE, ConsoleAction::HistoryForward, sidebar_normal),
        (KeyCode::Left, KeyModifiers::CONTROL, ConsoleAction::FocusLeft, sidebar_normal),
        (KeyCode::Right, KeyModifiers::CONTROL, ConsoleAction::FocusRight, sidebar_normal),
        (KeyCode::Tab, KeyModifiers::NONE, ConsoleAction::FocusNext, sidebar_normal),
        (KeyCode::BackTab, KeyModifiers::SHIFT, ConsoleAction::FocusPrevious, sidebar_normal),
        (
            KeyCode::Left,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ConsoleAction::NarrowSidebar,
            sidebar_normal,
        ),
        (
            KeyCode::Right,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ConsoleAction::WidenSidebar,
            sidebar_normal,
        ),
        (KeyCode::Enter, KeyModifiers::NONE, ConsoleAction::Activate, activate_available),
        (KeyCode::Esc, KeyModifiers::NONE, ConsoleAction::Dismiss, not_normal),
        (KeyCode::F(5), KeyModifiers::NONE, ConsoleAction::RetryConnection, not_terminal_normal),
        (
            KeyCode::Char('R'),
            KeyModifiers::NONE,
            ConsoleAction::RetryConnection,
            connection_retryable,
        ),
    ] {
        builder.bind_key(KeybindingPlacement {
            binding: KeyChord::new(code, modifiers).into(),
            action: action.id(),
            when,
        });
    }

    for (suffix, action) in [
        (keybindings.new_session, ConsoleAction::CreateSession),
        (keybindings.toggle_sessions, ConsoleAction::ToggleSidebar),
        (keybindings.help, ConsoleAction::OpenCommandPalette),
        (keybindings.quit, ConsoleAction::Quit),
    ] {
        builder.bind_key(KeybindingPlacement {
            binding: Keybinding::sequence(keybindings.prefix, suffix),
            action: action.id(),
            when: prefix_available,
        });
    }

    for (suffix, action) in [
        (KeyChord::new(KeyCode::Char('v'), KeyModifiers::NONE), ConsoleAction::SplitPanel),
        (KeyChord::new(KeyCode::Char('x'), KeyModifiers::NONE), ConsoleAction::ClosePanel),
        (KeyChord::new(KeyCode::Char('o'), KeyModifiers::NONE), ConsoleAction::FocusOtherPanel),
    ] {
        builder.bind_key(KeybindingPlacement {
            binding: Keybinding::sequence(keybindings.prefix, suffix),
            action: action.id(),
            when: prefix_available,
        });
    }

    builder.bind_key(KeybindingPlacement {
        binding: keybindings.command_palette.into(),
        action: ConsoleAction::OpenCommandPalette.id(),
        when: normal,
    });

    Ok(builder.build()?)
}

fn always(_: &ConsoleActionContext) -> bool {
    true
}

fn normal(context: &ConsoleActionContext) -> bool {
    context.surface == SurfaceKind::Normal
}

fn prefix_available(context: &ConsoleActionContext) -> bool {
    context.surface == SurfaceKind::Normal
}

fn not_normal(context: &ConsoleActionContext) -> bool {
    context.surface != SurfaceKind::Normal
}

fn not_terminal_normal(context: &ConsoleActionContext) -> bool {
    !terminal_normal(context)
}

fn sidebar_normal(context: &ConsoleActionContext) -> bool {
    normal(context) && context.region == ActiveRegion::Sessions
}

fn sidebar_hidden(context: &ConsoleActionContext) -> bool {
    normal(context) && !context.sidebar_visible
}

fn terminal_normal(context: &ConsoleActionContext) -> bool {
    normal(context)
        && matches!(context.region, ActiveRegion::PrimaryTerminal | ActiveRegion::SecondaryTerminal)
}

/// A read-only terminal never receives key input, so Console may retain local selection/copy
/// affordances there. A healthy controlled terminal keeps this key entirely for the shell.
fn terminal_local_tools(context: &ConsoleActionContext) -> bool {
    terminal_normal(context)
        && context.target_access.is_some_and(SessionAccess::supports_local_terminal_tools)
        && !matches!(context.target_access, Some(SessionAccess::ControlledBySelf))
}

fn local_tools_available(context: &ConsoleActionContext) -> bool {
    sidebar_normal(context) || terminal_local_tools(context)
}

fn connection_retryable(context: &ConsoleActionContext) -> bool {
    normal(context) && context.connection_retryable
}

fn activate_available(context: &ConsoleActionContext) -> bool {
    !terminal_normal(context) || matches!(context.target_access, Some(SessionAccess::Available))
}

fn take_control_available(context: &ConsoleActionContext) -> bool {
    sidebar_normal(context)
        || (terminal_normal(context)
            && matches!(
                context.target_access,
                Some(SessionAccess::Available | SessionAccess::ControlledByOther)
            ))
}

fn enabled(_: &ConsoleActionContext) -> ActionState {
    ActionState::Enabled
}

fn has_target(context: &ConsoleActionContext) -> ActionState {
    if context.target.is_some() {
        ActionState::Enabled
    } else {
        ActionState::disabled("no session is selected")
    }
}

fn has_terminal(context: &ConsoleActionContext) -> ActionState {
    if context.has_terminal {
        ActionState::Enabled
    } else {
        ActionState::disabled("the selected session has no terminal projection")
    }
}

fn can_split_panel(context: &ConsoleActionContext) -> ActionState {
    if context.can_split_panel {
        ActionState::Enabled
    } else if context.has_secondary_panel {
        ActionState::disabled("Console already has two terminal panels")
    } else {
        ActionState::disabled("a second session is required")
    }
}

fn can_close_panel(context: &ConsoleActionContext) -> ActionState {
    if context.has_secondary_panel {
        ActionState::Enabled
    } else {
        ActionState::disabled("the single terminal panel cannot be closed")
    }
}

fn can_activate(context: &ConsoleActionContext) -> ActionState {
    if context.surface != SurfaceKind::Normal || context.selected.is_some() {
        ActionState::Enabled
    } else {
        ActionState::disabled("no session is selected")
    }
}

fn can_dismiss(context: &ConsoleActionContext) -> ActionState {
    if context.surface != SurfaceKind::Normal {
        ActionState::Enabled
    } else {
        ActionState::disabled("nothing is open")
    }
}

fn can_mutate(context: &ConsoleActionContext) -> ActionState {
    match context.target_access {
        Some(SessionAccess::ControlledBySelf) => ActionState::Enabled,
        Some(SessionAccess::Synchronizing) => {
            ActionState::disabled("control state is synchronizing")
        }
        Some(SessionAccess::ControlledByOther | SessionAccess::Available) => {
            ActionState::disabled("take control before changing this session")
        }
        None => ActionState::disabled("no session is selected"),
    }
}

fn can_release(context: &ConsoleActionContext) -> ActionState {
    match context.target_access {
        Some(SessionAccess::ControlledBySelf) => ActionState::Enabled,
        Some(SessionAccess::Synchronizing) => {
            ActionState::disabled("control state is synchronizing")
        }
        Some(SessionAccess::ControlledByOther | SessionAccess::Available) => {
            ActionState::disabled("this client does not control the session")
        }
        None => ActionState::disabled("no session is selected"),
    }
}

fn can_take(context: &ConsoleActionContext) -> ActionState {
    match context.target_access {
        Some(SessionAccess::ControlledByOther | SessionAccess::Available) => ActionState::Enabled,
        Some(SessionAccess::ControlledBySelf) => ActionState::disabled("Already controlling"),
        Some(SessionAccess::Synchronizing) => {
            ActionState::disabled("control state is synchronizing")
        }
        None => ActionState::disabled("no session is selected"),
    }
}

fn can_primary_control(context: &ConsoleActionContext) -> ActionState {
    match context.target_access {
        Some(
            SessionAccess::Available
            | SessionAccess::ControlledBySelf
            | SessionAccess::ControlledByOther,
        ) => ActionState::Enabled,
        Some(SessionAccess::Synchronizing) => {
            ActionState::disabled("control state is synchronizing")
        }
        None => ActionState::disabled("no session is selected"),
    }
}

fn can_retry_connection(context: &ConsoleActionContext) -> ActionState {
    if context.connection_retryable {
        ActionState::Enabled
    } else {
        ActionState::disabled("the Console connection is not waiting for a manual retry")
    }
}

fn can_scroll_up(context: &ConsoleActionContext) -> ActionState {
    if context.can_scroll_up {
        ActionState::Enabled
    } else {
        ActionState::disabled("already at the oldest projected line")
    }
}

fn can_scroll_down(context: &ConsoleActionContext) -> ActionState {
    if context.can_scroll_down {
        ActionState::Enabled
    } else {
        ActionState::disabled("already at the live viewport")
    }
}

fn has_previous_session(context: &ConsoleActionContext) -> ActionState {
    if context.selected_index.is_some_and(|index| index > 0) {
        ActionState::Enabled
    } else {
        ActionState::disabled("already at the first session")
    }
}

fn has_next_session(context: &ConsoleActionContext) -> ActionState {
    if context.selected_index.is_some_and(|index| index.saturating_add(1) < context.session_count) {
        ActionState::Enabled
    } else {
        ActionState::disabled("already at the last session")
    }
}

fn can_history_back(context: &ConsoleActionContext) -> ActionState {
    if context.can_history_back {
        ActionState::Enabled
    } else {
        ActionState::disabled("no earlier session in history")
    }
}

fn can_history_forward(context: &ConsoleActionContext) -> ActionState {
    if context.can_history_forward {
        ActionState::Enabled
    } else {
        ActionState::disabled("no later session in history")
    }
}

fn can_focus_left(context: &ConsoleActionContext) -> ActionState {
    focus_available(context.focus_left)
}

fn can_focus_right(context: &ConsoleActionContext) -> ActionState {
    focus_available(context.focus_right)
}

fn can_focus_next(context: &ConsoleActionContext) -> ActionState {
    focus_available(context.focus_next)
}

fn can_focus_previous(context: &ConsoleActionContext) -> ActionState {
    focus_available(context.focus_previous)
}

fn focus_available(target: Option<ActiveRegion>) -> ActionState {
    if target.is_some() {
        ActionState::Enabled
    } else {
        ActionState::disabled("there is no region in that direction")
    }
}

fn render(frame: &mut Frame<'_>, app: &mut App) -> UiRegions {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::default().bg(NORD.background)), area);
    let mut regions = UiRegions::default();
    if let Surface::Settings(editor) = &mut app.surface {
        editor.render(frame, area);
        return regions;
    }

    match app.layout.effective(area.width) {
        EffectiveLayout::Split => {
            let split = SplitFrame::horizontal(
                area,
                app.layout.restore_ratio(),
                SplitMinimums::new(18, 20),
            );
            regions.split = Some(split);
            regions.sessions = Some(split.first);
            render_sessions(frame, split.first, app, &mut regions);
            render_terminals(frame, split.second, app, &mut regions);
            render_split_divider(
                frame,
                split,
                app.split_drag.is_some(),
                SplitDividerStyle {
                    idle_color: NORD.border,
                    active_color: NORD.focus,
                    idle_line: "│",
                    idle_grip: "┃",
                    active_line: "┃",
                },
            );
        }
        EffectiveLayout::TerminalOnly { reason: TerminalOnlyReason::Compact }
            if app.active_region == ActiveRegion::Sessions =>
        {
            regions.sessions = Some(area);
            render_sessions(frame, area, app, &mut regions);
        }
        EffectiveLayout::TerminalOnly { .. } => {
            render_terminals(frame, area, app, &mut regions);
        }
    }
    if let Some(menu) = app.menu.as_ref() {
        let layout = menu.layout(area);
        menu.render(frame, &layout, ContextMenuStyle::from_theme(NORD));
        regions.context_menu = Some(layout);
    }
    if let Surface::CommandPalette(command_palette) = &app.surface {
        let layout = command_palette.layout(area);
        command_palette.render(frame, &layout, NORD);
        regions.command_palette = Some(layout);
    }
    regions
}

fn render_sessions(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let focused = app.active_region == ActiveRegion::Sessions;
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { NORD.focus } else { NORD.border }))
        .title(Span::styled(
            format!(" Sessions {} ", app.snapshot.sessions.len()),
            Style::default()
                .fg(if focused { NORD.text_strong } else { NORD.text_muted })
                .add_modifier(if focused { Modifier::BOLD } else { Modifier::empty() }),
        ));
    if focused {
        if let Some(notice) = panel_notice(app) {
            block = block.title_bottom(Span::styled(
                format!(" {notice} "),
                Style::default().fg(NORD.warning),
            ));
        }
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);

    let item_width = usize::from(chunks[0].width.saturating_sub(2));
    let items = app.snapshot.sessions.iter().map(|session| {
        let panel = if app.selected == Some(session.id) {
            Some("1")
        } else if app.secondary.as_ref().is_some_and(|panel| panel.session_id == session.id) {
            Some("2")
        } else {
            None
        };
        session_item(session, &app.surface, item_width, panel)
    });
    let mut state = ListState::default();
    state.select(app.selected_index());
    frame.render_stateful_widget(
        List::new(items.collect::<Vec<_>>()).highlight_symbol("▸ ").highlight_style(
            Style::default().fg(NORD.text_strong).bg(NORD.selection).add_modifier(Modifier::BOLD),
        ),
        chunks[0],
        &mut state,
    );
    let offset = state.offset();
    regions.session_rows.clear();
    let mut row = chunks[0].y;
    let bottom = chunks[0].y.saturating_add(chunks[0].height);
    for session in app.snapshot.sessions.iter().skip(offset) {
        let height = session_item_height(session);
        if row.saturating_add(height) > bottom {
            break;
        }
        regions
            .session_rows
            .push((Rect::new(chunks[0].x, row, chunks[0].width, height), session.id));
        row = row.saturating_add(height);
    }
    for (row, id) in &regions.session_rows {
        let Some(session) = app.session(*id) else {
            continue;
        };
        let Some(badge) = control_badge(session.control) else {
            continue;
        };
        let control_width = row.width.min(badge.len() as u16);
        let control_area = Rect::new(
            row.x.saturating_add(row.width.saturating_sub(control_width)),
            row.y,
            control_width,
            1,
        );
        if session_access(session).primary_control().is_some() {
            regions.action_hits.push(ActionHitTarget {
                area: control_area,
                action: ConsoleAction::PrimaryControl,
                target: Some(*id),
            });
        }
    }

    let controls =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(chunks[1]);
    let control_style = if focused {
        Style::default().fg(NORD.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(NORD.text_muted)
    };
    frame.render_widget(Paragraph::new(" ＋ New session").style(control_style), controls[0]);
    frame.render_widget(
        Paragraph::new(" ⇤ Hide sessions").style(Style::default().fg(NORD.text_muted)),
        controls[1],
    );
    regions.action_hits.extend([
        ActionHitTarget { area: controls[0], action: ConsoleAction::CreateSession, target: None },
        ActionHitTarget { area: controls[1], action: ConsoleAction::ToggleSidebar, target: None },
    ]);
}

fn session_item(
    session: &SessionView,
    surface: &Surface,
    width: usize,
    panel: Option<&str>,
) -> ListItem<'static> {
    let mut title = match surface {
        Surface::Rename { id, input } if *id == session.id => format!("✎ {}▏", input.value()),
        _ => session.title.clone(),
    };
    if let Some(panel) = panel {
        title = format!("[{panel}] {title}");
    }
    let badge = control_badge(session.control);
    let status_width = badge.map_or(0, |badge| badge.len().min(width));
    let gap_width = usize::from(status_width > 0);
    let title_width = width.saturating_sub(status_width.saturating_add(gap_width));
    title.truncate(title.char_indices().nth(title_width).map_or(title.len(), |(index, _)| index));
    let title_style = session
        .agent
        .filter(|agent| {
            matches!(agent.activity, AgentActivity::NeedsAttention)
                || matches!(agent.activity, AgentActivity::Idle) && !agent.seen
        })
        .map_or_else(
            || Style::default().fg(NORD.text),
            |agent| Style::default().fg(agent_color(agent)).add_modifier(Modifier::BOLD),
        );
    let mut primary = vec![Span::styled(format!("{title:<title_width$}"), title_style)];
    if let Some(badge) = badge {
        primary.push(Span::raw(" "));
        primary.push(Span::styled(
            format!("{badge:>status_width$}"),
            Style::default().fg(control_color(session.control)),
        ));
    }
    let primary = Line::from(primary);
    match session.agent {
        Some(agent) => {
            let color = agent_color(agent);
            ListItem::new(vec![
                primary,
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(agent_icon(agent), Style::default().fg(color)),
                    Span::raw(" "),
                    Span::styled(agent.kind.label(), Style::default().fg(NORD.text_muted)),
                ]),
            ])
        }
        None => ListItem::new(primary),
    }
}

fn session_item_height(session: &SessionView) -> u16 {
    if session.agent.is_some() {
        2
    } else {
        1
    }
}

fn agent_icon(agent: AgentPresentation) -> &'static str {
    match (agent.activity, agent.seen) {
        (AgentActivity::NeedsAttention, _) => "◉",
        (AgentActivity::Working, _) => "●",
        (AgentActivity::Idle, false) => "●",
        (AgentActivity::Idle, true) => "✓",
        (AgentActivity::Unknown, _) => "○",
    }
}

fn agent_color(agent: AgentPresentation) -> Color {
    match (agent.activity, agent.seen) {
        (AgentActivity::NeedsAttention, _) => NORD.danger,
        (AgentActivity::Working, _) => NORD.warning,
        (AgentActivity::Idle, false) => NORD.accent_alt,
        (AgentActivity::Idle, true) => NORD.success,
        (AgentActivity::Unknown, _) => NORD.text_muted,
    }
}

fn control_badge(control: SessionControl) -> Option<&'static str> {
    match SessionAccess::from(control) {
        SessionAccess::Synchronizing => Some("syncing"),
        SessionAccess::Available => None,
        SessionAccess::ControlledBySelf => Some("you"),
        SessionAccess::ControlledByOther => Some("read-only"),
    }
}

fn control_color(control: SessionControl) -> Color {
    match SessionAccess::from(control) {
        SessionAccess::ControlledBySelf => NORD.success,
        SessionAccess::ControlledByOther => NORD.accent,
        SessionAccess::Synchronizing => NORD.warning,
        SessionAccess::Available => NORD.text_muted,
    }
}

fn render_terminals(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let slots = visible_panel_slots(app.secondary.is_some(), area.width, app.focused_panel);
    if slots.len() == 2 {
        let panels = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        render_terminal(frame, panels[0], app, regions, slots[0], false);
        render_terminal(frame, panels[1], app, regions, slots[1], false);
    } else {
        render_terminal(frame, area, app, regions, slots[0], app.secondary.is_some());
    }
}

fn render_terminal(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    regions: &mut UiRegions,
    slot: PanelSlot,
    narrow_fallback: bool,
) {
    let focused = app.active_region == slot.region();
    let session_title = app
        .panel_session_id(slot)
        .and_then(|id| app.session(id))
        .map(|session| session.title.as_str())
        .unwrap_or("Terminal");
    let panel_number = match slot {
        PanelSlot::Primary => 1,
        PanelSlot::Secondary => 2,
    };
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { NORD.focus } else { NORD.border }))
        .title(Span::styled(
            format!(" {panel_number}: {session_title} "),
            Style::default().fg(if focused { NORD.text_strong } else { NORD.text_muted }),
        ));
    if narrow_fallback {
        block = block.title_bottom(Span::styled(
            " Narrow view: one panel shown; use focus actions to switch ",
            Style::default().fg(NORD.text_muted),
        ));
    }
    if focused {
        if let Some(notice) = panel_notice(app) {
            block = block.title_bottom(Span::styled(
                format!(" {notice} "),
                Style::default().fg(NORD.warning),
            ));
        }
    }
    let content = block.inner(area);
    frame.render_widget(block, area);
    regions.terminals.push(TerminalRegion { slot, area, content });

    if let Some(terminal) = app.panel_terminal(slot) {
        let scroll = app.panel_scroll(slot);
        let range = scroll.visible_range(ScrollMetrics::new(
            terminal.first_row,
            terminal.lines.len(),
            usize::from(content.height),
        ));
        frame.render_widget(
            TerminalCells {
                terminal,
                start: range.start,
                query: app.search_query(),
                selection: app.panel_selection(slot),
            },
            content,
        );
        if focused
            && scroll.is_live()
            && terminal.cursor_x < usize::from(content.width)
            && terminal.cursor_y < usize::from(content.height)
        {
            frame.set_cursor_position((
                content.x.saturating_add(terminal.cursor_x as u16),
                content.y.saturating_add(terminal.cursor_y as u16),
            ));
        }
    } else {
        frame.render_widget(
            Paragraph::new(if app.snapshot.sessions.is_empty() {
                format!(
                    "No sessions yet\n\nPress {} {} or click New session",
                    app.config.keybindings().prefix,
                    app.config.keybindings().new_session
                )
            } else if slot == PanelSlot::Secondary {
                "Choose a session for this panel from the sessions list…".to_owned()
            } else {
                "Waiting for authoritative terminal projection…".to_owned()
            })
            .alignment(Alignment::Center)
            .style(Style::default().fg(NORD.text_muted)),
            content.inner(Margin { horizontal: 1, vertical: 1 }),
        );
    }
}

fn panel_notice(app: &App) -> Option<String> {
    let connection_notice = match app.connection {
        ConnectionState::Attaching => Some("Connecting to Console…".to_owned()),
        ConnectionState::Reconnecting { attempt } => {
            Some(format!("Reconnecting to Console… attempt {attempt}"))
        }
        ConnectionState::Failed => Some("Console connection failed — press F5 to retry".to_owned()),
        ConnectionState::RetryExhausted => {
            Some("Console reconnect limit reached — press F5 to retry".to_owned())
        }
        ConnectionState::Detached => Some("Console is detached — press F5 to retry".to_owned()),
        ConnectionState::Ready => None,
    };
    match &app.surface {
        Surface::Rename { input, .. } => {
            Some(format!("Rename: {}▏   Enter save   Esc cancel", input.value()))
        }
        Surface::Search { input, .. } => Some(format!(
            "Search: {}▏   Enter next   Esc close   PageUp/PageDown scroll",
            input.value()
        )),
        Surface::Normal | Surface::CommandPalette(_) | Surface::Settings(_) => {
            app.notice.clone().or_else(|| app.connection_detail.clone()).or(connection_notice)
        }
    }
}

struct TerminalCells<'a> {
    terminal: &'a TerminalView,
    start: usize,
    query: Option<&'a str>,
    selection: Option<TerminalSelection>,
}

impl Widget for TerminalCells<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        for (row, line) in
            self.terminal.lines.iter().skip(self.start).take(usize::from(area.height)).enumerate()
        {
            let line_index = self.start.saturating_add(row);
            let stable_row = self
                .terminal
                .first_row
                .saturating_add(StableRowIndex::try_from(line_index).unwrap_or_default());
            let matched = self.query.is_some_and(|query| line.as_str().contains(query));
            for cell in line.visible_cells() {
                let column = cell.cell_index();
                let width = cell.width().max(1);
                if column >= usize::from(area.width)
                    || column.saturating_add(width) > usize::from(area.width)
                {
                    continue;
                }
                let x = area.x.saturating_add(column as u16);
                let y = area.y.saturating_add(row as u16);
                let mut style = cell_style(cell.attrs());
                if self.selection.is_some_and(|selection| selection.contains(stable_row, column)) {
                    style = style.bg(NORD.focus).add_modifier(Modifier::REVERSED);
                } else if matched {
                    style = style.bg(NORD.selection);
                }
                buffer[(x, y)].set_symbol(cell.str()).set_style(style);
                for offset in 1..width {
                    buffer[(x.saturating_add(offset as u16), y)].set_symbol(" ").set_style(style);
                }
            }
        }
    }
}

fn terminal_point(
    terminal: Option<&TerminalView>,
    content: Rect,
    scroll: ScrollState,
    position: Position,
) -> Option<(StableRowIndex, usize)> {
    let terminal = terminal?;
    if !content.contains(position) {
        return None;
    }
    let range = scroll.visible_range(ScrollMetrics::new(
        terminal.first_row,
        terminal.lines.len(),
        usize::from(content.height),
    ));
    let line = range.start.checked_add(usize::from(position.y - content.y))?;
    (line < range.end).then(|| {
        (
            terminal.first_row.saturating_add(StableRowIndex::try_from(line).unwrap_or_default()),
            usize::from(position.x - content.x),
        )
    })
}

fn validated_rename(input: &LineEditor) -> Option<String> {
    let title = input.value().trim();
    (!title.is_empty()).then(|| title.to_owned())
}

fn cell_style(attributes: &CellAttributes) -> Style {
    let mut modifiers = Modifier::empty();
    match attributes.intensity() {
        Intensity::Bold => modifiers |= Modifier::BOLD,
        Intensity::Half => modifiers |= Modifier::DIM,
        Intensity::Normal => {}
    }
    if attributes.italic() {
        modifiers |= Modifier::ITALIC;
    }
    if attributes.underline() != Underline::None {
        modifiers |= Modifier::UNDERLINED;
    }
    match attributes.blink() {
        Blink::Slow => modifiers |= Modifier::SLOW_BLINK,
        Blink::Rapid => modifiers |= Modifier::RAPID_BLINK,
        Blink::None => {}
    }
    if attributes.reverse() {
        modifiers |= Modifier::REVERSED;
    }
    if attributes.strikethrough() {
        modifiers |= Modifier::CROSSED_OUT;
    }
    if attributes.invisible() {
        modifiers |= Modifier::HIDDEN;
    }
    Style::default()
        .fg(cell_color(attributes.foreground()))
        .bg(cell_color(attributes.background()))
        .add_modifier(modifiers)
}

fn cell_color(color: ColorAttribute) -> Color {
    match color {
        ColorAttribute::Default => Color::Reset,
        ColorAttribute::PaletteIndex(index) => Color::Indexed(index),
        ColorAttribute::TrueColorWithPaletteFallback(color, _)
        | ColorAttribute::TrueColorWithDefaultFallback(color) => {
            let (red, green, blue, _) = color.to_srgb_u8();
            Color::Rgb(red, green, blue)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::console::activity::AgentKind;
    use wezterm_term::Line as TerminalLine;

    fn context(control: SessionControl) -> ConsoleActionContext {
        ConsoleActionContext {
            target: Some(7),
            target_access: Some(control.into()),
            selected: Some(7),
            selected_index: Some(1),
            session_count: 3,
            sidebar_visible: true,
            region: ActiveRegion::Sessions,
            focus_left: None,
            focus_right: Some(ActiveRegion::PrimaryTerminal),
            focus_next: Some(ActiveRegion::PrimaryTerminal),
            focus_previous: Some(ActiveRegion::PrimaryTerminal),
            surface: SurfaceKind::Normal,
            has_terminal: true,
            has_secondary_panel: false,
            can_split_panel: true,
            visible_rows: 5,
            can_scroll_up: true,
            can_scroll_down: true,
            can_history_back: true,
            can_history_forward: true,
            create_cols: 80,
            create_rows: 24,
            requested_ratio: None,
            connection_retryable: false,
        }
    }

    fn registry() -> ActionRegistry<ConsoleActionContext, ConsoleAction> {
        console_actions(&Keybindings::default()).unwrap()
    }

    fn resolve(
        registry: &ActionRegistry<ConsoleActionContext, ConsoleAction>,
        chord: KeyChord,
        context: ConsoleActionContext,
    ) -> KeybindingResolution<ConsoleActionContext> {
        registry.resolve_keybinding(&mut KeybindingState::default(), chord, context)
    }

    fn invocation(
        registry: &ActionRegistry<ConsoleActionContext, ConsoleAction>,
        chord: KeyChord,
        context: ConsoleActionContext,
    ) -> ActionInvocation<ConsoleActionContext> {
        let KeybindingResolution::Invoke(invocation) = resolve(registry, chord, context) else {
            panic!("{chord} did not invoke a Console action");
        };
        invocation
    }

    #[test]
    fn catalog_projects_palette_actions_and_shared_enablement() {
        let registry = registry();
        let controller = context(SessionControl::Controller);
        let observer = context(SessionControl::Observer);
        let command_palette = registry.resolve_command_palette(&controller);
        let visible = ConsoleAction::ALL
            .iter()
            .filter(|action| {
                matches!(action.command_palette(), CommandPalettePlacement::Visible { .. })
            })
            .count();

        assert_eq!(command_palette.len(), visible);
        assert!(ConsoleAction::ALL
            .iter()
            .filter(|action| {
                matches!(action.command_palette(), CommandPalettePlacement::Visible { .. })
            })
            .all(|action| { command_palette.items().iter().any(|item| item.id == action.id()) }));

        for action in [
            ConsoleAction::RenameSession,
            ConsoleAction::CloseSession,
            ConsoleAction::ReleaseControl,
        ] {
            assert_eq!(
                registry.command_for(&ActionInvocation::new(action.id(), controller)),
                Ok(action)
            );
        }
        assert!(registry.command_for(&ActionInvocation::new(RENAME_SESSION, observer)).is_err());
        assert_eq!(
            registry.command_for(&ActionInvocation::new(TAKE_CONTROL, observer)),
            Ok(ConsoleAction::TakeControl)
        );
    }

    #[test]
    fn keyboard_and_menu_resolve_through_the_same_catalog() {
        let registry = registry();
        let context = context(SessionControl::Controller);
        let rename_key =
            invocation(&registry, KeyChord::new(KeyCode::F(2), KeyModifiers::NONE), context);
        let rename_menu = registry
            .resolve_menu(SESSION_MENU, &context)
            .items()
            .iter()
            .find(|action| action.id == RENAME_SESSION)
            .unwrap()
            .id;

        assert_eq!(rename_key.action, rename_menu);
        assert_eq!(registry.command_for(&rename_key), Ok(ConsoleAction::RenameSession));
    }

    #[test]
    fn configured_prefix_sequence_resolves_through_the_shared_catalog() {
        let keybindings = Keybindings {
            prefix: KeyChord::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            new_session: KeyChord::new(KeyCode::Char('c'), KeyModifiers::NONE),
            ..Keybindings::default()
        };
        let registry = console_actions(&keybindings).unwrap();
        let context = context(SessionControl::Controller);
        let mut state = KeybindingState::default();

        assert!(matches!(
            registry.resolve_keybinding(&mut state, keybindings.prefix, context),
            KeybindingResolution::Pending
        ));
        let KeybindingResolution::Invoke(invocation) =
            registry.resolve_keybinding(&mut state, keybindings.new_session, context)
        else {
            panic!("configured Console sequence did not resolve");
        };
        assert_eq!(registry.command_for(&invocation), Ok(ConsoleAction::CreateSession));
    }

    #[test]
    fn direct_and_prefixed_palette_bindings_share_one_action() {
        let keybindings = Keybindings::default();
        let registry = console_actions(&keybindings).unwrap();
        let context = context(SessionControl::Controller);

        let direct = invocation(&registry, keybindings.command_palette, context);
        assert_eq!(direct.action, OPEN_COMMAND_PALETTE);

        let mut state = KeybindingState::default();
        assert!(matches!(
            registry.resolve_keybinding(&mut state, keybindings.prefix, context),
            KeybindingResolution::Pending
        ));
        let KeybindingResolution::Invoke(prefixed) =
            registry.resolve_keybinding(&mut state, keybindings.help, context)
        else {
            panic!("configured Console palette sequence did not resolve");
        };
        assert_eq!(prefixed.action, direct.action);
        assert_eq!(registry.command_for(&direct), Ok(ConsoleAction::OpenCommandPalette));
    }

    #[test]
    fn session_menu_uses_one_rehydrated_control_intent() {
        let registry = registry();
        let menu = registry.resolve_menu(SESSION_MENU, &context(SessionControl::Controller));

        assert!(menu.items().iter().any(|item| item.id == PRIMARY_CONTROL));
        assert!(!menu.items().iter().any(|item| item.id == RELEASE_CONTROL));
        assert!(!menu.items().iter().any(|item| item.id == TAKE_CONTROL));

        let mut hidden = context(SessionControl::Controller);
        hidden.region = ActiveRegion::PrimaryTerminal;
        hidden.sidebar_visible = false;
        let hidden_menu = registry.resolve_menu(SESSION_MENU, &hidden);
        assert!(hidden_menu.items().iter().any(|item| item.id == CREATE_SESSION));
        assert!(hidden_menu.items().iter().any(|item| item.id == TOGGLE_SIDEBAR));
    }

    #[test]
    fn terminal_focus_leaves_the_configured_prefix_to_the_console_shell() {
        let registry = registry();
        let mut terminal = context(SessionControl::Controller);
        terminal.region = ActiveRegion::PrimaryTerminal;

        for chord in [
            KeyChord::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyChord::new(KeyCode::Tab, KeyModifiers::NONE),
            KeyChord::new(KeyCode::PageUp, KeyModifiers::NONE),
            KeyChord::new(KeyCode::Left, KeyModifiers::CONTROL),
            KeyChord::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            KeyChord::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            KeyChord::new(KeyCode::Char('t'), KeyModifiers::NONE),
            KeyChord::new(KeyCode::Char('?'), KeyModifiers::NONE),
            KeyChord::new(KeyCode::F(1), KeyModifiers::NONE),
        ] {
            assert!(
                matches!(resolve(&registry, chord, terminal), KeybindingResolution::Unmatched),
                "terminal key {chord} was captured by the Console shell"
            );
        }

        assert!(matches!(
            resolve(&registry, KeyChord::new(KeyCode::Char('b'), KeyModifiers::CONTROL), terminal,),
            KeybindingResolution::Pending
        ));
    }

    #[test]
    fn read_only_terminal_keeps_local_copy_without_capturing_healthy_terminal_keys() {
        let registry = registry();
        let mut observer = context(SessionControl::Observer);
        observer.region = ActiveRegion::PrimaryTerminal;

        let copy = invocation(
            &registry,
            KeyChord::new(KeyCode::Char('C'), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            observer,
        );
        assert_eq!(registry.command_for(&copy), Ok(ConsoleAction::CopyVisibleTerminal));
        let scroll =
            invocation(&registry, KeyChord::new(KeyCode::PageUp, KeyModifiers::NONE), observer);
        assert_eq!(registry.command_for(&scroll), Ok(ConsoleAction::ScrollUp));
        let take =
            invocation(&registry, KeyChord::new(KeyCode::Char('t'), KeyModifiers::NONE), observer);
        assert_eq!(registry.command_for(&take), Ok(ConsoleAction::TakeControl));
        assert!(matches!(
            resolve(&registry, KeyChord::new(KeyCode::Char('?'), KeyModifiers::NONE), observer),
            KeybindingResolution::Unmatched
        ));

        let mut available = context(SessionControl::Uncontrolled);
        available.region = ActiveRegion::PrimaryTerminal;
        let activate =
            invocation(&registry, KeyChord::new(KeyCode::Enter, KeyModifiers::NONE), available);
        assert_eq!(registry.command_for(&activate), Ok(ConsoleAction::Activate));

        let mut controller = context(SessionControl::Controller);
        controller.region = ActiveRegion::PrimaryTerminal;
        assert!(matches!(
            resolve(
                &registry,
                KeyChord::new(KeyCode::Char('C'), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                controller,
            ),
            KeybindingResolution::Unmatched
        ));
        assert!(matches!(
            resolve(&registry, KeyChord::new(KeyCode::PageUp, KeyModifiers::NONE), controller,),
            KeybindingResolution::Unmatched
        ));
    }

    #[test]
    fn sidebar_keeps_panel_navigation_keys() {
        let registry = registry();
        let sidebar = context(SessionControl::Controller);

        for (chord, expected) in [
            (KeyChord::new(KeyCode::Enter, KeyModifiers::NONE), ConsoleAction::Activate),
            (KeyChord::new(KeyCode::Tab, KeyModifiers::NONE), ConsoleAction::FocusNext),
        ] {
            let invocation = invocation(&registry, chord, sidebar);
            assert_eq!(registry.command_for(&invocation), Ok(expected));
        }
    }

    #[test]
    fn panel_commands_use_the_console_prefix() {
        let registry = registry();
        for (suffix, expected) in [
            ('v', ConsoleAction::SplitPanel),
            ('x', ConsoleAction::ClosePanel),
            ('o', ConsoleAction::FocusOtherPanel),
        ] {
            let mut context = context(SessionControl::Controller);
            if expected != ConsoleAction::SplitPanel {
                context.has_secondary_panel = true;
                context.can_split_panel = false;
            }
            let mut state = KeybindingState::default();
            assert!(matches!(
                registry.resolve_keybinding(
                    &mut state,
                    KeyChord::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
                    context,
                ),
                KeybindingResolution::Pending
            ));
            let invocation = match registry.resolve_keybinding(
                &mut state,
                KeyChord::new(KeyCode::Char(suffix), KeyModifiers::NONE),
                context,
            ) {
                KeybindingResolution::Invoke(invocation) => invocation,
                _ => panic!("panel shortcut did not invoke"),
            };
            assert_eq!(registry.command_for(&invocation), Ok(expected));
        }
    }

    #[test]
    fn navigation_history_and_geometry_preserve_left_right_behavior() {
        let map = NavigationMap::new([
            NavigationRegion::new(ActiveRegion::Sessions, Rect::new(0, 0, 20, 10)),
            NavigationRegion::new(ActiveRegion::PrimaryTerminal, Rect::new(21, 0, 40, 10)),
        ]);
        assert_eq!(
            map.neighbor(ActiveRegion::Sessions, Direction::Right),
            Some(ActiveRegion::PrimaryTerminal)
        );
        assert_eq!(
            map.neighbor(ActiveRegion::PrimaryTerminal, Direction::Left),
            Some(ActiveRegion::Sessions)
        );

        let mut history = NavigationHistory::default();
        history.visit(1);
        history.visit(2);
        history.visit(3);
        assert_eq!(history.target(-1).map(|(_, id)| *id), Some(2));
        let (cursor, _) = history.target(-1).unwrap();
        history.select(cursor);
        assert_eq!(history.target(1).map(|(_, id)| *id), Some(3));
    }

    #[test]
    fn rename_trims_input_and_rejects_empty_names() {
        let mut input = LineEditor::default();
        input.set("  deploy logs  ".to_owned());
        assert_eq!(validated_rename(&input), Some("deploy logs".to_owned()));
        input.set("   ".to_owned());
        assert_eq!(validated_rename(&input), None);
    }

    #[test]
    fn mouse_hit_routing_prefers_controls_and_preserves_rows() {
        let mut regions = UiRegions::default();
        regions.session_rows.push((Rect::new(0, 0, 20, 1), 7));
        regions.action_hits.push(ActionHitTarget {
            area: Rect::new(10, 0, 10, 1),
            action: ConsoleAction::PrimaryControl,
            target: Some(7),
        });

        let position = Position { x: 15, y: 0 };
        assert_eq!(action_at(&regions, position).unwrap().action, ConsoleAction::PrimaryControl);
        assert_eq!(session_at(&regions, position), Some(7));
    }

    #[test]
    fn split_drag_uses_shared_clamped_geometry() {
        let area = Rect::new(0, 0, 100, 20);
        let ratio = SplitRatio::new(260);
        let split = SplitFrame::horizontal(area, ratio, SplitMinimums::new(18, 20));
        let drag = SplitDrag::begin((), split, ratio, split.separator.x, 5).unwrap();
        let wider = drag.ratio_for_column((), split, 60).unwrap();

        assert!(wider.value() > ratio.value());
        assert!(drag.changed(wider));
    }

    #[test]
    fn wide_and_compact_terminal_projection_use_authoritative_lines() {
        let attrs = CellAttributes::default();
        let terminal = TerminalView {
            pane_id: 9,
            title: "shell".to_owned(),
            first_row: 0,
            cols: 12,
            rows: 2,
            cursor_x: 0,
            cursor_y: 0,
            lines: vec![
                TerminalLine::from_text("hello world", &attrs, 0, None),
                TerminalLine::from_text("second", &attrs, 0, None),
            ],
            mouse_reporting: false,
            content_sequence: 0,
        };
        let mut wide = Buffer::empty(Rect::new(0, 0, 12, 2));
        TerminalCells { terminal: &terminal, start: 0, query: Some("hello"), selection: None }
            .render(wide.area, &mut wide);
        let mut compact = Buffer::empty(Rect::new(0, 0, 5, 1));
        TerminalCells { terminal: &terminal, start: 0, query: None, selection: None }
            .render(compact.area, &mut compact);

        assert_eq!(wide[(0, 0)].symbol(), "h");
        assert_eq!(wide[(0, 0)].bg, NORD.selection);
        assert_eq!(compact[(4, 0)].symbol(), "o");
    }

    #[test]
    fn terminal_projection_uses_the_scroll_state_range() {
        let metrics = ScrollMetrics::new(100, 20, 5);
        let mut scroll = ScrollState::default();
        assert_eq!(scroll.visible_range(metrics), 15..20);
        scroll.scroll_up(3, metrics);
        assert_eq!(scroll.visible_range(metrics), 12..17);
    }

    #[test]
    fn agent_attention_projection_keeps_blocked_and_unseen_done_distinct() {
        let blocked = AgentPresentation {
            kind: AgentKind::Claude,
            activity: AgentActivity::NeedsAttention,
            seen: false,
        };
        let done = AgentPresentation {
            kind: AgentKind::Codex,
            activity: AgentActivity::Idle,
            seen: false,
        };

        assert_eq!((agent_icon(blocked), agent_color(blocked)), ("◉", NORD.danger));
        assert_eq!((agent_icon(done), agent_color(done)), ("●", NORD.accent_alt));
    }

    #[test]
    fn terminal_selection_maps_rendered_cells_to_authoritative_lines() {
        let attrs = CellAttributes::default();
        let terminal = TerminalView {
            pane_id: 9,
            title: "shell".to_owned(),
            first_row: 100,
            cols: 8,
            rows: 2,
            cursor_x: 0,
            cursor_y: 0,
            mouse_reporting: false,
            content_sequence: 0,
            lines: (0..6)
                .map(|index| TerminalLine::from_text(&format!("line-{index}"), &attrs, 0, None))
                .collect(),
        };
        let content = Rect::new(10, 4, 8, 2);

        assert_eq!(
            terminal_point(
                Some(&terminal),
                content,
                ScrollState::default(),
                Position { x: 12, y: 4 },
            ),
            Some((104, 2))
        );
        let mut scrolled = ScrollState::default();
        scrolled.scroll_up(2, ScrollMetrics::new(terminal.first_row, terminal.lines.len(), 2));
        assert_eq!(
            terminal_point(Some(&terminal), content, scrolled, Position { x: 10, y: 5 }),
            Some((103, 0))
        );
        assert_eq!(
            terminal_point(
                Some(&terminal),
                content,
                ScrollState::default(),
                Position { x: 9, y: 4 },
            ),
            None
        );
    }
}
