use std::{collections::HashSet, ffi::OsString, path::Path, time::SystemTime};

use ::time::{format_description::well_known::Rfc3339, OffsetDateTime};
use anyhow::{anyhow, Result};
#[cfg(test)]
use crossterm::event::KeyModifiers;
use crossterm::event::{Event, KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap},
    Frame,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time,
};
use unicode_width::UnicodeWidthStr;

use super::{
    actions::{self, DeployActionContext, DeployActionMode, DeployActionRegistry, DeployCommand},
    annotations::{Annotation, AnnotationStore, DeployAnnotations},
    cloudflare::{
        CloudflareDeployment, CloudflareEnvironment, CloudflarePagesClient, CloudflareStageStatus,
        CloudflareVersions,
    },
    config::{DeployAction, DeployTarget, LoadedPlan},
    journal::{DeployJournal, JournalEntry, JournalStatus, JournalStore, VersionId},
    layout::{DeployLayout, LayoutFrame, LayoutStore, SplitSurface},
    orchestration::{prepare_run, target_environment, target_working_dir},
    runner::{self, OutputStream, RunOperation, RunOutcome, RunSpec},
    state::{
        ActiveRegion, App, Modal, ModalResult, Phase, ProgressStatus, RunIntent, VersionsSource,
        VersionsState,
    },
};
#[cfg(test)]
use super::{orchestration::ensure_preview_branch, runner::RunTargetSpec};
use crate::framework::{
    process::ProcessSupervisor, start_external, ExternalCommand, ExternalTarget,
};
use crate::onepassword::OpClient;
use crate::tailscale::find_login_url;
use crate::tui::{
    render_split_divider, render_vertical_scrollbar, Direction, EventReader, KeyChord,
    KeybindingResolution, KeybindingState, NavigationMap, NavigationRegion, ScrollbarDrag,
    ScrollbarLayout, ScrollbarStyle, SelectableRegion, SelectionOutcome, Session, SessionOptions,
    SplitDividerStyle, TextSelection, ViewportMetrics,
};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const CYAN: Color = Color::Rgb(136, 192, 208);
const GREEN: Color = Color::Rgb(163, 190, 140);
const YELLOW: Color = Color::Rgb(235, 203, 139);
const RED: Color = Color::Rgb(191, 97, 106);
const MAGENTA: Color = Color::Rgb(180, 142, 173);
const TEXT: Color = Color::Rgb(216, 222, 233);
const MUTED: Color = Color::Rgb(129, 139, 157);
const BORDER: Color = Color::Rgb(76, 86, 106);
const SELECTED: Color = Color::Rgb(59, 66, 82);

pub struct Startup {
    pub processes: ProcessSupervisor,
    pub loaded: LoadedPlan,
    pub journal_store: JournalStore,
    pub journal: DeployJournal,
    pub annotation_store: AnnotationStore,
    pub annotations: DeployAnnotations,
    pub layout_store: LayoutStore,
    pub layout: DeployLayout,
    pub layout_warning: Option<String>,
}

pub async fn run(startup: Startup) -> Result<Option<RunOutcome>> {
    let Startup {
        processes,
        loaded,
        journal_store,
        journal,
        annotation_store,
        annotations,
        layout_store,
        layout,
        layout_warning,
    } = startup;
    let mut session =
        Session::open(SessionOptions { mouse_capture: true, bracketed_paste: false })?;
    let mut events = EventReader::start();
    let mut app = App::new(loaded, journal, annotations, layout);
    let mut interaction = DeployInteraction::new()?;
    app.notice = layout_warning;
    let (_idle_tx, mut run_events) = tokio::sync::mpsc::channel(1);
    let (backend_tx, mut backend_events) = mpsc::channel(8);
    let mut cancel: Option<watch::Sender<bool>> = None;
    let mut run_handle: Option<JoinHandle<()>> = None;
    let mut versions_handle: Option<JoinHandle<()>> = None;
    let mut preparation_handle: Option<(u64, JoinHandle<()>)> = None;
    let mut external_open_handle: Option<JoinHandle<()>> = None;
    let mut opened_tailscale_auth_urls = HashSet::new();
    let mut next_preparation_id = 0_u64;
    let mut run_active = false;
    let mut quit_after_run = false;
    let mut fatal_error = None;
    let mut layout_dirty = false;
    let mut tick = time::interval(std::time::Duration::from_millis(90));

    loop {
        session.draw(|frame| {
            render(frame, &mut app, journal_store.path().as_path(), &mut interaction)
        })?;

        tokio::select! {
            _ = tick.tick() => {
                if matches!(app.phase, Phase::Preparing | Phase::Running)
                    || matches!(app.versions, VersionsState::CloudflareLoading)
                {
                    app.spinner = app.spinner.wrapping_add(1);
                }
            }
            event = run_events.recv(), if run_active => {
                match event {
                    Some(event) => {
                        let finished = matches!(event, runner::RunEvent::Finished { .. });
                        if let runner::RunEvent::Output { line, .. } = &event {
                            if let Some(url) = extract_tailscale_auth_url(line) {
                                if opened_tailscale_auth_urls.insert(url.clone())
                                    && external_open_handle.is_none()
                                {
                                    app.notice = Some("Opening Tailscale authentication in Chrome…".to_owned());
                                    external_open_handle = Some(spawn_tailscale_auth_open(
                                        processes.clone(),
                                        app.loaded.base_dir.clone(),
                                        url,
                                        backend_tx.clone(),
                                    ));
                                }
                            }
                        }
                        app.ingest(event);
                        if finished {
                            run_active = false;
                            if !matches!(app.active_operation, Some(RunOperation::CloudflarePagesRollback { .. })) {
                                if let Err(error) = persist_run(&mut app, &journal_store) {
                                    app.notice = Some(format!("Could not record deploy Journal: {error:#}"));
                                    app.outcome = Some(RunOutcome::Failed);
                                    fatal_error = Some(error);
                                }
                            }
                            if quit_after_run {
                                break;
                            }
                        }
                    }
                    None => {
                        fatal_error = Some(anyhow!("deploy runner stopped before reporting a Summary"));
                        break;
                    }
                }
            }
            event = backend_events.recv() => {
                match event {
                    Some(BackendEvent::VersionsLoaded { target_id, result }) => {
                        app.set_cloudflare_versions(target_id, result);
                    }
                    Some(BackendEvent::Deleted { short_id, result }) => match result {
                        Ok(()) => {
                            app.notice = Some(format!("Deleted deployment {short_id}. Refreshing…"));
                            if let Some(handle) = versions_handle.take() {
                                handle.abort();
                            }
                            match spawn_versions_load(&app, backend_tx.clone()) {
                                Ok(handle) => versions_handle = Some(handle),
                                Err(error) => app.notice = Some(format!("{error:#}")),
                            }
                        }
                        Err(error) => {
                            app.notice = Some(format!("Could not delete {short_id}: {error}"));
                        }
                    },
                    Some(BackendEvent::RunPrepared { preparation_id, result }) => {
                        let current = preparation_handle
                            .as_ref()
                            .is_some_and(|(active_id, _)| *active_id == preparation_id);
                        if current {
                            preparation_handle = None;
                            if app.phase == Phase::Preparing {
                                match result {
                                    Ok(spec) => {
                                        app.begin_run(&spec);
                                        let (receiver, cancel_tx, handle) =
                                            runner::spawn_with_supervisor(processes.clone(), spec);
                                        run_events = receiver;
                                        cancel = Some(cancel_tx);
                                        run_handle = Some(handle);
                                        run_active = true;
                                    }
                                    Err(error) => {
                                        app.fail_run_preparation(error);
                                    }
                                }
                            }
                        }
                    }
                    Some(BackendEvent::ExternalOpenFinished { url, result }) => {
                        if let Some(handle) = external_open_handle.take() {
                            let _ = handle.await;
                        }
                        app.notice = Some(match result {
                            Ok(()) => format!("Opened {url}"),
                            Err(error) => format!("Could not open {url}: {error}"),
                        });
                    }
                    None => {}
                }
            }
            event = events.recv() => {
                let action = match event {
                    Some(event) => handle_event(event, &mut app, &mut interaction),
                    None => {
                        if run_active {
                            if let Some(cancel) = &cancel {
                                let _ = cancel.send(true);
                            }
                            quit_after_run = true;
                            continue;
                        } else {
                            break;
                        }
                    },
                };

                match action {
                    UiAction::None => {}
                    UiAction::Copy(text) => session.copy(&text)?,
                    UiAction::Quit => break,
                    UiAction::Cancel => {
                        if let Some(cancel) = &cancel {
                            let _ = cancel.send(true);
                            app.notice = Some("Cancelling the active Step…".to_owned());
                        }
                    }
                    UiAction::CancelPreparation => {
                        if let Some((_, handle)) = preparation_handle.take() {
                            handle.abort();
                        }
                        app.back_from_review();
                    }
                    UiAction::LoadVersions => {
                        if let Some(handle) = versions_handle.take() {
                            handle.abort();
                        }
                        match spawn_versions_load(&app, backend_tx.clone()) {
                            Ok(handle) => versions_handle = Some(handle),
                            Err(error) => {
                                let target_id = app
                                    .focused_target()
                                    .map(|target| target.id.clone())
                                    .unwrap_or_default();
                                app.set_cloudflare_versions(target_id, Err(format!("{error:#}")));
                            }
                        }
                    }
                    UiAction::PersistLayout => {
                        layout_dirty = true;
                        persist_layout(&mut app, &layout_store, &mut layout_dirty);
                    }
                    UiAction::PersistAnnotations => {
                        if let Err(error) = annotation_store.save(&app.annotations) {
                            app.notice = Some(format!("Could not save annotations: {error:#}"));
                        }
                    }
                    UiAction::DeleteDeployment { deployment_id, short_id } => {
                        app.notice = Some(format!("Deleting deployment {short_id}…"));
                        match spawn_delete(&app, deployment_id, short_id, backend_tx.clone()) {
                            Ok(handle) => {
                                if let Some(previous) = versions_handle.replace(handle) {
                                    previous.abort();
                                }
                            }
                            Err(error) => app.notice = Some(format!("{error:#}")),
                        }
                    }
                    UiAction::OpenUrl { url } => {
                        if external_open_handle.is_some() {
                            app.notice = Some("Another external target is still opening".into());
                        } else {
                            app.notice = Some(format!("Opening {url}…"));
                            external_open_handle = Some(spawn_external_open(
                                processes.clone(),
                                url,
                                backend_tx.clone(),
                            ));
                        }
                    }
                    UiAction::Start => {
                        match app.intent.clone() {
                            Some(RunIntent::CloudflarePagesRollback {
                                target_index,
                                deployment,
                            }) => {
                                let target = app
                                    .loaded
                                    .plan
                                    .targets
                                    .get(target_index)
                                    .cloned()
                                    .ok_or_else(|| anyhow!("selected Cloudflare Pages Target no longer exists"))?;
                                let environment = target_environment(&app.loaded, &target.id)?;
                                app.begin_cloudflare_rollback(&target, &deployment);
                                let (receiver, cancel_tx, handle) =
                                    spawn_cloudflare_rollback(target, environment, deployment.id);
                                run_events = receiver;
                                cancel = Some(cancel_tx);
                                run_handle = Some(handle);
                                run_active = true;
                            }
                            Some(intent) => {
                                if preparation_handle.is_none() {
                                    next_preparation_id = next_preparation_id.wrapping_add(1);
                                    let handle = spawn_run_preparation(
                                        &app,
                                        intent,
                                        journal_store.clone(),
                                        next_preparation_id,
                                        backend_tx.clone(),
                                    );
                                    app.begin_run_preparation();
                                    preparation_handle = Some((next_preparation_id, handle));
                                }
                            }
                            None => return Err(anyhow!("no deploy Run is selected")),
                        }
                    }
                }
            }
        }
    }

    if run_active {
        if let Some(cancel) = &cancel {
            let _ = cancel.send(true);
        }
    }
    if let Some(handle) = run_handle {
        let _ = handle.await;
    }
    if let Some(handle) = versions_handle {
        handle.abort();
    }
    if let Some((_, handle)) = preparation_handle {
        handle.abort();
    }
    if let Some(handle) = external_open_handle {
        handle.abort();
        let _ = handle.await;
    }
    let layout_exit_warning = if layout_dirty {
        layout_store
            .save(app.layout)
            .err()
            .map(|error| format!("could not save panel layout after retry: {error}"))
    } else {
        None
    };
    drop(session);

    if let Some(warning) = layout_exit_warning {
        eprintln!("kit deploy: warning: {warning}");
    }

    if let Some(error) = fatal_error {
        return Err(error);
    }
    Ok(app.outcome)
}

