use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use notify::Watcher;
use ratatui::{
    layout::{Constraint, Direction as LayoutDirection, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::framework::process::ProcessSupervisor;
use crate::tui::{
    markdown::MarkdownRenderer, render_split_divider, theme::NORD, EventReader, LineEditor,
    NavigationMap, NavigationRegion, Session, SessionOptions, SplitDividerStyle, SplitFrame,
    SplitMinimums, SplitRatio,
};

use super::{
    model::{
        CodexItemKind, DebatePolicy, ReasoningEffort, RunStatus, Stage, SwarmId, SwarmProjection,
    },
    runner::SwarmLauncher,
    store::{DiscoveredRun, JournalTail, NewSwarmSpec, SwarmStore},
    tree::{self, NodeId, RunSnapshot, SnapshotStatus, TreeRow},
};

const RECONCILE_INTERVAL: Duration = Duration::from_millis(250);
const NARROW_WIDTH: u16 = 72;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Region {
    Tree,
    Detail,
}

#[derive(Clone, Debug)]
enum Mode {
    Browse,
    NewRun,
    ConfirmCancel(SwarmId),
    ConfirmDelete(SwarmId),
}

struct App {
    store: SwarmStore,
    processes: ProcessSupervisor,
    tails: HashMap<SwarmId, JournalTail>,
    runs: Vec<RunSnapshot>,
    rows: Vec<TreeRow>,
    collapsed: HashSet<NodeId>,
    selected: Option<NodeId>,
    region: Region,
    narrow_detail: bool,
    detail_scroll: u16,
    mode: Mode,
    input: LineEditor,
    message: Option<String>,
    working_directory: PathBuf,
}

struct UiRegions {
    navigation: NavigationMap<Region>,
}

pub async fn run(store: SwarmStore, processes: ProcessSupervisor) -> Result<()> {
    let working_directory = std::env::current_dir().context("resolve current working directory")?;
    let mut app = App {
        store,
        processes,
        tails: HashMap::new(),
        runs: Vec::new(),
        rows: Vec::new(),
        collapsed: HashSet::new(),
        selected: None,
        region: Region::Tree,
        narrow_detail: false,
        detail_scroll: 0,
        mode: Mode::Browse,
        input: LineEditor::default(),
        message: None,
        working_directory,
    };
    app.reconcile().await;
    let mut session =
        Session::open(SessionOptions { mouse_capture: false, bracketed_paste: false })?;
    let mut events = EventReader::start();
    let mut interval = tokio::time::interval(RECONCILE_INTERVAL);
    let (hint_sender, mut hints) = tokio::sync::mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok() {
            let _ = hint_sender.send(());
        }
    })?;
    watcher.watch(app.store.root(), notify::RecursiveMode::Recursive)?;
    let mut regions = UiRegions { navigation: NavigationMap::new([]) };

    loop {
        session.draw(|frame| regions = render(frame, &app))?;
        tokio::select! {
            _ = interval.tick() => app.reconcile().await,
            hint = hints.recv() => {
                if hint.is_some() {
                    app.reconcile().await;
                }
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                if app.on_event(event, &regions).await? {
                    break;
                }
            }
        }
    }
    Ok(())
}

impl App {
    async fn reconcile(&mut self) {
        let selected_run = self.selected.as_ref().map(NodeId::run_id).cloned();
        let discovered = match self.store.discover() {
            Ok(discovered) => discovered,
            Err(error) => {
                self.message = Some(error.to_string());
                return;
            }
        };
        let known = discovered.iter().map(|run| run.id().clone()).collect::<HashSet<_>>();
        self.tails.retain(|id, _| known.contains(id));
        let mut runs = Vec::with_capacity(discovered.len());
        for run in discovered {
            runs.push(match run {
                DiscoveredRun::Valid(spec) => {
                    self.reconcile_run(spec.id, selected_run.as_ref()).await
                }
                DiscoveredRun::Corrupt { id, error } => corrupt_snapshot(id, error),
            });
        }
        self.runs = runs;
        self.reproject();
    }

