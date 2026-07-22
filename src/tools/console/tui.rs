use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Layout, Margin, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, List, ListItem, ListState, Paragraph, Widget, Wrap},
    Frame,
};
use tokio::time::MissedTickBehavior;
use wezterm_term::{color::ColorAttribute, Blink, CellAttributes, Intensity, Underline};

use crate::tui::{
    render_split_divider, theme::NORD, ActionId, ActionInvocation, ActionRegistry,
    ActionRegistryBuilder, ActionSpec, ActionState, ActionUnavailable, ContextMenu,
    ContextMenuLayout, ContextMenuOutcome, ContextMenuStyle, Direction, EventReader, KeyChord,
    KeybindingPlacement, LineEditor, MenuId, MenuPlacement, NavigationHistory, NavigationMap,
    NavigationRegion, Session, SessionOptions, SplitDividerStyle, SplitDrag, SplitFrame,
    SplitMinimums, SplitRatio,
};

use super::client::{
    ConnectionHealth, ConnectionState, ConsoleClient, ConsoleSnapshot, SessionControl, SessionId,
    SessionView, TerminalContentGeometry, TerminalView,
};
use super::config::Config;
use super::interaction::{
    resolve_control, ControlIntent, ControlOperation, EffectiveLayout, InteractionDecision,
    LayoutPreference, SessionAccess, TerminalOnlyReason,
};