enum UiAction {
    None,
    Copy(String),
    Quit,
    Start,
    Cancel,
    CancelPreparation,
    LoadVersions,
    PersistLayout,
    PersistAnnotations,
    DeleteDeployment { deployment_id: String, short_id: String },
    OpenUrl { url: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeploySurface {
    Progress,
    LiveOutput,
    SummaryOutput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeployRow {
    Target(usize),
    Version(usize),
}

struct DeployInteraction {
    actions: DeployActionRegistry,
    keybinding_state: KeybindingState,
    selection: TextSelection<DeploySurface>,
    selectable: Vec<SelectableRegion<DeploySurface>>,
    rows: Vec<(Rect, DeployRow)>,
    scrollbars: Vec<(DeploySurface, ScrollbarLayout, ViewportMetrics)>,
    scrollbar_drag: Option<(DeploySurface, ScrollbarDrag)>,
}

impl DeployInteraction {
    fn new() -> Result<Self> {
        Ok(Self {
            actions: actions::registry()?,
            keybinding_state: KeybindingState::default(),
            selection: TextSelection::default(),
            selectable: Vec::new(),
            rows: Vec::new(),
            scrollbars: Vec::new(),
            scrollbar_drag: None,
        })
    }
}

fn handle_event(event: Event, app: &mut App, interaction: &mut DeployInteraction) -> UiAction {
    match event {
        Event::Key(key) => handle_key(key, app, interaction),
        Event::Mouse(mouse) => handle_mouse(mouse, app, interaction),
        Event::Resize(_, _) => {
            app.cancel_layout_drag();
            interaction.keybinding_state = KeybindingState::default();
            interaction.scrollbar_drag = None;
            UiAction::None
        }
        Event::FocusGained | Event::FocusLost | Event::Paste(_) => UiAction::None,
    }
}

fn handle_mouse(mouse: MouseEvent, app: &mut App, interaction: &mut DeployInteraction) -> UiAction {
    if let Some((surface, drag)) = interaction.scrollbar_drag {
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((layout, metrics)) =
                    interaction.scrollbars.iter().find_map(|(candidate, layout, metrics)| {
                        (*candidate == surface).then_some((*layout, *metrics))
                    })
                {
                    set_surface_top(
                        app,
                        surface,
                        drag.top_for_row(layout, mouse.row),
                        metrics.max_top(),
                    );
                }
            }
            MouseEventKind::Up(MouseButton::Left) => interaction.scrollbar_drag = None,
            _ => {}
        }
        return UiAction::None;
    }
    if interaction.selection.is_dragging() && app.layout_drag.is_none() {
        return selection_mouse(mouse, app, interaction);
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let position = Position::new(mouse.column, mouse.row);
            if let Some((surface, layout, metrics)) = interaction
                .scrollbars
                .iter()
                .find(|(_, layout, _)| layout.contains(position))
                .copied()
            {
                interaction.selection.clear();
                if let Some(drag) = ScrollbarDrag::begin(layout, position) {
                    interaction.scrollbar_drag = Some((surface, drag));
                } else {
                    set_surface_top(
                        app,
                        surface,
                        layout.top_for_track_row(mouse.row),
                        metrics.max_top(),
                    );
                }
                return UiAction::None;
            }
            if let Some(url) = clicked_tailscale_auth_url(mouse.column, mouse.row, app) {
                return UiAction::OpenUrl { url };
            }
            if !app.begin_layout_drag(mouse.column, mouse.row) {
                if let Some(row) = interaction
                    .rows
                    .iter()
                    .find_map(|(area, row)| area.contains(position).then_some(*row))
                {
                    match row {
                        DeployRow::Target(index) => {
                            app.move_cursor(index as isize - app.cursor as isize)
                        }
                        DeployRow::Version(index) => {
                            app.move_history_cursor(index as isize - app.history_cursor as isize)
                        }
                    }
                    app.set_active_region(ActiveRegion::Primary);
                    interaction.selection.clear();
                    return UiAction::None;
                }
                if let Some(region) = navigation(app).hit_test(mouse.column, mouse.row) {
                    app.set_active_region(region);
                }
                return selection_mouse(mouse, app, interaction);
            }
            UiAction::None
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.layout_drag.is_some() {
                app.update_layout_drag(mouse.column);
                UiAction::None
            } else {
                selection_mouse(mouse, app, interaction)
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if app.finish_layout_drag() {
                UiAction::PersistLayout
            } else {
                selection_mouse(mouse, app, interaction)
            }
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let delta = if mouse.kind == MouseEventKind::ScrollUp { -1 } else { 1 };
            let position = Position::new(mouse.column, mouse.row);
            if let Some(surface) = interaction
                .selectable
                .iter()
                .find_map(|region| region.area.contains(position).then_some(region.id))
            {
                scroll_surface(app, surface, delta);
                interaction.selection.clear();
                return UiAction::None;
            }
            if let Some(region) = navigation(app).hit_test(mouse.column, mouse.row) {
                app.set_active_region(region);
                scroll_or_select(app, delta);
                interaction.selection.clear();
            }
            UiAction::None
        }
        MouseEventKind::Down(_)
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Moved
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => UiAction::None,
    }
}

fn set_surface_top(app: &mut App, surface: DeploySurface, top: usize, maximum: usize) {
    match surface {
        DeploySurface::Progress => {
            app.primary_scroll = u16::try_from(top).unwrap_or(u16::MAX);
        }
        DeploySurface::LiveOutput | DeploySurface::SummaryOutput => {
            app.secondary_scroll = u16::try_from(maximum.saturating_sub(top)).unwrap_or(u16::MAX);
        }
    }
}

fn selection_mouse(
    mouse: MouseEvent,
    app: &mut App,
    interaction: &mut DeployInteraction,
) -> UiAction {
    match interaction.selection.on_mouse(mouse) {
        SelectionOutcome::CopyReady(text) => UiAction::Copy(text),
        SelectionOutcome::EdgeScroll { surface, lines } => {
            scroll_surface(app, surface, lines);
            UiAction::None
        }
        SelectionOutcome::Captured | SelectionOutcome::Changed | SelectionOutcome::Unhandled => {
            UiAction::None
        }
    }
}

fn scroll_surface(app: &mut App, surface: DeploySurface, lines: isize) {
    match surface {
        DeploySurface::Progress => {
            app.set_active_region(ActiveRegion::Primary);
            let maximum = scroll_limit(app, ActiveRegion::Primary);
            app.scroll_active_region(lines, maximum);
        }
        DeploySurface::LiveOutput => {
            app.set_active_region(ActiveRegion::Secondary);
            let maximum = scroll_limit(app, ActiveRegion::Secondary);
            app.scroll_active_region(lines, maximum);
        }
        DeploySurface::SummaryOutput => app.scroll_summary_log(lines),
    }
}

fn handle_key(key: KeyEvent, app: &mut App, interaction: &mut DeployInteraction) -> UiAction {
    match interaction.selection.on_key(key) {
        SelectionOutcome::CopyReady(text) => return UiAction::Copy(text),
        SelectionOutcome::Captured | SelectionOutcome::Changed => return UiAction::None,
        SelectionOutcome::Unhandled | SelectionOutcome::EdgeScroll { .. } => {}
    }
    let Some(chord) = KeyChord::from_event(key) else {
        return UiAction::None;
    };
    let context = action_context(app);
    let invocation = match interaction.actions.resolve_keybinding(
        &mut interaction.keybinding_state,
        chord,
        context,
    ) {
        KeybindingResolution::Invoke(invocation) => invocation,
        KeybindingResolution::Pending | KeybindingResolution::UnmatchedSequence { .. } => {
            return UiAction::None
        }
        KeybindingResolution::Unmatched => return handle_text_input(key, app),
    };
    match interaction.actions.command_for(&invocation) {
        Ok(command) => execute_command(command, app),
        Err(error) => {
            app.notice = Some(error.to_string());
            UiAction::None
        }
    }
}

fn action_context(app: &App) -> DeployActionContext {
    let mode = match &app.modal {
        Some(Modal::ConfirmDelete { .. }) => DeployActionMode::ModalConfirm,
        Some(Modal::BranchInput { .. } | Modal::NoteInput { .. }) => DeployActionMode::ModalInput,
        None if app.layout_drag.is_some() => DeployActionMode::LayoutDrag,
        None => match app.phase {
            Phase::Browse => DeployActionMode::Browse,
            Phase::Versions => DeployActionMode::Versions,
            Phase::Review => DeployActionMode::Review,
            Phase::Preparing => DeployActionMode::Preparing,
            Phase::Running => DeployActionMode::Running,
            Phase::Summary => DeployActionMode::Summary,
        },
    };
    DeployActionContext {
        mode,
        split_available: app.layout_frame.surface.is_some(),
        cloudflare_versions: app.versions_source() == VersionsSource::CloudflarePages,
        open_url_available: match app.phase {
            Phase::Versions => app.selected_cloudflare_deployment().is_some(),
            Phase::Running => tailscale_auth_url(app).is_some(),
            Phase::Summary => summary_url(app).is_some(),
            Phase::Browse | Phase::Review | Phase::Preparing => false,
        },
    }
}

fn execute_command(command: DeployCommand, app: &mut App) -> UiAction {
    match command {
        DeployCommand::Quit => UiAction::Quit,
        DeployCommand::CancelOrQuit => {
            app.cancel_layout_drag();
            if app.phase == Phase::Running {
                UiAction::Cancel
            } else {
                UiAction::Quit
            }
        }
        DeployCommand::Escape => {
            if app.modal.is_some() {
                app.modal_cancel();
                return UiAction::None;
            }
            if app.layout_drag.is_some() {
                app.cancel_layout_drag();
                return UiAction::None;
            }
            match app.phase {
                Phase::Browse => UiAction::Quit,
                Phase::Versions => {
                    app.back_to_browse();
                    UiAction::None
                }
                Phase::Review => {
                    app.back_from_review();
                    UiAction::None
                }
                Phase::Preparing => UiAction::CancelPreparation,
                Phase::Running => UiAction::None,
                Phase::Summary => {
                    app.back_to_browse();
                    UiAction::None
                }
            }
        }
        DeployCommand::MoveUp => {
            if app.phase == Phase::Summary {
                app.scroll_summary_log(-1);
            } else {
                scroll_or_select(app, -1);
            }
            UiAction::None
        }
        DeployCommand::MoveDown => {
            if app.phase == Phase::Summary {
                app.scroll_summary_log(1);
            } else {
                scroll_or_select(app, 1);
            }
            UiAction::None
        }
        DeployCommand::MoveLeft => {
            move_active_region(app, Direction::Left);
            UiAction::None
        }
        DeployCommand::MoveRight => {
            move_active_region(app, Direction::Right);
            UiAction::None
        }
        DeployCommand::NextRegion => {
            cycle_active_region(app, false);
            UiAction::None
        }
        DeployCommand::PreviousRegion => {
            cycle_active_region(app, true);
            UiAction::None
        }
        DeployCommand::ResetLayout => {
            if app.reset_active_layout().is_some() {
                UiAction::PersistLayout
            } else {
                UiAction::None
            }
        }
        DeployCommand::ToggleSelection => {
            app.toggle_focused();
            UiAction::None
        }
        DeployCommand::ToggleAll => {
            app.toggle_all();
            UiAction::None
        }
        DeployCommand::OpenVersions => match app.open_versions() {
            VersionsSource::Journal => UiAction::None,
            VersionsSource::CloudflarePages => UiAction::LoadVersions,
        },
        DeployCommand::Preview => {
            let default_branch = app
                .focused_target()
                .map(|target| target_working_dir(&app.loaded.base_dir, target))
                .and_then(|dir| current_git_branch(&dir))
                .unwrap_or_default();
            app.open_branch_input(default_branch);
            UiAction::None
        }
        DeployCommand::Activate => {
            if app.modal.is_some() {
                return confirm_modal(app);
            }
            match app.phase {
                Phase::Browse => app.review_production_deploy(),
                Phase::Versions => app.review_rollback(),
                Phase::Review => return UiAction::Start,
                Phase::Summary => app.back_to_browse(),
                Phase::Preparing | Phase::Running => {}
            }
            UiAction::None
        }
        DeployCommand::RefreshVersions => {
            app.open_versions();
            UiAction::LoadVersions
        }
        DeployCommand::DeleteVersion => {
            app.open_confirm_delete();
            UiAction::None
        }
        DeployCommand::ToggleVersionError => match app.toggle_selected_annotation_error() {
            Some(_) => UiAction::PersistAnnotations,
            None => UiAction::None,
        },
        DeployCommand::NoteOrDecline => {
            if matches!(app.modal, Some(Modal::ConfirmDelete { .. })) {
                app.modal_cancel();
            } else {
                app.open_note_input();
            }
            UiAction::None
        }
        DeployCommand::OpenUrl => {
            let url = match app.phase {
                Phase::Versions => {
                    app.selected_cloudflare_deployment().map(|item| item.url.clone())
                }
                Phase::Running => tailscale_auth_url(app),
                Phase::Summary => summary_url(app),
                Phase::Browse | Phase::Review | Phase::Preparing => None,
            };
            url.map_or(UiAction::None, |url| UiAction::OpenUrl { url })
        }
        DeployCommand::Confirm => confirm_modal(app),
    }
}

fn confirm_modal(app: &mut App) -> UiAction {
    match app.modal_confirm() {
        ModalResult::SaveAnnotations => UiAction::PersistAnnotations,
        ModalResult::DeleteDeployment { deployment_id, short_id } => {
            UiAction::DeleteDeployment { deployment_id, short_id }
        }
        ModalResult::None | ModalResult::ReviewPreview => UiAction::None,
    }
}

fn handle_text_input(key: KeyEvent, app: &mut App) -> UiAction {
    if matches!(app.modal, Some(Modal::ConfirmDelete { .. })) {
        return UiAction::None;
    }
    match key.code {
        KeyCode::Backspace if app.modal.is_some() => app.modal_backspace(),
        KeyCode::Char(ch) if app.modal.is_some() => app.modal_push(ch),
        _ => {}
    }
    UiAction::None
}

fn navigation(app: &App) -> NavigationMap<ActiveRegion> {
    let regions = app.layout_frame.surface.into_iter().flat_map(|_| {
        [
            NavigationRegion::new(ActiveRegion::Primary, app.layout_frame.split.first),
            NavigationRegion::new(ActiveRegion::Secondary, app.layout_frame.split.second),
        ]
    });
    NavigationMap::new(regions)
}

fn move_active_region(app: &mut App, direction: Direction) -> bool {
    let Some(region) = navigation(app).neighbor(app.active_region, direction) else {
        return false;
    };
    app.set_active_region(region);
    clamp_scrolls(app);
    true
}

fn cycle_active_region(app: &mut App, reverse: bool) -> bool {
    let navigation = navigation(app);
    let region = if reverse {
        navigation.previous(app.active_region)
    } else {
        navigation.next(app.active_region)
    };
    let Some(region) = region else {
        return false;
    };
    app.set_active_region(region);
    clamp_scrolls(app);
    true
}

fn scroll_or_select(app: &mut App, delta: isize) {
    match (app.phase, app.active_region) {
        (Phase::Browse, ActiveRegion::Primary) => app.move_cursor(delta),
        (Phase::Versions, ActiveRegion::Primary) => app.move_history_cursor(delta),
        _ => {
            let maximum = scroll_limit(app, app.active_region);
            app.scroll_active_region(delta, maximum);
        }
    }
}

fn clamp_scrolls(app: &mut App) {
    app.primary_scroll = app.primary_scroll.min(scroll_limit(app, ActiveRegion::Primary));
    app.secondary_scroll = app.secondary_scroll.min(scroll_limit(app, ActiveRegion::Secondary));
}

fn scroll_limit(app: &App, region: ActiveRegion) -> u16 {
    let total = match (app.phase, region) {
        (Phase::Browse, ActiveRegion::Secondary) => {
            app.focused_target().map_or(0, |target| target.steps.len().saturating_add(3))
        }
        (Phase::Versions, ActiveRegion::Secondary) => match &app.versions {
            VersionsState::Journal => {
                app.selected_history_entry().map_or(0, |entry| version_detail(entry).len())
            }
            VersionsState::CloudflareReady { .. } => {
                app.selected_cloudflare_deployment().map_or(0, |deployment| {
                    cloudflare_version_detail(
                        deployment,
                        app.deployment_is_live(deployment),
                        app.annotation(deployment),
                    )
                    .len()
                })
            }
            VersionsState::CloudflareLoading | VersionsState::CloudflareError { .. } => 0,
        },
        (Phase::Running, ActiveRegion::Primary) => {
            app.progress.iter().fold(2usize, |total, target| {
                total.saturating_add(target.steps.len().saturating_add(1))
            })
        }
        (Phase::Running, ActiveRegion::Secondary) => app.output.len(),
        _ => 0,
    };
    let area = match region {
        ActiveRegion::Primary => app.layout_frame.split.first,
        ActiveRegion::Secondary => app.layout_frame.split.second,
    };
    let visible = usize::from(area.height.saturating_sub(2));
    u16::try_from(total.saturating_sub(visible)).unwrap_or(u16::MAX)
}

fn persist_layout(app: &mut App, store: &LayoutStore, dirty: &mut bool) {
    match store.save(app.layout) {
        Ok(()) => {
            *dirty = false;
            if app.notice.as_deref().is_some_and(|notice| {
                notice.starts_with("Could not load saved panel layout")
                    || notice.starts_with("Could not save panel layout")
            }) {
                app.notice = None;
            }
        }
        Err(error) => {
            *dirty = true;
            app.notice = Some(format!(
                "Could not save panel layout: {error}. The current layout remains active; quit to retry."
            ));
        }
    }
}

fn spawn_run_preparation(
    app: &App,
    intent: RunIntent,
    journal_store: JournalStore,
    preparation_id: u64,
    sender: mpsc::Sender<BackendEvent>,
) -> JoinHandle<()> {
    let loaded = app.loaded.clone();
    let targets = app.review_targets();
    tokio::spawn(async move {
        let result = prepare_run(&loaded, intent, targets, &journal_store)
            .await
            .map_err(|error| format!("{error:#}"));
        let _ = sender.send(BackendEvent::RunPrepared { preparation_id, result }).await;
    })
}

fn spawn_external_open(
    processes: ProcessSupervisor,
    url: String,
    sender: mpsc::Sender<BackendEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = match start_external(&processes, ExternalTarget::Url(url.clone())) {
            Ok(receipt) => receipt.completion().await,
            Err(error) => Err(error),
        }
        .map_err(|error| format!("{error:#}"));
        let _ = sender.send(BackendEvent::ExternalOpenFinished { url, result }).await;
    })
}

