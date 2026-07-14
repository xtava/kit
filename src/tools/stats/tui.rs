use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Flex, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use super::model::{DetailScope, ProcessKey, ProcessSample, StatsSnapshot};
use super::report;
use super::sampler::{Sampler, SamplerWorker};
use super::signal::{self, ProcessSignal};
use crate::tui::{theme::NORD, EventReader, LineEditor, Session, SessionOptions};

const BACKGROUND: Color = NORD.background;
const PANEL: Color = NORD.surface;
const BORDER: Color = NORD.border;
const TEXT: Color = NORD.text;
const PAPER: Color = NORD.text_strong;
const MUTED: Color = NORD.text_muted;
const ACCENT: Color = NORD.accent;
const CPU_ACCENT: Color = NORD.accent_alt;
const HIGHLIGHT: Color = NORD.focus;
const GOOD: Color = NORD.info;
const WARN: Color = NORD.warning;
const SELECTED: Color = NORD.selection;
const HISTORY: usize = 120;

pub async fn run(interval: Duration, mouse_capture: bool) -> Result<()> {
    let sampler = Sampler::new(interval)?;
    let (worker, mut snapshots) = SamplerWorker::start(sampler)?;
    let initial = Arc::clone(&snapshots.borrow());
    let mut app = StatsApp::new(initial);
    let mut session = Session::open(SessionOptions { mouse_capture })?;
    let mut events = EventReader::start();
    worker.set_detail_scope(app.detail_scope());

    loop {
        session.draw(|frame| render(frame, &mut app))?;
        tokio::select! {
            changed = snapshots.changed() => {
                if changed.is_err() {
                    break;
                }
                app.ingest(Arc::clone(&snapshots.borrow()));
            }
            event = events.recv() => {
                let Some(event) = event else { break };
                let previous_scope = app.detail_scope();
                match app.on_event(event) {
                    Action::Quit => break,
                    Action::Signal(key, requested) => {
                        app.status = Some(match signal::send(key, requested) {
                            Ok(()) => {
                                worker.refresh();
                                format!("Sent {} to PID {}", requested.label(), key.pid)
                            }
                            Err(error) => error.to_string(),
                        });
                    }
                    Action::None => {}
                }
                let next_scope = app.detail_scope();
                if previous_scope != next_scope {
                    worker.set_detail_scope(next_scope);
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SortBy {
    Cpu,
    Memory,
    Pid,
    Name,
}

impl SortBy {
    fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Memory => "RAM",
            Self::Pid => "PID",
            Self::Name => "NAME",
        }
    }
}

#[derive(Clone, Copy)]
enum ProcessColumnKind {
    Program,
    Command,
    Pid,
    Cpu,
    Core,
    Memory,
}

impl ProcessColumnKind {
    fn title(self) -> &'static str {
        match self {
            Self::Program => "PROGRAM",
            Self::Command => "COMMAND",
            Self::Pid => "PID",
            Self::Cpu => "CPU",
            Self::Core => "CORE",
            Self::Memory => "MEMORY",
        }
    }

    fn constraint(self, show_command: bool) -> Constraint {
        match self {
            Self::Program if show_command => Constraint::Length(22),
            Self::Program | Self::Command => Constraint::Min(18),
            Self::Pid => Constraint::Length(8),
            Self::Cpu => Constraint::Length(9),
            Self::Core => Constraint::Length(7),
            Self::Memory => Constraint::Length(9),
        }
    }

    fn width(self, inner_width: u16, show_command: bool) -> u16 {
        match self {
            Self::Program if show_command => 22,
            Self::Program => inner_width.saturating_sub(33),
            Self::Command => inner_width.saturating_sub(55),
            Self::Pid => 8,
            Self::Cpu => 9,
            Self::Core => 7,
            Self::Memory => 9,
        }
    }

    fn sort(self) -> Option<SortBy> {
        match self {
            Self::Program => Some(SortBy::Name),
            Self::Pid => Some(SortBy::Pid),
            Self::Cpu => Some(SortBy::Cpu),
            Self::Memory => Some(SortBy::Memory),
            Self::Command | Self::Core => None,
        }
    }
}

const WIDE_PROCESS_COLUMNS: &[ProcessColumnKind] = &[
    ProcessColumnKind::Program,
    ProcessColumnKind::Command,
    ProcessColumnKind::Pid,
    ProcessColumnKind::Cpu,
    ProcessColumnKind::Core,
    ProcessColumnKind::Memory,
];
const COMPACT_PROCESS_COLUMNS: &[ProcessColumnKind] = &[
    ProcessColumnKind::Program,
    ProcessColumnKind::Pid,
    ProcessColumnKind::Cpu,
    ProcessColumnKind::Core,
    ProcessColumnKind::Memory,
];

enum Action {
    None,
    Quit,
    Signal(ProcessKey, ProcessSignal),
}

enum RowKind {
    Process(ProcessKey),
    Thread { process: ProcessKey },
}

struct VisibleRow {
    kind: RowKind,
    name: String,
    command: Option<String>,
    pid: u32,
    cpu: f32,
    memory: u64,
    core: Option<u16>,
    depth: u16,
}

#[derive(Default)]
struct UiRegions {
    cores: Vec<(Rect, u16)>,
    rows: Vec<(Rect, usize)>,
    headers: Vec<(Rect, SortBy)>,
    end_process: Option<Rect>,
    confirm_yes: Option<Rect>,
    confirm_force: Option<Rect>,
    confirm_cancel: Option<Rect>,
}

struct Confirmation {
    key: ProcessKey,
    signal: ProcessSignal,
    name: String,
}

struct StatsApp {
    snapshot: Arc<StatsSnapshot>,
    selected: Option<ProcessKey>,
    expanded: HashSet<ProcessKey>,
    focused_core: Option<u16>,
    sort: SortBy,
    descending: bool,
    tree_mode: bool,
    filter: LineEditor,
    filtering: bool,
    visible: Vec<VisibleRow>,
    histories: Vec<VecDeque<u64>>,
    confirm: Option<Confirmation>,
    status: Option<String>,
    regions: UiRegions,
}

impl StatsApp {
    fn new(snapshot: Arc<StatsSnapshot>) -> Self {
        let selected = snapshot
            .processes
            .iter()
            .max_by(|left, right| left.cpu_percent.total_cmp(&right.cpu_percent))
            .map(|process| process.key);
        let mut app = Self {
            snapshot,
            selected,
            expanded: HashSet::new(),
            focused_core: None,
            sort: SortBy::Cpu,
            descending: true,
            tree_mode: false,
            filter: LineEditor::default(),
            filtering: false,
            visible: Vec::new(),
            histories: Vec::new(),
            confirm: None,
            status: None,
            regions: UiRegions::default(),
        };
        app.record_history();
        app.rebuild();
        app
    }

    fn ingest(&mut self, snapshot: Arc<StatsSnapshot>) {
        self.snapshot = snapshot;
        self.record_history();
        self.rebuild();
    }

    fn record_history(&mut self) {
        self.histories.resize_with(self.snapshot.system.cpus.len(), VecDeque::new);
        for (history, cpu) in self.histories.iter_mut().zip(&self.snapshot.system.cpus) {
            if history.len() == HISTORY {
                history.pop_front();
            }
            history.push_back(cpu.usage_percent.round() as u64);
        }
    }

    fn detail_scope(&self) -> DetailScope {
        if let Some(core) = self.focused_core {
            DetailScope::Core(core)
        } else if let Some(key) = self.selected.filter(|key| self.expanded.contains(key)) {
            DetailScope::Process(key)
        } else {
            DetailScope::None
        }
    }

    fn rebuild(&mut self) {
        let query = self.filter.value().to_ascii_lowercase();
        let core_cpu = self.core_process_cpu();
        let mut processes = self
            .snapshot
            .processes
            .iter()
            .filter(|process| {
                query.is_empty()
                    || process.name.to_ascii_lowercase().contains(&query)
                    || process.command.to_ascii_lowercase().contains(&query)
                    || process.key.pid.to_string().contains(&query)
                    || process
                        .user
                        .as_deref()
                        .unwrap_or_default()
                        .to_ascii_lowercase()
                        .contains(&query)
            })
            .filter(|process| self.focused_core.is_none() || core_cpu.contains_key(&process.key))
            .collect::<Vec<_>>();
        processes.sort_by(|left, right| {
            let order = match self.sort {
                SortBy::Cpu => {
                    let left = core_cpu.get(&left.key).copied().unwrap_or(left.cpu_percent);
                    let right = core_cpu.get(&right.key).copied().unwrap_or(right.cpu_percent);
                    left.total_cmp(&right)
                }
                SortBy::Memory => left.rss_bytes.cmp(&right.rss_bytes),
                SortBy::Pid => left.key.pid.cmp(&right.key.pid),
                SortBy::Name => {
                    left.name.to_ascii_lowercase().cmp(&right.name.to_ascii_lowercase())
                }
            };
            if self.descending {
                order.reverse()
            } else {
                order
            }
        });

        let processes = if self.tree_mode {
            tree_order(processes)
        } else {
            processes.into_iter().map(|process| (process, 0)).collect()
        };

        self.visible.clear();
        for (process, depth) in processes {
            self.visible.push(VisibleRow {
                kind: RowKind::Process(process.key),
                name: process.name.clone(),
                command: Some(process.command.clone()),
                pid: process.key.pid,
                cpu: core_cpu.get(&process.key).copied().unwrap_or(process.cpu_percent),
                memory: process.rss_bytes,
                core: self.focused_core.or(process.last_cpu),
                depth,
            });
            if self.expanded.contains(&process.key) {
                for thread in
                    self.snapshot.threads.iter().filter(|thread| thread.process == process.key)
                {
                    self.visible.push(VisibleRow {
                        kind: RowKind::Thread { process: process.key },
                        name: thread.name.clone(),
                        command: None,
                        pid: thread.key.tid,
                        cpu: thread.cpu_percent,
                        memory: 0,
                        core: thread.last_cpu,
                        depth: depth + 1,
                    });
                }
            }
        }
        if self.selected.is_none()
            || !self
                .visible
                .iter()
                .any(|row| matches!(row.kind, RowKind::Process(key) if Some(key) == self.selected))
        {
            self.selected = self.visible.iter().find_map(|row| match row.kind {
                RowKind::Process(key) => Some(key),
                RowKind::Thread { .. } => None,
            });
        }
    }

    fn core_process_cpu(&self) -> HashMap<ProcessKey, f32> {
        let mut values = HashMap::new();
        if let Some(core) = self.focused_core {
            for thread in
                self.snapshot.threads.iter().filter(|thread| thread.last_cpu == Some(core))
            {
                *values.entry(thread.process).or_insert(0.0) += thread.cpu_percent;
            }
        }
        values
    }

    fn selected_process(&self) -> Option<&ProcessSample> {
        let key = self.selected?;
        self.snapshot.processes.iter().find(|process| process.key == key)
    }

    fn select_index(&mut self, index: usize) {
        if let Some(row) = self.visible.get(index) {
            self.selected = Some(match row.kind {
                RowKind::Process(key) => key,
                RowKind::Thread { process } => process,
            });
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let current = self
            .selected
            .and_then(|selected| {
                self.visible
                    .iter()
                    .position(|row| matches!(row.kind, RowKind::Process(key) if key == selected))
            })
            .unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.visible.len() as isize - 1) as usize;
        self.select_index(next);
    }

    fn toggle_expand(&mut self) {
        let Some(key) = self.selected else { return };
        if !self.expanded.remove(&key) {
            self.expanded.clear();
            self.expanded.insert(key);
        }
        self.rebuild();
    }

    fn focus_core(&mut self, delta: isize) {
        let count = self.snapshot.system.cpus.len();
        if count == 0 {
            return;
        }
        let current = self.focused_core.map_or(if delta < 0 { 0 } else { count - 1 }, usize::from);
        let next = (current as isize + delta).rem_euclid(count as isize) as u16;
        self.focused_core = Some(next);
        self.rebuild();
    }

    fn set_sort(&mut self, sort: SortBy) {
        if self.sort == sort {
            self.descending = !self.descending;
        } else {
            self.sort = sort;
            self.descending = !matches!(sort, SortBy::Name | SortBy::Pid);
        }
        self.rebuild();
    }

    fn on_event(&mut self, event: Event) -> Action {
        match event {
            Event::Key(key) => self.on_key(key),
            Event::Mouse(mouse) => self.on_mouse(mouse),
            _ => Action::None,
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        if self.confirm.is_some() {
            return self.on_confirmation_key(key);
        }
        if self.filtering {
            match key.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.rebuild();
                }
                _ => {
                    self.filter.apply_key(key);
                    self.rebuild();
                }
            }
            return Action::None;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc if self.focused_core.is_none() => Action::Quit,
            KeyCode::Esc => {
                self.focused_core = None;
                self.rebuild();
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Action::None
            }
            KeyCode::PageUp => {
                self.move_selection(-10);
                Action::None
            }
            KeyCode::PageDown => {
                self.move_selection(10);
                Action::None
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => {
                self.toggle_expand();
                Action::None
            }
            KeyCode::Left => {
                if let Some(key) = self.selected {
                    self.expanded.remove(&key);
                    self.rebuild();
                }
                Action::None
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                Action::None
            }
            KeyCode::Char('c') => {
                self.focused_core = None;
                self.rebuild();
                Action::None
            }
            KeyCode::Char('[') => {
                self.focus_core(-1);
                Action::None
            }
            KeyCode::Char(']') => {
                self.focus_core(1);
                Action::None
            }
            KeyCode::Char('t') => {
                self.tree_mode = !self.tree_mode;
                self.rebuild();
                Action::None
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                self.open_confirmation(ProcessSignal::Terminate);
                Action::None
            }
            KeyCode::Char('X') => {
                self.open_confirmation(ProcessSignal::Kill);
                Action::None
            }
            KeyCode::Char('1') => {
                self.set_sort(SortBy::Cpu);
                Action::None
            }
            KeyCode::Char('2') => {
                self.set_sort(SortBy::Memory);
                Action::None
            }
            KeyCode::Char('3') => {
                self.set_sort(SortBy::Pid);
                Action::None
            }
            KeyCode::Char('4') => {
                self.set_sort(SortBy::Name);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn on_confirmation_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                self.confirm = None;
                Action::None
            }
            KeyCode::Char('f') => {
                if let Some(confirm) = &mut self.confirm {
                    confirm.signal = ProcessSignal::Kill;
                }
                Action::None
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                let confirm = self.confirm.take().expect("confirmation checked above");
                Action::Signal(confirm.key, confirm.signal)
            }
            _ => Action::None,
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) -> Action {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.move_selection(-3);
                return Action::None;
            }
            MouseEventKind::ScrollDown => {
                self.move_selection(3);
                return Action::None;
            }
            MouseEventKind::Down(MouseButton::Left) => {}
            _ => return Action::None,
        }
        let point = (mouse.column, mouse.row);
        if self.confirm.is_some() {
            if self.regions.confirm_yes.is_some_and(|area| contains(area, point)) {
                let confirm = self.confirm.take().expect("confirmation checked above");
                return Action::Signal(confirm.key, confirm.signal);
            }
            if self.regions.confirm_force.is_some_and(|area| contains(area, point)) {
                if let Some(confirm) = &mut self.confirm {
                    confirm.signal = ProcessSignal::Kill;
                }
                return Action::None;
            }
            if self.regions.confirm_cancel.is_some_and(|area| contains(area, point)) {
                self.confirm = None;
            }
            return Action::None;
        }
        if let Some((_, core)) = self.regions.cores.iter().find(|(area, _)| contains(*area, point))
        {
            self.focused_core = if self.focused_core == Some(*core) { None } else { Some(*core) };
            self.rebuild();
            return Action::None;
        }
        if let Some((_, sort)) =
            self.regions.headers.iter().find(|(area, _)| contains(*area, point))
        {
            self.set_sort(*sort);
            return Action::None;
        }
        if let Some((_, index)) = self.regions.rows.iter().find(|(area, _)| contains(*area, point))
        {
            let clicked = self.visible.get(*index).map(|row| match row.kind {
                RowKind::Process(key) => key,
                RowKind::Thread { process } => process,
            });
            let same = clicked == self.selected;
            self.select_index(*index);
            if same {
                self.toggle_expand();
            }
            return Action::None;
        }
        if self.regions.end_process.is_some_and(|area| contains(area, point)) {
            self.open_confirmation(ProcessSignal::Terminate);
        }
        Action::None
    }

    fn open_confirmation(&mut self, requested: ProcessSignal) {
        if let Some(process) = self.selected_process() {
            self.confirm = Some(Confirmation {
                key: process.key,
                signal: requested,
                name: process.name.clone(),
            });
        }
    }
}

fn render(frame: &mut Frame<'_>, app: &mut StatsApp) {
    let area = frame.area();
    app.regions = UiRegions::default();
    frame.render_widget(Block::new().style(Style::default().bg(BACKGROUND)), area);
    if area.width < 48 || area.height < 13 {
        frame.render_widget(
            Paragraph::new("kit stats needs at least 48 columns × 13 rows\n\nq  quit")
                .style(Style::default().fg(TEXT).bg(BACKGROUND))
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(BORDER))
                        .title(Span::styled(" KIT STATS ", Style::default().fg(ACCENT))),
                )
                .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    let cpu_count = app.snapshot.system.cpus.len().max(1);
    let per_row_by_width = ((area.width.saturating_sub(2)) / 11).max(4) as usize;
    let per_row_for_height = cpu_count.div_ceil(4);
    let per_row = per_row_by_width.max(per_row_for_height);
    let core_rows = app.snapshot.system.cpus.len().div_ceil(per_row).max(1) as u16;
    let cpu_height = core_rows + 3;
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(cpu_height),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .split(area);
    render_header(frame, app, chunks[0]);
    render_cores(frame, app, chunks[1], per_row);
    render_body(frame, app, chunks[2]);
    render_footer(frame, app, chunks[3]);
    if app.confirm.is_some() {
        render_confirmation(frame, app);
    }
}

fn render_header(frame: &mut Frame<'_>, app: &StatsApp, area: Rect) {
    let system = &app.snapshot.system;
    let line = Line::from(vec![
        Span::styled(" kit stats ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(" cpu ", Style::default().fg(MUTED)),
        Span::styled(
            format!("{:>5.1}%", system.global_cpu_percent),
            Style::default().fg(cpu_color(system.global_cpu_percent)).add_modifier(Modifier::BOLD),
        ),
        Span::styled("   load ", Style::default().fg(MUTED)),
        Span::styled(
            format!(
                "{:.2}  {:.2}  {:.2}",
                system.load_average[0], system.load_average[1], system.load_average[2]
            ),
            Style::default().fg(TEXT),
        ),
        Span::styled("   mem ", Style::default().fg(MUTED)),
        Span::styled(
            format!(
                "{} / {}",
                report::bytes(system.used_memory_bytes),
                report::bytes(system.total_memory_bytes)
            ),
            Style::default().fg(TEXT),
        ),
        Span::styled("   procs ", Style::default().fg(MUTED)),
        Span::styled(
            system.process_count.to_string(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line)
            .style(Style::default().bg(PANEL))
            .block(Block::new().borders(Borders::BOTTOM).border_style(Style::default().fg(BORDER))),
        area,
    );
}

fn render_cores(frame: &mut Frame<'_>, app: &mut StatsApp, area: Rect, per_row: usize) {
    let system = &app.snapshot.system;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .style(Style::default().fg(TEXT).bg(PANEL))
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(" CPU ", Style::default().fg(CPU_ACCENT).add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let graph_width = inner.width.saturating_sub(13) as usize;
    let total = Line::from(vec![
        Span::styled(" total ", Style::default().fg(MUTED)),
        Span::styled(
            format!("{:>5.1}% ", system.global_cpu_percent),
            Style::default().fg(cpu_color(system.global_cpu_percent)).add_modifier(Modifier::BOLD),
        ),
        Span::styled(global_spark(&app.histories, graph_width), Style::default().fg(CPU_ACCENT)),
    ]);
    frame.render_widget(Paragraph::new(total), Rect::new(inner.x, inner.y, inner.width, 1));

    let cell_width = (inner.width / per_row as u16).max(1);
    for (row_index, cpus) in system.cpus.chunks(per_row).enumerate() {
        let row_area = Rect::new(inner.x, inner.y + row_index as u16 + 1, inner.width, 1);
        let columns = Layout::horizontal(vec![Constraint::Length(cell_width); per_row])
            .flex(Flex::Start)
            .split(row_area);
        for (offset, (cpu, cpu_area)) in cpus.iter().zip(columns.iter()).enumerate() {
            let index = row_index * per_row + offset;
            let selected = app.focused_core == Some(cpu.logical_index);
            let graph = spark(app.histories.get(index)).chars().last().unwrap_or('▁');
            let label = if cpu_area.width >= 11 {
                format!(" C{:02} {:>3.0}% {graph}", cpu.logical_index, cpu.usage_percent)
            } else {
                format!(" {:02} {:>2.0}%", cpu.logical_index, cpu.usage_percent)
            };
            let style = if selected {
                Style::default().fg(PAPER).bg(SELECTED).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(cpu_color(cpu.usage_percent)).bg(PANEL)
            };
            frame.render_widget(Paragraph::new(label).style(style), *cpu_area);
            app.regions.cores.push((*cpu_area, cpu.logical_index));
        }
    }
}

fn render_body(frame: &mut Frame<'_>, app: &mut StatsApp, area: Rect) {
    if area.height >= 14 {
        let rows = Layout::vertical([Constraint::Min(8), Constraint::Length(6)]).split(area);
        render_processes(frame, app, rows[0]);
        render_detail(frame, app, rows[1]);
    } else {
        render_processes(frame, app, area);
    }
}

fn render_processes(frame: &mut Frame<'_>, app: &mut StatsApp, area: Rect) {
    let scope = app.focused_core.map_or_else(
        || "all processes".into(),
        |core| {
            if app.snapshot.detail_scope != DetailScope::Core(core)
                || !app.snapshot.threads_warmed_up
            {
                format!("Core {core} · warming thread deltas…")
            } else {
                format!("threads last seen on Core {core} · approx CPU")
            }
        },
    );
    let arrow = if app.descending { "▼" } else { "▲" };
    let mode = if app.tree_mode { "TREE" } else { "FLAT" };
    let title = Line::from(vec![
        Span::styled(" PROCESSES ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("  {} {arrow}   {mode}   {scope} ", app.sort.label()),
            Style::default().fg(MUTED),
        ),
    ]);
    let inner = area.inner(Margin { horizontal: 1, vertical: 1 });
    let show_command = area.width >= 90;
    let columns = if show_command { WIDE_PROCESS_COLUMNS } else { COMPACT_PROCESS_COLUMNS };
    let rows = app.visible.iter().map(|item| {
        let selected = matches!(item.kind, RowKind::Process(key) if Some(key) == app.selected);
        let (marker, style) = match item.kind {
            RowKind::Process(key) => {
                let marker = if app.expanded.contains(&key) { "▾" } else { "▸" };
                let style = if selected {
                    Style::default().fg(PAPER).bg(SELECTED).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(TEXT).bg(PANEL)
                };
                (marker, style)
            }
            RowKind::Thread { .. } => (" └", Style::default().fg(MUTED).bg(PANEL)),
        };
        let core = item.core.map(|core| format!("C{core}")).unwrap_or_else(|| "—".into());
        let program = format!("{}{marker}  {}", "  ".repeat(item.depth as usize), item.name);
        let cells = columns.iter().map(|column| match column {
            ProcessColumnKind::Program => Cell::from(program.clone()),
            ProcessColumnKind::Command => Cell::from(item.command.as_deref().unwrap_or("thread"))
                .style(Style::default().fg(if selected { TEXT } else { MUTED })),
            ProcessColumnKind::Pid => Cell::from(item.pid.to_string())
                .style(Style::default().fg(if selected { TEXT } else { MUTED })),
            ProcessColumnKind::Cpu => Cell::from(format!("{:.1}%", item.cpu))
                .style(Style::default().fg(if selected { PAPER } else { cpu_color(item.cpu) })),
            ProcessColumnKind::Core => Cell::from(core.clone())
                .style(Style::default().fg(if selected { TEXT } else { MUTED })),
            ProcessColumnKind::Memory => {
                Cell::from(if item.memory == 0 { "—".into() } else { report::bytes(item.memory) })
            }
        });
        Row::new(cells).style(style)
    });
    let header = Row::new(columns.iter().map(|column| column.title()))
        .style(Style::default().fg(MUTED).bg(PANEL).add_modifier(Modifier::BOLD));
    let table = Table::new(rows, columns.iter().map(|column| column.constraint(show_command)))
        .header(header)
        .style(Style::default().fg(TEXT).bg(PANEL))
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .style(Style::default().bg(PANEL))
                .border_style(Style::default().fg(BORDER))
                .title(title),
        );
    frame.render_widget(table, area);

    if inner.height < 3 {
        return;
    }
    let mut x = inner.x;
    for column in columns {
        let width = column.width(inner.width, show_command);
        if let Some(sort) = column.sort() {
            app.regions.headers.push((Rect::new(x, inner.y, width, 1), sort));
        }
        x = x.saturating_add(width);
    }
    let row_top = inner.y + 1;
    let visible_height = inner.height.saturating_sub(1) as usize;
    for index in 0..visible_height.min(app.visible.len()) {
        let y = row_top + index as u16;
        app.regions.rows.push((Rect::new(inner.x, y, inner.width, 1), index));
    }
}

fn render_detail(frame: &mut Frame<'_>, app: &mut StatsApp, area: Rect) {
    let Some(process) = app.selected_process() else {
        frame.render_widget(
            Paragraph::new("No process selected")
                .style(Style::default().fg(MUTED).bg(PANEL))
                .block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(BORDER)),
                ),
            area,
        );
        return;
    };
    let identity = if process.identity_verified { "verified" } else { "unverified" };
    let core =
        process.last_cpu.map(|core| format!("Core {core}")).unwrap_or_else(|| "unknown".into());
    let user = process.user.as_deref().unwrap_or("unknown");
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{:.1}% CPU", process.cpu_percent),
                Style::default().fg(cpu_color(process.cpu_percent)).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("   {} MEM", report::bytes(process.rss_bytes)),
                Style::default().fg(TEXT),
            ),
            Span::styled(format!("   {core}"), Style::default().fg(MUTED)),
        ]),
        Line::from(format!(
            "PID {}   PPID {}   {}   {user}   UP {}   {identity}",
            process.key.pid,
            process.parent_pid.unwrap_or(0),
            process.status,
            report::duration(process.run_time_seconds)
        )),
        Line::styled(&process.command, Style::default().fg(MUTED)),
    ];
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .style(Style::default().fg(TEXT).bg(PANEL))
        .border_style(Style::default().fg(BORDER))
        .title(Line::from(vec![
            Span::styled(" INSPECTOR ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {} ", process.name),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ]));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    if inner.height >= 3 {
        let button = Rect::new(inner.x, inner.bottom() - 1, 17.min(inner.width), 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" x ", Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD)),
                Span::styled("end process", Style::default().fg(MUTED)),
            ])),
            button,
        );
        app.regions.end_process = Some(button);
    }
}