const REFRESH_INTERVAL: Duration = Duration::from_millis(100);
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(350);
const SIDEBAR_STEP: i16 = 40;
const SESSION_MENU: MenuId = MenuId::new("console.session.context");
const HELP_MENU: MenuId = MenuId::new("console.help.actions");

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
const SCROLL_UP: ActionId = ActionId::new("console.terminal.scrollUp");
const SCROLL_DOWN: ActionId = ActionId::new("console.terminal.scrollDown");
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
const TOGGLE_HELP: ActionId = ActionId::new("console.help.toggle");
const RETRY_CONNECTION: ActionId = ActionId::new("console.connection.retry");
const QUIT: ActionId = ActionId::new("console.quit");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveRegion {
    Sessions,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceKind {
    Normal,
    Rename,
    Search,
    Help,
}

enum Surface {
    Normal,
    Rename { id: SessionId, input: LineEditor },
    Search { input: LineEditor, current_match: Option<usize> },
    Help,
}

impl Surface {
    fn kind(&self) -> SurfaceKind {
        match self {
            Self::Normal => SurfaceKind::Normal,
            Self::Rename { .. } => SurfaceKind::Rename,
            Self::Search { .. } => SurfaceKind::Search,
            Self::Help => SurfaceKind::Help,
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
    ScrollUp,
    ScrollDown,
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
    ToggleHelp,
    RetryConnection,
    Quit,
}

impl ConsoleAction {
    const ALL: [Self; 29] = [
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
        Self::ScrollUp,
        Self::ScrollDown,
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
        Self::ToggleHelp,
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
            Self::ScrollUp => SCROLL_UP,
            Self::ScrollDown => SCROLL_DOWN,
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
            Self::ToggleHelp => TOGGLE_HELP,
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
            Self::ScrollUp => "Scroll up",
            Self::ScrollDown => "Scroll down",
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
            Self::ToggleHelp => "Help",
            Self::RetryConnection => "Retry connection",
            Self::Quit => "Quit Console",
        }
    }
}

#[derive(Clone, Copy)]
struct ConsoleActionContext {
    target: Option<SessionId>,
    target_access: Option<SessionAccess>,
    selected: Option<SessionId>,
    selected_index: Option<usize>,
    session_count: usize,
    region: ActiveRegion,
    focus_left: Option<ActiveRegion>,
    focus_right: Option<ActiveRegion>,
    focus_next: Option<ActiveRegion>,
    focus_previous: Option<ActiveRegion>,
    surface: SurfaceKind,
    has_terminal: bool,
    terminal_line_count: usize,
    visible_rows: usize,
    scroll_offset: usize,
    can_history_back: bool,
    can_history_forward: bool,
    create_cols: u16,
    create_rows: u16,
    requested_ratio: Option<SplitRatio>,
    connection_retryable: bool,
}

enum Effect {
    None,
    RefreshSnapshot,
    RetryConnection,
    Create { cols: u16, rows: u16 },
    Rename { id: SessionId, title: String },
    Close(SessionId),
    AcquireControl(SessionId),
    ReleaseControl(SessionId),
    TakeControl(SessionId),
    Copy(String),
    SendKey(KeyEvent),
    SendMouse { id: SessionId, event: MouseEvent, geometry: TerminalContentGeometry },
    Paste(String),
    Quit,
}

#[derive(Clone, Copy)]
struct ActionHitTarget {
    area: Rect,
    action: ConsoleAction,
    target: Option<SessionId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalSelection {
    anchor_line: usize,
    anchor_column: usize,
    focus_line: usize,
    focus_column: usize,
}

impl TerminalSelection {
    fn point(line: usize, column: usize) -> Self {
        Self { anchor_line: line, anchor_column: column, focus_line: line, focus_column: column }
    }

    fn update(&mut self, line: usize, column: usize) {
        self.focus_line = line;
        self.focus_column = column;
    }

    fn ordered(self) -> ((usize, usize), (usize, usize)) {
        let anchor = (self.anchor_line, self.anchor_column);
        let focus = (self.focus_line, self.focus_column);
        if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        }
    }

    fn contains(self, line: usize, column: usize) -> bool {
        let (start, end) = self.ordered();
        (line, column) >= start && (line, column) <= end
    }
}

#[derive(Default)]
struct UiRegions {
    split: Option<SplitFrame>,
    sessions: Option<Rect>,
    terminal: Option<Rect>,
    terminal_content: Option<Rect>,
    session_rows: Vec<(Rect, SessionId)>,
    action_hits: Vec<ActionHitTarget>,
    context_menu: Option<ContextMenuLayout>,
}

impl UiRegions {
    fn navigation(&self) -> NavigationMap<ActiveRegion> {
        NavigationMap::new([
            NavigationRegion::new(ActiveRegion::Sessions, self.sessions.unwrap_or_default()),
            NavigationRegion::new(ActiveRegion::Terminal, self.terminal.unwrap_or_default()),
        ])
    }

    fn create_size(&self) -> (u16, u16) {
        self.terminal_content
            .map(|area| (area.width.max(1), area.height.max(1)))
            .unwrap_or((80, 24))
    }
}

struct App {
    client: ConsoleClient,
    config: Config,
    snapshot: ConsoleSnapshot,
    selected: Option<SessionId>,
    active_region: ActiveRegion,
    surface: Surface,
    layout: LayoutPreference,
    split_drag: Option<SplitDrag<()>>,
    history: NavigationHistory<SessionId>,
    menu: Option<ContextMenu<ConsoleActionContext>>,
    registry: ActionRegistry<ConsoleActionContext, ConsoleAction>,
    last_terminal_size: Option<(usize, u16, u16)>,
    last_session_click: Option<(SessionId, Instant)>,
    scroll_offset: usize,
    selection: Option<TerminalSelection>,
    notice: Option<String>,
    connection_generation: u64,
    connection: ConnectionState,
    connection_detail: Option<String>,
}

impl App {
    async fn new(client: ConsoleClient, config: Config) -> Result<Self> {
        let snapshot = client.snapshot(None).await?;
        let health = client.drain_connection_health()?;
        let connection_generation = health.map_or(0, |health| health.generation);
        let connection = health.map_or(ConnectionState::Ready, |health| health.state);
        let connection_detail = client.drain_remote_status().flatten().map(|status| status.text());
        let selected = snapshot.sessions.first().map(|session| session.id);
        let mut history = NavigationHistory::default();
        if let Some(id) = selected {
            history.visit(id);
        }
        let mut app = Self {
            layout: LayoutPreference::split(config.sidebar_split_ratio()),
            client,
            config,
            snapshot,
            selected,
            active_region: ActiveRegion::Sessions,
            surface: Surface::Normal,
            split_drag: None,
            history,
            menu: None,
            registry: console_actions()?,
            last_terminal_size: None,
            last_session_click: None,
            scroll_offset: 0,
            selection: None,
            notice: None,
            connection_generation,
            connection,
            connection_detail,
        };
        if app.selected.is_some() {
            app.refresh().await?;
        }
        Ok(app)
    }

    async fn refresh(&mut self) -> Result<()> {
        if let Some(status) = self.client.drain_remote_status() {
            self.connection_detail = status.map(|status| status.text());
        }
        if let Some(health) = self.client.drain_connection_health()? {
            self.apply_connection_health(health);
        }
        if matches!(
            self.connection,
            ConnectionState::Attaching | ConnectionState::Reconnecting { .. }
        ) {
            return Ok(());
        }
        let snapshot = match self.client.snapshot(self.selected).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if let Some(health) = self.client.drain_connection_health()? {
                    self.apply_connection_health(health);
                }
                if matches!(
                    self.connection,
                    ConnectionState::Attaching | ConnectionState::Reconnecting { .. }
                ) {
                    return Ok(());
                }
                return Err(error);
            }
        };
        self.reconcile(snapshot);
        Ok(())
    }

    async fn refresh_or_notice(&mut self) {
        if let Err(error) = self.refresh().await {
            if let Ok(Some(health)) = self.client.drain_connection_health() {
                self.apply_connection_health(health);
            }
            self.notice = Some(format!("Could not refresh Console: {error:#}"));
        }
    }

    fn reconcile(&mut self, snapshot: ConsoleSnapshot) {
        self.snapshot = snapshot;
        let selection_exists = self
            .selected
            .is_some_and(|id| self.snapshot.sessions.iter().any(|session| session.id == id));
        if !selection_exists {
            self.selected = self.snapshot.sessions.first().map(|session| session.id);
            self.scroll_offset = 0;
            if let Some(id) = self.selected {
                self.history.replace_current(id);
            }
        }
        if self.menu.as_ref().is_some_and(|menu| !self.has_session(menu.context().target)) {
            self.menu = None;
        }
        if let Surface::Rename { id, .. } = &self.surface {
            if !self.has_session(Some(*id)) {
                self.surface = Surface::Normal;
            }
        }
        let line_count = self.snapshot.terminal.as_ref().map_or(0, |terminal| terminal.lines.len());
        self.scroll_offset = self.scroll_offset.min(line_count.saturating_sub(1));
        if self.selection.is_some_and(|selection| {
            selection.anchor_line >= line_count || selection.focus_line >= line_count
        }) {
            self.selection = None;
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

    fn selected_session(&self) -> Option<&SessionView> {
        let selected = self.selected?;
        self.snapshot.sessions.iter().find(|session| session.id == selected)
    }

    fn session(&self, id: SessionId) -> Option<&SessionView> {
        self.snapshot.sessions.iter().find(|session| session.id == id)
    }

    fn selected_index(&self) -> Option<usize> {
        let selected = self.selected?;
        self.snapshot.sessions.iter().position(|session| session.id == selected)
    }

    fn select(&mut self, id: SessionId) -> bool {
        if !self.has_session(Some(id)) || self.selected == Some(id) {
            return false;
        }
        self.selected = Some(id);
        self.scroll_offset = 0;
        self.selection = None;
        self.history.visit(id);
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
                self.selected = Some(id);
                self.scroll_offset = 0;
                self.selection = None;
                return true;
            }
            offset += delta;
        }
        false
    }

    fn resize_request(&mut self, content: Option<Rect>) -> Option<(SessionId, u16, u16)> {
        let (Some(id), Some(session), Some(terminal), Some(content)) =
            (self.selected, self.selected_session(), self.snapshot.terminal.as_ref(), content)
        else {
            self.last_terminal_size = None;
            return None;
        };
        if session.control != SessionControl::Controller
            || content.width == 0
            || content.height == 0
        {
            self.last_terminal_size = None;
            return None;
        }
        let observed = (terminal.pane_id, content.width, content.height);
        if self.last_terminal_size == Some(observed) {
            return None;
        }
        self.last_terminal_size = Some(observed);
        Some((id, content.width, content.height))
    }

    fn action_context(
        &self,
        target: Option<SessionId>,
        regions: &UiRegions,
        requested_ratio: Option<SplitRatio>,
    ) -> ConsoleActionContext {
        let target = target.or(self.selected);
        let target_access = target.and_then(|id| self.session(id)).map(session_access);
        let (create_cols, create_rows) = regions.create_size();
        let visible_rows = regions.terminal_content.map_or(0, |area| usize::from(area.height));
        let navigation = regions.navigation();
        ConsoleActionContext {
            target,
            target_access,
            selected: self.selected,
            selected_index: self.selected_index(),
            session_count: self.snapshot.sessions.len(),
            region: self.active_region,
            focus_left: navigation.neighbor(self.active_region, Direction::Left),
            focus_right: navigation.neighbor(self.active_region, Direction::Right),
            focus_next: navigation.next(self.active_region),
            focus_previous: navigation.previous(self.active_region),
            surface: self.surface.kind(),
            has_terminal: self.snapshot.terminal.is_some(),
            terminal_line_count: self
                .snapshot
                .terminal
                .as_ref()
                .map_or(0, |terminal| terminal.lines.len()),
            visible_rows,
            scroll_offset: self.scroll_offset,
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
                Effect::RefreshSnapshot
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
            ConsoleAction::ScrollUp => {
                let page = context.visible_rows.saturating_sub(1).max(1);
                let maximum = context.terminal_line_count.saturating_sub(context.visible_rows);
                self.scroll_offset = self.scroll_offset.saturating_add(page).min(maximum);
                self.selection = None;
                Effect::None
            }
            ConsoleAction::ScrollDown => {
                let page = context.visible_rows.saturating_sub(1).max(1);
                self.scroll_offset = self.scroll_offset.saturating_sub(page);
                self.selection = None;
                Effect::None
            }
            ConsoleAction::PreviousSession => {
                self.move_selection(-1);
                Effect::RefreshSnapshot
            }
            ConsoleAction::NextSession => {
                self.move_selection(1);
                Effect::RefreshSnapshot
            }
            ConsoleAction::HistoryBack => {
                self.navigate_history(-1);
                Effect::RefreshSnapshot
            }
            ConsoleAction::HistoryForward => {
                self.navigate_history(1);
                Effect::RefreshSnapshot
            }
            ConsoleAction::FocusSessions => {
                self.active_region = ActiveRegion::Sessions;
                Effect::None
            }
            ConsoleAction::FocusLeft => {
                if let Some(region) = context.focus_left {
                    self.active_region = region;
                }
                Effect::None
            }
            ConsoleAction::FocusRight => {
                if let Some(region) = context.focus_right {
                    self.active_region = region;
                }
                Effect::None
            }
            ConsoleAction::FocusNext => {
                if let Some(region) = context.focus_next {
                    self.active_region = region;
                }
                Effect::None
            }
            ConsoleAction::FocusPrevious => {
                if let Some(region) = context.focus_previous {
                    self.active_region = region;
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
            ConsoleAction::ToggleHelp => {
                self.surface = if matches!(self.surface, Surface::Help) {
                    Surface::Normal
                } else {
                    Surface::Help
                };
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
            Surface::Help => {
                self.surface = Surface::Normal;
                Effect::None
            }
        }
    }

    fn control_intent(&mut self, target: Option<SessionId>, intent: ControlIntent) -> Effect {
        let Some(id) = target else {
            return Effect::None;
        };
        let Some(access) = self.session(id).map(session_access) else {
            return Effect::RefreshSnapshot;
        };
        match resolve_control(intent, access) {
            InteractionDecision::FocusTerminal => {
                self.active_region = ActiveRegion::Terminal;
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
        let Surface::Search { input, current_match } = &mut self.surface else {
            return;
        };
        let query = input.value();
        if query.is_empty() {
            *current_match = None;
            return;
        }
        let Some(terminal) = self.snapshot.terminal.as_ref() else {
            return;
        };
        let matches = terminal
            .lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| line.as_str().contains(query).then_some(index))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            self.notice = Some(format!("No matches for {query:?}"));
            *current_match = None;
            return;
        }
        let next = current_match
            .and_then(|current| matches.iter().copied().find(|index| *index > current))
            .unwrap_or(matches[0]);
        *current_match = Some(next);
        let visible_rows = visible_rows.max(1);
        let maximum_start = terminal.lines.len().saturating_sub(visible_rows);
        let desired_start = next.min(maximum_start);
        self.scroll_offset = maximum_start.saturating_sub(desired_start);
        self.notice = Some(format!(
            "Match {} of {}",
            matches.iter().position(|index| *index == next).unwrap_or_default() + 1,
            matches.len()
        ));
    }

    fn visible_terminal_text(&self, visible_rows: usize) -> Option<String> {
        let terminal = self.snapshot.terminal.as_ref()?;
        let range = visible_line_range(terminal.lines.len(), visible_rows, self.scroll_offset);
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
        let selection = self.selection?;
        let terminal = self.snapshot.terminal.as_ref()?;
        if terminal.lines.is_empty() {
            return None;
        }
        let (start, end) = selection.ordered();
        let mut selected_lines = Vec::new();
        for line_index in start.0..=end.0.min(terminal.lines.len().saturating_sub(1)) {
            let line = &terminal.lines[line_index];
            let mut text = String::new();
            for cell in line
                .visible_cells()
                .filter(|cell| selection.contains(line_index, cell.cell_index()))
            {
                text.push_str(cell.str());
            }
            selected_lines.push(text.trim_end().to_owned());
        }
        let text = selected_lines.join("\n");
        (!text.is_empty()).then_some(text)
    }

    fn begin_selection(&mut self, position: Position, content: Rect) {
        let Some((line, column)) =
            terminal_point(self.snapshot.terminal.as_ref(), content, self.scroll_offset, position)
        else {
            return;
        };
        self.selection = Some(TerminalSelection::point(line, column));
    }

    fn update_selection(&mut self, position: Position, content: Rect) {
        let Some((line, column)) =
            terminal_point(self.snapshot.terminal.as_ref(), content, self.scroll_offset, position)
        else {
            return;
        };
        if let Some(selection) = self.selection.as_mut() {
            selection.update(line, column);
        }
    }

    fn terminal_input_enabled(&self) -> bool {
        self.selected_session()
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
                self.active_region = ActiveRegion::Terminal;
                self.menu = None;
                self.split_drag = None;
            }
            (LayoutPreference::Split { .. }, ActiveRegion::Terminal) => {
                self.active_region = ActiveRegion::Sessions;
            }
        }
    }

    fn normalize_focus(&mut self, regions: &UiRegions) {
        self.active_region =
            regions.navigation().normalize(self.active_region).unwrap_or(ActiveRegion::Terminal);
        if regions.sessions.is_none() {
            self.menu = None;
            self.split_drag = None;
        }
    }
}

fn session_access(session: &SessionView) -> SessionAccess {
    session.control.into()
}

pub async fn run(client: ConsoleClient, config: Config) -> Result<()> {
    let mut app = App::new(client, config).await?;
    let mut session = Session::open(SessionOptions { mouse_capture: true, bracketed_paste: true })?;
    let mut events = EventReader::start();
    let mut refresh = tokio::time::interval(REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    refresh.tick().await;

    loop {
        let mut regions = UiRegions::default();
        session.draw(|frame| regions = render(frame, &app))?;
        app.normalize_focus(&regions);

        if let Some((id, cols, rows)) = app.resize_request(regions.terminal_content) {
            if let Err(error) = app.client.resize(id, cols, rows).await {
                app.notice = Some(error.to_string());
            }
            app.refresh_or_notice().await;
            continue;
        }

        let effect = tokio::select! {
            _ = refresh.tick() => {
                app.refresh_or_notice().await;
                Effect::None
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                handle_event(event, &mut app, &regions)?
            }
        };

        match apply_effect(effect, &mut app, &mut session).await {
            Ok(EffectFlow::Continue) => {}
            Ok(EffectFlow::Quit) => break,
            Err(error) => {
                app.notice = Some(error.to_string());
                app.refresh_or_notice().await;
            }
        }
    }
    Ok(())
}

enum EffectFlow {
    Continue,
    Quit,
}

async fn apply_effect(effect: Effect, app: &mut App, session: &mut Session) -> Result<EffectFlow> {
    match effect {
        Effect::None => {}
        Effect::RefreshSnapshot => app.refresh().await?,
        Effect::RetryConnection => {
            app.client.retry().await?;
            app.connection = ConnectionState::Attaching;
            app.connection_detail = None;
            app.refresh().await?;
        }
        Effect::Create { cols, rows } => {
            let id = app.client.create_session(cols, rows).await?;
            app.selected = Some(id);
            app.history.visit(id);
            app.active_region = ActiveRegion::Terminal;
            app.scroll_offset = 0;
            app.selection = None;
            app.refresh().await?;
        }
        Effect::Rename { id, title } => {
            app.client.rename_session(id, title).await?;
            app.refresh().await?;
        }
        Effect::Close(id) => {
            app.client.close_session(id).await?;
            app.refresh().await?;
        }
        Effect::AcquireControl(id) => {
            app.client.acquire_control(id).await?;
            app.active_region = ActiveRegion::Terminal;
            app.refresh().await?;
        }
        Effect::ReleaseControl(id) => {
            app.client.release_control(id).await?;
            app.active_region = ActiveRegion::Sessions;
            app.refresh().await?;
        }
        Effect::TakeControl(id) => {
            app.client.take_control(id).await?;
            app.active_region = ActiveRegion::Terminal;
            app.refresh().await?;
        }
        Effect::Copy(text) => {
            session.copy(&text)?;
            app.notice = Some(format!("Copied {} visible lines", text.lines().count()));
        }
        Effect::SendKey(key) => {
            if let Some(id) = app.selected {
                app.client.send_key(id, key).await?;
                app.refresh().await?;
            }
        }
        Effect::SendMouse { id, event, geometry } => {
            if app.client.send_mouse(id, event, geometry).await? {
                app.refresh().await?;
            }
        }
        Effect::Paste(text) => {
            if let Some(id) = app.selected {
                app.client.paste(id, text).await?;
                app.refresh().await?;
            }
        }
        Effect::Quit => return Ok(EffectFlow::Quit),
    }
    Ok(EffectFlow::Continue)
}

fn handle_event(event: Event, app: &mut App, regions: &UiRegions) -> Result<Effect> {
    app.notice = None;
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
        Event::Mouse(mouse) => handle_mouse(mouse, app, regions),
        Event::Paste(text)
            if app.active_region == ActiveRegion::Terminal
                && app.surface.kind() == SurfaceKind::Normal
                && app.terminal_input_enabled() =>
        {
            Ok(Effect::Paste(text))
        }
        Event::Resize(_, _) => {
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
    let context = app.action_context(None, regions, None);
    if let Some(chord) = KeyChord::from_event(key) {
        if let Some(invocation) = app.registry.resolve_keybinding(chord, context) {
            return app.invoke(invocation, regions);
        }
    }

    match &mut app.surface {
        Surface::Rename { input, .. } => {
            input.apply_key(key);
            Ok(Effect::None)
        }
        Surface::Search { input, current_match } => {
            input.apply_key(key);
            *current_match = None;
            app.scroll_offset = 0;
            Ok(Effect::None)
        }
        Surface::Normal if app.active_region == ActiveRegion::Terminal => {
            if app.terminal_input_enabled() {
                Ok(Effect::SendKey(key))
            } else {
                Ok(Effect::None)
            }
        }
        Surface::Normal | Surface::Help => Ok(Effect::None),
    }
}

fn handle_mouse(mouse: MouseEvent, app: &mut App, regions: &UiRegions) -> Result<Effect> {
    let position = Position { x: mouse.column, y: mouse.row };

    if let (Some(content), Some(terminal), Some(id)) =
        (regions.terminal_content, app.snapshot.terminal.as_ref(), app.selected)
    {
        if terminal.mouse_reporting
            && content.contains(position)
            && app.terminal_input_enabled()
            && !mouse.modifiers.contains(KeyModifiers::SHIFT)
        {
            return Ok(Effect::SendMouse {
                id,
                event: mouse,
                geometry: TerminalContentGeometry::new(
                    content.x,
                    content.y,
                    content.width,
                    content.height,
                ),
            });
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
            if regions.terminal_content.is_some_and(|area| area.contains(position)) {
                if let Some(content) = regions.terminal_content {
                    app.begin_selection(position, content);
                }
                return app.invoke_action(ConsoleAction::Activate, None, regions);
            }
            if let Some(region) = regions.navigation().hit_test(mouse.column, mouse.row) {
                return match region {
                    ActiveRegion::Sessions => {
                        app.invoke_action(ConsoleAction::FocusSessions, None, regions)
                    }
                    ActiveRegion::Terminal => {
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
                return Ok(Effect::RefreshSnapshot);
            }
            if regions.terminal_content.is_some_and(|area| area.contains(position)) {
                app.active_region = ActiveRegion::Terminal;
                let context = app.action_context(app.selected, regions, None);
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
            if let Some(content) = regions.terminal_content.filter(|area| area.contains(position)) {
                app.update_selection(position, content);
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
        MouseEventKind::ScrollUp
            if regions.terminal_content.is_some_and(|area| area.contains(position)) =>
        {
            return app.invoke_action(ConsoleAction::ScrollUp, None, regions);
        }
        MouseEventKind::ScrollDown
            if regions.terminal_content.is_some_and(|area| area.contains(position)) =>
        {
            return app.invoke_action(ConsoleAction::ScrollDown, None, regions);
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

fn console_actions() -> Result<ActionRegistry<ConsoleActionContext, ConsoleAction>> {
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
            | ConsoleAction::ToggleHelp
            | ConsoleAction::Quit => enabled,
            ConsoleAction::RetryConnection => can_retry_connection,
            ConsoleAction::Activate => can_activate,
            ConsoleAction::Dismiss => can_dismiss,
            ConsoleAction::RenameSession | ConsoleAction::CloseSession => can_mutate,
            ConsoleAction::ReleaseControl => can_release,
            ConsoleAction::TakeControl => can_take,
            ConsoleAction::PrimaryControl => can_primary_control,
            ConsoleAction::CopyVisibleTerminal | ConsoleAction::OpenSearch => has_terminal,
            ConsoleAction::ScrollUp => can_scroll_up,
            ConsoleAction::ScrollDown => can_scroll_down,
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

    for (order, action) in ConsoleAction::ALL.into_iter().enumerate() {
        builder.place_menu(MenuPlacement {
            menu: HELP_MENU,
            action: action.id(),
            group: "commands",
            group_order: 10,
            order: order as i16,
            when: visible_in_help,
        });
    }

    for (code, modifiers, action, when) in [
        (
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
            ConsoleAction::CreateSession,
            sidebar_normal as fn(&ConsoleActionContext) -> bool,
        ),
        (KeyCode::Char('w'), KeyModifiers::CONTROL, ConsoleAction::CloseSession, sidebar_normal),
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
        (KeyCode::Up, KeyModifiers::NONE, ConsoleAction::PreviousSession, sidebar_normal),
        (KeyCode::Down, KeyModifiers::NONE, ConsoleAction::NextSession, sidebar_normal),
        (KeyCode::Left, KeyModifiers::NONE, ConsoleAction::HistoryBack, sidebar_normal),
        (KeyCode::Right, KeyModifiers::NONE, ConsoleAction::HistoryForward, sidebar_normal),
        (KeyCode::Char('b'), KeyModifiers::CONTROL, ConsoleAction::ToggleSidebar, normal),
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
        (KeyCode::F(1), KeyModifiers::NONE, ConsoleAction::ToggleHelp, not_terminal_normal),
        (KeyCode::Char('?'), KeyModifiers::NONE, ConsoleAction::ToggleHelp, help_key_available),
        (KeyCode::F(5), KeyModifiers::NONE, ConsoleAction::RetryConnection, not_terminal_normal),
        (
            KeyCode::Char('R'),
            KeyModifiers::NONE,
            ConsoleAction::RetryConnection,
            connection_retryable,
        ),
        (KeyCode::Char('q'), KeyModifiers::NONE, ConsoleAction::Quit, connection_retryable),
        (KeyCode::Char('q'), KeyModifiers::CONTROL, ConsoleAction::Quit, not_terminal_normal),
    ] {
        builder.bind_key(KeybindingPlacement {
            chord: KeyChord::new(code, modifiers),
            action: action.id(),
            when,
        });
    }

    Ok(builder.build()?)
}

fn always(_: &ConsoleActionContext) -> bool {
    true
}

fn normal(context: &ConsoleActionContext) -> bool {
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

fn terminal_normal(context: &ConsoleActionContext) -> bool {
    normal(context) && context.region == ActiveRegion::Terminal
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

fn help_key_available(context: &ConsoleActionContext) -> bool {
    sidebar_normal(context) || terminal_local_tools(context) || connection_retryable(context)
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

fn visible_in_help(_: &ConsoleActionContext) -> bool {
    true
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
        Some(SessionAccess::ControlledBySelf) => {
            ActionState::disabled("this client already has control")
        }
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
    if context.terminal_line_count > context.visible_rows.saturating_add(context.scroll_offset) {
        ActionState::Enabled
    } else {
        ActionState::disabled("already at the oldest projected line")
    }
}

fn can_scroll_down(context: &ConsoleActionContext) -> ActionState {
    if context.scroll_offset > 0 {
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

fn render(frame: &mut Frame<'_>, app: &App) -> UiRegions {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::default().bg(NORD.background)), area);
    let rows = Layout::vertical([Constraint::Length(2), Constraint::Min(1), Constraint::Length(2)])
        .split(area);
    let mut regions = UiRegions::default();
    render_header(frame, rows[0], app, &mut regions);

    match app.layout.effective(rows[1].width) {
        EffectiveLayout::Split => {
            let split = SplitFrame::horizontal(
                rows[1],
                app.layout.restore_ratio(),
                SplitMinimums::new(18, 20),
            );
            regions.split = Some(split);
            regions.sessions = Some(split.first);
            regions.terminal = Some(split.second);
            render_sessions(frame, split.first, app, &mut regions);
            render_terminal(frame, split.second, app, &mut regions);
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
            regions.sessions = Some(rows[1]);
            render_sessions(frame, rows[1], app, &mut regions);
        }
        EffectiveLayout::TerminalOnly { .. } => {
            regions.terminal = Some(rows[1]);
            render_terminal(frame, rows[1], app, &mut regions);
        }
    }
    render_footer(frame, rows[2], app, &mut regions);

    if let Some(menu) = app.menu.as_ref() {
        let layout = menu.layout(area);
        menu.render(frame, &layout, ContextMenuStyle::from_theme(NORD));
        regions.context_menu = Some(layout);
    }
    if matches!(app.surface, Surface::Help) {
        render_help(frame, area, app, &regions);
    }
    regions
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let selected = app.selected_session().map_or_else(String::new, |session| {
        format!(
            "  {}  ·  {}  ·  window {} / tab {} / pane {}",
            session.title,
            control_label(session.control),
            session.window_id,
            session.tab_id,
            session.pane_id
        )
    });
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " kit console ",
                Style::default().fg(NORD.text_strong).bg(NORD.focus).add_modifier(Modifier::BOLD),
            ),
            Span::styled(selected, Style::default().fg(NORD.text_muted)),
        ]))
        .block(
            Block::new()
                .borders(ratatui::widgets::Borders::BOTTOM)
                .border_style(Style::default().fg(NORD.border)),
        ),
        area,
    );
    if !app.layout.effective(area.width).is_split() {
        let width = area.width.min(20);
        let restore = Rect::new(area.right().saturating_sub(width), area.y, width, 1);
        let label = if app.active_region == ActiveRegion::Sessions {
            "[Ctrl+B] Terminal"
        } else {
            "[Ctrl+B] Sessions"
        };
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Right)
                .style(Style::default().fg(NORD.accent)),
            restore,
        );
        regions.action_hits.push(ActionHitTarget {
            area: restore,
            action: ConsoleAction::ToggleSidebar,
            target: None,
        });
    }
}

fn render_sessions(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let focused = app.active_region == ActiveRegion::Sessions;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { NORD.focus } else { NORD.border }))
        .title(Span::styled(
            format!(" Sessions {} ", app.snapshot.sessions.len()),
            Style::default()
                .fg(if focused { NORD.text_strong } else { NORD.text_muted })
                .add_modifier(if focused { Modifier::BOLD } else { Modifier::empty() }),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).split(inner);

    let item_width = usize::from(chunks[0].width.saturating_sub(2));
    let items =
        app.snapshot.sessions.iter().map(|session| session_item(session, &app.surface, item_width));
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
    regions.session_rows = app
        .snapshot
        .sessions
        .iter()
        .skip(offset)
        .take(usize::from(chunks[0].height))
        .enumerate()
        .map(|(row, session)| {
            (
                Rect::new(chunks[0].x, chunks[0].y.saturating_add(row as u16), chunks[0].width, 1),
                session.id,
            )
        })
        .collect();
    for (row, id) in &regions.session_rows {
        let control_width = row.width.min(11);
        let control_area = Rect::new(
            row.x.saturating_add(row.width.saturating_sub(control_width)),
            row.y,
            control_width,
            1,
        );
        if let Some(session) = app.session(*id) {
            if session_access(session).primary_control().is_some() {
                regions.action_hits.push(ActionHitTarget {
                    area: control_area,
                    action: ConsoleAction::PrimaryControl,
                    target: Some(*id),
                });
            }
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

fn session_item(session: &SessionView, surface: &Surface, width: usize) -> ListItem<'static> {
    let mut title = match surface {
        Surface::Rename { id, input } if *id == session.id => format!("✎ {}▏", input.value()),
        _ => session.title.clone(),
    };
    let status = control_label(session.control);
    let status_width = 10.min(width);
    let title_width = width.saturating_sub(status_width.saturating_add(1));
    title.truncate(title.char_indices().nth(title_width).map_or(title.len(), |(index, _)| index));
    ListItem::new(Line::from(vec![
        Span::styled(format!("{title:<title_width$} "), Style::default().fg(NORD.text)),
        Span::styled(
            format!("{status:>status_width$}"),
            Style::default().fg(control_color(session.control)),
        ),
    ]))
}

fn control_label(control: SessionControl) -> &'static str {
    SessionAccess::from(control).label()
}

fn control_color(control: SessionControl) -> Color {
    match SessionAccess::from(control) {
        SessionAccess::ControlledBySelf => NORD.success,
        SessionAccess::ControlledByOther => NORD.accent,
        SessionAccess::Synchronizing => NORD.warning,
        SessionAccess::Available => NORD.text_muted,
    }
}

fn render_terminal(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let focused = app.active_region == ActiveRegion::Terminal;
    let title = app.snapshot.terminal.as_ref().map_or_else(
        || " Terminal ".to_owned(),
        |terminal| {
            let scroll = if app.scroll_offset > 0 {
                format!("  ·  -{} lines", app.scroll_offset)
            } else {
                String::new()
            };
            format!(
                " {}  ·  pane {}  ·  {}×{}{} ",
                terminal.title, terminal.pane_id, terminal.cols, terminal.rows, scroll
            )
        },
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { NORD.focus } else { NORD.border }))
        .title(Span::styled(
            title,
            Style::default()
                .fg(if focused { NORD.text_strong } else { NORD.text_muted })
                .add_modifier(if focused { Modifier::BOLD } else { Modifier::empty() }),
        ));
    let content = block.inner(area);
    frame.render_widget(block, area);
    regions.terminal_content = Some(content);

    if let Some(terminal) = app.snapshot.terminal.as_ref() {
        let range = visible_line_range(
            terminal.lines.len(),
            usize::from(content.height),
            app.scroll_offset,
        );
        frame.render_widget(
            TerminalCells {
                terminal,
                start: range.start,
                query: app.search_query(),
                selection: app.selection,
            },
            content,
        );
        if focused
            && app.scroll_offset == 0
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
                "No sessions yet\n\nPress Ctrl+N or click New session"
            } else {
                "Waiting for authoritative terminal projection…"
            })
            .alignment(Alignment::Center)
            .style(Style::default().fg(NORD.text_muted)),
            content.inner(Margin { horizontal: 1, vertical: 1 }),
        );
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let inner = Block::new()
        .borders(ratatui::widgets::Borders::TOP)
        .border_style(Style::default().fg(NORD.border))
        .inner(area);
    frame.render_widget(
        Block::new()
            .borders(ratatui::widgets::Borders::TOP)
            .border_style(Style::default().fg(NORD.border)),
        area,
    );
    let connection_notice = match app.connection {
        ConnectionState::Attaching => Some("Connecting to Console…".to_owned()),
        ConnectionState::Reconnecting { attempt } => {
            Some(format!("Reconnecting to Console… attempt {attempt}"))
        }
        ConnectionState::Failed => Some("Console connection failed — press R to retry".to_owned()),
        ConnectionState::RetryExhausted => {
            Some("Console reconnect limit reached — press R to retry".to_owned())
        }
        ConnectionState::Detached => Some("Console is detached — press R to retry".to_owned()),
        ConnectionState::Ready => None,
    };
    let selected = app.selected_session();
    let access = selected.map(session_access);
    let base_left = match &app.surface {
        Surface::Rename { input, .. } => {
            format!(" Rename: {}▏   Enter save   Esc cancel", input.value())
        }
        Surface::Search { input, .. } => {
            format!(" Search: {}▏   Enter next   Esc close   PageUp/PageDown scroll", input.value())
        }
        Surface::Normal | Surface::Help => app
            .connection_detail
            .clone()
            .or_else(|| connection_notice.clone())
            .or_else(|| app.notice.clone())
            .unwrap_or_else(|| {
                if matches!(access, Some(SessionAccess::ControlledBySelf))
                    && app.active_region == ActiveRegion::Terminal
                {
                    " Ctrl+B Sessions   ·   keyboard → terminal   ·   Shift+mouse selects"
                        .to_owned()
                } else {
                    " ↑↓ sessions   ←→ history   Enter open   Ctrl+B collapse".to_owned()
                }
            }),
    };

    // Connection recovery, navigation, and session-control state are independent facts. Keep
    // them visible together: a failed transport must not erase a read-only warning, and a
    // read-only lease must not erase the path back to the sidebar.
    let left = if matches!(app.surface, Surface::Normal | Surface::Help) {
        access
            .and_then(SessionAccess::banner)
            .map(|banner| format!("{base_left}  ·  {banner}"))
            .unwrap_or(base_left)
    } else {
        base_left
    };

    let mut actions = if matches!(app.surface, Surface::Normal | Surface::Help) {
        if matches!(
            app.connection,
            ConnectionState::Failed | ConnectionState::RetryExhausted | ConnectionState::Detached
        ) {
            vec![
                ("[R] Retry", ConsoleAction::RetryConnection, None),
                ("[q] Quit", ConsoleAction::Quit, None),
                ("[?] Help", ConsoleAction::ToggleHelp, None),
            ]
        } else {
            match (app.selected, access) {
                (Some(id), Some(SessionAccess::Available)) => {
                    vec![("[Enter] Acquire", ConsoleAction::PrimaryControl, Some(id))]
                }
                (Some(id), Some(SessionAccess::ControlledByOther)) => {
                    vec![("[T] Take control", ConsoleAction::PrimaryControl, Some(id))]
                }
                (Some(id), Some(SessionAccess::ControlledBySelf))
                    if app.active_region == ActiveRegion::Sessions =>
                {
                    vec![("[D] Release", ConsoleAction::PrimaryControl, Some(id))]
                }
                _ => vec![("[?] Help", ConsoleAction::ToggleHelp, None)],
            }
        }
    } else {
        Vec::new()
    };

    let action_width = |label: &str| u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
    while actions.len() > 1
        && actions.iter().map(|(label, _, _)| action_width(label).saturating_add(1)).sum::<u16>()
            > inner.width
    {
        let _ = actions.pop();
    }
    let actions_width = actions
        .iter()
        .map(|(label, _, _)| action_width(label).saturating_add(1))
        .sum::<u16>()
        .min(inner.width);
    let left_width = inner.width.saturating_sub(actions_width);
    let left_area = Rect::new(inner.x, inner.y, left_width, inner.height);
    let left_color = if app.connection_detail.is_some()
        || connection_notice.is_some()
        || app.notice.is_some()
        || matches!(access, Some(SessionAccess::Synchronizing | SessionAccess::ControlledByOther))
    {
        NORD.warning
    } else {
        NORD.text_muted
    };
    frame.render_widget(Paragraph::new(left).style(Style::default().fg(left_color)), left_area);

    let mut action_x = inner.right().saturating_sub(actions_width);
    for (label, action, target) in actions {
        let width = action_width(label).min(inner.right().saturating_sub(action_x));
        let action_area = Rect::new(action_x, inner.y, width, inner.height);
        frame.render_widget(
            Paragraph::new(label)
                .alignment(Alignment::Right)
                .style(Style::default().fg(NORD.accent).add_modifier(Modifier::BOLD)),
            action_area,
        );
        regions.action_hits.push(ActionHitTarget { area: action_area, action, target });
        action_x = action_x.saturating_add(width.saturating_add(1));
    }
}

fn render_help(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &UiRegions) {
    let width = area.width.saturating_sub(4).min(64).max(20.min(area.width));
    let height = area.height.saturating_sub(4).min(30).max(8.min(area.height));
    let popup = Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y.saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    );
    let context = app.action_context(None, regions, None);
    let lines = app
        .registry
        .resolve_menu(HELP_MENU, &context)
        .items()
        .iter()
        .filter(|action| action.primary_keybinding().is_some())
        .map(|action| {
            let key = action.primary_keybinding().expect("filtered keybinding");
            let state = if action.state.is_enabled() { "" } else { "  unavailable" };
            Line::from(vec![
                Span::styled(format!("{key:<18}"), Style::default().fg(NORD.accent)),
                Span::styled(action.title, Style::default().fg(NORD.text)),
                Span::styled(state, Style::default().fg(NORD.text_muted)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(NORD.focus))
                .title(" Console help · F1/Esc close ")
                .style(Style::default().bg(NORD.background)),
        ),
        popup,
    );
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
                if self.selection.is_some_and(|selection| selection.contains(line_index, column)) {
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
    scroll_offset: usize,
    position: Position,
) -> Option<(usize, usize)> {
    let terminal = terminal?;
    if !content.contains(position) {
        return None;
    }
    let range =
        visible_line_range(terminal.lines.len(), usize::from(content.height), scroll_offset);
    let line = range.start.checked_add(usize::from(position.y - content.y))?;
    (line < range.end).then_some((line, usize::from(position.x - content.x)))
}

fn visible_line_range(
    line_count: usize,
    visible_rows: usize,
    scroll_offset: usize,
) -> std::ops::Range<usize> {
    let visible_rows = visible_rows.min(line_count);
    let live_start = line_count.saturating_sub(visible_rows);
    let start = live_start.saturating_sub(scroll_offset.min(live_start));
    start..start.saturating_add(visible_rows).min(line_count)
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
    use wezterm_term::Line as TerminalLine;

    fn context(control: SessionControl) -> ConsoleActionContext {
        ConsoleActionContext {
            target: Some(7),
            target_access: Some(control.into()),
            selected: Some(7),
            selected_index: Some(1),
            session_count: 3,
            region: ActiveRegion::Sessions,
            focus_left: None,
            focus_right: Some(ActiveRegion::Terminal),
            focus_next: Some(ActiveRegion::Terminal),
            focus_previous: Some(ActiveRegion::Terminal),
            surface: SurfaceKind::Normal,
            has_terminal: true,
            terminal_line_count: 20,
            visible_rows: 5,
            scroll_offset: 1,
            can_history_back: true,
            can_history_forward: true,
            create_cols: 80,
            create_rows: 24,
            requested_ratio: None,
            connection_retryable: false,
        }
    }

    #[test]
    fn catalog_projects_every_phase_two_action_and_shared_enablement() {
        let registry = console_actions().unwrap();
        let controller = context(SessionControl::Controller);
        let observer = context(SessionControl::Observer);
        let help = registry.resolve_menu(HELP_MENU, &controller);

        assert_eq!(help.len(), ConsoleAction::ALL.len());
        assert!(ConsoleAction::ALL
            .iter()
            .all(|action| help.items().iter().any(|item| item.id == action.id())));

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
        let registry = console_actions().unwrap();
        let context = context(SessionControl::Controller);
        let rename_key = registry
            .resolve_keybinding(KeyChord::new(KeyCode::F(2), KeyModifiers::NONE), context)
            .unwrap();
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
    fn session_menu_uses_one_rehydrated_control_intent() {
        let registry = console_actions().unwrap();
        let menu = registry.resolve_menu(SESSION_MENU, &context(SessionControl::Controller));

        assert!(menu.items().iter().any(|item| item.id == PRIMARY_CONTROL));
        assert!(!menu.items().iter().any(|item| item.id == RELEASE_CONTROL));
        assert!(!menu.items().iter().any(|item| item.id == TAKE_CONTROL));
    }

    #[test]
    fn terminal_focus_reserves_only_the_sidebar_escape_key() {
        let registry = console_actions().unwrap();
        let mut terminal = context(SessionControl::Controller);
        terminal.region = ActiveRegion::Terminal;

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
                registry.resolve_keybinding(chord, terminal).is_none(),
                "terminal key {chord} was captured by the Console shell"
            );
        }

        let escape = registry
            .resolve_keybinding(KeyChord::new(KeyCode::Char('b'), KeyModifiers::CONTROL), terminal)
            .unwrap();
        assert_eq!(registry.command_for(&escape), Ok(ConsoleAction::ToggleSidebar));
    }

    #[test]
    fn read_only_terminal_keeps_local_copy_without_capturing_healthy_terminal_keys() {
        let registry = console_actions().unwrap();
        let mut observer = context(SessionControl::Observer);
        observer.region = ActiveRegion::Terminal;

        let copy = registry
            .resolve_keybinding(
                KeyChord::new(KeyCode::Char('C'), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                observer,
            )
            .expect("read-only terminal keeps local copy");
        assert_eq!(registry.command_for(&copy), Ok(ConsoleAction::CopyVisibleTerminal));
        let scroll = registry
            .resolve_keybinding(KeyChord::new(KeyCode::PageUp, KeyModifiers::NONE), observer)
            .expect("read-only terminal keeps local scrolling");
        assert_eq!(registry.command_for(&scroll), Ok(ConsoleAction::ScrollUp));
        let take = registry
            .resolve_keybinding(KeyChord::new(KeyCode::Char('t'), KeyModifiers::NONE), observer)
            .expect("read-only terminal exposes its advertised takeover key");
        assert_eq!(registry.command_for(&take), Ok(ConsoleAction::TakeControl));
        let help = registry
            .resolve_keybinding(KeyChord::new(KeyCode::Char('?'), KeyModifiers::NONE), observer)
            .expect("read-only terminal keeps local help");
        assert_eq!(registry.command_for(&help), Ok(ConsoleAction::ToggleHelp));

        let mut available = context(SessionControl::Uncontrolled);
        available.region = ActiveRegion::Terminal;
        let activate = registry
            .resolve_keybinding(KeyChord::new(KeyCode::Enter, KeyModifiers::NONE), available)
            .expect("available terminal exposes its advertised acquire key");
        assert_eq!(registry.command_for(&activate), Ok(ConsoleAction::Activate));

        let mut controller = context(SessionControl::Controller);
        controller.region = ActiveRegion::Terminal;
        assert!(registry
            .resolve_keybinding(
                KeyChord::new(KeyCode::Char('C'), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                controller,
            )
            .is_none());
        assert!(registry
            .resolve_keybinding(KeyChord::new(KeyCode::PageUp, KeyModifiers::NONE), controller)
            .is_none());
    }

    #[test]
    fn sidebar_keeps_panel_navigation_keys() {
        let registry = console_actions().unwrap();
        let sidebar = context(SessionControl::Controller);

        for (chord, expected) in [
            (KeyChord::new(KeyCode::Enter, KeyModifiers::NONE), ConsoleAction::Activate),
            (KeyChord::new(KeyCode::Tab, KeyModifiers::NONE), ConsoleAction::FocusNext),
            (KeyChord::new(KeyCode::Char('q'), KeyModifiers::CONTROL), ConsoleAction::Quit),
        ] {
            let invocation = registry.resolve_keybinding(chord, sidebar).unwrap();
            assert_eq!(registry.command_for(&invocation), Ok(expected));
        }
    }

    #[test]
    fn navigation_history_and_geometry_preserve_left_right_behavior() {
        let map = NavigationMap::new([
            NavigationRegion::new(ActiveRegion::Sessions, Rect::new(0, 0, 20, 10)),
            NavigationRegion::new(ActiveRegion::Terminal, Rect::new(21, 0, 40, 10)),
        ]);
        assert_eq!(
            map.neighbor(ActiveRegion::Sessions, Direction::Right),
            Some(ActiveRegion::Terminal)
        );
        assert_eq!(
            map.neighbor(ActiveRegion::Terminal, Direction::Left),
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
            cols: 12,
            rows: 2,
            cursor_x: 0,
            cursor_y: 0,
            lines: vec![
                TerminalLine::from_text("hello world", &attrs, 0, None),
                TerminalLine::from_text("second", &attrs, 0, None),
            ],
            mouse_reporting: false,
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
    fn visible_range_scrolls_only_over_projected_lines() {
        assert_eq!(visible_line_range(20, 5, 0), 15..20);
        assert_eq!(visible_line_range(20, 5, 3), 12..17);
        assert_eq!(visible_line_range(3, 5, usize::MAX), 0..3);
    }

    #[test]
    fn terminal_selection_maps_rendered_cells_to_authoritative_lines() {
        let attrs = CellAttributes::default();
        let terminal = TerminalView {
            pane_id: 9,
            title: "shell".to_owned(),
            cols: 8,
            rows: 2,
            cursor_x: 0,
            cursor_y: 0,
            mouse_reporting: false,
            lines: (0..6)
                .map(|index| TerminalLine::from_text(&format!("line-{index}"), &attrs, 0, None))
                .collect(),
        };
        let content = Rect::new(10, 4, 8, 2);

        assert_eq!(
            terminal_point(Some(&terminal), content, 0, Position { x: 12, y: 4 }),
            Some((4, 2))
        );
        assert_eq!(
            terminal_point(Some(&terminal), content, 2, Position { x: 10, y: 5 }),
            Some((3, 0))
        );
        assert_eq!(terminal_point(Some(&terminal), content, 0, Position { x: 9, y: 4 }), None);
    }
}
