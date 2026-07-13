use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context as AnyhowContext, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};
use ratatui::Frame;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command as TokioCommand};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::time;
use unicode_width::UnicodeWidthStr;

use crate::tui::{fuzzy, EventReader, LineEditor, Session, SessionOptions};

use super::{
    artifacts_report, cancel_args, current_recording_dir, events_summary, normalize_repo,
    record_args, rename_current_recording, replay_args, saved_recording_root, status_report,
    stop_args,
};

const REDRAW: Duration = Duration::from_secs(1);
const FEED_CAP: usize = 5_000;
const HISTORY_CAP: usize = 500;

pub async fn run(repo: PathBuf, scenario: String) -> Result<()> {
    let mut session = Session::open(SessionOptions::default())?;
    let mut events = EventReader::start();
    let (tx, mut rx) = mpsc::unbounded_channel::<Async>();
    let mut app = App::new(repo, scenario);
    app.notice(
        "ready — start, stop, cancel, replay, status, events, artifacts, rename, help".to_owned(),
    );
    let mut redraw = time::interval(REDRAW);

    loop {
        session.draw(|frame| render(frame, &mut app))?;

        tokio::select! {
            _ = redraw.tick() => {}
            Some(message) = rx.recv() => app.on_async(message),
            event = events.recv() => match event {
                Some(Event::Key(key)) if key.is_press() => match app.on_key(key, &tx).await? {
                    Flow::Quit => break,
                    Flow::Continue => {}
                },
                None => break,
                _ => {}
            },
        }
    }

    Ok(())
}

enum Flow {
    Continue,
    Quit,
}

enum Async {
    Output { label: String, stream: Stream, line: String },
    Finished { label: String, kind: ProcessKind, result: Result<String, String> },
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcessKind {
    Recording,
    Stop,
    Cancel,
    Replay,
}

enum FeedItem {
    Notice(String),
    Block { label: String, lines: Vec<String>, ok: bool },
    Output { label: String, stream: Stream, line: String },
}

#[derive(Clone)]
struct Candidate {
    insert: String,
    hint: String,
}

struct Completion {
    start: usize,
    candidates: Vec<Candidate>,
}

struct Suggestions {
    candidates: Vec<Candidate>,
    start: usize,
    selected: Option<usize>,
}

struct Ghost {
    text: String,
    acceptable: bool,
}

struct App {
    repo: PathBuf,
    scenario: String,
    feed: Vec<FeedItem>,
    view_top: Option<usize>,
    feed_height: usize,
    feed_total: usize,
    input: LineEditor,
    history: Vec<String>,
    history_pos: Option<usize>,
    draft: String,
    help_open: bool,
    recording: bool,
    replay_stdin: Option<ChildStdin>,
    suggestions: Option<Suggestions>,
    muted_at: Option<usize>,
}

impl App {
    fn new(repo: PathBuf, scenario: String) -> Self {
        Self {
            repo,
            scenario,
            feed: Vec::new(),
            view_top: None,
            feed_height: 0,
            feed_total: 0,
            input: LineEditor::default(),
            history: Vec::new(),
            history_pos: None,
            draft: String::new(),
            help_open: false,
            recording: false,
            replay_stdin: None,
            suggestions: None,
            muted_at: None,
        }
    }

