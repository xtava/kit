use std::time::Duration;

use anyhow::{Context as _, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    layout::{Constraint, Layout, Margin, Position, Rect},
    style::{Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, Block, BorderType, Borders, Chart, Clear, Dataset, GraphType, List, ListItem,
        ListState, Padding, Paragraph, Wrap,
    },
    Frame,
};
use tokio::sync::mpsc;

use crate::tui::{
    theme::NORD, ActionId, ActionInvocation, ActionUnavailable, CommandPalette,
    CommandPaletteLayout, CommandPaletteOutcome, ContextMenu, ContextMenuLayout,
    ContextMenuOutcome, ContextMenuStyle, EventReader, KeyChord, KeybindingResolution,
    KeybindingState, NavigationMap, NavigationRegion, ResolvedAction, SelectableRegion,
    SelectionOutcome, Session, SessionOptions, SplitFrame, SplitMinimums, SplitRatio,
    TextSelection,
};

use super::{
    config::LoadedConfig,
    contributions::{
        self, ActionCapability, MonitorActionContext, MonitorActionRegistry, MonitorActionTarget,
        MonitorCommand, CORRELATION_INLINE, ITEM_CONTEXT, ITEM_INLINE, SCOPE_INLINE,
    },
    model::{
        CostSnapshot, HealthState, LogEvent, MetricSample, MetricSnapshot, MetricValue,
        MonitorSnapshot, MonitorView, ServiceSnapshot, SourceSnapshot, SourceState,
    },
    report,
    sources::{CollectionRequest, Collector},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const FOLLOW_INTERVAL: Duration = Duration::from_secs(3);
const WIDE_WIDTH: u16 = 120;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveRegion {
    Primary,
    Secondary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Flow {
    Continue,
    Copy(String),
    Quit,
    Refresh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MonitorSurface {
    Inspector,
}

enum MonitorOverlay {
    CommandPalette(CommandPalette<MonitorActionContext>),
    ContextMenu(ContextMenu<MonitorActionContext>),
    Help,
}

#[derive(Default)]
struct UiRegions {
    tabs: Vec<(Rect, MonitorView)>,
    primary: Option<Rect>,
    secondary: Option<Rect>,
    rows: Vec<(Rect, usize)>,
    inline_actions: Vec<(Rect, ActionId)>,
    command_palette: Option<CommandPaletteLayout>,
    context_menu: Option<ContextMenuLayout>,
    selectable: Vec<SelectableRegion<MonitorSurface>>,
}

impl UiRegions {
    fn navigation(&self) -> NavigationMap<ActiveRegion> {
        NavigationMap::new(
            [
                self.primary.map(|area| NavigationRegion::new(ActiveRegion::Primary, area)),
                self.secondary.map(|area| NavigationRegion::new(ActiveRegion::Secondary, area)),
            ]
            .into_iter()
            .flatten(),
        )
    }
}

struct App {
    config_path: String,
    snapshot: MonitorSnapshot,
    snapshot_generation: u64,
    registry: MonitorActionRegistry,
    keybinding_state: KeybindingState,
    view: MonitorView,
    active_region: ActiveRegion,
    selections: [usize; 7],
    filter: String,
    filtering: bool,
    refreshing: bool,
    follow_logs: bool,
    overlay: Option<MonitorOverlay>,
    notice: Option<String>,
    mouse: bool,
    selection: TextSelection<MonitorSurface>,
}

pub async fn run(
    loaded: LoadedConfig,
    environment: String,
    collector: Collector,
    mouse: bool,
) -> Result<()> {
    let registry = contributions::registry().context("build Monitor action registry")?;
    let initial_request = collection_request(&loaded, &environment, MonitorView::Overview);
    let snapshot = collector.collect(&loaded.config, &initial_request).await?;
    let mut app = App::new(snapshot, registry, mouse, loaded.path.display().to_string());
    let mut session =
        Session::open(SessionOptions { mouse_capture: mouse, bracketed_paste: false })?;
    let mut events = EventReader::start();
    let (refresh_sender, mut refresh_receiver) = mpsc::unbounded_channel();
    let mut interval = tokio::time::interval(REFRESH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    let mut follow_interval = tokio::time::interval(FOLLOW_INTERVAL);
    follow_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    follow_interval.tick().await;
    let mut regions = UiRegions::default();

    loop {
        session.draw(|frame| regions = render(frame, &mut app))?;
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    let flow = app.on_event(event, &regions);
                    match flow {
                        Flow::Copy(text) => session.copy(&text)?,
                        Flow::Quit => break,
                        Flow::Refresh => start_refresh(
                            &mut app,
                            &loaded,
                            &environment,
                            &collector,
                            refresh_sender.clone(),
                        ),
                        Flow::Continue => {}
                    }
                }
                None => break,
            },
            refreshed = refresh_receiver.recv() => {
                let Some(refreshed) = refreshed else { break };
                app.refreshing = false;
                match refreshed {
                    Ok(snapshot) => app.replace_snapshot(snapshot),
                    Err(detail) => app.notice = Some(detail),
                }
            },
            _ = interval.tick() => {
                if !app.refreshing {
                    start_refresh(
                        &mut app,
                        &loaded,
                        &environment,
                        &collector,
                        refresh_sender.clone(),
                    );
                }
            }
            _ = follow_interval.tick(), if app.follow_logs => {
                if !app.refreshing {
                    start_refresh(
                        &mut app,
                        &loaded,
                        &environment,
                        &collector,
                        refresh_sender.clone(),
                    );
                }
            }
        }
    }
    Ok(())
}

fn start_refresh(
    app: &mut App,
    loaded: &LoadedConfig,
    environment: &str,
    collector: &Collector,
    sender: mpsc::UnboundedSender<Result<MonitorSnapshot, String>>,
) {
    if app.refreshing {
        app.notice = Some("Refresh already in progress".to_owned());
        return;
    }
    app.refreshing = true;
    app.notice = None;
    let request = collection_request(loaded, environment, app.view);
    let config = loaded.config.clone();
    let collector = collector.clone();
    tokio::spawn(async move {
        let outcome = collector
            .collect(&config, &request)
            .await
            .map_err(|error| format!("Refresh failed: {error:#}"));
        let _ = sender.send(outcome);
    });
}

fn collection_request(
    loaded: &LoadedConfig,
    environment: &str,
    view: MonitorView,
) -> CollectionRequest {
    CollectionRequest {
        environment: environment.to_owned(),
        include_logs: view == MonitorView::Logs,
        log_service: None,
        log_lookback_secs: 30 * 60,
        log_limit: loaded.config.limits.max_log_events.min(200),
    }
}

impl App {
    fn new(
        snapshot: MonitorSnapshot,
        registry: MonitorActionRegistry,
        mouse: bool,
        config_path: String,
    ) -> Self {
        Self {
            config_path,
            snapshot,
            snapshot_generation: 1,
            registry,
            keybinding_state: KeybindingState::default(),
            view: MonitorView::Overview,
            active_region: ActiveRegion::Primary,
            selections: [0; 7],
            filter: String::new(),
            filtering: false,
            refreshing: false,
            follow_logs: false,
            overlay: None,
            notice: None,
            mouse,
            selection: TextSelection::default(),
        }
    }

    fn on_event(&mut self, event: Event, regions: &UiRegions) -> Flow {
        if self.overlay.is_none() {
            if let Event::Key(key) = &event {
                match self.selection.on_key(*key) {
                    SelectionOutcome::CopyReady(text) => return Flow::Copy(text),
                    SelectionOutcome::Captured | SelectionOutcome::Changed => {
                        return Flow::Continue
                    }
                    SelectionOutcome::Unhandled | SelectionOutcome::EdgeScroll { .. } => {}
                }
            }
        }
        if matches!(
            &event,
            Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            })
        ) {
            return Flow::Quit;
        }
        if self.overlay.is_some() {
            return self.on_overlay_event(event, regions);
        }
        match event {
            Event::Key(key) if key.is_press() => self.on_key(key, regions),
            Event::Mouse(mouse) if self.mouse => {
                if self.selection.is_dragging() {
                    return self.on_selection_mouse(mouse);
                }
                self.on_mouse(mouse, regions)
            }
            Event::Resize(_, _) => Flow::Continue,
            _ => Flow::Continue,
        }
    }

    fn on_overlay_event(&mut self, event: Event, regions: &UiRegions) -> Flow {
        if matches!(self.overlay, Some(MonitorOverlay::CommandPalette(_))) {
            let Some(layout) = regions.command_palette.as_ref() else {
                self.overlay = None;
                return Flow::Continue;
            };
            let outcome = match self.overlay.as_mut() {
                Some(MonitorOverlay::CommandPalette(palette)) => palette.on_event(event, layout),
                _ => unreachable!("command palette overlay checked above"),
            };
            return match outcome {
                CommandPaletteOutcome::Captured => Flow::Continue,
                CommandPaletteOutcome::Dismissed => {
                    self.overlay = None;
                    Flow::Continue
                }
                CommandPaletteOutcome::Invoke(invocation) => {
                    self.overlay = None;
                    self.invoke_action(invocation)
                }
            };
        }
        let outcome = match self.overlay.as_mut() {
            Some(MonitorOverlay::CommandPalette(_)) => {
                unreachable!("command palette events return above")
            }
            Some(MonitorOverlay::ContextMenu(menu)) => {
                let layout = regions.context_menu.clone().unwrap_or_default();
                Some(menu.on_event(event, &layout))
            }
            Some(MonitorOverlay::Help) => {
                if matches!(
                    event,
                    Event::Key(KeyEvent {
                        code: KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q'),
                        ..
                    }) | Event::Mouse(MouseEvent { kind: MouseEventKind::Down(_), .. })
                ) {
                    self.overlay = None;
                }
                None
            }
            None => None,
        };
        match outcome {
            Some(ContextMenuOutcome::Dismissed) => {
                self.overlay = None;
                Flow::Continue
            }
            Some(ContextMenuOutcome::Unavailable { reason, .. }) => {
                self.notice = Some(reason.into_owned());
                self.overlay = None;
                Flow::Continue
            }
            Some(ContextMenuOutcome::Invoke(invocation)) => {
                self.overlay = None;
                self.invoke_action(invocation)
            }
            Some(ContextMenuOutcome::Captured) | None => Flow::Continue,
        }
    }

    fn on_key(&mut self, key: KeyEvent, regions: &UiRegions) -> Flow {
        if self.filtering {
            let command_palette = KeyChord::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
            if KeyChord::from_event(key) == Some(command_palette) {
                let context = self.action_context();
                match self.registry.resolve_keybinding(
                    &mut self.keybinding_state,
                    command_palette,
                    context,
                ) {
                    KeybindingResolution::Invoke(invocation) => {
                        return self.invoke_action(invocation)
                    }
                    KeybindingResolution::Pending => return Flow::Continue,
                    KeybindingResolution::Unmatched
                    | KeybindingResolution::UnmatchedSequence { .. } => {}
                }
            }
            match key.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.set_selection(0);
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.set_selection(0);
                }
                KeyCode::Char(character)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.filter.push(character);
                    self.set_selection(0);
                }
                _ => {}
            }
            return Flow::Continue;
        }

        if let Some(chord) = KeyChord::from_event(key) {
            let context = self.action_context();
            match self.registry.resolve_keybinding(&mut self.keybinding_state, chord, context) {
                KeybindingResolution::Invoke(invocation) => return self.invoke_action(invocation),
                KeybindingResolution::Pending => return Flow::Continue,
                KeybindingResolution::Unmatched
                | KeybindingResolution::UnmatchedSequence { .. } => {}
            }
        }

        match key.code {
            KeyCode::Char('q') => Flow::Quit,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.set_selection(0);
                    Flow::Continue
                } else if self.active_region == ActiveRegion::Secondary {
                    self.active_region = ActiveRegion::Primary;
                    Flow::Continue
                } else if self.view != MonitorView::Overview {
                    self.set_view(MonitorView::Overview)
                } else {
                    Flow::Quit
                }
            }
            KeyCode::Char('/') => {
                self.selection.clear();
                self.filtering = true;
                Flow::Continue
            }
            KeyCode::Char('?') => {
                self.selection.clear();
                self.overlay = Some(MonitorOverlay::Help);
                Flow::Continue
            }
            KeyCode::Char(character @ '1'..='7') => {
                let index = usize::from(character as u8 - b'1');
                self.set_view(MonitorView::ALL[index])
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Flow::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Flow::Continue
            }
            KeyCode::PageUp => {
                self.move_selection(-10);
                Flow::Continue
            }
            KeyCode::PageDown => {
                self.move_selection(10);
                Flow::Continue
            }
            KeyCode::Home => {
                self.set_selection(0);
                Flow::Continue
            }
            KeyCode::End => {
                self.set_selection(self.visible_indices().len().saturating_sub(1));
                Flow::Continue
            }
            KeyCode::Tab => {
                if let Some(region) = regions.navigation().next(self.active_region) {
                    self.active_region = region;
                }
                Flow::Continue
            }
            KeyCode::BackTab => {
                if let Some(region) = regions.navigation().previous(self.active_region) {
                    self.active_region = region;
                }
                Flow::Continue
            }
            KeyCode::Left => {
                if let Some(region) =
                    regions.navigation().neighbor(self.active_region, crate::tui::Direction::Left)
                {
                    self.active_region = region;
                }
                Flow::Continue
            }
            KeyCode::Right => {
                if let Some(region) =
                    regions.navigation().neighbor(self.active_region, crate::tui::Direction::Right)
                {
                    self.active_region = region;
                }
                Flow::Continue
            }
            _ => Flow::Continue,
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent, regions: &UiRegions) -> Flow {
        let position = Position { x: mouse.column, y: mouse.row };
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some((_, action)) =
                regions.inline_actions.iter().find(|(area, _)| area.contains(position))
            {
                return self.invoke_action(ActionInvocation::new(*action, self.action_context()));
            }
            if let Some((_, view)) = regions.tabs.iter().find(|(area, _)| area.contains(position)) {
                return self.set_view(*view);
            }
            if let Some((_, index)) = regions.rows.iter().find(|(area, _)| area.contains(position))
            {
                self.set_selection(*index);
                self.active_region = ActiveRegion::Primary;
                self.selection.clear();
                return Flow::Continue;
            }
            if regions.secondary.is_some_and(|area| area.contains(position)) {
                self.active_region = ActiveRegion::Secondary;
            } else if regions.primary.is_some_and(|area| area.contains(position)) {
                self.active_region = ActiveRegion::Primary;
            }
        }
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right)) {
            if let Some((_, index)) = regions.rows.iter().find(|(area, _)| area.contains(position))
            {
                self.set_selection(*index);
                let context = self.action_context();
                let items = self.registry.resolve_menu(ITEM_CONTEXT, &context);
                self.overlay =
                    ContextMenu::open(position, context, items).map(MonitorOverlay::ContextMenu);
                self.selection.clear();
            }
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_selection(-3),
            MouseEventKind::ScrollDown => self.move_selection(3),
            _ => {}
        }
        self.on_selection_mouse(mouse)
    }

    fn on_selection_mouse(&mut self, mouse: MouseEvent) -> Flow {
        match self.selection.on_mouse(mouse) {
            SelectionOutcome::CopyReady(text) => Flow::Copy(text),
            SelectionOutcome::Captured
            | SelectionOutcome::Changed
            | SelectionOutcome::Unhandled
            | SelectionOutcome::EdgeScroll { .. } => Flow::Continue,
        }
    }

    fn invoke_action(&mut self, invocation: ActionInvocation<MonitorActionContext>) -> Flow {
        if invocation.context.snapshot_generation != self.snapshot_generation {
            self.notice =
                Some("That item belongs to an older snapshot; refresh the selection".into());
            return Flow::Continue;
        }
        let command = match self.registry.command_for(&invocation) {
            Ok(command) => command,
            Err(ActionUnavailable::Disabled { reason, .. }) => {
                self.notice = Some(reason.into_owned());
                return Flow::Continue;
            }
            Err(ActionUnavailable::Unknown { action }) => {
                self.notice = Some(format!("Unknown action {action}"));
                return Flow::Continue;
            }
        };
        match command {
            MonitorCommand::OpenCommandPalette => {
                self.selection.clear();
                self.overlay = Some(MonitorOverlay::CommandPalette(CommandPalette::open(
                    invocation.context,
                    &self.registry,
                )));
                Flow::Continue
            }
            MonitorCommand::Inspect => {
                self.active_region = ActiveRegion::Secondary;
                Flow::Continue
            }
            MonitorCommand::Refresh => Flow::Refresh,
            MonitorCommand::ToggleLogFollow => {
                self.follow_logs = !self.follow_logs;
                self.notice = Some(if self.follow_logs {
                    format!("Following logs every {}s", FOLLOW_INTERVAL.as_secs())
                } else {
                    "Log follow paused".to_owned()
                });
                if self.follow_logs {
                    Flow::Refresh
                } else {
                    Flow::Continue
                }
            }
            MonitorCommand::OpenExternal
            | MonitorCommand::OpenInDeploy
            | MonitorCommand::OpenTraceCorrelation
            | MonitorCommand::OpenMetricsCorrelation
            | MonitorCommand::OpenDeploymentCorrelation => {
                self.notice = Some("This source does not publish that handoff capability".into());
                Flow::Continue
            }
        }
    }

    fn action_context(&self) -> MonitorActionContext {
        let target = self.selected_target();
        let inspectable = self.view != MonitorView::Overview && !self.visible_indices().is_empty();
        MonitorActionContext {
            view: self.view,
            target,
            snapshot_generation: self.snapshot_generation,
            inspectable,
            refreshable: true,
            external_open: ActionCapability::Unavailable("provider URL is not available"),
            deploy_handoff: ActionCapability::Unavailable(
                "deployment is not mapped to a kit deploy target",
            ),
            follow: if self.view == MonitorView::Logs
                && self.snapshot.logs.state == SourceState::Ready
            {
                ActionCapability::Available
            } else {
                ActionCapability::Unavailable("a ready Loki query is required")
            },
            trace_correlation: ActionCapability::Unavailable("trace correlation is unavailable"),
            metrics_correlation: ActionCapability::Unavailable("metric correlation is unavailable"),
            deployment_correlation: ActionCapability::Unavailable(
                "deployment correlation is unavailable",
            ),
        }
    }

    fn selected_target(&self) -> MonitorActionTarget {
        let indices = self.visible_indices();
        let data_index = indices.get(self.selection()).copied();
        match (self.view, data_index) {
            (MonitorView::Overview, _) => MonitorActionTarget::Overview,
            (MonitorView::Services, Some(index)) => {
                MonitorActionTarget::Service(self.snapshot.services[index].id.clone())
            }
            (MonitorView::Performance, Some(index)) => {
                let metric = &self.snapshot.performance[index];
                MonitorActionTarget::Metric {
                    service_id: metric.service_id.clone(),
                    metric_id: metric.id.clone(),
                }
            }
            (MonitorView::Logs, Some(index)) => {
                MonitorActionTarget::LogEvent(self.snapshot.logs.events[index].id.clone())
            }
            (MonitorView::Deployments, Some(index)) => {
                MonitorActionTarget::Deployment(self.snapshot.deployments.entries[index].id.clone())
            }
            (MonitorView::Costs, Some(index)) => {
                MonitorActionTarget::Cost(self.snapshot.costs.items[index].id.clone())
            }
            (MonitorView::Sources, Some(index)) => {
                MonitorActionTarget::Source(self.snapshot.sources[index].id.clone())
            }
            _ => MonitorActionTarget::Overview,
        }
    }

    fn set_view(&mut self, view: MonitorView) -> Flow {
        let changed = self.view != view;
        self.view = view;
        self.active_region = ActiveRegion::Primary;
        self.filter.clear();
        self.clamp_selection();
        if changed && view == MonitorView::Logs && self.snapshot.logs.state != SourceState::Ready {
            Flow::Refresh
        } else {
            Flow::Continue
        }
    }

    fn replace_snapshot(&mut self, snapshot: MonitorSnapshot) {
        let selected = self.selected_identity();
        self.snapshot = snapshot;
        self.snapshot_generation = self.snapshot_generation.saturating_add(1);
        if let Some(selected) = selected {
            let indices = self.visible_indices();
            if let Some(position) = indices
                .iter()
                .position(|index| self.identity_at(*index).as_deref() == Some(selected.as_str()))
            {
                self.set_selection(position);
            } else {
                self.clamp_selection();
            }
        } else {
            self.clamp_selection();
        }
    }

    fn selected_identity(&self) -> Option<String> {
        let index = self.visible_indices().get(self.selection()).copied()?;
        self.identity_at(index)
    }

    fn identity_at(&self, index: usize) -> Option<String> {
        match self.view {
            MonitorView::Overview => None,
            MonitorView::Services => self.snapshot.services.get(index).map(|item| item.id.clone()),
            MonitorView::Performance => self
                .snapshot
                .performance
                .get(index)
                .map(|item| format!("{}:{}", item.service_id, item.id)),
            MonitorView::Logs => self.snapshot.logs.events.get(index).map(|item| item.id.clone()),
            MonitorView::Deployments => {
                self.snapshot.deployments.entries.get(index).map(|item| item.id.clone())
            }
            MonitorView::Costs => self.snapshot.costs.items.get(index).map(|item| item.id.clone()),
            MonitorView::Sources => self.snapshot.sources.get(index).map(|item| item.id.clone()),
        }
    }

    fn visible_indices(&self) -> Vec<usize> {
        let filter = self.filter.to_ascii_lowercase();
        match self.view {
            MonitorView::Overview => Vec::new(),
            MonitorView::Services => self
                .snapshot
                .services
                .iter()
                .enumerate()
                .filter(|(_, item)| matches_filter(&filter, &[&item.id, &item.name, &item.reason]))
                .map(|(index, _)| index)
                .collect(),
            MonitorView::Performance => self
                .snapshot
                .performance
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    matches_filter(&filter, &[&item.id, &item.name, &item.service_id])
                })
                .map(|(index, _)| index)
                .collect(),
            MonitorView::Logs => self
                .snapshot
                .logs
                .events
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    matches_filter(&filter, &[&item.service_id, &item.level, &item.message])
                })
                .map(|(index, _)| index)
                .collect(),
            MonitorView::Deployments => self
                .snapshot
                .deployments
                .entries
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    matches_filter(&filter, &[&item.service_id, &item.version, &item.status])
                })
                .map(|(index, _)| index)
                .collect(),
            MonitorView::Costs => self
                .snapshot
                .costs
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| matches_filter(&filter, &[&item.name, &item.provider]))
                .map(|(index, _)| index)
                .collect(),
            MonitorView::Sources => self
                .snapshot
                .sources
                .iter()
                .enumerate()
                .filter(|(_, item)| {
                    matches_filter(&filter, &[&item.id, &item.name, &item.kind, &item.detail])
                })
                .map(|(index, _)| index)
                .collect(),
        }
    }

    fn selection(&self) -> usize {
        self.selections[view_index(self.view)]
    }

    fn set_selection(&mut self, selection: usize) {
        self.selections[view_index(self.view)] = selection;
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        let max = self.visible_indices().len().saturating_sub(1);
        let index = view_index(self.view);
        self.selections[index] = self.selections[index].min(max);
    }

    fn move_selection(&mut self, delta: isize) {
        if self.active_region != ActiveRegion::Primary {
            return;
        }
        let len = self.visible_indices().len();
        if len == 0 {
            return;
        }
        self.set_selection((self.selection() as isize + delta).clamp(0, len as isize - 1) as usize);
    }
}

