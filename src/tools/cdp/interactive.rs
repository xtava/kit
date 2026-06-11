//! `kit cdp -i` — the live interactive debugger. One screen: a streaming Timeline pane on top, a
//! command line on the bottom. The Timeline arrives over a [`Frame`] subscription to the warm
//! Attachment; typed commands run against that same Attachment and their output lands inline on the
//! feed, so an `eval` and the network calls it triggers sit next to each other on one clock.
//!
//! Two input grammars meet at the prompt: **session commands** (`eval`, `snap`, `tail`, …) are the
//! exact clap grammar the CLI uses, routed through [`super::session_command`]; **meta commands**
//! (`target`, `track`, `source`, `clear`, `help`, `quit`) are interactive-only view state.
//!
//! **Focus** is the central idea: picking a target (with `Tab`, the fuzzy picker) both filters the
//! feed to that target *and* makes it the default `--target` for commands — the Chrome DevTools
//! "context" model. `Tab` opens the picker; `target <text>` sets it directly; `target main` clears.

use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use ratatui::Frame as TuiFrame;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time;

use crate::cdp::{Source, TargetKind, TimelineEvent, TrackKind};
use crate::tui::{fuzzy, EventReader, LineEditor, Session};

use super::client;
use super::protocol::{Command, Frame, Reply, TargetActivity};
use super::registry::Record;

/// How much recent Timeline to request when the session opens.
const BACKFILL_MS: u64 = 30_000;
/// Idle redraw cadence — live frames and keystrokes drive most redraws; this is the floor.
const REDRAW: time::Duration = time::Duration::from_secs(1);
/// Cap on retained feed items. Past this the oldest are dropped and a scrolled view re-pins.
const FEED_CAP: usize = 10_000;
/// Cap on persisted history lines.
const HISTORY_CAP: usize = 1_000;
/// Columns panned per Shift+Left/Right.
const PAN_STEP: u16 = 12;

pub async fn run(app: Option<&str>) -> Result<()> {
    let record = client::ensure_attached(app).await?;
    let mut frames = client::subscribe(&record, BACKFILL_MS).await?;
    let (async_tx, mut async_rx) = mpsc::unbounded_channel::<Async>();

    let mut session = Session::open()?;
    let mut events = EventReader::start();
    let mut repl = Repl::new(record);
    let mut redraw = time::interval(REDRAW);

    loop {
        session.draw(|frame| render(frame, &mut repl))?;

        tokio::select! {
            _ = redraw.tick() => {}
            frame = frames.recv() => match frame {
                Some(frame) => repl.ingest(frame),
                None => repl.disconnect(),
            },
            Some(message) = async_rx.recv() => repl.on_async(message),
            event = events.recv() => match event {
                Some(Event::Key(key)) if key.is_press() => match repl.on_key(key, &async_tx) {
                    Flow::Quit => break,
                    Flow::Continue => {}
                },
                None => break,
                _ => {}
            },
        }

        if let Some(text) = repl.take_pending_copy() {
            let outcome = session.copy(&text).map(|()| text.lines().count());
            repl.copied(outcome);
        }
    }

    repl.save_history();
    Ok(())
}

enum Flow {
    Continue,
    Quit,
}

/// Work that finished off the UI thread and came back over the channel.
enum Async {
    Command(CommandResult),
    Targets(Result<Vec<TargetEntry>>),
}

struct CommandResult {
    label: String,
    outcome: Result<Reply>,
}

/// One row of the feed. Events stream in; Blocks are command output; Notices are session messages.
enum FeedItem {
    Event(TimelineEvent),
    Block { label: String, body: String, ok: bool },
    Notice(String),
}

struct Repl {
    record: Record,
    /// The focused target: feed filter *and* default `--target` for commands. `None` = all targets,
    /// commands fall back to the main window.
    target: Option<String>,
    /// Live-pane view filters (client-side; toggled instantly, no re-subscribe).
    view_tracks: Option<Vec<TrackKind>>,
    view_source: Option<Source>,
    feed: Vec<FeedItem>,
    /// Absolute top line of the viewport. `None` = pinned to the bottom (following live).
    view_top: Option<usize>,
    /// Horizontal pan, in columns — for reading lines wider than the pane (long ws frames, urls).
    view_left: u16,
    /// Rendered feed geometry from the last frame — what scrolling needs to clamp against.
    feed_height: usize,
    feed_total: usize,
    input: LineEditor,
    history: Vec<String>,
    history_pos: Option<usize>,
    draft: String,
    help_open: bool,
    picker: Option<Picker>,
    connected: bool,
    history_path: Option<PathBuf>,
    /// A clipboard yank awaiting the event loop, which owns the terminal the OSC 52 write goes to.
    pending_copy: Option<String>,
}