    async fn on_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Async>) -> Result<Flow> {
        if self.help_open {
            self.help_open = false;
            return Ok(Flow::Continue);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c') if self.input.value().is_empty() => {
                    self.close_active_windows().await?;
                    Ok(Flow::Quit)
                }
                KeyCode::Char('c') | KeyCode::Char('u') => {
                    self.input.clear();
                    self.refresh_suggestions();
                    Ok(Flow::Continue)
                }
                KeyCode::Char('d') => Ok(Flow::Quit),
                KeyCode::Char('l') => {
                    self.feed.clear();
                    self.view_top = None;
                    Ok(Flow::Continue)
                }
                KeyCode::Char('p') => {
                    self.history_prev();
                    self.refresh_suggestions();
                    Ok(Flow::Continue)
                }
                KeyCode::Char('n') => {
                    self.history_next();
                    self.refresh_suggestions();
                    Ok(Flow::Continue)
                }
                _ => Ok(Flow::Continue),
            };
        }

        match key.code {
            KeyCode::Enter if self.engaged() => self.accept_selected(),
            KeyCode::Enter
                if self.input.value().trim().is_empty() && self.replay_stdin.is_some() =>
            {
                self.close_replay_window().await?;
            }
            KeyCode::Enter => return self.submit(tx),
            KeyCode::Tab if self.suggestions.is_some() => self.cycle_selection(1),
            KeyCode::BackTab if self.suggestions.is_some() => self.cycle_selection(-1),
            KeyCode::Tab => {
                self.muted_at = None;
                self.refresh_suggestions();
                self.cycle_selection(1);
            }
            KeyCode::Right
                if self.input.cursor() == self.input.value().len() && self.ghost_acceptable() =>
            {
                self.accept_ghost()
            }
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-(self.feed_height.max(1) as isize)),
            KeyCode::PageDown => self.scroll_by(self.feed_height.max(1) as isize),
            KeyCode::Esc if self.engaged() => self.disengage(),
            KeyCode::Esc if self.suggestions.is_some() => self.mute_suggestions(),
            KeyCode::Esc | KeyCode::End => self.view_top = None,
            _ => {
                self.input.apply_key(key);
                self.refresh_suggestions();
            }
        }

        Ok(Flow::Continue)
    }

    fn submit(&mut self, tx: &UnboundedSender<Async>) -> Result<Flow> {
        let line = self.input.value().trim().to_owned();
        self.input.clear();
        self.suggestions = None;
        self.muted_at = None;
        self.history_pos = None;
        if line.is_empty() {
            return Ok(Flow::Continue);
        }
        self.remember(&line);

        let tokens = match shell_words::split(&line) {
            Ok(tokens) if !tokens.is_empty() => tokens,
            Ok(_) => return Ok(Flow::Continue),
            Err(error) => {
                self.block(line, vec![format!("could not parse command: {error}")], false);
                return Ok(Flow::Continue);
            }
        };

        match tokens[0].as_str() {
            "q" | "quit" | "exit" => return Ok(Flow::Quit),
            "h" | "help" | "?" => self.help_open = true,
            "clear" => {
                self.feed.clear();
                self.view_top = None;
            }
            "repo" => self.set_repo(&tokens[1..], line),
            "scenario" => self.set_scenario(&tokens[1..], line),
            "start" | "record" => self.start_recording(&tokens[1..], tx, line),
            "stop" => {
                self.spawn_command("stop", stop_args(&self.scenario), ProcessKind::Stop, false, tx)?
            }
            "cancel" => self.cancel_recording(tx, line),
            "replay" => self.start_replay(&tokens[1..], tx, line),
            "status" => self.status(line),
            "events" => self.events(line),
            "artifacts" | "files" => self.artifacts(line),
            "rename" => self.rename_recording(&tokens[1..], line),
            command => self.block(
                line,
                vec![format!("unknown command `{command}`"), "type `help` for commands".to_owned()],
                false,
            ),
        }

        Ok(Flow::Continue)
    }

    fn cancel_recording(&mut self, tx: &UnboundedSender<Async>, label: String) {
        match self.spawn_command(
            "cancel",
            cancel_args(&self.scenario),
            ProcessKind::Cancel,
            false,
            tx,
        ) {
            Ok(()) => {}
            Err(error) => self.block(label, vec![error.to_string()], false),
        }
    }

    fn set_repo(&mut self, args: &[String], label: String) {
        if args.is_empty() {
            self.block(label, vec![format!("repo: {}", self.repo.display())], true);
            return;
        }
        match normalize_repo(PathBuf::from(args.join(" "))) {
            Ok(repo) => {
                self.repo = repo;
                self.block(label, vec![format!("repo: {}", self.repo.display())], true);
            }
            Err(error) => self.block(label, vec![error.to_string()], false),
        }
    }

    fn set_scenario(&mut self, args: &[String], label: String) {
        if let Some(next) = args.first() {
            self.scenario = next.to_owned();
        }
        self.block(label, vec![format!("scenario: {}", self.scenario)], true);
    }

    fn start_recording(&mut self, args: &[String], tx: &UnboundedSender<Async>, label: String) {
        if self.recording {
            self.block(label, vec!["recording already running".to_owned()], false);
            return;
        }

        let out = match parse_out_arg(args) {
            Ok(out) => out,
            Err(error) => {
                self.block(label, vec![error.to_string()], false);
                return;
            }
        };

        let args = record_args(&self.scenario, out.as_deref());
        match self.spawn_command("record", args, ProcessKind::Recording, false, tx) {
            Ok(()) => self.recording = true,
            Err(error) => self.block(label, vec![error.to_string()], false),
        }
    }

    fn start_replay(&mut self, args: &[String], tx: &UnboundedSender<Async>, label: String) {
        if self.replay_stdin.is_some() {
            self.block(label, vec!["replay already running".to_owned()], false);
            return;
        }
        let dir = match args {
            [] => None,
            [dir] => Some(PathBuf::from(dir)),
            _ => {
                self.block(label, vec!["usage: replay [DIR]".to_owned()], false);
                return;
            }
        };

        let args = replay_args(&self.scenario, dir.as_deref());
        match self.spawn_command_with_stdin("replay", args, ProcessKind::Replay, tx) {
            Ok(stdin) => {
                self.replay_stdin = stdin;
                self.notice(
                    "replay started — when it prints its close prompt, press Enter on an empty prompt"
                        .to_owned(),
                );
            }
            Err(error) => self.block(label, vec![error.to_string()], false),
        }
    }

    fn status(&mut self, label: String) {
        match status_report(&self.repo, &self.scenario) {
            Ok(report) => {
                let mut lines = vec![format!("artifact dir: {}", report.artifact_dir.display())];
                match report.run_state {
                    Some(state) => {
                        lines.push(format!(
                            "status: {}",
                            state.status.unwrap_or_else(|| "unknown".to_owned())
                        ));
                        if let Some(pid) = state.pid {
                            lines.push(format!("pid: {pid}"));
                        }
                        if let Some(started_at) = state.started_at {
                            lines.push(format!("started: {started_at}"));
                        }
                        if let Some(finished_at) = state.finished_at {
                            lines.push(format!("finished: {finished_at}"));
                        }
                        if let Some(error) = state.error {
                            lines.push(format!("error: {error}"));
                        }
                    }
                    None => lines.push("run-state: missing".to_owned()),
                }
                self.block(label, lines, true);
            }
            Err(error) => self.block(label, vec![error.to_string()], false),
        }
    }

    fn events(&mut self, label: String) {
        match events_summary(&self.repo, &self.scenario) {
            Ok(summary) => {
                let mut lines = vec![
                    format!("events: {}", summary.path.display()),
                    format!("total: {}", summary.total),
                ];
                for (event_type, count) in summary.counts {
                    lines.push(format!("  {event_type:<16} {count}"));
                }
                self.block(label, lines, true);
            }
            Err(error) => self.block(label, vec![error.to_string()], false),
        }
    }

    fn artifacts(&mut self, label: String) {
        match artifacts_report(&self.repo, &self.scenario) {
            Ok(report) => {
                let mut lines = vec![format!("artifact dir: {}", report.dir.display())];
                if report.files.is_empty() {
                    lines.push("no files".to_owned());
                }
                for file in report.files {
                    lines.push(format!("{:>10}  {}", file.bytes, file.name));
                }
                self.block(label, lines, true);
            }
            Err(error) => self.block(label, vec![error.to_string()], false),
        }
    }

    fn rename_recording(&mut self, args: &[String], label: String) {
        let [name] = args else {
            self.block(label, vec!["usage: rename NAME".to_owned()], false);
            return;
        };

        match rename_current_recording(&self.repo, &self.scenario, name) {
            Ok(target) => self.block(
                label,
                vec![
                    format!("saved recording: {}", target.display()),
                    format!("replay it with: replay {}", target.display()),
                ],
                true,
            ),
            Err(error) => self.block(label, vec![error.to_string()], false),
        }
    }

    fn spawn_command(
        &mut self,
        label: &str,
        args: Vec<String>,
        kind: ProcessKind,
        pipe_stdin: bool,
        tx: &UnboundedSender<Async>,
    ) -> Result<()> {
        let stdin = self.spawn_command_with_stdin(label, args, kind, tx)?;
        if pipe_stdin && stdin.is_none() {
            return Err(anyhow!("failed to pipe stdin for {label}"));
        }
        Ok(())
    }

    fn spawn_command_with_stdin(
        &mut self,
        label: &str,
        args: Vec<String>,
        kind: ProcessKind,
        tx: &UnboundedSender<Async>,
    ) -> Result<Option<ChildStdin>> {
        let mut child = TokioCommand::new("pnpm")
            .current_dir(&self.repo)
            .args(args)
            .stdin(if kind == ProcessKind::Replay { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to start pnpm in {}", self.repo.display()))?;

        let stdin = child.stdin.take();
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(label.to_owned(), Stream::Stdout, stdout, tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(label.to_owned(), Stream::Stderr, stderr, tx.clone());
        }

        let tx = tx.clone();
        let label = label.to_owned();
        let notice_label = label.clone();
        tokio::spawn(async move {
            let result = child
                .wait()
                .await
                .map(|status| status.to_string())
                .map_err(|error| error.to_string());
            let _ = tx.send(Async::Finished { label, kind, result });
        });

        self.notice(format!("{notice_label} started"));
        Ok(stdin)
    }

    async fn close_replay_window(&mut self) -> Result<()> {
        let Some(stdin) = &mut self.replay_stdin else {
            return Ok(());
        };
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        self.notice("sent Enter to replay process".to_owned());
        Ok(())
    }

    async fn close_active_windows(&mut self) -> Result<()> {
        let mut closed = false;
        if self.replay_stdin.is_some() {
            self.close_replay_window().await?;
            closed = true;
        }
        if self.recording {
            run_modular_command_quiet(&self.repo, cancel_args(&self.scenario)).await?;
            self.notice("sent cancel request to recorder process".to_owned());
            closed = true;
        }
        if !closed {
            self.notice("no active recording or replay window".to_owned());
        }
        Ok(())
    }

    fn on_async(&mut self, message: Async) {
        match message {
            Async::Output { label, stream, line } => {
                self.push(FeedItem::Output { label, stream, line })
            }
            Async::Finished { label, kind, result } => {
                match kind {
                    ProcessKind::Recording => self.recording = false,
                    ProcessKind::Stop => {}
                    ProcessKind::Cancel => self.recording = false,
                    ProcessKind::Replay => self.replay_stdin = None,
                }
                match result {
                    Ok(status) => {
                        let mut lines = vec![format!("exited with {status}")];
                        if matches!(
                            kind,
                            ProcessKind::Recording | ProcessKind::Stop | ProcessKind::Cancel
                        ) {
                            lines.extend(self.recording_location_lines(kind));
                        }
                        self.block(label, lines, true);
                    }
                    Err(error) => self.block(label, vec![error], false),
                }
            }
        }
    }

    fn recording_location_lines(&self, kind: ProcessKind) -> Vec<String> {
        let location = current_recording_dir(&self.repo, &self.scenario);
        let mut lines = vec![format!("recording dir: {}", location.display())];
        if kind == ProcessKind::Stop && self.recording {
            lines.push("waiting for recorder process to flush and exit".to_owned());
        }
        if kind == ProcessKind::Cancel && self.recording {
            lines.push("waiting for recorder process to cancel and close".to_owned());
        }
        lines.push(format!("rename with: rename {}", default_recording_name(&self.scenario)));
        lines
    }

    fn refresh_suggestions(&mut self) {
        let line = self.input.value();
        if line.is_empty() {
            self.suggestions = None;
            self.muted_at = None;
            return;
        }
        let Some(found) = self.complete(line) else {
            self.suggestions = None;
            return;
        };
        if self.muted_at.is_some_and(|at| at != found.start) {
            self.muted_at = None;
        }
        if self.muted_at == Some(found.start) {
            self.suggestions = None;
            return;
        }
        let word = line[found.start..].trim_start_matches(['\'', '"']);
        if found.candidates.len() == 1 && found.candidates[0].insert == word {
            self.suggestions = None;
            return;
        }
        self.suggestions =
            Some(Suggestions { candidates: found.candidates, start: found.start, selected: None });
    }

    fn complete(&self, line: &str) -> Option<Completion> {
        let (start, word) = current_word(line);
        let tokens = shell_words::split(&line[..start]).unwrap_or_default();
        let candidates = if tokens.is_empty() {
            command_candidates()
        } else {
            match tokens[0].as_str() {
                "replay" => self.replay_candidates(),
                "scenario" => self.scenario_candidates(),
                "rename" => vec![Candidate {
                    insert: default_recording_name(&self.scenario),
                    hint: "archive current recording".to_owned(),
                }],
                "start" | "record" if word.starts_with('-') => {
                    vec![Candidate { insert: "--out".to_owned(), hint: "output dir".to_owned() }]
                }
                "repo" => vec![Candidate {
                    insert: self.repo.display().to_string(),
                    hint: "current Modular checkout".to_owned(),
                }],
                _ => Vec::new(),
            }
        };
        let ranked = rank_candidates(candidates, word.trim_start_matches(['\'', '"']));
        (!ranked.is_empty()).then_some(Completion { start, candidates: ranked })
    }

    fn replay_candidates(&self) -> Vec<Candidate> {
        let mut candidates = vec![Candidate {
            insert: current_recording_dir(&self.repo, &self.scenario).display().to_string(),
            hint: "current recording".to_owned(),
        }];
        candidates.extend(saved_recording_candidates(&self.repo));
        candidates
    }

    fn scenario_candidates(&self) -> Vec<Candidate> {
        let mut names = vec![self.scenario.clone()];
        let current_root =
            current_recording_dir(&self.repo, &self.scenario).parent().map(PathBuf::from);
        if let Some(root) = current_root {
            if let Ok(entries) = fs::read_dir(root) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|ty| ty.is_dir()) {
                        names.push(entry.file_name().to_string_lossy().into_owned());
                    }
                }
            }
        }
        names.sort();
        names.dedup();
        names.into_iter().map(|insert| Candidate { insert, hint: "scenario".to_owned() }).collect()
    }

    fn engaged(&self) -> bool {
        self.suggestions.as_ref().is_some_and(|menu| menu.selected.is_some())
    }

    fn cycle_selection(&mut self, step: isize) {
        let Some(menu) = &mut self.suggestions else {
            return;
        };
        if menu.candidates.is_empty() {
            return;
        }
        let len = menu.candidates.len() as isize;
        menu.selected = Some(match menu.selected {
            None if step > 0 => 0,
            None => (len - 1) as usize,
            Some(at) => ((at as isize + step).rem_euclid(len)) as usize,
        });
    }

    fn accept_selected(&mut self) {
        let Some(menu) = self.suggestions.take() else {
            return;
        };
        let Some(candidate) = menu.selected.and_then(|at| menu.candidates.get(at)) else {
            return;
        };
        self.replace_current_word(menu.start, &candidate.insert);
        self.refresh_suggestions();
    }

    fn disengage(&mut self) {
        if let Some(menu) = &mut self.suggestions {
            menu.selected = None;
        }
    }

    fn mute_suggestions(&mut self) {
        self.muted_at = self.suggestions.as_ref().map(|menu| menu.start);
        self.suggestions = None;
    }

    fn ghost(&self) -> Option<Ghost> {
        if self.engaged() {
            return None;
        }
        let line = self.input.value();
        if line.is_empty() {
            return None;
        }
        let (start, word) = current_word(line);
        if word.is_empty() {
            let placeholder = self.placeholder_for(line, start)?;
            return Some(Ghost { text: placeholder, acceptable: false });
        }

        let completion = self.complete(line)?;
        let needle = word.trim_start_matches(['\'', '"']);
        let top = completion
            .candidates
            .iter()
            .find(|candidate| starts_with_ci(&candidate.insert, needle))?;
        let remainder = &top.insert[needle.len()..];
        (!remainder.is_empty()).then_some(Ghost { text: remainder.to_owned(), acceptable: true })
    }

    fn ghost_acceptable(&self) -> bool {
        self.ghost().is_some_and(|ghost| ghost.acceptable)
    }

    fn accept_ghost(&mut self) {
        let Some(ghost) = self.ghost() else {
            return;
        };
        if !ghost.acceptable {
            return;
        }
        let line = self.input.value().to_owned();
        self.input.set(format!("{line}{} ", ghost.text));
        self.refresh_suggestions();
    }

    fn placeholder_for(&self, line: &str, start: usize) -> Option<String> {
        let tokens = shell_words::split(&line[..start]).unwrap_or_default();
        if tokens.is_empty() {
            return Some("‹command · ⇥ list›".to_owned());
        }
        match tokens[0].as_str() {
            "rename" => Some("‹name · ⇥ suggestion›".to_owned()),
            "replay" => Some("‹recording dir · ⇥ list›".to_owned()),
            "scenario" => Some("‹scenario · ⇥ list›".to_owned()),
            "repo" => Some("‹repo path›".to_owned()),
            "start" | "record" => Some("‹--out DIR›".to_owned()),
            _ => None,
        }
    }

    fn replace_current_word(&mut self, start: usize, insert: &str) {
        let mut line = self.input.value()[..start].to_owned();
        line.push_str(insert);
        line.push(' ');
        self.input.set(line);
    }

    fn block(&mut self, label: String, lines: Vec<String>, ok: bool) {
        self.push(FeedItem::Block { label, lines, ok });
    }

    fn notice(&mut self, text: String) {
        self.push(FeedItem::Notice(text));
    }

    fn push(&mut self, item: FeedItem) {
        self.feed.push(item);
        if self.feed.len() > FEED_CAP {
            let overflow = self.feed.len() - FEED_CAP;
            self.feed.drain(0..overflow);
            self.view_top = None;
        }
    }

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

    fn scroll_by(&mut self, delta: isize) {
        let max_top = self.feed_total.saturating_sub(self.feed_height);
        let current = self.view_top.unwrap_or(max_top) as isize;
        let next = (current + delta).clamp(0, max_top as isize) as usize;
        self.view_top = (next < max_top).then_some(next);
    }

    fn visible_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for item in &self.feed {
            match item {
                FeedItem::Notice(text) => lines.push(Line::from(Span::styled(
                    text.clone(),
                    Style::default().fg(Color::DarkGray),
                ))),
                FeedItem::Block { label, lines: body, ok } => {
                    let color = if *ok { Color::Green } else { Color::Red };
                    lines.push(Line::from(vec![
                        Span::styled("● ", Style::default().fg(color)),
                        Span::styled(label.clone(), Style::default().fg(Color::Cyan)),
                    ]));
                    for line in body {
                        lines.push(Line::from(Span::raw(format!("  {line}"))));
                    }
                }
                FeedItem::Output { label, stream, line } => {
                    let color = match stream {
                        Stream::Stdout => Color::Gray,
                        Stream::Stderr => Color::Yellow,
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("{label:<7} "), Style::default().fg(Color::DarkGray)),
                        Span::styled(line.clone(), Style::default().fg(color)),
                    ]));
                }
            }
        }
        lines
    }
}

