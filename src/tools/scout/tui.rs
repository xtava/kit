//! scout's live TUI: a navigable instance → role/target tree that re-surveys on an interval, with
//! per-instance PSS deltas between sweeps. The survey runs on a spawned task and arrives over a
//! channel, so the UI stays responsive while CDP probes are in flight.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Padding, Paragraph};
use ratatui::Frame;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time;

use super::format::{human, role_breakdown, target_groups};
use super::model::{Survey, TargetKind};
use super::proc::total_pss;
use super::survey;
use crate::tui::{
    render_vertical_scrollbar, theme::NORD, CommandPalette, CommandPaletteLayout,
    CommandPaletteOutcome, EventReader, KeyChord, KeybindingResolution, KeybindingState,
    ScrollbarDrag, ScrollbarLayout, ScrollbarStyle, SelectableRegion, SelectionOutcome, Session,
    SessionOptions, TextSelection, Viewport, ViewportMetrics,
};

use super::actions::{self, ScoutActionRegistry, ScoutCommand};

const REFRESH: Duration = Duration::from_secs(4);
const DELTA_FLOOR_KIB: i64 = 512;

pub async fn run(marker: String) -> Result<()> {
    let mut session =
        Session::open(SessionOptions { mouse_capture: true, bracketed_paste: false })?;
    let mut events = EventReader::start();
    let (tx, mut rx) = mpsc::unbounded_channel::<Survey>();

    let mut app = App::new(marker);
    app.loading = true;
    spawn_survey(app.marker.clone(), tx.clone());
    let mut tick = time::interval(REFRESH);
    let mut regions = UiRegions::default();

    loop {
        session.draw(|frame| regions = render(frame, &mut app))?;

        tokio::select! {
            _ = tick.tick() => {
                if !app.loading {
                    app.loading = true;
                    spawn_survey(app.marker.clone(), tx.clone());
                }
            }
            Some(survey) = rx.recv() => app.ingest(survey),
            event = events.recv() => match event {
                Some(event) => match app.on_event(event, &regions) {
                    Flow::Quit => break,
                    Flow::Refresh => {
                        if !app.loading {
                            app.loading = true;
                            spawn_survey(app.marker.clone(), tx.clone());
                        }
                    }
                    Flow::Continue => {}
                },
                None => break,
            },
        }
        if let Some(text) = app.pending_copy.take() {
            session.copy(&text)?;
        }
    }
    Ok(())
}