impl Repl {
    fn new(record: Record) -> Self {
        let history_path = history_path();
        let history = history_path.as_ref().map(load_history).unwrap_or_default();
        Self {
            record,
            target: None,
            view_tracks: None,
            view_source: None,
            feed: Vec::new(),
            view_top: None,
            view_left: 0,
            feed_height: 0,
            feed_total: 0,
            input: LineEditor::default(),
            history,
            history_pos: None,
            draft: String::new(),
            help_open: false,
            picker: None,
            connected: true,
            history_path,
            pending_copy: None,
        }
    }

    // --- inbound: stream + async results ---

    fn ingest(&mut self, frame: Frame) {
        match frame {
            Frame::Backfill(events) => {
                events.into_iter().for_each(|event| self.push(FeedItem::Event(event)))
            }
            Frame::Event(event) => self.push(FeedItem::Event(event)),
        }
    }

    fn on_async(&mut self, message: Async) {
        match message {
            Async::Command(result) => self.push_result(result),
            Async::Targets(result) => self.populate_picker(result),
        }
    }

    fn push_result(&mut self, result: CommandResult) {
        let (body, ok) = match result.outcome {
            Ok(reply) => (reply.output, reply.ok),
            Err(error) => (error.to_string(), false),
        };
        self.push(FeedItem::Block { label: result.label, body, ok });
    }

    fn disconnect(&mut self) {
        self.connected = false;
        self.notice("subscription closed — the daemon went away".to_owned());
    }

    fn push(&mut self, item: FeedItem) {
        self.feed.push(item);
        if self.feed.len() > FEED_CAP {
            let overflow = self.feed.len() - FEED_CAP;
            self.feed.drain(0..overflow);
            self.view_top = None;
        }
    }

    fn notice(&mut self, text: String) {
        self.push(FeedItem::Notice(text));
    }

    // --- keys ---