fn spawn_tailscale_auth_open(
    processes: ProcessSupervisor,
    working_directory: std::path::PathBuf,
    url: String,
    sender: mpsc::Sender<BackendEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = match start_external(
            &processes,
            ExternalTarget::Command(ExternalCommand {
                program: OsString::from("/usr/bin/google-chrome-stable"),
                arguments: vec![OsString::from(&url)],
                working_directory,
            }),
        ) {
            Ok(receipt) => receipt.completion().await,
            Err(error) => Err(error),
        }
        .map_err(|error| format!("{error:#}"));
        let _ = sender.send(BackendEvent::ExternalOpenFinished { url, result }).await;
    })
}

enum BackendEvent {
    VersionsLoaded { target_id: String, result: Result<CloudflareVersions, String> },
    Deleted { short_id: String, result: Result<(), String> },
    RunPrepared { preparation_id: u64, result: Result<RunSpec, String> },
    ExternalOpenFinished { url: String, result: Result<(), String> },
}

fn spawn_versions_load(app: &App, sender: mpsc::Sender<BackendEvent>) -> Result<JoinHandle<()>> {
    let target = app.focused_target().cloned().ok_or_else(|| anyhow!("no Target is selected"))?;
    let target_id = target.id.clone();
    let environment = target_environment(&app.loaded, &target.id)?;
    Ok(tokio::spawn(async move {
        let result = async {
            let client = CloudflarePagesClient::for_target(&target, &environment, &OpClient::new())
                .await?
                .ok_or_else(|| anyhow!("selected Target has no Cloudflare Pages backend"))?;
            client.load_versions().await.map_err(anyhow::Error::from)
        }
        .await
        .map_err(|error| format!("{error:#}"));
        let _ = sender.send(BackendEvent::VersionsLoaded { target_id, result }).await;
    }))
}