fn spawn_reader<R>(label: String, stream: Stream, reader: R, tx: UnboundedSender<Async>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let _ = tx.send(Async::Output { label: label.clone(), stream, line });
        }
    });
}

async fn run_modular_command_quiet(repo: &PathBuf, args: Vec<String>) -> Result<()> {
    let status = TokioCommand::new("pnpm")
        .current_dir(repo)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .with_context(|| format!("failed to start pnpm in {}", repo.display()))?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("pnpm command exited with {status}"))
    }
}

fn parse_out_arg(words: &[String]) -> Result<Option<PathBuf>> {
    match words {
        [] => Ok(None),
        [flag, dir] if flag == "--out" => Ok(Some(PathBuf::from(dir))),
        _ => Err(anyhow!("usage: start [--out DIR]")),
    }
}

fn command_candidates() -> Vec<Candidate> {
    [
        ("start", "start recording"),
        ("stop", "stop and flush artifacts"),
        ("cancel", "cancel recording and close window"),
        ("replay", "replay a recording"),
        ("status", "show run state"),
        ("events", "summarize physical events"),
        ("artifacts", "list current files"),
        ("rename", "save current recording under a stable name"),
        ("repo", "show or switch Modular checkout"),
        ("scenario", "show or switch scenario"),
        ("help", "show help"),
        ("clear", "clear feed"),
        ("quit", "exit"),
    ]
    .into_iter()
    .map(|(insert, hint)| Candidate { insert: insert.to_owned(), hint: hint.to_owned() })
    .collect()
}

