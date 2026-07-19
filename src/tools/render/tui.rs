use std::{
    fs, future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
    Frame,
};
use tokio::sync::{
    mpsc::{self, UnboundedReceiver},
    Notify,
};
use tokio::time::{self, Instant};
use unicode_width::UnicodeWidthStr;
use url::Url;

use super::{
    config::{self, Config},
    search::SearchIndex,
};
use crate::tui::{
    fuzzy,
    markdown::{has_heading, MarkdownHeading, MarkdownLink, MarkdownRenderer, MarkdownSearchLine},
    render_split_divider,
    theme::{self, TuiTheme},
    CommandSet, CommandSpec, EventReader, Frecency, FrecencyStore, LineEditor, ParsedInput,
    Session, SessionOptions, SettingsEditor, SettingsFlow, SplitDividerStyle, SplitDrag,
    SplitFrame, SplitMinimums, SplitRatio, Suggestion, SuggestionMenu,
};

const SUGGESTION_ROWS: usize = 8;
const SCROLL_STEP: isize = 3;
const WATCH_SETTLE_TIME: Duration = Duration::from_millis(60);
const TOC_MIN_DOCUMENT_WIDTH: u16 = 48;
const TOC_MIN_WIDTH: u16 = 24;
const TOC_MIN_LAYOUT_WIDTH: u16 = TOC_MIN_DOCUMENT_WIDTH + TOC_MIN_WIDTH + 1;
const DEFAULT_TOC_SPLIT_RATIO: SplitRatio = SplitRatio::new(700);
const COMMANDS: CommandSet = CommandSet::new(&[
    CommandSpec {
        name: "configure",
        aliases: &["config"],
        usage: "/configure",
        description: "configure the Markdown viewer",
    },
    CommandSpec {
        name: "theme",
        aliases: &[],
        usage: "/theme <nord|terminal|path>",
        description: "change and persist the render theme",
    },
    CommandSpec {
        name: "find",
        aliases: &["search"],
        usage: "/find <text>",
        description: "search inside the open document",
    },
    CommandSpec {
        name: "help",
        aliases: &["?"],
        usage: "/help",
        description: "show render commands",
    },
    CommandSpec { name: "quit", aliases: &["q"], usage: "/quit", description: "exit cleanly" },
]);

pub async fn run(
    root: PathBuf,
    initial: Option<PathBuf>,
    config: Config,
    theme_spec: String,
    theme: TuiTheme,
) -> Result<()> {
    let root = root.canonicalize().context("resolve Markdown search root")?;
    let search_wake = Arc::new(Notify::new());
    let index = SearchIndex::discover(&root, Arc::clone(&search_wake));
    let frecency_store =
        FrecencyStore::bootstrap("render").context("open Render frecency store")?;
    let frecency = frecency_store.load().context("load Render frecency")?;
    let mut app = App::new(root, index, frecency, initial, config, theme_spec, theme)?;
    let mut session =
        Session::open(SessionOptions { mouse_capture: true, bracketed_paste: false })?;
    let mut events = EventReader::start();
    let mut document_watch = DocumentWatch::start()?;
    document_watch.follow(app.document_path())?;
    let mut reload_at = None;

    loop {
        session.draw(|frame| render(frame, &mut app))?;
        tokio::select! {
            event = events.recv() => match event {
                Some(Event::Key(key)) if key.is_press() => {
                    let previous_document = app.document_path().map(Path::to_path_buf);
                    if matches!(app.on_key(key), Flow::Quit) {
                        break;
                    }
                    if app.document_path() != previous_document.as_deref() {
                        reload_at = None;
                    }
                    if let Err(error) = document_watch.follow(app.document_path()) {
                        app.notice = Notice::error(format!("watch document: {error:#}"));
                    }
                }
                Some(Event::Mouse(mouse)) => {
                    let previous_document = app.document_path().map(Path::to_path_buf);
                    app.on_mouse(mouse);
                    if app.document_path() != previous_document.as_deref() {
                        reload_at = None;
                    }
                    if let Err(error) = document_watch.follow(app.document_path()) {
                        app.notice = Notice::error(format!("watch document: {error:#}"));
                    }
                }
                Some(Event::Resize(_, _)) => {
                    app.cancel_toc_drag();
                }
                None => break,
                _ => {}
            },
            change = document_watch.recv() => match change {
                Some(Ok(event)) if app.document_affected(&event) => {
                    reload_at = Some(Instant::now() + WATCH_SETTLE_TIME);
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    app.notice = Notice::error(format!("watch document: {error}"));
                }
                None => break,
            },
            () = wait_for_reload(reload_at) => {
                reload_at = None;
                app.reload_document(false);
            },
            () = search_wake.notified() => {
                app.refresh_search_results();
            },
        }
    }

    if app.frecency.is_dirty() {
        frecency_store.save(&mut app.frecency).context("save Render frecency")?;
    }
    Ok(())
}

async fn wait_for_reload(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(deadline).await,
        None => future::pending().await,
    }
}

struct DocumentWatch {
    watcher: RecommendedWatcher,
    receiver: UnboundedReceiver<notify::Result<notify::Event>>,
    parent: Option<PathBuf>,
}