fn spawn_delete(
    app: &App,
    deployment_id: String,
    short_id: String,
    sender: mpsc::Sender<BackendEvent>,
) -> Result<JoinHandle<()>> {
    let target = app.focused_target().cloned().ok_or_else(|| anyhow!("no Target is selected"))?;
    let environment = target_environment(&app.loaded, &target.id)?;
    Ok(tokio::spawn(async move {
        let result = async {
            let client = CloudflarePagesClient::for_target(&target, &environment, &OpClient::new())
                .await?
                .ok_or_else(|| anyhow!("selected Target has no Cloudflare Pages backend"))?;
            client.delete_deployment(&deployment_id).await.map_err(anyhow::Error::from)
        }
        .await
        .map_err(|error| format!("{error:#}"));
        let _ = sender.send(BackendEvent::Deleted { short_id, result }).await;
    }))
}

fn spawn_cloudflare_rollback(
    target: DeployTarget,
    environment: crate::onepassword::OpEnvironment,
    deployment_id: String,
) -> (mpsc::Receiver<runner::RunEvent>, watch::Sender<bool>, JoinHandle<()>) {
    let (event_tx, event_rx) = mpsc::channel(16);
    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    let handle = tokio::spawn(async move {
        let started = time::Instant::now();
        if event_tx.send(runner::RunEvent::TargetStarted { target: 0 }).await.is_err()
            || event_tx.send(runner::RunEvent::StepStarted { target: 0, step: 0 }).await.is_err()
        {
            return;
        }

        let cancellation = async {
            loop {
                if *cancel_rx.borrow() || cancel_rx.changed().await.is_err() {
                    return;
                }
            }
        };
        tokio::pin!(cancellation);
        let rollback = async {
            let client = CloudflarePagesClient::for_target(&target, &environment, &OpClient::new())
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "selected Target has no Cloudflare Pages backend".to_owned())?;
            client.rollback(&deployment_id).await.map_err(|error| error.to_string())
        };
        tokio::pin!(rollback);
        let result = tokio::select! {
            result = &mut rollback => Some(result),
            () = &mut cancellation => None,
        };
        let elapsed = started.elapsed();
        let (step_outcome, target_outcome, run_outcome) = match result {
            Some(Ok(deployment)) => {
                let _ = event_tx
                    .send(runner::RunEvent::Output {
                        stream: OutputStream::Stdout,
                        line: format!(
                            "Cloudflare Pages rollback accepted: {}  {}",
                            deployment.short_id, deployment.url
                        ),
                    })
                    .await;
                (
                    runner::StepOutcome::Succeeded,
                    runner::TargetOutcome::Succeeded,
                    RunOutcome::Succeeded,
                )
            }
            Some(Err(error)) => (
                runner::StepOutcome::Failed(error.to_string()),
                runner::TargetOutcome::Failed,
                RunOutcome::Failed,
            ),
            None => (
                runner::StepOutcome::Cancelled,
                runner::TargetOutcome::Cancelled,
                RunOutcome::Cancelled,
            ),
        };
        if event_tx
            .send(runner::RunEvent::StepFinished {
                target: 0,
                step: 0,
                outcome: step_outcome,
                elapsed,
            })
            .await
            .is_err()
        {
            return;
        }
        if event_tx
            .send(runner::RunEvent::TargetFinished {
                target: 0,
                artifact: None,
                outcome: target_outcome,
                elapsed,
            })
            .await
            .is_err()
        {
            return;
        }
        let _ = event_tx.send(runner::RunEvent::Finished { outcome: run_outcome, elapsed }).await;
    });
    (event_rx, cancel_tx, handle)
}

fn current_git_branch(working_dir: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(working_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!branch.is_empty() && branch != "HEAD").then_some(branch)
}

fn summary_url(app: &App) -> Option<String> {
    app.output.iter().rev().find_map(|line| extract_deploy_url(&line.text))
}

fn tailscale_auth_url(app: &App) -> Option<String> {
    app.output.iter().rev().find_map(|line| extract_tailscale_auth_url(&line.text))
}

fn extract_tailscale_auth_url(line: &str) -> Option<String> {
    find_login_url(line).map(|url| url.as_str().to_owned())
}

fn clicked_tailscale_auth_url(column: u16, row: u16, app: &App) -> Option<String> {
    if app.phase != Phase::Running {
        return None;
    }
    let area = app.layout_frame.split.second;
    if column <= area.x
        || column >= area.x.saturating_add(area.width).saturating_sub(1)
        || row <= area.y
        || row >= area.y.saturating_add(area.height).saturating_sub(1)
    {
        return None;
    }
    let visible_rows = usize::from(area.height.saturating_sub(2));
    let output_end = app.output.len().saturating_sub(usize::from(app.secondary_scroll));
    let output_start = output_end.saturating_sub(visible_rows);
    let visible_index = usize::from(row.saturating_sub(area.y).saturating_sub(1));
    app.output
        .get(output_start.saturating_add(visible_index))
        .and_then(|line| extract_tailscale_auth_url(&line.text))
}

fn extract_deploy_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let url: String = line[start..].chars().take_while(|ch| !ch.is_whitespace()).collect();
    url.contains(".pages.dev").then_some(url)
}

fn persist_run(app: &mut App, store: &JournalStore) -> Result<()> {
    let timestamp_secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| anyhow!("system clock is before the Unix epoch"))?
        .as_secs();
    let entries = app.journal_entries(timestamp_secs);
    if entries.is_empty() {
        return Ok(());
    }
    store.record_many(entries)?;
    app.journal = store.load()?;
    Ok(())
}

fn render(
    frame: &mut Frame<'_>,
    app: &mut App,
    journal_path: &Path,
    interaction: &mut DeployInteraction,
) {
    interaction.selectable.clear();
    interaction.rows.clear();
    interaction.scrollbars.clear();
    let areas = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(if app.notice.is_some() { 3 } else { 1 }),
    ])
    .split(frame.area());
    app.set_layout_frame(LayoutFrame::default());
    render_header(frame, areas[0], app);
    match app.phase {
        Phase::Browse => render_browse(frame, areas[1], app),
        Phase::Versions => render_versions(frame, areas[1], app),
        Phase::Review | Phase::Preparing => render_review(frame, areas[1], app),
        Phase::Running => render_running(frame, areas[1], app),
        Phase::Summary => render_summary(frame, areas[1], app, journal_path),
    }
    render_footer(frame, areas[2], app, &interaction.actions);
    if app.modal.is_some() {
        render_modal(frame, app, &interaction.actions);
    } else {
        publish_selectable_regions(areas[1], app, interaction);
    }
    render_deploy_scrollbars(frame, interaction);
    interaction.selection.capture_frame(
        frame,
        &interaction.selectable,
        Style::default().bg(SELECTED).add_modifier(Modifier::REVERSED),
    );
}

fn publish_selectable_regions(body: Rect, app: &App, interaction: &mut DeployInteraction) {
    match app.phase {
        Phase::Browse if app.layout_frame.surface == Some(SplitSurface::Browse) => {
            publish_rows(
                app.layout_frame.split.first,
                app.loaded.plan.targets.len(),
                app.cursor,
                DeployRow::Target,
                &mut interaction.rows,
            );
        }
        Phase::Versions if app.layout_frame.surface == Some(SplitSurface::Versions) => {
            let count = match &app.versions {
                VersionsState::Journal => app.history().len(),
                VersionsState::CloudflareReady { deployments, .. } => deployments.len(),
                VersionsState::CloudflareLoading | VersionsState::CloudflareError { .. } => 0,
            };
            publish_rows(
                app.layout_frame.split.first,
                count,
                app.history_cursor,
                DeployRow::Version,
                &mut interaction.rows,
            );
        }
        Phase::Running if app.layout_frame.surface == Some(SplitSurface::Running) => {
            let progress = selectable_panel_content(app.layout_frame.split.first);
            let progress_len = app.progress.iter().fold(2usize, |total, target| {
                total.saturating_add(target.steps.len().saturating_add(1))
            });
            publish_surface(
                interaction,
                DeploySurface::Progress,
                progress,
                i64::from(app.primary_scroll),
                progress_len,
                usize::from(app.primary_scroll),
                1,
            );
            let output = selectable_panel_content(app.layout_frame.split.second);
            let output_end = app.output.len().saturating_sub(usize::from(app.secondary_scroll));
            let output_start = output_end.saturating_sub(usize::from(output.height));
            publish_surface(
                interaction,
                DeploySurface::LiveOutput,
                output,
                i64::try_from(output_start).unwrap_or(i64::MAX),
                app.output.len(),
                output_start,
                1,
            );
        }
        Phase::Summary if !app.output.is_empty() => {
            let panes =
                Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                    .split(body);
            let output = selectable_panel_content(panes[1]);
            let output_end = app.output.len().saturating_sub(usize::from(app.secondary_scroll));
            let output_start = output_end.saturating_sub(usize::from(output.height));
            publish_surface(
                interaction,
                DeploySurface::SummaryOutput,
                output,
                i64::try_from(output_start).unwrap_or(i64::MAX),
                app.output.len(),
                output_start,
                1,
            );
        }
        Phase::Browse | Phase::Versions | Phase::Review | Phase::Preparing | Phase::Summary => {}
        Phase::Running => {}
    }
}

fn publish_rows(
    area: Rect,
    count: usize,
    selected: usize,
    row: impl Fn(usize) -> DeployRow,
    regions: &mut Vec<(Rect, DeployRow)>,
) {
    let capacity = usize::from(area.height.saturating_sub(2));
    let start = selected.saturating_add(1).saturating_sub(capacity);
    let inner = selectable_panel_content(area);
    for (visible, index) in (start..count).take(capacity).enumerate() {
        regions.push((
            Rect::new(
                inner.x,
                inner.y.saturating_add(u16::try_from(visible).unwrap_or(u16::MAX)),
                inner.width,
                1,
            ),
            row(index),
        ));
    }
}

fn publish_surface(
    interaction: &mut DeployInteraction,
    surface: DeploySurface,
    mut area: Rect,
    row_origin: i64,
    content_len: usize,
    top: usize,
    revision: u64,
) {
    let metrics = ViewportMetrics::new(content_len, usize::from(area.height));
    if let Some(layout) = ScrollbarLayout::vertical_right(area, metrics, top) {
        interaction.scrollbars.push((surface, layout, metrics));
        area.width = area.width.saturating_sub(1);
    }
    interaction.selectable.push(SelectableRegion::new(surface, area, row_origin, 0, revision));
}

fn render_deploy_scrollbars(frame: &mut Frame<'_>, interaction: &DeployInteraction) {
    for (surface, layout, _) in &interaction.scrollbars {
        render_vertical_scrollbar(
            frame,
            *layout,
            interaction.scrollbar_drag.is_some_and(|(dragging, _)| dragging == *surface),
            ScrollbarStyle {
                track_color: BORDER,
                thumb_color: MUTED,
                active_thumb_color: CYAN,
                track_symbol: "│",
                thumb_symbol: "┃",
            },
        );
    }
}

fn selectable_panel_content(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(1),
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    )
}

