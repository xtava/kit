//! Interactive Build presentation over the same in-process Build execution engine used by
//! `kit build run`. This module owns terminal state only; provider protocol validation and
//! process control stay in `super`.

use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};
use tokio::{
    sync::{mpsc, watch},
    time,
};
use unicode_width::UnicodeWidthStr as _;

use super::{
    actions::{self, BuildActionContext, BuildActionMode, BuildActionRegistry, BuildCommand},
    build_presentation_channel, build_runtime_availability, evidence, execute_build,
    load_interactive_manifest,
    manifest::WorkflowAction,
    BuildControl, BuildFailureEvidence, BuildInvocation, BuildRuntimeAvailability, BuildTerminal,
    BuildTranscriptTails, BuildUpdate,
};
use crate::{
    framework::{
        process::ProcessSupervisor, start_external, Context, ExternalCommand, ExternalTarget,
    },
    tui::theme::NORD,
    tui::{
        render_vertical_scrollbar, EventReader, FollowViewport, KeyChord, KeybindingResolution,
        KeybindingState, ScrollbarDrag, ScrollbarLayout, ScrollbarStyle, SelectableRegion,
        SelectionOutcome, Session, SessionOptions, TextSelection, ViewportMetrics,
    },
};

const TICK: Duration = Duration::from_millis(80);
const MAX_STAGE_ROWS: usize = 512;
const MAX_EVENT_LINES: usize = 2_048;
const MAX_UPDATE_DRAIN_PER_DRAW: usize = 8;
const PAGE_ROWS: usize = 12;

pub(super) async fn run(cx: &Context) -> Result<()> {
    let (root, manifest) = load_interactive_manifest(cx)?;
    let runtime = build_runtime_availability(cx).await;
    let mut session =
        Session::open(SessionOptions { mouse_capture: true, bracketed_paste: false })?;
    let mut input = EventReader::start();
    let mut app = App::new(&root, &manifest, runtime);
    let result = run_session(cx, &root, &manifest, &mut session, &mut input, &mut app).await;
    // Restore cooked mode and leave the alternate screen before the dispatcher prints a failed
    // interactive build's error and exits nonzero.
    drop(session);
    result
}

async fn run_session(
    cx: &Context,
    root: &crate::framework::WorktreeRoot,
    manifest: &super::manifest::LoadedBuildManifest,
    session: &mut Session,
    input: &mut EventReader,
    app: &mut App,
) -> Result<()> {
    let mut tick = time::interval(TICK);

    loop {
        session.draw(|frame| render(frame, app))?;
        tokio::select! {
            _ = tick.tick() => {}
            event = input.recv() => {
                let Some(event) = event else { return app.exit_result(); };
                if app.view == View::Evidence {
                    match app.active_key(event, true) {
                        ActiveAction::Quit => return app.exit_result(),
                        ActiveAction::Copy(text) => session.copy(&text)?,
                        ActiveAction::None
                        | ActiveAction::Cancel
                        | ActiveAction::ChooseWorkflow
                        | ActiveAction::Evidence
                        | ActiveAction::RunConfigured(_) => {}
                    }
                    apply_evidence_action(app);
                    continue;
                }
                match app.select_key(event) {
                    SelectAction::None => {}
                    SelectAction::Quit => return app.exit_result(),
                    SelectAction::Evidence => app.open_evidence(),
                    SelectAction::Start(index) => {
                        let workflow = manifest.workflows()[index].clone();
                        let invocation = BuildInvocation {
                            root: root.clone(),
                            manifest: manifest.clone(),
                            workflow,
                            deadline_seconds: None,
                        };
                        match run_active(cx, invocation, session, input, app).await? {
                            ActiveExit::Quit => return app.exit_result(),
                            ActiveExit::ChooseWorkflow => app.reset_for_selection(),
                        }
                    }
                }
            }
        }
    }
}