fn render(frame: &mut Frame<'_>, app: &mut App) -> UiRegions {
    frame.render_widget(Block::new().style(Style::default().bg(NORD.background)), frame.area());
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(8),
        Constraint::Length(if app.notice.is_some() { 2 } else { 1 }),
    ])
    .split(frame.area());
    let mut regions = UiRegions::default();
    render_tabs(frame, rows[0], app, &mut regions);
    render_context(frame, rows[1], app);
    if app.view == MonitorView::Overview {
        regions.primary = Some(rows[2]);
        render_overview(frame, rows[2], app);
    } else {
        render_detail_view(frame, rows[2], app, &mut regions);
    }
    render_footer(frame, rows[3], app, &mut regions);
    match app.overlay.as_ref() {
        Some(MonitorOverlay::CommandPalette(palette)) => {
            let layout = palette.layout(frame.area());
            palette.render(frame, &layout, NORD);
            regions.command_palette = Some(layout);
        }
        Some(MonitorOverlay::ContextMenu(menu)) => {
            let layout = menu.layout(frame.area());
            menu.render(frame, &layout, ContextMenuStyle::from_theme(NORD));
            regions.context_menu = Some(layout);
        }
        Some(MonitorOverlay::Help) => render_help(frame, app),
        None => {}
    }
    if app.overlay.is_some() {
        regions.selectable.clear();
    }
    let selectable = regions.selectable.clone();
    app.selection.capture_frame(
        frame,
        &selectable,
        Style::default().bg(NORD.selection).add_modifier(Modifier::REVERSED),
    );
    regions
}