    fn on_key(&mut self, key: KeyEvent, async_tx: &UnboundedSender<Async>) -> Flow {
        if self.picker.is_some() {
            return self.picker_key(key);
        }
        if self.help_open {
            self.help_open = false;
            return Flow::Continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') if self.input.value().is_empty() => Flow::Quit,
                KeyCode::Char('c') | KeyCode::Char('u') => {
                    self.input.clear();
                    Flow::Continue
                }
                KeyCode::Char('d') => Flow::Quit,
                KeyCode::Char('l') => {
                    self.feed.clear();
                    self.view_top = None;
                    Flow::Continue
                }
                // Command history — the readline pairing, kept off the arrows so it never contends
                // with scrolling the feed.
                KeyCode::Char('p') => {
                    self.history_prev();
                    Flow::Continue
                }
                KeyCode::Char('n') => {
                    self.history_next();
                    Flow::Continue
                }
                _ => Flow::Continue,
            };
        }
        // While scrolled (reviewing history), `c`/`y` yank the view — they can't shadow typing here
        // because the prompt isn't in compose mode. Pinned to live, they're ordinary characters.
        if self.view_top.is_some() && matches!(key.code, KeyCode::Char('c' | 'y')) {
            self.copy_view();
            return Flow::Continue;
        }
        match key.code {
            KeyCode::Enter => return self.submit(async_tx),
            // Tab on an empty prompt opens the target picker; (completion-while-typing is deferred).
            KeyCode::Tab if self.input.value().is_empty() => self.open_picker(async_tx),
            // The arrows drive the feed — ↑/↓ scroll (the first press from live enters the scrolled
            // state), ←/→ pan wide lines once scrolled. Command history is on Ctrl+P/N (below), the
            // readline pairing, so it never contends with scrolling.
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::Left if self.view_top.is_some() => {
                self.view_left = self.view_left.saturating_sub(PAN_STEP)
            }
            KeyCode::Right if self.view_top.is_some() => {
                self.view_left = self.view_left.saturating_add(PAN_STEP)
            }
            KeyCode::PageUp => self.page_up(),
            KeyCode::PageDown => self.page_down(),
            // Back to the home view: pinned to live, panned fully left.
            KeyCode::End | KeyCode::Esc => {
                self.view_top = None;
                self.view_left = 0;
            }
            _ => self.input.apply_key(key),
        }
        Flow::Continue
    }

    fn submit(&mut self, async_tx: &UnboundedSender<Async>) -> Flow {
        let line = self.input.value().trim().to_owned();
        self.input.clear();
        self.history_pos = None;
        if line.is_empty() {
            return Flow::Continue;
        }
        self.remember(&line);

        match parse_input(&line) {
            Input::Empty => {}
            Input::Meta(Meta::Quit) => return Flow::Quit,
            Input::Meta(Meta::PickTarget) => self.open_picker(async_tx),
            Input::Meta(meta) => self.apply_meta(meta),
            Input::Session(command) => self.run_command(command, &line, async_tx),
            Input::Error(message) => {
                self.push(FeedItem::Block { label: line, body: message, ok: false });
            }
        }
        Flow::Continue
    }

    fn apply_meta(&mut self, meta: Meta) {
        match meta {
            Meta::Target(target) => self.set_target(target),
            Meta::Track(tracks) => {
                let shown = describe_tracks(&tracks);
                self.view_tracks = tracks;
                self.notice(format!("tracks → {shown}"));
            }
            Meta::Source(source) => {
                self.view_source = source;
                self.notice(format!("source → {}", describe_source(source)));
            }
            Meta::Clear => {
                self.feed.clear();
                self.view_top = None;
            }
            Meta::Help => self.help_open = true,
            Meta::Quit | Meta::PickTarget => {}
        }
    }

    fn set_target(&mut self, target: Option<String>) {
        let shown = target.clone().unwrap_or_else(|| "all targets".to_owned());
        self.target = target;
        self.notice(format!("focus → {shown}"));
    }

    fn run_command(
        &mut self,
        mut command: Command,
        label: &str,
        async_tx: &UnboundedSender<Async>,
    ) {
        apply_target(&mut command, &self.target);
        let record = self.record.clone();
        let label = label.to_owned();
        let async_tx = async_tx.clone();
        tokio::spawn(async move {
            let outcome = client::run_one(&record, command, false).await;
            let _ = async_tx.send(Async::Command(CommandResult { label, outcome }));
        });
    }

    // --- target picker ---

    fn open_picker(&mut self, async_tx: &UnboundedSender<Async>) {
        self.picker = Some(Picker::loading());
        let record = self.record.clone();
        let async_tx = async_tx.clone();
        tokio::spawn(async move {
            let _ = async_tx.send(Async::Targets(fetch_targets(&record).await));
        });
    }

    fn populate_picker(&mut self, result: Result<Vec<TargetEntry>>) {
        let current = self.target.clone();
        let Some(picker) = &mut self.picker else {
            return;
        };
        match result {
            Ok(entries) => picker.populate(entries, current),
            Err(error) => {
                self.picker = None;
                self.notice(format!("could not list targets: {error}"));
            }
        }
    }

    fn picker_key(&mut self, key: KeyEvent) -> Flow {
        let Some(mut picker) = self.picker.take() else {
            return Flow::Continue;
        };
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let keep = match key.code {
            KeyCode::Esc => false,
            KeyCode::Char('c') if control => false,
            KeyCode::Enter => {
                self.apply_choice(picker.choice());
                false
            }
            KeyCode::Up => {
                picker.move_selection(-1);
                true
            }
            KeyCode::Down => {
                picker.move_selection(1);
                true
            }
            KeyCode::Char('p' | 'k') if control => {
                picker.move_selection(-1);
                true
            }
            KeyCode::Char('n' | 'j') if control => {
                picker.move_selection(1);
                true
            }
            _ => {
                picker.input.apply_key(key);
                picker.refilter();
                true
            }
        };
        if keep {
            self.picker = Some(picker);
        }
        Flow::Continue
    }

    fn apply_choice(&mut self, choice: Choice) {
        match choice {
            Choice::Keep => {}
            Choice::All => self.set_target(None),
            Choice::Target(label) => self.set_target(Some(label)),
        }
    }

    // --- history ---

    fn remember(&mut self, line: &str) {
        if self.history.last().map(String::as_str) == Some(line) {
            return;
        }
        self.history.push(line.to_owned());
        if self.history.len() > HISTORY_CAP {
            let overflow = self.history.len() - HISTORY_CAP;
            self.history.drain(0..overflow);
        }
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let position = match self.history_pos {
            None => {
                self.draft = self.input.value().to_owned();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(position) => position - 1,
        };
        self.history_pos = Some(position);
        self.input.set(self.history[position].clone());
    }

    fn history_next(&mut self) {
        let Some(position) = self.history_pos else {
            return;
        };
        if position + 1 >= self.history.len() {
            self.history_pos = None;
            self.input.set(std::mem::take(&mut self.draft));
        } else {
            self.history_pos = Some(position + 1);
            self.input.set(self.history[position + 1].clone());
        }
    }

    fn save_history(&self) {
        let Some(path) = &self.history_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, self.history.join("\n"));
    }

    // --- scroll + view ---

    /// The topmost line index when pinned to the bottom — the anchor scrolling moves away from.
    fn max_top(&self) -> usize {
        self.feed_total.saturating_sub(self.feed_height)
    }

    fn page_up(&mut self) {
        self.scroll_by(-(self.feed_height.max(1) as isize));
    }

    fn page_down(&mut self) {
        self.scroll_by(self.feed_height.max(1) as isize);
    }

    /// Move the viewport by `delta` lines (negative = toward older). Reaching the bottom re-pins so
    /// the feed follows live again; `None` view_top always means pinned.
    fn scroll_by(&mut self, delta: isize) {
        self.view_top = scrolled(self.view_top, self.max_top(), delta);
    }

    /// Queue a yank of the current timeline view — exactly the lines on screen, as plain text. The
    /// event loop performs the OSC 52 write (it owns the terminal) and reports the outcome.
    fn copy_view(&mut self) {
        let text = self
            .visible_lines()
            .iter()
            .map(|line| line.spans.iter().map(|span| span.content.as_ref()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        self.pending_copy = Some(text);
    }

    /// Hand the queued yank to the caller (the event loop), clearing it.
    fn take_pending_copy(&mut self) -> Option<String> {
        self.pending_copy.take()
    }

    /// Report a completed yank back onto the feed.
    fn copied(&mut self, result: Result<usize>) {
        match result {
            Ok(lines) => self.notice(format!("copied {lines} line(s) to clipboard")),
            Err(error) => self.notice(format!("clipboard write failed: {error}")),
        }
    }

    fn visible_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for item in &self.feed {
            match item {
                FeedItem::Event(event) => {
                    if self.event_visible(event) {
                        lines.push(event_line(event));
                    }
                }
                FeedItem::Block { label, body, ok } => block_lines(label, body, *ok, &mut lines),
                FeedItem::Notice(text) => lines.push(notice_line(text)),
            }
        }
        lines
    }

    fn event_visible(&self, event: &TimelineEvent) -> bool {
        self.view_source.is_none_or(|source| event.source == source)
            && self.view_tracks.as_ref().is_none_or(|tracks| tracks.contains(&event.track.kind()))
            && self.target.as_ref().is_none_or(|focus| label_matches(&event.target, focus))
    }
}