fn render_footer(frame: &mut Frame<'_>, app: &StatsApp, area: Rect) {
    if app.filtering {
        frame.render_widget(
            Paragraph::new(format!(" /{}▏", app.filter.value()))
                .style(Style::default().fg(TEXT).bg(BACKGROUND)),
            area,
        );
    } else if let Some(status) = &app.status {
        frame.render_widget(
            Paragraph::new(format!(" {status}  │  q quit  / search  [ ] cores  t tree  x end"))
                .style(Style::default().fg(MUTED).bg(BACKGROUND)),
            area,
        );
    } else {
        let key = Style::default().fg(HIGHLIGHT).add_modifier(Modifier::BOLD);
        let help = Line::from(vec![
            Span::styled(" ↑↓ ", key),
            Span::styled("select   ", Style::default().fg(MUTED)),
            Span::styled("enter ", key),
            Span::styled("expand   ", Style::default().fg(MUTED)),
            Span::styled("/ ", key),
            Span::styled("search   ", Style::default().fg(MUTED)),
            Span::styled("[ ] ", key),
            Span::styled("cores   ", Style::default().fg(MUTED)),
            Span::styled("t ", key),
            Span::styled("tree   ", Style::default().fg(MUTED)),
            Span::styled("x ", key),
            Span::styled("end   ", Style::default().fg(MUTED)),
            Span::styled("q ", key),
            Span::styled("quit", Style::default().fg(MUTED)),
        ]);
        frame.render_widget(Paragraph::new(help).style(Style::default().bg(BACKGROUND)), area);
    }
}