fn render_modal(frame: &mut Frame<'_>, app: &App, actions: &DeployActionRegistry) {
    let Some(modal) = &app.modal else {
        return;
    };
    let controls = action_help(app, actions);
    let (title, body) = match modal {
        Modal::BranchInput { buffer, .. } => (
            " Preview deploy · branch ",
            vec![
                Line::styled(
                    "Deploy a preview to a Cloudflare Pages branch alias.",
                    Style::default().fg(MUTED),
                ),
                Line::raw(""),
                input_line(buffer),
                Line::raw(""),
                Line::styled(controls.clone(), Style::default().fg(MUTED)),
            ],
        ),
        Modal::NoteInput { buffer, .. } => (
            " Annotate deployment · note ",
            vec![
                Line::styled("Attach a short note to this deployment.", Style::default().fg(MUTED)),
                Line::raw(""),
                input_line(buffer),
                Line::raw(""),
                Line::styled(controls.clone(), Style::default().fg(MUTED)),
            ],
        ),
        Modal::ConfirmDelete { label, .. } => (
            " Delete deployment ",
            vec![
                Line::from(vec![
                    Span::styled("Permanently delete ", Style::default().fg(TEXT)),
                    Span::styled(
                        label.clone(),
                        Style::default().fg(RED).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" from Cloudflare Pages?", Style::default().fg(TEXT)),
                ]),
                Line::raw(""),
                Line::styled(controls, Style::default().fg(MUTED)),
            ],
        ),
    };
    let area = centered_rect(frame.area(), 62, body.len() as u16 + 2);
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(CYAN))
                .padding(Padding::horizontal(1))
                .title(Span::styled(title, Style::default().fg(CYAN).add_modifier(Modifier::BOLD))),
        ),
        area,
    );
}

fn input_line(buffer: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("› ", Style::default().fg(CYAN)),
        Span::styled(buffer.to_owned(), Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        Span::styled("▌", Style::default().fg(CYAN)),
    ])
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

fn install_split_frame(app: &mut App, surface: SplitSurface, area: Rect) -> LayoutFrame {
    let layout_frame = LayoutFrame::split(surface, area, app.layout.ratio(surface));
    app.set_layout_frame(layout_frame);
    if let Some(region) = navigation(app).normalize(app.active_region) {
        app.set_active_region(region);
    }
    clamp_scrolls(app);
    layout_frame
}

fn render_layout_divider(frame: &mut Frame<'_>, layout_frame: LayoutFrame, dragging: bool) {
    render_split_divider(
        frame,
        layout_frame.split,
        dragging,
        SplitDividerStyle {
            idle_color: MUTED,
            active_color: CYAN,
            idle_line: " ",
            idle_grip: "┋",
            active_line: "┃",
        },
    );
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let phase = match app.phase {
        Phase::Browse => "targets",
        Phase::Versions => "versions",
        Phase::Review => "review",
        Phase::Preparing => "preparing",
        Phase::Running => "running",
        Phase::Summary => "summary",
    };
    let title = Line::from(vec![
        Span::styled("deploy", Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        Span::styled("  /  ", Style::default().fg(BORDER)),
        Span::styled(phase, Style::default().fg(TEXT)),
        Span::styled(
            format!("    {}", app.loaded.path.display()),
            Style::default().fg(MUTED).add_modifier(Modifier::DIM),
        ),
    ]);
    frame.render_widget(Paragraph::new(title).block(panel(" kit ")), area);
}

fn render_browse(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let layout_frame = install_split_frame(app, SplitSurface::Browse, area);

    let items = app
        .loaded
        .plan
        .targets
        .iter()
        .enumerate()
        .map(|(index, target)| {
            let selected = app.selected.get(index).copied().unwrap_or(false);
            let versions = if target.backend.is_some() {
                "Cloudflare Pages".to_owned()
            } else {
                format!("{} versions", app.journal.entries(&target.id).len())
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    if selected { "● " } else { "○ " },
                    Style::default().fg(if selected { CYAN } else { BORDER }),
                ),
                Span::styled(
                    target.name.clone(),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {} steps · {versions}", target.steps.len()),
                    Style::default().fg(MUTED),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(app.cursor));
    let list = List::new(items)
        .block(active_panel(
            format!(
                " Targets · {} selected · {} Steps ",
                app.selected_count(),
                app.selected_step_count()
            ),
            app.active_region == ActiveRegion::Primary,
        ))
        .highlight_style(Style::default().bg(SELECTED))
        .highlight_symbol("▌");
    frame.render_stateful_widget(list, layout_frame.split.first, &mut state);

    render_target_detail(
        frame,
        layout_frame.split.second,
        app.focused_target(),
        app.secondary_scroll,
        app.active_region == ActiveRegion::Secondary,
    );
    render_layout_divider(frame, layout_frame, app.layout_drag.is_some());
}

fn render_target_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    target: Option<&DeployTarget>,
    scroll: u16,
    active: bool,
) {
    let Some(target) = target else {
        frame.render_widget(
            Paragraph::new("No Targets configured").block(active_panel(" Target ", active)),
            area,
        );
        return;
    };
    let rollback = match (&target.backend, &target.rollback) {
        (Some(_), _) => Span::styled(
            " platform history + rollback ",
            Style::default().fg(MAGENTA).add_modifier(Modifier::BOLD),
        ),
        (None, Some(_)) => Span::styled(
            " rollback ready ",
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ),
        (None, None) => Span::styled(" deploy only ", Style::default().fg(YELLOW)),
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                target.name.clone(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            rollback,
        ]),
        Line::styled(
            target.description.clone().unwrap_or_else(|| "No description".to_owned()),
            Style::default().fg(MUTED),
        ),
        Line::raw(""),
    ];
    for (index, step) in target.steps.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!("{:>2}  ", index + 1), Style::default().fg(BORDER)),
            Span::styled(step.name.clone(), Style::default().fg(TEXT)),
            Span::styled(format!("  {}", action_label(&step.action)), Style::default().fg(MUTED)),
        ]));
    }
    lines.push(Line::raw(""));
    let mut actions = vec![
        Span::styled("Space", Style::default().fg(CYAN)),
        Span::styled(" select   ", Style::default().fg(MUTED)),
        Span::styled("Enter", Style::default().fg(CYAN)),
        Span::styled(" production deploy   ", Style::default().fg(MUTED)),
        Span::styled("v", Style::default().fg(CYAN)),
        Span::styled(" versions", Style::default().fg(MUTED)),
    ];
    if target.backend.is_some() {
        actions.push(Span::styled("   ", Style::default().fg(MUTED)));
        actions.push(Span::styled("p", Style::default().fg(CYAN).add_modifier(Modifier::BOLD)));
        actions.push(Span::styled(" preview deploy", Style::default().fg(MUTED)));
    }
    lines.push(Line::from(actions));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .block(active_panel(" Plan ", active)),
        area,
    );
}

fn render_versions(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let active_region = app.active_region;
    let detail_scroll = app.secondary_scroll;
    let target_name = app
        .focused_target()
        .map(|target| target.name.clone())
        .unwrap_or_else(|| "Target".to_owned());
    let has_split = match &app.versions {
        VersionsState::CloudflareReady { deployments, .. } => !deployments.is_empty(),
        VersionsState::Journal => !app.history().is_empty(),
        VersionsState::CloudflareLoading | VersionsState::CloudflareError { .. } => false,
    };
    let layout_frame = has_split.then(|| install_split_frame(app, SplitSurface::Versions, area));
    match &app.versions {
        VersionsState::CloudflareLoading => {
            let spinner = SPINNER[app.spinner % SPINNER.len()];
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        format!("{spinner}  Loading Cloudflare Pages deployments…"),
                        Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                    ),
                    Line::raw(""),
                    Line::styled(
                        "Version history is read directly from the platform.",
                        Style::default().fg(MUTED),
                    ),
                ])
                .block(panel(format!(" {target_name} · Versions "))),
                area,
            );
            return;
        }
        VersionsState::CloudflareError { message, .. } => {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        "Could not load Cloudflare Pages deployments.",
                        Style::default().fg(RED).add_modifier(Modifier::BOLD),
                    ),
                    Line::raw(""),
                    Line::styled(message.clone(), Style::default().fg(TEXT)),
                    Line::raw(""),
                    Line::styled("Press r to retry.", Style::default().fg(MUTED)),
                ])
                .wrap(Wrap { trim: false })
                .block(panel(format!(" {target_name} · Versions "))),
                area,
            );
            return;
        }
        VersionsState::CloudflareReady { .. } => {
            render_cloudflare_versions(frame, area, &target_name, app);
            return;
        }
        VersionsState::Journal => {}
    }

    let entries = app.history();
    if entries.is_empty() {
        let message = vec![
            Line::styled(
                "No recorded Versions yet.",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            Line::styled(
                "Run this Target once; every completed Run is written to the deploy Journal.",
                Style::default().fg(MUTED),
            ),
        ];
        frame.render_widget(
            Paragraph::new(message).block(panel(format!(" {target_name} · Versions "))),
            area,
        );
        return;
    }

    let Some(layout_frame) = layout_frame else {
        return;
    };
    let items =
        entries.iter().rev().map(|entry| ListItem::new(version_line(entry))).collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(app.history_cursor));
    let list = List::new(items)
        .block(active_panel(
            format!(" {target_name} · Versions "),
            active_region == ActiveRegion::Primary,
        ))
        .highlight_style(Style::default().bg(SELECTED))
        .highlight_symbol("▌");
    frame.render_stateful_widget(list, layout_frame.split.first, &mut state);

    let detail = app
        .selected_history_entry()
        .map(version_detail)
        .unwrap_or_else(|| vec![Line::styled("No Version selected", Style::default().fg(MUTED))]);
    frame.render_widget(
        Paragraph::new(detail)
            .scroll((detail_scroll, 0))
            .block(active_panel(" Recorded Run ", active_region == ActiveRegion::Secondary)),
        layout_frame.split.second,
    );
    render_layout_divider(frame, layout_frame, app.layout_drag.is_some());
}

fn render_cloudflare_versions(frame: &mut Frame<'_>, area: Rect, target_name: &str, app: &App) {
    let (deployments, live_id, production_branch) = match &app.versions {
        VersionsState::CloudflareReady { deployments, live_id, production_branch } => {
            (deployments.as_slice(), live_id.as_deref(), production_branch.as_str())
        }
        VersionsState::Journal
        | VersionsState::CloudflareLoading
        | VersionsState::CloudflareError { .. } => return,
    };
    if deployments.is_empty() {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    "No Cloudflare Pages deployments found.",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
                Line::raw(""),
                Line::styled(
                    "The project has no deployments visible to this API token.",
                    Style::default().fg(MUTED),
                ),
            ])
            .block(panel(format!(" {target_name} · Versions "))),
            area,
        );
        return;
    }

    let Some(layout_frame) =
        (app.layout_frame.surface == Some(SplitSurface::Versions)).then_some(app.layout_frame)
    else {
        return;
    };
    let items = deployments
        .iter()
        .map(|deployment| {
            let live = live_id == Some(deployment.id.as_str());
            ListItem::new(cloudflare_version_line(deployment, live, app.annotation(deployment)))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default().with_selected(Some(app.history_cursor));
    frame.render_stateful_widget(
        List::new(items)
            .block(active_panel(
                format!(" {target_name} · Cloudflare Pages · prod: {production_branch} "),
                app.active_region == ActiveRegion::Primary,
            ))
            .highlight_style(Style::default().bg(SELECTED))
            .highlight_symbol("▌"),
        layout_frame.split.first,
        &mut state,
    );
    let detail = deployments
        .get(app.history_cursor)
        .map(|deployment| {
            let live = live_id == Some(deployment.id.as_str());
            cloudflare_version_detail(deployment, live, app.annotation(deployment))
        })
        .unwrap_or_else(|| {
            vec![Line::styled("No deployment selected", Style::default().fg(MUTED))]
        });
    frame.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: false }).scroll((app.secondary_scroll, 0)).block(
            active_panel(" Platform deployment ", app.active_region == ActiveRegion::Secondary),
        ),
        layout_frame.split.second,
    );
    render_layout_divider(frame, layout_frame, app.layout_drag.is_some());
}