fn saved_recording_candidates(repo: &PathBuf) -> Vec<Candidate> {
    let root = saved_recording_root(repo);
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|ty| ty.is_dir()) {
            candidates.push(Candidate {
                insert: entry.path().display().to_string(),
                hint: "saved recording".to_owned(),
            });
        }
    }
    candidates.sort_by(|a, b| a.insert.cmp(&b.insert));
    candidates
}

fn default_recording_name(scenario: &str) -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("{scenario}-{seconds}")
}

fn current_word(text: &str) -> (usize, &str) {
    if text.ends_with(char::is_whitespace) {
        return (text.len(), "");
    }
    match text.char_indices().rev().find(|(_, ch)| ch.is_whitespace()) {
        Some((index, ch)) => {
            let start = index + ch.len_utf8();
            (start, &text[start..])
        }
        None => (0, text),
    }
}

fn rank_candidates(candidates: Vec<Candidate>, needle: &str) -> Vec<Candidate> {
    if needle.is_empty() {
        return candidates;
    }

    let mut scored: Vec<(u16, Candidate)> = candidates
        .into_iter()
        .filter_map(|candidate| {
            fuzzy::score_ci(&candidate.insert, needle).map(|score| (score, candidate))
        })
        .collect();
    scored.sort_by(|(left_score, left), (right_score, right)| {
        left_score.cmp(right_score).then_with(|| left.insert.cmp(&right.insert))
    });
    scored.into_iter().map(|(_, candidate)| candidate).collect()
}