fn render_tabs(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let mut spans = Vec::new();
    let mut x = area.x;
    for view in MonitorView::ALL {
        let label = format!(" {} ", view.label());
        let width = label.len() as u16;
        let selected = view == app.view;
        spans.push(Span::styled(
            label,
            Style::default()
                .fg(if selected { NORD.text_strong } else { NORD.text_muted })
                .bg(if selected { NORD.selection } else { NORD.background })
                .add_modifier(if selected { Modifier::BOLD } else { Modifier::empty() }),
        ));
        regions.tabs.push((Rect::new(x, area.y, width, 1), view));
        x = x.saturating_add(width);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_context(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let ready =
        app.snapshot.sources.iter().filter(|source| source.state == SourceState::Ready).count();
    let age = unix_time_secs().saturating_sub(app.snapshot.observed_at_secs);
    let mut spans = vec![
        Span::styled(
            format!(" {} ", app.snapshot.environment.name.to_ascii_uppercase()),
            Style::default().fg(NORD.accent_alt).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", report::health_label(app.snapshot.health.state)),
            Style::default()
                .fg(health_color(app.snapshot.health.state))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {ready}/{} SOURCES ", app.snapshot.sources.len()),
            Style::default().fg(NORD.text),
        ),
        Span::styled(format!(" UPDATED {age}s ago "), Style::default().fg(NORD.text_muted)),
    ];
    if app.refreshing {
        spans.push(Span::styled(" REFRESHING ", Style::default().fg(NORD.warning)));
    }
    if app.follow_logs {
        spans.push(Span::styled(" FOLLOWING ", Style::default().fg(NORD.success)));
    }
    if app.filtering || !app.filter.is_empty() {
        spans.push(Span::styled(
            format!(" FILTER /{}{} ", app.filter, if app.filtering { "▌" } else { "" }),
            Style::default().fg(NORD.accent),
        ));
    }
    if area.width >= 150 {
        spans.push(Span::styled(
            format!(" {} ", app.config_path),
            Style::default().fg(NORD.text_muted).add_modifier(Modifier::DIM),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(NORD.surface)),
        area,
    );
}

fn render_overview(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = Layout::vertical([
        Constraint::Percentage(34),
        Constraint::Percentage(40),
        Constraint::Percentage(26),
    ])
    .split(area);
    let top =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[0]);
    let middle =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).split(rows[1]);
    let bottom =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[2]);
    let health_lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{} ", state_glyph(app.snapshot.health.state)),
                Style::default().fg(health_color(app.snapshot.health.state)),
            ),
            Span::styled(
                report::health_label(app.snapshot.health.state),
                Style::default()
                    .fg(health_color(app.snapshot.health.state))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::styled(
            format!(
                "{} healthy · {} degraded · {} incident · {} unknown",
                app.snapshot.health.healthy_services,
                app.snapshot.health.degraded_services,
                app.snapshot.health.incident_services,
                app.snapshot.health.unknown_services
            ),
            Style::default().fg(NORD.text),
        ),
    ];
    frame.render_widget(Paragraph::new(health_lines).block(panel(" HEALTH ", false)), top[0]);
    let source_lines = app
        .snapshot
        .sources
        .iter()
        .map(|source| {
            Line::from(vec![
                Span::styled(
                    format!("{} ", source_glyph(source.state)),
                    Style::default().fg(source_color(source.state)),
                ),
                Span::styled(format!("{:<18}", source.name), Style::default().fg(NORD.text)),
                Span::styled(source.detail.clone(), Style::default().fg(NORD.text_muted)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(source_lines).block(panel(" SOURCES ", false)), top[1]);
    let service_lines = app
        .snapshot
        .services
        .iter()
        .map(|service| {
            Line::from(vec![
                Span::styled(
                    format!("{} ", state_glyph(service.state)),
                    Style::default().fg(health_color(service.state)),
                ),
                Span::styled(
                    format!("{:<18}", service.name),
                    Style::default().fg(NORD.text_strong),
                ),
                Span::styled(
                    format!("{:>5}ms  {}", service.latency_ms, service.reason),
                    Style::default().fg(NORD.text_muted),
                ),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(service_lines).block(panel(" SERVICES ", false)), middle[0]);
    let metric_lines = app
        .snapshot
        .performance
        .iter()
        .take(8)
        .map(|metric| {
            let value = report::metric_value(&metric.value, metric.unit);
            Line::from(vec![
                Span::styled(format!("{:<16}", metric.name), Style::default().fg(NORD.text)),
                Span::styled(
                    format!("{:>9} ", value),
                    Style::default().fg(health_color(metric.state)).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    compact_sparkline(&metric.samples, 12),
                    Style::default().fg(health_color(metric.state)),
                ),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(metric_lines).block(panel(" PRESSURE ", false)), middle[1]);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!("${:.2} / month", app.snapshot.costs.monthly_total),
                Style::default().fg(NORD.special).add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                app.snapshot
                    .costs
                    .monthly_budget
                    .map(|budget| {
                        format!(
                            "{:.0}% of ${budget:.2} budget",
                            app.snapshot.costs.budget_percent.unwrap_or_default()
                        )
                    })
                    .unwrap_or_else(|| "No monthly budget configured".to_owned()),
                Style::default().fg(NORD.warning),
            ),
            Line::styled(app.snapshot.costs.detail.clone(), Style::default().fg(NORD.text_muted)),
        ])
        .block(panel(" COST ", false)),
        bottom[0],
    );
    let attention = if app.snapshot.warnings.is_empty() {
        vec![Line::styled("No active warnings", Style::default().fg(NORD.success))]
    } else {
        app.snapshot
            .warnings
            .iter()
            .take(4)
            .map(|warning| Line::styled(format!("! {warning}"), Style::default().fg(NORD.warning)))
            .collect()
    };
    frame.render_widget(Paragraph::new(attention).block(panel(" ATTENTION ", false)), bottom[1]);
}

fn render_detail_view(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let wide = area.width >= WIDE_WIDTH;
    if wide {
        let split = SplitFrame::horizontal(area, SplitRatio::new(590), SplitMinimums::new(42, 38));
        regions.primary = Some(split.first);
        regions.secondary = Some(split.second);
        render_primary(frame, split.first, app, regions);
        render_inspector(frame, split.second, app, false, regions);
        frame.render_widget(
            Paragraph::new("┋".repeat(usize::from(split.separator.height)))
                .style(Style::default().fg(NORD.border)),
            split.separator,
        );
    } else if app.active_region == ActiveRegion::Secondary {
        regions.secondary = Some(area);
        render_inspector(frame, area, app, true, regions);
    } else {
        regions.primary = Some(area);
        render_primary(frame, area, app, regions);
    }
}

fn render_primary(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let indices = app.visible_indices();
    let items =
        indices.iter().map(|index| ListItem::new(primary_line(app, *index))).collect::<Vec<_>>();
    let mut state =
        ListState::default().with_selected((!items.is_empty()).then_some(app.selection()));
    frame.render_stateful_widget(
        List::new(items)
            .block(active_panel(
                &format!(" {} · {} ", app.view.label().to_ascii_uppercase(), indices.len()),
                app.active_region == ActiveRegion::Primary,
            ))
            .highlight_style(Style::default().bg(NORD.selection).add_modifier(Modifier::BOLD))
            .highlight_symbol("▌"),
        area,
        &mut state,
    );
    let inner = area.inner(Margin { horizontal: 1, vertical: 1 });
    let visible_start = state.offset();
    for (row, _) in indices.iter().skip(visible_start).take(usize::from(inner.height)).enumerate() {
        regions
            .rows
            .push((Rect::new(inner.x, inner.y + row as u16, inner.width, 1), row + visible_start));
    }
}

fn primary_line(app: &App, index: usize) -> Line<'static> {
    match app.view {
        MonitorView::Services => service_line(&app.snapshot.services[index]),
        MonitorView::Performance => metric_line(&app.snapshot.performance[index]),
        MonitorView::Logs => log_line(&app.snapshot.logs.events[index]),
        MonitorView::Deployments => {
            let item = &app.snapshot.deployments.entries[index];
            Line::raw(format!("{:<18} {:<12} {}", item.service_id, item.status, item.version))
        }
        MonitorView::Costs => cost_line(&app.snapshot.costs.items[index]),
        MonitorView::Sources => source_line(&app.snapshot.sources[index]),
        MonitorView::Overview => Line::raw(""),
    }
}

fn render_inspector(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    compact: bool,
    regions: &mut UiRegions,
) {
    let title = if compact { " DETAIL · Tab returns " } else { " DETAIL " };
    let lines = inspector_lines(app);
    let block = active_panel(title, app.active_region == ActiveRegion::Secondary);
    let inner = block.inner(area);
    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
    frame.render_widget(block, area);
    if app.view == MonitorView::Performance {
        render_metric_inspector(frame, chunks[0], app);
    } else {
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[0]);
        regions.selectable.push(SelectableRegion::new(
            MonitorSurface::Inspector,
            chunks[0],
            0,
            0,
            app.snapshot_generation,
        ));
    }
    let context = app.action_context();
    let mut actions = app.registry.resolve_menu(ITEM_INLINE, &context).items().to_vec();
    if app.view == MonitorView::Logs {
        actions.extend_from_slice(app.registry.resolve_menu(CORRELATION_INLINE, &context).items());
    }
    render_inline_actions(frame, chunks[1], &actions, regions);
}

fn render_metric_inspector(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(index) = app.visible_indices().get(app.selection()).copied() else {
        frame.render_widget(
            Paragraph::new(empty_detail(app)).style(Style::default().fg(NORD.text_muted)),
            area,
        );
        return;
    };
    let metric = &app.snapshot.performance[index];
    let rows = Layout::vertical([Constraint::Length(6), Constraint::Min(4)]).split(area);
    let evidence = match &metric.value {
        MetricValue::Unavailable(detail) => detail.clone(),
        _ if metric.samples.len() >= 2 => {
            format!("{} samples · 5m resolution", metric.samples.len())
        }
        _ => "instant query · no history collected".to_owned(),
    };
    frame.render_widget(
        Paragraph::new(compact_detail_lines(&[
            ("Metric", metric.name.clone()),
            ("Service", metric.service_id.clone()),
            ("Current", report::metric_value(&metric.value, metric.unit)),
            ("State", report::health_label(metric.state).to_owned()),
            ("Source", metric.source_id.clone()),
            ("Evidence", evidence),
        ])),
        rows[0],
    );
    render_metric_chart(frame, rows[1], metric);
}

fn render_metric_chart(frame: &mut Frame<'_>, area: Rect, metric: &MetricSnapshot) {
    if metric.samples.len() < 2 {
        frame.render_widget(
            Paragraph::new("No time-series evidence collected")
                .style(Style::default().fg(NORD.text_muted))
                .block(panel(" TREND ", false)),
            area,
        );
        return;
    }
    let first = metric.samples.first().map_or(0, |sample| sample.observed_at_secs);
    let points = metric
        .samples
        .iter()
        .map(|sample| (sample.observed_at_secs.saturating_sub(first) as f64 / 60.0, sample.value))
        .collect::<Vec<_>>();
    let (minimum, maximum) = metric_bounds(&metric.samples);
    let padding = ((maximum - minimum) * 0.12).max(maximum.abs() * 0.02).max(0.001);
    let minimum_label = report::metric_value(&MetricValue::Available(minimum), metric.unit);
    let maximum_label = report::metric_value(&MetricValue::Available(maximum), metric.unit);
    let duration_minutes = points.last().map_or(0.0, |point| point.0);
    let dataset = Dataset::default()
        .marker(symbols::Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(health_color(metric.state)))
        .data(&points);
    let chart = Chart::new(vec![dataset])
        .block(panel(" 1H TREND ", false))
        .x_axis(
            Axis::default()
                .style(Style::default().fg(NORD.border))
                .bounds([0.0, duration_minutes.max(1.0)])
                .labels([format!("-{duration_minutes:.0}m"), "now".to_owned()]),
        )
        .y_axis(
            Axis::default()
                .style(Style::default().fg(NORD.border))
                .bounds([minimum - padding, maximum + padding])
                .labels([minimum_label, maximum_label]),
        );
    frame.render_widget(chart, area);
}

fn inspector_lines(app: &App) -> Vec<Line<'static>> {
    let Some(index) = app.visible_indices().get(app.selection()).copied() else {
        return vec![Line::styled(empty_detail(app), Style::default().fg(NORD.text_muted))];
    };
    match app.view {
        MonitorView::Services => {
            let item = &app.snapshot.services[index];
            detail_lines(&[
                ("Service", item.name.clone()),
                ("State", report::health_label(item.state).to_owned()),
                ("Evidence", item.reason.clone()),
                ("Source", item.source_id.clone()),
                ("Latency", format!("{}ms", item.latency_ms)),
                ("Observed", item.observed_at_secs.to_string()),
            ])
        }
        MonitorView::Performance => {
            let item = &app.snapshot.performance[index];
            let detail = match &item.value {
                MetricValue::Unavailable(detail) => detail.clone(),
                _ => "Prometheus instant query".to_owned(),
            };
            detail_lines(&[
                ("Metric", item.name.clone()),
                ("Service", item.service_id.clone()),
                ("Value", report::metric_value(&item.value, item.unit)),
                ("State", report::health_label(item.state).to_owned()),
                ("Source", item.source_id.clone()),
                ("Detail", detail),
            ])
        }
        MonitorView::Logs => {
            let item = &app.snapshot.logs.events[index];
            detail_lines(&[
                ("Timestamp", item.timestamp_ns.clone()),
                ("Service", item.service_id.clone()),
                ("Level", item.level.clone()),
                ("Source", item.source_id.clone()),
                ("Redacted", item.redacted_fields.to_string()),
                ("Message", item.message.clone()),
            ])
        }
        MonitorView::Deployments => {
            let item = &app.snapshot.deployments.entries[index];
            detail_lines(&[
                ("Service", item.service_id.clone()),
                ("Version", item.version.clone()),
                ("Status", item.status.clone()),
                ("Deployed", item.deployed_at_secs.to_string()),
                ("Duration", format!("{}ms", item.duration_ms)),
            ])
        }
        MonitorView::Costs => {
            let item = &app.snapshot.costs.items[index];
            detail_lines(&[
                ("Item", item.name.clone()),
                ("Provider", item.provider.clone()),
                ("Monthly", format!("${:.2}", item.monthly_usd)),
                ("Confidence", format!("{:?}", item.confidence)),
                ("Scope", item.service_id.clone().unwrap_or_else(|| "environment".into())),
                ("Coverage", app.snapshot.costs.detail.clone()),
            ])
        }
        MonitorView::Sources => {
            let item = &app.snapshot.sources[index];
            detail_lines(&[
                ("Source", item.name.clone()),
                ("Type", item.kind.clone()),
                ("State", report::source_label(item.state).to_owned()),
                ("Required", item.required.to_string()),
                ("Latency", format!("{}ms", item.latency_ms)),
                ("Detail", item.detail.clone()),
            ])
        }
        MonitorView::Overview => Vec::new(),
    }
}

fn render_inline_actions(
    frame: &mut Frame<'_>,
    area: Rect,
    actions: &[ResolvedAction],
    regions: &mut UiRegions,
) {
    let mut spans = Vec::new();
    let mut x = area.x;
    for action in actions {
        let label = inline_action_label(action);
        let requested_width = label.chars().count() as u16;
        let width = requested_width.min(area.right().saturating_sub(x));
        if width == 0 {
            break;
        }
        spans.push(Span::styled(
            label,
            Style::default().fg(match action.state {
                crate::tui::ActionState::Enabled => NORD.accent,
                crate::tui::ActionState::Disabled { .. } => NORD.text_muted,
            }),
        ));
        regions.inline_actions.push((Rect::new(x, area.y, width, 1), action.id));
        x = x.saturating_add(width);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn inline_action_label(action: &ResolvedAction) -> String {
    format!(" {} {} ", action_shortcut(action), action.title)
}

fn action_shortcut(action: &ResolvedAction) -> String {
    action.primary_keybinding().map_or_else(|| "·".to_owned(), |key| key.to_string())
}

fn line_width(spans: &[Span<'_>]) -> u16 {
    spans
        .iter()
        .map(|span| u16::try_from(span.content.chars().count()).unwrap_or(u16::MAX))
        .fold(0, u16::saturating_add)
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App, regions: &mut UiRegions) {
    let mut spans = vec![
        Span::styled(" 1–7 ", Style::default().fg(NORD.accent)),
        Span::styled("views  ", Style::default().fg(NORD.text_muted)),
        Span::styled("Tab ", Style::default().fg(NORD.accent)),
        Span::styled("regions  ", Style::default().fg(NORD.text_muted)),
        Span::styled("/ ", Style::default().fg(NORD.accent)),
        Span::styled("filter  ", Style::default().fg(NORD.text_muted)),
        Span::styled("Ctrl-P ", Style::default().fg(NORD.accent)),
        Span::styled("commands  ", Style::default().fg(NORD.text_muted)),
        Span::styled("? ", Style::default().fg(NORD.accent)),
        Span::styled("help  ", Style::default().fg(NORD.text_muted)),
        Span::styled("q ", Style::default().fg(NORD.accent)),
        Span::styled("quit", Style::default().fg(NORD.text_muted)),
    ];
    let context = app.action_context();
    let mut actions = app.registry.resolve_menu(SCOPE_INLINE, &context).items().to_vec();
    actions.extend_from_slice(app.registry.resolve_menu(ITEM_INLINE, &context).items());
    let action_row = if app.notice.is_some() { area.y.saturating_add(1) } else { area.y };
    let mut x = area.x.saturating_add(line_width(&spans));
    for action in &actions {
        let label = inline_action_label(action);
        let width = label.chars().count() as u16;
        spans.push(Span::styled(
            label,
            Style::default().fg(match action.state {
                crate::tui::ActionState::Enabled => NORD.accent,
                crate::tui::ActionState::Disabled { .. } => NORD.text_muted,
            }),
        ));
        regions.inline_actions.push((Rect::new(x, action_row, width, 1), action.id));
        x = x.saturating_add(width);
    }
    let footer = Line::from(spans);
    if let Some(notice) = &app.notice {
        let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {notice}"), Style::default().fg(NORD.warning))),
            rows[0],
        );
        frame.render_widget(Paragraph::new(footer), rows[1]);
    } else {
        frame.render_widget(Paragraph::new(footer), area);
    }
}

fn render_help(frame: &mut Frame<'_>, app: &App) {
    let area = centered_rect(frame.area(), 72, 19);
    frame.render_widget(Clear, area);
    let context = app.action_context();
    let mut lines = vec![
        Line::styled(
            "MONITOR CONTROLS",
            Style::default().fg(NORD.accent).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw("1–7                 switch top-row views"),
        Line::raw("Up/Down · j/k        move within the active list"),
        Line::raw("Tab · Shift-Tab      move between list and inspector"),
        Line::raw("/                    filter the current view"),
        Line::raw("Ctrl-P               search every contextual command"),
        Line::raw("right-click          exact-target action menu"),
        Line::raw("Esc                  dismiss, clear, back, then quit"),
        Line::raw("q · Ctrl-C           quit"),
        Line::raw(""),
        Line::styled("CURRENT ACTIONS", Style::default().fg(NORD.accent_alt)),
    ];
    let mut actions = app.registry.resolve_menu(SCOPE_INLINE, &context).items().to_vec();
    actions.extend_from_slice(app.registry.resolve_menu(ITEM_INLINE, &context).items());
    lines.extend(actions.iter().map(|action| {
        Line::styled(
            format!("{:<22} {}", action_shortcut(action), action.title),
            Style::default().fg(match action.state {
                crate::tui::ActionState::Enabled => NORD.text,
                crate::tui::ActionState::Disabled { .. } => NORD.text_muted,
            }),
        )
    }));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Monitor is read-only; actions hand off to their owning tools.",
        Style::default().fg(NORD.text_muted),
    ));
    lines.push(Line::styled("Press ? or Esc to close", Style::default().fg(NORD.text_muted)));
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(NORD.accent))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
}

fn service_line(item: &ServiceSnapshot) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{} ", state_glyph(item.state)),
            Style::default().fg(health_color(item.state)),
        ),
        Span::styled(format!("{:<22}", item.name), Style::default().fg(NORD.text_strong)),
        Span::styled(format!("{:>5}ms  ", item.latency_ms), Style::default().fg(NORD.text)),
        Span::styled(item.reason.clone(), Style::default().fg(NORD.text_muted)),
    ])
}