fn cloudflare_version_line(
    deployment: &CloudflareDeployment,
    live: bool,
    annotation: Option<&Annotation>,
) -> Line<'static> {
    let status = deployment.latest_stage.as_ref().map(|stage| stage.status);
    let (symbol, color) = cloudflare_status(status);
    let commit = deployment.commit_hash().map(short_text).unwrap_or_else(|| "no commit".to_owned());
    let mut spans = vec![
        Span::styled(format!("{symbol} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(
            deployment.short_id.clone(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {commit}  {}  {}",
                deployment.created_on,
                cloudflare_environment(deployment.environment)
            ),
            Style::default().fg(MUTED),
        ),
    ];
    if live {
        spans.push(Span::styled(
            "  ● LIVE",
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ));
    }
    if annotation.is_some_and(|annotation| annotation.error) {
        spans
            .push(Span::styled("  ⚠ ERROR", Style::default().fg(RED).add_modifier(Modifier::BOLD)));
    }
    if annotation.and_then(|annotation| annotation.note.as_deref()).is_some() {
        spans.push(Span::styled("  ✎", Style::default().fg(YELLOW)));
    }
    Line::from(spans)
}

fn cloudflare_version_detail(
    deployment: &CloudflareDeployment,
    live: bool,
    annotation: Option<&Annotation>,
) -> Vec<Line<'static>> {
    let status = deployment.latest_stage.as_ref().map(|stage| stage.status);
    let (symbol, color) = cloudflare_status(status);
    let mut header = vec![
        Span::styled(format!("{symbol} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(
            deployment.short_id.clone(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
    ];
    if live {
        header.push(Span::styled(
            "  ● LIVE",
            Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
        ));
    }
    let mut lines = vec![
        Line::from(header),
        Line::raw(""),
        detail_line("Commit", deployment.commit_hash().unwrap_or("—")),
        detail_line("Branch", deployment.branch().unwrap_or("—")),
        detail_line("Created", &deployment.created_on),
        detail_line("Environment", cloudflare_environment(deployment.environment)),
        detail_line("Status", cloudflare_status_label(status)),
    ];
    if let Some(annotation) = annotation {
        lines.push(Line::raw(""));
        if annotation.error {
            lines.push(Line::styled(
                "⚠ Marked as an error",
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ));
        }
        if let Some(note) = annotation.note.as_deref() {
            lines.push(detail_line("Note", note));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(deployment.url.clone(), Style::default().fg(CYAN)));
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        if deployment.rollback_eligible() {
            "Enter roll back  ·  e error  ·  n note  ·  d delete"
        } else {
            "e error  ·  n note  ·  d delete  ·  rollback needs a successful production deploy"
        },
        Style::default().fg(if deployment.rollback_eligible() { MAGENTA } else { MUTED }),
    ));
    lines
}

fn detail_line(label: &'static str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<12}"), Style::default().fg(MUTED)),
        Span::styled(value.to_owned(), Style::default().fg(TEXT)),
    ])
}

fn version_line(entry: &JournalEntry) -> Line<'static> {
    let (symbol, color) = journal_status(entry.status);
    Line::from(vec![
        Span::styled(format!("{symbol} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(
            entry.version.0.clone(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {}  {}",
                format_timestamp(entry.timestamp_secs),
                duration_ms(entry.duration_ms)
            ),
            Style::default().fg(MUTED),
        ),
    ])
}

fn version_detail(entry: &JournalEntry) -> Vec<Line<'static>> {
    let (symbol, color) = journal_status(entry.status);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{symbol} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                entry.version.0.clone(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::styled(format_timestamp(entry.timestamp_secs), Style::default().fg(MUTED)),
        Line::raw(""),
    ];
    for step in &entry.steps {
        lines.push(Line::from(vec![
            Span::styled("· ", Style::default().fg(BORDER)),
            Span::styled(step.name.clone(), Style::default().fg(TEXT)),
            Span::styled(
                format!("  {}", duration_ms(step.duration_ms)),
                Style::default().fg(MUTED),
            ),
        ]));
    }
    lines
}

fn render_review(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rollback = matches!(
        app.intent,
        Some(RunIntent::Rollback { .. } | RunIntent::CloudflarePagesRollback { .. })
    );
    let platform_rollback = matches!(app.intent, Some(RunIntent::CloudflarePagesRollback { .. }));
    let production = matches!(app.intent, Some(RunIntent::DeployProduction));
    let preview_branch = match &app.intent {
        Some(RunIntent::DeployPreview { branch, .. }) => Some(branch.as_str()),
        _ => None,
    };
    let accent = if rollback { MAGENTA } else { CYAN };
    let heading = if rollback {
        "Rollback plan"
    } else if production {
        "Production deployment plan"
    } else {
        "Preview deployment plan"
    };
    let mut lines = vec![
        Line::styled(heading, Style::default().fg(accent).add_modifier(Modifier::BOLD)),
        Line::styled(
            if rollback {
                if platform_rollback {
                    "Cloudflare Pages will restore the selected production deployment."
                } else {
                    "The selected recorded Version will be passed to every rollback Step."
                }
            } else if production {
                "Selected Targets will publish to their production destinations."
            } else {
                "The selected Cloudflare Pages Target will publish to a preview branch alias."
            },
            Style::default().fg(MUTED),
        ),
        Line::raw(""),
    ];
    if production {
        lines.push(detail_line("Environment", "production"));
        lines.push(Line::raw(""));
    }
    if let Some(branch) = preview_branch {
        lines.push(detail_line("Environment", "preview"));
        lines.push(detail_line("Branch", branch));
        lines.push(Line::raw(""));
    }
    if let Some(RunIntent::Rollback { version, .. }) = &app.intent {
        lines.push(Line::from(vec![
            Span::styled("Version  ", Style::default().fg(MUTED)),
            Span::styled(version.0.clone(), Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::raw(""));
    }
    if let Some(RunIntent::CloudflarePagesRollback { deployment, .. }) = &app.intent {
        lines.push(detail_line("Deployment", &deployment.short_id));
        lines.push(detail_line("Commit", deployment.commit_hash().unwrap_or("—")));
        lines.push(detail_line("Created", &deployment.created_on));
        lines.push(detail_line("URL", &deployment.url));
        lines.push(Line::raw(""));
    }
    for target in app.review_targets() {
        lines.push(Line::styled(
            target.name,
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ));
        if platform_rollback {
            lines.push(Line::from(vec![
                Span::styled("   ↶  ", Style::default().fg(MAGENTA)),
                Span::styled("Request platform rollback", Style::default().fg(TEXT)),
                Span::styled("  Cloudflare API", Style::default().fg(MUTED)),
            ]));
        } else {
            for (index, step) in target.steps.iter().enumerate() {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {:>2}  ", index + 1), Style::default().fg(BORDER)),
                    Span::styled(step.name.clone(), Style::default().fg(TEXT)),
                    Span::styled(
                        format!("  {}", action_label(&step.action)),
                        Style::default().fg(MUTED),
                    ),
                ]));
            }
        }
        lines.push(Line::raw(""));
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(panel(" Confirm ")),
        area,
    );
}