    async fn reconcile_run(&mut self, id: SwarmId, selected_run: Option<&SwarmId>) -> RunSnapshot {
        let load_detail = selected_run == Some(&id);
        match self.store.valid_result(&id) {
            Ok(Some(result)) if !load_detail => {
                self.tails.remove(&id);
                return RunSnapshot {
                    id,
                    status: SnapshotStatus::Run(result.status),
                    projection: None,
                    error: None,
                };
            }
            Ok(Some(_)) => {
                self.tails.remove(&id);
                return self.snapshot_from_replay(id).await;
            }
            Ok(None) => {}
            Err(error) => return corrupt_snapshot(id, error.to_string()),
        }

        if let Some(tail) = self.tails.get_mut(&id) {
            if let Err(error) = tail.refresh() {
                return corrupt_snapshot(id, error.to_string());
            }
            let mut projection = tail.projection().clone();
            let _ = tail;
            if let Ok(inspected) = self.store.inspect(&self.processes, &id).await {
                projection = inspected;
            }
            return RunSnapshot {
                id,
                status: SnapshotStatus::Run(projection.status),
                projection: Some(projection),
                error: None,
            };
        }
        match self.store.tail(&id) {
            Ok(tail) => {
                let mut projection = tail.projection().clone();
                if let Ok(inspected) = self.store.inspect(&self.processes, &id).await {
                    projection = inspected;
                }
                self.tails.insert(id.clone(), tail);
                RunSnapshot {
                    id,
                    status: SnapshotStatus::Run(projection.status),
                    projection: Some(projection),
                    error: None,
                }
            }
            Err(error) => corrupt_snapshot(id, error.to_string()),
        }
    }

    async fn snapshot_from_replay(&self, id: SwarmId) -> RunSnapshot {
        match self.store.inspect(&self.processes, &id).await {
            Ok(projection) => RunSnapshot {
                id,
                status: SnapshotStatus::Run(projection.status),
                projection: Some(projection),
                error: None,
            },
            Err(error) => corrupt_snapshot(id, error.to_string()),
        }
    }

    fn reproject(&mut self) {
        self.rows = tree::project(&self.runs, &self.collapsed);
        self.selected = tree::normalize_selection(&self.rows, self.selected.take());
    }

    fn load_selected_run(&mut self) {
        let Some(id) = self.selected.as_ref().map(NodeId::run_id).cloned() else {
            return;
        };
        let Some(snapshot) = self.runs.iter_mut().find(|run| run.id == id) else {
            return;
        };
        if snapshot.projection.is_none() {
            match self.store.read_journal(&id) {
                Ok(journal) => {
                    let projection = journal.projection;
                    snapshot.status = SnapshotStatus::Run(projection.status);
                    snapshot.projection = Some(projection);
                    snapshot.error = None;
                }
                Err(error) => {
                    snapshot.status = SnapshotStatus::Corrupt;
                    snapshot.error = Some(error.to_string());
                }
            }
            self.reproject();
        }
    }

