use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph, Wrap},
    Frame,
};
use tokio::{
    sync::mpsc::{self, UnboundedSender},
    time,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::config::{Config, FavoriteAdd};
use super::engine::{expand_domains, CheckClient, CheckResult, Disposition, Verdict};
use crate::tui::{CommandSet, CommandSpec, EventReader, LineEditor, ParsedInput, Session};

const HISTORY_LIMIT: usize = 200;
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const COMMANDS: CommandSet = CommandSet::new(&[
    CommandSpec {
        name: "tlds",
        aliases: &[],
        usage: "/tlds [com,ai,io,studio]",
        description: "set the active TLDs, or open the TLD editor with no args",
    },
    CommandSpec { name: "quit", aliases: &["q"], usage: "/quit", description: "exit cleanly" },
    CommandSpec {
        name: "help",
        aliases: &["?"],
        usage: "/help",
        description: "show available slash commands",
    },
    CommandSpec {
        name: "clear",
        aliases: &[],
        usage: "/clear",
        description: "clear the results region",
    },
    CommandSpec {
        name: "favorites",
        aliases: &["fav"],
        usage: "/favorites",
        description: "show saved favorite names",
    },
]);

pub async fn run(config: Config) -> Result<()> {
    let mut terminal = Session::open()?;
    let mut input = EventReader::start();
    let client = CheckClient::new()?;
    let (result_tx, mut result_rx) = mpsc::unbounded_channel();
    let mut app = App::new(config);
    let mut tick = time::interval(Duration::from_millis(90));

    loop {
        terminal.draw(|frame| render(frame, &app))?;

        tokio::select! {
            _ = tick.tick() => {
                if app.status.is_checking() {
                    app.spinner = app.spinner.wrapping_add(1);
                }
            }
            maybe_event = input.recv() => {
                let Some(event) = maybe_event else {
                    app.quit = true;
                    continue;
                };
                handle_event(event, &mut app, &client, &result_tx);
            }
            Some(result) = result_rx.recv() => {
                if result.generation == app.generation {
                    app.status = ResultsStatus::Ready {
                        query: result.query,
                        results: result.results,
                    };
                }
            }
        }

        if app.quit {
            break;
        }
    }

    Ok(())
}

fn handle_event(
    event: Event,
    app: &mut App,
    client: &CheckClient,
    result_tx: &UnboundedSender<QueryResult>,
) {
    let Event::Key(key) = event else {
        return;
    };

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.quit = true;
        return;
    }

    match app.mode {
        InputMode::Query => handle_query_key(key, app, client, result_tx),
        InputMode::Tlds => handle_tlds_key(key, app),
    }
}

fn handle_query_key(
    key: KeyEvent,
    app: &mut App,
    client: &CheckClient,
    result_tx: &UnboundedSender<QueryResult>,
) {
    match key.code {
        KeyCode::Enter => submit_input(app, client, result_tx),
        KeyCode::Tab => app.complete_command(),
        KeyCode::Up => app.recall_history(-1),
        KeyCode::Down => app.recall_history(1),
        KeyCode::Esc => app.clear_input(),
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.save_current_favorite();
        }
        _ => app.input.apply_key(key),
    }
}

fn submit_input(app: &mut App, client: &CheckClient, result_tx: &UnboundedSender<QueryResult>) {
    let raw = app.input.value().to_owned();
    match COMMANDS.parse(&raw) {
        ParsedInput::Empty => {}
        ParsedInput::Query(query) => {
            app.push_history(raw);
            app.clear_input();
            start_query(app, client, result_tx, query);
        }
        ParsedInput::Command { name, args } => {
            app.push_history(raw);
            app.clear_input();
            dispatch_command(app, name, &args);
        }
        ParsedInput::Unknown(name) => {
            app.push_history(raw);
            app.clear_input();
            app.notice = Some(if name.is_empty() {
                "Unknown command.".to_owned()
            } else {
                format!("Unknown command /{name}.")
            });
        }
    }
}