fn render_running(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let layout_frame = install_split_frame(app, SplitSurface::Running, area);
    let spinner = SPINNER[app.spinner % SPINNER.len()];
    let run_title = match &app.active_operation {
        Some(RunOperation::DeployProduction) => "Production deploy in progress".to_owned(),
        Some(RunOperation::DeployPreview) => {
            app.progress.first().and_then(|target| target.branch.as_deref()).map_or_else(
                || "Preview deploy in progress".to_owned(),
                |branch| format!("Preview deploy in progress · {branch}"),
            )
        }
        Some(RunOperation::Rollback { .. } | RunOperation::CloudflarePagesRollback { .. }) => {
            "Rollback in progress".to_owned()
        }
        None => "Run in progress".to_owned(),
    };
    let mut progress = vec![
        Line::from(vec![
            Span::styled(
                format!("{spinner} "),
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(run_title, Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
        ]),
        Line::raw(""),
    ];
    for target in &app.progress {
        progress.push(Line::from(vec![
            status_span(target.status),
            Span::raw(" "),
            Span::styled(
                target.name.clone(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}", short_version(&target.version)),
                Style::default().fg(MUTED),
            ),
        ]));
        for step in &target.steps {
            progress.push(Line::from(vec![
                Span::raw("   "),
                status_span(step.status),
                Span::raw(" "),
                Span::styled(
                    step.name.clone(),
                    Style::default().fg(if step.status == ProgressStatus::Skipped {
                        MUTED
                    } else {
                        TEXT
                    }),
                ),
                Span::styled(
                    step.elapsed
                        .map(|elapsed| format!("  {}", duration(elapsed)))
                        .unwrap_or_default(),
                    Style::default().fg(MUTED),
                ),
            ]));
        }
    }
    frame.render_widget(
        Paragraph::new(progress)
            .scroll((app.primary_scroll, 0))
            .block(active_panel(" Progress ", app.active_region == ActiveRegion::Primary)),
        layout_frame.split.first,
    );

    let inner_height = layout_frame.split.second.height.saturating_sub(2) as usize;
    let output_end = app.output.len().saturating_sub(app.secondary_scroll as usize);
    let output_start = output_end.saturating_sub(inner_height);
    let output = app
        .output
        .iter()
        .skip(output_start)
        .take(output_end.saturating_sub(output_start))
        .map(|line| {
            let mut style = match line.stream {
                OutputStream::Stdout => Style::default().fg(TEXT),
                OutputStream::Stderr => Style::default().fg(YELLOW),
            };
            if extract_tailscale_auth_url(&line.text).is_some() {
                style =
                    Style::default().fg(CYAN).add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
            }
            Line::styled(
                truncate(&line.text, layout_frame.split.second.width.saturating_sub(4) as usize),
                style,
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(output)
            .block(active_panel(" Live output ", app.active_region == ActiveRegion::Secondary)),
        layout_frame.split.second,
    );
    render_layout_divider(frame, layout_frame, app.layout_drag.is_some());
}

fn render_summary(frame: &mut Frame<'_>, area: Rect, app: &App, journal_path: &Path) {
    let rollback = matches!(
        app.active_operation,
        Some(RunOperation::Rollback { .. } | RunOperation::CloudflarePagesRollback { .. })
    );
    let operation = match &app.active_operation {
        Some(RunOperation::DeployProduction) => "Production deploy",
        Some(RunOperation::DeployPreview) => "Preview deploy",
        Some(RunOperation::Rollback { .. } | RunOperation::CloudflarePagesRollback { .. }) => {
            "Rollback"
        }
        None => "Run",
    };
    let (status, color) = match app.outcome {
        Some(RunOutcome::Succeeded) => ("complete", if rollback { MAGENTA } else { GREEN }),
        Some(RunOutcome::Failed) => ("failed", RED),
        Some(RunOutcome::Cancelled) => ("cancelled", YELLOW),
        None => ("ended", MUTED),
    };
    let title = format!("{operation} {status}");
    let mut lines = vec![
        Line::styled(title, Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Line::styled(
            app.run_elapsed.map(duration).unwrap_or_else(|| "—".to_owned()),
            Style::default().fg(MUTED),
        ),
        Line::raw(""),
    ];
    match &app.active_operation {
        Some(RunOperation::DeployProduction) => {
            lines.push(detail_line("Environment", "production"));
            lines.push(Line::raw(""));
        }
        Some(RunOperation::DeployPreview) => {
            lines.push(detail_line("Environment", "preview"));
            if let Some(branch) = app.progress.first().and_then(|target| target.branch.as_deref()) {
                lines.push(detail_line("Branch", branch));
            }
            lines.push(Line::raw(""));
        }
        Some(RunOperation::Rollback { .. } | RunOperation::CloudflarePagesRollback { .. })
        | None => {}
    }
    if let Some(url) = summary_url(app) {
        lines.push(Line::from(vec![
            Span::styled("Deployed  ", Style::default().fg(MUTED)),
            Span::styled(url, Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        ]));
        lines.push(Line::styled("press o to open", Style::default().fg(MUTED)));
        lines.push(Line::raw(""));
    }
    for target in &app.progress {
        lines.push(Line::from(vec![
            status_span(target.status),
            Span::raw(" "),
            Span::styled(
                target.name.clone(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}", short_version(&target.version)),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                target
                    .elapsed
                    .map(|elapsed| format!("  {}", duration(elapsed)))
                    .unwrap_or_default(),
                Style::default().fg(MUTED),
            ),
        ]));
        for step in &target.steps {
            if step.status == ProgressStatus::Skipped {
                continue;
            }
            lines.push(Line::from(vec![
                Span::raw("   "),
                status_span(step.status),
                Span::raw(" "),
                Span::styled(step.name.clone(), Style::default().fg(TEXT)),
                Span::styled(
                    step.elapsed
                        .map(|elapsed| format!("  {}", duration(elapsed)))
                        .unwrap_or_default(),
                    Style::default().fg(MUTED),
                ),
            ]));
            if let Some(failure) = &step.failure {
                lines.push(Line::styled(format!("      {failure}"), Style::default().fg(RED)));
            }
        }
        lines.push(Line::raw(""));
    }
    lines.push(Line::styled(
        if matches!(app.active_operation, Some(RunOperation::CloudflarePagesRollback { .. })) {
            "History  Cloudflare Pages".to_owned()
        } else {
            format!("Journal  {}", journal_path.display())
        },
        Style::default().fg(MUTED),
    ));
    if app.output.is_empty() {
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(panel(" Summary ")),
            area,
        );
        return;
    }
    let panes =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).split(area);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(panel(" Summary ")),
        panes[0],
    );
    let log_area = panes[1];
    let inner_height = log_area.height.saturating_sub(2) as usize;
    let output_end = app.output.len().saturating_sub(app.secondary_scroll as usize);
    let output_start = output_end.saturating_sub(inner_height);
    let output = app
        .output
        .iter()
        .skip(output_start)
        .take(output_end.saturating_sub(output_start))
        .map(|line| {
            let style = match line.stream {
                OutputStream::Stdout => Style::default().fg(TEXT),
                OutputStream::Stderr => Style::default().fg(YELLOW),
            };
            Line::styled(truncate(&line.text, log_area.width.saturating_sub(4) as usize), style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(output).block(panel(" Deploy log · ↑↓ scroll ")), log_area);
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App, actions: &DeployActionRegistry) {
    let controls = action_help(app, actions);
    if let Some(notice) = &app.notice {
        let notice = if app.phase == Phase::Preparing {
            format!("{} {notice}", SPINNER[app.spinner % SPINNER.len()])
        } else {
            notice.clone()
        };
        let lines = vec![
            Line::styled(notice, Style::default().fg(YELLOW)),
            Line::styled(controls.clone(), Style::default().fg(MUTED)),
        ];
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
    } else {
        frame.render_widget(
            Paragraph::new(controls).style(Style::default().fg(MUTED)).alignment(Alignment::Center),
            area,
        );
    }
}

fn action_help(app: &App, actions: &DeployActionRegistry) -> String {
    let mut help = actions
        .resolve_command_palette(&action_context(app))
        .items()
        .iter()
        .filter_map(|action| {
            action.primary_keybinding().map(|binding| format!("{binding} {}", action.title))
        })
        .collect::<Vec<_>>();
    if matches!(app.modal, Some(Modal::BranchInput { .. } | Modal::NoteInput { .. })) {
        help.insert(0, "Type to edit".to_owned());
    }
    help.join("  ")
}

fn panel(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .title(title.into())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .padding(Padding::horizontal(1))
}

fn active_panel(title: impl Into<String>, active: bool) -> Block<'static> {
    panel(title).border_style(Style::default().fg(if active { CYAN } else { BORDER }))
}

fn status_span(status: ProgressStatus) -> Span<'static> {
    let (symbol, color) = match status {
        ProgressStatus::Pending => ("○", BORDER),
        ProgressStatus::Running => ("●", CYAN),
        ProgressStatus::Succeeded => ("✓", GREEN),
        ProgressStatus::Failed => ("✗", RED),
        ProgressStatus::Cancelled => ("■", YELLOW),
        ProgressStatus::Skipped => ("–", MUTED),
    };
    Span::styled(symbol, Style::default().fg(color).add_modifier(Modifier::BOLD))
}

fn journal_status(status: JournalStatus) -> (&'static str, Color) {
    match status {
        JournalStatus::Success => ("✓", GREEN),
        JournalStatus::Failed => ("✗", RED),
        JournalStatus::Cancelled => ("■", YELLOW),
        JournalStatus::RolledBack => ("↶", MAGENTA),
    }
}

fn cloudflare_environment(environment: CloudflareEnvironment) -> &'static str {
    match environment {
        CloudflareEnvironment::Production => "production",
        CloudflareEnvironment::Preview => "preview",
        CloudflareEnvironment::Unknown => "unknown",
    }
}

fn cloudflare_status(status: Option<CloudflareStageStatus>) -> (&'static str, Color) {
    match status {
        Some(CloudflareStageStatus::Success) => ("✓", GREEN),
        Some(CloudflareStageStatus::Active) => ("●", CYAN),
        Some(CloudflareStageStatus::Failure) => ("✗", RED),
        Some(CloudflareStageStatus::Canceled) => ("■", YELLOW),
        Some(CloudflareStageStatus::Idle) => ("○", BORDER),
        Some(CloudflareStageStatus::Unknown) | None => ("?", MUTED),
    }
}

fn cloudflare_status_label(status: Option<CloudflareStageStatus>) -> &'static str {
    match status {
        Some(CloudflareStageStatus::Success) => "success",
        Some(CloudflareStageStatus::Active) => "active",
        Some(CloudflareStageStatus::Failure) => "failure",
        Some(CloudflareStageStatus::Canceled) => "canceled",
        Some(CloudflareStageStatus::Idle) => "idle",
        Some(CloudflareStageStatus::Unknown) => "unknown",
        None => "—",
    }
}

fn short_text(text: &str) -> String {
    if text.chars().count() > 12 {
        format!("{}…", text.chars().take(12).collect::<String>())
    } else {
        text.to_owned()
    }
}

fn action_label(action: &DeployAction) -> &'static str {
    match action {
        DeployAction::Command { .. } => "command",
        DeployAction::Shell { .. } => "shell",
    }
}

fn short_version(version: &VersionId) -> String {
    if version.0.chars().count() > 12 {
        format!("{}…", version.0.chars().take(12).collect::<String>())
    } else {
        version.0.clone()
    }
}