    async fn on_event(&mut self, event: Event, regions: &UiRegions) -> Result<bool> {
        let Event::Key(key) = event else {
            return Ok(false);
        };
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }
        match self.mode.clone() {
            Mode::NewRun => return self.on_new_run_key(key).await,
            Mode::ConfirmCancel(id) => {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        match self.store.request_cancellation(&id) {
                            Ok(()) => {
                                self.message = Some(format!("Cancellation requested for {id}"))
                            }
                            Err(error) => self.message = Some(error.to_string()),
                        }
                        self.mode = Mode::Browse;
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        self.mode = Mode::Browse;
                    }
                    _ => {}
                }
                return Ok(false);
            }
            Mode::ConfirmDelete(id) => {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        match self.store.inspect(&self.processes, &id).await {
                            Ok(_) => match self.store.delete(&id) {
                                Ok(()) => self.message = Some(format!("Deleted {id}")),
                                Err(error) => self.message = Some(error.to_string()),
                            },
                            Err(error) => self.message = Some(error.to_string()),
                        }
                        self.mode = Mode::Browse;
                        self.reconcile().await;
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        self.mode = Mode::Browse;
                    }
                    _ => {}
                }
                return Ok(false);
            }
            Mode::Browse => {}
        }

        match key.code {
            KeyCode::Char('q') if key.modifiers.is_empty() => return Ok(true),
            KeyCode::Tab => {
                self.region = regions.navigation.next(self.region).unwrap_or(self.region);
                self.narrow_detail = self.region == Region::Detail;
            }
            KeyCode::Up if self.region == Region::Tree => self.move_selection(-1),
            KeyCode::Down if self.region == Region::Tree => self.move_selection(1),
            KeyCode::Left if self.region == Region::Tree => self.collapse_or_parent(),
            KeyCode::Right if self.region == Region::Tree => self.expand_or_child(),
            KeyCode::PageUp if self.region == Region::Detail => {
                self.detail_scroll = self.detail_scroll.saturating_sub(10);
            }
            KeyCode::PageDown if self.region == Region::Detail => {
                self.detail_scroll = self.detail_scroll.saturating_add(10);
            }
            KeyCode::Char('n') if key.modifiers.is_empty() => {
                self.input.clear();
                self.mode = Mode::NewRun;
            }
            KeyCode::Char('c') if key.modifiers.is_empty() => {
                if let Some(id) = self.selected.as_ref().map(NodeId::run_id).cloned() {
                    match self.store.inspect(&self.processes, &id).await {
                        Ok(projection)
                            if matches!(
                                projection.status,
                                RunStatus::Queued | RunStatus::Running
                            ) =>
                        {
                            self.mode = Mode::ConfirmCancel(id);
                        }
                        Ok(projection) => {
                            self.message = Some(format!(
                                "Cannot cancel {id} while it is {}",
                                snapshot_name(&SnapshotStatus::Run(projection.status))
                            ));
                        }
                        Err(error) => self.message = Some(error.to_string()),
                    }
                }
            }
            KeyCode::Char('d') if key.modifiers.is_empty() => {
                if let Some(id) = self.selected.as_ref().map(NodeId::run_id).cloned() {
                    self.mode = Mode::ConfirmDelete(id);
                }
            }
            _ => {}
        }
        Ok(false)
    }

    async fn on_new_run_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Browse,
            KeyCode::Enter => {
                let prompt = self.input.value().trim().to_owned();
                if prompt.is_empty() {
                    self.message = Some("Prompt must not be empty".to_owned());
                    return Ok(false);
                }
                let spec = self.store.create(NewSwarmSpec {
                    prompt,
                    working_directory: self.working_directory.clone(),
                    model: None,
                    reasoning: ReasoningEffort::High,
                    debate: DebatePolicy::Enabled,
                    retry_limit: 2,
                })?;
                match SwarmLauncher::installed(self.store.clone(), self.processes.clone())?
                    .launch(&spec.id)
                    .await
                {
                    Ok(_) => {
                        self.selected = Some(NodeId::Run(spec.id.clone()));
                        self.message = Some(format!("Started {}", spec.id));
                    }
                    Err(error) => self.message = Some(error.to_string()),
                }
                self.mode = Mode::Browse;
                self.input.clear();
                self.reconcile().await;
            }
            _ => self.input.apply_key(key),
        }
        Ok(false)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|selected| self.rows.iter().position(|row| &row.id == selected))
            .unwrap_or(0);
        let next = current.saturating_add_signed(delta).min(self.rows.len() - 1);
        self.selected = Some(self.rows[next].id.clone());
        self.detail_scroll = 0;
        self.load_selected_run();
    }

    fn collapse_or_parent(&mut self) {
        let Some(selected) = self.selected.clone() else {
            return;
        };
        let can_collapse =
            self.rows.iter().find(|row| row.id == selected).is_some_and(|row| row.has_children)
                && !self.collapsed.contains(&selected);
        if can_collapse {
            self.collapsed.insert(selected);
            self.reproject();
        } else if let Some(parent) = tree::parent(&selected) {
            self.selected = Some(parent);
        }
    }

    fn expand_or_child(&mut self) {
        let Some(selected) = self.selected.clone() else {
            return;
        };
        self.load_selected_run();
        if self.collapsed.remove(&selected) {
            self.reproject();
            return;
        }
        if let Some(child) = tree::first_child(&self.rows, &selected) {
            self.selected = Some(child);
        }
    }

    fn selected_snapshot(&self) -> Option<&RunSnapshot> {
        let id = self.selected.as_ref()?.run_id();
        self.runs.iter().find(|run| &run.id == id)
    }
}

fn corrupt_snapshot(id: SwarmId, error: String) -> RunSnapshot {
    RunSnapshot { id, status: SnapshotStatus::Corrupt, projection: None, error: Some(error) }
}