fn start_query(
    app: &mut App,
    client: &CheckClient,
    result_tx: &UnboundedSender<QueryResult>,
    query: String,
) {
    let domains = expand_domains([query.as_str()], app.config.tlds());
    if domains.is_empty() {
        return;
    }

    app.generation = app.generation.wrapping_add(1);
    app.status = ResultsStatus::Checking {
        query: query.clone(),
        domains: domains.clone(),
    };
    app.spinner = 0;

    let generation = app.generation;
    let client = client.clone();
    let result_tx = result_tx.clone();
    tokio::spawn(async move {
        let results = client.check_many(domains, 8).await;
        let _ = result_tx.send(QueryResult {
            generation,
            query,
            results,
        });
    });
}

fn dispatch_command(app: &mut App, name: &str, args: &str) {
    match name {
        "tlds" if args.trim().is_empty() => app.enter_tld_mode(),
        "tlds" => app.save_tlds(args),
        "quit" => app.quit = true,
        "help" => {
            app.generation = app.generation.wrapping_add(1);
            app.status = ResultsStatus::Help;
            app.notice = None;
        }
        "clear" => {
            app.generation = app.generation.wrapping_add(1);
            app.status = ResultsStatus::Empty;
            app.notice = Some("Results cleared".to_owned());
        }
        "favorites" => app.show_favorites(),
        _ => {}
    }
}

fn handle_tlds_key(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Enter => {
            let raw = app.input.value().to_owned();
            app.save_tlds(&raw);
        }
        KeyCode::Esc => app.leave_tld_mode("TLD edit cancelled"),
        _ => app.input.apply_key(key),
    }
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(1),
            Constraint::Length(8),
        ])
        .split(area);

    render_header(frame, vertical[0]);
    render_results(frame, vertical[1], app);
    render_input(frame, vertical[2], app);
}

fn render_header(frame: &mut Frame<'_>, area: Rect) {
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "domain",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "authoritative registration checker",
                Style::default().fg(Color::Gray),
            ),
        ]),
        Line::styled(
            "DNS delegation -> registry RDAP -> registry WHOIS",
            Style::default().fg(Color::DarkGray),
        ),
    ])
    .alignment(Alignment::Center)
    .block(panel(" domain "));
    frame.render_widget(header, area);
}

fn render_results(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = panel(app.results_title());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = app.result_lines(inner.width as usize);
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(Color::White));

    frame.render_widget(paragraph, inner);
}

fn render_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = panel(" input ").border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let footer_y = inner.y + inner.height.saturating_sub(1);
    if inner.height < 2 {
        frame.render_widget(
            Paragraph::new(app.footer()).wrap(Wrap { trim: false }),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
        return;
    }

    let input_y = footer_y.saturating_sub(1);
    let hint_height = input_y.saturating_sub(inner.y);

    if hint_height > 0 {
        let hint_area = Rect::new(inner.x, inner.y, inner.width, hint_height);
        let hints = app.command_hint_lines(hint_height as usize);
        frame.render_widget(Paragraph::new(hints).wrap(Wrap { trim: false }), hint_area);
    }

    let prompt = app.prompt();
    let input_line = Line::from(vec![
        Span::styled(prompt.clone(), Style::default().fg(Color::Cyan)),
        Span::raw(app.input.value()),
    ]);

    if input_y >= inner.y {
        frame.render_widget(
            Paragraph::new(input_line).wrap(Wrap { trim: false }),
            Rect::new(inner.x, input_y, inner.width, 1),
        );
    }

    frame.render_widget(
        Paragraph::new(app.footer()).wrap(Wrap { trim: false }),
        Rect::new(inner.x, footer_y, inner.width, 1),
    );

    let cursor_x = inner.x
        + prompt.width() as u16
        + UnicodeWidthStr::width(&app.input.value()[..app.input.cursor()]) as u16;
    frame.set_cursor_position((cursor_x.min(inner.right().saturating_sub(1)), input_y));
}

fn panel(title: &'static str) -> Block<'static> {
    Block::default()
        .title(title)
        .title_alignment(Alignment::Left)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::new(1, 1, 0, 0))
}

#[derive(Debug)]
struct App {
    config: Config,
    mode: InputMode,
    input: LineEditor,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
    status: ResultsStatus,
    generation: u64,
    spinner: usize,
    quit: bool,
    notice: Option<String>,
}