impl DocumentWatch {
    fn start() -> Result<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let watcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(event);
        })
        .context("start document watcher")?;
        Ok(Self { watcher, receiver, parent: None })
    }

    fn follow(&mut self, path: Option<&Path>) -> Result<()> {
        let parent = path.and_then(Path::parent).map(Path::to_path_buf);
        if parent == self.parent {
            return Ok(());
        }

        if let Some(parent) = &parent {
            self.watcher
                .watch(parent, RecursiveMode::NonRecursive)
                .with_context(|| format!("watch {}", parent.display()))?;
        }
        let previous = std::mem::replace(&mut self.parent, parent);
        if let Some(previous) = previous {
            self.watcher
                .unwatch(&previous)
                .with_context(|| format!("stop watching {}", previous.display()))?;
        }
        Ok(())
    }

    async fn recv(&mut self) -> Option<notify::Result<notify::Event>> {
        self.receiver.recv().await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Flow {
    Continue,
    Quit,
}

enum Surface {
    Viewer,
    Settings(SettingsEditor),
}

struct App {
    root: PathBuf,
    index: SearchIndex,
    frecency: Frecency<PathBuf>,
    config: Config,
    theme_spec: String,
    theme: TuiTheme,
    surface: Surface,
    document: Option<Document>,
    scroll_top: usize,
    content_height: usize,
    viewport_height: usize,
    input: LineEditor,
    suggestions: Option<SuggestionMenu>,
    search: Option<DocumentSearch>,
    toc_hits: Vec<TocHit>,
    link_hits: Vec<LinkHit>,
    toc_split_ratio: SplitRatio,
    toc_split_frame: SplitFrame,
    toc_drag: Option<SplitDrag<()>>,
    history: NavigationHistory,
    notice: Notice,
}

struct DocumentSearch {
    query: String,
    matches: Vec<usize>,
    selected: usize,
}

#[derive(Clone, Copy)]
struct TocHit {
    area: Rect,
    line: usize,
}

struct LinkHit {
    area: Rect,
    destination: String,
}

#[derive(Default)]
struct NavigationHistory {
    entries: Vec<PathBuf>,
    cursor: Option<usize>,
}

impl NavigationHistory {
    fn visit(&mut self, path: PathBuf) {
        if self.current() == Some(path.as_path()) {
            return;
        }
        let keep = self.cursor.map_or(0, |cursor| cursor + 1);
        self.entries.truncate(keep);
        self.entries.push(path);
        self.cursor = Some(self.entries.len() - 1);
    }

    fn current(&self) -> Option<&Path> {
        self.cursor.and_then(|cursor| self.entries.get(cursor)).map(PathBuf::as_path)
    }

    fn target(&self, delta: isize) -> Option<(usize, &Path)> {
        let cursor = self.cursor?;
        let target = cursor.checked_add_signed(delta)?;
        self.entries.get(target).map(|path| (target, path.as_path()))
    }

    fn select(&mut self, cursor: usize) {
        debug_assert!(cursor < self.entries.len());
        self.cursor = Some(cursor);
    }
}

fn document_search_notice(search: &DocumentSearch) -> String {
    if search.matches.is_empty() {
        format!("no matches for {:?} · Esc clears", search.query)
    } else {
        format!(
            "match {}/{} for {:?} · n/N navigates · Esc clears",
            search.selected + 1,
            search.matches.len(),
            search.query
        )
    }
}

impl App {
    fn new(
        root: PathBuf,
        index: SearchIndex,
        frecency: Frecency<PathBuf>,
        initial: Option<PathBuf>,
        config: Config,
        theme_spec: String,
        theme: TuiTheme,
    ) -> Result<Self> {
        let mut app = Self {
            root,
            index,
            frecency,
            config,
            theme_spec,
            theme,
            surface: Surface::Viewer,
            document: None,
            scroll_top: 0,
            content_height: 0,
            viewport_height: 0,
            input: LineEditor::default(),
            suggestions: None,
            search: None,
            toc_hits: Vec::new(),
            link_hits: Vec::new(),
            toc_split_ratio: DEFAULT_TOC_SPLIT_RATIO,
            toc_split_frame: SplitFrame::default(),
            toc_drag: None,
            history: NavigationHistory::default(),
            notice: Notice::info(String::new()),
        };

        if let Some(path) = initial {
            app.open(path)?;
        } else {
            app.refresh_suggestions();
        }
        Ok(app)
    }

    fn on_key(&mut self, key: KeyEvent) -> Flow {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return Flow::Quit;
        }
        if let Surface::Settings(editor) = &mut self.surface {
            if editor.on_key(key) == SettingsFlow::Exit {
                self.leave_settings();
            }
            return Flow::Continue;
        }
        if self.toc_drag.is_some() {
            if key.code == KeyCode::Esc {
                self.cancel_toc_drag();
            }
            return Flow::Continue;
        }
        if self.input.value().is_empty() && self.toc_split_frame.separator.width > 0 {
            match key.code {
                KeyCode::Char('<') => {
                    self.resize_toc_split(-5);
                    return Flow::Continue;
                }
                KeyCode::Char('>') => {
                    self.resize_toc_split(5);
                    return Flow::Continue;
                }
                KeyCode::Char('=') if key.modifiers.is_empty() => {
                    self.reset_toc_split();
                    self.notice = Notice::info("reset contents panel width".to_owned());
                    return Flow::Continue;
                }
                _ => {}
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            self.clear_search();
            return Flow::Continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f') {
            self.open_document_search();
            return Flow::Continue;
        }
        if key.modifiers.is_empty()
            && key.code == KeyCode::Char('r')
            && self.input.value().is_empty()
        {
            self.refresh_index();
            return Flow::Continue;
        }

        let menu = self.suggestions.is_some();
        let engaged = self.suggestions.as_ref().is_some_and(SuggestionMenu::is_engaged);
        match key.code {
            KeyCode::Up
                if key.modifiers == KeyModifiers::SHIFT && self.input.value().is_empty() =>
            {
                self.scroll_top = 0;
            }
            KeyCode::Down
                if key.modifiers == KeyModifiers::SHIFT && self.input.value().is_empty() =>
            {
                self.scroll_top = self.max_scroll();
            }
            KeyCode::Char('n')
                if key.modifiers.is_empty()
                    && self.input.value().is_empty()
                    && self.search.is_some() =>
            {
                self.move_search(1);
            }
            KeyCode::Char('N')
                if (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
                    && self.input.value().is_empty()
                    && self.search.is_some() =>
            {
                self.move_search(-1);
            }
            KeyCode::Left if key.modifiers.is_empty() && self.input.value().is_empty() && !menu => {
                self.move_history(-1);
            }
            KeyCode::Right
                if key.modifiers.is_empty() && self.input.value().is_empty() && !menu =>
            {
                self.move_history(1);
            }
            KeyCode::Enter if engaged => return self.submit_selected(),
            KeyCode::Enter => return self.submit_typed(),
            KeyCode::Tab if menu => self.cycle_selection(1),
            KeyCode::BackTab if menu => self.cycle_selection(-1),
            KeyCode::Tab => {
                self.refresh_suggestions();
                self.cycle_selection(1);
            }
            KeyCode::Down if menu => self.cycle_selection(1),
            KeyCode::Up if menu => self.cycle_selection(-1),
            KeyCode::Down if self.input.value().is_empty() => self.scroll_by(1),
            KeyCode::Up if self.input.value().is_empty() => self.scroll_by(-1),
            KeyCode::Right if engaged => self.complete_selected(),
            KeyCode::Right
                if self.input.cursor() == self.input.value().len() && self.ghost().is_some() =>
            {
                self.accept_ghost()
            }
            KeyCode::PageDown => self.page_by(1),
            KeyCode::PageUp => self.page_by(-1),
            KeyCode::Home if self.input.value().is_empty() => self.scroll_top = 0,
            KeyCode::End if self.input.value().is_empty() => self.scroll_top = self.max_scroll(),
            KeyCode::Esc if engaged => {
                if let Some(menu) = &mut self.suggestions {
                    menu.disengage();
                }
            }
            KeyCode::Esc if !self.input.value().is_empty() => self.clear_search(),
            KeyCode::Esc if self.search.is_some() => self.clear_document_search(),
            KeyCode::Esc => return Flow::Quit,
            _ => {
                self.input.apply_key(key);
                self.refresh_suggestions();
            }
        }
        Flow::Continue
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        if matches!(self.surface, Surface::Settings(_)) {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.begin_toc_drag(mouse.column, mouse.row) {
                    return;
                }
                if let Some(destination) = self
                    .link_hits
                    .iter()
                    .find(|hit| rect_contains(hit.area, mouse.column, mouse.row))
                    .map(|hit| hit.destination.clone())
                {
                    self.open_link(&destination);
                    return;
                }
                if let Some(line) = self
                    .toc_hits
                    .iter()
                    .find(|hit| rect_contains(hit.area, mouse.column, mouse.row))
                    .map(|hit| hit.line)
                {
                    self.scroll_top = line.min(self.max_scroll());
                    self.notice = Notice::info("jumped to heading".to_owned());
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.update_toc_drag(mouse.column);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.finish_toc_drag();
            }
            MouseEventKind::ScrollUp => self.scroll_by(-SCROLL_STEP),
            MouseEventKind::ScrollDown => self.scroll_by(SCROLL_STEP),
            _ => {}
        }
    }

    fn begin_toc_drag(&mut self, column: u16, row: u16) -> bool {
        self.toc_drag =
            SplitDrag::begin((), self.toc_split_frame, self.toc_split_ratio, column, row);
        self.toc_drag.is_some()
    }

    fn update_toc_drag(&mut self, column: u16) -> bool {
        let Some(drag) = self.toc_drag else {
            return false;
        };
        let Some(ratio) = drag.ratio_for_column((), self.toc_split_frame, column) else {
            self.cancel_toc_drag();
            return false;
        };
        let changed = self.toc_split_ratio != ratio;
        self.toc_split_ratio = ratio;
        changed
    }

    fn finish_toc_drag(&mut self) -> bool {
        let Some(drag) = self.toc_drag.take() else {
            return false;
        };
        drag.changed(self.toc_split_ratio)
    }

    fn cancel_toc_drag(&mut self) -> bool {
        let Some(drag) = self.toc_drag.take() else {
            return false;
        };
        let (_, start_ratio) = drag.cancel();
        let changed = self.toc_split_ratio != start_ratio;
        self.toc_split_ratio = start_ratio;
        changed
    }

    fn reset_toc_split(&mut self) {
        self.toc_split_ratio = DEFAULT_TOC_SPLIT_RATIO;
    }

    fn resize_toc_split(&mut self, cells: i16) {
        let column = self.toc_split_frame.separator.x.saturating_add_signed(cells);
        if let Some(ratio) = self.toc_split_frame.ratio_for_column(column) {
            self.toc_split_ratio = ratio;
            let action = if cells.is_negative() { "narrowed" } else { "widened" };
            self.notice = Notice::info(format!("{action} document panel"));
        }
    }

    fn submit_typed(&mut self) -> Flow {
        let raw = self.input.value().trim().to_owned();
        match COMMANDS.parse(&raw) {
            ParsedInput::Empty => {
                self.notice = Notice::info("type a path, then use Tab or ↑/↓ to select".to_owned());
                Flow::Continue
            }
            ParsedInput::Query(path) => {
                self.open_query(&path);
                Flow::Continue
            }
            ParsedInput::Command { name, args } => self.dispatch_command(name, &args),
            ParsedInput::Unknown(name) => {
                let path = resolve_from(&self.root, &raw);
                if path.is_file() {
                    self.try_open(path);
                } else {
                    self.notice = Notice::error(format!("unknown command /{name} — try /help"));
                }
                Flow::Continue
            }
        }
    }

    fn open_query(&mut self, raw: &str) {
        if raw.is_empty() {
            self.notice = Notice::info("type a path, then use Tab or ↑/↓ to select".to_owned());
            return;
        }

        let path = resolve_from(&self.root, raw);
        if path.is_file() {
            self.try_open(path);
            return;
        }

        if let Some(candidate) = self
            .suggestions
            .as_ref()
            .filter(|menu| menu.len() == 1)
            .and_then(SuggestionMenu::first)
            .map(|candidate| candidate.insert.clone())
        {
            self.try_open(resolve_from(&self.root, &candidate));
            return;
        }

        self.notice = Notice::error(format!(
            "{} is not a file — Tab or ↓ selects a fuzzy match",
            path.display()
        ));
    }

    fn submit_selected(&mut self) -> Flow {
        let selected = self
            .suggestions
            .as_ref()
            .and_then(SuggestionMenu::selected)
            .map(|candidate| candidate.insert.clone());
        let Some(selected) = selected else {
            return Flow::Continue;
        };
        if selected.starts_with('/') {
            self.input.set(selected);
            self.submit_typed()
        } else {
            self.try_open(resolve_from(&self.root, &selected));
            Flow::Continue
        }
    }

    fn dispatch_command(&mut self, name: &str, args: &str) -> Flow {
        match name {
            "configure" if args.trim().is_empty() => {
                self.surface = Surface::Settings(SettingsEditor::open(
                    self.config.store(),
                    vec![config::settings()],
                    self.theme,
                ));
                self.input.clear();
                self.suggestions = None;
                Flow::Continue
            }
            "configure" => {
                self.notice = Notice::error("/configure takes no arguments".to_owned());
                Flow::Continue
            }
            "theme" if !args.trim().is_empty() => {
                self.apply_theme(args.trim());
                Flow::Continue
            }
            "theme" => {
                self.open_theme_picker();
                Flow::Continue
            }
            "find" if args.trim().is_empty() => {
                self.open_document_search();
                Flow::Continue
            }
            "find" => {
                self.start_document_search(args.trim());
                Flow::Continue
            }
            "help" => {
                self.input.clear();
                self.suggestions = None;
                self.notice = Notice::info(
                    "/find searches · n/N moves · drag or </> resizes · = resets · /quit exits"
                        .to_owned(),
                );
                Flow::Continue
            }
            "quit" => Flow::Quit,
            _ => Flow::Continue,
        }
    }

    fn apply_theme(&mut self, requested: &str) {
        let (spec, next) = match theme::resolve(requested) {
            Ok(resolved) => resolved,
            Err(error) => {
                self.notice = Notice::error(format!("load theme: {error:#}"));
                return;
            }
        };
        match self.config.set_theme(&spec) {
            Ok(()) => {
                self.theme_spec = spec;
                self.theme = next;
                self.input.clear();
                self.suggestions = None;
                self.notice =
                    Notice::info(format!("theme changed · saved {}", self.config.path().display()));
            }
            Err(error) => {
                self.notice = Notice::error(format!("save render theme: {error:#}"));
            }
        }
    }

    fn open_theme_picker(&mut self) {
        self.input.set("/theme ".to_owned());
        self.suggestions = Some(SuggestionMenu::new(self.theme_suggestions(""), 0));
        self.notice = Notice::info("select a theme with Tab or ↑/↓, then press Enter".to_owned());
    }

    fn open_document_search(&mut self) {
        if self.document.is_none() {
            self.notice = Notice::error("open a Markdown file before searching".to_owned());
            return;
        }
        self.input.set("/find ".to_owned());
        self.suggestions = None;
        self.notice = Notice::info("type text to find, then press Enter".to_owned());
    }

    fn start_document_search(&mut self, query: &str) {
        if self.document.is_none() {
            self.notice = Notice::error("open a Markdown file before searching".to_owned());
            return;
        }
        self.search =
            Some(DocumentSearch { query: query.to_owned(), matches: Vec::new(), selected: 0 });
        self.input.clear();
        self.suggestions = None;
        self.notice = Notice::info(String::new());
    }

    fn sync_document_search(&mut self, matches: Vec<usize>) {
        let Some(search) = &mut self.search else {
            return;
        };
        let jump_to_first = search.matches.is_empty() && !matches.is_empty();
        search.matches = matches;
        search.selected = search.selected.min(search.matches.len().saturating_sub(1));
        let line = search.matches.get(search.selected).copied();
        if jump_to_first {
            if let Some(line) = line {
                self.scroll_top = line.min(self.max_scroll());
            }
        }
    }

    fn move_search(&mut self, delta: isize) {
        let Some(search) = &mut self.search else {
            return;
        };
        if search.matches.is_empty() {
            return;
        }
        search.selected =
            (search.selected as isize + delta).rem_euclid(search.matches.len() as isize) as usize;
        let line = search.matches[search.selected];
        self.scroll_top = line.min(self.max_scroll());
    }

    fn clear_document_search(&mut self) {
        self.search = None;
        self.notice = Notice::info("document search cleared".to_owned());
    }

    fn theme_suggestions(&self, query: &str) -> Vec<Suggestion> {
        let mut choices = vec![
            ("nord".to_owned(), "built-in Nord palette".to_owned()),
            ("terminal".to_owned(), "built-in terminal colors".to_owned()),
        ];
        let theme_dir = self.config.theme_dir();
        if let Ok(entries) = fs::read_dir(&theme_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file()
                    && path.extension().and_then(|extension| extension.to_str()) == Some("toml")
                {
                    choices.push((
                        path.display().to_string(),
                        format!("custom theme from {}", theme_dir.display()),
                    ));
                }
            }
        }
        if theme::built_in(&self.theme_spec).is_none() {
            let path = PathBuf::from(&self.theme_spec);
            if path.is_file() && !choices.iter().any(|(spec, _)| spec == &self.theme_spec) {
                choices.push((self.theme_spec.clone(), "current custom theme".to_owned()));
            }
        }
        choices.sort_by(|left, right| left.0.cmp(&right.0));

        let mut matcher = fuzzy::Matcher::case_insensitive(query);
        let mut ranked = choices
            .into_iter()
            .filter_map(|(spec, hint)| {
                let score = if query.is_empty() { 0 } else { matcher.score(&spec)? };
                let active = if spec == self.theme_spec { " · active" } else { "" };
                Some((score, Suggestion::new(format!("/theme {spec}"), format!("{hint}{active}"))))
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            left.0.cmp(&right.0).then_with(|| left.1.insert.cmp(&right.1.insert))
        });
        ranked.into_iter().map(|(_, suggestion)| suggestion).collect()
    }

    fn leave_settings(&mut self) {
        let store = self.config.store();
        match Config::load(store) {
            Ok(config) => {
                let requested_theme = config.theme().to_owned();
                self.config = config;
                match theme::resolve(&requested_theme) {
                    Ok((spec, theme)) => {
                        self.theme_spec = spec;
                        self.theme = theme;
                        self.notice = Notice::info("Settings updated".to_owned());
                    }
                    Err(error) => {
                        self.notice = Notice::error(format!("reload Render theme: {error:#}"));
                    }
                }
            }
            Err(error) => {
                self.notice = Notice::error(format!("reload Render Settings: {error:#}"));
            }
        }
        self.surface = Surface::Viewer;
        self.input.clear();
        self.refresh_suggestions();
    }

    fn try_open(&mut self, path: PathBuf) {
        match self.open(path) {
            Ok(()) => {}
            Err(error) => self.notice = Notice::error(error.to_string()),
        }
    }

    fn document_path(&self) -> Option<&Path> {
        self.document.as_ref().map(|document| document.path.as_path())
    }

    fn document_affected(&self, event: &notify::Event) -> bool {
        if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_))
        {
            return false;
        }
        let Some(path) = self.document_path() else {
            return false;
        };
        event.paths.iter().any(|changed| changed == path)
    }

    fn reload_document(&mut self, announce_unchanged: bool) -> bool {
        let Some(path) = self.document_path().map(Path::to_path_buf) else {
            return true;
        };
        match Document::load(&self.root, path) {
            Ok(document)
                if self
                    .document
                    .as_ref()
                    .is_some_and(|open| open.markdown == document.markdown) =>
            {
                if announce_unchanged {
                    self.notice = Notice::info(format!("refreshed {}", document.display));
                }
                true
            }
            Ok(document) => {
                self.notice = Notice::info(format!("reloaded {}", document.display));
                self.document = Some(document);
                true
            }
            Err(error) => {
                self.notice = Notice::error(format!("reload document: {error:#}"));
                false
            }
        }
    }

    fn refresh_index(&mut self) {
        let had_suggestions = self.suggestions.is_some();
        let indexed = self.index.refresh(&self.root);
        if had_suggestions || !self.input.value().trim().is_empty() {
            self.refresh_suggestions();
        }

        let display = self.document.as_ref().map(|document| document.display.clone());
        if !self.reload_document(false) {
            return;
        }
        let noun = if indexed == 1 { "file" } else { "files" };
        self.notice = Notice::info(match display {
            Some(display) => format!("refreshed {display} · indexed {indexed} Markdown {noun}"),
            None => format!("indexed {indexed} Markdown {noun}"),
        });
    }

    fn open(&mut self, path: PathBuf) -> Result<()> {
        let path = if path.is_absolute() { path } else { self.root.join(path) };
        let document = Document::load(&self.root, path)?;
        self.history.visit(document.path.clone());
        self.frecency.record(document.path.clone());
        self.show_document(document);
        Ok(())
    }

    fn show_document(&mut self, document: Document) {
        self.notice = Notice::info(format!(
            "opened {} · ←/→ history · Ctrl-F finds · ⇧↑/⇧↓ jumps",
            document.display
        ));
        self.document = Some(document);
        self.search = None;
        self.scroll_top = 0;
        self.input.clear();
        self.suggestions = None;
    }

    fn move_history(&mut self, delta: isize) {
        let Some((cursor, path)) =
            self.history.target(delta).map(|(cursor, path)| (cursor, path.to_path_buf()))
        else {
            let direction = if delta.is_negative() { "back" } else { "forward" };
            self.notice = Notice::info(format!("no {direction} history"));
            return;
        };
        match Document::load(&self.root, path) {
            Ok(document) => {
                self.history.select(cursor);
                let display = document.display.clone();
                self.show_document(document);
                self.notice = Notice::info(format!("history · {display}"));
            }
            Err(error) => {
                self.notice = Notice::error(format!("open history entry: {error:#}"));
            }
        }
    }

    fn open_link(&mut self, destination: &str) {
        let Some(current) = self.document_path().map(Path::to_path_buf) else {
            return;
        };
        match resolve_document_link(&current, destination).and_then(|path| self.open(path)) {
            Ok(()) => {}
            Err(error) => {
                self.notice = Notice::error(format!("open link {destination:?}: {error:#}"));
            }
        }
    }

    fn clear_search(&mut self) {
        self.input.clear();
        self.refresh_suggestions();
    }

    fn refresh_suggestions(&mut self) {
        let raw = self.input.value().to_owned();
        if let Some(query) = raw.strip_prefix("/theme ") {
            let candidates = self.theme_suggestions(query.trim());
            self.set_suggestions(&raw, candidates);
            return;
        }
        let query = raw.trim().to_owned();
        if query.starts_with('/') {
            if query.chars().any(char::is_whitespace) {
                self.suggestions = None;
                return;
            }
            let candidates = COMMANDS
                .suggestions(&query)
                .into_iter()
                .map(|command| Suggestion::new(format!("/{}", command.name), command.description))
                .collect::<Vec<_>>();
            self.set_suggestions(&query, candidates);
            return;
        }
        self.refresh_file_suggestions(&query);
    }

    fn refresh_search_results(&mut self) {
        let query = self.input.value().trim().to_owned();
        if !query.is_empty() && !query.starts_with('/') {
            self.refresh_file_suggestions(&query);
        }
    }

    fn refresh_file_suggestions(&mut self, query: &str) {
        let current = self.document.as_ref().map(|document| document.path.as_path());
        match self.index.suggestions(query, current, self.config.show_git_ignored(), &self.frecency)
        {
            Some(candidates) => self.set_suggestions(query, candidates),
            None => self.suggestions = None,
        }
    }

    fn set_suggestions(&mut self, query: &str, candidates: Vec<Suggestion>) {
        if candidates.is_empty() || (candidates.len() == 1 && candidates[0].insert == query) {
            self.suggestions = None;
        } else {
            self.suggestions = Some(SuggestionMenu::new(candidates, 0));
        }
    }

    fn cycle_selection(&mut self, step: isize) {
        if let Some(menu) = &mut self.suggestions {
            menu.cycle(step);
        }
    }

    fn complete_selected(&mut self) {
        let selected = self
            .suggestions
            .as_ref()
            .and_then(SuggestionMenu::selected)
            .map(|candidate| candidate.insert.clone());
        if let Some(path) = selected {
            self.input.set(path);
            self.refresh_suggestions();
        }
    }

    fn ghost(&self) -> Option<String> {
        let query = self.input.value();
        if query.is_empty() || self.suggestions.as_ref().is_some_and(SuggestionMenu::is_engaged) {
            return None;
        }
        let candidate = self.suggestions.as_ref()?.first()?;
        let prefix = candidate.insert.get(..query.len())?;
        if !prefix.eq_ignore_ascii_case(query) {
            return None;
        }
        candidate.insert.get(query.len()..).filter(|rest| !rest.is_empty()).map(str::to_owned)
    }

    fn accept_ghost(&mut self) {
        let Some(ghost) = self.ghost() else {
            return;
        };
        let mut input = self.input.value().to_owned();
        input.push_str(&ghost);
        self.input.set(input);
        self.refresh_suggestions();
    }

    fn set_geometry(&mut self, content_height: usize, viewport_height: usize) {
        self.content_height = content_height;
        self.viewport_height = viewport_height;
        self.scroll_top = self.scroll_top.min(self.max_scroll());
    }

    fn max_scroll(&self) -> usize {
        self.content_height.saturating_sub(self.viewport_height).min(u16::MAX as usize)
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll_top =
            (self.scroll_top as isize + delta).clamp(0, self.max_scroll() as isize) as usize;
    }

    fn page_by(&mut self, direction: isize) {
        self.scroll_by(direction.saturating_mul(self.viewport_height.max(1) as isize));
    }
}