fn render(frame: &mut Frame<'_>, app: &App) -> UiRegions {
    let area = frame.area();
    let layout = Layout::default()
        .direction(LayoutDirection::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(2)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "kit swarm",
                Style::default().fg(NORD.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  deterministic Codex council", Style::default().fg(NORD.text_muted)),
        ])),
        layout[0],
    );

    let narrow = area.width < NARROW_WIDTH;
    let (tree_area, detail_area, split) = if narrow {
        if app.narrow_detail {
            (Rect::default(), layout[1], None)
        } else {
            (layout[1], Rect::default(), None)
        }
    } else {
        let split =
            SplitFrame::horizontal(layout[1], SplitRatio::new(360), SplitMinimums::new(24, 32));
        (split.first, split.second, Some(split))
    };
    if tree_area.width > 0 {
        render_tree(frame, app, tree_area);
    }
    if detail_area.width > 0 {
        render_detail(frame, app, detail_area);
    }
    if let Some(split) = split {
        render_split_divider(
            frame,
            split,
            false,
            SplitDividerStyle {
                idle_color: NORD.border,
                active_color: NORD.accent,
                idle_line: "│",
                idle_grip: "┊",
                active_line: "┃",
            },
        );
    }
    frame.render_widget(footer(app), layout[2]);
    UiRegions {
        navigation: NavigationMap::new([
            NavigationRegion::new(Region::Tree, tree_area),
            NavigationRegion::new(Region::Detail, detail_area),
        ]),
    }
}

fn render_tree(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let items = app
        .rows
        .iter()
        .map(|row| {
            let indent = "  ".repeat(row.depth as usize);
            let branch = if row.has_children {
                if app.collapsed.contains(&row.id) {
                    "▸ "
                } else {
                    "▾ "
                }
            } else {
                "  "
            };
            let line = Line::from(vec![
                Span::raw(indent),
                Span::styled(status_glyph(&row.status), status_style(&row.status)),
                Span::raw(" "),
                Span::styled(branch, Style::default().fg(NORD.text_muted)),
                Span::raw(row.label.clone()),
            ]);
            ListItem::new(line)
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    state.select(
        app.selected
            .as_ref()
            .and_then(|selected| app.rows.iter().position(|row| &row.id == selected)),
    );
    let title = if app.region == Region::Tree { " Runs • " } else { " Runs " };
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().title(title).borders(Borders::ALL).border_style(
                Style::default().fg(if app.region == Region::Tree {
                    NORD.accent
                } else {
                    NORD.border
                }),
            ))
            .highlight_style(Style::default().bg(NORD.selection).fg(NORD.text_strong)),
        area,
        &mut state,
    );
}

fn render_detail(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let source = detail_markdown(app);
    let width = area.width.saturating_sub(2);
    let text = MarkdownRenderer::new(NORD).render(&source, width);
    let title = if app.region == Region::Detail { " Detail • " } else { " Detail " };
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().title(title).borders(Borders::ALL).border_style(
                Style::default().fg(if app.region == Region::Detail {
                    NORD.accent
                } else {
                    NORD.border
                }),
            ))
            .wrap(Wrap { trim: false })
            .scroll((app.detail_scroll, 0)),
        area,
    );
}

fn detail_markdown(app: &App) -> String {
    let Some(snapshot) = app.selected_snapshot() else {
        return "# No swarms\n\nPress `n` to start one.".to_owned();
    };
    if let Some(error) = snapshot.error.as_ref() {
        return format!("# {}\n\n**Corrupt:** {}", snapshot.id, error);
    }
    let Some(projection) = snapshot.projection.as_ref() else {
        return format!(
            "# {}\n\nStatus: `{}`\n\nPress `Right` to load details.",
            snapshot.id,
            snapshot_name(&snapshot.status)
        );
    };
    let selected = app.selected.as_ref().expect("snapshot implies selection");
    match selected {
        NodeId::Run(_) => run_markdown(projection),
        NodeId::Stage { stage, .. } => stage_markdown(projection, *stage),
        NodeId::Agent { stage, agent, .. } => agent_markdown(projection, *stage, agent),
    }
}

