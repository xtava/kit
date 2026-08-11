use std::time::Duration;

use anyhow::{Context as _, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{
    layout::{Position, Rect},
    style::{Modifier, Style},
};
use tokio::task::JoinSet;

use crate::tui::{
    theme::NORD, ActionId, ActionInvocation, CommandPalette, CommandPaletteLayout,
    CommandPaletteOutcome, ContextMenu, ContextMenuLayout, ContextMenuOutcome, EventReader,
    KeyChord, KeybindingResolution, KeybindingState, NavigationHistory, NavigationMap,
    NavigationRegion, SelectableRegion, SelectionOutcome, Session, SessionOptions, SettingsEditor,
    SettingsFlow, SplitDrag, SplitFrame, SplitRatio, TextSelection,
};

use super::{
    config::{self, Config},
    contributions::{
        self, SyncAction, SyncActionContext, SyncActionRegistry, SyncRegion, DASHBOARD_ACTIONS,
        PROJECT_CONTEXT,
    },
    controller::{AddRequest, DoctorReport, ProjectReport, ProjectState, SyncController},
    engine::SessionHealth,
    model::{ProjectId, ProjectLifecycle, SyncedProject},
};

mod form;
mod operation;
mod view;

use form::{AddFormOutcome, AddProjectForm, AddProjectLayout, ConfirmationLayout};
use operation::{spawn_operation, start_operation, Operation, OperationOutcome, OperationState};
use view::render;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const MIN_PROJECTS_WIDTH: u16 = 22;
const MIN_DETAILS_WIDTH: u16 = 34;

pub(super) async fn run(controller: SyncController) -> Result<()> {
    let config = controller.load_config()?;
    let initial = controller.status(None).await;
    let (reports, notice) = match initial {
        Ok(reports) => (reports, None),
        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
    };
    let mut app = App::new(config, reports, notice)?;
    let mut terminal =
        Session::open(SessionOptions { mouse_capture: true, bracketed_paste: true })?;
    let mut events = EventReader::start();
    let mut operations = JoinSet::new();
    let mut interval = tokio::time::interval(REFRESH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    let mut regions = UiRegions::default();
    let mut selection = TextSelection::<SyncSelectionSurface>::default();

    loop {
        terminal.draw(|frame| {
            regions = render(frame, &mut app);
            let selectable = regions.selectable.clone();
            selection.capture_frame(
                frame,
                &selectable,
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
                        SelectionOutcome::Captured | SelectionOutcome::Changed => continue,
                        SelectionOutcome::Unhandled | SelectionOutcome::EdgeScroll { .. } => {}
                    }
                }
                if let Event::Mouse(mouse) = &event {
                    if selection.is_dragging() {
                        let _ = selection.on_mouse(*mouse);
                        continue;
                    }
                }
                let selection_mouse = match &event {
                    Event::Mouse(mouse) => Some(*mouse),
                    _ => None,
                };
                let flow = app.on_event(event, &regions)?;
                if matches!(&flow, Flow::Continue)
                    && matches!(&app.surface, Surface::Normal)
                    && app.menu.is_none()
                {
                    if let Some(mouse) = selection_mouse {
                        let _ = selection.on_mouse(mouse);
                    }
                }
                match flow {
                    Flow::Continue => {}
                    Flow::Quit => break,
                    Flow::Start(operation) => {
                        start_operation(
                            &mut app,
                            controller.clone(),
                            operation,
                            &mut operations,
                        );
                    }
                    Flow::ReloadSettings => {
                        match controller.load_config() {
                            Ok(config) => app.replace_config(config)?,
                            Err(error) => app.notice = Some(format!("reload settings: {error:#}")),
                        }
                    }
                }
            }
            completed = operations.join_next(), if !operations.is_empty() => {
                let outcome = match completed {
                    Some(Ok(outcome)) => outcome,
                    Some(Err(error)) => OperationOutcome::Failed(format!(
                        "Synced Projects operation task failed: {error}"
                    )),
                    None => continue,
                };
                if let Some(operation) = app.finish_operation(outcome) {
                    spawn_operation(
                        controller.clone(),
                        operation,
                        &mut operations,
                    );
                }
            }
            _ = interval.tick(), if app.operation.is_idle() => {
                start_operation(
                    &mut app,
                    controller.clone(),
                    Operation::Refresh { quiet: true },
                    &mut operations,
                );
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceKind {
    Normal,
    CommandPalette,
    Settings,
    AddProject,
    ConfirmRemove,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncSelectionSurface {
    Details,
}

enum Surface {
    Normal,
    CommandPalette(CommandPalette<SyncActionContext>),
    Settings(SettingsEditor),
    AddProject(AddProjectForm),
    ConfirmRemove { project: ProjectId, confirm: bool },
}

impl Surface {
    const fn kind(&self) -> SurfaceKind {
        match self {
            Self::Normal => SurfaceKind::Normal,
            Self::CommandPalette(_) => SurfaceKind::CommandPalette,
            Self::Settings(_) => SurfaceKind::Settings,
            Self::AddProject(_) => SurfaceKind::AddProject,
            Self::ConfirmRemove { .. } => SurfaceKind::ConfirmRemove,
        }
    }
}

enum Flow {
    Continue,
    Quit,
    Start(Operation),
    ReloadSettings,
}

#[derive(Default)]
struct UiRegions {
    split: Option<SplitFrame>,
    projects: Option<Rect>,
    details: Option<Rect>,
    project_rows: Vec<(Rect, ProjectId)>,
    action_rows: Vec<(Rect, ActionId)>,
    context_menu: Option<ContextMenuLayout>,
    command_palette: Option<CommandPaletteLayout>,
    add_project: Option<AddProjectLayout>,
    confirmation: Option<ConfirmationLayout>,
    selectable: Vec<SelectableRegion<SyncSelectionSurface>>,
}

impl UiRegions {
    fn navigation(&self) -> NavigationMap<SyncRegion> {
        NavigationMap::new([
            NavigationRegion::new(SyncRegion::Projects, self.projects.unwrap_or_default()),
            NavigationRegion::new(SyncRegion::Details, self.details.unwrap_or_default()),
        ])
    }
}

struct App {
    config: Config,
    reports: Vec<ProjectReport>,
    selected: Option<ProjectId>,
    active_region: SyncRegion,
    detail_action: usize,
    surface: Surface,
    menu: Option<ContextMenu<SyncActionContext>>,
    registry: SyncActionRegistry,
    keybinding_state: KeybindingState,
    history: NavigationHistory<ProjectId>,
    split_ratio: SplitRatio,
    split_drag: Option<SplitDrag<()>>,
    doctor: Option<DoctorReport>,
    notice: Option<String>,
    operation: OperationState,
}

impl App {
    fn new(config: Config, reports: Vec<ProjectReport>, notice: Option<String>) -> Result<Self> {
        let registry = contributions::registry(config.ui().keybindings())
            .context("build Synced Projects action registry")?;
        let selected = reports.first().map(|report| report.project.id());
        let mut history = NavigationHistory::default();
        if let Some(selected) = selected {
            history.visit(selected);
        }
        let split_ratio = config.ui().panel_ratio();
        Ok(Self {
            config,
            reports,
            selected,
            active_region: SyncRegion::Projects,
            detail_action: 0,
            surface: Surface::Normal,
            menu: None,
            registry,
            keybinding_state: KeybindingState::default(),
            history,
            split_ratio,
            split_drag: None,
            doctor: None,
            notice,
            operation: OperationState::Idle,
        })
    }

    fn replace_config(&mut self, config: Config) -> Result<()> {
        self.registry = contributions::registry(config.ui().keybindings())
            .context("rebuild Synced Projects action registry")?;
        self.split_ratio = config.ui().panel_ratio();
        self.config = config;
        self.notice = Some("Settings applied".to_owned());
        Ok(())
    }

    fn finish_operation(&mut self, outcome: OperationOutcome) -> Option<Operation> {
        let pending = self.operation.complete();
        match outcome {
            OperationOutcome::Reports { reports, notice, select, quiet } => {
                self.replace_reports(reports, select);
                if !quiet || self.notice.is_none() {
                    self.notice = notice;
                }
            }
            OperationOutcome::Upsert { report, notice, select } => {
                let selected = select.then(|| report.project.id());
                self.upsert_report(report, selected);
                self.notice = Some(notice);
            }
            OperationOutcome::Removed { project, notice } => {
                self.remove_report(project.id());
                self.notice = Some(notice);
            }
            OperationOutcome::Doctor(report) => {
                self.notice =
                    report.next_action.clone().or_else(|| Some("Everything is ready".to_owned()));
                self.doctor = Some(report);
            }
            OperationOutcome::Failed(error) => {
                self.notice = Some(error);
            }
        }
        pending
    }

    fn replace_reports(&mut self, reports: Vec<ProjectReport>, preferred: Option<ProjectId>) {
        let previous = self.selected;
        self.reports = reports;
        self.selected = preferred
            .or(previous)
            .filter(|id| self.report(*id).is_some())
            .or_else(|| self.reports.first().map(|report| report.project.id()));
        if self.selected != previous {
            self.doctor = None;
            if let Some(selected) = self.selected {
                self.history.visit(selected);
            }
        }
        self.detail_action = 0;
    }

    fn upsert_report(&mut self, report: ProjectReport, selected: Option<ProjectId>) {
        if let Some(index) =
            self.reports.iter().position(|current| current.project.id() == report.project.id())
        {
            self.reports[index] = report;
        } else {
            self.reports.push(report);
        }
        let reports = std::mem::take(&mut self.reports);
        self.replace_reports(reports, selected);
    }

    fn remove_report(&mut self, project: ProjectId) {
        self.reports.retain(|report| report.project.id() != project);
        let reports = std::mem::take(&mut self.reports);
        self.replace_reports(reports, None);
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
            SurfaceKind::CommandPalette => return self.on_palette_event(event, regions),
            SurfaceKind::Settings => return self.on_settings_event(event),
            SurfaceKind::AddProject => return self.on_add_event(event, regions),
            SurfaceKind::ConfirmRemove => return self.on_confirm_event(event, regions),
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
        if self.active_region == SyncRegion::Details {
            match key.code {
                KeyCode::Up => {
                    self.move_detail_action(-1, regions);
                    return Ok(Flow::Continue);
                }
                KeyCode::Down => {
                    self.move_detail_action(1, regions);
                    return Ok(Flow::Continue);
                }
                KeyCode::Enter => {
                    if let Some(invocation) = self.detail_invocation(regions) {
                        return self.invoke(invocation, regions);
                    }
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
            KeybindingResolution::Pending => Ok(Flow::Continue),
            KeybindingResolution::Unmatched | KeybindingResolution::UnmatchedSequence { .. } => {
                Ok(Flow::Continue)
            }
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
                if let Some((_, id)) =
                    regions.project_rows.iter().find(|(area, _)| area.contains(position))
                {
                    self.select(*id, true);
                    self.active_region = SyncRegion::Projects;
                    return Ok(Flow::Continue);
                }
                if let Some((_, action)) =
                    regions.action_rows.iter().find(|(area, _)| area.contains(position))
                {
                    let context = self.action_context(regions);
                    return self.invoke(ActionInvocation::new(*action, context), regions);
                }
                if let Some(region) = regions.navigation().hit_test(mouse.column, mouse.row) {
                    self.active_region = region;
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some((_, id)) =
                    regions.project_rows.iter().find(|(area, _)| area.contains(position))
                {
                    self.select(*id, true);
                    let context = self.action_context(regions);
                    let actions = self.registry.resolve_menu(PROJECT_CONTEXT, &context);
                    self.menu = ContextMenu::open(position, context, actions);
                }
            }
            MouseEventKind::ScrollUp
                if regions.projects.is_some_and(|area| area.contains(position)) =>
            {
                self.move_project(-3);
            }
            MouseEventKind::ScrollDown
                if regions.projects.is_some_and(|area| area.contains(position)) =>
            {
                self.move_project(3);
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
                if drag.changed(self.split_ratio) {
                    if let Err(error) = self.config.set_panel_ratio(self.split_ratio) {
                        let (_, original) = drag.cancel();
                        self.split_ratio = original;
                        self.notice = Some(format!("save panel width: {error:#}"));
                    }
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

    fn on_add_event(&mut self, event: Event, regions: &UiRegions) -> Result<Flow> {
        let Some(layout) = regions.add_project.as_ref() else {
            self.surface = Surface::Normal;
            return Ok(Flow::Continue);
        };
        let outcome = match &mut self.surface {
            Surface::AddProject(form) => form.on_event(event, layout),
            _ => unreachable!("add surface checked above"),
        };
        match outcome {
            AddFormOutcome::Captured => Ok(Flow::Continue),
            AddFormOutcome::Cancelled => {
                self.surface = Surface::Normal;
                Ok(Flow::Continue)
            }
            AddFormOutcome::Submit(request) => {
                self.surface = Surface::Normal;
                Ok(Flow::Start(Operation::Add(request)))
            }
        }
    }

    fn on_confirm_event(&mut self, event: Event, regions: &UiRegions) -> Result<Flow> {
        let Some(layout) = regions.confirmation.as_ref() else {
            self.surface = Surface::Normal;
            return Ok(Flow::Continue);
        };
        let Surface::ConfirmRemove { project, confirm } = &self.surface else {
            unreachable!("confirmation surface checked above")
        };
        let project = *project;
        let confirm = *confirm;
        match event {
            Event::Key(key) if !key.is_press() => {}
            Event::Key(KeyEvent { code: KeyCode::Esc, .. }) => {
                self.surface = Surface::Normal;
            }
            Event::Key(KeyEvent {
                code: KeyCode::Left | KeyCode::Right | KeyCode::Tab, ..
            }) => {
                if let Surface::ConfirmRemove { confirm, .. } = &mut self.surface {
                    *confirm = !*confirm;
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                if confirm {
                    let selector =
                        self.report(project).map(|report| report.project.name().to_owned());
                    self.surface = Surface::Normal;
                    if let Some(selector) = selector {
                        return Ok(Flow::Start(Operation::Remove { selector }));
                    }
                } else {
                    self.surface = Surface::Normal;
                }
            }
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                ..
            }) => {
                let position = Position { x: column, y: row };
                if layout.confirm.contains(position) {
                    let selector =
                        self.report(project).map(|report| report.project.name().to_owned());
                    self.surface = Surface::Normal;
                    if let Some(selector) = selector {
                        return Ok(Flow::Start(Operation::Remove { selector }));
                    }
                } else if layout.cancel.contains(position) {
                    self.surface = Surface::Normal;
                }
            }
            _ => {}
        }
        Ok(Flow::Continue)
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
        invocation: ActionInvocation<SyncActionContext>,
        regions: &UiRegions,
    ) -> Result<Flow> {
        let command = match self.registry.command_for(&invocation) {
            Ok(command) => command,
            Err(error) => {
                self.notice = Some(error.to_string());
                return Ok(Flow::Continue);
            }
        };
        let selected = self.selected_report();
        Ok(match command {
            SyncAction::AddProject => {
                self.surface = Surface::AddProject(AddProjectForm::new()?);
                Flow::Continue
            }
            SyncAction::TogglePause => selected.map_or(Flow::Continue, |report| {
                Flow::Start(Operation::TogglePause {
                    selector: report.project.name().to_owned(),
                    paused: report.project.lifecycle() == ProjectLifecycle::Paused,
                })
            }),
            SyncAction::Flush => selected.map_or(Flow::Continue, |report| {
                Flow::Start(Operation::Flush { selector: report.project.name().to_owned() })
            }),
            SyncAction::Doctor => Flow::Start(Operation::Doctor {
                selector: selected.map(|report| report.project.name().to_owned()),
            }),
            SyncAction::Remove => {
                if let Some(project) = invocation.context.target {
                    self.surface = Surface::ConfirmRemove { project, confirm: false };
                }
                Flow::Continue
            }
            SyncAction::Refresh => Flow::Start(Operation::Refresh { quiet: false }),
            SyncAction::OpenSettings => {
                self.surface = Surface::Settings(SettingsEditor::open(
                    self.config.store(),
                    vec![config::settings()],
                    NORD,
                ));
                Flow::Continue
            }
            SyncAction::PreviousProject => {
                self.move_project(-1);
                Flow::Continue
            }
            SyncAction::NextProject => {
                self.move_project(1);
                Flow::Continue
            }
            SyncAction::HistoryBack => {
                self.move_history(-1);
                Flow::Continue
            }
            SyncAction::HistoryForward => {
                self.move_history(1);
                Flow::Continue
            }
            SyncAction::FocusNext => {
                if let Some(region) = regions.navigation().next(self.active_region) {
                    self.active_region = region;
                }
                Flow::Continue
            }
            SyncAction::FocusPrevious => {
                if let Some(region) = regions.navigation().previous(self.active_region) {
                    self.active_region = region;
                }
                Flow::Continue
            }
            SyncAction::OpenCommandPalette => {
                self.surface = Surface::CommandPalette(CommandPalette::open(
                    invocation.context,
                    &self.registry,
                ));
                Flow::Continue
            }
            SyncAction::Quit => Flow::Quit,
        })
    }

    fn action_context(&self, regions: &UiRegions) -> SyncActionContext {
        let selected_index = self.selected_index();
        let navigation = regions.navigation();
        SyncActionContext {
            target: self.selected,
            state: self.selected_report().map(|report| report.state),
            region: self.active_region,
            busy: self.operation.is_busy(),
            can_previous: selected_index.is_some_and(|index| index > 0),
            can_next: selected_index.is_some_and(|index| index + 1 < self.reports.len()),
            can_history_back: self.history.target(-1).is_some(),
            can_history_forward: self.history.target(1).is_some(),
            can_focus_next: navigation.next(self.active_region).is_some(),
            can_focus_previous: navigation.previous(self.active_region).is_some(),
        }
    }

    fn detail_invocation(
        &self,
        regions: &UiRegions,
    ) -> Option<ActionInvocation<SyncActionContext>> {
        let context = self.action_context(regions);
        self.registry
            .resolve_menu(DASHBOARD_ACTIONS, &context)
            .items()
            .get(self.detail_action)
            .map(|action| ActionInvocation::new(action.id, context))
    }

    fn move_detail_action(&mut self, delta: isize, regions: &UiRegions) {
        let context = self.action_context(regions);
        let count = self.registry.resolve_menu(DASHBOARD_ACTIONS, &context).len();
        if count > 0 {
            self.detail_action =
                (self.detail_action as isize + delta).rem_euclid(count as isize) as usize;
        }
    }

    fn move_project(&mut self, delta: isize) {
        let Some(index) = self.selected_index() else {
            if let Some(first) = self.reports.first() {
                self.select(first.project.id(), true);
            }
            return;
        };
        let next = index.saturating_add_signed(delta).min(self.reports.len().saturating_sub(1));
        if let Some(report) = self.reports.get(next) {
            self.select(report.project.id(), true);
        }
    }

    fn move_history(&mut self, delta: isize) {
        while let Some((cursor, target)) =
            self.history.target(delta).map(|(cursor, id)| (cursor, *id))
        {
            self.history.select(cursor);
            if self.report(target).is_some() {
                self.select(target, false);
                return;
            }
        }
    }

    fn select(&mut self, id: ProjectId, record: bool) {
        if self.report(id).is_none() || self.selected == Some(id) {
            return;
        }
        self.selected = Some(id);
        self.detail_action = 0;
        self.doctor = None;
        if record {
            self.history.visit(id);
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let selected = self.selected?;
        self.reports.iter().position(|report| report.project.id() == selected)
    }

    fn selected_report(&self) -> Option<&ProjectReport> {
        self.selected.and_then(|id| self.report(id))
    }

    fn report(&self, id: ProjectId) -> Option<&ProjectReport> {
        self.reports.iter().find(|report| report.project.id() == id)
    }
}