struct Document {
    path: PathBuf,
    display: String,
    markdown: String,
}

impl Document {
    fn load(root: &Path, path: PathBuf) -> Result<Self> {
        let path = path
            .canonicalize()
            .with_context(|| format!("resolve Markdown file {}", path.display()))?;
        if !path.is_file() {
            return Err(anyhow!("{} is not a file", path.display()));
        }
        let markdown = fs::read_to_string(&path)
            .with_context(|| format!("read Markdown file {} as UTF-8", path.display()))?;
        let display = display_path(root, &path);
        Ok(Self { path, display, markdown })
    }
}

struct Notice {
    text: String,
    error: bool,
}

impl Notice {
    fn info(text: String) -> Self {
        Self { text, error: false }
    }

    fn error(text: String) -> Self {
        Self { text, error: true }
    }
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::default().bg(app.theme.background)), area);
    if let Surface::Settings(editor) = &mut app.surface {
        editor.render(frame, area);
        return;
    }
    let shown =
        app.suggestions.as_ref().map_or(0, |menu| menu.visible_rows(area, SUGGESTION_ROWS, 8));
    let menu_height = if shown == 0 { 0 } else { shown as u16 + 1 };
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(menu_height),
        Constraint::Length(1),
    ])
    .split(area);

    render_document(frame, chunks[0], app);
    if let Some(menu) = &app.suggestions {
        menu.render(frame, chunks[1], shown, 44, app.theme);
    }
    render_input(frame, chunks[2], app);
}