fn run_markdown(projection: &SwarmProjection) -> String {
    let mut source = format!(
        "# {}\n\nStatus: `{:?}`  \nSequence: `{}`  \nCreated: `{}` ms  \nLast activity: `{}`  \nWorking directory: `{}`\n\n## Prompt\n\n{}",
        projection.spec.id,
        projection.status,
        projection.last_sequence,
        projection.spec.created_at_ms,
        projection
            .last_event_at_ms
            .map(|value| format!("{value} ms"))
            .unwrap_or_else(|| "not started".to_owned()),
        projection.spec.working_directory.display(),
        projection.spec.prompt
    );
    if let Some(result) = projection.result.as_ref() {
        source.push_str("\n\n## Final answer\n\n");
        source.push_str(&result.answer);
        source.push_str("\n\n## Confidence\n\n");
        source.push_str(&result.confidence);
    }
    if let Some(failure) = projection.failure.as_ref() {
        source.push_str("\n\n## Failure\n\n");
        source.push_str(failure);
    }
    source
}

fn stage_markdown(projection: &SwarmProjection, stage: Stage) -> String {
    let mut source = format!("# {:?}\n\nRun: `{}`\n\n## Agents\n", stage, projection.spec.id);
    let timings = projection
        .nodes
        .iter()
        .flat_map(|node| node.timings.iter())
        .filter(|timing| timing.stage == stage)
        .collect::<Vec<_>>();
    if let (Some(started), Some(last)) = (
        timings.iter().map(|timing| timing.started_at_ms).min(),
        timings.iter().map(|timing| timing.last_event_at_ms).max(),
    ) {
        source.push_str(&format!(
            "\nStarted: `{started}` ms  \nLast activity: `{last}` ms  \nElapsed: `{}` ms\n",
            last.saturating_sub(started)
        ));
    }
    for node in projection
        .nodes
        .iter()
        .filter(|node| node.prompts.iter().any(|prompt| prompt.stage == stage))
    {
        source.push_str(&format!("\n- **{}** — `{:?}`", node.agent, node.status));
    }
    source
}

fn agent_markdown(
    projection: &SwarmProjection,
    stage: Stage,
    agent: &super::model::AgentId,
) -> String {
    let Some(node) = projection.nodes.iter().find(|node| &node.agent == agent) else {
        return format!("# {agent}\n\nAgent is not present in replay.");
    };
    let mut source = format!(
        "# {}\n\nStage: `{:?}`  \nStatus: `{:?}`  \nAttempts: `{}`  \nTokens: `{}` input / `{}` cached / `{}` output / `{}` reasoning\n",
        node.role.as_ref().map(|role| role.title.as_str()).unwrap_or_else(|| agent.as_str()),
        stage,
        node.status,
        node.attempt,
        node.usage.input_tokens,
        node.usage.cached_input_tokens,
        node.usage.output_tokens,
        node.usage.reasoning_output_tokens
    );
    let threads = node
        .threads
        .iter()
        .filter(|thread| thread.stage == stage)
        .map(|thread| format!("`{}`", thread.thread_id))
        .collect::<Vec<_>>();
    if !threads.is_empty() {
        source.push_str(&format!("\nThreads: {}\n", threads.join(", ")));
    }
    if let Some(timing) = node.timings.iter().find(|timing| timing.stage == stage) {
        source.push_str(&format!(
            "\nStarted: `{}` ms  \nLast activity: `{}` ms  \nElapsed: `{}` ms\n",
            timing.started_at_ms,
            timing.last_event_at_ms,
            timing.last_event_at_ms.saturating_sub(timing.started_at_ms)
        ));
    }
    source.push_str("\n## Stream\n");
    for item in node.items.iter().filter(|item| item.stage == stage) {
        source.push_str(&format!("\n- `{:?}` {}", item.lifecycle, item_text(&item.item.kind)));
    }
    if let Some(error) = node.error.as_ref() {
        source.push_str("\n\n## Error\n\n");
        source.push_str(error);
    }
    source
}