/// New viewport top after moving `delta` lines from `view_top` (`None` = pinned at `max_top`).
/// Clamps to `[0, max_top]` and returns `None` once the move reaches the bottom, re-pinning to live.
fn scrolled(view_top: Option<usize>, max_top: usize, delta: isize) -> Option<usize> {
    let current = view_top.unwrap_or(max_top) as isize;
    let next = (current + delta).clamp(0, max_top as isize) as usize;
    (next < max_top).then_some(next)
}

/// Fill a command's `--target` with the focused target when the line didn't specify one.
fn apply_target(command: &mut Command, target: &Option<String>) {
    let Some(default) = target else {
        return;
    };
    let slot = match command {
        Command::Eval { target, .. }
        | Command::Navigate { target, .. }
        | Command::Ready { target }
        | Command::Heap { target }
        | Command::Snap { target, .. }
        | Command::Click { target, .. }
        | Command::Fill { target, .. }
        | Command::Lens { target, .. }
        | Command::ExtensionBundle { target, .. } => target,
        Command::Tail(query) | Command::Brief { query, .. } | Command::Errors { query, .. } => {
            &mut query.target
        }
        _ => return,
    };
    if slot.is_none() {
        *slot = Some(default.clone());
    }
}

/// Whether a focus selector applies to an event's target label — case-insensitive substring, the
/// same spirit as the daemon's `Target::matches`, so the feed filter and command resolution agree.
fn label_matches(label: &str, focus: &str) -> bool {
    label.to_lowercase().contains(&focus.to_lowercase())
}

// --- the fuzzy target picker ---

/// One pickable target, with the event volume the daemon measured over its full Timeline.
struct TargetEntry {
    /// Title-or-url — both the display name and the selector a command/feed-filter uses.
    label: String,
    kind: TargetKind,
    url: String,
    events: usize,
    extension_id: Option<String>,
    purpose: Option<String>,
}

impl TargetEntry {
    fn from_activity(activity: TargetActivity) -> Self {
        Self {
            label: activity.label,
            kind: activity.kind,
            url: activity.url,
            events: activity.events,
            extension_id: activity.extension_id,
            purpose: activity.purpose,
        }
    }

    fn is_active(&self) -> bool {
        self.events > 0
    }

    fn score(&self, needle: &str) -> Option<u16> {
        [
            Some(self.label.as_str()),
            Some(self.url.as_str()),
            Some(self.kind.as_str()),
            self.extension_id.as_deref(),
            self.purpose.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter_map(|field| fuzzy::score_ci(field, needle))
        .min()
    }
}

/// What selecting a row means.
enum Choice {
    Keep,
    All,
    Target(String),
}

struct Picker {
    input: LineEditor,
    /// `None` = the synthetic "all targets" row; `Some` = a real target.
    entries: Vec<Option<TargetEntry>>,
    filtered: Vec<usize>,
    selected: usize,
    current: Option<String>,
    idle: usize,
    loading: bool,
}

impl Picker {
    fn loading() -> Self {
        Self {
            input: LineEditor::default(),
            entries: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            current: None,
            idle: 0,
            loading: true,
        }
    }

    /// Daemon hands entries pre-ranked active-first; we keep that order and just prepend the "all" row.
    fn populate(&mut self, entries: Vec<TargetEntry>, current: Option<String>) {
        self.idle = entries.iter().filter(|entry| !entry.is_active()).count();
        self.entries = std::iter::once(None).chain(entries.into_iter().map(Some)).collect();
        self.current = current;
        self.loading = false;
        self.selected = 0;
        self.refilter();
    }