fn spawn_survey(marker: String, tx: UnboundedSender<Survey>) {
    tokio::spawn(async move {
        let _ = tx.send(survey::collect(&marker).await);
    });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectionSurface {
    ChildDetails,
}

#[derive(Default)]
struct UiRegions {
    tree: Option<Rect>,
    rows: Vec<(Rect, usize)>,
    scrollbar: Option<ScrollbarLayout>,
    command_palette: Option<CommandPaletteLayout>,
    selectable: Vec<SelectableRegion<SelectionSurface>>,
}

enum Surface {
    Normal,
    CommandPalette(CommandPalette<()>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Flow {
    Continue,
    Quit,
    Refresh,
}

struct App {
    marker: String,
    survey: Option<Survey>,
    prev_pss: HashMap<String, u64>,
    expanded: HashSet<String>,
    rows: Vec<Row>,
    selected: usize,
    list: ListState,
    loading: bool,
    registry: ScoutActionRegistry,
    keybindings: KeybindingState,
    viewport: Viewport,
    metrics: ViewportMetrics,
    scrollbar_drag: Option<ScrollbarDrag>,
    selection: TextSelection<SelectionSurface>,
    revision: u64,
    pending_copy: Option<String>,
    surface: Surface,
}

struct Row {
    depth: u8,
    label: String,
    detail: String,
    delta_kib: Option<i64>,
    instance: Option<String>,
}

impl App {
    fn new(marker: String) -> Self {
        Self {
            marker,
            survey: None,
            prev_pss: HashMap::new(),
            expanded: HashSet::new(),
            rows: Vec::new(),
            selected: 0,
            list: ListState::default(),
            loading: false,
            registry: actions::registry().expect("valid Scout action registry"),
            keybindings: KeybindingState::default(),
            viewport: Viewport::default(),
            metrics: ViewportMetrics::default(),
            scrollbar_drag: None,
            selection: TextSelection::default(),
            revision: 0,
            pending_copy: None,
            surface: Surface::Normal,
        }
    }

    fn ingest(&mut self, survey: Survey) {
        self.selection.clear();
        self.revision = self.revision.wrapping_add(1);
        if self.survey.is_none() {
            if let Some(first) = survey.instances.first() {
                self.expanded.insert(first.name.clone());
            }
        }
        self.survey = Some(survey);
        self.loading = false;
        self.rebuild();
        if let Some(survey) = &self.survey {
            self.prev_pss =
                survey.instances.iter().map(|inst| (inst.name.clone(), total_pss(inst))).collect();
        }
    }

    fn on_event(&mut self, event: Event, regions: &UiRegions) -> Flow {
        if matches!(self.surface, Surface::CommandPalette(_)) {
            return self.on_palette_event(event, regions);
        }
        match event {
            Event::Key(key) => self.on_key(key),
            Event::Mouse(mouse) => {
                self.on_mouse(mouse, regions);
                Flow::Continue
            }
            Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {
                Flow::Continue
            }
        }
    }

    fn on_palette_event(&mut self, event: Event, regions: &UiRegions) -> Flow {
        let Some(layout) = regions.command_palette.as_ref() else {
            self.surface = Surface::Normal;
            return Flow::Continue;
        };
        let outcome = match &mut self.surface {
            Surface::CommandPalette(palette) => palette.on_event(event, layout),
            Surface::Normal => unreachable!("palette surface checked above"),
        };
        match outcome {
            CommandPaletteOutcome::Captured => Flow::Continue,
            CommandPaletteOutcome::Dismissed => {
                self.surface = Surface::Normal;
                Flow::Continue
            }
            CommandPaletteOutcome::Invoke(invocation) => {
                self.surface = Surface::Normal;
                self.invoke(invocation)
            }
        }
    }

    fn on_key(&mut self, key: crossterm::event::KeyEvent) -> Flow {
        match self.selection.on_key(key) {
            SelectionOutcome::CopyReady(text) => {
                self.pending_copy = Some(text);
                return Flow::Continue;
            }
            SelectionOutcome::Captured | SelectionOutcome::Changed => return Flow::Continue,
            SelectionOutcome::Unhandled | SelectionOutcome::EdgeScroll { .. } => {}
        }
        let Some(chord) = KeyChord::from_event(key) else {
            return Flow::Continue;
        };
        let invocation = match self.registry.resolve_keybinding(&mut self.keybindings, chord, ()) {
            KeybindingResolution::Invoke(invocation) => invocation,
            _ => return Flow::Continue,
        };
        self.invoke(invocation)
    }

    fn invoke(&mut self, invocation: crate::tui::ActionInvocation<()>) -> Flow {
        let Ok(command) = self.registry.command_for(&invocation) else { return Flow::Continue };
        match command {
            ScoutCommand::Previous => self.move_selection(-1),
            ScoutCommand::Next => self.move_selection(1),
            ScoutCommand::Toggle => self.toggle(),
            ScoutCommand::OpenCommandPalette => {
                self.surface = Surface::CommandPalette(CommandPalette::open(
                    invocation.context,
                    &self.registry,
                ));
            }
            ScoutCommand::Quit => return Flow::Quit,
            ScoutCommand::Refresh => return Flow::Refresh,
        }
        Flow::Continue
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let bound = self.rows.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, bound) as usize;
    }

    fn on_mouse(&mut self, mouse: MouseEvent, regions: &UiRegions) {
        let position = Position::new(mouse.column, mouse.row);
        if let Some(drag) = self.scrollbar_drag {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(scrollbar) = regions.scrollbar {
                        let top = drag.top_for_row(scrollbar, position.y);
                        self.viewport.set_top(top, self.metrics);
                        self.selected = top;
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => self.scrollbar_drag = None,
                _ => {}
            }
            return;
        }
        if let Some(scrollbar) = regions.scrollbar.filter(|scrollbar| scrollbar.contains(position))
        {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                if let Some(drag) = ScrollbarDrag::begin(scrollbar, position) {
                    self.scrollbar_drag = Some(drag);
                } else {
                    let top = scrollbar.top_for_track_row(position.y);
                    self.viewport.set_top(top, self.metrics);
                    self.selected = top;
                }
            }
            return;
        }
        if matches!(mouse.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown)
            && regions.tree.is_some_and(|area| area.contains(position))
        {
            self.move_selection(if mouse.kind == MouseEventKind::ScrollUp { -3 } else { 3 });
            return;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some((_, index)) = regions.rows.iter().find(|(area, _)| area.contains(position))
            {
                self.selected = *index;
                if self.rows[*index].instance.is_some() {
                    self.toggle();
                    return;
                }
            }
        }
        match self.selection.on_mouse(mouse) {
            SelectionOutcome::CopyReady(text) => self.pending_copy = Some(text),
            SelectionOutcome::EdgeScroll { lines, .. } => self.move_selection(lines),
            _ => {}
        }
    }

    fn toggle(&mut self) {
        let Some(name) = self.rows.get(self.selected).and_then(|row| row.instance.clone()) else {
            return;
        };
        if !self.expanded.remove(&name) {
            self.expanded.insert(name);
        }
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let Some(survey) = &self.survey else {
            return;
        };
        let mut rows = Vec::new();
        for instance in &survey.instances {
            let pss = total_pss(instance);
            let open = self.expanded.contains(&instance.name);
            let port = instance
                .debug_port
                .map(|port| format!(":{port}"))
                .unwrap_or_else(|| "no-debug".to_owned());
            rows.push(Row {
                depth: 0,
                label: format!(
                    "{} {}  ▕ {} procs · {port}",
                    if open { "▾" } else { "▸" },
                    instance.name,
                    instance.processes.len()
                ),
                detail: human(pss),
                delta_kib: self.prev_pss.get(&instance.name).map(|prev| pss as i64 - *prev as i64),
                instance: Some(instance.name.clone()),
            });

            if !open {
                continue;
            }
            for (label, count, role_pss) in role_breakdown(instance) {
                rows.push(Row::child(format!("{label} ×{count}"), human(role_pss)));
            }
            for target in &instance.targets {
                if let TargetKind::Workbench { workspace } = &target.kind {
                    let js = target.js_heap_kib.map(human).unwrap_or_else(|| "—".to_owned());
                    let nodes =
                        target.dom_nodes.map(|n| n.to_string()).unwrap_or_else(|| "—".to_owned());
                    rows.push(Row::child(
                        format!("⊞ workspace {}", workspace.chars().take(8).collect::<String>()),
                        format!("{js} js · {nodes} nodes"),
                    ));
                }
            }
            for (label, count, js) in target_groups(instance) {
                rows.push(Row::child(format!("{label} ×{count}"), format!("{} js", human(js))));
            }
        }

        self.rows = rows;
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }
}

impl Row {
    fn child(label: String, detail: String) -> Self {
        Self { depth: 1, label, detail, delta_kib: None, instance: None }
    }
}

fn render(frame: &mut Frame, app: &mut App) -> UiRegions {
    let chunks =
        Layout::vertical([Constraint::Length(3), Constraint::Min(1), Constraint::Length(1)])
            .split(frame.area());
    render_header(frame, chunks[0], app);
    let mut regions = UiRegions::default();
    render_tree(frame, chunks[1], app, &mut regions);
    render_footer(frame, chunks[2], &app.registry);
    let selectable = regions.selectable.clone();
    app.selection.capture_frame(
        frame,
        &selectable,
        Style::default().bg(Color::Rgb(40, 44, 52)).add_modifier(Modifier::REVERSED),
    );
    if let Surface::CommandPalette(palette) = &app.surface {
        let layout = palette.layout(frame.area());
        palette.render(frame, &layout, NORD);
        regions.command_palette = Some(layout);
    }
    regions
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let headline = match &app.survey {
        Some(survey) => {
            let system = &survey.system;
            let used = system.total_kib.saturating_sub(system.available_kib);
            format!(
                "{} fleet · {} instances · system {} / {} · swap {} / {}",
                app.marker,
                survey.instances.len(),
                human(used),
                human(system.total_kib),
                human(system.swap_used_kib),
                human(system.swap_total_kib),
            )
        }
        None => format!("{} fleet · surveying…", app.marker),
    };
    let status = if app.loading { "  ⟳ surveying" } else { "" };
    let header = Paragraph::new(Line::from(vec![
        Span::styled(headline, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(status, Style::default().fg(Color::DarkGray)),
    ]))
    .block(panel(" scout "));
    frame.render_widget(header, area);
}

fn render_tree(frame: &mut Frame, area: Rect, app: &mut App, regions: &mut UiRegions) {
    let width = area.width.saturating_sub(4) as usize;
    let inner = panel(" fleet ").inner(area);
    app.metrics = ViewportMetrics::new(app.rows.len(), usize::from(inner.height));
    app.viewport.ensure_visible(app.selected, app.metrics);
    let visible = app.viewport.visible_range(app.metrics);
    let top = visible.start;
    let items: Vec<ListItem> =
        app.rows[visible.clone()].iter().map(|row| ListItem::new(row_line(row, width))).collect();
    let list = List::new(items)
        .block(panel(" fleet "))
        .highlight_style(Style::default().bg(Color::Rgb(40, 44, 52)).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌ ");
    app.list.select(if app.rows.is_empty() { None } else { Some(app.selected - top) });
    frame.render_stateful_widget(list, area, &mut app.list);
    regions.tree = Some(inner);
    regions.rows = visible
        .clone()
        .enumerate()
        .map(|(offset, index)| (Rect::new(inner.x, inner.y + offset as u16, inner.width, 1), index))
        .collect();
    for (offset, index) in visible.enumerate() {
        if app.rows[index].instance.is_none() {
            regions.selectable.push(SelectableRegion::new(
                SelectionSurface::ChildDetails,
                Rect::new(inner.x, inner.y + offset as u16, inner.width, 1),
                index as i64,
                0,
                app.revision,
            ));
        }
    }
    regions.scrollbar = ScrollbarLayout::vertical_right(inner, app.metrics, top);
    if let Some(scrollbar) = regions.scrollbar {
        render_vertical_scrollbar(
            frame,
            scrollbar,
            app.scrollbar_drag.is_some(),
            ScrollbarStyle {
                track_color: Color::DarkGray,
                thumb_color: Color::Gray,
                active_thumb_color: Color::Cyan,
                track_symbol: "│",
                thumb_symbol: "┃",
            },
        );
    }
}

fn render_footer(frame: &mut Frame, area: Rect, registry: &ScoutActionRegistry) {
    let actions = registry.resolve_command_palette(&());
    let title = |command| {
        actions
            .items()
            .iter()
            .find(|action| {
                registry.command_for(&crate::tui::ActionInvocation::new(action.id, ())).ok()
                    == Some(command)
            })
            .map(|action| action.title)
            .unwrap_or("")
    };
    let footer = Paragraph::new(Line::from(vec![
        key("↑↓"),
        Span::raw(format!(" {}  ", title(ScoutCommand::Next))),
        key("⏎"),
        Span::raw(format!(" {}  ", title(ScoutCommand::Toggle))),
        key("r"),
        Span::raw(format!(" {}  ", title(ScoutCommand::Refresh))),
        key("Ctrl-P"),
        Span::raw(" commands  "),
        key("q"),
        Span::raw(format!(" {}", title(ScoutCommand::Quit))),
    ]))
    .style(Style::default().fg(Color::DarkGray))
    .alignment(Alignment::Center);
    frame.render_widget(footer, area);
}

fn row_line(row: &Row, width: usize) -> Line<'static> {
    let indent = "  ".repeat(row.depth as usize);
    let label = format!("{indent}{}", row.label);
    let delta = delta_span(row.delta_kib);
    let used = label.chars().count() + row.detail.chars().count() + delta.0.chars().count();
    let gap = width.saturating_sub(used).max(1);

    let label_style = if row.depth == 0 {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };

    Line::from(vec![
        Span::styled(label, label_style),
        Span::raw(" ".repeat(gap)),
        Span::styled(row.detail.clone(), Style::default().fg(Color::White)),
        Span::styled(delta.0, delta.1),
    ])
}

fn delta_span(delta_kib: Option<i64>) -> (String, Style) {
    match delta_kib {
        Some(delta) if delta >= DELTA_FLOOR_KIB => {
            (format!(" ▲{}", human(delta as u64)), Style::default().fg(Color::Red))
        }
        Some(delta) if delta <= -DELTA_FLOOR_KIB => {
            (format!(" ▼{}", human((-delta) as u64)), Style::default().fg(Color::Green))
        }
        _ => (String::new(), Style::default()),
    }
}

fn key(label: &'static str) -> Span<'static> {
    Span::styled(label, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
}

fn panel(title: &'static str) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::horizontal(1))
}