fn item_text(item: &CodexItemKind) -> String {
    match item {
        CodexItemKind::AgentMessage { text }
        | CodexItemKind::Reasoning { text }
        | CodexItemKind::Error { message: text } => text.clone(),
        CodexItemKind::CommandExecution { command, output, exit_code, status } => {
            format!("command `{:?}` exit {:?}: {}\n{}", status, exit_code, command, output)
        }
        CodexItemKind::FileChange { changes, status } => format!(
            "file change `{:?}`: {}",
            status,
            changes
                .iter()
                .map(|change| format!("{:?} {}", change.kind, change.path))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        CodexItemKind::McpToolCall { server, tool, arguments, result, error, status } => format!(
            "MCP `{}.{}` `{:?}` args={} result={} error={}",
            server,
            tool,
            status,
            arguments,
            result.as_deref().unwrap_or("none"),
            error.as_deref().unwrap_or("none")
        ),
        CodexItemKind::WebSearch { query } => format!("web search: {query}"),
        CodexItemKind::TodoList { items } => items
            .iter()
            .map(|item| format!("[{}] {}", if item.completed { "x" } else { " " }, item.text))
            .collect::<Vec<_>>()
            .join("; "),
    }
}

fn footer(app: &App) -> Paragraph<'static> {
    let text = match &app.mode {
        Mode::Browse => app.message.clone().unwrap_or_else(|| {
            "↑↓ select  ←→ tree  Tab panel  PgUp/PgDn scroll  n new  c cancel  d delete  q quit"
                .to_owned()
        }),
        Mode::NewRun => format!("New prompt: {}_  (Enter start, Esc cancel)", app.input.value()),
        Mode::ConfirmCancel(id) => format!("Cancel {id}? y/N"),
        Mode::ConfirmDelete(id) => format!("Delete {id}? y/N"),
    };
    Paragraph::new(text).style(Style::default().fg(NORD.text_muted)).block(
        Block::default().borders(Borders::TOP).border_style(Style::default().fg(NORD.border)),
    )
}

fn status_glyph(status: &str) -> &'static str {
    match status {
        "running" => "●",
        "waiting" => "◌",
        "succeeded" => "✓",
        "failed" | "corrupt" => "×",
        "cancelling" => "◐",
        "cancelled" => "–",
        "orphaned" => "!",
        "unavailable" => "?",
        _ => "○",
    }
}

fn status_style(status: &str) -> Style {
    let color = match status {
        "running" => NORD.accent,
        "waiting" | "cancelling" => NORD.warning,
        "succeeded" => NORD.success,
        "failed" | "corrupt" => NORD.danger,
        "orphaned" => NORD.attention,
        "unavailable" => NORD.warning,
        _ => NORD.text_muted,
    };
    Style::default().fg(color)
}