    fn refilter(&mut self) {
        let needle = self.input.value().trim().to_owned();
        if needle.is_empty() {
            // Default view: the "all" row plus only targets actually streaming. The idle majority
            // (dead service workers, silent webviews) stays out of the way until you search.
            self.filtered = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.as_ref().is_none_or(TargetEntry::is_active))
                .map(|(index, _)| index)
                .collect();
        } else {
            // Searching reaches every target, active or not, so any view is findable by name.
            let mut scored: Vec<(u16, usize)> = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    entry
                        .as_ref()
                        .and_then(|entry| entry.score(&needle))
                        .map(|score| (score, index))
                })
                .collect();
            scored.sort_by_key(|(score, index)| (*score, *index));
            self.filtered = scored.into_iter().map(|(_, index)| index).collect();
        }
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let bound = self.filtered.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, bound) as usize;
    }

    fn choice(&self) -> Choice {
        match self.filtered.get(self.selected).map(|&index| &self.entries[index]) {
            None => Choice::Keep,
            Some(None) => Choice::All,
            Some(Some(entry)) => Choice::Target(entry.label.clone()),
        }
    }
}

async fn fetch_targets(record: &Record) -> Result<Vec<TargetEntry>> {
    let reply = client::run_one(record, Command::TargetList, false).await?;
    let activity: Vec<TargetActivity> = serde_json::from_str(&reply.output)?;
    Ok(activity.into_iter().map(TargetEntry::from_activity).collect())
}

// --- line parsing ---

enum Input {
    Empty,
    Meta(Meta),
    Session(Command),
    Error(String),
}

enum Meta {
    Target(Option<String>),
    PickTarget,
    Track(Option<Vec<TrackKind>>),
    Source(Option<Source>),
    Clear,
    Help,
    Quit,
}

fn parse_input(line: &str) -> Input {
    let tokens = match shell_words::split(line.trim()) {
        Ok(tokens) if !tokens.is_empty() => tokens,
        Ok(_) => return Input::Empty,
        Err(error) => return Input::Error(format!("unbalanced quotes: {error}")),
    };
    let rest = &tokens[1..];

    match tokens[0].as_str() {
        "quit" | "exit" => Input::Meta(Meta::Quit),
        "clear" => Input::Meta(Meta::Clear),
        "help" | "?" => Input::Meta(Meta::Help),
        "target" => Input::Meta(parse_target(rest)),
        "track" => match parse_track_filter(rest) {
            Ok(tracks) => Input::Meta(Meta::Track(tracks)),
            Err(error) => Input::Error(error),
        },
        "source" => match parse_source_filter(rest) {
            Ok(source) => Input::Meta(Meta::Source(source)),
            Err(error) => Input::Error(error),
        },
        _ => match super::flow::parse_session_tokens(&tokens) {
            Ok(command) => Input::Session(command),
            Err(error) => Input::Error(error.to_string()),
        },
    }
}

/// `target` (bare) opens the picker; `target main`/`*` clears focus; `target <text>` sets it.
fn parse_target(rest: &[String]) -> Meta {
    match rest.first().map(String::as_str) {
        None => Meta::PickTarget,
        Some("main") | Some("*") | Some("all") | Some("default") => Meta::Target(None),
        Some(_) => Meta::Target(Some(rest.join(" "))),
    }
}

fn parse_track_filter(rest: &[String]) -> Result<Option<Vec<TrackKind>>, String> {
    let joined = rest.join(",");
    if joined.is_empty() || joined == "all" {
        return Ok(None);
    }
    let mut tracks = Vec::new();
    for name in joined.split(',').filter(|name| !name.is_empty()) {
        match TrackKind::parse(name) {
            Some(track) => tracks.push(track),
            None => {
                return Err(format!(
                    "unknown track '{name}' — console, exception, log, network, ws, lifecycle"
                ))
            }
        }
    }
    Ok(Some(tracks))
}

fn parse_source_filter(rest: &[String]) -> Result<Option<Source>, String> {
    match rest.first().map(String::as_str) {
        None | Some("all") | Some("*") => Ok(None),
        Some(name) => Source::parse(name)
            .map(Some)
            .ok_or_else(|| format!("unknown source '{name}' — main, renderer, or all")),
    }
}

// --- rendering ---

fn render(frame: &mut TuiFrame, repl: &mut Repl) {
    let area = frame.area();
    let chunks =
        Layout::vertical([Constraint::Length(3), Constraint::Min(1), Constraint::Length(1)])
            .split(area);

    render_header(frame, chunks[0], repl);
    let lines = repl.visible_lines();
    repl.feed_height = chunks[1].height.saturating_sub(2) as usize;
    repl.feed_total = lines.len();
    render_feed(frame, chunks[1], repl, lines);
    render_input(frame, chunks[2], repl);

    if repl.help_open {
        render_help(frame, area);
    }
    if let Some(picker) = &repl.picker {
        render_picker(frame, area, picker);
    }
}

