use std::{path::PathBuf, time::Duration};

use anyhow::{Context as _, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    layout::{Position, Rect},
    style::{Modifier, Style},
};

use crate::tui::{
    fuzzy::Matcher, theme::NORD, ActionId, ActionInvocation, ActionUnavailable, CommandPalette,
    CommandPaletteLayout, CommandPaletteOutcome, ContextMenu, ContextMenuLayout,
    ContextMenuOutcome, EventReader, KeyChord, KeybindingResolution, KeybindingState, LineEditor,
    NavigationHistory, NavigationMap, NavigationRegion, SelectableRegion, SelectionOutcome,
    Session, SessionOptions, SettingsEditor, SettingsFlow, SplitDrag, SplitFrame, SplitRatio,
    TextSelection, Viewport, ViewportMetrics,
};

use super::{
    config::{self, UiConfig},
    contributions::{
        self, BulkProjection, FocusedProjection, SkillsAction, SkillsActionContext,
        SkillsActionRegistry, SkillsRegion, DASHBOARD_ACTIONS, SKILL_CONTEXT,
    },
    controller::SkillsController,
    model::{
        DoctorIssue, DoctorReport, LibraryReport, OperationKind, ProjectionId, ProjectionReport,
        ProjectionScope, ProjectionState, SkillStatus, SkillsSnapshot,
    },
};

mod form;
mod view;

use form::{
    CreateFormOutcome, CreateSkillForm, CreateSkillLayout, LibraryForm, LibraryFormOutcome,
    LibraryLayout, LibraryRequest,
};
use view::render;

const REFRESH_INTERVAL: Duration = Duration::from_secs(3);
const MIN_CATALOG_WIDTH: u16 = 68;
const MIN_DETAILS_WIDTH: u16 = 36;

pub(super) async fn run(mut controller: SkillsController) -> Result<()> {
    let library = controller.library_report();
    let (snapshot, notice) = match &library {
        LibraryReport::Configured { .. } => match controller.snapshot(None) {
            Ok(snapshot) => (Some(snapshot), None),
            Err(error) => (None, Some(format!("load Skills library: {error:#}"))),
        },
        LibraryReport::Unconfigured => (None, None),
    };
    let mut app =
        App::new(controller.ui().clone(), controller.config_store(), library, snapshot, notice)?;
    load_selected_document(&controller, &mut app);

    let mut terminal =
        Session::open(SessionOptions { mouse_capture: true, bracketed_paste: true })?;
    let mut events = EventReader::start();
    let mut interval = tokio::time::interval(REFRESH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    let mut regions = UiRegions::default();
    let mut selection = TextSelection::<SkillsSelectionSurface>::default();

    loop {
        terminal.draw(|frame| {
            regions = render(frame, &mut app);
            selection.capture_frame(
                frame,
                &regions.selectable,
                Style::default().bg(NORD.selection).add_modifier(Modifier::REVERSED),
            );
        })?;

        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { break };
                if let Event::Key(key) = &event {
                    match selection.on_key(*key) {
                        SelectionOutcome::CopyReady(text) => {
                            terminal.copy(&text)?;
                            continue;
                        }
                        SelectionOutcome::EdgeScroll { surface: SkillsSelectionSurface::Details, lines } => {
                            app.scroll_details(lines, &regions);
                            continue;
                        }
                        SelectionOutcome::Captured | SelectionOutcome::Changed => continue,
                        SelectionOutcome::Unhandled => {}
                    }
                }
                if let Event::Mouse(mouse) = &event {
                    if selection.is_dragging() {
                        if let SelectionOutcome::EdgeScroll {
                            surface: SkillsSelectionSurface::Details,
                            lines,
                        } = selection.on_mouse(*mouse)
                        {
                            app.scroll_details(lines, &regions);
                        }
                        continue;
                    }
                }
                let selection_mouse = match &event {
                    Event::Mouse(mouse) => Some(*mouse),
                    _ => None,
                };
                let flow = app.on_event(event, &regions)?;
                if matches!(flow, Flow::Continue)
                    && matches!(&app.surface, Surface::Normal)
                    && app.menu.is_none()
                {
                    if let Some(mouse) = selection_mouse {
                        let _ = selection.on_mouse(mouse);
                    }
                }
                if apply_flow(&mut controller, &mut app, &mut terminal, flow)? {
                    break;
                }
                load_selected_document(&controller, &mut app);
            }
            _ = interval.tick(), if matches!(&app.surface, Surface::Normal) => {
                refresh(&controller, &mut app, true);
                load_selected_document(&controller, &mut app);
            }
        }
    }
    Ok(())
}

