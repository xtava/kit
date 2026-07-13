//! scout's live TUI: a navigable instance → role/target tree that re-surveys on an interval, with
//! per-instance PSS deltas between sweeps. The survey runs on a spawned task and arrives over a
//! channel, so the UI stays responsive while CDP probes are in flight.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
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
use crate::tui::{EventReader, Session, SessionOptions};

const REFRESH: Duration = Duration::from_secs(4);
const DELTA_FLOOR_KIB: i64 = 512;

pub async fn run(marker: String) -> Result<()> {
    let mut session = Session::open(SessionOptions::default())?;
    let mut events = EventReader::start();
    let (tx, mut rx) = mpsc::unbounded_channel::<Survey>();

    let mut app = App::new(marker);
    app.loading = true;
    spawn_survey(app.marker.clone(), tx.clone());
    let mut tick = time::interval(REFRESH);

    loop {
        session.draw(|frame| render(frame, &mut app))?;

        tokio::select! {
            _ = tick.tick() => {
                if !app.loading {
                    app.loading = true;
                    spawn_survey(app.marker.clone(), tx.clone());
                }
            }
            Some(survey) = rx.recv() => app.ingest(survey),
            event = events.recv() => match event {
                Some(Event::Key(key)) => match app.on_key(key) {
                    Action::Quit => break,
                    Action::Refresh => {
                        if !app.loading {
                            app.loading = true;
                            spawn_survey(app.marker.clone(), tx.clone());
                        }
                    }
                    Action::None => {}
                },
                None => break,
                _ => {}
            },
        }
    }
    Ok(())
}

fn spawn_survey(marker: String, tx: UnboundedSender<Survey>) {
    tokio::spawn(async move {
        let _ = tx.send(survey::collect(&marker).await);
    });
}

enum Action {
    None,
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
        }
    }

    fn ingest(&mut self, survey: Survey) {
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

    fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('r') => Action::Refresh,
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Action::None
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.toggle();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let bound = self.rows.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, bound) as usize;
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

fn render(frame: &mut Frame, app: &mut App) {
    let chunks =
        Layout::vertical([Constraint::Length(3), Constraint::Min(1), Constraint::Length(1)])
            .split(frame.area());
    render_header(frame, chunks[0], app);
    render_tree(frame, chunks[1], app);
    render_footer(frame, chunks[2]);
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

fn render_tree(frame: &mut Frame, area: Rect, app: &mut App) {
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> =
        app.rows.iter().map(|row| ListItem::new(row_line(row, width))).collect();
    let list = List::new(items)
        .block(panel(" fleet "))
        .highlight_style(Style::default().bg(Color::Rgb(40, 44, 52)).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌ ");
    app.list.select(if app.rows.is_empty() { None } else { Some(app.selected) });
    frame.render_stateful_widget(list, area, &mut app.list);
}

fn render_footer(frame: &mut Frame, area: Rect) {
    let footer = Paragraph::new(Line::from(vec![
        key("↑↓"),
        Span::raw(" nav  "),
        key("⏎"),
        Span::raw(" expand  "),
        key("r"),
        Span::raw(" refresh  "),
        key("q"),
        Span::raw(" quit"),
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