fn render_document(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    app.toc_hits.clear();
    app.link_hits.clear();
    app.toc_split_frame = SplitFrame::default();
    let Some(document) = &app.document else {
        let body = vec![
            Line::from(""),
            Line::from(Span::styled(
                "Type a Markdown filename below.",
                Style::default().fg(app.theme.text_strong).add_modifier(Modifier::BOLD),
            )),
            Line::from("Use fuzzy fragments from any part of the path, then Tab or ↑/↓ to select."),
            Line::from("Type /configure to change viewer settings."),
            Line::from(""),
            Line::from(Span::styled(
                "Enter opens a selected or exact path · Esc clears or quits",
                Style::default().fg(app.theme.text_muted),
            )),
        ];
        app.set_geometry(body.len(), area.height.saturating_sub(2) as usize);
        frame.render_widget(Paragraph::new(body).block(panel(" markdown ", app.theme)), area);
        return;
    };

    let renderer = MarkdownRenderer::new(app.theme);
    let toc_heading_depth = app.config.toc_heading_depth();
    let supports_toc = area.width >= TOC_MIN_LAYOUT_WIDTH;
    let mut document_area = area;
    let mut toc_area = None;
    let rendered = if supports_toc && has_heading(&document.markdown, toc_heading_depth) {
        let split = SplitFrame::horizontal(
            area,
            app.toc_split_ratio,
            SplitMinimums::new(TOC_MIN_DOCUMENT_WIDTH, TOC_MIN_WIDTH),
        );
        app.toc_split_frame = split;
        document_area = split.first;
        toc_area = Some(split.second);
        renderer.render_with_outline(&document.markdown, split.first.width.saturating_sub(4).max(1))
    } else {
        renderer.render_with_outline(&document.markdown, area.width.saturating_sub(4).max(1))
    };
    if toc_area.is_none() {
        app.cancel_toc_drag();
    }
    let mut text = rendered.text;
    let mut headings = rendered.headings;
    headings.retain(|heading| heading.level <= toc_heading_depth);
    let links = rendered.links;
    let search_lines = rendered.search_lines;
    let content_height = text.lines.len();
    let viewport_height = document_area.height.saturating_sub(2) as usize;
    app.content_height = content_height;
    app.viewport_height = viewport_height;
    let max_scroll = content_height.saturating_sub(viewport_height).min(u16::MAX as usize);
    app.scroll_top = app.scroll_top.min(max_scroll);
    if let Some(query) = app.search.as_ref().map(|search| search.query.clone()) {
        app.sync_document_search(matching_document_lines(&text, &search_lines, &query));
    }
    if let Some(search) = &app.search {
        highlight_document_matches(&mut text, search, app.theme);
    }
    app.link_hits = rendered_link_hits(document_area, app.scroll_top, viewport_height, &links);
    let title = if max_scroll == 0 {
        " markdown ".to_owned()
    } else {
        format!(" markdown ─ {}/{} ", app.scroll_top + 1, max_scroll + 1)
    };
    let scroll = app.scroll_top.min(u16::MAX as usize) as u16;
    frame.render_widget(
        Paragraph::new(text).block(panel(title, app.theme)).scroll((scroll, 0)),
        document_area,
    );
    if let Some(toc_area) = toc_area {
        app.toc_hits = render_toc(frame, toc_area, &headings, app.scroll_top, app.theme);
        render_split_divider(
            frame,
            app.toc_split_frame,
            app.toc_drag.is_some(),
            SplitDividerStyle {
                idle_color: app.theme.text_muted,
                active_color: app.theme.accent,
                idle_line: " ",
                idle_grip: "┋",
                active_line: "┃",
            },
        );
    }
}