fn apply_flow(
    controller: &mut SkillsController,
    app: &mut App,
    terminal: &mut Session,
    flow: Flow,
) -> Result<bool> {
    match flow {
        Flow::Continue => {}
        Flow::Quit => return Ok(true),
        Flow::Refresh => refresh(controller, app, false),
        Flow::Create { name, description } => match controller.create(&name, &description) {
            Ok(skill) => {
                app.notice = Some(format!("Created {}", skill.name));
                refresh(controller, app, true);
                app.select(skill.name.as_str(), true);
            }
            Err(error) => app.notice = Some(format!("create skill: {error:#}")),
        },
        Flow::SetLibrary(request) => match controller.set_library(&request.path, request.create) {
            Ok(report) => {
                app.library = LibraryReport::Configured { path: report.path.clone() };
                app.notice = Some(if report.created {
                    format!("Created and configured {}", report.path.display())
                } else {
                    format!("Configured {}", report.path.display())
                });
                refresh(controller, app, true);
            }
            Err(error) => {
                app.notice = Some(format!("set library: {error:#}"));
                app.surface =
                    Surface::Library(LibraryForm::new(Some(&request.path), request.required));
            }
        },
        Flow::Mutate { skill, projections, operation } => {
            match controller.mutate(operation, std::slice::from_ref(&skill), &projections, None) {
                Ok(report) => {
                    let outcomes = report
                        .changes
                        .iter()
                        .map(|change| {
                            format!("{} {}", change.outcome.label(), change.projection.label())
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    app.notice = Some(format!("{skill}: {outcomes}"));
                    refresh(controller, app, true);
                }
                Err(error) => app.notice = Some(format!("change availability: {error:#}")),
            }
        }
        Flow::Doctor => {
            let report = controller.doctor(None);
            app.notice = Some(if report.healthy() {
                "Skills doctor: healthy".to_owned()
            } else {
                format!("Skills doctor found {} issue(s)", report.issues.len())
            });
            app.doctor = Some(report.clone());
            app.surface = Surface::Doctor(DoctorView { report, selected: 0 });
        }
        Flow::Copy(text) => {
            terminal.copy(&text)?;
            app.notice = Some("Copied path".to_owned());
        }
        Flow::ReloadSettings => match controller.reload_config() {
            Ok(()) => app.replace_ui(controller.ui().clone())?,
            Err(error) => app.notice = Some(format!("reload settings: {error:#}")),
        },
        Flow::SaveSplit { ratio, original } => {
            if let Err(error) = controller.set_panel_ratio(ratio) {
                app.split_ratio = original;
                app.notice = Some(format!("save panel width: {error:#}"));
            }
        }
    }
    Ok(false)
}

fn refresh(controller: &SkillsController, app: &mut App, quiet: bool) {
    match &app.library {
        LibraryReport::Unconfigured => {
            if !quiet {
                app.notice = Some("Configure the canonical Skills library first".to_owned());
            }
        }
        LibraryReport::Configured { .. } => match controller.snapshot(None) {
            Ok(snapshot) => {
                app.replace_snapshot(snapshot);
                if !quiet {
                    app.notice = Some("Refreshed from disk".to_owned());
                }
            }
            Err(error) => app.notice = Some(format!("refresh Skills: {error:#}")),
        },
    }
}

fn load_selected_document(controller: &SkillsController, app: &mut App) {
    let Some(name) = app.document_request() else {
        return;
    };
    match controller.skill(&name) {
        Ok(skill) => app.set_document(name, skill.markdown().to_owned()),
        Err(error) => app.set_document(name, format!("# Unable to load skill\n\n{error:#}")),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceKind {
    Normal,
    Search,
    CommandPalette,
    Settings,
    CreateSkill,
    Library,
    Doctor,
    Help,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SkillsSelectionSurface {
    Details,
}

enum Surface {
    Normal,
    Search(LineEditor),
    CommandPalette(CommandPalette<SkillsActionContext>),
    Settings(SettingsEditor),
    CreateSkill(CreateSkillForm),
    Library(LibraryForm),
    Doctor(DoctorView),
    Help,
}

impl Surface {
    const fn kind(&self) -> SurfaceKind {
        match self {
            Self::Normal => SurfaceKind::Normal,
            Self::Search(_) => SurfaceKind::Search,
            Self::CommandPalette(_) => SurfaceKind::CommandPalette,
            Self::Settings(_) => SurfaceKind::Settings,
            Self::CreateSkill(_) => SurfaceKind::CreateSkill,
            Self::Library(_) => SurfaceKind::Library,
            Self::Doctor(_) => SurfaceKind::Doctor,
            Self::Help => SurfaceKind::Help,
        }
    }
}

struct DoctorView {
    report: DoctorReport,
    selected: usize,
}

enum Flow {
    Continue,
    Quit,
    Refresh,
    Create { name: String, description: String },
    SetLibrary(LibraryRequest),
    Mutate { skill: String, projections: Vec<ProjectionId>, operation: OperationKind },
    Doctor,
    Copy(String),
    ReloadSettings,
    SaveSplit { ratio: SplitRatio, original: SplitRatio },
}

#[derive(Default)]
struct UiRegions {
    compact: bool,
    split: Option<SplitFrame>,
    catalog: Option<Rect>,
    details: Option<Rect>,
    catalog_content: Option<Rect>,
    detail_content: Option<Rect>,
    detail_content_len: usize,
    skill_rows: Vec<(Rect, String)>,
    projection_cells: Vec<(Rect, String, ProjectionId)>,
    tab_rows: Vec<(Rect, DetailTab)>,
    action_rows: Vec<(Rect, ActionId)>,
    context_menu: Option<ContextMenuLayout>,
    command_palette: Option<CommandPaletteLayout>,
    create_skill: Option<CreateSkillLayout>,
    library: Option<LibraryLayout>,
    search: Option<Rect>,
    doctor_rows: Vec<(Rect, usize)>,
    doctor_close: Option<Rect>,
    help_close: Option<Rect>,
    selectable: Vec<SelectableRegion<SkillsSelectionSurface>>,
}

impl UiRegions {
    fn navigation(&self) -> NavigationMap<SkillsRegion> {
        NavigationMap::new([
            NavigationRegion::new(SkillsRegion::Catalog, self.catalog.unwrap_or_default()),
            NavigationRegion::new(SkillsRegion::Details, self.details.unwrap_or_default()),
        ])
    }

    fn catalog_metrics(&self, content_len: usize) -> ViewportMetrics {
        ViewportMetrics::new(
            content_len,
            self.catalog_content.map_or(0, |area| usize::from(area.height)),
        )
    }

    fn detail_metrics(&self) -> ViewportMetrics {
        ViewportMetrics::new(
            self.detail_content_len,
            self.detail_content.map_or(0, |area| usize::from(area.height)),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DetailTab {
    Overview,
    Content,
    Diagnostics,
}

impl DetailTab {
    const ALL: [Self; 3] = [Self::Overview, Self::Content, Self::Diagnostics];

    const fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Content => "Content",
            Self::Diagnostics => "Diagnostics",
        }
    }

    fn move_by(self, delta: isize) -> Self {
        let index = Self::ALL.iter().position(|tab| *tab == self).unwrap_or_default();
        Self::ALL[(index as isize + delta).rem_euclid(Self::ALL.len() as isize) as usize]
    }
}

struct LoadedDocument {
    skill: String,
    markdown: String,
}

struct App {
    ui: UiConfig,
    config_store: crate::framework::ConfigStore,
    library: LibraryReport,
    snapshot: Option<SkillsSnapshot>,
    search_query: String,
    visible_skills: Vec<String>,
    selected: Option<String>,
    projection_index: usize,
    active_region: SkillsRegion,
    detail_tab: DetailTab,
    surface: Surface,
    menu: Option<ContextMenu<SkillsActionContext>>,
    registry: SkillsActionRegistry,
    keybinding_state: KeybindingState,
    history: NavigationHistory<String>,
    split_ratio: SplitRatio,
    split_drag: Option<SplitDrag<()>>,
    catalog_viewport: Viewport,
    detail_viewport: Viewport,
    document: Option<LoadedDocument>,
    document_revision: u64,
    doctor: Option<DoctorReport>,
    notice: Option<String>,
}

impl App {
    fn new(
        ui: UiConfig,
        config_store: crate::framework::ConfigStore,
        library: LibraryReport,
        snapshot: Option<SkillsSnapshot>,
        notice: Option<String>,
    ) -> Result<Self> {
        let registry =
            contributions::registry(ui.keybindings()).context("build Skills action registry")?;
        let selected = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.skills.first())
            .map(|status| status.skill.name.as_str().to_owned());
        let visible_skills = snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot.skills.iter().map(|status| status.skill.name.as_str().to_owned()).collect()
            })
            .unwrap_or_default();
        let mut history = NavigationHistory::default();
        if let Some(selected) = &selected {
            history.visit(selected.clone());
        }
        let split_ratio = ui.panel_ratio();
        let surface = if matches!(&library, LibraryReport::Unconfigured) {
            Surface::Library(LibraryForm::new(None, true))
        } else {
            Surface::Normal
        };
        Ok(Self {
            ui,
            config_store,
            library,
            snapshot,
            search_query: String::new(),
            visible_skills,
            selected,
            projection_index: 0,
            active_region: SkillsRegion::Catalog,
            detail_tab: DetailTab::Overview,
            surface,
            menu: None,
            registry,
            keybinding_state: KeybindingState::default(),
            history,
            split_ratio,
            split_drag: None,
            catalog_viewport: Viewport::new(0),
            detail_viewport: Viewport::new(0),
            document: None,
            document_revision: 0,
            doctor: None,
            notice,
        })
    }

    fn replace_ui(&mut self, ui: UiConfig) -> Result<()> {
        self.registry =
            contributions::registry(ui.keybindings()).context("rebuild Skills action registry")?;
        self.split_ratio = ui.panel_ratio();
        self.ui = ui;
        self.notice = Some("Settings applied".to_owned());
        Ok(())
    }

    fn replace_snapshot(&mut self, snapshot: SkillsSnapshot) {
        let previous = self.selected.clone();
        self.snapshot = Some(snapshot);
        self.rebuild_visible_skills();
        self.selected = previous
            .clone()
            .filter(|name| self.visible_skills.iter().any(|visible| visible == name))
            .or_else(|| self.visible_skills.first().cloned());
        if self.selected != previous {
            self.document = None;
            self.detail_viewport.home();
            if let Some(selected) = &self.selected {
                self.history.visit(selected.clone());
            }
        }
        self.projection_index = self.projection_index.min(ProjectionId::ALL.len() - 1);
    }

    fn on_event(&mut self, event: Event, regions: &UiRegions) -> Result<Flow> {
        if matches!(
            event,
            Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            })
        ) {
            return Ok(Flow::Quit);
        }
        if self.split_drag.is_some() {
            return self.on_split_drag(event, regions);
        }
        match self.surface.kind() {
            SurfaceKind::Search => return self.on_search_event(event),
            SurfaceKind::CommandPalette => return self.on_palette_event(event, regions),
            SurfaceKind::Settings => return self.on_settings_event(event),
            SurfaceKind::CreateSkill => return self.on_create_event(event, regions),
            SurfaceKind::Library => return self.on_library_event(event, regions),
            SurfaceKind::Doctor => return self.on_doctor_event(event, regions),
            SurfaceKind::Help => return self.on_help_event(event, regions),
            SurfaceKind::Normal => {}
        }
        if self.menu.is_some() {
            return self.on_menu_event(event, regions);
        }
        match event {
            Event::Key(key) if key.is_press() => self.on_key(key, regions),
            Event::Mouse(mouse) => self.on_mouse(mouse, regions),
            Event::Resize(_, _) => Ok(Flow::Continue),
            _ => Ok(Flow::Continue),
        }
    }

    fn on_key(&mut self, key: KeyEvent, regions: &UiRegions) -> Result<Flow> {
        if self.active_region == SkillsRegion::Details {
            match key.code {
                KeyCode::Up => {
                    self.scroll_details(-1, regions);
                    return Ok(Flow::Continue);
                }
                KeyCode::Down => {
                    self.scroll_details(1, regions);
                    return Ok(Flow::Continue);
                }
                KeyCode::PageUp => {
                    self.detail_viewport.page_by(-1, regions.detail_metrics());
                    return Ok(Flow::Continue);
                }
                KeyCode::PageDown => {
                    self.detail_viewport.page_by(1, regions.detail_metrics());
                    return Ok(Flow::Continue);
                }
                KeyCode::Home => {
                    self.detail_viewport.home();
                    return Ok(Flow::Continue);
                }
                KeyCode::End => {
                    self.detail_viewport.end(regions.detail_metrics());
                    return Ok(Flow::Continue);
                }
                _ => {}
            }
        }
        let Some(chord) = KeyChord::from_event(key) else {
            return Ok(Flow::Continue);
        };
        let context = self.action_context(regions);
        match self.registry.resolve_keybinding(&mut self.keybinding_state, chord, context) {
            KeybindingResolution::Invoke(invocation) => self.invoke(invocation, regions),
            KeybindingResolution::Pending
            | KeybindingResolution::Unmatched
            | KeybindingResolution::UnmatchedSequence { .. } => Ok(Flow::Continue),
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent, regions: &UiRegions) -> Result<Flow> {
        self.keybinding_state.cancel();
        let position = Position { x: mouse.column, y: mouse.row };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(split) = regions.split {
                    if let Some(drag) =
                        SplitDrag::begin((), split, self.split_ratio, mouse.column, mouse.row)
                    {
                        self.split_drag = Some(drag);
                        return Ok(Flow::Continue);
                    }
                }
                if regions.search.is_some_and(|area| area.contains(position)) {
                    let context = self.action_context(regions);
                    return self
                        .invoke(ActionInvocation::new(contributions::SEARCH, context), regions);
                }
                if let Some((_, name, projection)) =
                    regions.projection_cells.iter().find(|(area, _, _)| area.contains(position))
                {
                    self.select(name, true);
                    self.projection_index = ProjectionId::ALL
                        .iter()
                        .position(|candidate| candidate == projection)
                        .unwrap_or_default();
                    self.active_region = SkillsRegion::Catalog;
                    return Ok(Flow::Continue);
                }
                if let Some((_, name)) =
                    regions.skill_rows.iter().find(|(area, _)| area.contains(position))
                {
                    self.select(name, true);
                    self.active_region = SkillsRegion::Catalog;
                    return Ok(Flow::Continue);
                }
                if let Some((_, tab)) =
                    regions.tab_rows.iter().find(|(area, _)| area.contains(position))
                {
                    self.detail_tab = *tab;
                    self.detail_viewport.home();
                    self.active_region = SkillsRegion::Details;
                    return Ok(Flow::Continue);
                }
                if let Some((_, action)) =
                    regions.action_rows.iter().find(|(area, _)| area.contains(position))
                {
                    let context = self.action_context(regions);
                    return self.invoke(ActionInvocation::new(*action, context), regions);
                }
                if !regions.compact {
                    if let Some(region) = regions.navigation().hit_test(mouse.column, mouse.row) {
                        self.active_region = region;
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some((_, name, projection)) =
                    regions.projection_cells.iter().find(|(area, _, _)| area.contains(position))
                {
                    self.select(name, true);
                    self.projection_index = ProjectionId::ALL
                        .iter()
                        .position(|candidate| candidate == projection)
                        .unwrap_or_default();
                    let context = self.action_context(regions);
                    self.menu = ContextMenu::open(
                        position,
                        context,
                        self.registry.resolve_menu(SKILL_CONTEXT, &context),
                    );
                }
            }
            MouseEventKind::ScrollUp
                if regions.catalog.is_some_and(|area| area.contains(position)) =>
            {
                self.move_skill(-3, regions);
            }
            MouseEventKind::ScrollDown
                if regions.catalog.is_some_and(|area| area.contains(position)) =>
            {
                self.move_skill(3, regions);
            }
            MouseEventKind::ScrollUp
                if regions.details.is_some_and(|area| area.contains(position)) =>
            {
                self.scroll_details(-3, regions);
            }
            MouseEventKind::ScrollDown
                if regions.details.is_some_and(|area| area.contains(position)) =>
            {
                self.scroll_details(3, regions);
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    fn on_split_drag(&mut self, event: Event, regions: &UiRegions) -> Result<Flow> {
        let Some(drag) = self.split_drag else {
            return Ok(Flow::Continue);
        };
        match event {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column,
                ..
            }) => {
                if let Some(split) = regions.split {
                    if let Some(ratio) = drag.ratio_for_column((), split, column) {
                        self.split_ratio = ratio;
                    }
                }
            }
            Event::Mouse(MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), .. }) => {
                self.split_drag = None;
                let (_, original) = drag.cancel();
                if drag.changed(self.split_ratio) {
                    return Ok(Flow::SaveSplit { ratio: self.split_ratio, original });
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
                let (_, original) = drag.cancel();
                self.split_ratio = original;
                self.split_drag = None;
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    fn on_search_event(&mut self, event: Event) -> Result<Flow> {
        let mut close = false;
        let query = match &mut self.surface {
            Surface::Search(editor) => {
                match event {
                    Event::Key(key) if !key.is_press() => {}
                    Event::Key(KeyEvent { code: KeyCode::Esc | KeyCode::Enter, .. }) => {
                        close = true;
                    }
                    Event::Key(key) => editor.apply_key(key),
                    Event::Paste(text) => {
                        for character in text.chars().filter(|character| !character.is_control()) {
                            editor.insert(character);
                        }
                    }
                    _ => {}
                }
                editor.value().to_owned()
            }
            _ => return Ok(Flow::Continue),
        };
        self.set_search_query(query);
        if close {
            self.surface = Surface::Normal;
        }
        Ok(Flow::Continue)
    }

    fn on_doctor_event(&mut self, event: Event, regions: &UiRegions) -> Result<Flow> {
        let issue_count = match &self.surface {
            Surface::Doctor(view) => view.report.issues.len(),
            _ => return Ok(Flow::Continue),
        };
        match event {
            Event::Key(key) if !key.is_press() => {}
            Event::Key(KeyEvent { code: KeyCode::Esc | KeyCode::Char('q'), .. }) => {
                self.surface = Surface::Normal;
            }
            Event::Key(KeyEvent { code: KeyCode::Char('r'), .. }) => return Ok(Flow::Doctor),
            Event::Key(KeyEvent { code: KeyCode::Up | KeyCode::Char('k'), .. }) => {
                if let Surface::Doctor(view) = &mut self.surface {
                    view.selected = view.selected.saturating_sub(1);
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Down | KeyCode::Char('j'), .. }) => {
                if let Surface::Doctor(view) = &mut self.surface {
                    view.selected =
                        view.selected.saturating_add(1).min(issue_count.saturating_sub(1));
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => self.inspect_doctor_issue(),
            Event::Key(KeyEvent { code: KeyCode::Char('c'), .. }) => {
                if let Some(path) = self.doctor_issue_path() {
                    return Ok(Flow::Copy(path.display().to_string()));
                }
            }
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                ..
            }) => {
                let position = Position { x: column, y: row };
                if regions.doctor_close.is_some_and(|area| area.contains(position)) {
                    self.surface = Surface::Normal;
                } else if let Some((_, index)) =
                    regions.doctor_rows.iter().find(|(area, _)| area.contains(position))
                {
                    if let Surface::Doctor(view) = &mut self.surface {
                        view.selected = *index;
                    }
                }
            }
            _ => {}
        }
        Ok(Flow::Continue)
    }

    fn on_help_event(&mut self, event: Event, regions: &UiRegions) -> Result<Flow> {
        let dismiss = match event {
            Event::Key(key) if !key.is_press() => false,
            Event::Key(KeyEvent {
                code: KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?') | KeyCode::Char('q'),
                ..
            }) => true,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                ..
            }) => {
                regions.help_close.is_some_and(|area| area.contains(Position { x: column, y: row }))
            }
            _ => false,
        };
        if dismiss {
            self.surface = Surface::Normal;
        }
        Ok(Flow::Continue)
    }

    fn on_palette_event(&mut self, event: Event, regions: &UiRegions) -> Result<Flow> {
        let Some(layout) = regions.command_palette.as_ref() else {
            self.surface = Surface::Normal;
            return Ok(Flow::Continue);
        };
        let outcome = match &mut self.surface {
            Surface::CommandPalette(palette) => palette.on_event(event, layout),
            _ => unreachable!("palette surface checked above"),
        };
        match outcome {
            CommandPaletteOutcome::Captured => Ok(Flow::Continue),
            CommandPaletteOutcome::Dismissed => {
                self.surface = Surface::Normal;
                Ok(Flow::Continue)
            }
            CommandPaletteOutcome::Invoke(invocation) => {
                self.surface = Surface::Normal;
                self.invoke(invocation, regions)
            }
        }
    }

    fn on_settings_event(&mut self, event: Event) -> Result<Flow> {
        let flow = match (&mut self.surface, event) {
            (Surface::Settings(editor), Event::Key(key)) => editor.on_key(key),
            (Surface::Settings(editor), Event::Mouse(mouse)) => editor.on_mouse(mouse),
            _ => SettingsFlow::Continue,
        };
        if flow == SettingsFlow::Exit {
            self.surface = Surface::Normal;
            return Ok(Flow::ReloadSettings);
        }
        Ok(Flow::Continue)
    }

    fn on_create_event(&mut self, event: Event, regions: &UiRegions) -> Result<Flow> {
        let Some(layout) = regions.create_skill.as_ref() else {
            self.surface = Surface::Normal;
            return Ok(Flow::Continue);
        };
        let outcome = match &mut self.surface {
            Surface::CreateSkill(form) => form.on_event(event, layout),
            _ => unreachable!("create surface checked above"),
        };
        match outcome {
            CreateFormOutcome::Captured => Ok(Flow::Continue),
            CreateFormOutcome::Cancelled => {
                self.surface = Surface::Normal;
                Ok(Flow::Continue)
            }
            CreateFormOutcome::Submit(request) => {
                self.surface = Surface::Normal;
                Ok(Flow::Create { name: request.name, description: request.description })
            }
        }
    }

    fn on_library_event(&mut self, event: Event, regions: &UiRegions) -> Result<Flow> {
        let Some(layout) = regions.library else {
            self.surface = Surface::Normal;
            return Ok(Flow::Continue);
        };
        let required = match &self.surface {
            Surface::Library(form) => form.required,
            _ => false,
        };
        let outcome = match &mut self.surface {
            Surface::Library(form) => form.on_event(event, layout),
            _ => unreachable!("library surface checked above"),
        };
        match outcome {
            LibraryFormOutcome::Captured => Ok(Flow::Continue),
            LibraryFormOutcome::Cancelled if required => Ok(Flow::Quit),
            LibraryFormOutcome::Cancelled => {
                self.surface = Surface::Normal;
                Ok(Flow::Continue)
            }
            LibraryFormOutcome::Submit(request) => {
                self.surface = Surface::Normal;
                Ok(Flow::SetLibrary(request))
            }
        }
    }

    fn on_menu_event(&mut self, event: Event, regions: &UiRegions) -> Result<Flow> {
        let Some(layout) = regions.context_menu.as_ref() else {
            self.menu = None;
            return Ok(Flow::Continue);
        };
        let outcome = self.menu.as_mut().expect("menu checked above").on_event(event, layout);
        match outcome {
            ContextMenuOutcome::Captured => Ok(Flow::Continue),
            ContextMenuOutcome::Dismissed => {
                self.menu = None;
                Ok(Flow::Continue)
            }
            ContextMenuOutcome::Unavailable { reason, .. } => {
                self.menu = None;
                self.notice = Some(reason.into_owned());
                Ok(Flow::Continue)
            }
            ContextMenuOutcome::Invoke(invocation) => {
                self.menu = None;
                self.invoke(invocation, regions)
            }
        }
    }

    fn invoke(
        &mut self,
        invocation: ActionInvocation<SkillsActionContext>,
        regions: &UiRegions,
    ) -> Result<Flow> {
        let command = match self.registry.command_for(&invocation) {
            Ok(command) => command,
            Err(ActionUnavailable::Disabled { reason, .. }) => {
                self.notice = Some(reason.into_owned());
                return Ok(Flow::Continue);
            }
            Err(error) => {
                self.notice = Some(error.to_string());
                return Ok(Flow::Continue);
            }
        };
        Ok(match command {
            SkillsAction::CreateSkill => {
                self.surface = Surface::CreateSkill(CreateSkillForm::new());
                Flow::Continue
            }
            SkillsAction::Search => {
                let mut editor = LineEditor::default();
                editor.set(self.search_query.clone());
                self.surface = Surface::Search(editor);
                Flow::Continue
            }
            SkillsAction::ToggleProjection => {
                let Some(name) = self.selected.clone() else {
                    return Ok(Flow::Continue);
                };
                let projection = ProjectionId::ALL[self.projection_index];
                match self.selected_projection().and_then(ProjectionReport::state) {
                    Some(ProjectionState::Disabled) => Flow::Mutate {
                        skill: name,
                        projections: vec![projection],
                        operation: OperationKind::Enable,
                    },
                    Some(ProjectionState::Enabled { .. }) => Flow::Mutate {
                        skill: name,
                        projections: vec![projection],
                        operation: OperationKind::Disable,
                    },
                    _ => return Ok(Flow::Continue),
                }
            }
            SkillsAction::EnableThisProject => {
                self.bulk_flow(ProjectionScope::ThisProject, OperationKind::Enable)
            }
            SkillsAction::DisableThisProject => {
                self.bulk_flow(ProjectionScope::ThisProject, OperationKind::Disable)
            }
            SkillsAction::EnableAllProjects => {
                self.bulk_flow(ProjectionScope::AllProjects, OperationKind::Enable)
            }
            SkillsAction::DisableAllProjects => {
                self.bulk_flow(ProjectionScope::AllProjects, OperationKind::Disable)
            }
            SkillsAction::Doctor => Flow::Doctor,
            SkillsAction::SetLibrary => {
                let path = match &self.library {
                    LibraryReport::Configured { path } => Some(path.as_path()),
                    LibraryReport::Unconfigured => None,
                };
                self.surface = Surface::Library(LibraryForm::new(path, false));
                Flow::Continue
            }
            SkillsAction::Refresh => Flow::Refresh,
            SkillsAction::OpenSettings => {
                self.surface = Surface::Settings(SettingsEditor::open(
                    self.config_store.clone(),
                    vec![config::settings()],
                    NORD,
                ));
                Flow::Continue
            }
            SkillsAction::Help => {
                self.surface = Surface::Help;
                Flow::Continue
            }
            SkillsAction::Inspect => {
                self.active_region = SkillsRegion::Details;
                Flow::Continue
            }
            SkillsAction::OpenContext => {
                let selected = self.selected.as_deref();
                let projection = ProjectionId::ALL[self.projection_index];
                let position = regions
                    .projection_cells
                    .iter()
                    .find(|(_, name, id)| selected == Some(name.as_str()) && *id == projection)
                    .map(|(area, _, _)| Position { x: area.x, y: area.y.saturating_add(1) })
                    .or_else(|| {
                        regions.catalog.map(|area| Position {
                            x: area.x.saturating_add(2),
                            y: area.y.saturating_add(2),
                        })
                    })
                    .unwrap_or_default();
                let context = self.action_context(regions);
                self.menu = ContextMenu::open(
                    position,
                    context,
                    self.registry.resolve_menu(SKILL_CONTEXT, &context),
                );
                Flow::Continue
            }
            SkillsAction::PreviousSkill => {
                self.move_skill(-1, regions);
                Flow::Continue
            }
            SkillsAction::NextSkill => {
                self.move_skill(1, regions);
                Flow::Continue
            }
            SkillsAction::PreviousProjection => {
                self.move_projection(-1);
                Flow::Continue
            }
            SkillsAction::NextProjection => {
                self.move_projection(1);
                Flow::Continue
            }
            SkillsAction::HistoryBack => {
                self.move_history(-1);
                Flow::Continue
            }
            SkillsAction::HistoryForward => {
                self.move_history(1);
                Flow::Continue
            }
            SkillsAction::PreviousTab => {
                self.detail_tab = self.detail_tab.move_by(-1);
                self.detail_viewport.home();
                Flow::Continue
            }
            SkillsAction::NextTab => {
                self.detail_tab = self.detail_tab.move_by(1);
                self.detail_viewport.home();
                Flow::Continue
            }
            SkillsAction::FocusNext => {
                if let Some(region) = regions.navigation().next(self.active_region) {
                    self.active_region = region;
                }
                Flow::Continue
            }
            SkillsAction::FocusPrevious => {
                if let Some(region) = regions.navigation().previous(self.active_region) {
                    self.active_region = region;
                }
                Flow::Continue
            }
            SkillsAction::OpenCommandPalette => {
                self.surface = Surface::CommandPalette(CommandPalette::open(
                    invocation.context,
                    &self.registry,
                ));
                Flow::Continue
            }
            SkillsAction::Quit => Flow::Quit,
        })
    }

    fn bulk_flow(&self, scope: ProjectionScope, operation: OperationKind) -> Flow {
        let Some(skill) = self.selected.clone() else {
            return Flow::Continue;
        };
        let projections = ProjectionId::ALL
            .into_iter()
            .filter(|projection| projection.scope == scope)
            .collect::<Vec<_>>();
        Flow::Mutate { skill, projections, operation }
    }

    fn action_context(&self, regions: &UiRegions) -> SkillsActionContext {
        let selected_index = self.selected_index();
        let navigation = regions.navigation();
        SkillsActionContext {
            configured: matches!(&self.library, LibraryReport::Configured { .. }),
            projection: self.focused_projection(),
            this_project: self.bulk_projection(ProjectionScope::ThisProject),
            all_projects: self.bulk_projection(ProjectionScope::AllProjects),
            region: self.active_region,
            can_previous_skill: selected_index.is_some_and(|index| index > 0),
            can_next_skill: selected_index
                .is_some_and(|index| index + 1 < self.visible_skills.len()),
            can_history_back: self.history.target(-1).is_some(),
            can_history_forward: self.history.target(1).is_some(),
            can_focus_next: navigation.next(self.active_region).is_some(),
            can_focus_previous: navigation.previous(self.active_region).is_some(),
        }
    }

    fn focused_projection(&self) -> FocusedProjection {
        match self.selected_projection() {
            None => FocusedProjection::Missing,
            Some(ProjectionReport::Unavailable { .. }) => FocusedProjection::Unavailable,
            Some(ProjectionReport::Observed {
                projection: ProjectionState::Disabled | ProjectionState::Enabled { .. },
                ..
            }) => FocusedProjection::Toggleable,
            Some(ProjectionReport::Observed { .. }) => FocusedProjection::Unsafe,
        }
    }

    fn bulk_projection(&self, scope: ProjectionScope) -> BulkProjection {
        let Some(status) = self.selected_status() else {
            return BulkProjection::Missing;
        };
        let reports = status.projections.iter().filter(|report| report.id().scope == scope);
        let mut saw_unavailable = false;
        let mut saw_unsafe = false;
        for report in reports {
            match report {
                ProjectionReport::Unavailable { .. } => saw_unavailable = true,
                ProjectionReport::Observed {
                    projection: ProjectionState::Disabled | ProjectionState::Enabled { .. },
                    ..
                } => {}
                ProjectionReport::Observed { .. } => saw_unsafe = true,
            }
        }
        if saw_unavailable {
            BulkProjection::Unavailable
        } else if saw_unsafe {
            BulkProjection::Unsafe
        } else {
            BulkProjection::Safe
        }
    }

    fn move_skill(&mut self, delta: isize, regions: &UiRegions) {
        if self.visible_skills.is_empty() {
            return;
        }
        let index = self.selected_index().unwrap_or_default();
        let next = index.saturating_add_signed(delta).min(self.visible_skills.len() - 1);
        let name = self.visible_skills[next].clone();
        let content_len = self.visible_skills.len();
        self.select(&name, true);
        self.catalog_viewport.ensure_visible(next, regions.catalog_metrics(content_len));
    }

    fn move_projection(&mut self, delta: isize) {
        self.projection_index = (self.projection_index as isize + delta)
            .rem_euclid(ProjectionId::ALL.len() as isize) as usize;
    }

    fn move_history(&mut self, delta: isize) {
        while let Some((cursor, target)) =
            self.history.target(delta).map(|(cursor, name)| (cursor, name.clone()))
        {
            self.history.select(cursor);
            if self.status(&target).is_some() {
                self.select(&target, false);
                return;
            }
        }
    }

    fn inspect_doctor_issue(&mut self) {
        let target = match &self.surface {
            Surface::Doctor(view) => {
                view.report.issues.get(view.selected).and_then(|issue| match issue {
                    DoctorIssue::ProjectionProblem { skill, projection, .. }
                    | DoctorIssue::ProjectionUnavailable { skill, projection, .. } => {
                        Some((skill.as_str().to_owned(), *projection))
                    }
                    _ => None,
                })
            }
            _ => None,
        };
        let Some((skill, projection)) = target else {
            self.notice = Some("This issue has no catalog destination to inspect".to_owned());
            return;
        };
        self.select(&skill, true);
        self.projection_index = ProjectionId::ALL
            .iter()
            .position(|candidate| *candidate == projection)
            .unwrap_or_default();
        self.detail_tab = DetailTab::Diagnostics;
        self.active_region = SkillsRegion::Details;
        self.detail_viewport.home();
        self.surface = Surface::Normal;
    }

    fn doctor_issue_path(&self) -> Option<PathBuf> {
        let Surface::Doctor(view) = &self.surface else {
            return None;
        };
        match view.report.issues.get(view.selected)? {
            DoctorIssue::LibraryUnavailable { path, .. }
            | DoctorIssue::InvalidSkill { path, .. }
            | DoctorIssue::ProjectionProblem { path, .. } => Some(path.clone()),
            DoctorIssue::LibraryUnconfigured
            | DoctorIssue::RepositoryUnavailable { .. }
            | DoctorIssue::ProjectionUnavailable { .. } => None,
        }
    }

    fn scroll_details(&mut self, lines: isize, regions: &UiRegions) {
        self.detail_viewport.scroll_by(lines, regions.detail_metrics());
    }

    fn select(&mut self, name: &str, record: bool) {
        if self.status(name).is_none() || self.selected.as_deref() == Some(name) {
            return;
        }
        self.selected = Some(name.to_owned());
        self.document = None;
        self.document_revision = self.document_revision.wrapping_add(1);
        self.detail_viewport.home();
        if record {
            self.history.visit(name.to_owned());
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let selected = self.selected.as_deref()?;
        self.visible_skills.iter().position(|name| name == selected)
    }

    fn selected_status(&self) -> Option<&SkillStatus> {
        self.selected.as_deref().and_then(|name| self.status(name))
    }

    fn status(&self, name: &str) -> Option<&SkillStatus> {
        self.snapshot.as_ref()?.skills.iter().find(|status| status.skill.name.as_str() == name)
    }

    fn selected_projection(&self) -> Option<&ProjectionReport> {
        let id = ProjectionId::ALL[self.projection_index];
        self.selected_status()?.projections.iter().find(|report| report.id() == id)
    }

    fn document_request(&self) -> Option<String> {
        let selected = self.selected.as_ref()?;
        match &self.document {
            Some(document) if &document.skill == selected => None,
            _ => Some(selected.clone()),
        }
    }

    fn set_document(&mut self, skill: String, markdown: String) {
        if self.selected.as_deref() != Some(skill.as_str()) {
            return;
        }
        self.document = Some(LoadedDocument { skill, markdown });
        self.document_revision = self.document_revision.wrapping_add(1);
    }

    fn set_search_query(&mut self, query: String) {
        if self.search_query == query {
            return;
        }
        self.search_query = query;
        self.rebuild_visible_skills();
        if !self
            .selected
            .as_ref()
            .is_some_and(|selected| self.visible_skills.iter().any(|name| name == selected))
        {
            self.selected = self.visible_skills.first().cloned();
            self.document = None;
            self.document_revision = self.document_revision.wrapping_add(1);
            self.detail_viewport.home();
        }
        self.catalog_viewport.home();
    }

    fn rebuild_visible_skills(&mut self) {
        let Some(snapshot) = &self.snapshot else {
            self.visible_skills.clear();
            return;
        };
        if self.search_query.is_empty() {
            self.visible_skills = snapshot
                .skills
                .iter()
                .map(|status| status.skill.name.as_str().to_owned())
                .collect();
            return;
        }
        let mut matcher = Matcher::case_insensitive(&self.search_query);
        let mut matches = snapshot
            .skills
            .iter()
            .enumerate()
            .filter_map(|(index, status)| {
                matcher
                    .score(status.skill.name.as_str())
                    .map(|score| (score, index, status.skill.name.as_str().to_owned()))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|(score, index, _)| (*score, *index));
        self.visible_skills = matches.into_iter().map(|(_, _, name)| name).collect();
    }
}
