use std::{
    collections::HashSet,
    fs, future,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ignore::WalkBuilder;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Padding, Paragraph},
    Frame,
};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::time::{self, Instant};
use unicode_width::UnicodeWidthStr;

use super::config::{self, Config};
use crate::tui::{
    fuzzy,
    markdown::MarkdownRenderer,
    theme::{self, TuiTheme},
    CommandSet, CommandSpec, EventReader, LineEditor, ParsedInput, Session, SessionOptions,
    SettingsEditor, SettingsFlow, Suggestion, SuggestionMenu,
};

const SUGGESTION_ROWS: usize = 8;
const SCROLL_STEP: isize = 3;
const WATCH_SETTLE_TIME: Duration = Duration::from_millis(60);
const COMMANDS: CommandSet = CommandSet::new(&[
    CommandSpec {
        name: "configure",
        aliases: &["config"],
        usage: "/configure",
        description: "configure Markdown discovery",
    },
    CommandSpec {
        name: "theme",
        aliases: &[],
        usage: "/theme <nord|terminal|path>",
        description: "change and persist the render theme",
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
    let catalog = Catalog::discover(&root);
    let mut app = App::new(root, catalog, initial, config, theme_spec, theme)?;
    let mut session = Session::open(SessionOptions { mouse_capture: true })?;
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
                Some(Event::Mouse(mouse)) => app.on_mouse(mouse),
                Some(Event::Resize(_, _)) => {}
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
        }
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
    catalog: Catalog,
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
    notice: Notice,
}

impl App {
    fn new(
        root: PathBuf,
        catalog: Catalog,
        initial: Option<PathBuf>,
        config: Config,
        theme_spec: String,
        theme: TuiTheme,
    ) -> Result<Self> {
        let mut app = Self {
            root,
            catalog,
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
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u') {
            self.clear_search();
            return Flow::Continue;
        }
        if key.modifiers.is_empty()
            && key.code == KeyCode::Char('r')
            && self.input.value().is_empty()
            && self.document.is_some()
        {
            self.reload_document(true);
            return Flow::Continue;
        }

        let menu = self.suggestions.is_some();
        let engaged = self.suggestions.as_ref().is_some_and(SuggestionMenu::is_engaged);
        match key.code {
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
            MouseEventKind::ScrollUp => self.scroll_by(-SCROLL_STEP),
            MouseEventKind::ScrollDown => self.scroll_by(SCROLL_STEP),
            _ => {}
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
            "help" => {
                self.input.clear();
                self.suggestions = None;
                self.notice = Notice::info(
                    "/configure changes settings · /theme changes appearance · /quit exits"
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

        let mut ranked = choices
            .into_iter()
            .filter_map(|(spec, hint)| {
                let score = if query.is_empty() { 0 } else { fuzzy::score_ci(&spec, query)? };
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

    fn reload_document(&mut self, announce_unchanged: bool) {
        let Some(path) = self.document_path().map(Path::to_path_buf) else {
            return;
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
            }
            Ok(document) => {
                self.notice = Notice::info(format!("reloaded {}", document.display));
                self.document = Some(document);
            }
            Err(error) => {
                self.notice = Notice::error(format!("reload document: {error:#}"));
            }
        }
    }

    fn open(&mut self, path: PathBuf) -> Result<()> {
        let path = if path.is_absolute() { path } else { self.root.join(path) };
        let document = Document::load(&self.root, path)?;
        self.notice = Notice::info(format!("opened {} · r refreshes", document.display));
        self.document = Some(document);
        self.scroll_top = 0;
        self.input.clear();
        self.suggestions = None;
        Ok(())
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
            let candidates = COMMANDS
                .suggestions(&query)
                .into_iter()
                .map(|command| Suggestion::new(format!("/{}", command.name), command.description))
                .collect::<Vec<_>>();
            self.set_suggestions(&query, candidates);
            return;
        }
        if query.is_empty() && self.document.is_some() {
            self.suggestions = None;
            return;
        }

        let current = self.document.as_ref().map(|document| document.path.as_path());
        let candidates = self.catalog.suggestions(&query, current, self.config.show_git_ignored());
        self.set_suggestions(&query, candidates);
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

struct Catalog {
    entries: Vec<FileEntry>,
}

impl Catalog {
    fn discover(root: &Path) -> Self {
        let mut entries = WalkBuilder::new(root)
            .follow_links(false)
            .standard_filters(true)
            .build()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .filter(|entry| is_markdown(entry.path()))
            .map(|entry| FileEntry::new(root, entry.into_path(), false))
            .collect::<Vec<_>>();

        let mut known = entries.iter().map(|entry| entry.path.clone()).collect::<HashSet<_>>();
        for relative in git_ignored_markdown(root) {
            let path = root.join(relative);
            let Ok(path) = path.canonicalize() else {
                continue;
            };
            if !path.starts_with(root)
                || !path.is_file()
                || !is_markdown(&path)
                || !known.insert(path.clone())
            {
                continue;
            }
            entries.push(FileEntry::new(root, path, true));
        }

        entries
            .sort_by(|left, right| left.display.to_lowercase().cmp(&right.display.to_lowercase()));
        Self { entries }
    }

    fn suggestions(
        &self,
        query: &str,
        current: Option<&Path>,
        show_git_ignored: bool,
    ) -> Vec<Suggestion> {
        let needle = query.strip_prefix("./").unwrap_or(query);
        let mut ranked = self
            .entries
            .iter()
            .filter(|entry| show_git_ignored || !entry.ignored)
            .filter_map(|entry| {
                let rank = if needle.is_empty() {
                    (3, 0)
                } else if entry.display.eq_ignore_ascii_case(needle) {
                    (0, 0)
                } else if let Some(score) = fuzzy::score_ci(&entry.basename, needle) {
                    (1, score)
                } else {
                    (2, fuzzy::score_ci(&entry.display, needle)?)
                };
                Some((rank, entry))
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|(left_rank, left), (right_rank, right)| {
            left_rank.cmp(right_rank).then_with(|| left.display.cmp(&right.display))
        });
        ranked
            .into_iter()
            .map(|(_, entry)| {
                let mut hint = Vec::with_capacity(3);
                if entry.ignored {
                    hint.push("ignored".to_owned());
                }
                if current == Some(entry.path.as_path()) {
                    hint.push("open".to_owned());
                }
                hint.push(format_bytes(entry.bytes));
                Suggestion::new(entry.display.clone(), hint.join(" · "))
            })
            .collect()
    }
}

struct FileEntry {
    path: PathBuf,
    display: String,
    basename: String,
    bytes: u64,
    ignored: bool,
}

impl FileEntry {
    fn new(root: &Path, path: PathBuf, ignored: bool) -> Self {
        let display = display_path(root, &path);
        let basename = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| display.clone());
        let bytes = path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        Self { path, display, basename, bytes, ignored }
    }
}

fn git_ignored_markdown(root: &Path) -> Vec<PathBuf> {
    let output = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--",
            ":(icase)*.md",
            ":(icase)*.markdown",
            ":(icase)*.mdown",
            ":(icase)*.mkd",
            ":(icase)*.mdx",
        ])
        .current_dir(root)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
        .filter_map(|raw| std::str::from_utf8(raw).ok())
        .map(PathBuf::from)
        .collect()
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
    let Some(document) = &app.document else {
        let body = vec![
            Line::from(""),
            Line::from(Span::styled(
                "Type a Markdown filename below.",
                Style::default().fg(app.theme.text_strong).add_modifier(Modifier::BOLD),
            )),
            Line::from("Use fuzzy fragments from any part of the path, then Tab or ↑/↓ to select."),
            Line::from("Type /configure to control whether Git-ignored Markdown is shown."),
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

    let inner_width = area.width.saturating_sub(4).max(1);
    let text = MarkdownRenderer::new(app.theme).render(&document.markdown, inner_width);
    let content_height = text.lines.len();
    let paragraph = Paragraph::new(text);
    let viewport_height = area.height.saturating_sub(2) as usize;
    app.content_height = content_height;
    app.viewport_height = viewport_height;
    let max_scroll = content_height.saturating_sub(viewport_height).min(u16::MAX as usize);
    app.scroll_top = app.scroll_top.min(max_scroll);
    let title = if max_scroll == 0 {
        " markdown ".to_owned()
    } else {
        format!(" markdown ─ {}/{} ", app.scroll_top + 1, max_scroll + 1)
    };
    let scroll = app.scroll_top.min(u16::MAX as usize) as u16;
    frame.render_widget(paragraph.block(panel(title, app.theme)).scroll((scroll, 0)), area);
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
        let (hint, color) = if !app.notice.text.is_empty() {
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

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned()
}

fn is_markdown(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| {
        matches!(
            extension.to_ascii_lowercase().as_str(),
            "md" | "markdown" | "mdown" | "mkd" | "mdx"
        )
    })
}

fn format_bytes(bytes: u64) -> String {
    match bytes {
        0..=999 => format!("{bytes} B"),
        1_000..=999_999 => format!("{:.1} KB", bytes as f64 / 1_000.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_000_000.0),
    }
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

    #[test]
    fn catalog_classifies_gitignored_markdown() {
        let temp = TempDir::new();
        let status = Command::new("git").args(["init", "--quiet"]).current_dir(&temp.0).status();
        assert!(status.is_ok_and(|status| status.success()));
        fs::create_dir_all(temp.0.join("docs")).unwrap();
        fs::write(temp.0.join("README.md"), "# Read me").unwrap();
        fs::write(temp.0.join("docs/guide.markdown"), "# Guide").unwrap();
        fs::write(temp.0.join("ignored.md"), "# Ignore me").unwrap();
        fs::write(temp.0.join("notes.txt"), "not Markdown").unwrap();
        fs::write(temp.0.join(".gitignore"), "ignored.md\n").unwrap();

        let catalog = Catalog::discover(&temp.0);
        let paths = catalog.entries.iter().map(|entry| entry.display.as_str()).collect::<Vec<_>>();
        assert_eq!(paths, vec!["docs/guide.markdown", "ignored.md", "README.md"]);
        assert!(
            catalog.entries.iter().find(|entry| entry.display == "ignored.md").unwrap().ignored
        );

        let hidden = catalog.suggestions("ignored", None, false);
        assert!(hidden.is_empty());
        let shown = catalog.suggestions("ignored", None, true);
        assert_eq!(shown[0].insert, "ignored.md");
        assert!(shown[0].hint.contains("ignored"));
    }

    #[test]
    fn fuzzy_search_prefers_basename_then_full_path() {
        let catalog = Catalog {
            entries: vec![
                FileEntry {
                    path: PathBuf::from("/repo/docs/setup.md"),
                    display: "docs/setup.md".to_owned(),
                    basename: "setup.md".to_owned(),
                    bytes: 10,
                    ignored: false,
                },
                FileEntry {
                    path: PathBuf::from("/repo/setup/notes.md"),
                    display: "setup/notes.md".to_owned(),
                    basename: "notes.md".to_owned(),
                    bytes: 20,
                    ignored: false,
                },
            ],
        };

        let suggestions = catalog.suggestions("setup", None, true);
        assert_eq!(suggestions[0].insert, "docs/setup.md");
        assert_eq!(suggestions[1].insert, "setup/notes.md");
    }

    #[test]
    fn selecting_a_fuzzy_match_opens_and_clears_the_prompt() {
        let temp = TempDir::new();
        fs::create_dir_all(temp.0.join("docs")).unwrap();
        fs::write(temp.0.join("docs/guide.md"), "# Guide").unwrap();
        let root = temp.0.canonicalize().unwrap();
        let catalog = Catalog::discover(&root);
        let config = test_config(&temp);
        let mut app =
            App::new(root, catalog, None, config, "nord".to_owned(), theme::NORD).unwrap();

        app.input.set("guide".to_owned());
        app.refresh_suggestions();
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
            Catalog::discover(&root),
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
            Catalog::discover(&root),
            Some(path.clone()),
            test_config(&temp),
            "nord".to_owned(),
            theme::NORD,
        )
        .unwrap();
        app.set_geometry(100, 20);
        app.scroll_top = 12;
        fs::write(path, "# After").unwrap();

        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            Flow::Continue
        );

        assert_eq!(
            app.document.as_ref().map(|document| document.markdown.as_str()),
            Some("# After")
        );
        assert_eq!(app.scroll_top, 12);
        assert!(app.input.value().is_empty());
        assert_eq!(app.notice.text, "reloaded README.md");
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
        let catalog = Catalog {
            entries: vec![FileEntry {
                path: PathBuf::from("/repo/README.md"),
                display: "README.md".to_owned(),
                basename: "README.md".to_owned(),
                bytes: 10,
                ignored: false,
            }],
        };
        let suggestions = catalog.suggestions("readme.md", None, true);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].insert, "README.md");
    }

    #[test]
    fn viewer_renders_markdown_and_bottom_file_prompt() {
        let temp = TempDir::new();
        fs::write(temp.0.join("README.md"), "# Hello\n\nThis is **Markdown**.").unwrap();
        let root = temp.0.canonicalize().unwrap();
        let catalog = Catalog::discover(&root);
        let config = test_config(&temp);
        let mut app = App::new(
            root,
            catalog,
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
    fn configure_command_toggles_and_persists_ignored_visibility() {
        let temp = TempDir::new();
        let config = test_config(&temp);
        let mut app = App::new(
            temp.0.clone(),
            Catalog { entries: Vec::new() },
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
            Catalog { entries: Vec::new() },
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
            Catalog { entries: Vec::new() },
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
            Catalog { entries: Vec::new() },
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
    }
}