fn metric_line(item: &MetricSnapshot) -> Line<'static> {
    let value = report::metric_value(&item.value, item.unit);
    Line::from(vec![
        Span::styled(
            format!("{} ", state_glyph(item.state)),
            Style::default().fg(health_color(item.state)),
        ),
        Span::styled(format!("{:<16}", item.service_id), Style::default().fg(NORD.text_muted)),
        Span::styled(format!("{:<20}", item.name), Style::default().fg(NORD.text)),
        Span::styled(
            format!("{:>10}  ", value),
            Style::default().fg(health_color(item.state)).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            compact_sparkline(&item.samples, 16),
            Style::default().fg(health_color(item.state)),
        ),
    ])
}

fn log_line(item: &LogEvent) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{:<7}", item.level.to_ascii_uppercase()),
            Style::default().fg(level_color(&item.level)),
        ),
        Span::styled(format!("{:<16}", item.service_id), Style::default().fg(NORD.text_muted)),
        Span::styled(item.message.clone(), Style::default().fg(NORD.text)),
        Span::styled(
            if item.redacted_fields > 0 {
                format!("  REDACTED {}", item.redacted_fields)
            } else {
                String::new()
            },
            Style::default().fg(NORD.danger),
        ),
    ])
}

fn cost_line(item: &CostSnapshot) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{:<20}", item.provider), Style::default().fg(NORD.text_muted)),
        Span::styled(format!("{:<24}", item.name), Style::default().fg(NORD.text)),
        Span::styled(
            format!("${:>9.2}  ", item.monthly_usd),
            Style::default().fg(NORD.special).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:?}", item.confidence).to_ascii_uppercase(),
            Style::default().fg(NORD.accent_alt),
        ),
    ])
}

