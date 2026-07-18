//! Interactive Build presentation over the same in-process Build execution engine used by
//! `kit build run`. This module owns terminal state only; provider protocol validation and
//! process control stay in `super`.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout},
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
    build_presentation_channel, build_runtime_availability, evidence, execute_build,
    load_interactive_manifest, BuildControl, BuildFailureEvidence, BuildInvocation,
    BuildRuntimeAvailability, BuildTerminal, BuildTranscriptTails, BuildUpdate,
};
use crate::{
    framework::Context,
    tui::theme::NORD,
    tui::{EventReader, Session, SessionOptions},
};

const TICK: Duration = Duration::from_millis(80);
const MAX_STAGE_ROWS: usize = 512;
const MAX_EVENT_LINES: usize = 2_048;
const MAX_UPDATE_DRAIN_PER_DRAW: usize = 8;
const PAGE_ROWS: usize = 12;

pub(super) async fn run(cx: &Context) -> Result<()> {
    let (root, manifest) = load_interactive_manifest(cx)?;
    let runtime = build_runtime_availability(cx).await;
    let mut session = Session::open(SessionOptions { mouse_capture: true })?;
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
                    if app.active_key(event, true) == ActiveAction::Quit {
                        return app.exit_result();
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
    app.begin_run();
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
                    ActiveAction::ChooseWorkflow | ActiveAction::Evidence => {}
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveExit {
    Quit,
    ChooseWorkflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectAction {
    None,
    Quit,
    Evidence,
    Start(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveAction {
    None,
    Cancel,
    Quit,
    ChooseWorkflow,
    Evidence,
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

#[derive(Clone, Debug)]
struct ScrollState {
    offset: usize,
    total_rows: usize,
    viewport_rows: usize,
    follow: bool,
}

impl ScrollState {
    fn following() -> Self {
        Self { offset: 0, total_rows: 0, viewport_rows: 1, follow: true }
    }

    fn at_top() -> Self {
        Self { offset: 0, total_rows: 0, viewport_rows: 1, follow: false }
    }

    fn update_layout(&mut self, total_rows: usize, viewport_rows: usize) {
        self.total_rows = total_rows;
        self.viewport_rows = viewport_rows.max(1);
        let bottom = self.bottom();
        self.offset = if self.follow { bottom } else { self.offset.min(bottom) };
    }

    fn scroll_up(&mut self, rows: usize) {
        self.follow = false;
        self.offset = self.offset.saturating_sub(rows);
    }

    fn scroll_down(&mut self, rows: usize) {
        let bottom = self.bottom();
        self.offset = self.offset.saturating_add(rows).min(bottom);
        self.follow = self.offset == bottom;
    }

    fn reset(&mut self) {
        self.offset = 0;
        self.total_rows = 0;
        self.viewport_rows = 1;
        self.follow = true;
    }

    fn offset_u16(&self) -> u16 {
        u16::try_from(self.offset).unwrap_or(u16::MAX)
    }

    fn bottom(&self) -> usize {
        self.total_rows.saturating_sub(self.viewport_rows)
    }
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
    stage_scroll: ScrollState,
    event_scroll: ScrollState,
    stdout_scroll: ScrollState,
    stderr_scroll: ScrollState,
    terminal_scroll: ScrollState,
    notice: String,
    terminal: Option<BuildTerminal>,
    exit_failure: Option<String>,
    evidence: Vec<evidence::TuiEvidenceRecord>,
    selected_evidence: usize,
    evidence_mode: EvidenceMode,
    evidence_inspect_scroll: ScrollState,
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
            stage_scroll: ScrollState::following(),
            event_scroll: ScrollState::following(),
            stdout_scroll: ScrollState::following(),
            stderr_scroll: ScrollState::following(),
            terminal_scroll: ScrollState::at_top(),
            notice: "Select a workflow. Build eligibility is evaluated on this host.".to_owned(),
            terminal: None,
            exit_failure: None,
            evidence: Vec::new(),
            selected_evidence: 0,
            evidence_mode: EvidenceMode::List,
            evidence_inspect_scroll: ScrollState::at_top(),
        }
    }

    fn reset_for_selection(&mut self) {
        self.view = View::Select;
        self.notice = "Select a workflow. Build eligibility is evaluated on this host.".to_owned();
        self.terminal = None;
    }

    fn begin_run(&mut self) {
        self.view = View::Running;
        self.run_started = Some(Instant::now());
        self.run_id.clear();
        self.stage_order.clear();
        self.stages.clear();
        self.events.clear();
        self.stdout = Arc::from("");
        self.stderr = Arc::from("");
        self.run_pane = RunPane::Stages;
        self.stage_scroll.reset();
        self.event_scroll.reset();
        self.stdout_scroll.reset();
        self.stderr_scroll.reset();
        self.terminal_scroll = ScrollState::at_top();
        self.notice = "preparing private workspace and supervisor".to_owned();
        self.terminal = None;
    }

    fn tick(&mut self) {}

    fn select_key(&mut self, event: Event) -> SelectAction {
        let Event::Key(key) = event else {
            return SelectAction::None;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return SelectAction::Quit;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => SelectAction::Quit,
            KeyCode::Char('e') => SelectAction::Evidence,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected_workflow = self.selected_workflow.saturating_sub(1);
                SelectAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected_workflow =
                    (self.selected_workflow + 1).min(self.workflows.len().saturating_sub(1));
                SelectAction::None
            }
            KeyCode::Enter => match self.workflows.get(self.selected_workflow) {
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
            _ => SelectAction::None,
        }
    }

    fn active_key(&mut self, event: Event, terminal_ready: bool) -> ActiveAction {
        let Event::Key(key) = event else {
            return ActiveAction::None;
        };
        if self.view == View::Evidence {
            return self.evidence_key(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return if terminal_ready { ActiveAction::Quit } else { ActiveAction::Cancel };
        }
        match key.code {
            KeyCode::Char('q') => ActiveAction::Quit,
            KeyCode::Char('c') if !terminal_ready => ActiveAction::Cancel,
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
                self.cycle_run_pane(true);
                ActiveAction::None
            }
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                self.cycle_run_pane(false);
                ActiveAction::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_active_pane(false, 1);
                ActiveAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_active_pane(true, 1);
                ActiveAction::None
            }
            KeyCode::PageUp => {
                self.scroll_active_pane(false, PAGE_ROWS);
                ActiveAction::None
            }
            KeyCode::PageDown => {
                self.scroll_active_pane(true, PAGE_ROWS);
                ActiveAction::None
            }
            KeyCode::Char('e') if terminal_ready => ActiveAction::Evidence,
            KeyCode::Char('r') if terminal_ready => ActiveAction::ChooseWorkflow,
            KeyCode::Esc if terminal_ready => ActiveAction::ChooseWorkflow,
            _ => ActiveAction::None,
        }
    }

    fn evidence_key(&mut self, key: KeyEvent) -> ActiveAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return ActiveAction::Quit;
        }
        match &self.evidence_mode {
            EvidenceMode::ConfirmForget(run_id) => match key.code {
                KeyCode::Char('y') => {
                    self.notice = format!("forgetting evidence {run_id}");
                    self.evidence_mode = EvidenceMode::ForgetReady(run_id.clone());
                    ActiveAction::None
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    self.evidence_mode = EvidenceMode::List;
                    ActiveAction::None
                }
                _ => ActiveAction::None,
            },
            EvidenceMode::Inspect(_) => match key.code {
                KeyCode::Esc | KeyCode::Char('b') => {
                    self.evidence_mode = EvidenceMode::List;
                    ActiveAction::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.evidence_inspect_scroll.scroll_up(1);
                    ActiveAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.evidence_inspect_scroll.scroll_down(1);
                    ActiveAction::None
                }
                KeyCode::PageUp => {
                    self.evidence_inspect_scroll.scroll_up(PAGE_ROWS);
                    ActiveAction::None
                }
                KeyCode::PageDown => {
                    self.evidence_inspect_scroll.scroll_down(PAGE_ROWS);
                    ActiveAction::None
                }
                _ => ActiveAction::None,
            },
            EvidenceMode::List => match key.code {
                KeyCode::Char('q') => ActiveAction::Quit,
                KeyCode::Esc | KeyCode::Char('b') => {
                    self.close_evidence();
                    ActiveAction::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected_evidence = self.selected_evidence.saturating_sub(1);
                    ActiveAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected_evidence =
                        (self.selected_evidence + 1).min(self.evidence.len().saturating_sub(1));
                    ActiveAction::None
                }
                KeyCode::Char('i') | KeyCode::Enter => {
                    if let Some(record) = self.evidence.get(self.selected_evidence) {
                        match evidence::tui_inspect(&record.run_id) {
                            Ok(contents) => {
                                self.evidence_inspect_scroll = ScrollState::at_top();
                                self.evidence_mode = EvidenceMode::Inspect(contents);
                            }
                            Err(error) => {
                                self.notice = format!("could not inspect evidence: {error:#}")
                            }
                        }
                    }
                    ActiveAction::None
                }
                KeyCode::Char('f') => {
                    if let Some(record) = self.evidence.get(self.selected_evidence) {
                        self.evidence_mode = EvidenceMode::ConfirmForget(record.run_id.clone());
                    }
                    ActiveAction::None
                }
                _ => ActiveAction::None,
            },
            EvidenceMode::ForgetReady(_) => ActiveAction::None,
        }
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
                        if !self.stage_scroll.follow {
                            self.stage_scroll.offset = self.stage_scroll.offset.saturating_sub(1);
                        }
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
            if !self.event_scroll.follow {
                self.event_scroll.offset = self.event_scroll.offset.saturating_sub(1);
            }
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
        let scroll = match self.run_pane {
            RunPane::Stages => &mut self.stage_scroll,
            RunPane::Events => &mut self.event_scroll,
            RunPane::Stdout => &mut self.stdout_scroll,
            RunPane::Stderr => &mut self.stderr_scroll,
            RunPane::Terminal => &mut self.terminal_scroll,
        };
        if down {
            scroll.scroll_down(rows);
        } else {
            scroll.scroll_up(rows);
        }
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

fn render(frame: &mut Frame<'_>, app: &mut App) {
    match app.view {
        View::Select => render_selection(frame, app),
        View::Running | View::Terminal => render_run(frame, app),
        View::Evidence => render_evidence(frame, app),
    }
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

fn render_selection(frame: &mut Frame<'_>, app: &App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(6), Constraint::Length(3)])
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
            Line::from(Span::styled(
                "Enter run  j/k select  e evidence  q quit",
                Style::default().fg(NORD.text_muted),
            )),
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
    app.stage_scroll.update_layout(app.stage_order.len(), stage_capacity);
    let stage_start = app.stage_scroll.offset;
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
    let event_capacity = usize::from(top[1].height.saturating_sub(2));
    app.event_scroll.update_layout(app.events.len(), event_capacity);
    let event_start = app.event_scroll.offset;
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

    let terminal_area = terminal_visible.then_some(areas[2]);
    if let Some(area) = terminal_area {
        let terminal = app.terminal.as_ref().expect("terminal visibility follows terminal state");
        let message = terminal_message(terminal);
        app.terminal_scroll.update_layout(
            wrapped_rows(message, area.width.saturating_sub(2)),
            usize::from(area.height.saturating_sub(2)),
        );
        frame.render_widget(
            Paragraph::new(message)
                .block(pane_block("Terminal truth", app.run_pane == RunPane::Terminal))
                .wrap(Wrap { trim: false })
                .scroll((app.terminal_scroll.offset_u16(), 0)),
            area,
        );
    }
    let tails_area = if terminal_visible { areas[3] } else { areas[2] };
    let controls_area = if terminal_visible { areas[4] } else { areas[3] };
    let tails = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(tails_area);
    app.stdout_scroll.update_layout(
        wrapped_rows(app.stdout.as_ref(), tails[0].width.saturating_sub(2)),
        usize::from(tails[0].height.saturating_sub(2)),
    );
    app.stderr_scroll.update_layout(
        wrapped_rows(app.stderr.as_ref(), tails[1].width.saturating_sub(2)),
        usize::from(tails[1].height.saturating_sub(2)),
    );
    frame.render_widget(
        Paragraph::new(if app.stdout.is_empty() { "<no stdout yet>" } else { app.stdout.as_ref() })
            .block(pane_block("stdout tail (bounded)", app.run_pane == RunPane::Stdout))
            .wrap(Wrap { trim: false })
            .scroll((app.stdout_scroll.offset_u16(), 0)),
        tails[0],
    );
    frame.render_widget(
        Paragraph::new(if app.stderr.is_empty() { "<no stderr yet>" } else { app.stderr.as_ref() })
            .block(pane_block("stderr tail (bounded)", app.run_pane == RunPane::Stderr))
            .wrap(Wrap { trim: false })
            .scroll((app.stderr_scroll.offset_u16(), 0)),
        tails[1],
    );

    let help = if app.view == View::Running {
        "Tab/h/l focus  j/k or PgUp/PgDn scroll  c cancel  q cancel and quit"
    } else {
        "Tab/h/l focus  j/k or PgUp/PgDn scroll  r workflows  e evidence  q quit"
    };
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
                Paragraph::new("j/k select  Enter/i inspect  f forget (confirm)  b back")
                    .block(block("Controls")),
                areas[2],
            );
        }
        EvidenceMode::Inspect(contents) => {
            app.evidence_inspect_scroll.update_layout(
                wrapped_rows(contents, areas[1].width.saturating_sub(2)),
                usize::from(areas[1].height.saturating_sub(2)),
            );
            frame.render_widget(
                Paragraph::new(contents.as_str())
                    .block(block("Validated evidence record"))
                    .wrap(Wrap { trim: false })
                    .scroll((app.evidence_inspect_scroll.offset_u16(), 0)),
                areas[1],
            );
            frame.render_widget(
                Paragraph::new("j/k or PgUp/PgDn scroll  b or Esc back").block(block("Controls")),
                areas[2],
            );
        }
        EvidenceMode::ConfirmForget(run_id) => {
            frame.render_widget(Paragraph::new(format!("Forget Build evidence {run_id}? This removes only this exact retained record.\n\ny confirm  n cancel")).block(block("Confirm deliberate removal")), areas[1]);
            frame.render_widget(
                Paragraph::new("y forget  n cancel").block(block("Controls")),
                areas[2],
            );
        }
        EvidenceMode::ForgetReady(_) => {}
    }
}