fn render_confirmation(frame: &mut Frame<'_>, app: &mut StatsApp) {
    let confirm = app.confirm.as_ref().expect("called only with confirmation");
    let area = centered(frame.area(), 58, 9);
    frame.render_widget(Clear, area);
    frame.render_widget(Block::new().style(Style::default().bg(PANEL)), area);
    let warning = if confirm.signal == ProcessSignal::Kill {
        "Force kill cannot be handled or cleaned up by the process."
    } else {
        "The process may save work and shut down cleanly."
    };
    let text = vec![
        Line::from(format!("End {} (PID {})?", confirm.name, confirm.key.pid)),
        Line::from(""),
        Line::styled(
            confirm.signal.label(),
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ),
        Line::from(warning),
        Line::from(""),
        Line::styled("ENTER confirm   F force kill   ESC cancel", Style::default().fg(ACCENT)),
    ];
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(TEXT).bg(PANEL))
            .wrap(Wrap { trim: true })
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .style(Style::default().bg(PANEL))
                    .border_style(Style::default().fg(WARN))
                    .title(Span::styled(
                        " CONFIRM ACTION ",
                        Style::default().fg(WARN).add_modifier(Modifier::BOLD),
                    )),
            ),
        area,
    );
    let inner = area.inner(Margin { horizontal: 1, vertical: 1 });
    let buttons = Layout::horizontal([
        Constraint::Length(16),
        Constraint::Length(17),
        Constraint::Length(13),
        Constraint::Min(0),
    ])
    .split(Rect::new(inner.x, inner.bottom() - 1, inner.width, 1));
    app.regions.confirm_yes = Some(buttons[0]);
    app.regions.confirm_force = Some(buttons[1]);
    app.regions.confirm_cancel = Some(buttons[2]);
}