fn snapshot_name(status: &SnapshotStatus) -> &'static str {
    match status {
        SnapshotStatus::Run(RunStatus::Queued) => "queued",
        SnapshotStatus::Run(RunStatus::Running) => "running",
        SnapshotStatus::Run(RunStatus::Cancelling) => "cancelling",
        SnapshotStatus::Run(RunStatus::Succeeded) => "succeeded",
        SnapshotStatus::Run(RunStatus::Failed) => "failed",
        SnapshotStatus::Run(RunStatus::Cancelled) => "cancelled",
        SnapshotStatus::Run(RunStatus::Orphaned) => "orphaned",
        SnapshotStatus::Run(RunStatus::Unavailable) => "unavailable",
        SnapshotStatus::Corrupt => "corrupt",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::swarm::model::{SwarmSpec, SWARM_SCHEMA_VERSION};
    use crossterm::event::KeyModifiers;

    fn app(width_fixture: bool) -> App {
        let root = std::env::temp_dir().join(format!("kit-swarm-tui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = SwarmStore::at(root.clone()).unwrap();
        let processes = ProcessSupervisor::for_test(root.join("processes")).unwrap();
        let spec = SwarmSpec {
            schema_version: SWARM_SCHEMA_VERSION,
            id: SwarmId::new("swarm-1").unwrap(),
            prompt: if width_fixture { "long prompt ".repeat(20) } else { "prompt".to_owned() },
            working_directory: std::env::current_dir().unwrap(),
            model: None,
            reasoning: ReasoningEffort::High,
            debate: DebatePolicy::Enabled,
            created_at_ms: 1,
            retry_limit: 1,
        };
        let projection = SwarmProjection::new(spec.clone()).unwrap();
        let runs = vec![RunSnapshot {
            id: spec.id.clone(),
            status: SnapshotStatus::Run(RunStatus::Queued),
            projection: Some(projection),
            error: None,
        }];
        let rows = tree::project(&runs, &HashSet::new());
        App {
            store,
            processes,
            tails: HashMap::new(),
            runs,
            rows: rows.clone(),
            collapsed: HashSet::new(),
            selected: rows.first().map(|row| row.id.clone()),
            region: Region::Tree,
            narrow_detail: false,
            detail_scroll: 0,
            mode: Mode::Browse,
            input: LineEditor::default(),
            message: None,
            working_directory: std::env::current_dir().unwrap(),
        }
    }

    #[test]
    fn wide_and_narrow_tree_detail_render_without_mock_runtime_state() {
        for (width, height) in [(40, 12), (71, 20), (72, 20), (120, 32)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let app = app(true);
            terminal
                .draw(|frame| {
                    render(frame, &app);
                })
                .unwrap();
            let content = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(content.contains("swarm-1"));
            let _ = std::fs::remove_dir_all(app.store.root());
        }
    }

    #[tokio::test]
    async fn keyboard_traversal_and_quit_only_change_viewer_state() {
        let mut app = app(false);
        let navigation = NavigationMap::new([
            NavigationRegion::new(Region::Tree, Rect::new(0, 0, 20, 20)),
            NavigationRegion::new(Region::Detail, Rect::new(20, 0, 20, 20)),
        ]);
        let regions = UiRegions { navigation };
        app.on_event(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)), &regions)
            .await
            .unwrap();
        assert_eq!(app.region, Region::Detail);
        app.region = Region::Tree;
        let run = app.rows[0].id.clone();
        let stage = app.rows[1].id.clone();
        app.selected = Some(stage);
        app.collapse_or_parent();
        assert_eq!(app.selected, Some(run.clone()));
        app.collapse_or_parent();
        assert!(app.collapsed.contains(&run));
        app.expand_or_child();
        assert_eq!(app.selected, Some(run.clone()));
        app.expand_or_child();
        assert_eq!(app.selected, Some(app.rows[1].id.clone()));
        assert!(app
            .on_event(Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)), &regions)
            .await
            .unwrap());
        let _ = std::fs::remove_dir_all(app.store.root());
    }

    #[tokio::test]
    async fn two_viewers_tail_one_live_journal_without_owning_its_lifecycle() {
        use crate::tools::swarm::model::{AgentId, Stage, SwarmEvent, SwarmOwner};

        let root = std::env::temp_dir().join(format!(
            "kit-swarm-tui-viewers-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let store = SwarmStore::at(root.clone()).unwrap();
        let spec = store
            .create(NewSwarmSpec {
                prompt: "viewer lifecycle".to_owned(),
                working_directory: std::env::current_dir().unwrap(),
                model: None,
                reasoning: ReasoningEffort::Low,
                debate: DebatePolicy::Disabled,
                retry_limit: 0,
            })
            .unwrap();
        let journal = store.start_journal(&spec.id).unwrap();
        journal.append(SwarmEvent::RunStarted { owner: SwarmOwner::fixture() }).await.unwrap();
        journal.append(SwarmEvent::StageStarted { stage: Stage::Planning }).await.unwrap();

        let mut first = app(false);
        let first_fixture_root = first.store.root().to_path_buf();
        first.store = store.clone();
        first.runs.clear();
        first.tails.clear();
        first.selected = None;
        let mut second = app(false);
        let second_fixture_root = second.store.root().to_path_buf();
        second.store = store.clone();
        second.runs.clear();
        second.tails.clear();
        second.selected = None;
        first.reconcile().await;
        second.reconcile().await;
        assert_eq!(first.runs[0].projection.as_ref().unwrap().last_sequence, 2);
        assert_eq!(second.runs[0].projection.as_ref().unwrap().last_sequence, 2);

        let planner = AgentId::new("planner").unwrap();
        journal
            .append(SwarmEvent::AgentPrompted {
                agent: planner.clone(),
                stage: Stage::Planning,
                prompt: "plan".to_owned(),
            })
            .await
            .unwrap();
        journal
            .append(SwarmEvent::AgentStarted { agent: planner, stage: Stage::Planning, attempt: 1 })
            .await
            .unwrap();
        first.reconcile().await;
        second.reconcile().await;
        assert_eq!(first.runs[0].projection.as_ref().unwrap().last_sequence, 4);
        assert_eq!(second.runs[0].projection.as_ref().unwrap().last_sequence, 4);

        drop(first);
        journal
            .append(SwarmEvent::RunFailed { error: "fixture complete".to_owned() })
            .await
            .unwrap();
        second.reconcile().await;
        assert_eq!(second.runs[0].projection.as_ref().unwrap().status, RunStatus::Failed);
        journal.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(first_fixture_root);
        let _ = std::fs::remove_dir_all(second_fixture_root);
    }
}