fn rendered_link_hits(
    area: Rect,
    scroll_top: usize,
    viewport_height: usize,
    links: &[MarkdownLink],
) -> Vec<LinkHit> {
    let content_x = area.x.saturating_add(2);
    let content_y = area.y.saturating_add(1);
    let content_width = area.width.saturating_sub(4);
    links
        .iter()
        .filter_map(|link| {
            let row = link.line.checked_sub(scroll_top)?;
            if row >= viewport_height {
                return None;
            }
            let start = link.start.min(content_width);
            let end = link.end.min(content_width);
            (end > start).then(|| LinkHit {
                area: Rect::new(
                    content_x.saturating_add(start),
                    content_y.saturating_add(row as u16),
                    end - start,
                    1,
                ),
                destination: link.destination.clone(),
            })
        })
        .collect()
}

fn render_toc(
    frame: &mut Frame<'_>,
    area: Rect,
    headings: &[MarkdownHeading],
    scroll_top: usize,
    theme: TuiTheme,
) -> Vec<TocHit> {
    let active = headings.iter().rposition(|heading| heading.line <= scroll_top);
    let visible_rows = area.height.saturating_sub(2) as usize;
    let start = active
        .unwrap_or_default()
        .saturating_sub(visible_rows / 2)
        .min(headings.len().saturating_sub(visible_rows));
    let lines = headings
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(index, heading)| {
            let selected = active == Some(index);
            let prefix = if selected { "› " } else { "  " };
            let indent = "  ".repeat(heading.level.saturating_sub(1).min(4) as usize);
            let style = if selected {
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text_muted)
            };
            Line::styled(format!("{prefix}{indent}{}", heading.title), style)
        })
        .collect::<Vec<_>>();
    let hits = headings
        .iter()
        .skip(start)
        .take(visible_rows)
        .enumerate()
        .map(|(row, heading)| TocHit {
            area: Rect::new(
                area.x.saturating_add(1),
                area.y.saturating_add(1).saturating_add(row as u16),
                area.width.saturating_sub(2),
                1,
            ),
            line: heading.line,
        })
        .collect();
    let title = active.map_or_else(
        || " contents ".to_owned(),
        |index| format!(" contents ─ {}/{} ", index + 1, headings.len()),
    );
    frame.render_widget(Paragraph::new(lines).block(panel(title, theme)), area);
    hits
}

fn matching_document_lines(
    text: &Text<'_>,
    search_lines: &[MarkdownSearchLine],
    query: &str,
) -> Vec<usize> {
    let query = query.to_lowercase();
    search_lines
        .iter()
        .filter_map(|search_line| {
            if !search_line.text.to_lowercase().contains(&query) {
                return None;
            }
            (search_line.start_line..search_line.end_line)
                .find(|index| {
                    text.lines.get(*index).is_some_and(|line| {
                        line.spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>()
                            .to_lowercase()
                            .contains(&query)
                    })
                })
                .or(Some(search_line.start_line))
        })
        .collect()
}