fn source_line(item: &SourceSnapshot) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{} ", source_glyph(item.state)),
            Style::default().fg(source_color(item.state)),
        ),
        Span::styled(format!("{:<22}", item.name), Style::default().fg(NORD.text)),
        Span::styled(format!("{:<14}", item.kind), Style::default().fg(NORD.text_muted)),
        Span::styled(format!("{:>5}ms  ", item.latency_ms), Style::default().fg(NORD.text)),
        Span::styled(item.detail.clone(), Style::default().fg(NORD.text_muted)),
    ])
}

fn detail_lines(values: &[(&str, String)]) -> Vec<Line<'static>> {
    values
        .iter()
        .flat_map(|(label, value)| {
            [
                Line::from(vec![
                    Span::styled(format!("{label:<12}"), Style::default().fg(NORD.text_muted)),
                    Span::styled(value.clone(), Style::default().fg(NORD.text)),
                ]),
                Line::raw(""),
            ]
        })
        .collect()
}

fn compact_detail_lines(values: &[(&str, String)]) -> Vec<Line<'static>> {
    values
        .iter()
        .map(|(label, value)| {
            Line::from(vec![
                Span::styled(format!("{label:<10}"), Style::default().fg(NORD.text_muted)),
                Span::styled(value.clone(), Style::default().fg(NORD.text)),
            ])
        })
        .collect()
}