fn duration(elapsed: std::time::Duration) -> String {
    if elapsed.as_secs() >= 60 {
        format!("{}m {:02}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

fn duration_ms(milliseconds: u64) -> String {
    duration(std::time::Duration::from_millis(milliseconds))
}

fn format_timestamp(timestamp_secs: u64) -> String {
    let Ok(timestamp) = i64::try_from(timestamp_secs) else {
        return format!("unix:{timestamp_secs}");
    };
    OffsetDateTime::from_unix_timestamp(timestamp)
        .ok()
        .and_then(|timestamp| timestamp.format(&Rfc3339).ok())
        .unwrap_or_else(|| format!("unix:{timestamp_secs}"))
}

fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let mut out = String::new();
    let target = width - 1;
    let mut used = 0;
    for character in text.chars() {
        let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > target {
            break;
        }
        out.push(character);
        used += character_width;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use ratatui::{backend::TestBackend, Terminal};

    use super::*;
    use crate::{
        onepassword::OpEnvironment,
        tools::deploy::config::{DeployStep, DeploymentPlan, RollbackStrategy},
    };

    #[test]
    fn preview_branch_validation_uses_the_remote_production_branch() {
        assert!(ensure_preview_branch("feature", "main").is_ok());
        let error = ensure_preview_branch("main", "main").expect_err("production must fail");
        assert!(error.to_string().contains("Cloudflare's production branch"));
    }

    fn test_app() -> App {
        App::new(
            LoadedPlan {
                path: PathBuf::from(".kit/deploy.toml"),
                base_dir: PathBuf::from(".kit"),
                plan: DeploymentPlan {
                    version: 1,
                    targets: vec![DeployTarget {
                        id: "preview".to_owned(),
                        name: "Preview".to_owned(),
                        description: Some("Publish preview artifacts".to_owned()),
                        working_dir: None,
                        source_roots: Vec::new(),
                        env_file: None,
                        artifact: None,
                        steps: vec![DeployStep {
                            name: "Build".to_owned(),
                            working_dir: None,
                            action: DeployAction::Command {
                                program: "builder".to_owned(),
                                args: Vec::new(),
                            },
                        }],
                        backend: None,
                        rollback: Some(RollbackStrategy::Redeploy),
                    }],
                },
                environments: Default::default(),
            },
            DeployJournal::default(),
            DeployAnnotations::default(),
            DeployLayout::default(),
        )
    }

    #[test]
    fn browse_state_renders_target_plan_and_controls() -> Result<()> {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend)?;
        let mut app = test_app();
        let mut interaction = DeployInteraction::new()?;
        app.toggle_focused();

        terminal
            .draw(|frame| render(frame, &mut app, Path::new("journal.json"), &mut interaction))?;
        let buffer = terminal.backend().buffer();
        let screen = buffer.content.iter().map(|cell| cell.symbol()).collect::<Vec<_>>().join("");

        assert!(screen.contains("Preview"));
        assert!(screen.contains("Build"));
        assert!(screen.contains("rollback ready"));
        assert!(screen.contains("Space select"));
        assert!(screen.contains("production deploy"));
        Ok(())
    }

    #[test]
    fn production_and_preview_destinations_remain_explicit_through_summary() -> Result<()> {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend)?;
        let mut app = test_app();
        let mut interaction = DeployInteraction::new()?;
        app.toggle_focused();
        app.review_production_deploy();

        terminal
            .draw(|frame| render(frame, &mut app, Path::new("journal.json"), &mut interaction))?;
        let review = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(review.contains("Production deployment plan"));
        assert!(review.contains("Environment production"));

        let target = app.loaded.plan.targets[0].clone();
        app.begin_run(&RunSpec {
            base_dir: PathBuf::from("."),
            operation: RunOperation::DeployPreview,
            targets: vec![RunTargetSpec {
                target,
                version: VersionId("version-placeholder".to_owned()),
                source: None,
                branch: Some("feature-x".to_owned()),
                environment: OpEnvironment::default(),
            }],
        });
        app.ingest(runner::RunEvent::Finished {
            outcome: RunOutcome::Succeeded,
            elapsed: Duration::from_millis(25),
        });

        terminal
            .draw(|frame| render(frame, &mut app, Path::new("journal.json"), &mut interaction))?;
        let summary = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(summary.contains("Preview deploy complete"));
        assert!(summary.contains("Environment preview"));
        assert!(summary.contains("Branch      feature-x"));
        Ok(())
    }

    #[test]
    fn deploy_regions_use_shared_navigation_and_keep_vertical_input_local() -> Result<()> {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend)?;
        let mut app = test_app();
        let mut interaction = DeployInteraction::new()?;
        let step = app.loaded.plan.targets[0].steps[0].clone();
        app.loaded.plan.targets[0].steps.extend((0..30).map(|index| {
            let mut step = step.clone();
            step.name = format!("Step {index}");
            step
        }));
        let mut second = app.loaded.plan.targets[0].clone();
        second.id = "production".to_owned();
        second.name = "Production".to_owned();
        app.loaded.plan.targets.push(second);
        app.selected.push(false);

        terminal
            .draw(|frame| render(frame, &mut app, Path::new("journal.json"), &mut interaction))?;
        let primary = app.layout_frame.split.first;
        let secondary = app.layout_frame.split.second;
        assert_eq!(terminal.backend().buffer()[(primary.x, primary.y)].fg, CYAN);

        handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &mut app, &mut interaction);
        handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut app, &mut interaction);
        assert_eq!(app.active_region, ActiveRegion::Secondary);
        assert_eq!(app.secondary_scroll, 1);
        assert_eq!(app.cursor, 0);

        terminal
            .draw(|frame| render(frame, &mut app, Path::new("journal.json"), &mut interaction))?;
        assert_eq!(terminal.backend().buffer()[(primary.x, primary.y)].fg, BORDER);
        assert_eq!(terminal.backend().buffer()[(secondary.x, secondary.y)].fg, CYAN);

        handle_key(
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            &mut app,
            &mut interaction,
        );
        handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut app, &mut interaction);
        assert_eq!(app.active_region, ActiveRegion::Primary);
        assert_eq!(app.cursor, 1);

        handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: secondary.x.saturating_add(1),
                row: secondary.y.saturating_add(1),
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut interaction,
        );
        assert_eq!(app.active_region, ActiveRegion::Secondary);
        Ok(())
    }

    #[test]
    fn running_regions_scroll_progress_and_live_output_independently() -> Result<()> {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend)?;
        let mut app = test_app();
        let mut interaction = DeployInteraction::new()?;
        let step = app.loaded.plan.targets[0].steps[0].clone();
        app.loaded.plan.targets[0].steps.extend((0..30).map(|index| {
            let mut step = step.clone();
            step.name = format!("Step {index}");
            step
        }));
        let target = app.loaded.plan.targets[0].clone();
        app.begin_run(&RunSpec {
            base_dir: PathBuf::from("."),
            operation: RunOperation::DeployProduction,
            targets: vec![RunTargetSpec {
                target,
                version: VersionId("version-placeholder".to_owned()),
                source: None,
                branch: None,
                environment: OpEnvironment::default(),
            }],
        });
        for index in 0..30 {
            app.ingest(runner::RunEvent::Output {
                stream: OutputStream::Stdout,
                line: format!("output {index}"),
            });
        }

        terminal
            .draw(|frame| render(frame, &mut app, Path::new("journal.json"), &mut interaction))?;
        handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &mut app, &mut interaction);
        assert_eq!(app.primary_scroll, 1);
        assert_eq!(app.secondary_scroll, 0);

        handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &mut app, &mut interaction);
        handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &mut app, &mut interaction);
        assert_eq!(app.active_region, ActiveRegion::Secondary);
        assert_eq!(app.primary_scroll, 1);
        assert_eq!(app.secondary_scroll, 1);
        Ok(())
    }

    #[test]
    fn journal_and_cloudflare_versions_share_the_saved_split() -> Result<()> {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend)?;
        let mut app = test_app();
        let mut interaction = DeployInteraction::new()?;
        app.layout.versions = crate::tools::deploy::layout::SplitRatio::new(700);
        app.phase = Phase::Versions;
        app.journal.targets.push(crate::tools::deploy::journal::TargetJournal {
            target_id: "preview".to_owned(),
            entries: vec![JournalEntry {
                version: VersionId("version-1".to_owned()),
                timestamp_secs: 1,
                operation: crate::tools::deploy::journal::JournalOperation::Deploy,
                status: JournalStatus::Success,
                duration_ms: 1,
                steps: Vec::new(),
            }],
        });

        terminal
            .draw(|frame| render(frame, &mut app, Path::new("journal.json"), &mut interaction))?;
        let journal_width = app.layout_frame.split.first.width;
        let primary = app.layout_frame.split.first;
        let secondary = app.layout_frame.split.second;
        handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &mut app, &mut interaction);
        terminal
            .draw(|frame| render(frame, &mut app, Path::new("journal.json"), &mut interaction))?;
        assert_eq!(terminal.backend().buffer()[(primary.x, primary.y)].fg, BORDER);
        assert_eq!(terminal.backend().buffer()[(secondary.x, secondary.y)].fg, CYAN);

        app.versions = VersionsState::CloudflareReady {
            deployments: vec![CloudflareDeployment {
                id: "deployment-placeholder".to_owned(),
                short_id: "short-id".to_owned(),
                created_on: "2026-01-02T03:04:05Z".to_owned(),
                environment: CloudflareEnvironment::Production,
                url: "https://placeholder.invalid".to_owned(),
                latest_stage: Some(crate::tools::deploy::cloudflare::CloudflareStage {
                    status: CloudflareStageStatus::Success,
                }),
                deployment_trigger: None,
            }],
            live_id: Some("deployment-placeholder".to_owned()),
            production_branch: "main".to_owned(),
        };
        terminal
            .draw(|frame| render(frame, &mut app, Path::new("journal.json"), &mut interaction))?;

        assert_eq!(app.layout_frame.split.first.width, journal_width);
        assert!(journal_width > app.layout_frame.split.second.width);
        assert_eq!(terminal.backend().buffer()[(secondary.x, secondary.y)].fg, CYAN);
        Ok(())
    }

    #[test]
    fn extracts_only_pages_dev_deploy_urls() {
        assert_eq!(
            extract_deploy_url(
                "✨ Deployment alias URL: https://feature-x.modular-marketing.pages.dev"
            ),
            Some("https://feature-x.modular-marketing.pages.dev".to_owned())
        );
        assert_eq!(extract_deploy_url("(!) see https://rolldown.rs/reference for options"), None);
        assert_eq!(extract_deploy_url("Building client environment…"), None);
    }

    #[test]
    fn tailscale_auth_link_opens_by_key_and_click_during_run() -> Result<()> {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend)?;
        let mut app = test_app();
        let mut interaction = DeployInteraction::new()?;
        let target = app.loaded.plan.targets[0].clone();
        app.begin_run(&RunSpec {
            base_dir: PathBuf::from("."),
            operation: RunOperation::DeployProduction,
            targets: vec![RunTargetSpec {
                target,
                version: VersionId("version-placeholder".to_owned()),
                source: None,
                branch: None,
                environment: OpEnvironment::default(),
            }],
        });
        app.ingest(runner::RunEvent::Output {
            stream: OutputStream::Stderr,
            line: "Authenticate at https://login.tailscale.com/a/example-auth".to_owned(),
        });
        terminal
            .draw(|frame| render(frame, &mut app, Path::new("journal.json"), &mut interaction))?;

        match handle_key(
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
            &mut app,
            &mut interaction,
        ) {
            UiAction::OpenUrl { url } => {
                assert_eq!(url, "https://login.tailscale.com/a/example-auth")
            }
            _ => panic!("expected the Tailscale authentication URL to open"),
        }

        let live_output = app.layout_frame.split.second;
        match handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: live_output.x.saturating_add(2),
                row: live_output.y.saturating_add(1),
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            &mut interaction,
        ) {
            UiAction::OpenUrl { url } => {
                assert_eq!(url, "https://login.tailscale.com/a/example-auth")
            }
            _ => panic!("expected a click on the authentication link to open it"),
        }
        Ok(())
    }

    #[test]
    fn recognizes_only_tailscale_https_login_links() {
        assert_eq!(
            extract_tailscale_auth_url(
                "Visit https://login.tailscale.com/a/example-auth, then return here"
            )
            .as_deref(),
            Some("https://login.tailscale.com/a/example-auth")
        );
        assert_eq!(extract_tailscale_auth_url("https://example.com/a/example-auth"), None);
        assert_eq!(extract_tailscale_auth_url("http://login.tailscale.com/a/example-auth"), None);
    }

    #[test]
    fn summary_url_returns_the_last_deploy_link_in_output() {
        let mut app = test_app();
        app.output.push_back(crate::tools::deploy::state::OutputLine {
            stream: OutputStream::Stdout,
            text: "Take a peek over at https://abc123.example.pages.dev".to_owned(),
        });
        app.output.push_back(crate::tools::deploy::state::OutputLine {
            stream: OutputStream::Stdout,
            text: "Deployment alias URL: https://feature-x.example.pages.dev".to_owned(),
        });
        assert_eq!(summary_url(&app).as_deref(), Some("https://feature-x.example.pages.dev"));
    }

    #[test]
    fn summary_stays_open_and_returns_to_browse() {
        let mut app = test_app();
        let mut interaction = DeployInteraction::new().unwrap();
        app.phase = Phase::Summary;
        app.outcome = Some(RunOutcome::Succeeded);

        assert!(matches!(
            handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &mut app,
                &mut interaction,
            ),
            UiAction::None
        ));
        assert_eq!(app.phase, Phase::Browse);

        app.phase = Phase::Summary;
        assert!(matches!(
            handle_key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                &mut app,
                &mut interaction,
            ),
            UiAction::Quit
        ));
    }
}