async fn run_active(
    cx: &Context,
    invocation: BuildInvocation,
    session: &mut Session,
    input: &mut EventReader,
    app: &mut App,
) -> Result<ActiveExit> {
    let actions = invocation.workflow.actions().to_vec();
    let repository_root = invocation.root.as_path().to_path_buf();
    app.begin_run(actions);
    let (updates, mut presentation) = build_presentation_channel();
    let (controls, control_receiver) = mpsc::channel(1);
    let execution = execute_build(cx, invocation, Some(updates), control_receiver, true);
    tokio::pin!(execution);
    let mut tick = time::interval(TICK);
    let mut engine_finished = false;
    let mut update_stream_open = true;
    let mut transcript_stream_open = true;
    let mut input_open = true;
    let mut quit_after_terminal = false;
    let mut engine_fallback = None;
    let (action_sender, mut action_receiver) = mpsc::channel(1);
    let mut action_handle = None;

    loop {
        session.draw(|frame| render(frame, app))?;
        tokio::select! {
            _ = tick.tick() => app.tick(),
            result = &mut execution, if !engine_finished => {
                engine_finished = true;
                if let Err(error) = result {
                    engine_fallback = Some(format!("{error:#}"));
                }
            }
            update = presentation.updates.recv(), if update_stream_open => {
                match update {
                    Some(update) => {
                        ingest_presentation_update(
                            app,
                            &mut presentation.transcripts,
                            update,
                        );
                        for _ in 1..MAX_UPDATE_DRAIN_PER_DRAW {
                            match presentation.updates.try_recv() {
                                Ok(update) => ingest_presentation_update(
                                    app,
                                    &mut presentation.transcripts,
                                    update,
                                ),
                                Err(mpsc::error::TryRecvError::Empty) => break,
                                Err(mpsc::error::TryRecvError::Disconnected) => {
                                    update_stream_open = false;
                                    if app.terminal.is_none() {
                                        app.note_missing_terminal(engine_fallback.as_deref());
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    None => {
                        update_stream_open = false;
                        if app.terminal.is_none() {
                            app.note_missing_terminal(engine_fallback.as_deref());
                        }
                    }
                }
            }
            changed = presentation.transcripts.changed(), if transcript_stream_open => {
                match changed {
                    Ok(()) => {
                        let tails = presentation.transcripts.borrow_and_update().clone();
                        app.ingest_transcripts(tails);
                    }
                    Err(_) => transcript_stream_open = false,
                }
            }
            result = action_receiver.recv(), if action_handle.is_some() => {
                if let Some(handle) = action_handle.take() {
                    let _ = handle.await;
                }
                if let Some((label, result)) = result {
                    app.notice = match result {
                        Ok(()) => format!("Completed {label}"),
                        Err(error) => format!("Could not run {label}: {error}"),
                    };
                }
            }
            event = input.recv(), if input_open => {
                let Some(event) = event else {
                    input_open = false;
                    quit_after_terminal = true;
                    if !engine_finished {
                        request_cancel(&controls, app);
                    }
                    continue;
                };
                let terminal_ready = app.terminal.is_some();
                match app.active_key(event, terminal_ready) {
                    ActiveAction::None => {}
                    ActiveAction::Copy(text) => session.copy(&text)?,
                    ActiveAction::Cancel => request_cancel(&controls, app),
                    ActiveAction::Quit => {
                        if !terminal_ready {
                            quit_after_terminal = true;
                            if !engine_finished {
                                request_cancel(&controls, app);
                            }
                            continue;
                        }
                        return Ok(ActiveExit::Quit);
                    }
                    ActiveAction::ChooseWorkflow if terminal_ready => {
                        return Ok(ActiveExit::ChooseWorkflow);
                    }
                    ActiveAction::Evidence if terminal_ready => app.open_evidence(),
                    ActiveAction::RunConfigured(index) if terminal_ready => {
                        if action_handle.is_some() {
                            app.notice = "Another workflow action is still running".to_owned();
                        } else if let Some(action) = app.configured_action(index).cloned() {
                            app.notice = format!("Running {}…", action.label.as_str());
                            action_handle = Some(spawn_configured_action(
                                cx.processes.clone(),
                                repository_root.clone(),
                                action,
                                action_sender.clone(),
                            ));
                        }
                    }
                    ActiveAction::ChooseWorkflow
                    | ActiveAction::Evidence
                    | ActiveAction::RunConfigured(_) => {}
                }
            }
        }

        apply_evidence_action(app);
        if app.terminal.is_some() && quit_after_terminal {
            return Ok(ActiveExit::Quit);
        }
    }
}

fn ingest_presentation_update(
    app: &mut App,
    transcripts: &mut watch::Receiver<BuildTranscriptTails>,
    update: BuildUpdate,
) {
    if matches!(&update, BuildUpdate::Terminal(_)) {
        let tails = transcripts.borrow_and_update().clone();
        app.ingest_transcripts(tails);
    }
    app.ingest_update(update);
}

fn apply_evidence_action(app: &mut App) {
    if let Some(EvidenceAction::Forget(run_id)) = app.evidence_action() {
        match evidence::tui_forget(&run_id) {
            Ok(message) => {
                app.notice = message;
                app.open_evidence();
            }
            Err(error) => app.notice = format!("could not forget evidence: {error:#}"),
        }
    }
}

fn request_cancel(controls: &mpsc::Sender<BuildControl>, app: &mut App) {
    match controls.try_send(BuildControl::Cancel) {
        Ok(()) | Err(mpsc::error::TrySendError::Full(BuildControl::Cancel)) => {
            app.notice = "cancellation requested; waiting for the owned process tree".to_owned();
        }
        Err(mpsc::error::TrySendError::Closed(BuildControl::Cancel)) => {
            app.notice = "Build control is no longer available".to_owned();
        }
    }
}

fn spawn_configured_action(
    processes: ProcessSupervisor,
    repository_root: PathBuf,
    action: WorkflowAction,
    sender: mpsc::Sender<(String, Result<(), String>)>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let label = action.label.as_str().to_owned();
        let result = match start_external(
            &processes,
            ExternalTarget::Command(ExternalCommand {
                program: action.command.program,
                arguments: action.command.arguments,
                working_directory: repository_root,
            }),
        ) {
            Ok(receipt) => receipt.completion().await,
            Err(error) => Err(error),
        }
        .map_err(|error| format!("{error:#}"));
        let _ = sender.send((label, result)).await;
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveExit {
    Quit,
    ChooseWorkflow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SelectAction {
    None,
    Quit,
    Evidence,
    Start(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ActiveAction {
    None,
    Copy(String),
    Cancel,
    Quit,
    ChooseWorkflow,
    Evidence,
    RunConfigured(usize),
}

#[derive(Clone, Debug)]
struct WorkflowRow {
    id: String,
    label: String,
    availability: WorkflowAvailability,
}

#[derive(Clone, Debug)]
enum WorkflowAvailability {
    Ready,
    Unavailable { reason: String },
}

impl WorkflowAvailability {
    fn from_workflow(
        workflow: &super::manifest::Workflow,
        runtime: &BuildRuntimeAvailability,
    ) -> Self {
        if !workflow.supports_current_platform() {
            return Self::Unavailable {
                reason: "workflow does not support this platform".to_owned(),
            };
        }
        match runtime {
            BuildRuntimeAvailability::Ready => Self::Ready,
            BuildRuntimeAvailability::Unavailable { reason } => {
                Self::Unavailable { reason: reason.clone() }
            }
        }
    }

    fn ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

#[derive(Clone, Debug)]
struct StageRow {
    id: String,
    parent: Option<String>,
    state: StageState,
    detail: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StageState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum View {
    Select,
    Running,
    Terminal,
    Evidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunPane {
    Stages,
    Events,
    Stdout,
    Stderr,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BuildSurface {
    Stages,
    Events,
    Stdout,
    Stderr,
    Terminal,
    Evidence,
}

#[derive(Default)]
struct UiRegions {
    workflows: Vec<(Rect, usize)>,
    run_panes: Vec<(Rect, RunPane)>,
    evidence_records: Vec<(Rect, usize)>,
    evidence_inspect: Option<Rect>,
    selectable: Vec<SelectableRegion<BuildSurface>>,
    scrollbars: Vec<(BuildSurface, ScrollbarLayout)>,
}

#[derive(Clone, Debug)]
enum EvidenceMode {
    List,
    Inspect(String),
    ConfirmForget(String),
    ForgetReady(String),
}

#[derive(Clone, Debug)]
enum EvidenceAction {
    Forget(String),
}

struct App {
    actions: BuildActionRegistry,
    keybinding_state: KeybindingState,
    root: String,
    manifest: String,
    workflows: Vec<WorkflowRow>,
    selected_workflow: usize,
    view: View,
    run_started: Option<Instant>,
    run_id: String,
    stage_order: VecDeque<String>,
    stages: BTreeMap<String, StageRow>,
    events: VecDeque<String>,
    stdout: Arc<str>,
    stderr: Arc<str>,
    run_pane: RunPane,
    stage_scroll: FollowViewport,
    stage_metrics: ViewportMetrics,
    event_scroll: FollowViewport,
    event_metrics: ViewportMetrics,
    stdout_scroll: FollowViewport,
    stdout_metrics: ViewportMetrics,
    stderr_scroll: FollowViewport,
    stderr_metrics: ViewportMetrics,
    terminal_scroll: FollowViewport,
    terminal_metrics: ViewportMetrics,
    notice: String,
    terminal: Option<BuildTerminal>,
    configured_actions: Vec<WorkflowAction>,
    exit_failure: Option<String>,
    evidence: Vec<evidence::TuiEvidenceRecord>,
    selected_evidence: usize,
    evidence_mode: EvidenceMode,
    evidence_inspect_scroll: FollowViewport,
    evidence_inspect_metrics: ViewportMetrics,
    selection: TextSelection<BuildSurface>,
    content_revision: u64,
    regions: UiRegions,
    scrollbar_drag: Option<(BuildSurface, ScrollbarDrag)>,
}

impl App {
    fn new(
        root: &crate::framework::WorktreeRoot,
        manifest: &super::manifest::LoadedBuildManifest,
        runtime: BuildRuntimeAvailability,
    ) -> Self {
        let workflows = manifest
            .workflows()
            .iter()
            .map(|workflow| WorkflowRow {
                id: workflow.id().to_owned(),
                label: workflow.label().to_owned(),
                availability: WorkflowAvailability::from_workflow(workflow, &runtime),
            })
            .collect::<Vec<_>>();
        let selected_workflow =
            workflows.iter().position(|workflow| workflow.availability.ready()).unwrap_or(0);
        Self {
            actions: actions::registry().expect("Build action contributions are valid"),
            keybinding_state: KeybindingState::default(),
            root: root.as_path().display().to_string(),
            manifest: manifest.path.display().to_string(),
            workflows,
            selected_workflow,
            view: View::Select,
            run_started: None,
            run_id: String::new(),
            stage_order: VecDeque::new(),
            stages: BTreeMap::new(),
            events: VecDeque::new(),
            stdout: Arc::from(""),
            stderr: Arc::from(""),
            run_pane: RunPane::Stages,
            stage_scroll: FollowViewport::default(),
            stage_metrics: ViewportMetrics::default(),
            event_scroll: FollowViewport::default(),
            event_metrics: ViewportMetrics::default(),
            stdout_scroll: FollowViewport::default(),
            stdout_metrics: ViewportMetrics::default(),
            stderr_scroll: FollowViewport::default(),
            stderr_metrics: ViewportMetrics::default(),
            terminal_scroll: FollowViewport::at_top(),
            terminal_metrics: ViewportMetrics::default(),
            notice: "Select a workflow. Build eligibility is evaluated on this host.".to_owned(),
            terminal: None,
            configured_actions: Vec::new(),
            exit_failure: None,
            evidence: Vec::new(),
            selected_evidence: 0,
            evidence_mode: EvidenceMode::List,
            evidence_inspect_scroll: FollowViewport::at_top(),
            evidence_inspect_metrics: ViewportMetrics::default(),
            selection: TextSelection::default(),
            content_revision: 0,
            regions: UiRegions::default(),
            scrollbar_drag: None,
        }
    }

    fn reset_for_selection(&mut self) {
        self.view = View::Select;
        self.notice = "Select a workflow. Build eligibility is evaluated on this host.".to_owned();
        self.terminal = None;
    }

    fn begin_run(&mut self, configured_actions: Vec<WorkflowAction>) {
        self.selection.clear();
        self.scrollbar_drag = None;
        self.content_revision = self.content_revision.wrapping_add(1);
        self.view = View::Running;
        self.run_started = Some(Instant::now());
        self.run_id.clear();
        self.stage_order.clear();
        self.stages.clear();
        self.events.clear();
        self.stdout = Arc::from("");
        self.stderr = Arc::from("");
        self.run_pane = RunPane::Stages;
        self.stage_scroll = FollowViewport::default();
        self.stage_metrics = ViewportMetrics::default();
        self.event_scroll = FollowViewport::default();
        self.event_metrics = ViewportMetrics::default();
        self.stdout_scroll = FollowViewport::default();
        self.stdout_metrics = ViewportMetrics::default();
        self.stderr_scroll = FollowViewport::default();
        self.stderr_metrics = ViewportMetrics::default();
        self.terminal_scroll = FollowViewport::at_top();
        self.terminal_metrics = ViewportMetrics::default();
        self.notice = "preparing private workspace and supervisor".to_owned();
        self.terminal = None;
        self.configured_actions = configured_actions;
    }

    fn tick(&mut self) {}

    fn action_context(&self) -> BuildActionContext {
        let mode = match (&self.view, &self.evidence_mode) {
            (View::Select, _) => BuildActionMode::Select,
            (View::Running, _) => BuildActionMode::Running,
            (View::Terminal, _) => BuildActionMode::Terminal,
            (View::Evidence, EvidenceMode::List | EvidenceMode::ForgetReady(_)) => {
                BuildActionMode::EvidenceList
            }
            (View::Evidence, EvidenceMode::Inspect(_)) => BuildActionMode::EvidenceInspect,
            (View::Evidence, EvidenceMode::ConfirmForget(_)) => BuildActionMode::EvidenceConfirm,
        };
        let activate_available = match mode {
            BuildActionMode::Select => self
                .workflows
                .get(self.selected_workflow)
                .is_some_and(|workflow| workflow.availability.ready()),
            BuildActionMode::EvidenceList => self.evidence.get(self.selected_evidence).is_some(),
            BuildActionMode::Running
            | BuildActionMode::Terminal
            | BuildActionMode::EvidenceInspect
            | BuildActionMode::EvidenceConfirm => false,
        };
        BuildActionContext { mode, activate_available }
    }

    fn resolve_action_key(&mut self, key: KeyEvent) -> Option<BuildCommand> {
        let chord = KeyChord::from_event(key)?;
        let context = self.action_context();
        let invocation =
            match self.actions.resolve_keybinding(&mut self.keybinding_state, chord, context) {
                KeybindingResolution::Invoke(invocation) => invocation,
                KeybindingResolution::Pending | KeybindingResolution::UnmatchedSequence { .. } => {
                    return None
                }
                KeybindingResolution::Unmatched => return None,
            };
        match self.actions.command_for(&invocation) {
            Ok(command) => Some(command),
            Err(error) => {
                self.notice = error.to_string();
                None
            }
        }
    }

    fn select_key(&mut self, event: Event) -> SelectAction {
        let key = match event {
            Event::Key(key) => key,
            Event::Mouse(mouse) => {
                let position = Position::new(mouse.column, mouse.row);
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        if let Some(index) = self
                            .regions
                            .workflows
                            .iter()
                            .find(|(area, _)| area.contains(position))
                            .map(|(_, index)| *index)
                        {
                            self.selected_workflow = index;
                        }
                    }
                    MouseEventKind::ScrollUp => {
                        self.selected_workflow = self.selected_workflow.saturating_sub(1)
                    }
                    MouseEventKind::ScrollDown => {
                        self.selected_workflow =
                            (self.selected_workflow + 1).min(self.workflows.len().saturating_sub(1))
                    }
                    _ => {}
                }
                return SelectAction::None;
            }
            _ => return SelectAction::None,
        };
        match self.resolve_action_key(key) {
            Some(BuildCommand::Quit | BuildCommand::CancelOrQuit | BuildCommand::Escape) => {
                SelectAction::Quit
            }
            Some(BuildCommand::OpenEvidence) => SelectAction::Evidence,
            Some(BuildCommand::MoveUp) => {
                self.selected_workflow = self.selected_workflow.saturating_sub(1);
                SelectAction::None
            }
            Some(BuildCommand::MoveDown) => {
                self.selected_workflow =
                    (self.selected_workflow + 1).min(self.workflows.len().saturating_sub(1));
                SelectAction::None
            }
            Some(BuildCommand::Activate) => match self.workflows.get(self.selected_workflow) {
                Some(workflow) if workflow.availability.ready() => {
                    SelectAction::Start(self.selected_workflow)
                }
                Some(WorkflowRow {
                    id,
                    availability: WorkflowAvailability::Unavailable { reason },
                    ..
                }) => {
                    self.notice = format!("{id} is unavailable: {reason}");
                    SelectAction::None
                }
                None => SelectAction::None,
                Some(_) => SelectAction::None,
            },
            Some(
                BuildCommand::PageUp
                | BuildCommand::PageDown
                | BuildCommand::NextPane
                | BuildCommand::PreviousPane
                | BuildCommand::Cancel
                | BuildCommand::ChooseWorkflow
                | BuildCommand::Back
                | BuildCommand::Forget
                | BuildCommand::Confirm
                | BuildCommand::Decline,
            )
            | None => SelectAction::None,
        }
    }

    fn active_key(&mut self, event: Event, terminal_ready: bool) -> ActiveAction {
        let key = match event {
            Event::Key(key) => key,
            Event::Mouse(mouse) => return self.active_mouse(mouse),
            Event::Resize(_, _) => {
                self.scrollbar_drag = None;
                return ActiveAction::None;
            }
            _ => return ActiveAction::None,
        };
        match self.selection.on_key(key) {
            SelectionOutcome::CopyReady(text) => return ActiveAction::Copy(text),
            SelectionOutcome::Captured | SelectionOutcome::Changed => return ActiveAction::None,
            SelectionOutcome::Unhandled | SelectionOutcome::EdgeScroll { .. } => {}
        }
        if let Some(command) = self.resolve_action_key(key) {
            return self.execute_active_command(command, terminal_ready);
        }
        match key.code {
            KeyCode::Char(key) if terminal_ready => self
                .configured_action_index(key)
                .map(ActiveAction::RunConfigured)
                .unwrap_or(ActiveAction::None),
            _ => ActiveAction::None,
        }
    }

    fn execute_active_command(
        &mut self,
        command: BuildCommand,
        terminal_ready: bool,
    ) -> ActiveAction {
        match command {
            BuildCommand::Quit => ActiveAction::Quit,
            BuildCommand::CancelOrQuit => {
                if terminal_ready {
                    ActiveAction::Quit
                } else {
                    ActiveAction::Cancel
                }
            }
            BuildCommand::Escape | BuildCommand::Back => match &self.evidence_mode {
                EvidenceMode::ConfirmForget(_) => {
                    self.evidence_mode = EvidenceMode::List;
                    ActiveAction::None
                }
                EvidenceMode::Inspect(_) if self.view == View::Evidence => {
                    self.evidence_mode = EvidenceMode::List;
                    ActiveAction::None
                }
                EvidenceMode::List if self.view == View::Evidence => {
                    self.close_evidence();
                    ActiveAction::None
                }
                _ if terminal_ready => ActiveAction::ChooseWorkflow,
                _ => ActiveAction::None,
            },
            BuildCommand::OpenEvidence if terminal_ready => ActiveAction::Evidence,
            BuildCommand::OpenEvidence => ActiveAction::None,
            BuildCommand::NextPane => {
                self.cycle_run_pane(true);
                ActiveAction::None
            }
            BuildCommand::PreviousPane => {
                self.cycle_run_pane(false);
                ActiveAction::None
            }
            BuildCommand::MoveUp => {
                match &self.evidence_mode {
                    EvidenceMode::List if self.view == View::Evidence => {
                        self.selected_evidence = self.selected_evidence.saturating_sub(1)
                    }
                    EvidenceMode::Inspect(_) if self.view == View::Evidence => {
                        self.scroll_surface(BuildSurface::Evidence, -1)
                    }
                    _ => self.scroll_active_pane(false, 1),
                }
                ActiveAction::None
            }
            BuildCommand::MoveDown => {
                match &self.evidence_mode {
                    EvidenceMode::List if self.view == View::Evidence => {
                        self.selected_evidence =
                            (self.selected_evidence + 1).min(self.evidence.len().saturating_sub(1))
                    }
                    EvidenceMode::Inspect(_) if self.view == View::Evidence => {
                        self.scroll_surface(BuildSurface::Evidence, 1)
                    }
                    _ => self.scroll_active_pane(true, 1),
                }
                ActiveAction::None
            }
            BuildCommand::PageUp => {
                if matches!(self.evidence_mode, EvidenceMode::Inspect(_))
                    && self.view == View::Evidence
                {
                    self.evidence_inspect_scroll.page_by(-1, self.evidence_inspect_metrics);
                } else {
                    self.scroll_active_pane(false, PAGE_ROWS);
                }
                ActiveAction::None
            }
            BuildCommand::PageDown => {
                if matches!(self.evidence_mode, EvidenceMode::Inspect(_))
                    && self.view == View::Evidence
                {
                    self.evidence_inspect_scroll.page_by(1, self.evidence_inspect_metrics);
                } else {
                    self.scroll_active_pane(true, PAGE_ROWS);
                }
                ActiveAction::None
            }
            BuildCommand::Cancel => {
                if terminal_ready {
                    ActiveAction::None
                } else {
                    ActiveAction::Cancel
                }
            }
            BuildCommand::ChooseWorkflow => {
                if terminal_ready {
                    ActiveAction::ChooseWorkflow
                } else {
                    ActiveAction::None
                }
            }
            BuildCommand::Activate => {
                if let Some(record) = self.evidence.get(self.selected_evidence) {
                    match evidence::tui_inspect(&record.run_id) {
                        Ok(contents) => {
                            self.selection.clear();
                            self.content_revision = self.content_revision.wrapping_add(1);
                            self.evidence_inspect_scroll = FollowViewport::at_top();
                            self.evidence_inspect_metrics = ViewportMetrics::default();
                            self.evidence_mode = EvidenceMode::Inspect(contents);
                        }
                        Err(error) => {
                            self.notice = format!("could not inspect evidence: {error:#}")
                        }
                    }
                }
                ActiveAction::None
            }
            BuildCommand::Forget => {
                if let Some(record) = self.evidence.get(self.selected_evidence) {
                    self.evidence_mode = EvidenceMode::ConfirmForget(record.run_id.clone());
                }
                ActiveAction::None
            }
            BuildCommand::Confirm => {
                if let EvidenceMode::ConfirmForget(run_id) = &self.evidence_mode {
                    self.notice = format!("forgetting evidence {run_id}");
                    self.evidence_mode = EvidenceMode::ForgetReady(run_id.clone());
                }
                ActiveAction::None
            }
            BuildCommand::Decline => {
                self.evidence_mode = EvidenceMode::List;
                ActiveAction::None
            }
        }
    }

    fn active_mouse(&mut self, mouse: MouseEvent) -> ActiveAction {
        if let Some((surface, drag)) = self.scrollbar_drag {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(layout) = self
                        .regions
                        .scrollbars
                        .iter()
                        .find(|(candidate, _)| *candidate == surface)
                        .map(|(_, layout)| *layout)
                    {
                        let top = drag.top_for_row(layout, mouse.row);
                        self.set_surface_top(surface, top);
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => self.scrollbar_drag = None,
                _ => {}
            }
            return ActiveAction::None;
        }
        if self.selection.is_dragging() {
            match self.selection.on_mouse(mouse) {
                SelectionOutcome::CopyReady(text) => return ActiveAction::Copy(text),
                SelectionOutcome::Captured | SelectionOutcome::Changed => {
                    return ActiveAction::None
                }
                SelectionOutcome::EdgeScroll { surface, lines } => {
                    self.scroll_surface(surface, lines);
                    return ActiveAction::None;
                }
                SelectionOutcome::Unhandled => {}
            }
        }
        let position = Position::new(mouse.column, mouse.row);
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some((surface, layout)) = self
                .regions
                .scrollbars
                .iter()
                .find(|(_, layout)| layout.contains(position))
                .copied()
            {
                self.selection.clear();
                if let Some(drag) = ScrollbarDrag::begin(layout, position) {
                    self.scrollbar_drag = Some((surface, drag));
                } else {
                    self.set_surface_top(surface, layout.top_for_track_row(mouse.row));
                }
                return ActiveAction::None;
            }
        }
        if self.view == View::Evidence {
            match (&self.evidence_mode, mouse.kind) {
                (EvidenceMode::List, MouseEventKind::Down(MouseButton::Left)) => {
                    if let Some(index) = self
                        .regions
                        .evidence_records
                        .iter()
                        .find(|(area, _)| area.contains(position))
                        .map(|(_, index)| *index)
                    {
                        self.selected_evidence = index;
                    }
                }
                (EvidenceMode::List, MouseEventKind::ScrollUp) => {
                    self.selected_evidence = self.selected_evidence.saturating_sub(1)
                }
                (EvidenceMode::List, MouseEventKind::ScrollDown) => {
                    self.selected_evidence =
                        (self.selected_evidence + 1).min(self.evidence.len().saturating_sub(1))
                }
                (EvidenceMode::Inspect(_), MouseEventKind::ScrollUp) => {
                    self.scroll_surface(BuildSurface::Evidence, -1)
                }
                (EvidenceMode::Inspect(_), MouseEventKind::ScrollDown) => {
                    self.scroll_surface(BuildSurface::Evidence, 1)
                }
                (EvidenceMode::Inspect(_), MouseEventKind::Down(MouseButton::Left)) => {
                    let _ = self.selection.on_mouse(mouse);
                }
                _ => {}
            }
            return ActiveAction::None;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(pane) = self
                    .regions
                    .run_panes
                    .iter()
                    .find(|(area, _)| area.contains(position))
                    .map(|(_, pane)| *pane)
                {
                    self.run_pane = pane;
                }
                let _ = self.selection.on_mouse(mouse);
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if let Some(pane) = self
                    .regions
                    .run_panes
                    .iter()
                    .find(|(area, _)| area.contains(position))
                    .map(|(_, pane)| *pane)
                {
                    self.run_pane = pane;
                    self.scroll_active_pane(matches!(mouse.kind, MouseEventKind::ScrollDown), 1);
                    self.selection.clear();
                }
            }
            _ => {}
        }
        ActiveAction::None
    }

    fn scroll_surface(&mut self, surface: BuildSurface, lines: isize) {
        let (viewport, metrics) = match surface {
            BuildSurface::Stages => (&mut self.stage_scroll, self.stage_metrics),
            BuildSurface::Events => (&mut self.event_scroll, self.event_metrics),
            BuildSurface::Stdout => (&mut self.stdout_scroll, self.stdout_metrics),
            BuildSurface::Stderr => (&mut self.stderr_scroll, self.stderr_metrics),
            BuildSurface::Terminal => (&mut self.terminal_scroll, self.terminal_metrics),
            BuildSurface::Evidence => {
                (&mut self.evidence_inspect_scroll, self.evidence_inspect_metrics)
            }
        };
        viewport.scroll_by(lines, metrics);
    }

    fn set_surface_top(&mut self, surface: BuildSurface, top: usize) {
        let (viewport, metrics) = match surface {
            BuildSurface::Stages => (&mut self.stage_scroll, self.stage_metrics),
            BuildSurface::Events => (&mut self.event_scroll, self.event_metrics),
            BuildSurface::Stdout => (&mut self.stdout_scroll, self.stdout_metrics),
            BuildSurface::Stderr => (&mut self.stderr_scroll, self.stderr_metrics),
            BuildSurface::Terminal => (&mut self.terminal_scroll, self.terminal_metrics),
            BuildSurface::Evidence => {
                (&mut self.evidence_inspect_scroll, self.evidence_inspect_metrics)
            }
        };
        viewport.set_top(top, metrics);
    }

    fn ingest_update(&mut self, update: BuildUpdate) {
        match update {
            BuildUpdate::Started { run_id, workflow_id, workflow_label } => {
                self.run_id = run_id;
                self.notice = format!(
                    "running {workflow_id} ({workflow_label}) under complete-tree supervision"
                );
            }
            BuildUpdate::Events(events) => {
                for event in events {
                    self.ingest_event(event);
                }
            }
            BuildUpdate::Terminal(terminal) => {
                self.notice = terminal_summary(&terminal);
                self.exit_failure = match &terminal {
                    BuildTerminal::Succeeded(_) => None,
                    BuildTerminal::Failed { message, .. }
                    | BuildTerminal::InfrastructureFailure { message, .. } => Some(message.clone()),
                };
                self.terminal = Some(terminal);
                self.view = View::Terminal;
            }
        }
    }

    fn ingest_transcripts(&mut self, tails: BuildTranscriptTails) {
        self.stdout = tails.stdout;
        self.stderr = tails.stderr;
    }

    fn ingest_event(&mut self, event: super::protocol::BuildEvent) {
        use super::protocol::{BuildEventKind, StageOutcome};

        match event.event {
            BuildEventKind::RunStarted {} => {
                self.push_event(format!("#{:04} provider started", event.sequence))
            }
            BuildEventKind::StageStarted { stage_id, parent_stage_id } => {
                self.stage_order.push_back(stage_id.clone());
                self.stages.insert(
                    stage_id.clone(),
                    StageRow {
                        id: super::escape_inline_terminal_controls(&stage_id),
                        parent: parent_stage_id,
                        state: StageState::Running,
                        detail: "started".to_owned(),
                    },
                );
                while self.stage_order.len() > MAX_STAGE_ROWS {
                    if let Some(expired) = self.stage_order.pop_front() {
                        self.stages.remove(&expired);
                        if let FollowViewport::Historical(top) = &mut self.stage_scroll {
                            *top = top.saturating_sub(1);
                        }
                        self.content_revision = self.content_revision.wrapping_add(1);
                    }
                }
                self.push_event(format!(
                    "#{:04} stage {} started",
                    event.sequence,
                    super::escape_inline_terminal_controls(&stage_id)
                ));
            }
            BuildEventKind::StageProgress { stage_id, message } => {
                if let Some(stage) = self.stages.get_mut(&stage_id) {
                    stage.detail = super::escape_inline_terminal_controls(&message);
                }
                self.push_event(format!(
                    "#{:04} {}: {}",
                    event.sequence,
                    super::escape_inline_terminal_controls(&stage_id),
                    super::escape_inline_terminal_controls(&message)
                ));
            }
            BuildEventKind::StageFinished { stage_id, outcome } => {
                let state = match outcome {
                    StageOutcome::Succeeded => StageState::Succeeded,
                    StageOutcome::Failed => StageState::Failed,
                };
                if let Some(stage) = self.stages.get_mut(&stage_id) {
                    stage.state = state;
                    stage.detail = stage_state_label(state).to_owned();
                }
                self.push_event(format!(
                    "#{:04} stage {} {}",
                    event.sequence,
                    super::escape_inline_terminal_controls(&stage_id),
                    stage_state_label(state)
                ));
            }
            BuildEventKind::ArtifactReported { path, label } => {
                self.push_event(format!(
                    "#{:04} artifact {}: {}",
                    event.sequence,
                    super::escape_inline_terminal_controls(&label),
                    super::escape_inline_terminal_controls(&path)
                ));
            }
            BuildEventKind::EvidenceReported { path, label } => {
                self.push_event(format!(
                    "#{:04} evidence {}: {}",
                    event.sequence,
                    super::escape_inline_terminal_controls(&label),
                    super::escape_inline_terminal_controls(&path)
                ));
            }
        }
    }

    fn push_event(&mut self, line: String) {
        self.events.push_back(line);
        while self.events.len() > MAX_EVENT_LINES {
            self.events.pop_front();
            if let FollowViewport::Historical(top) = &mut self.event_scroll {
                *top = top.saturating_sub(1);
            }
            self.content_revision = self.content_revision.wrapping_add(1);
        }
    }

    fn note_missing_terminal(&mut self, engine_fallback: Option<&str>) {
        if self.terminal.is_none() {
            let message = engine_fallback
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "Build engine completed without a terminal update".to_owned());
            self.terminal = Some(BuildTerminal::InfrastructureFailure {
                message,
                evidence: BuildFailureEvidence::NotRetained {
                    reason: "the Build engine did not publish a terminal update".to_owned(),
                },
            });
            self.exit_failure = self.terminal.as_ref().and_then(|terminal| match terminal {
                BuildTerminal::InfrastructureFailure { message, .. } => Some(message.clone()),
                BuildTerminal::Succeeded(_) | BuildTerminal::Failed { .. } => None,
            });
            self.notice = "Build engine stopped before publishing a terminal update".to_owned();
            self.view = View::Terminal;
        }
    }

    fn open_evidence(&mut self) {
        self.selection.clear();
        self.scrollbar_drag = None;
        self.evidence = match evidence::tui_records() {
            Ok(records) => records,
            Err(error) => {
                self.notice = format!("could not load build evidence: {error:#}");
                Vec::new()
            }
        };
        self.selected_evidence = self.selected_evidence.min(self.evidence.len().saturating_sub(1));
        self.evidence_mode = EvidenceMode::List;
        self.view = View::Evidence;
    }

    fn close_evidence(&mut self) {
        self.selection.clear();
        self.scrollbar_drag = None;
        self.view = if self.terminal.is_some() { View::Terminal } else { View::Select };
        self.evidence_mode = EvidenceMode::List;
    }

    fn evidence_action(&mut self) -> Option<EvidenceAction> {
        let EvidenceMode::ForgetReady(run_id) = &self.evidence_mode else {
            return None;
        };
        let run_id = run_id.clone();
        self.evidence_mode = EvidenceMode::List;
        Some(EvidenceAction::Forget(run_id))
    }

    fn configured_action(&self, index: usize) -> Option<&WorkflowAction> {
        self.terminal.as_ref()?;
        self.configured_actions.get(index)
    }

    fn configured_action_index(&self, key: char) -> Option<usize> {
        self.terminal.as_ref()?;
        self.configured_actions.iter().position(|action| action.key == key)
    }

    fn terminal_controls(&self) -> String {
        let mut controls = vec![self.action_help()];
        if self.terminal.is_some() {
            controls.extend(
                self.configured_actions
                    .iter()
                    .map(|action| format!("{} {}", action.key, action.label.as_str())),
            );
        }
        controls.join("  ")
    }

    fn action_help(&self) -> String {
        let context = self.action_context();
        self.actions
            .resolve_command_palette(&context)
            .items()
            .iter()
            .filter_map(|action| {
                action.primary_keybinding().map(|binding| format!("{binding} {}", action.title))
            })
            .collect::<Vec<_>>()
            .join("  ")
    }

    fn elapsed(&self) -> String {
        let Some(started) = self.run_started else {
            return "--:--".to_owned();
        };
        let seconds = started.elapsed().as_secs();
        format!("{:02}:{:02}:{:02}", seconds / 3600, (seconds / 60) % 60, seconds % 60)
    }

    fn stage_depth(&self, stage: &StageRow) -> usize {
        let mut depth = 0usize;
        let mut parent = stage.parent.as_deref();
        while let Some(parent_id) = parent {
            let Some(parent_stage) = self.stages.get(parent_id) else {
                break;
            };
            depth = depth.saturating_add(1);
            parent = parent_stage.parent.as_deref();
        }
        depth
    }

    fn cycle_run_pane(&mut self, forward: bool) {
        let panes = if self.terminal.is_some() {
            &[RunPane::Stages, RunPane::Events, RunPane::Stdout, RunPane::Stderr, RunPane::Terminal]
                [..]
        } else {
            &[RunPane::Stages, RunPane::Events, RunPane::Stdout, RunPane::Stderr][..]
        };
        let current = panes.iter().position(|pane| *pane == self.run_pane).unwrap_or(0);
        let next = if forward {
            (current + 1) % panes.len()
        } else {
            current.checked_sub(1).unwrap_or(panes.len() - 1)
        };
        self.run_pane = panes[next];
    }

    fn scroll_active_pane(&mut self, down: bool, rows: usize) {
        let (scroll, metrics) = match self.run_pane {
            RunPane::Stages => (&mut self.stage_scroll, self.stage_metrics),
            RunPane::Events => (&mut self.event_scroll, self.event_metrics),
            RunPane::Stdout => (&mut self.stdout_scroll, self.stdout_metrics),
            RunPane::Stderr => (&mut self.stderr_scroll, self.stderr_metrics),
            RunPane::Terminal => (&mut self.terminal_scroll, self.terminal_metrics),
        };
        let delta = isize::try_from(rows).unwrap_or(isize::MAX);
        scroll.scroll_by(if down { delta } else { -delta }, metrics);
    }

    fn exit_result(&self) -> Result<()> {
        match &self.exit_failure {
            Some(message) => Err(anyhow!(message.clone())),
            None => Ok(()),
        }
    }
}

fn stage_state_label(state: StageState) -> &'static str {
    match state {
        StageState::Running => "running",
        StageState::Succeeded => "succeeded",
        StageState::Failed => "failed",
    }
}

fn terminal_summary(terminal: &BuildTerminal) -> String {
    match terminal {
        BuildTerminal::Succeeded(output) => {
            format!("succeeded: {} events, run {}", output.event_count, output.run_id)
        }
        BuildTerminal::Failed { evidence, .. } => {
            format!("failed: {}", evidence_summary(evidence))
        }
        BuildTerminal::InfrastructureFailure { evidence, .. } => {
            format!("infrastructure failure: {}", evidence_summary(evidence))
        }
    }
}

fn evidence_summary(evidence: &BuildFailureEvidence) -> String {
    match evidence {
        BuildFailureEvidence::Retained { run_id } => {
            format!("evidence retained for run {run_id}")
        }
        BuildFailureEvidence::NotRetained { reason } => {
            format!("evidence not retained ({})", super::escape_inline_terminal_controls(reason))
        }
        BuildFailureEvidence::NotCreated => "no process evidence record was created".to_owned(),
    }
}

fn viewport_start(selected: usize, total: usize, capacity: usize) -> usize {
    if capacity == 0 || total <= capacity {
        return 0;
    }
    selected.saturating_sub(capacity / 2).min(total - capacity)
}

fn wrapped_rows(text: &str, width: u16) -> usize {
    let width = usize::from(width.max(1));
    text.split('\n')
        .map(|line| line.width().max(1).div_ceil(width))
        .fold(0usize, usize::saturating_add)
}

fn update_follow_layout(
    viewport: &mut FollowViewport,
    metrics: &mut ViewportMetrics,
    total_rows: usize,
    viewport_rows: usize,
) -> usize {
    *metrics = ViewportMetrics::new(total_rows, viewport_rows.max(1));
    viewport.normalize(*metrics);
    viewport.top(*metrics)
}

fn scroll_row(top: usize) -> u16 {
    u16::try_from(top).unwrap_or(u16::MAX)
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    app.regions = UiRegions::default();
    match app.view {
        View::Select => render_selection(frame, app),
        View::Running | View::Terminal => render_run(frame, app),
        View::Evidence => render_evidence(frame, app),
    }
    let selectable = app.regions.selectable.clone();
    app.selection.capture_frame(
        frame,
        &selectable,
        Style::default().bg(NORD.selection).add_modifier(Modifier::REVERSED),
    );
}

fn block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(NORD.border))
        .title(Span::styled(title, Style::default().fg(NORD.accent).add_modifier(Modifier::BOLD)))
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let color = if focused { NORD.focus } else { NORD.border };
    block(title).border_style(Style::default().fg(color))
}

fn content_rect(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

fn selectable_content_rect(area: Rect, metrics: ViewportMetrics) -> Rect {
    let inner = content_rect(area);
    Rect::new(
        inner.x,
        inner.y,
        inner.width.saturating_sub(u16::from(metrics.has_overflow())),
        inner.height,
    )
}

fn render_scrollbar(
    frame: &mut Frame<'_>,
    regions: &mut UiRegions,
    surface: BuildSurface,
    area: Rect,
    viewport: FollowViewport,
    metrics: ViewportMetrics,
    dragging: bool,
) {
    let Some(layout) =
        ScrollbarLayout::vertical_right(content_rect(area), metrics, viewport.top(metrics))
    else {
        return;
    };
    regions.scrollbars.push((surface, layout));
    render_vertical_scrollbar(
        frame,
        layout,
        dragging,
        ScrollbarStyle {
            track_color: NORD.border,
            thumb_color: NORD.text_muted,
            active_thumb_color: NORD.accent,
            track_symbol: "│",
            thumb_symbol: "┃",
        },
    );
}

fn render_selection(frame: &mut Frame<'_>, app: &mut App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(6), Constraint::Length(4)])
        .split(frame.area());
    let header = vec![
        Line::from(Span::styled(
            "KIT BUILD",
            Style::default().fg(NORD.text_strong).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("{}  |  {}", app.root, app.manifest)),
    ];
    frame.render_widget(Paragraph::new(header).block(block("Repository build provider")), areas[0]);
    let workflow_capacity = usize::from(areas[1].height.saturating_sub(2));
    let workflow_start =
        viewport_start(app.selected_workflow, app.workflows.len(), workflow_capacity);
    let workflow_inner = content_rect(areas[1]);
    for (visible_row, index) in
        (workflow_start..app.workflows.len()).take(workflow_capacity).enumerate()
    {
        app.regions.workflows.push((
            Rect::new(
                workflow_inner.x,
                workflow_inner.y.saturating_add(visible_row as u16),
                workflow_inner.width,
                1,
            ),
            index,
        ));
    }
    let items = app
        .workflows
        .iter()
        .enumerate()
        .skip(workflow_start)
        .take(workflow_capacity)
        .map(|(index, workflow)| {
            let selected = index == app.selected_workflow;
            let status = if workflow.availability.ready() { "ready" } else { "unavailable" };
            let status_style = if workflow.availability.ready() {
                Style::default().fg(NORD.success)
            } else {
                Style::default().fg(NORD.warning)
            };
            let style = if selected {
                Style::default().bg(NORD.selection).fg(NORD.text_strong)
            } else {
                Style::default().fg(NORD.text)
            };
            ListItem::new(Line::from(vec![
                Span::styled(if selected { "› " } else { "  " }, style),
                Span::styled(format!("{:<28}", workflow.id), style.add_modifier(Modifier::BOLD)),
                Span::styled(format!("{:<48}", workflow.label), style),
                Span::styled(status, status_style),
            ]))
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items).block(block("Workflows")), areas[1]);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(app.notice.as_str()),
            Line::from(Span::styled(app.action_help(), Style::default().fg(NORD.text_muted))),
        ])
        .block(block("Build console")),
        areas[2],
    );
}

fn render_run(frame: &mut Frame<'_>, app: &mut App) {
    let terminal_visible = app.terminal.is_some();
    let constraints = if terminal_visible {
        vec![
            Constraint::Length(4),
            Constraint::Percentage(35),
            Constraint::Length(8),
            Constraint::Percentage(52),
            Constraint::Length(3),
        ]
    } else {
        vec![
            Constraint::Length(4),
            Constraint::Percentage(43),
            Constraint::Percentage(47),
            Constraint::Length(3),
        ]
    };
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());
    let status = match app.view {
        View::Running => "RUNNING",
        View::Terminal => "TERMINAL",
        _ => unreachable!(),
    };
    let header = vec![
        Line::from(vec![
            Span::styled(
                format!("{status}  "),
                Style::default()
                    .fg(if app.view == View::Running { NORD.focus } else { NORD.accent })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("elapsed {}  ", app.elapsed())),
            Span::styled(
                if app.run_id.is_empty() {
                    "allocating run id".to_owned()
                } else {
                    format!("run {}", app.run_id)
                },
                Style::default().fg(NORD.text_muted),
            ),
        ]),
        Line::from(app.notice.as_str()),
    ];
    frame.render_widget(Paragraph::new(header).block(block("Kit process supervisor")), areas[0]);

    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(areas[1]);
    let stage_capacity = usize::from(top[0].height.saturating_sub(2));
    let stage_start = update_follow_layout(
        &mut app.stage_scroll,
        &mut app.stage_metrics,
        app.stage_order.len(),
        stage_capacity,
    );
    app.regions.run_panes.push((top[0], RunPane::Stages));
    app.regions.selectable.push(SelectableRegion::new(
        BuildSurface::Stages,
        selectable_content_rect(top[0], app.stage_metrics),
        i64::try_from(stage_start).unwrap_or(i64::MAX),
        0,
        app.content_revision,
    ));
    let stages = app
        .stage_order
        .iter()
        .skip(stage_start)
        .take(stage_capacity)
        .filter_map(|id| app.stages.get(id))
        .map(|stage| {
            let indentation = "  ".repeat(app.stage_depth(stage));
            let marker = match stage.state {
                StageState::Running => Span::styled("●", Style::default().fg(NORD.focus)),
                StageState::Succeeded => Span::styled("✓", Style::default().fg(NORD.success)),
                StageState::Failed => Span::styled("×", Style::default().fg(NORD.danger)),
            };
            ListItem::new(Line::from(vec![
                marker,
                Span::raw(format!(" {indentation}{}  ", stage.id)),
                Span::styled(stage.detail.as_str(), Style::default().fg(NORD.text_muted)),
            ]))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(stages).block(pane_block(
            &format!("Validated stages (last {MAX_STAGE_ROWS})"),
            app.run_pane == RunPane::Stages,
        )),
        top[0],
    );
    render_scrollbar(
        frame,
        &mut app.regions,
        BuildSurface::Stages,
        top[0],
        app.stage_scroll,
        app.stage_metrics,
        app.scrollbar_drag.is_some_and(|(surface, _)| surface == BuildSurface::Stages),
    );
    let event_capacity = usize::from(top[1].height.saturating_sub(2));
    let event_start = update_follow_layout(
        &mut app.event_scroll,
        &mut app.event_metrics,
        app.events.len(),
        event_capacity,
    );
    app.regions.run_panes.push((top[1], RunPane::Events));
    app.regions.selectable.push(SelectableRegion::new(
        BuildSurface::Events,
        selectable_content_rect(top[1], app.event_metrics),
        i64::try_from(event_start).unwrap_or(i64::MAX),
        0,
        app.content_revision,
    ));
    let event_lines = app
        .events
        .iter()
        .skip(event_start)
        .take(event_capacity)
        .map(|line| ListItem::new(Line::from(line.as_str())))
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(event_lines).block(pane_block(
            &format!("Protocol events (last {MAX_EVENT_LINES})"),
            app.run_pane == RunPane::Events,
        )),
        top[1],
    );
    render_scrollbar(
        frame,
        &mut app.regions,
        BuildSurface::Events,
        top[1],
        app.event_scroll,
        app.event_metrics,
        app.scrollbar_drag.is_some_and(|(surface, _)| surface == BuildSurface::Events),
    );

    let terminal_area = terminal_visible.then_some(areas[2]);
    if let Some(area) = terminal_area {
        let terminal = app.terminal.as_ref().expect("terminal visibility follows terminal state");
        let message = terminal_message(terminal);
        let terminal_top = update_follow_layout(
            &mut app.terminal_scroll,
            &mut app.terminal_metrics,
            wrapped_rows(message, area.width.saturating_sub(2)),
            usize::from(area.height.saturating_sub(2)),
        );
        app.regions.run_panes.push((area, RunPane::Terminal));
        app.regions.selectable.push(SelectableRegion::new(
            BuildSurface::Terminal,
            selectable_content_rect(area, app.terminal_metrics),
            i64::try_from(terminal_top).unwrap_or(i64::MAX),
            0,
            app.content_revision,
        ));
        frame.render_widget(
            Paragraph::new(message)
                .block(pane_block("Terminal truth", app.run_pane == RunPane::Terminal))
                .wrap(Wrap { trim: false })
                .scroll((scroll_row(terminal_top), 0)),
            area,
        );
        render_scrollbar(
            frame,
            &mut app.regions,
            BuildSurface::Terminal,
            area,
            app.terminal_scroll,
            app.terminal_metrics,
            app.scrollbar_drag.is_some_and(|(surface, _)| surface == BuildSurface::Terminal),
        );
    }
    let tails_area = if terminal_visible { areas[3] } else { areas[2] };
    let controls_area = if terminal_visible { areas[4] } else { areas[3] };
    let tails = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(tails_area);
    let stdout_top = update_follow_layout(
        &mut app.stdout_scroll,
        &mut app.stdout_metrics,
        wrapped_rows(app.stdout.as_ref(), tails[0].width.saturating_sub(2)),
        usize::from(tails[0].height.saturating_sub(2)),
    );
    let stderr_top = update_follow_layout(
        &mut app.stderr_scroll,
        &mut app.stderr_metrics,
        wrapped_rows(app.stderr.as_ref(), tails[1].width.saturating_sub(2)),
        usize::from(tails[1].height.saturating_sub(2)),
    );
    app.regions.run_panes.push((tails[0], RunPane::Stdout));
    app.regions.run_panes.push((tails[1], RunPane::Stderr));
    app.regions.selectable.push(SelectableRegion::new(
        BuildSurface::Stdout,
        selectable_content_rect(tails[0], app.stdout_metrics),
        i64::try_from(stdout_top).unwrap_or(i64::MAX),
        0,
        app.content_revision,
    ));
    app.regions.selectable.push(SelectableRegion::new(
        BuildSurface::Stderr,
        selectable_content_rect(tails[1], app.stderr_metrics),
        i64::try_from(stderr_top).unwrap_or(i64::MAX),
        0,
        app.content_revision,
    ));
    frame.render_widget(
        Paragraph::new(if app.stdout.is_empty() { "<no stdout yet>" } else { app.stdout.as_ref() })
            .block(pane_block("stdout tail (bounded)", app.run_pane == RunPane::Stdout))
            .wrap(Wrap { trim: false })
            .scroll((scroll_row(stdout_top), 0)),
        tails[0],
    );
    render_scrollbar(
        frame,
        &mut app.regions,
        BuildSurface::Stdout,
        tails[0],
        app.stdout_scroll,
        app.stdout_metrics,
        app.scrollbar_drag.is_some_and(|(surface, _)| surface == BuildSurface::Stdout),
    );
    frame.render_widget(
        Paragraph::new(if app.stderr.is_empty() { "<no stderr yet>" } else { app.stderr.as_ref() })
            .block(pane_block("stderr tail (bounded)", app.run_pane == RunPane::Stderr))
            .wrap(Wrap { trim: false })
            .scroll((scroll_row(stderr_top), 0)),
        tails[1],
    );
    render_scrollbar(
        frame,
        &mut app.regions,
        BuildSurface::Stderr,
        tails[1],
        app.stderr_scroll,
        app.stderr_metrics,
        app.scrollbar_drag.is_some_and(|(surface, _)| surface == BuildSurface::Stderr),
    );

    let help = if app.view == View::Running { app.action_help() } else { app.terminal_controls() };
    frame.render_widget(Paragraph::new(help).block(block("Controls")), controls_area);
}

fn terminal_message(terminal: &BuildTerminal) -> &str {
    match terminal {
        BuildTerminal::Succeeded(_) => "The provider and Kit process evidence agree on success.",
        BuildTerminal::Failed { message, .. }
        | BuildTerminal::InfrastructureFailure { message, .. } => message,
    }
}

fn render_evidence(frame: &mut Frame<'_>, app: &mut App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(6), Constraint::Length(3)])
        .split(frame.area());
    frame.render_widget(
        Paragraph::new("Retained evidence is bounded and never evicts an older record. Inspect before forgetting.")
            .block(block("Build evidence")),
        areas[0],
    );
    match &app.evidence_mode {
        EvidenceMode::List => {
            let capacity = usize::from(areas[1].height.saturating_sub(2));
            let start = viewport_start(app.selected_evidence, app.evidence.len(), capacity);
            let records_inner = content_rect(areas[1]);
            for (visible_row, index) in (start..app.evidence.len()).take(capacity).enumerate() {
                app.regions.evidence_records.push((
                    Rect::new(
                        records_inner.x,
                        records_inner.y.saturating_add(visible_row as u16),
                        records_inner.width,
                        1,
                    ),
                    index,
                ));
            }
            let items = app
                .evidence
                .iter()
                .enumerate()
                .skip(start)
                .take(capacity)
                .map(|(index, record)| {
                    let selected = index == app.selected_evidence;
                    let style = if selected {
                        Style::default().bg(NORD.selection).fg(NORD.text_strong)
                    } else {
                        Style::default().fg(NORD.text)
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(if selected { "› " } else { "  " }, style),
                        Span::styled(
                            format!("{}  ", record.run_id),
                            style.add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("{}  {} bytes  ", record.state, record.bytes), style),
                        Span::styled(
                            record.workflow.as_str(),
                            Style::default().fg(NORD.text_muted),
                        ),
                    ]))
                })
                .collect::<Vec<_>>();
            let content = if items.is_empty() {
                List::new(vec![ListItem::new("No retained Build evidence")])
            } else {
                List::new(items)
            };
            frame.render_widget(content.block(block("Records")), areas[1]);
            frame.render_widget(
                Paragraph::new(app.action_help()).block(block("Controls")),
                areas[2],
            );
        }
        EvidenceMode::Inspect(contents) => {
            let evidence_top = update_follow_layout(
                &mut app.evidence_inspect_scroll,
                &mut app.evidence_inspect_metrics,
                wrapped_rows(contents, areas[1].width.saturating_sub(2)),
                usize::from(areas[1].height.saturating_sub(2)),
            );
            let evidence_area = content_rect(areas[1]);
            app.regions.evidence_inspect = Some(evidence_area);
            app.regions.selectable.push(SelectableRegion::new(
                BuildSurface::Evidence,
                selectable_content_rect(areas[1], app.evidence_inspect_metrics),
                i64::try_from(evidence_top).unwrap_or(i64::MAX),
                0,
                app.content_revision,
            ));
            frame.render_widget(
                Paragraph::new(contents.as_str())
                    .block(block("Validated evidence record"))
                    .wrap(Wrap { trim: false })
                    .scroll((scroll_row(evidence_top), 0)),
                areas[1],
            );
            render_scrollbar(
                frame,
                &mut app.regions,
                BuildSurface::Evidence,
                areas[1],
                app.evidence_inspect_scroll,
                app.evidence_inspect_metrics,
                app.scrollbar_drag.is_some_and(|(surface, _)| surface == BuildSurface::Evidence),
            );
            frame.render_widget(
                Paragraph::new(app.action_help()).block(block("Controls")),
                areas[2],
            );
        }
        EvidenceMode::ConfirmForget(run_id) => {
            frame.render_widget(Paragraph::new(format!("Forget Build evidence {run_id}? This removes only this exact retained record.\n\ny confirm  n cancel")).block(block("Confirm deliberate removal")), areas[1]);
            frame.render_widget(
                Paragraph::new(app.action_help()).block(block("Controls")),
                areas[2],
            );
        }
        EvidenceMode::ForgetReady(_) => {}
    }
}