fn render_header(frame: &mut TuiFrame, area: Rect, repl: &Repl) {
    let record = &repl.record;
    let identity = format!("{}  {}  :{}", record.name, record.app, record.port);
    let filters = format!(
        "track: {}   ·   source: {}   ·   focus: {}",
        describe_tracks(&repl.view_tracks),
        describe_source(repl.view_source),
        repl.target.as_deref().unwrap_or("all targets"),
    );

    let dot = if repl.connected {
        Span::styled("● live", Style::default().fg(Color::Green))
    } else {
        Span::styled("○ offline", Style::default().fg(Color::Red))
    };

    let body = vec![
        Line::from(vec![
            Span::styled(identity, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            dot,
        ]),
        Line::from(Span::styled(filters, Style::default().fg(Color::DarkGray))),
    ];
    frame.render_widget(Paragraph::new(body).block(panel(" kit cdp ")), area);
}

fn render_feed(frame: &mut TuiFrame, area: Rect, repl: &Repl, lines: Vec<Line<'static>>) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let max_top = lines.len().saturating_sub(inner_height);
    let top = repl.view_top.map_or(max_top, |top| top.min(max_top));
    let below = max_top - top;

    let vertical = if below == 0 { "● live".to_owned() } else { format!("▲ {below} below") };
    let pan =
        if repl.view_left > 0 { format!(" ─ → {}", repl.view_left) } else { String::new() };
    let title = format!(" timeline ─ {vertical}{pan} ");

    let feed =
        Paragraph::new(lines).block(panel_titled(title)).scroll((top as u16, repl.view_left));
    frame.render_widget(feed, area);
}

fn render_input(frame: &mut TuiFrame, area: Rect, repl: &Repl) {
    let line = Line::from(vec![
        Span::styled("cdp› ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(repl.input.value().to_owned()),
    ]);
    frame.render_widget(Paragraph::new(line), area);

    if repl.picker.is_none() && !repl.help_open {
        let cursor_x =
            area.x + 5 + repl.input.value()[..repl.input.cursor()].chars().count() as u16;
        frame.set_cursor_position((cursor_x, area.y));
    }
}

fn render_picker(frame: &mut TuiFrame, area: Rect, picker: &Picker) {
    let height = (picker.filtered.len() as u16 + 4).min(area.height).max(5);
    let popup = centered(area, 72, height);
    frame.render_widget(Clear, popup);

    let rows = popup.height.saturating_sub(3) as usize;
    let start = picker.selected.saturating_sub(rows.saturating_sub(1));
    let mut lines = vec![Line::from(vec![
        Span::styled("› ", Style::default().fg(Color::Cyan)),
        Span::raw(picker.input.value().to_owned()),
        Span::styled(
            if picker.loading { "  (loading targets…)" } else { "" },
            Style::default().fg(Color::DarkGray),
        ),
    ])];

    for (offset, &entry_index) in picker.filtered.iter().enumerate().skip(start).take(rows) {
        let active = offset == picker.selected;
        lines.push(picker_row(&picker.entries[entry_index], &picker.current, active));
    }

    if picker.input.value().is_empty() && picker.idle > 0 {
        lines.push(Line::from(Span::styled(
            format!("  {} idle target(s) hidden — type to search", picker.idle),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        )));
    }

    let streaming =
        picker.filtered.iter().filter(|&&index| picker.entries[index].is_some()).count();
    let title = format!(" pick target ─ {streaming} streaming ");
    frame.render_widget(Paragraph::new(lines).block(panel_titled(title)), popup);
}

fn picker_row(
    entry: &Option<TargetEntry>,
    current: &Option<String>,
    active: bool,
) -> Line<'static> {
    let marker = if active { "▌ " } else { "  " };
    let base = if active {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };

    let Some(entry) = entry else {
        return Line::from(vec![
            Span::styled(marker, Style::default().fg(Color::Cyan)),
            Span::styled("✸ all targets", base),
        ]);
    };

    let focused = current.as_ref().is_some_and(|focus| label_matches(&entry.label, focus));
    let dot = if focused { "● " } else { "  " };
    let meta = entry
        .extension_id
        .as_deref()
        .or(entry.purpose.as_deref())
        .map(|value| format!("  {}", truncate(value, 28)))
        .unwrap_or_default();
    Line::from(vec![
        Span::styled(marker, Style::default().fg(Color::Cyan)),
        Span::styled(dot, Style::default().fg(Color::Green)),
        Span::styled(format!("{:<9}", entry.kind.as_str()), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{:<40}", truncate(&entry.label, 40)), base),
        Span::styled(format!("{:>8}", compact(entry.events)), Style::default().fg(Color::Cyan)),
        Span::styled(meta, Style::default().fg(Color::DarkGray)),
    ])
}

fn render_help(frame: &mut TuiFrame, area: Rect) {
    let body = vec![
        section("OBSERVE"),
        entry("tail [--since 5s] [--track ..] [--source ..]", "slice the live feed"),
        entry("brief [--since 30s]", "agent-safe compact timeline"),
        entry("console · net · ws [--grep ..] [--extension ..]", "filtered slices"),
        entry("errors [--explain]", "what's broken — deduped, never silently lossy"),
        section("PROBE"),
        entry("eval <expr> · heap · targets · snap [-i]", "one-shot queries"),
        entry("ready", "is the workbench up? selected target + why"),
        section("EXTENSIONS"),
        entry("lens extensions -- <id>", "runtime graph + webview target diagnosis"),
        entry("ext doctor <id> · ext bundle <id>", "diagnosis plus bounded timeline"),
        section("INTERACT"),
        entry("click <loc> · fill <loc> <text>", "locators: @ref · button:Save · 'bare name'"),
        entry(
            "press <chord> · select <loc> <option>",
            "keys to the focused element · pick an option",
        ),
        section("VERIFY"),
        entry(
            "wait '<expr>' · expect text/eval/net/no-errors",
            "poll a condition · assert one fact",
        ),
        entry("verify · snap --diff", "PASS/FAIL since last action · what changed on screen"),
        section("BATCH & SUBSCRIBE"),
        entry("do \"<step>; <step>\" · flow run <name> [k=v]", "whole sequences, one round trip"),
        entry("watch add <name> '<expr>' · ls · rm · clear", "value changes land on the feed live"),
        section("FOCUS & FILTER"),
        entry("Tab  ·  target [<text>|main]", "pick / set / clear the focused target"),
        entry("track <list> | all", "filter the live pane by track"),
        entry("source main | renderer | all", "filter the live pane by side"),
        entry("ignore <substr> · clear · help · quit", "noise, this help, exit"),
        section("MOVE & COPY"),
        entry("↑↓ · PgUp/PgDn", "scroll the timeline · End re-pins to live"),
        entry("←→", "pan to read lines wider than the pane"),
        entry("^P · ^N", "previous / next command in history"),
        entry("c · y  (while reviewing)", "yank the timeline view to the clipboard"),
        entry("drag to select", "the mouse is free — copy any line natively"),
        Line::from(""),
        Line::from(Span::styled("  any key to dismiss", Style::default().fg(Color::DarkGray))),
    ];

    let popup = centered(area, 64, body.len() as u16 + 2);
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(body).block(panel(" kit cdp · interactive ")), popup);
}

fn event_line(event: &TimelineEvent) -> Line<'static> {
    let text = super::format::event_line(event);
    Line::from(Span::styled(text, Style::default().fg(track_color(event.track.kind()))))
}