fn compact_sparkline(samples: &[MetricSample], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if samples.len() < 2 || width == 0 {
        return "—".to_owned();
    }
    let samples = &samples[samples.len().saturating_sub(width)..];
    let (minimum, maximum) = metric_bounds(samples);
    let range = maximum - minimum;
    samples
        .iter()
        .map(|sample| {
            if range.abs() <= f64::EPSILON {
                BARS[3]
            } else {
                let index = ((sample.value - minimum) / range * (BARS.len() - 1) as f64)
                    .round()
                    .clamp(0.0, (BARS.len() - 1) as f64) as usize;
                BARS[index]
            }
        })
        .collect()
}

fn metric_bounds(samples: &[MetricSample]) -> (f64, f64) {
    samples.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(minimum, maximum), sample| {
        (minimum.min(sample.value), maximum.max(sample.value))
    })
}

fn empty_detail(app: &App) -> String {
    match app.view {
        MonitorView::Logs => app.snapshot.logs.detail.clone(),
        MonitorView::Deployments => app.snapshot.deployments.detail.clone(),
        MonitorView::Costs => app.snapshot.costs.detail.clone(),
        _ if !app.filter.is_empty() => "No items match the current filter".to_owned(),
        _ => "No items are available".to_owned(),
    }
}

fn active_panel(title: &str, active: bool) -> Block<'static> {
    panel(title, active)
}