fn highlight_document_matches(text: &mut Text<'_>, search: &DocumentSearch, theme: TuiTheme) {
    for (index, line_index) in search.matches.iter().copied().enumerate() {
        let Some(line) = text.lines.get_mut(line_index) else {
            continue;
        };
        let style = if index == search.selected {
            Style::default()
                .fg(theme.code_background)
                .bg(theme.warning)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(theme.selection)
        };
        for span in &mut line.spans {
            span.style = span.style.patch(style);
        }
    }
}

fn rect_contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

fn render_input(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let prompt = "file› ";
    let mut spans = vec![
        Span::styled(prompt, Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD)),
        Span::raw(app.input.value().to_owned()),
    ];
    if let Some(ghost) = app.ghost() {
        spans.push(Span::styled(ghost, Style::default().fg(app.theme.text_muted)));
    } else if app.input.value().is_empty() {
        let search_notice = app.search.as_ref().map(document_search_notice);
        let (hint, color) = if app.notice.error {
            (format!("‹{}›", app.notice.text), app.theme.danger)
        } else if let Some(search_notice) = search_notice {
            (format!("‹{search_notice}›"), app.theme.text_muted)
        } else if !app.notice.text.is_empty() {
            let color = if app.notice.error { app.theme.danger } else { app.theme.text_muted };
            (format!("‹{}›", app.notice.text), color)
        } else {
            ("‹path · fuzzy search›".to_owned(), app.theme.text_muted)
        };
        spans.push(Span::styled(hint, Style::default().fg(color)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans))
            .style(Style::default().fg(app.theme.text).bg(app.theme.background)),
        area,
    );

    let cursor_width = app.input.value()[..app.input.cursor()].width() as u16;
    let cursor_x = area.x + prompt.width() as u16 + cursor_width;
    frame.set_cursor_position((cursor_x.min(area.right().saturating_sub(1)), area.y));
}

fn panel<'a>(title: impl Into<Line<'a>>, theme: TuiTheme) -> Block<'a> {
    Block::default()
        .title(title)
        .title_alignment(Alignment::Left)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().fg(theme.text).bg(theme.surface))
        .border_style(Style::default().fg(theme.border))
        .padding(Padding::new(1, 1, 0, 0))
}