fn block_lines(label: &str, body: &str, ok: bool, out: &mut Vec<Line<'static>>) {
    let marker = if ok { "└" } else { "✗" };
    let head_color = if ok { Color::Yellow } else { Color::Red };
    out.push(Line::from(Span::styled(
        format!("┌ {label}"),
        Style::default().fg(head_color).add_modifier(Modifier::BOLD),
    )));
    for body_line in body.lines() {
        out.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(Color::DarkGray)),
            Span::styled(body_line.to_owned(), Style::default().fg(Color::Gray)),
        ]));
    }
    out.push(Line::from(Span::styled(marker.to_owned(), Style::default().fg(head_color))));
}

fn notice_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("· {text}"),
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
    ))
}

fn track_color(kind: TrackKind) -> Color {
    match kind {
        TrackKind::Console => Color::Gray,
        TrackKind::Exception => Color::Red,
        TrackKind::Log => Color::Blue,
        TrackKind::Network => Color::Cyan,
        TrackKind::Ws => Color::Magenta,
        TrackKind::Lifecycle => Color::Yellow,
        TrackKind::Watch => Color::Green,
    }
}

fn describe_tracks(tracks: &Option<Vec<TrackKind>>) -> String {
    match tracks {
        None => "all".to_owned(),
        Some(tracks) if tracks.is_empty() => "none".to_owned(),
        Some(tracks) => tracks.iter().map(|track| track.as_str()).collect::<Vec<_>>().join(","),
    }
}

fn describe_source(source: Option<Source>) -> &'static str {
    match source {
        None => "all",
        Some(Source::Main) => "main",
        Some(Source::Renderer) => "renderer",
    }
}

fn section(title: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {title}"),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ))
}

fn entry(command: &'static str, description: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("    {command:<44}"), Style::default().fg(Color::White)),
        Span::styled(description, Style::default().fg(Color::DarkGray)),
    ])
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn compact(count: usize) -> String {
    match count {
        0 => String::new(),
        n if n < 1000 => n.to_string(),
        n => format!("{:.1}k", n as f64 / 1000.0),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn panel(title: &'static str) -> Block<'static> {
    base_panel().title(title)
}

fn panel_titled(title: String) -> Block<'static> {
    base_panel().title(title)
}

fn base_panel() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::horizontal(1))
}

// --- history persistence ---