fn panel(title: &str, active: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(if active { NORD.accent } else { NORD.border }))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            title.to_owned(),
            Style::default()
                .fg(if active { NORD.accent } else { NORD.text_muted })
                .add_modifier(if active { Modifier::BOLD } else { Modifier::empty() }),
        ))
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + area.width.saturating_sub(width.min(area.width)) / 2,
        area.y + area.height.saturating_sub(height.min(area.height)) / 2,
        width.min(area.width),
        height.min(area.height),
    )
}

fn matches_filter(filter: &str, values: &[&str]) -> bool {
    filter.is_empty() || values.iter().any(|value| value.to_ascii_lowercase().contains(filter))
}

fn view_index(view: MonitorView) -> usize {
    MonitorView::ALL.iter().position(|candidate| *candidate == view).unwrap_or_default()
}

fn health_color(state: HealthState) -> ratatui::style::Color {
    match state {
        HealthState::Healthy => NORD.success,
        HealthState::Degraded => NORD.warning,
        HealthState::Incident => NORD.danger,
        HealthState::Unknown => NORD.text_muted,
    }
}

fn source_color(state: SourceState) -> ratatui::style::Color {
    match state {
        SourceState::Ready => NORD.success,
        SourceState::Partial => NORD.warning,
        SourceState::Unavailable => NORD.text_muted,
        SourceState::Unauthorized | SourceState::Error => NORD.danger,
    }
}