fn spark(history: Option<&VecDeque<u64>>) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    history
        .into_iter()
        .flat_map(|history| history.iter().rev().take(6).rev())
        .map(|value| BARS[(*value).min(99) as usize * BARS.len() / 100])
        .collect()
}

fn global_spark(histories: &[VecDeque<u64>], width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let sample_count = histories.iter().map(VecDeque::len).min().unwrap_or(0);
    let first = sample_count.saturating_sub(width);
    let values = (first..sample_count)
        .map(|index| {
            let total = histories.iter().map(|history| history[index]).sum::<u64>();
            let average = total / histories.len().max(1) as u64;
            BARS[average.min(99) as usize * BARS.len() / 100]
        })
        .collect::<String>();
    format!("{}{values}", " ".repeat(width.saturating_sub(values.chars().count())))
}

fn cpu_color(value: f32) -> Color {
    if value >= 85.0 {
        PAPER
    } else if value >= 60.0 {
        CPU_ACCENT
    } else {
        GOOD
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn contains(area: Rect, (x, y): (u16, u16)) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}

fn tree_order(processes: Vec<&ProcessSample>) -> Vec<(&ProcessSample, u16)> {
    let fallback = processes.clone();
    let by_pid =
        processes.iter().map(|process| (process.key.pid, process.key)).collect::<HashMap<_, _>>();
    let mut children = HashMap::<Option<ProcessKey>, Vec<&ProcessSample>>::new();
    for process in processes {
        let parent = process.parent_pid.and_then(|pid| by_pid.get(&pid).copied());
        children.entry(parent).or_default().push(process);
    }
    let mut ordered = Vec::new();
    let mut visited = HashSet::new();
    append_tree(None, 0, &children, &mut visited, &mut ordered);
    for process in fallback {
        if !visited.contains(&process.key) {
            append_process(process, 0, &children, &mut visited, &mut ordered);
        }
    }
    ordered
}

fn append_tree<'a>(
    parent: Option<ProcessKey>,
    depth: u16,
    children: &HashMap<Option<ProcessKey>, Vec<&'a ProcessSample>>,
    visited: &mut HashSet<ProcessKey>,
    ordered: &mut Vec<(&'a ProcessSample, u16)>,
) {
    if let Some(processes) = children.get(&parent) {
        for process in processes {
            append_process(process, depth, children, visited, ordered);
        }
    }
}

fn append_process<'a>(
    process: &'a ProcessSample,
    depth: u16,
    children: &HashMap<Option<ProcessKey>, Vec<&'a ProcessSample>>,
    visited: &mut HashSet<ProcessKey>,
    ordered: &mut Vec<(&'a ProcessSample, u16)>,
) {
    if !visited.insert(process.key) {
        return;
    }
    ordered.push((process, depth));
    append_tree(Some(process.key), depth.saturating_add(1), children, visited, ordered);
}

#[cfg(test)]
mod tests {
    use super::super::model::{CpuSample, ProcessSample, SystemSample, ThreadKey, ThreadSample};
    use super::*;

    fn snapshot() -> Arc<StatsSnapshot> {
        Arc::new(StatsSnapshot {
            sampled_at_ms: 0,
            interval_ms: 1_000,
            sample_duration_ms: 5,
            warmed_up: true,
            detail_scope: DetailScope::None,
            threads_warmed_up: false,
            system: SystemSample {
                global_cpu_percent: 25.0,
                cpus: vec![CpuSample { logical_index: 0, usage_percent: 25.0 }],
                total_memory_bytes: 1024,
                used_memory_bytes: 512,
                total_swap_bytes: 0,
                used_swap_bytes: 0,
                process_count: 2,
                thread_count: 0,
                load_average: [1.0, 0.5, 0.25],
                uptime_seconds: 1,
            },
            processes: vec![process(2, "cool", 20.0), process(3, "quiet", 1.0)],
            threads: Vec::new(),
            warnings: Vec::new(),
        })
    }

    fn snapshot_with_cores(count: u16) -> Arc<StatsSnapshot> {
        let mut snapshot = Arc::unwrap_or_clone(snapshot());
        snapshot.system.cpus = (0..count)
            .map(|logical_index| CpuSample {
                logical_index,
                usage_percent: logical_index as f32 * 3.0 % 100.0,
            })
            .collect();
        Arc::new(snapshot)
    }

    fn process(pid: u32, name: &str, cpu: f32) -> ProcessSample {
        ProcessSample {
            key: ProcessKey { pid, start_token: pid as u64 },
            identity_verified: true,
            parent_pid: Some(1),
            name: name.into(),
            command: format!("/bin/{name}"),
            user: Some("user".into()),
            status: "Run".into(),
            cpu_percent: cpu,
            rss_bytes: pid as u64 * 100,
            virtual_memory_bytes: 0,
            started_at_ms: 0,
            run_time_seconds: 1,
            last_cpu: Some(0),
        }
    }

    #[test]
    fn filtering_and_sorting_preserve_process_identity() {
        let mut app = StatsApp::new(snapshot());
        app.selected = Some(ProcessKey { pid: 3, start_token: 3 });
        app.filter.set("quiet".into());
        app.rebuild();
        assert_eq!(app.selected.unwrap().pid, 3);
        assert_eq!(app.visible.len(), 1);
    }

    #[test]
    fn compact_and_wide_render_without_panicking() {
        for size in [(30, 8), (60, 18), (130, 35)] {
            let backend = ratatui::backend::TestBackend::new(size.0, size.1);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let mut app = StatsApp::new(snapshot());
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            if size.0 >= 48 && size.1 >= 13 {
                assert!(!app.regions.cores.is_empty());
                assert!(!app.regions.rows.is_empty());
            }
        }
    }

    #[test]
    fn many_cores_stay_compact_and_leave_the_process_surface_dominant() {
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = StatsApp::new(snapshot_with_cores(32));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let core_bottom = app.regions.cores.iter().map(|(area, _)| area.bottom()).max().unwrap();
        let first_process_row = app.regions.rows.first().unwrap().0.y;
        assert!(core_bottom <= 8, "32 cores consumed rows through {core_bottom}");
        assert!(
            first_process_row <= 12,
            "process table did not begin until row {first_process_row}"
        );
        assert!(app.regions.end_process.is_some(), "inspector was not visible");

        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in ["kit stats", "CPU", "PROCESSES", "INSPECTOR"] {
            assert!(screen.contains(label), "missing {label} surface");
        }
    }

    #[test]
    fn selected_inspector_identity_never_scrolls_the_top_processes_away() {
        let mut snapshot = Arc::unwrap_or_clone(snapshot());
        snapshot.processes = (0..30)
            .map(|index| process(index + 10, &format!("process-{index:02}"), index as f32))
            .collect();
        let quiet = snapshot.processes[0].key;
        let mut app = StatsApp::new(Arc::new(snapshot));
        app.selected = Some(quiet);

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(app.visible[0].name, "process-29");
        assert_eq!(app.regions.rows[0].1, 0);
        assert_eq!(app.selected, Some(quiet));
    }

    #[test]
    fn tree_mode_places_a_child_after_its_parent() {
        let mut snapshot = Arc::unwrap_or_clone(snapshot());
        snapshot.processes[1].parent_pid = Some(2);
        let mut app = StatsApp::new(Arc::new(snapshot));
        app.tree_mode = true;
        app.rebuild();
        assert_eq!(app.visible[0].pid, 2);
        assert_eq!(app.visible[1].pid, 3);
        assert_eq!(app.visible[1].depth, 1);
    }

    #[test]
    fn tree_mode_orders_a_parent_cycle_from_the_active_sort() {
        let mut snapshot = Arc::unwrap_or_clone(snapshot());
        snapshot.processes[0].parent_pid = Some(3);
        snapshot.processes[1].parent_pid = Some(2);
        let mut app = StatsApp::new(Arc::new(snapshot));
        app.tree_mode = true;
        app.rebuild();
        assert_eq!(app.visible[0].pid, 2);
        assert_eq!(app.visible[1].pid, 3);
    }

    #[test]
    fn expansion_requests_only_the_selected_process_threads() {
        let mut app = StatsApp::new(snapshot());
        let selected = app.selected.unwrap();
        app.toggle_expand();
        assert_eq!(app.detail_scope(), DetailScope::Process(selected));
        assert_eq!(app.expanded.len(), 1);
    }

    #[test]
    fn core_focus_aggregates_only_threads_last_seen_on_that_core() {
        let mut snapshot = Arc::unwrap_or_clone(snapshot());
        snapshot.detail_scope = DetailScope::Core(0);
        snapshot.threads_warmed_up = true;
        let process = snapshot.processes[0].key;
        snapshot.threads = vec![ThreadSample {
            key: ThreadKey { tid: 20, start_token: 20 },
            process,
            name: "worker".into(),
            cpu_percent: 12.5,
            last_cpu: Some(0),
        }];
        let mut app = StatsApp::new(Arc::new(snapshot));
        app.focused_core = Some(0);
        app.rebuild();
        assert_eq!(app.visible.len(), 1);
        assert_eq!(app.visible[0].pid, process.pid);
        assert_eq!(app.visible[0].cpu, 12.5);
        assert_eq!(app.visible[0].core, Some(0));
    }

    #[test]
    fn drawn_core_and_confirmation_buttons_are_clickable() {
        let backend = ratatui::backend::TestBackend::new(130, 35);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = StatsApp::new(snapshot());
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let core = app.regions.cores[0].0;
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: core.x,
            row: core.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focused_core, Some(0));

        app.focused_core = None;
        app.rebuild();
        app.open_confirmation(ProcessSignal::Terminate);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let force = app.regions.confirm_force.unwrap();
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: force.x,
            row: force.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.confirm.as_ref().unwrap().signal, ProcessSignal::Kill);
        let yes = app.regions.confirm_yes.unwrap();
        assert!(matches!(
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: yes.x,
                row: yes.y,
                modifiers: KeyModifiers::NONE,
            }),
            Action::Signal(_, ProcessSignal::Kill)
        ));
    }
}