fn history_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "kit").map(|dirs| dirs.config_dir().join("cdp/history"))
}

fn load_history(path: &PathBuf) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|raw| raw.lines().map(str::to_owned).filter(|line| !line.is_empty()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tools::cdp::protocol;

    fn session(line: &str) -> Command {
        match parse_input(line) {
            Input::Session(command) => command,
            other => panic!("expected session command, got {}", label(&other)),
        }
    }

    fn label(input: &Input) -> &'static str {
        match input {
            Input::Empty => "empty",
            Input::Meta(_) => "meta",
            Input::Session(_) => "session",
            Input::Error(_) => "error",
        }
    }

    /// The interactive prompt speaks the whole verification grammar — a typed `do`, `watch`,
    /// `verify`, or `expect` is the same wire command the CLI sends.
    #[test]
    fn verification_grammar_parses_at_the_prompt() {
        assert!(matches!(
            session("click 'button:Save settings'"),
            Command::Click { locator: protocol::Locator::Query { .. }, settle: Some(_), .. }
        ));
        assert!(matches!(session("verify"), Command::Verify { window: None, .. }));
        assert!(matches!(
            session("expect text 'Saved'"),
            Command::Expect { expectation: protocol::Expectation::Text { .. }, .. }
        ));
        assert!(matches!(
            session("wait 'window.ready' --timeout 10s"),
            Command::WaitFor { timeout_ms: 10_000, .. }
        ));
        assert!(matches!(
            session("watch add cart 'cart.length'"),
            Command::Watch(protocol::WatchOp::Add { .. })
        ));
        assert!(matches!(session("press Meta+s"), Command::Press { .. }));
        assert!(matches!(session("snap --diff"), Command::Snap { diff: true, .. }));
        match session("do \"click 'button:Save'; verify\"") {
            Command::Do { steps } => assert_eq!(steps.len(), 2),
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn parses_eval_with_quoted_argument() {
        match session("eval 'document.title'") {
            Command::Eval { expr, target } => {
                assert_eq!(expr, "document.title");
                assert!(target.is_none());
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn target_meta_picks_sets_and_clears() {
        assert!(matches!(parse_input("target"), Input::Meta(Meta::PickTarget)));
        assert!(matches!(parse_input("target workspace"), Input::Meta(Meta::Target(Some(_)))));
        assert!(matches!(parse_input("target main"), Input::Meta(Meta::Target(None))));
    }

    #[test]
    fn meta_commands_route_before_clap() {
        assert!(matches!(parse_input("quit"), Input::Meta(Meta::Quit)));
        assert!(matches!(parse_input("clear"), Input::Meta(Meta::Clear)));
        assert!(matches!(parse_input("help"), Input::Meta(Meta::Help)));
    }

    #[test]
    fn track_filter_parses_and_rejects() {
        match parse_input("track net,ws") {
            Input::Meta(Meta::Track(Some(tracks))) => {
                assert_eq!(tracks, vec![TrackKind::Network, TrackKind::Ws]);
            }
            other => panic!("expected track meta, got {}", label(&other)),
        }
        assert!(matches!(parse_input("track all"), Input::Meta(Meta::Track(None))));
        assert!(matches!(parse_input("track bogus"), Input::Error(_)));
    }

    #[test]
    fn bad_flag_is_an_error_not_a_panic() {
        assert!(matches!(parse_input("eval --nonexistent"), Input::Error(_)));
        assert!(matches!(parse_input("eval 'unterminated"), Input::Error(_)));
    }

    #[test]
    fn sticky_target_fills_only_empty_target() {
        let sticky = Some("workspace".to_owned());

        let mut command = session("eval location.href");
        apply_target(&mut command, &sticky);
        assert!(matches!(command, Command::Eval { target: Some(ref t), .. } if t == "workspace"));

        let mut explicit = session("eval location.href --target @e1");
        apply_target(&mut explicit, &sticky);
        assert!(matches!(explicit, Command::Eval { target: Some(ref t), .. } if t == "@e1"));
    }

    #[test]
    fn focus_filters_feed_by_label_substring() {
        assert!(label_matches("mine - workspace - modular", "workspace"));
        assert!(label_matches("VSCODE-WEBVIEW://abc", "webview"));
        assert!(!label_matches("modular://background-worker", "workspace"));
    }

    #[test]
    fn scrolling_up_from_pinned_actually_moves() {
        // The bug: pinned (None) scrolled up did nothing because it clamped back to the bottom.
        assert_eq!(scrolled(None, 10, -1), Some(9));
        assert_eq!(scrolled(None, 10, -3), Some(7));
    }

    #[test]
    fn scrolling_clamps_and_repins() {
        assert_eq!(scrolled(Some(2), 10, -5), Some(0)); // clamp at the top
        assert_eq!(scrolled(Some(8), 10, 5), None); // reaching the bottom re-pins to live
        assert_eq!(scrolled(None, 0, -1), None); // nothing to scroll when it all fits
    }
}