fn level_color(level: &str) -> ratatui::style::Color {
    match level.to_ascii_lowercase().as_str() {
        "error" | "fatal" => NORD.danger,
        "warn" | "warning" => NORD.warning,
        _ => NORD.text_muted,
    }
}

fn state_glyph(state: HealthState) -> &'static str {
    match state {
        HealthState::Healthy => "●",
        HealthState::Degraded => "◐",
        HealthState::Incident => "×",
        HealthState::Unknown => "?",
    }
}

fn source_glyph(state: SourceState) -> &'static str {
    match state {
        SourceState::Ready => "●",
        SourceState::Partial => "◐",
        SourceState::Unavailable => "—",
        SourceState::Unauthorized => "!",
        SourceState::Error => "×",
    }
}

fn unix_time_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::monitor::config::MetricUnit;
    use crate::tools::monitor::model::{
        CostSummary, DeploymentCollection, EnvironmentSnapshot, HealthSummary, LogCollection,
        SourceState, SNAPSHOT_SCHEMA_VERSION,
    };
    use ratatui::{backend::TestBackend, Terminal};

    fn snapshot() -> MonitorSnapshot {
        MonitorSnapshot {
            schema_version: SNAPSHOT_SCHEMA_VERSION,
            environment: EnvironmentSnapshot { id: "production".into(), name: "Production".into() },
            observed_at_secs: unix_time_secs(),
            collection_duration_ms: 5,
            health: HealthSummary {
                state: HealthState::Degraded,
                healthy_services: 0,
                degraded_services: 1,
                incident_services: 0,
                unknown_services: 0,
                reasons: vec!["API: latency".into()],
            },
            services: vec![ServiceSnapshot {
                id: "api".into(),
                name: "API".into(),
                state: HealthState::Degraded,
                reason: "latency".into(),
                source_id: "health".into(),
                observed_at_secs: 1,
                latency_ms: 12,
            }],
            performance: vec![MetricSnapshot {
                id: "latency-p95".into(),
                service_id: "api".into(),
                name: "Request p95".into(),
                source_id: "metrics".into(),
                unit: MetricUnit::Milliseconds,
                state: HealthState::Degraded,
                value: MetricValue::Available(280.0),
                samples: vec![
                    MetricSample { observed_at_secs: 1, value: 120.0 },
                    MetricSample { observed_at_secs: 301, value: 180.0 },
                    MetricSample { observed_at_secs: 601, value: 280.0 },
                ],
                observed_at_secs: 601,
                latency_ms: 8,
            }],
            logs: LogCollection {
                state: SourceState::Unavailable,
                events: Vec::new(),
                detail: "no Loki source".into(),
                truncated: false,
                limit: 200,
            },
            deployments: DeploymentCollection {
                state: SourceState::Unavailable,
                entries: Vec::new(),
                detail: "no deployment source".into(),
                truncated: false,
            },
            costs: CostSummary {
                currency: "USD".into(),
                monthly_total: 24.0,
                monthly_budget: Some(100.0),
                budget_percent: Some(24.0),
                state: SourceState::Partial,
                items: Vec::new(),
                detail: "configured only".into(),
            },
            sources: Vec::new(),
            warnings: vec!["billing partial".into()],
        }
    }

    #[test]
    fn tabs_are_the_first_rendered_row() {
        let mut app =
            App::new(snapshot(), contributions::registry().unwrap(), false, "monitor.toml".into());
        let backend = TestBackend::new(140, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let _ = render(frame, &mut app);
            })
            .unwrap();
        let first_row = (0..140)
            .map(|x| terminal.backend().buffer().cell((x, 0)).unwrap().symbol())
            .collect::<String>();
        assert!(!first_row.contains("MONITOR /"));
        assert!(first_row.trim_start().starts_with("Overview"));
        assert!(first_row.contains("Overview"));
        assert!(first_row.contains("Sources"));
    }

    #[test]
    fn performance_projects_metric_history_as_sparklines_and_a_chart() {
        let mut app =
            App::new(snapshot(), contributions::registry().unwrap(), false, "monitor.toml".into());
        app.view = MonitorView::Performance;
        app.active_region = ActiveRegion::Secondary;
        let backend = TestBackend::new(140, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let _ = render(frame, &mut app);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("1H TREND"));
        assert!(rendered.chars().any(|character| matches!(character, '▁'..='█')));
    }

    #[test]
    fn compact_sparkline_preserves_direction_and_bounds_width() {
        let samples = [
            MetricSample { observed_at_secs: 1, value: 1.0 },
            MetricSample { observed_at_secs: 2, value: 2.0 },
            MetricSample { observed_at_secs: 3, value: 3.0 },
        ];
        assert_eq!(compact_sparkline(&samples, 2), "▁█");
    }
}