fn resolve_from(root: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn resolve_document_link(current: &Path, destination: &str) -> Result<PathBuf> {
    let base = Url::from_file_path(current)
        .map_err(|()| anyhow!("cannot convert {} to a file URL", current.display()))?;
    let target = base
        .join(destination)
        .with_context(|| format!("resolve link from {}", current.display()))?;
    if target.scheme() != "file" {
        return Err(anyhow!("{} links do not open inside kit render", target.scheme()));
    }
    target.to_file_path().map_err(|()| anyhow!("link is not a local file path: {destination:?}"))
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use ratatui::{backend::TestBackend, Terminal};

    use super::*;

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("kit-render-test-{}-{nonce}-{sequence}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_config(temp: &TempDir) -> Config {
        Config::load(crate::framework::ConfigStore::rooted(temp.0.join("config"))).unwrap()
    }

    fn test_index(root: &Path) -> SearchIndex {
        SearchIndex::discover(root, Arc::new(Notify::new()))
    }

    fn wait_for_search(app: &mut App) {
        for _ in 0..100 {
            app.refresh_search_results();
            if app.suggestions.is_some() {
                return;
            }
            std::thread::yield_now();
        }
        panic!("search index did not finish matching");
    }

    #[test]
    fn selecting_a_fuzzy_match_opens_and_clears_the_prompt() {
        let temp = TempDir::new();
        fs::create_dir_all(temp.0.join("docs")).unwrap();
        fs::write(temp.0.join("docs/guide.md"), "# Guide").unwrap();
        let root = temp.0.canonicalize().unwrap();
        let catalog = test_index(&root);
        let config = test_config(&temp);
        let mut app = App::new(
            root,
            catalog,
            Frecency::default(),
            None,
            config,
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();

        app.input.set("guide".to_owned());
        app.refresh_suggestions();
        wait_for_search(&mut app);
        app.cycle_selection(1);
        assert_eq!(app.submit_selected(), Flow::Continue);

        assert_eq!(
            app.document.as_ref().map(|document| document.display.as_str()),
            Some("docs/guide.md")
        );
        assert!(app.input.value().is_empty());
        assert!(app.suggestions.is_none());
    }

    #[test]
    fn document_events_only_match_the_open_file() {
        let temp = TempDir::new();
        let open_path = temp.0.join("README.md");
        let neighbor_path = temp.0.join("notes.md");
        fs::write(&open_path, "# Open").unwrap();
        fs::write(&neighbor_path, "# Neighbor").unwrap();
        let root = temp.0.canonicalize().unwrap();
        let mut app = App::new(
            root.clone(),
            test_index(&root),
            Frecency::default(),
            Some(open_path.clone()),
            test_config(&temp),
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();

        let open_change = notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(open_path.clone());
        let neighbor_change = notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(neighbor_path);
        let access = notify::Event::new(EventKind::Access(notify::event::AccessKind::Any))
            .add_path(open_path);

        assert!(app.document_affected(&open_change));
        assert!(!app.document_affected(&neighbor_change));
        assert!(!app.document_affected(&access));

        app.document = None;
        assert!(!app.document_affected(&open_change));
    }

    #[test]
    fn r_refreshes_the_open_document_without_resetting_scroll() {
        let temp = TempDir::new();
        let path = temp.0.join("README.md");
        fs::write(&path, "# Before").unwrap();
        let root = temp.0.canonicalize().unwrap();
        let mut app = App::new(
            root.clone(),
            test_index(&root),
            Frecency::default(),
            Some(path.clone()),
            test_config(&temp),
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();
        app.set_geometry(100, 20);
        app.scroll_top = 12;
        fs::write(path, "# After").unwrap();
        fs::write(temp.0.join("new.md"), "# Newly indexed").unwrap();

        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            Flow::Continue
        );

        assert_eq!(
            app.document.as_ref().map(|document| document.markdown.as_str()),
            Some("# After")
        );
        assert_eq!(app.scroll_top, 12);
        assert_eq!(app.index.len(), 2);
        assert!(app.input.value().is_empty());
        assert_eq!(app.notice.text, "refreshed README.md · indexed 2 Markdown files");

        app.input.set("new".to_owned());
        app.refresh_suggestions();
        wait_for_search(&mut app);
        assert_eq!(app.suggestions.as_ref().unwrap().first().unwrap().insert, "new.md");
    }

    #[tokio::test]
    async fn watcher_reports_changes_to_a_file_in_the_watched_parent() {
        let temp = TempDir::new();
        let path = temp.0.join("README.md");
        fs::write(&path, "# Before").unwrap();
        let path = path.canonicalize().unwrap();
        let mut watcher = DocumentWatch::start().unwrap();
        watcher.follow(Some(&path)).unwrap();

        fs::write(&path, "# After").unwrap();
        let observed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let event = watcher.recv().await.unwrap().unwrap();
                if event.paths.iter().any(|changed| changed == &path)
                    && matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    )
                {
                    break;
                }
            }
        })
        .await;

        assert!(observed.is_ok(), "watcher did not report the file write");
    }

    #[test]
    fn case_insensitive_match_stays_selectable_on_case_sensitive_filesystems() {
        let temp = TempDir::new();
        fs::write(temp.0.join("README.md"), "# Read me").unwrap();
        let root = temp.0.canonicalize().unwrap();
        let mut app = App::new(
            root.clone(),
            test_index(&root),
            Frecency::default(),
            None,
            test_config(&temp),
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();
        app.input.set("readme.md".to_owned());
        app.refresh_suggestions();
        wait_for_search(&mut app);
        assert_eq!(app.suggestions.as_ref().unwrap().first().unwrap().insert, "README.md");
    }

    #[test]
    fn viewer_renders_markdown_and_bottom_file_prompt() {
        let temp = TempDir::new();
        fs::write(temp.0.join("README.md"), "# Hello\n\nThis is **Markdown**.").unwrap();
        let root = temp.0.canonicalize().unwrap();
        let catalog = test_index(&root);
        let config = test_config(&temp);
        let mut app = App::new(
            root,
            catalog,
            Frecency::default(),
            Some(PathBuf::from("README.md")),
            config,
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen = (0..buffer.area.height)
            .map(|y| (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("Hello"), "{screen}");
        assert!(screen.contains("This is Markdown."), "{screen}");
        assert!(!screen.contains("# Hello"), "{screen}");
        assert!(!screen.contains("**Markdown**"), "{screen}");
        assert!(!screen.contains("indexed"), "{screen}");
        assert!(!screen.contains("ignored"), "{screen}");
        assert!(screen.contains("file›"), "{screen}");
        assert_eq!(buffer[(0, 0)].fg, theme::NORD.border);
        assert_eq!(buffer[(2, 1)].fg, theme::NORD.text_strong);
    }

    #[test]
    fn wide_viewer_renders_a_heading_toc_and_narrow_viewer_does_not() {
        let temp = TempDir::new();
        fs::write(
            temp.0.join("README.md"),
            "# Overview\n\nIntro.\n\n## Setup\n\nSteps.\n\n### Verify\n\nDone.",
        )
        .unwrap();
        let root = temp.0.canonicalize().unwrap();
        let config = test_config(&temp);
        let mut app = App::new(
            root.clone(),
            test_index(&root),
            Frecency::default(),
            Some(PathBuf::from("README.md")),
            config,
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();

        let wide_backend = TestBackend::new(100, 24);
        let mut wide = Terminal::new(wide_backend).unwrap();
        wide.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.toc_hits.len(), 1);

        app.config.set_toc_heading_depth(3).unwrap();
        wide.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.toc_hits.len(), 3);
        let wide_screen = (0..wide.backend().buffer().area.height)
            .map(|y| {
                (0..wide.backend().buffer().area.width)
                    .map(|x| wide.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(wide_screen.contains("contents"), "{wide_screen}");
        assert!(wide_screen.contains("Overview"), "{wide_screen}");
        assert!(wide_screen.contains("Setup"), "{wide_screen}");
        assert!(wide_screen.contains("Verify"), "{wide_screen}");

        let narrow_backend = TestBackend::new(70, 24);
        let mut narrow = Terminal::new(narrow_backend).unwrap();
        narrow.draw(|frame| render(frame, &mut app)).unwrap();
        let narrow_screen = (0..narrow.backend().buffer().area.height)
            .map(|y| {
                (0..narrow.backend().buffer().area.width)
                    .map(|x| narrow.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!narrow_screen.contains("contents"), "{narrow_screen}");
        assert_eq!(app.toc_split_frame, SplitFrame::default());
    }

    #[test]
    fn contents_divider_resizes_cancels_and_resets_through_shared_split_state() {
        let temp = TempDir::new();
        fs::write(temp.0.join("README.md"), "# Overview\n\nIntro.").unwrap();
        let root = temp.0.canonicalize().unwrap();
        let mut app = App::new(
            root.clone(),
            test_index(&root),
            Frecency::default(),
            Some(PathBuf::from("README.md")),
            test_config(&temp),
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let original_ratio = app.toc_split_ratio;
        let original_frame = app.toc_split_frame;
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: original_frame.first.x.saturating_add(2),
            row: original_frame.first.y.saturating_add(2),
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.toc_drag.is_none());

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: original_frame.separator.x,
            row: original_frame.separator.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.toc_drag.is_some());

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: original_frame.separator.x.saturating_sub(8),
            row: original_frame.separator.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_ne!(app.toc_split_ratio, original_ratio);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.toc_split_frame.first.width < original_frame.first.width);

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: app.toc_split_frame.separator.x,
            row: app.toc_split_frame.separator.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(app.toc_drag.is_none());
        let resized_ratio = app.toc_split_ratio;

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.toc_split_frame.separator.x,
            row: app.toc_split_frame.separator.y,
            modifiers: KeyModifiers::NONE,
        });
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: app.toc_split_frame.separator.x.saturating_add(8),
            row: app.toc_split_frame.separator.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_ne!(app.toc_split_ratio, resized_ratio);
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.toc_drag.is_none());
        assert_eq!(app.toc_split_ratio, resized_ratio);

        app.on_key(KeyEvent::new(KeyCode::Char('='), KeyModifiers::NONE));
        assert_eq!(app.toc_split_ratio, DEFAULT_TOC_SPLIT_RATIO);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let narrower = app
            .toc_split_frame
            .ratio_for_column(app.toc_split_frame.separator.x.saturating_sub(5))
            .unwrap();
        app.on_key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::NONE));
        assert_eq!(app.toc_split_ratio, narrower);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let wider = app
            .toc_split_frame
            .ratio_for_column(app.toc_split_frame.separator.x.saturating_add(5))
            .unwrap();
        app.on_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::NONE));
        assert_eq!(app.toc_split_ratio, wider);
    }

    #[test]
    fn clicking_a_toc_heading_jumps_to_its_rendered_line() {
        let temp = TempDir::new();
        let mut source = String::from("# First section\n\n");
        for index in 0..30 {
            source.push_str(&format!("Body paragraph {index}.\n\n"));
        }
        source.push_str("## Last section\n\nDone.\n");
        fs::write(temp.0.join("README.md"), source).unwrap();
        let root = temp.0.canonicalize().unwrap();
        let mut config = test_config(&temp);
        config.set_toc_heading_depth(2).unwrap();
        let mut app = App::new(
            root.clone(),
            test_index(&root),
            Frecency::default(),
            Some(PathBuf::from("README.md")),
            config,
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let hit = *app.toc_hits.last().unwrap();

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.area.x,
            row: hit.area.y,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(app.scroll_top, hit.line.min(app.max_scroll()));
        assert!(app.scroll_top > 0);
    }

    #[test]
    fn clicking_a_local_markdown_link_opens_it_and_arrow_keys_traverse_history() {
        let temp = TempDir::new();
        fs::create_dir_all(temp.0.join("docs")).unwrap();
        fs::write(temp.0.join("README.md"), "Read the [guide](docs/guide.md).").unwrap();
        fs::write(temp.0.join("docs/guide.md"), "# Guide").unwrap();
        let root = temp.0.canonicalize().unwrap();
        let readme = root.join("README.md").canonicalize().unwrap();
        let guide = root.join("docs/guide.md").canonicalize().unwrap();
        let mut app = App::new(
            root.clone(),
            test_index(&root),
            Frecency::default(),
            Some(readme.clone()),
            test_config(&temp),
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();
        let backend = TestBackend::new(70, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let hit = &app.link_hits[0];
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.area.x,
            row: hit.area.y,
            modifiers: KeyModifiers::NONE,
        };

        app.on_mouse(click);
        assert_eq!(app.document_path(), Some(guide.as_path()));
        assert_eq!(app.history.entries, vec![readme.clone(), guide.clone()]);
        assert_eq!(app.frecency.visits(&readme), 1);
        assert_eq!(app.frecency.visits(&guide), 1);

        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.document_path(), Some(readme.as_path()));
        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.document_path(), Some(guide.as_path()));
        assert_eq!(app.history.entries, vec![readme.clone(), guide.clone()]);
        assert_eq!(app.frecency.visits(&readme), 1);
        assert_eq!(app.frecency.visits(&guide), 1);
    }

    #[test]
    fn nested_history_truncates_the_forward_branch_after_new_navigation() {
        let temp = TempDir::new();
        for name in ["a.md", "b.md", "c.md", "d.md"] {
            fs::write(temp.0.join(name), format!("# {name}")).unwrap();
        }
        let root = temp.0.canonicalize().unwrap();
        let paths =
            ["a.md", "b.md", "c.md", "d.md"].map(|name| root.join(name).canonicalize().unwrap());
        let mut app = App::new(
            root.clone(),
            test_index(&root),
            Frecency::default(),
            Some(paths[0].clone()),
            test_config(&temp),
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();
        app.open(paths[1].clone()).unwrap();
        app.open(paths[2].clone()).unwrap();

        app.move_history(-1);
        assert_eq!(app.document_path(), Some(paths[1].as_path()));
        app.open(paths[3].clone()).unwrap();

        assert_eq!(app.history.entries, vec![paths[0].clone(), paths[1].clone(), paths[3].clone()]);
        assert!(app.history.target(1).is_none());
        app.move_history(-1);
        assert_eq!(app.document_path(), Some(paths[1].as_path()));
        app.move_history(-1);
        assert_eq!(app.document_path(), Some(paths[0].as_path()));
        app.move_history(1);
        assert_eq!(app.document_path(), Some(paths[1].as_path()));
        app.move_history(1);
        assert_eq!(app.document_path(), Some(paths[3].as_path()));
    }

    #[test]
    fn document_links_resolve_relative_file_urls_and_reject_external_urls() {
        let temp = TempDir::new();
        fs::create_dir_all(temp.0.join("docs")).unwrap();
        let current = temp.0.join("docs/index.md");
        let target = temp.0.join("docs/guide one.md");
        fs::write(&current, "# Index").unwrap();
        fs::write(&target, "# Guide").unwrap();

        assert_eq!(resolve_document_link(&current, "guide%20one.md#usage").unwrap(), target);
        assert!(resolve_document_link(&current, "https://example.com/guide.md").is_err());
    }

    #[test]
    fn document_search_finds_rendered_lines_and_navigates_results() {
        let temp = TempDir::new();
        let mut source = String::from("# Search demo\n\nFirst needle result.\n\n");
        for index in 0..30 {
            source.push_str(&format!("Paragraph {index} without the term.\n\n"));
        }
        source.push_str("Last NEEDLE result.\n");
        fs::write(temp.0.join("README.md"), source).unwrap();
        let root = temp.0.canonicalize().unwrap();
        let mut app = App::new(
            root.clone(),
            test_index(&root),
            Frecency::default(),
            Some(PathBuf::from("README.md")),
            test_config(&temp),
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();
        app.start_document_search("needle");
        let backend = TestBackend::new(70, 12);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let search = app.search.as_ref().unwrap();
        assert_eq!(search.matches.len(), 2);
        assert_eq!(search.selected, 0);
        assert_eq!(app.scroll_top, search.matches[0]);

        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        let search = app.search.as_ref().unwrap();
        assert_eq!(search.selected, 1);
        assert_eq!(app.scroll_top, search.matches[1].min(app.max_scroll()));

        app.on_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        assert_eq!(app.search.as_ref().unwrap().selected, 0);
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.search.is_none());
    }

    #[test]
    fn control_f_opens_document_find_input() {
        let temp = TempDir::new();
        fs::write(temp.0.join("README.md"), "# Search demo").unwrap();
        let root = temp.0.canonicalize().unwrap();
        let mut app = App::new(
            root.clone(),
            test_index(&root),
            Frecency::default(),
            Some(PathBuf::from("README.md")),
            test_config(&temp),
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();

        app.on_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));

        assert_eq!(app.input.value(), "/find ");
        assert!(app.suggestions.is_none());
    }

    #[test]
    fn configure_command_toggles_and_persists_ignored_visibility() {
        let temp = TempDir::new();
        let config = test_config(&temp);
        let mut app = App::new(
            temp.0.clone(),
            SearchIndex::empty(),
            Frecency::default(),
            None,
            config,
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();

        app.input.set("/configure".to_owned());
        assert_eq!(app.submit_typed(), Flow::Continue);
        assert!(matches!(app.surface, Surface::Settings(_)));
        assert!(app.config.show_git_ignored());

        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(!app.config.show_git_ignored());
        assert_eq!(app.theme_spec, "terminal");
        let reloaded =
            Config::load(crate::framework::ConfigStore::rooted(temp.0.join("config"))).unwrap();
        assert!(!reloaded.show_git_ignored());
        assert_eq!(reloaded.theme(), "terminal");
    }

    #[test]
    fn theme_command_applies_and_persists_a_custom_theme_file() {
        let temp = TempDir::new();
        let path = temp.0.join("custom-theme.toml");
        fs::write(&path, "base = 'nord'\n\n[colors]\naccent = '#010203'\ntext = '#040506'\n")
            .unwrap();
        let config = test_config(&temp);
        let mut app = App::new(
            temp.0.clone(),
            SearchIndex::empty(),
            Frecency::default(),
            None,
            config,
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();

        app.input.set(format!("/theme {}", path.display()));
        assert_eq!(app.submit_typed(), Flow::Continue);
        assert_eq!(app.theme.accent, ratatui::style::Color::Rgb(1, 2, 3));
        assert_eq!(app.theme.text, ratatui::style::Color::Rgb(4, 5, 6));
        assert_eq!(app.theme_spec, path.canonicalize().unwrap().to_string_lossy());

        let reloaded =
            Config::load(crate::framework::ConfigStore::rooted(temp.0.join("config"))).unwrap();
        assert_eq!(reloaded.theme(), app.theme_spec);

        app.input.set("/theme missing-theme".to_owned());
        assert_eq!(app.submit_typed(), Flow::Continue);
        assert!(app.notice.error);
        assert_eq!(app.theme.accent, ratatui::style::Color::Rgb(1, 2, 3));
    }

    #[test]
    fn bare_theme_command_opens_a_filterable_persistent_picker() {
        let temp = TempDir::new();
        let config = test_config(&temp);
        fs::create_dir_all(config.theme_dir()).unwrap();
        fs::write(
            config.theme_dir().join("solarized.toml"),
            "base = 'nord'\n\n[colors]\naccent = '#268bd2'\n",
        )
        .unwrap();
        let mut app = App::new(
            temp.0.clone(),
            SearchIndex::empty(),
            Frecency::default(),
            None,
            config,
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();

        app.input.set("/theme".to_owned());
        assert_eq!(app.submit_typed(), Flow::Continue);
        assert_eq!(app.input.value(), "/theme ");
        assert_eq!(app.suggestions.as_ref().map(SuggestionMenu::len), Some(3));

        app.input.set("/theme termina".to_owned());
        app.refresh_suggestions();
        assert_eq!(
            app.suggestions
                .as_ref()
                .and_then(SuggestionMenu::first)
                .map(|suggestion| suggestion.insert.as_str()),
            Some("/theme terminal")
        );
        app.cycle_selection(1);
        assert_eq!(app.submit_selected(), Flow::Continue);
        assert_eq!(app.theme_spec, "terminal");

        let reloaded =
            Config::load(crate::framework::ConfigStore::rooted(temp.0.join("config"))).unwrap();
        assert_eq!(reloaded.theme(), "terminal");
    }

    #[test]
    fn scrolling_clamps_to_rendered_geometry() {
        let temp = TempDir::new();
        let config = test_config(&temp);
        let mut app = App::new(
            temp.0.clone(),
            SearchIndex::empty(),
            Frecency::default(),
            None,
            config,
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();
        app.set_geometry(100, 20);
        app.scroll_by(10_000);
        assert_eq!(app.scroll_top, 80);
        app.page_by(-1);
        assert_eq!(app.scroll_top, 60);

        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
        assert_eq!(app.scroll_top, 0);
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        assert_eq!(app.scroll_top, 80);
    }
}