fn starts_with_ci(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let shown = app.suggestions.as_ref().map_or(0, |menu| menu_rows(menu, area));
    let menu_height = if shown == 0 { 0 } else { shown as u16 + 1 };
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(menu_height),
        Constraint::Length(1),
    ])
    .split(area);

    render_header(frame, chunks[0], app);
    let lines = app.visible_lines();
    app.feed_height = chunks[1].height.saturating_sub(2) as usize;
    app.feed_total = lines.len();
    render_feed(frame, chunks[1], app, lines);
    if shown > 0 {
        if let Some(menu) = &app.suggestions {
            render_suggestions(frame, chunks[2], menu, shown);
        }
    }
    render_input(frame, chunks[3], app);

    if app.help_open {
        render_help(frame, area);
    }
}

fn menu_rows(menu: &Suggestions, area: Rect) -> usize {
    const ROWS: usize = 6;
    let room = (area.height as usize).saturating_sub(7);
    menu.candidates.len().min(ROWS).min(room)
}

fn render_suggestions(frame: &mut Frame<'_>, area: Rect, menu: &Suggestions, shown: usize) {
    let offset = menu.selected.unwrap_or(0).saturating_sub(shown - 1);
    let hint_width = (area.width as usize).saturating_sub(34);
    let mut lines = vec![menu_rule(menu, offset, shown, area.width as usize)];

    for (index, candidate) in menu.candidates.iter().enumerate().skip(offset).take(shown) {
        let engaged = menu.selected == Some(index);
        let marker = if engaged { "▌ " } else { "  " };
        let insert_style = if engaged {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(Color::Cyan)),
            Span::styled(format!("{:<28}", truncate(&candidate.insert, 28)), insert_style),
            Span::styled(
                format!("  {}", truncate(&candidate.hint, hint_width)),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn menu_rule(menu: &Suggestions, offset: usize, shown: usize, width: usize) -> Line<'static> {
    let mut label = format!("─ suggestions · {} ", menu.candidates.len());
    if offset > 0 {
        label.push_str(&format!("· ↑ {offset} "));
    }
    let below = menu.candidates.len() - offset - shown;
    if below > 0 {
        label.push_str(&format!("· ↓ {below} "));
    }
    let keys =
        if menu.selected.is_some() { " ⏎ accept · esc back ─" } else { " ⇥ select ─" };
    let fill = width.saturating_sub(label.chars().count() + keys.chars().count());
    Line::from(Span::styled(
        format!("{label}{}{keys}", "─".repeat(fill)),
        Style::default().fg(Color::DarkGray),
    ))
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let recording = if app.recording {
        Span::styled("● recording", Style::default().fg(Color::Green))
    } else {
        Span::styled("○ idle", Style::default().fg(Color::DarkGray))
    };
    let replay = if app.replay_stdin.is_some() {
        Span::styled("  ● replay", Style::default().fg(Color::Yellow))
    } else {
        Span::raw("")
    };
    let repo = app.repo.file_name().and_then(|name| name.to_str()).unwrap_or("repo");

    let body = vec![
        Line::from(vec![
            Span::styled("record", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(format!("  {repo}  {}  ", app.scenario)),
            recording,
            replay,
        ]),
        Line::from(Span::styled(
            "start · stop · cancel · replay · status · events · artifacts · rename · help",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(body).block(panel(" kit record ")), area);
}

fn render_feed(frame: &mut Frame<'_>, area: Rect, app: &App, lines: Vec<Line<'static>>) {
    let inner_height = area.height.saturating_sub(2) as usize;
    let max_top = lines.len().saturating_sub(inner_height);
    let top = app.view_top.map_or(max_top, |top| top.min(max_top));
    let below = max_top - top;
    let title = if below == 0 {
        " feed ─ ● live ".to_owned()
    } else {
        format!(" feed ─ ▲ {below} below ")
    };

    frame.render_widget(
        Paragraph::new(lines).block(panel_titled(title)).scroll((top as u16, 0)),
        area,
    );
}

fn render_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let prompt = "record› ";
    let mut spans = vec![
        Span::styled(prompt, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(app.input.value().to_owned()),
    ];
    if let Some(ghost) = app.ghost() {
        spans.push(Span::styled(ghost.text, Style::default().fg(Color::DarkGray)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    let cursor_x =
        area.x + prompt.width() as u16 + app.input.value()[..app.input.cursor()].width() as u16;
    frame.set_cursor_position((cursor_x.min(area.right().saturating_sub(1)), area.y));
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let width = area.width.min(78);
    let height = 16.min(area.height.saturating_sub(2));
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect::new(x, y, width, height);

    let body = vec![
        Line::from(vec![
            Span::styled("start [--out DIR]", Style::default().fg(Color::Cyan)),
            Span::raw("  start pnpm record in the background"),
        ]),
        Line::from(vec![
            Span::styled("stop", Style::default().fg(Color::Cyan)),
            Span::raw("               ask pnpm record-stop to flush artifacts"),
        ]),
        Line::from(vec![
            Span::styled("cancel", Style::default().fg(Color::Cyan)),
            Span::raw("             close the recorder window without finalizing"),
        ]),
        Line::from(vec![
            Span::styled("replay [DIR]", Style::default().fg(Color::Cyan)),
            Span::raw("       replay current recording or DIR"),
        ]),
        Line::from(vec![
            Span::styled("status", Style::default().fg(Color::Cyan)),
            Span::raw("             show run state"),
        ]),
        Line::from(vec![
            Span::styled("events", Style::default().fg(Color::Cyan)),
            Span::raw("             summarize physical-events.jsonl"),
        ]),
        Line::from(vec![
            Span::styled("artifacts", Style::default().fg(Color::Cyan)),
            Span::raw("          list current recording files"),
        ]),
        Line::from(vec![
            Span::styled("rename NAME", Style::default().fg(Color::Cyan)),
            Span::raw("       move current recording into saved recordings"),
        ]),
        Line::from(vec![
            Span::styled("repo [PATH]", Style::default().fg(Color::Cyan)),
            Span::raw("        show or switch Modular checkout"),
        ]),
        Line::from(vec![
            Span::styled("scenario [ID]", Style::default().fg(Color::Cyan)),
            Span::raw("      show or switch scenario"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Tab suggestions · Right ghost · Ctrl-C cancel/quit · PgUp/PgDn scroll · Esc live",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(body).block(panel(" kit record · interactive ")).alignment(Alignment::Left),
        popup,
    );
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

fn panel_titled(title: String) -> Block<'static> {
    Block::default()
        .title(title)
        .title_alignment(Alignment::Left)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::new(1, 1, 0, 0))
}

fn truncate(text: &str, max: usize) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if out.chars().count() + 1 >= max {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}