impl App {
    fn new(config: Config) -> Self {
        Self {
            config,
            mode: InputMode::Query,
            input: LineEditor::default(),
            history: Vec::new(),
            history_index: None,
            draft: String::new(),
            status: ResultsStatus::Empty,
            generation: 0,
            spinner: 0,
            quit: false,
            notice: None,
        }
    }

    fn prompt(&self) -> String {
        match self.mode {
            InputMode::Query => "› ".to_owned(),
            InputMode::Tlds => "tlds › ".to_owned(),
        }
    }

    fn footer(&self) -> Line<'static> {
        let tlds = self.config.tlds().join(", ");
        let mut spans = vec![
            Span::styled(
                "TLDs:",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(tlds, Style::default().fg(Color::White)),
            Span::styled("  |  ", Style::default().fg(Color::DarkGray)),
        ];

        match self.mode {
            InputMode::Query => {
                spans.extend([
                    Span::styled("Enter", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" check  "),
                    Span::styled("↑/↓", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" history  "),
                    Span::styled("Ctrl-F", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" favorite  "),
                    Span::styled("Ctrl-C", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" quit"),
                ]);
            }
            InputMode::Tlds => {
                spans.extend([
                    Span::styled("Enter", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" save TLDs  "),
                    Span::styled("Esc", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(" cancel"),
                ]);
            }
        }

        Line::from(spans).style(Style::default().fg(Color::Gray).add_modifier(Modifier::DIM))
    }

    fn command_hint_lines(&self, max_lines: usize) -> Vec<Line<'static>> {
        if self.mode == InputMode::Tlds {
            return vec![Line::styled(
                "Edit comma-separated TLDs, then press Enter to save.",
                Style::default().fg(Color::Gray),
            )];
        }

        if !self.input.value().trim_start().starts_with('/') {
            return vec![Line::styled(
                "Enter a name or full domain to check.",
                Style::default().fg(Color::DarkGray),
            )];
        }

        let matches = COMMANDS.suggestions(self.input.value());
        if matches.is_empty() {
            return vec![Line::styled(
                "No matching command.",
                Style::default().fg(Color::Yellow),
            )];
        }

        matches
            .into_iter()
            .take(max_lines.max(1))
            .map(|spec| {
                Line::from(vec![
                    Span::styled(
                        spec.usage,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(spec.description, Style::default().fg(Color::Gray)),
                ])
            })
            .collect()
    }

    fn results_title(&self) -> &'static str {
        match &self.status {
            ResultsStatus::Empty => " results ",
            ResultsStatus::Checking { .. } => " checking ",
            ResultsStatus::Ready { .. } => " results ",
            ResultsStatus::Help => " slash commands ",
            ResultsStatus::Favorites => " favorites ",
        }
    }

    fn result_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();

        if let Some(notice) = &self.notice {
            lines.push(Line::styled(
                notice.clone(),
                Style::default().fg(Color::Green),
            ));
            lines.push(Line::raw(""));
        }

        match &self.status {
            ResultsStatus::Empty => {
                lines.push(Line::styled(
                    "Type a name or full domain, then press Enter.",
                    Style::default().fg(Color::Gray),
                ));
                lines.push(Line::styled(
                    "Available means a registry source confirmed the domain is not registered.",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ResultsStatus::Help => {
                lines.push(Line::styled(
                    "Slash commands",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::styled(
                    "Lines beginning with / are commands; everything else is a domain query.",
                    Style::default().fg(Color::Gray),
                ));
                lines.push(Line::raw(""));

                for spec in COMMANDS.all() {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{:<28}", spec.usage),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(spec.description, Style::default().fg(Color::Gray)),
                    ]));
                }
            }
            ResultsStatus::Favorites => {
                lines.push(Line::styled(
                    "Favorites",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
                lines.push(Line::styled(
                    "Press Ctrl-F after a check to save the current name.",
                    Style::default().fg(Color::Gray),
                ));
                lines.push(Line::raw(""));

                if self.config.favorites().is_empty() {
                    lines.push(Line::styled(
                        "No favorites saved yet.",
                        Style::default().fg(Color::DarkGray),
                    ));
                } else {
                    for (index, favorite) in self.config.favorites().iter().enumerate() {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("{:>2}. ", index + 1),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(favorite.clone(), Style::default().fg(Color::White)),
                        ]));
                    }
                }
            }
            ResultsStatus::Checking { query, domains } => {
                let spinner = SPINNER[self.spinner % SPINNER.len()];
                lines.push(Line::from(vec![
                    Span::styled(
                        spinner,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" checking "),
                    Span::styled(query.clone(), Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!(" across {} domain(s)", domains.len()),
                        Style::default().fg(Color::Gray),
                    ),
                ]));
                lines.push(Line::raw(""));
                for domain in domains.iter().take(visible_domain_count(width)) {
                    lines.push(Line::styled(
                        format!("  {domain}"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }
            ResultsStatus::Ready { query, results } => {
                lines.push(Line::from(vec![
                    Span::styled(query.clone(), Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(" results", Style::default().fg(Color::Gray)),
                ]));
                lines.push(Line::raw(""));

                let domain_width = results
                    .iter()
                    .map(|result| result.domain.len())
                    .max()
                    .unwrap_or(6);
                for result in results {
                    lines.push(result_line(result, domain_width, width));
                    if let Some(trace) = trace_line(result, width) {
                        lines.push(trace);
                    }
                }

                let available = results
                    .iter()
                    .filter(|result| result.verdict == Verdict::Available)
                    .count();
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    format!("{available}/{} available", results.len()),
                    Style::default().fg(Color::Gray),
                ));
            }
        }

        lines
    }

    fn enter_tld_mode(&mut self) {
        self.mode = InputMode::Tlds;
        self.input.set(self.config.tlds().join(","));
        self.notice = None;
    }

    fn leave_tld_mode(&mut self, notice: impl Into<String>) {
        self.mode = InputMode::Query;
        self.input.clear();
        self.notice = Some(notice.into());
    }

    fn save_tlds(&mut self, raw: &str) {
        let tlds = raw
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();

        match self.config.set_tlds(tlds) {
            Ok(()) => {
                let saved = self.config.tlds().join(",");
                self.leave_tld_mode(format!(
                    "TLD set saved to {}: [{}]",
                    self.config.path().display(),
                    saved
                ));
            }
            Err(err) => {
                self.notice = Some(format!("Could not save TLDs: {err:#}"));
            }
        }
    }

    fn show_favorites(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.status = ResultsStatus::Favorites;
        self.notice = None;
    }

    fn save_current_favorite(&mut self) {
        let Some(target) = self.favorite_target().map(ToOwned::to_owned) else {
            self.notice = Some("No checked name to favorite yet.".to_owned());
            return;
        };

        match self.config.add_favorite(&target) {
            Ok(FavoriteAdd::Added(favorite)) => {
                self.notice = Some(format!("Saved favorite: {favorite}"));
            }
            Ok(FavoriteAdd::AlreadyExists(favorite)) => {
                self.notice = Some(format!("Already in favorites: {favorite}"));
            }
            Err(err) => {
                self.notice = Some(format!("Could not save favorite: {err:#}"));
            }
        }
    }

    fn favorite_target(&self) -> Option<&str> {
        match &self.status {
            ResultsStatus::Checking { query, .. } | ResultsStatus::Ready { query, .. } => {
                Some(query)
            }
            ResultsStatus::Empty | ResultsStatus::Help | ResultsStatus::Favorites => None,
        }
    }

    fn push_history(&mut self, entry: String) {
        if self.history.last() != Some(&entry) {
            self.history.push(entry);
            if self.history.len() > HISTORY_LIMIT {
                self.history.remove(0);
            }
        }
        self.history_index = None;
        self.draft.clear();
    }

    fn recall_history(&mut self, direction: isize) {
        if self.history.is_empty() {
            return;
        }

        match self.history_index {
            None if direction < 0 => {
                self.draft = self.input.value().to_owned();
                self.history_index = Some(self.history.len().saturating_sub(1));
            }
            None => return,
            Some(index) => {
                let next = index as isize + direction;
                if next >= self.history.len() as isize {
                    self.history_index = None;
                    self.input.set(self.draft.clone());
                    return;
                }
                self.history_index = Some(next.max(0) as usize);
            }
        }

        if let Some(index) = self.history_index {
            if let Some(entry) = self.history.get(index) {
                self.input.set(entry.clone());
            }
        }
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.history_index = None;
    }

    fn complete_command(&mut self) {
        if let Some(completion) = COMMANDS.complete(self.input.value()) {
            self.input.set(completion);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputMode {
    Query,
    Tlds,
}

#[derive(Clone, Debug)]
enum ResultsStatus {
    Empty,
    Help,
    Favorites,
    Checking {
        query: String,
        domains: Vec<String>,
    },
    Ready {
        query: String,
        results: Vec<CheckResult>,
    },
}

impl ResultsStatus {
    fn is_checking(&self) -> bool {
        matches!(self, Self::Checking { .. })
    }
}

#[derive(Debug)]
struct QueryResult {
    generation: u64,
    query: String,
    results: Vec<CheckResult>,
}

fn visible_domain_count(width: usize) -> usize {
    if width < 40 {
        5
    } else {
        12
    }
}

fn result_line(result: &CheckResult, domain_width: usize, width: usize) -> Line<'static> {
    let (symbol, verdict_style) = match result.verdict {
        Verdict::Available => ("✓", Style::default().fg(Color::Green)),
        Verdict::Taken => (
            "✗",
            Style::default().fg(Color::Red).add_modifier(Modifier::DIM),
        ),
        Verdict::Inconclusive => ("?", Style::default().fg(Color::Yellow)),
    };

    let badge = disposition_badge(result);
    let meta = format!("{} · {}ms · {}", result.source, result.ms, result.evidence);

    let left = format!(
        "{symbol} {:<domain_width$}  {:<12}",
        result.domain, result.verdict
    );
    let badge_cells = badge
        .as_ref()
        .map(|(text, _)| 2 + UnicodeWidthStr::width(text.as_str()))
        .unwrap_or(0);
    let left_width = UnicodeWidthStr::width(left.as_str());
    let meta_width = UnicodeWidthStr::width(meta.as_str());
    let gap = width
        .saturating_sub(left_width + badge_cells + meta_width)
        .max(2);

    let mut spans = vec![
        Span::styled(symbol, verdict_style.add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(
            format!("{:<domain_width$}", result.domain),
            Style::default().fg(Color::White),
        ),
        Span::raw("  "),
        Span::styled(format!("{:<12}", result.verdict), verdict_style),
    ];

    if let Some((text, style)) = badge {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(text, style));
    }

    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(
        meta,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    ));

    Line::from(spans)
}

fn disposition_badge(result: &CheckResult) -> Option<(String, Style)> {
    let disposition = result.disposition()?;
    match disposition {
        Disposition::Active => None,
        Disposition::Parked(_) => {
            Some((disposition.to_string(), Style::default().fg(Color::Magenta)))
        }
        Disposition::Expiring(_) => {
            let text = match result
                .record
                .as_ref()
                .and_then(|record| record.expires_on.as_deref())
            {
                Some(expiration) => format!("{disposition} (exp {expiration})"),
                None => disposition.to_string(),
            };
            Some((
                text,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ))
        }
    }
}

fn trace_line(result: &CheckResult, width: usize) -> Option<Line<'static>> {
    if result.attempts.is_empty() {
        return None;
    }

    let summary = result
        .attempts
        .iter()
        .map(|attempt| {
            format!(
                "{} {} {}ms {}",
                attempt.source, attempt.status, attempt.ms, attempt.evidence
            )
        })
        .collect::<Vec<_>>()
        .join(" -> ");
    let text = truncate_width(&format!("  trace: {summary}"), width);

    Some(Line::styled(
        text,
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    ))
}

fn truncate_width(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }

    if max_width < 4 {
        return ".".repeat(max_width);
    }

    let marker = "...";
    let target = max_width.saturating_sub(UnicodeWidthStr::width(marker));
    let mut out = String::new();
    let mut width = 0;

    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > target {
            break;
        }
        out.push(ch);
        width += ch_width;
    }

    out.push_str(marker);
    out
}

