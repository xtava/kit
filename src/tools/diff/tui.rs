use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Context as _, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Direction as LayoutDirection, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::config::LineNumbers;
use super::git::{load_repository, stage_document, unstage_document};
use super::model::{
    ChangeGroup, ChangeKind, DiffBody, DiffContext, DiffDocument, LineCell, RowKind, SpecialState,
    TextDiffDocument, TextSnapshot,
};
use crate::tui::theme::TuiTheme;
use crate::tui::{
    Direction, EventReader, NavigationMap, NavigationRegion, Session, SessionOptions,
};

const WIDE_MIN_WIDTH: u16 = 84;
const MIN_WIDTH: u16 = 30;
const MIN_HEIGHT: u16 = 8;
const TREE_WIDTH: u16 = 34;
const TREE_ACTION_WIDTH: u16 = 3;
const CHANGE_INDICATOR_WIDTH: usize = 2;
const SPLIT_GUTTER_WIDTH: usize = 8;
const SPLIT_MIN_WIDTH: u16 = 50;
const DARK_ADDED_BACKGROUND: Color = Color::Rgb(33, 58, 43);
const DARK_DELETED_BACKGROUND: Color = Color::Rgb(74, 34, 29);
const LIGHT_ADDED_BACKGROUND: Color = Color::Rgb(218, 251, 225);
const LIGHT_DELETED_BACKGROUND: Color = Color::Rgb(255, 235, 233);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ViewMode {
    Auto,
    Inline,
    Split,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EffectiveMode {
    Single,
    Inline,
    Split,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChangeSide {
    Addition,
    Deletion,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveRegion {
    Changes,
    Old,
    New,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReviewAnchor {
    hunk: usize,
    row: Option<usize>,
}

pub async fn run(
    cwd: PathBuf,
    documents: Vec<DiffDocument>,
    theme: TuiTheme,
    mouse_capture: bool,
    mode: ViewMode,
    context: DiffContext,
    line_numbers: LineNumbers,
) -> Result<()> {
    let mut app = DiffApp::with_line_numbers(documents, theme, mode, line_numbers);
    let mut session = Session::open(SessionOptions { mouse_capture, bracketed_paste: false })?;
    let mut events = EventReader::start();
    let mut repository_task = None;

    loop {
        session.draw(|frame| render(frame, &mut app))?;
        let event = if let Some(task) = repository_task.as_mut() {
            tokio::select! {
                event = events.recv() => RuntimeEvent::Terminal(event),
                result = task => RuntimeEvent::RepositoryUpdated(result),
            }
        } else {
            RuntimeEvent::Terminal(events.recv().await)
        };
        let flow = match event {
            RuntimeEvent::Terminal(event) => {
                handle_terminal_events(&mut app, event, || events.try_recv())
            }
            RuntimeEvent::RepositoryUpdated(result) => {
                repository_task = None;
                app.finish_repository_operation(match result {
                    Ok(result) => result,
                    Err(error) => Err(anyhow!("repository task failed: {error}")),
                });
                Flow::Continue
            }
        };
        match flow {
            Flow::Quit => break,
            Flow::Refresh if repository_task.is_none() => {
                let operation = RepositoryOperation::Refresh;
                app.repository_status = Some(RepositoryStatus::Running(operation.running_label()));
                repository_task = Some(spawn_repository_operation(cwd.clone(), operation, context));
            }
            Flow::ToggleStage if repository_task.is_none() => {
                if let Some(operation) = app.selected_repository_operation() {
                    app.repository_status =
                        Some(RepositoryStatus::Running(operation.running_label()));
                    repository_task =
                        Some(spawn_repository_operation(cwd.clone(), operation, context));
                }
            }
            Flow::Continue | Flow::Refresh | Flow::ToggleStage => {}
        }
    }
    Ok(())
}

fn handle_terminal_events(
    app: &mut DiffApp,
    event: Option<Event>,
    mut try_recv: impl FnMut() -> Option<Event>,
) -> Flow {
    let mut flow = handle_terminal_event(app, event);
    while flow == Flow::Continue {
        let Some(event) = try_recv() else {
            break;
        };
        flow = handle_terminal_event(app, Some(event));
    }
    flow
}

fn handle_terminal_event(app: &mut DiffApp, event: Option<Event>) -> Flow {
    match event {
        Some(Event::Key(key)) if key.is_press() => app.on_key(key),
        Some(Event::Mouse(mouse)) => app.on_mouse(mouse),
        Some(Event::Resize(_, _)) => Flow::Continue,
        None => Flow::Quit,
        _ => Flow::Continue,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Flow {
    Continue,
    Refresh,
    ToggleStage,
    Quit,
}

enum RuntimeEvent {
    Terminal(Option<Event>),
    RepositoryUpdated(
        std::result::Result<Result<(Vec<DiffDocument>, &'static str)>, tokio::task::JoinError>,
    ),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RepositoryStatus {
    Running(&'static str),
    Success(&'static str),
    Error(String),
}

#[derive(Clone, Debug)]
enum RepositoryOperation {
    Refresh,
    Stage(DiffDocument),
    Unstage(DiffDocument),
}

impl RepositoryOperation {
    fn running_label(&self) -> &'static str {
        match self {
            Self::Refresh => "refreshing…",
            Self::Stage(_) => "staging…",
            Self::Unstage(_) => "unstaging…",
        }
    }

    fn success_label(&self) -> &'static str {
        match self {
            Self::Refresh => "refreshed",
            Self::Stage(_) => "staged",
            Self::Unstage(_) => "unstaged",
        }
    }

    fn execute(&self, cwd: &Path) -> Result<()> {
        match self {
            Self::Refresh => Ok(()),
            Self::Stage(document) => stage_document(cwd, document).context("stage selected file"),
            Self::Unstage(document) => {
                unstage_document(cwd, document).context("unstage selected file")
            }
        }
    }
}

fn spawn_repository_operation(
    cwd: PathBuf,
    operation: RepositoryOperation,
    context: DiffContext,
) -> tokio::task::JoinHandle<Result<(Vec<DiffDocument>, &'static str)>> {
    tokio::task::spawn_blocking(move || {
        operation.execute(&cwd)?;
        let documents =
            load_repository(&cwd, context).context("reload repository after operation")?;
        Ok((documents, operation.success_label()))
    })
}

#[derive(Clone, Debug)]
enum TreeTarget {
    Group(ChangeGroup),
    Directory(ChangeGroup, PathBuf),
    File(usize),
}

#[derive(Clone, Debug)]
struct TreeRow {
    target: TreeTarget,
    depth: usize,
    label: String,
    additions: usize,
    deletions: usize,
    kind: Option<ChangeKind>,
}

#[derive(Default)]
struct UiRegions {
    tree: Vec<(Rect, TreeTarget)>,
    tree_actions: Vec<(Rect, usize)>,
    tree_area: Rect,
    content_area: Rect,
    content_inner: Rect,
    divider: Option<Rect>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProjectionKey {
    document: usize,
    mode: EffectiveMode,
    width: usize,
    old_horizontal_scroll: usize,
    new_horizontal_scroll: usize,
    divider_percent: u16,
    line_numbers: LineNumbers,
}

struct DocumentProjection {
    key: ProjectionKey,
    lines: Vec<RenderedLine>,
}

struct DiffApp {
    documents: Vec<DiffDocument>,
    selected: Option<usize>,
    expanded: HashSet<(ChangeGroup, PathBuf)>,
    tree_scroll: usize,
    content_scroll: usize,
    old_horizontal_scroll: usize,
    new_horizontal_scroll: usize,
    selected_hunk: usize,
    anchor: ReviewAnchor,
    document_projection: Option<DocumentProjection>,
    restore_anchor: bool,
    mode: ViewMode,
    last_effective_mode: Option<EffectiveMode>,
    active_region: ActiveRegion,
    divider_percent: u16,
    dragging_divider: bool,
    hovered_file: Option<usize>,
    repository_status: Option<RepositoryStatus>,
    theme: TuiTheme,
    line_numbers: LineNumbers,
    regions: UiRegions,
}

impl DiffApp {
    #[cfg(test)]
    fn new(documents: Vec<DiffDocument>, theme: TuiTheme, mode: ViewMode) -> Self {
        Self::with_line_numbers(documents, theme, mode, LineNumbers::Auto)
    }

    fn with_line_numbers(
        documents: Vec<DiffDocument>,
        theme: TuiTheme,
        mode: ViewMode,
        line_numbers: LineNumbers,
    ) -> Self {
        let expanded = directory_keys(&documents).into_iter().collect();
        Self {
            selected: (!documents.is_empty()).then_some(0),
            documents,
            expanded,
            tree_scroll: 0,
            content_scroll: 0,
            old_horizontal_scroll: 0,
            new_horizontal_scroll: 0,
            selected_hunk: 0,
            anchor: ReviewAnchor { hunk: 0, row: None },
            document_projection: None,
            restore_anchor: false,
            mode,
            last_effective_mode: None,
            active_region: ActiveRegion::Changes,
            divider_percent: 50,
            dragging_divider: false,
            hovered_file: None,
            repository_status: None,
            theme,
            line_numbers,
            regions: UiRegions::default(),
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Flow {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return Flow::Quit;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Flow::Quit,
            KeyCode::Char('r') if key.modifiers.is_empty() => return Flow::Refresh,
            KeyCode::Char('s') if key.modifiers.is_empty() => return Flow::ToggleStage,
            KeyCode::Down | KeyCode::Char('j') => match self.active_region {
                ActiveRegion::Changes => self.select_relative(1),
                ActiveRegion::Old | ActiveRegion::New => self.scroll_content(1),
            },
            KeyCode::Up | KeyCode::Char('k') => match self.active_region {
                ActiveRegion::Changes => self.select_relative(-1),
                ActiveRegion::Old | ActiveRegion::New => self.scroll_content(-1),
            },
            KeyCode::PageDown => self.scroll_content(self.regions.content_area.height as isize - 2),
            KeyCode::PageUp => {
                self.scroll_content(-(self.regions.content_area.height as isize - 2))
            }
            KeyCode::Char('n') | KeyCode::Char(']') => self.select_hunk(1),
            KeyCode::Char('N') | KeyCode::Char('[') => self.select_hunk(-1),
            KeyCode::Char('v') => self.toggle_mode(),
            KeyCode::Tab => self.move_tab(1),
            KeyCode::BackTab => self.move_tab(-1),
            KeyCode::Right => self.move_region(Direction::Right),
            KeyCode::Left => self.move_region(Direction::Left),
            KeyCode::Char('l') => self.pan(4),
            KeyCode::Char('h') => self.pan(-4),
            KeyCode::Home => {
                self.content_scroll = 0;
                self.old_horizontal_scroll = 0;
                self.new_horizontal_scroll = 0;
                self.sync_anchor_from_scroll();
            }
            KeyCode::End => {
                self.content_scroll = self
                    .document_projection
                    .as_ref()
                    .map_or(0, |projection| projection.lines.len().saturating_sub(1));
                self.sync_anchor_from_scroll();
            }
            _ => {}
        }
        Flow::Continue
    }

    fn finish_repository_operation(&mut self, result: Result<(Vec<DiffDocument>, &'static str)>) {
        match result {
            Ok((documents, label)) => {
                self.replace_documents(documents);
                self.repository_status = Some(RepositoryStatus::Success(label));
            }
            Err(error) => self.repository_status = Some(RepositoryStatus::Error(error.to_string())),
        }
    }

    fn selected_repository_operation(&self) -> Option<RepositoryOperation> {
        let document = self.selected.and_then(|index| self.documents.get(index))?.clone();
        Some(match document.group {
            ChangeGroup::Staged => RepositoryOperation::Unstage(document),
            ChangeGroup::Changes => RepositoryOperation::Stage(document),
        })
    }

    fn replace_documents(&mut self, documents: Vec<DiffDocument>) {
        self.hovered_file = None;
        let previous_index = self.selected.unwrap_or(0);
        let previous_identity = self
            .selected
            .and_then(|index| self.documents.get(index))
            .map(|document| (document.group, document.display_path().map(Path::to_path_buf)));
        let previous_keys = directory_keys(&self.documents).into_iter().collect::<HashSet<_>>();
        let new_keys = directory_keys(&documents);
        self.expanded = new_keys
            .into_iter()
            .filter(|key| !previous_keys.contains(key) || self.expanded.contains(key))
            .collect();
        self.documents = documents;
        self.selected = previous_identity
            .as_ref()
            .and_then(|(group, path)| {
                self.documents.iter().position(|document| {
                    document.group == *group && document.display_path() == path.as_deref()
                })
            })
            .or_else(|| {
                previous_identity.as_ref().and_then(|(_, path)| {
                    self.documents
                        .iter()
                        .position(|document| document.display_path() == path.as_deref())
                })
            })
            .or_else(|| {
                (!self.documents.is_empty()).then_some(previous_index.min(self.documents.len() - 1))
            });
        self.reset_document_position();
        if self.selected.is_none() {
            self.tree_scroll = 0;
        }
    }

    fn reset_document_position(&mut self) {
        self.content_scroll = 0;
        self.old_horizontal_scroll = 0;
        self.new_horizontal_scroll = 0;
        self.selected_hunk = 0;
        self.anchor = ReviewAnchor { hunk: 0, row: None };
        self.document_projection = None;
        self.restore_anchor = false;
        self.last_effective_mode = None;
        self.dragging_divider = false;
    }

    fn on_mouse(&mut self, mouse: MouseEvent) -> Flow {
        let point = Rect::new(mouse.column, mouse.row, 1, 1);
        match mouse.kind {
            MouseEventKind::Moved => {
                self.hovered_file = self.regions.tree.iter().find_map(|(area, target)| {
                    if area.intersects(point) {
                        match target {
                            TreeTarget::File(index) => Some(*index),
                            _ => None,
                        }
                    } else {
                        None
                    }
                });
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = self
                    .regions
                    .tree_actions
                    .iter()
                    .find(|(area, _)| area.intersects(point))
                    .map(|(_, index)| *index)
                {
                    self.select(index);
                    return Flow::ToggleStage;
                }
                if self.regions.divider.is_some_and(|area| area.intersects(point)) {
                    self.dragging_divider = true;
                    return Flow::Continue;
                }
                if let Some(target) = self
                    .regions
                    .tree
                    .iter()
                    .find(|(area, _)| area.intersects(point))
                    .map(|(_, target)| target.clone())
                {
                    self.active_region = ActiveRegion::Changes;
                    self.activate_tree_target(target);
                } else if self.regions.content_inner.intersects(point) {
                    self.active_region =
                        if self.regions.divider.is_some_and(|divider| mouse.column < divider.x) {
                            ActiveRegion::Old
                        } else {
                            ActiveRegion::New
                        };
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging_divider => {
                self.drag_divider(mouse.column)
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging_divider = false,
            MouseEventKind::ScrollDown if self.regions.tree_area.intersects(point) => {
                self.hovered_file = None;
                self.tree_scroll = self.tree_scroll.saturating_add(1)
            }
            MouseEventKind::ScrollUp if self.regions.tree_area.intersects(point) => {
                self.hovered_file = None;
                self.tree_scroll = self.tree_scroll.saturating_sub(1)
            }
            MouseEventKind::ScrollDown if self.regions.content_area.intersects(point) => {
                self.hovered_file = None;
                if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                    self.pan(4)
                } else {
                    self.scroll_content(1)
                }
            }
            MouseEventKind::ScrollUp if self.regions.content_area.intersects(point) => {
                self.hovered_file = None;
                if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                    self.pan(-4)
                } else {
                    self.scroll_content(-1)
                }
            }
            _ => {}
        }
        Flow::Continue
    }

    fn activate_tree_target(&mut self, target: TreeTarget) {
        match target {
            TreeTarget::File(index) => self.select(index),
            TreeTarget::Directory(group, path) => {
                let key = (group, path);
                if !self.expanded.remove(&key) {
                    self.expanded.insert(key);
                }
            }
            TreeTarget::Group(group) => {
                let key = (group, PathBuf::new());
                if !self.expanded.remove(&key) {
                    self.expanded.insert(key);
                }
            }
        }
    }

    fn select_relative(&mut self, delta: isize) {
        if self.documents.is_empty() {
            return;
        }
        let current = self.selected.unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.documents.len() as isize - 1) as usize;
        self.select(next);
    }

    fn select(&mut self, index: usize) {
        if self.selected != Some(index) {
            self.selected = Some(index);
            self.reset_document_position();
            self.restore_anchor = true;
        }
        self.reveal(index);
    }

    fn reveal(&mut self, index: usize) {
        if let Some(document) = self.documents.get(index) {
            self.expanded.insert((document.group, PathBuf::new()));
            if let Some(path) = document.display_path() {
                let mut parent = PathBuf::new();
                let components: Vec<_> = path.components().collect();
                for component in components.iter().take(components.len().saturating_sub(1)) {
                    if let Component::Normal(value) = component {
                        parent.push(value);
                        self.expanded.insert((document.group, parent.clone()));
                    }
                }
            }
        }
    }

    fn scroll_content(&mut self, delta: isize) {
        self.content_scroll = self.content_scroll.saturating_add_signed(delta);
        self.sync_anchor_from_scroll();
    }

    fn select_hunk(&mut self, delta: isize) {
        let Some(document) = self.selected.and_then(|index| self.documents.get(index)) else {
            return;
        };
        let DiffBody::Text(text) = &document.body else {
            return;
        };
        if text.hunks.is_empty() {
            return;
        }
        self.selected_hunk =
            self.selected_hunk.saturating_add_signed(delta).min(text.hunks.len().saturating_sub(1));
        self.anchor = ReviewAnchor { hunk: self.selected_hunk, row: Some(0) };
        self.restore_anchor = true;
    }

    fn toggle_mode(&mut self) {
        self.mode = match self.base_mode(self.regions.content_inner.width) {
            EffectiveMode::Inline => ViewMode::Split,
            EffectiveMode::Split => ViewMode::Inline,
            EffectiveMode::Single => unreachable!("base mode is never one-sided"),
        };
        self.restore_anchor = true;
    }

    fn base_mode(&self, width: u16) -> EffectiveMode {
        match self.mode {
            ViewMode::Inline => EffectiveMode::Inline,
            ViewMode::Split => EffectiveMode::Split,
            ViewMode::Auto if width >= 72 => EffectiveMode::Split,
            ViewMode::Auto => EffectiveMode::Inline,
        }
    }

    fn effective_mode(&self, width: u16) -> EffectiveMode {
        if self.selected.and_then(|index| self.documents.get(index)).is_some_and(one_sided_document)
        {
            EffectiveMode::Single
        } else {
            self.base_mode(width)
        }
    }

    fn pan(&mut self, delta: isize) {
        if self.effective_mode(self.regions.content_inner.width) != EffectiveMode::Split {
            self.old_horizontal_scroll = self.old_horizontal_scroll.saturating_add_signed(delta);
            self.new_horizontal_scroll = self.new_horizontal_scroll.saturating_add_signed(delta);
            return;
        }
        let offset = match self.active_region {
            ActiveRegion::Changes => return,
            ActiveRegion::Old => &mut self.old_horizontal_scroll,
            ActiveRegion::New => &mut self.new_horizontal_scroll,
        };
        *offset = offset.saturating_add_signed(delta);
    }

    fn sync_anchor_from_scroll(&mut self) {
        if let Some(projection) = &self.document_projection {
            if let Some(line) = projection
                .lines
                .get(self.content_scroll.min(projection.lines.len().saturating_sub(1)))
            {
                self.anchor = line.anchor;
                self.selected_hunk = line.anchor.hunk;
            }
        }
    }

    fn drag_divider(&mut self, column: u16) {
        let area = self.regions.content_inner;
        if area.width <= 1 {
            return;
        }
        let relative = column.saturating_sub(area.x).min(area.width - 1);
        self.divider_percent =
            ((u32::from(relative) * 100) / u32::from(area.width)).clamp(25, 75) as u16;
    }

    fn navigation(&self) -> NavigationMap<ActiveRegion> {
        let mut regions =
            vec![NavigationRegion::new(ActiveRegion::Changes, self.regions.tree_area)];
        if let Some(divider) = self.regions.divider {
            regions.push(NavigationRegion::new(
                ActiveRegion::Old,
                Rect::new(
                    self.regions.content_inner.x,
                    self.regions.content_inner.y,
                    divider.x.saturating_sub(self.regions.content_inner.x),
                    self.regions.content_inner.height,
                ),
            ));
            let new_x = divider.right();
            regions.push(NavigationRegion::new(
                ActiveRegion::New,
                Rect::new(
                    new_x,
                    self.regions.content_inner.y,
                    self.regions.content_inner.right().saturating_sub(new_x),
                    self.regions.content_inner.height,
                ),
            ));
        } else {
            regions.push(NavigationRegion::new(ActiveRegion::New, self.regions.content_inner));
        }
        NavigationMap::new(regions)
    }

    fn normalize_active_region(&mut self) {
        if self.active_region == ActiveRegion::Old && self.regions.divider.is_none() {
            self.active_region = ActiveRegion::New;
        }
        if let Some(region) = self.navigation().normalize(self.active_region) {
            self.active_region = region;
        }
    }

    fn move_region(&mut self, direction: Direction) {
        if let Some(region) = self.navigation().neighbor(self.active_region, direction) {
            self.active_region = region;
        }
    }

    fn move_tab(&mut self, delta: isize) {
        let navigation = self.navigation();
        let region = if delta < 0 {
            navigation.previous(self.active_region)
        } else {
            navigation.next(self.active_region)
        };
        if let Some(region) = region {
            self.active_region = region;
        }
    }
}

fn one_sided_document(document: &DiffDocument) -> bool {
    matches!(document.kind, ChangeKind::Added | ChangeKind::Untracked | ChangeKind::Deleted)
}

fn render(frame: &mut Frame<'_>, app: &mut DiffApp) {
    let area = frame.area();
    app.regions = UiRegions::default();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new("kit diff needs at least 30 columns × 8 rows")
                .style(Style::default().fg(app.theme.warning)),
            area,
        );
        return;
    }

    let (tree_area, content_area) = if area.width >= WIDE_MIN_WIDTH {
        let chunks = Layout::default()
            .direction(LayoutDirection::Horizontal)
            .constraints([Constraint::Length(TREE_WIDTH), Constraint::Min(40)])
            .split(area);
        (chunks[0], chunks[1])
    } else {
        let tree_height = (area.height / 3).clamp(5, 10);
        let chunks = Layout::default()
            .direction(LayoutDirection::Vertical)
            .constraints([Constraint::Length(tree_height), Constraint::Min(3)])
            .split(area);
        (chunks[0], chunks[1])
    };
    app.regions.tree_area = tree_area;
    app.regions.content_area = content_area;
    render_tree(frame, tree_area, app);
    render_document(frame, content_area, app);
    app.normalize_active_region();
}

fn render_tree(frame: &mut Frame<'_>, area: Rect, app: &mut DiffApp) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if app.active_region == ActiveRegion::Changes {
            app.theme.accent
        } else {
            app.theme.border
        }))
        .title(" changes ")
        .title_style(Style::default().fg(app.theme.text_strong).add_modifier(Modifier::BOLD));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = tree_rows(&app.documents, &app.expanded);
    if let Some(selected) = app.selected {
        if let Some(position) = rows
            .iter()
            .position(|row| matches!(row.target, TreeTarget::File(index) if index == selected))
        {
            let height = inner.height as usize;
            if position < app.tree_scroll {
                app.tree_scroll = position;
            } else if position >= app.tree_scroll.saturating_add(height) {
                app.tree_scroll = position.saturating_sub(height.saturating_sub(1));
            }
        }
    }
    app.tree_scroll = app.tree_scroll.min(rows.len().saturating_sub(inner.height as usize));

    let lines = rows
        .iter()
        .skip(app.tree_scroll)
        .take(inner.height as usize)
        .enumerate()
        .map(|(visible_index, row)| {
            let selected =
                matches!(row.target, TreeTarget::File(index) if Some(index) == app.selected);
            let style = if selected {
                Style::default().fg(app.theme.text_strong).bg(app.theme.selection)
            } else {
                Style::default().fg(app.theme.text)
            };
            let row_area = Rect::new(inner.x, inner.y + visible_index as u16, inner.width, 1);
            app.regions.tree.push((row_area, row.target.clone()));
            let hover_action = match row.target {
                TreeTarget::File(index) if app.hovered_file == Some(index) => {
                    app.documents.get(index).map(|document| document.group)
                }
                _ => None,
            };
            if let (TreeTarget::File(index), Some(_)) = (&row.target, hover_action) {
                let action_width = TREE_ACTION_WIDTH.min(row_area.width);
                app.regions.tree_actions.push((
                    Rect::new(
                        row_area.x + row_area.width.saturating_sub(action_width),
                        row_area.y,
                        action_width,
                        1,
                    ),
                    *index,
                ));
            }
            tree_line(row, inner.width as usize, style, &app.expanded, hover_action, app.theme)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn tree_line(
    row: &TreeRow,
    width: usize,
    style: Style,
    expanded: &HashSet<(ChangeGroup, PathBuf)>,
    hover_action: Option<ChangeGroup>,
    theme: TuiTheme,
) -> Line<'static> {
    let prefix = match &row.target {
        TreeTarget::Group(group) if expanded.contains(&(*group, PathBuf::new())) => "▾ ",
        TreeTarget::Group(_) => "▸ ",
        TreeTarget::Directory(group, path) if expanded.contains(&(*group, path.clone())) => "▾ ",
        TreeTarget::Directory(_, _) => "▸ ",
        TreeTarget::File(_) => change_glyph(row.kind),
    };
    let indent = "  ".repeat(row.depth);
    let trailing = hover_action
        .map(|group| tree_action_spans(group, style, theme))
        .unwrap_or_else(|| tree_count_spans(row, style, theme));
    let reserved =
        trailing.iter().map(|span| UnicodeWidthStr::width(span.content.as_ref())).sum::<usize>();
    let available = width.saturating_sub(
        UnicodeWidthStr::width(indent.as_str()) + UnicodeWidthStr::width(prefix) + reserved,
    );
    let label = truncate(&row.label, available);
    let used = UnicodeWidthStr::width(label.as_str());
    let padding = " ".repeat(available.saturating_sub(used));
    let prefix_color = row.kind.map_or(theme.text_muted, |kind| change_color(kind, theme));
    let mut spans = vec![
        Span::styled(indent, style),
        Span::styled(prefix, style.fg(prefix_color)),
        Span::styled(format!("{label}{padding}"), style),
    ];
    spans.extend(trailing);
    Line::from(spans).style(style)
}

fn tree_action_spans(group: ChangeGroup, style: Style, theme: TuiTheme) -> Vec<Span<'static>> {
    let (label, color) = match group {
        ChangeGroup::Changes => (" + ", theme.success),
        ChangeGroup::Staged => (" - ", theme.danger),
    };
    vec![Span::styled(label, style.fg(color).add_modifier(Modifier::BOLD))]
}

fn tree_count_spans(row: &TreeRow, style: Style, theme: TuiTheme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if row.additions > 0 {
        spans.push(Span::styled(format!(" +{}", row.additions), style.fg(theme.success)));
    }
    if row.deletions > 0 {
        spans.push(Span::styled(format!(" -{}", row.deletions), style.fg(theme.danger)));
    }
    spans
}

fn change_color(kind: ChangeKind, theme: TuiTheme) -> Color {
    match kind {
        ChangeKind::Added | ChangeKind::Untracked => theme.success,
        ChangeKind::Deleted => theme.danger,
        ChangeKind::Conflict => theme.warning,
        ChangeKind::Modified
        | ChangeKind::Renamed
        | ChangeKind::Copied
        | ChangeKind::TypeChanged
        | ChangeKind::Submodule => theme.accent,
    }
}

fn change_glyph(kind: Option<ChangeKind>) -> &'static str {
    match kind {
        Some(ChangeKind::Modified) => "M ",
        Some(ChangeKind::Added) => "A ",
        Some(ChangeKind::Deleted) => "D ",
        Some(ChangeKind::Renamed) => "R ",
        Some(ChangeKind::Copied) => "C ",
        Some(ChangeKind::TypeChanged) => "T ",
        Some(ChangeKind::Untracked) => "A ",
        Some(ChangeKind::Conflict) => "! ",
        Some(ChangeKind::Submodule) => "S ",
        None => "• ",
    }
}

fn render_document(frame: &mut Frame<'_>, area: Rect, app: &mut DiffApp) {
    let effective = app.effective_mode(area.width.saturating_sub(2));
    let controls = match effective {
        EffectiveMode::Single => {
            " ↑↓ move  ←→ region  h/l pan  n/N change  s stage/unstage  r refresh  q quit "
        }
        EffectiveMode::Inline => {
            " ↑↓ move  ←→ region  h/l pan  n/N change  v view  s stage/unstage  r refresh  q quit "
        }
        EffectiveMode::Split => {
            " ↑↓ move  ←→ region  h/l pan  n/N change  v view  Tab cycle  s stage/unstage  r refresh  q quit "
        }
    };
    let footer = repository_footer(app, controls);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if app.active_region == ActiveRegion::Changes {
            app.theme.border
        } else {
            app.theme.accent
        }))
        .title(document_title(app))
        .title_bottom(footer);
    let inner = block.inner(area);
    app.regions.content_inner = inner;
    frame.render_widget(block, area);
    let effective = app.effective_mode(inner.width);
    if app.last_effective_mode.is_some_and(|previous| previous != effective) {
        app.restore_anchor = true;
    }
    app.last_effective_mode = Some(effective);
    if effective == EffectiveMode::Split && inner.width < SPLIT_MIN_WIDTH {
        frame.render_widget(
            Paragraph::new(
                "Split mode needs at least 50 content columns. Press v for inline mode.",
            )
            .style(Style::default().fg(app.theme.warning)),
            inner,
        );
        return;
    }
    let Some(document_index) = app.selected else {
        frame.render_widget(
            Paragraph::new("Working tree is clean.").style(Style::default().fg(app.theme.success)),
            inner,
        );
        return;
    };
    let key = ProjectionKey {
        document: document_index,
        mode: effective,
        width: inner.width as usize,
        old_horizontal_scroll: app.old_horizontal_scroll,
        new_horizontal_scroll: app.new_horizontal_scroll,
        divider_percent: app.divider_percent,
        line_numbers: app.line_numbers,
    };
    if app.document_projection.as_ref().is_none_or(|projection| projection.key != key) {
        let document = &app.documents[document_index];
        let lines = document_lines(document, app, effective, inner.width as usize);
        app.document_projection = Some(DocumentProjection { key, lines });
    }
    let projection = app.document_projection.as_ref().expect("projection was initialized");
    if app.restore_anchor {
        app.content_scroll =
            projection.lines.iter().position(|line| line.anchor == app.anchor).unwrap_or_else(
                || {
                    projection
                        .lines
                        .iter()
                        .position(|line| line.anchor.hunk == app.anchor.hunk)
                        .unwrap_or(0)
                },
            );
        app.restore_anchor = false;
    }
    let max_scroll = projection.lines.len().saturating_sub(inner.height as usize);
    app.content_scroll = app.content_scroll.min(max_scroll);
    if effective == EffectiveMode::Split {
        let left_width =
            split_widths(inner.width as usize, app.divider_percent, app.line_numbers.show(true)).0;
        app.regions.divider =
            Some(Rect::new(inner.x + left_width as u16, inner.y, 1, inner.height));
    }
    let visible = projection
        .lines
        .iter()
        .skip(app.content_scroll)
        .take(inner.height as usize)
        .map(|rendered| rendered.line.clone())
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn repository_footer(app: &DiffApp, controls: &'static str) -> Line<'static> {
    let Some(status) = &app.repository_status else {
        return Line::from(controls);
    };
    let (message, color) = match status {
        RepositoryStatus::Running(label) => (format!(" {label} "), app.theme.warning),
        RepositoryStatus::Success(label) => (format!(" {label} "), app.theme.success),
        RepositoryStatus::Error(error) => {
            (format!(" operation failed: {error} "), app.theme.danger)
        }
    };
    Line::from(vec![Span::styled(message, Style::default().fg(color)), Span::raw(controls)])
}

fn document_title(app: &DiffApp) -> Line<'static> {
    let Some(document) = app.selected.and_then(|index| app.documents.get(index)) else {
        return Line::from(Span::styled(
            " no changes ",
            Style::default().fg(app.theme.text_strong).add_modifier(Modifier::BOLD),
        ));
    };
    let mut spans = vec![Span::styled(
        format!(" {}", change_glyph(Some(document.kind))),
        Style::default().fg(change_color(document.kind, app.theme)).add_modifier(Modifier::BOLD),
    )];
    let renamed_paths = match (&document.old_path, &document.new_path) {
        (Some(old_path), Some(new_path)) if old_path != new_path => Some((old_path, new_path)),
        _ => None,
    };
    if let Some((old_path, new_path)) = renamed_paths {
        spans.push(Span::styled(
            format!("{} → {}", display_path(old_path), display_path(new_path)),
            Style::default().fg(app.theme.text_strong).add_modifier(Modifier::BOLD),
        ));
    } else {
        spans.push(Span::styled(
            document.display_path().map(display_path).unwrap_or_else(|| "unknown path".to_owned()),
            Style::default().fg(app.theme.text_strong).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(additions) = document.additions.filter(|count| *count > 0) {
        spans.push(Span::styled(format!("  +{additions}"), Style::default().fg(app.theme.success)));
    }
    if let Some(deletions) = document.deletions.filter(|count| *count > 0) {
        spans.push(Span::styled(format!("  -{deletions}"), Style::default().fg(app.theme.danger)));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

struct RenderedLine {
    line: Line<'static>,
    anchor: ReviewAnchor,
}

fn document_lines(
    document: &DiffDocument,
    app: &DiffApp,
    mode: EffectiveMode,
    width: usize,
) -> Vec<RenderedLine> {
    match &document.body {
        DiffBody::Text(text) => match mode {
            EffectiveMode::Single | EffectiveMode::Inline => inline_text_lines(text, app, width),
            EffectiveMode::Split => split_text_lines(text, app, width),
        },
        DiffBody::Binary => special_lines("Binary content cannot be rendered as text.", app.theme),
        DiffBody::NonUtf8 => {
            special_lines("Non-UTF-8 content cannot be rendered losslessly.", app.theme)
        }
        DiffBody::TooLarge { old_bytes, new_bytes } => special_lines(
            &format!("Diff exceeds the 8 MiB safety limit ({old_bytes} → {new_bytes} bytes)."),
            app.theme,
        ),
        DiffBody::Unavailable(error) => special_lines(&format!("Unavailable: {error}"), app.theme),
        DiffBody::Special(SpecialState::Conflict) => special_lines(
            "Unmerged conflict. Resolve it in the worktree, then press s to stage it.",
            app.theme,
        ),
        DiffBody::Special(SpecialState::Submodule { state }) => {
            special_lines(&format!("Submodule state: {state}"), app.theme)
        }
    }
}

fn special_lines(message: &str, theme: TuiTheme) -> Vec<RenderedLine> {
    vec![RenderedLine {
        line: Line::from(Span::styled(
            message.to_owned(),
            Style::default().fg(theme.warning).add_modifier(Modifier::BOLD),
        )),
        anchor: ReviewAnchor { hunk: 0, row: None },
    }]
}

fn inline_text_lines(text: &TextDiffDocument, app: &DiffApp, width: usize) -> Vec<RenderedLine> {
    let theme = app.theme;
    let mut lines = Vec::new();
    for (hunk_index, hunk) in text.hunks.iter().enumerate() {
        push_hunk_separator(&mut lines, text, hunk_index, width, theme);
        for (row_index, row) in hunk.rows.iter().enumerate() {
            let anchor = ReviewAnchor { hunk: hunk_index, row: Some(row_index) };
            match row.kind {
                RowKind::Context => {
                    push_source_line(
                        &mut lines,
                        anchor,
                        row.old.as_ref().or(row.new.as_ref()).expect("context row has a side"),
                        None,
                        &text.old,
                        theme,
                        app.new_horizontal_scroll,
                        width,
                        app.line_numbers.show(false),
                    );
                }
                RowKind::Changed => {
                    if let Some(old) = &row.old {
                        push_source_line(
                            &mut lines,
                            anchor,
                            old,
                            Some(ChangeSide::Deletion),
                            &text.old,
                            theme,
                            app.old_horizontal_scroll,
                            width,
                            app.line_numbers.show(false),
                        );
                    }
                    if let Some(new) = &row.new {
                        push_source_line(
                            &mut lines,
                            anchor,
                            new,
                            Some(ChangeSide::Addition),
                            &text.new,
                            theme,
                            app.new_horizontal_scroll,
                            width,
                            app.line_numbers.show(false),
                        );
                    }
                }
            }
        }
    }
    push_trailing_separator(&mut lines, text, width, theme);
    if lines.is_empty() {
        lines.push(RenderedLine {
            line: Line::from(Span::styled(
                "No textual changes.",
                Style::default().fg(theme.text_muted),
            )),
            anchor: ReviewAnchor { hunk: 0, row: None },
        });
    }
    lines
}

fn split_text_lines(text: &TextDiffDocument, app: &DiffApp, width: usize) -> Vec<RenderedLine> {
    let theme = app.theme;
    let show_line_numbers = app.line_numbers.show(true);
    let (left_width, right_width) = split_widths(width, app.divider_percent, show_line_numbers);
    let mut lines = Vec::new();
    for (hunk_index, hunk) in text.hunks.iter().enumerate() {
        push_hunk_separator(&mut lines, text, hunk_index, width, theme);
        for (row_index, row) in hunk.rows.iter().enumerate() {
            let anchor = ReviewAnchor { hunk: hunk_index, row: Some(row_index) };
            let mut spans = split_side(
                row.old.as_ref(),
                (row.kind == RowKind::Changed && row.old.is_some()).then_some(ChangeSide::Deletion),
                &text.old,
                theme,
                app.old_horizontal_scroll,
                left_width,
                show_line_numbers,
            );
            spans.push(Span::styled("│", Style::default().fg(theme.border)));
            spans.extend(split_side(
                row.new.as_ref(),
                (row.kind == RowKind::Changed && row.new.is_some()).then_some(ChangeSide::Addition),
                &text.new,
                theme,
                app.new_horizontal_scroll,
                right_width,
                show_line_numbers,
            ));
            lines.push(RenderedLine { line: Line::from(spans), anchor });
        }
    }
    push_trailing_separator(&mut lines, text, width, theme);
    if lines.is_empty() {
        lines.push(RenderedLine {
            line: Line::from(Span::styled(
                "No textual changes.",
                Style::default().fg(theme.text_muted),
            )),
            anchor: ReviewAnchor { hunk: 0, row: None },
        });
    }
    lines
}

#[allow(clippy::too_many_arguments)]
fn split_side(
    cell: Option<&LineCell>,
    change: Option<ChangeSide>,
    snapshot: &TextSnapshot,
    theme: TuiTheme,
    horizontal_scroll: usize,
    width: usize,
    show_line_numbers: bool,
) -> Vec<Span<'static>> {
    let background = change.map(|side| change_background(theme, side));
    let base = background
        .map(|color| Style::default().fg(theme.text).bg(color))
        .unwrap_or_else(|| Style::default().fg(theme.text));
    let Some(cell) = cell else {
        return vec![Span::styled(" ".repeat(width), base)];
    };
    let source = snapshot.display_line(cell.line_index);
    let styled = apply_emphasis(source, &cell.emphasis, base);
    let newline_marker = cell.missing_newline.then_some(" ⏎");
    let marker_width = newline_marker.map(UnicodeWidthStr::width).unwrap_or(0);
    let gutter_width = if show_line_numbers { SPLIT_GUTTER_WIDTH } else { CHANGE_INDICATOR_WIDTH };
    let available = width.saturating_sub(gutter_width + marker_width);
    let gutter_foreground = readable_foreground(theme.text_muted, background, theme);
    let mut spans = Vec::new();
    if show_line_numbers {
        spans
            .push(Span::styled(format!("{:>5} ", cell.line_index + 1), base.fg(gutter_foreground)));
    }
    spans.push(change_indicator(change, base, theme));
    spans.extend(crop_spans(styled, horizontal_scroll, available));
    if let Some(marker) = newline_marker {
        spans.push(Span::styled(marker, base.fg(theme.warning)));
    }
    pad_spans(&mut spans, width, base);
    spans
}

fn push_hunk_separator(
    lines: &mut Vec<RenderedLine>,
    text: &TextDiffDocument,
    hunk_index: usize,
    width: usize,
    theme: TuiTheme,
) {
    let hunk = &text.hunks[hunk_index];
    let (previous_old_end, previous_new_end) = hunk_index
        .checked_sub(1)
        .map(|previous| {
            let previous = &text.hunks[previous];
            (previous.old_range.end, previous.new_range.end)
        })
        .unwrap_or((0, 0));
    let collapsed = hunk
        .old_range
        .start
        .saturating_sub(previous_old_end)
        .max(hunk.new_range.start.saturating_sub(previous_new_end));
    push_context_separator(lines, collapsed, hunk_index, width, theme);
}

fn push_trailing_separator(
    lines: &mut Vec<RenderedLine>,
    text: &TextDiffDocument,
    width: usize,
    theme: TuiTheme,
) {
    let Some((hunk_index, hunk)) = text.hunks.iter().enumerate().next_back() else {
        return;
    };
    let collapsed = text
        .old
        .line_count()
        .saturating_sub(hunk.old_range.end)
        .max(text.new.line_count().saturating_sub(hunk.new_range.end));
    push_context_separator(lines, collapsed, hunk_index, width, theme);
}

fn push_context_separator(
    lines: &mut Vec<RenderedLine>,
    collapsed: usize,
    hunk_index: usize,
    width: usize,
    theme: TuiTheme,
) {
    if collapsed == 0 {
        return;
    }
    let noun = if collapsed == 1 { "line" } else { "lines" };
    let label = truncate(&format!("  ⋯ {collapsed} unmodified {noun} ⋯"), width);
    let padding = " ".repeat(width.saturating_sub(UnicodeWidthStr::width(label.as_str())));
    let style = Style::default().fg(theme.text_muted).bg(theme.code_background);
    lines.push(RenderedLine {
        line: Line::from(Span::styled(format!("{label}{padding}"), style)),
        anchor: ReviewAnchor { hunk: hunk_index, row: None },
    });
}

fn split_widths(width: usize, percent: u16, show_line_numbers: bool) -> (usize, usize) {
    let content = width.saturating_sub(1);
    let minimum = if show_line_numbers { SPLIT_GUTTER_WIDTH + 1 } else { 3 };
    let left = ((content * percent as usize) / 100).clamp(minimum, content.saturating_sub(minimum));
    (left, content.saturating_sub(left))
}

fn pad_spans(spans: &mut Vec<Span<'static>>, width: usize, style: Style) {
    let used =
        spans.iter().map(|span| UnicodeWidthStr::width(span.content.as_ref())).sum::<usize>();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
}

#[allow(clippy::too_many_arguments)]
fn push_source_line(
    output: &mut Vec<RenderedLine>,
    anchor: ReviewAnchor,
    cell: &LineCell,
    change: Option<ChangeSide>,
    snapshot: &TextSnapshot,
    theme: TuiTheme,
    horizontal_scroll: usize,
    width: usize,
    show_line_numbers: bool,
) {
    let background = change.map(|side| change_background(theme, side));
    let base = background
        .map(|color| Style::default().fg(theme.text).bg(color))
        .unwrap_or_else(|| Style::default().fg(theme.text));
    let source = snapshot.display_line(cell.line_index);
    let styled = apply_emphasis(source, &cell.emphasis, base);
    let newline_marker = cell.missing_newline.then_some("  ⏎ no newline");
    let marker_width = newline_marker.map(UnicodeWidthStr::width).unwrap_or(0);
    let gutter_width = if show_line_numbers { 6 } else { 0 };
    let available = width.saturating_sub(gutter_width + CHANGE_INDICATOR_WIDTH + marker_width);
    let mut spans = Vec::new();
    if show_line_numbers {
        spans.push(Span::styled(
            format!("{:>5} ", cell.line_index + 1),
            base.fg(readable_foreground(theme.text_muted, background, theme)),
        ));
    }
    spans.push(change_indicator(change, base, theme));
    spans.extend(crop_spans(styled, horizontal_scroll, available));
    if let Some(marker) = newline_marker {
        spans.push(Span::styled(marker, base.fg(theme.warning).add_modifier(Modifier::ITALIC)));
    }
    output.push(RenderedLine { line: Line::from(spans).style(base), anchor });
}

fn apply_emphasis(
    source: &str,
    emphasis: &[std::ops::Range<usize>],
    base: Style,
) -> Vec<Span<'static>> {
    let mut output = Vec::new();
    let mut current = String::new();
    let mut current_emphasis = None;
    for (position, character) in source.char_indices() {
        let emphasized = emphasis.iter().any(|range| range.contains(&position));
        if current_emphasis.is_some_and(|value| value != emphasized) {
            push_emphasis_span(
                &mut output,
                std::mem::take(&mut current),
                base,
                current_emphasis.unwrap(),
            );
        }
        current_emphasis = Some(emphasized);
        current.push(character);
    }
    if !current.is_empty() {
        push_emphasis_span(&mut output, current, base, current_emphasis.unwrap_or(false));
    }
    output
}

fn push_emphasis_span(
    output: &mut Vec<Span<'static>>,
    content: String,
    style: Style,
    emphasized: bool,
) {
    let style =
        if emphasized { style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { style };
    output.push(Span::styled(content, style));
}

fn change_indicator(change: Option<ChangeSide>, base: Style, theme: TuiTheme) -> Span<'static> {
    match change {
        Some(side) => Span::styled("▌ ", base.fg(change_side_color(theme, side))),
        None => Span::styled("  ", base),
    }
}

fn change_side_color(theme: TuiTheme, side: ChangeSide) -> Color {
    match side {
        ChangeSide::Addition => theme.success,
        ChangeSide::Deletion => theme.danger,
    }
}

fn change_background(theme: TuiTheme, side: ChangeSide) -> Color {
    match side {
        ChangeSide::Addition => added_background(theme),
        ChangeSide::Deletion => deleted_background(theme),
    }
}

fn added_background(theme: TuiTheme) -> Color {
    if is_light_background(theme.code_background) {
        LIGHT_ADDED_BACKGROUND
    } else {
        DARK_ADDED_BACKGROUND
    }
}

fn deleted_background(theme: TuiTheme) -> Color {
    if is_light_background(theme.code_background) {
        LIGHT_DELETED_BACKGROUND
    } else {
        DARK_DELETED_BACKGROUND
    }
}

fn is_light_background(color: Color) -> bool {
    color_rgb(color).is_some_and(|rgb| relative_luminance(rgb) > 0.5)
}

fn readable_foreground(preferred: Color, background: Option<Color>, theme: TuiTheme) -> Color {
    let Some(background) = background else {
        return preferred;
    };
    if contrast_ratio(preferred, background).is_some_and(|ratio| ratio >= 4.5) {
        return preferred;
    }
    [theme.text, theme.text_strong, Color::White, Color::Black]
        .into_iter()
        .max_by(|left, right| {
            contrast_ratio(*left, background)
                .partial_cmp(&contrast_ratio(*right, background))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(theme.text)
}

fn contrast_ratio(left: Color, right: Color) -> Option<f64> {
    let left = relative_luminance(color_rgb(left)?);
    let right = relative_luminance(color_rgb(right)?);
    let (lighter, darker) = if left > right { (left, right) } else { (right, left) };
    Some((lighter + 0.05) / (darker + 0.05))
}

fn relative_luminance((red, green, blue): (u8, u8, u8)) -> f64 {
    let linear = |channel: u8| {
        let channel = f64::from(channel) / 255.0;
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
}

fn color_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Black => Some((0, 0, 0)),
        Color::Red => Some((205, 49, 49)),
        Color::Green => Some((13, 188, 121)),
        Color::Yellow => Some((229, 229, 16)),
        Color::Blue => Some((36, 114, 200)),
        Color::Magenta => Some((188, 63, 188)),
        Color::Cyan => Some((17, 168, 205)),
        Color::Gray => Some((204, 204, 204)),
        Color::DarkGray => Some((102, 102, 102)),
        Color::LightRed => Some((241, 76, 76)),
        Color::LightGreen => Some((35, 209, 139)),
        Color::LightYellow => Some((245, 245, 67)),
        Color::LightBlue => Some((59, 142, 234)),
        Color::LightMagenta => Some((214, 112, 214)),
        Color::LightCyan => Some((41, 184, 219)),
        Color::White => Some((255, 255, 255)),
        Color::Rgb(red, green, blue) => Some((red, green, blue)),
        Color::Reset | Color::Indexed(_) => None,
    }
}

fn crop_spans(spans: Vec<Span<'static>>, skip: usize, width: usize) -> Vec<Span<'static>> {
    let mut output = Vec::new();
    let mut skipped = 0;
    let mut written = 0;
    for span in spans {
        let mut content = String::new();
        for character in span.content.chars() {
            let character_width = character.width().unwrap_or(0);
            if skipped < skip {
                skipped += character_width;
                continue;
            }
            if written + character_width > width {
                break;
            }
            content.push(character);
            written += character_width;
        }
        if !content.is_empty() {
            output.push(Span::styled(content, span.style));
        }
        if written >= width {
            break;
        }
    }
    output
}

#[derive(Default)]
struct TreeNode {
    directories: BTreeMap<OsString, TreeNode>,
    files: Vec<usize>,
}

fn tree_rows(
    documents: &[DiffDocument],
    expanded: &HashSet<(ChangeGroup, PathBuf)>,
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    for group in [ChangeGroup::Staged, ChangeGroup::Changes] {
        let indices: Vec<_> = documents
            .iter()
            .enumerate()
            .filter_map(|(index, document)| (document.group == group).then_some(index))
            .collect();
        let (additions, deletions) = totals(documents, &indices);
        rows.push(TreeRow {
            target: TreeTarget::Group(group),
            depth: 0,
            label: group_label(group).to_owned(),
            additions,
            deletions,
            kind: None,
        });
        let root = build_tree(documents, &indices);
        if expanded.contains(&(group, PathBuf::new())) {
            append_tree_rows(&mut rows, documents, group, &root, Path::new(""), 1, expanded);
        }
    }
    rows
}

fn build_tree(documents: &[DiffDocument], indices: &[usize]) -> TreeNode {
    let mut root = TreeNode::default();
    for &index in indices {
        let Some(path) = documents[index].display_path() else {
            continue;
        };
        let components: Vec<_> = path
            .components()
            .filter_map(|component| match component {
                Component::Normal(value) => Some(value.to_os_string()),
                _ => None,
            })
            .collect();
        let Some((file, directories)) = components.split_last() else {
            continue;
        };
        let mut node = &mut root;
        for directory in directories {
            node = node.directories.entry(directory.clone()).or_default();
        }
        let _ = file;
        node.files.push(index);
    }
    root
}

fn append_tree_rows(
    rows: &mut Vec<TreeRow>,
    documents: &[DiffDocument],
    group: ChangeGroup,
    node: &TreeNode,
    parent: &Path,
    depth: usize,
    expanded: &HashSet<(ChangeGroup, PathBuf)>,
) {
    for (name, child) in &node.directories {
        let mut path = parent.join(name);
        let mut label = display_os(name);
        let mut terminal = child;
        while terminal.files.is_empty() && terminal.directories.len() == 1 {
            let Some((name, child)) = terminal.directories.iter().next() else {
                break;
            };
            path.push(name);
            label.push('/');
            label.push_str(&display_os(name));
            terminal = child;
        }
        let indices = descendant_files(terminal);
        let (additions, deletions) = totals(documents, &indices);
        rows.push(TreeRow {
            target: TreeTarget::Directory(group, path.clone()),
            depth,
            label,
            additions,
            deletions,
            kind: None,
        });
        if expanded.contains(&(group, path.clone())) {
            append_tree_rows(rows, documents, group, terminal, &path, depth + 1, expanded);
        }
    }
    for &index in &node.files {
        let document = &documents[index];
        rows.push(TreeRow {
            target: TreeTarget::File(index),
            depth,
            label: document
                .display_path()
                .and_then(Path::file_name)
                .map(display_os)
                .unwrap_or_else(|| "?".to_owned()),
            additions: document.additions.unwrap_or(0),
            deletions: document.deletions.unwrap_or(0),
            kind: Some(document.kind),
        });
    }
}

fn descendant_files(node: &TreeNode) -> Vec<usize> {
    let mut indices = node.files.clone();
    for child in node.directories.values() {
        indices.extend(descendant_files(child));
    }
    indices
}

fn totals(documents: &[DiffDocument], indices: &[usize]) -> (usize, usize) {
    indices.iter().fold((0, 0), |(additions, deletions), index| {
        (
            additions + documents[*index].additions.unwrap_or(0),
            deletions + documents[*index].deletions.unwrap_or(0),
        )
    })
}

fn directory_keys(documents: &[DiffDocument]) -> Vec<(ChangeGroup, PathBuf)> {
    let mut keys = HashSet::new();
    keys.insert((ChangeGroup::Staged, PathBuf::new()));
    keys.insert((ChangeGroup::Changes, PathBuf::new()));
    for document in documents {
        let Some(path) = document.display_path() else {
            continue;
        };
        let mut parent = PathBuf::new();
        let components: Vec<_> = path.components().collect();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            if let Component::Normal(value) = component {
                parent.push(value);
                keys.insert((document.group, parent.clone()));
            }
        }
    }
    keys.into_iter().collect()
}

fn group_label(group: ChangeGroup) -> &'static str {
    match group {
        ChangeGroup::Staged => "STAGED",
        ChangeGroup::Changes => "CHANGES",
    }
}

fn display_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(display_os(value)),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(unix)]
fn display_os(value: &std::ffi::OsStr) -> String {
    use std::os::unix::ffi::OsStrExt;

    value
        .as_bytes()
        .iter()
        .flat_map(|byte| std::ascii::escape_default(*byte))
        .map(char::from)
        .collect()
}

#[cfg(not(unix))]
fn display_os(value: &std::ffi::OsStr) -> String {
    value.to_string_lossy().into_owned()
}

fn truncate(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let target = width.saturating_sub(1);
    for character in value.chars() {
        if UnicodeWidthStr::width(output.as_str()) + character.width().unwrap_or(0) > target {
            break;
        }
        output.push(character);
    }
    output.push('…');
    output
}

#[cfg(test)]
#[path = "tui_perf_tests.rs"]
mod perf_tests;

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::tools::diff::model::{DiffContext, DiffInput, SourceSnapshot};
    use crate::tui::theme::NORD;

    fn document(group: ChangeGroup, path: &str, old: &str, new: &str) -> DiffDocument {
        DiffDocument::build(
            DiffInput {
                group,
                kind: ChangeKind::Modified,
                old_path: Some(path.into()),
                new_path: Some(path.into()),
                old: SourceSnapshot::Bytes(Arc::from(old.as_bytes())),
                new: SourceSnapshot::Bytes(Arc::from(new.as_bytes())),
                special: None,
            },
            DiffContext::default(),
        )
    }

    fn document_of_kind(kind: ChangeKind, path: &str, old: &str, new: &str) -> DiffDocument {
        let (old_path, new_path, old, new) = match kind {
            ChangeKind::Added | ChangeKind::Untracked => (
                None,
                Some(path.into()),
                SourceSnapshot::Absent,
                SourceSnapshot::Bytes(Arc::from(new.as_bytes())),
            ),
            ChangeKind::Deleted => (
                Some(path.into()),
                None,
                SourceSnapshot::Bytes(Arc::from(old.as_bytes())),
                SourceSnapshot::Absent,
            ),
            _ => (
                Some(path.into()),
                Some(path.into()),
                SourceSnapshot::Bytes(Arc::from(old.as_bytes())),
                SourceSnapshot::Bytes(Arc::from(new.as_bytes())),
            ),
        };
        DiffDocument::build(
            DiffInput {
                group: ChangeGroup::Changes,
                kind,
                old_path,
                new_path,
                old,
                new,
                special: None,
            },
            DiffContext::default(),
        )
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        terminal.backend().buffer().content().iter().map(|cell| cell.symbol()).collect()
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn wide_compact_and_minimum_surfaces_render() {
        let documents = vec![
            document(ChangeGroup::Staged, "src/lib.rs", "let a = 1;\n", "let a = 2;\n"),
            document(ChangeGroup::Changes, "docs/readme.md", "old\n", "new\n"),
        ];
        for (width, height) in [(120, 30), (60, 20), (30, 8)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut app = DiffApp::new(documents.clone(), NORD, ViewMode::Inline);
            terminal.draw(|frame| render(frame, &mut app)).unwrap();
            let mut evidence = screen(&terminal);
            assert!(evidence.contains("STAGED"));
            if !evidence.contains("CHANGES") {
                app.select(1);
                terminal.draw(|frame| render(frame, &mut app)).unwrap();
                evidence.push_str(&screen(&terminal));
            }
            assert!(evidence.contains("CHANGES"));
            assert!(evidence.contains("lib.rs"));
        }
    }

    #[test]
    fn navigation_and_tree_mouse_regions_preserve_canonical_selection() {
        let documents = vec![
            document(ChangeGroup::Staged, "src/a.rs", "a\n", "A\n"),
            document(ChangeGroup::Changes, "src/b.rs", "b\n", "B\n"),
        ];
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = DiffApp::new(documents, NORD, ViewMode::Inline);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected, Some(1));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("b.rs"));
        assert!(!app.regions.tree.is_empty());
    }

    #[test]
    fn refresh_key_is_an_explicit_runtime_action() {
        let mut app = DiffApp::new(Vec::new(), NORD, ViewMode::Inline);

        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            Flow::Refresh
        );
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Flow::Continue
        );
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            Flow::ToggleStage
        );
    }

    #[test]
    fn queued_navigation_is_applied_before_the_next_frame() {
        let documents = (0..5)
            .map(|index| {
                document(ChangeGroup::Changes, &format!("src/file-{index}.rs"), "old\n", "new\n")
            })
            .collect();
        let mut app = DiffApp::new(documents, NORD, ViewMode::Inline);
        let mut events =
            VecDeque::from(vec![Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); 3]);

        let flow = handle_terminal_events(&mut app, events.pop_front(), || events.pop_front());

        assert_eq!(flow, Flow::Continue);
        assert_eq!(app.selected, Some(3));
        assert!(events.is_empty());
        assert!(app.document_projection.is_none());
    }

    #[test]
    fn selected_group_determines_the_index_operation() {
        let mut app = DiffApp::new(
            vec![
                document(ChangeGroup::Staged, "staged.rs", "old\n", "new\n"),
                document(ChangeGroup::Changes, "changed.rs", "old\n", "new\n"),
            ],
            NORD,
            ViewMode::Inline,
        );

        assert!(matches!(
            app.selected_repository_operation(),
            Some(RepositoryOperation::Unstage(document))
                if document.display_path() == Some(Path::new("staged.rs"))
        ));
        app.select(1);
        assert!(matches!(
            app.selected_repository_operation(),
            Some(RepositoryOperation::Stage(document))
                if document.display_path() == Some(Path::new("changed.rs"))
        ));
    }

    #[test]
    fn successful_refresh_preserves_identity_and_expansion_policy() {
        let documents = vec![
            document(ChangeGroup::Changes, "src/lib.rs", "old\n", "new\n"),
            document(ChangeGroup::Changes, "src/nested/keep.rs", "old\n", "new\n"),
        ];
        let mut app = DiffApp::new(documents, NORD, ViewMode::Inline);
        app.select(1);
        app.expanded.remove(&(ChangeGroup::Changes, "src/nested".into()));
        app.content_scroll = 12;

        app.finish_repository_operation(Ok((
            vec![
                document(ChangeGroup::Changes, "fresh/deep/new.rs", "before\n", "after\n"),
                document(ChangeGroup::Changes, "src/nested/keep.rs", "old\n", "newer\nextra\n"),
            ],
            "refreshed",
        )));

        let selected = app.selected.and_then(|index| app.documents.get(index)).unwrap();
        assert_eq!(selected.display_path(), Some(Path::new("src/nested/keep.rs")));
        assert!(!app.expanded.contains(&(ChangeGroup::Changes, "src/nested".into())));
        assert!(app.expanded.contains(&(ChangeGroup::Changes, "fresh/deep".into())));
        assert_eq!(app.content_scroll, 0);
        assert_eq!(app.repository_status, Some(RepositoryStatus::Success("refreshed")));
    }

    #[test]
    fn refresh_follows_a_selected_path_between_change_groups() {
        let mut app = DiffApp::new(
            vec![document(ChangeGroup::Changes, "src/lib.rs", "old\n", "new\n")],
            NORD,
            ViewMode::Inline,
        );

        app.finish_repository_operation(Ok((
            vec![document(ChangeGroup::Staged, "src/lib.rs", "old\n", "new\n")],
            "staged",
        )));

        let selected = app.selected.and_then(|index| app.documents.get(index)).unwrap();
        assert_eq!(selected.group, ChangeGroup::Staged);
        assert_eq!(selected.display_path(), Some(Path::new("src/lib.rs")));
    }

    #[test]
    fn failed_refresh_keeps_the_last_valid_snapshot() {
        let mut app = DiffApp::new(
            vec![document(ChangeGroup::Changes, "src/lib.rs", "old\n", "new\n")],
            NORD,
            ViewMode::Inline,
        );

        app.finish_repository_operation(Err(anyhow!("git status failed")));

        assert_eq!(app.documents.len(), 1);
        assert_eq!(app.documents[0].display_path(), Some(Path::new("src/lib.rs")));
        assert_eq!(
            app.repository_status,
            Some(RepositoryStatus::Error("git status failed".to_owned()))
        );
    }

    #[test]
    fn region_navigation_uses_arrows_tabs_and_local_scroll_without_stealing_selection() {
        let old = (0..40).map(|index| format!("old {index}\n")).collect::<String>();
        let new = old.replace("old 20\n", "new 20\n");
        let documents = vec![
            document(ChangeGroup::Staged, "src/a.rs", "a\n", "A\n"),
            document(ChangeGroup::Changes, "src/b.rs", &old, &new),
        ];
        let backend = TestBackend::new(120, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = DiffApp::new(documents, NORD, ViewMode::Split);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.active_region, ActiveRegion::Old);
        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.active_region, ActiveRegion::New);
        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.active_region, ActiveRegion::Old);
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.active_region, ActiveRegion::New);

        let selected = app.selected;
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected, selected);
        assert_eq!(app.content_scroll, 1);

        app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(app.active_region, ActiveRegion::Changes);
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected, Some(1));
    }

    #[test]
    fn region_click_and_responsive_projection_keep_an_active_visible_region() {
        let documents =
            vec![document(ChangeGroup::Changes, "src/lib.rs", "let old = 1;\n", "let new = 2;\n")];
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = DiffApp::new(documents, NORD, ViewMode::Auto);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let old =
            app.navigation().hit_test(app.regions.content_inner.x, app.regions.content_inner.y);
        assert_eq!(old, Some(ActiveRegion::Old));
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.regions.content_inner.x,
            row: app.regions.content_inner.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.active_region, ActiveRegion::Old);

        terminal.backend_mut().resize(70, 20);
        terminal.autoresize().unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.active_region, ActiveRegion::New);
    }

    #[test]
    fn each_vertical_wheel_event_moves_exactly_one_row() {
        let mut app = DiffApp::new(Vec::new(), NORD, ViewMode::Inline);
        app.regions.tree_area = Rect::new(0, 0, 20, 10);
        app.regions.content_area = Rect::new(20, 0, 20, 10);

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 25,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.tree_scroll, 1);
        assert_eq!(app.content_scroll, 1);

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 25,
            row: 5,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.tree_scroll, 0);
        assert_eq!(app.content_scroll, 0);
    }

    #[test]
    fn explicit_states_are_visible_and_empty_repository_is_honest() {
        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut empty = DiffApp::new(Vec::new(), NORD, ViewMode::Inline);
        terminal.draw(|frame| render(frame, &mut empty)).unwrap();
        assert!(screen(&terminal).contains("Working tree is clean"));

        let binary = DiffDocument::build(
            DiffInput {
                group: ChangeGroup::Changes,
                kind: ChangeKind::Modified,
                old_path: Some("binary.dat".into()),
                new_path: Some("binary.dat".into()),
                old: SourceSnapshot::from_bytes(&b"a\0b"[..]),
                new: SourceSnapshot::from_bytes(&b"a\0c"[..]),
                special: None,
            },
            DiffContext::default(),
        );
        let mut app = DiffApp::new(vec![binary], NORD, ViewMode::Inline);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(screen(&terminal).contains("Binary content"));
    }

    #[test]
    fn mode_switch_preserves_the_canonical_row_anchor() {
        let old = (0..24).map(|index| format!("old {index}\n")).collect::<String>();
        let new = old.replace("old 10\n", "new 10\n");
        let documents = vec![document(ChangeGroup::Changes, "src/lib.rs", &old, &new)];
        let backend = TestBackend::new(120, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = DiffApp::new(documents, NORD, ViewMode::Inline);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        app.content_scroll = app
            .document_projection
            .as_ref()
            .unwrap()
            .lines
            .iter()
            .position(|line| line.anchor.row == Some(2))
            .expect("canonical changed-row anchor");
        app.sync_anchor_from_scroll();
        let anchor = app.anchor;

        app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(app.last_effective_mode, Some(EffectiveMode::Split));
        assert_eq!(app.anchor, anchor);
        assert!(app
            .document_projection
            .as_ref()
            .unwrap()
            .lines
            .iter()
            .any(|line| line.anchor == anchor));
        assert!(screen(&terminal).contains('│'));

        app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.last_effective_mode, Some(EffectiveMode::Inline));
        assert_eq!(app.anchor, anchor);
    }

    #[test]
    fn vertical_scroll_reuses_projection_and_projection_inputs_rebuild_it() {
        let documents = vec![document(
            ChangeGroup::Changes,
            "src/lib.rs",
            "one\ntwo\nthree\nfour\nfive\n",
            "one\nTWO\nthree\nFOUR\nfive\n",
        )];
        let backend = TestBackend::new(120, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = DiffApp::new(documents, NORD, ViewMode::Inline);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let initial_key = app.document_projection.as_ref().unwrap().key;
        let initial_lines = app.document_projection.as_ref().unwrap().lines.as_ptr();

        app.active_region = ActiveRegion::New;
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.document_projection.as_ref().unwrap().key, initial_key);
        assert_eq!(app.document_projection.as_ref().unwrap().lines.as_ptr(), initial_lines);

        app.on_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_ne!(app.document_projection.as_ref().unwrap().key, initial_key);

        app.select(0);
        assert!(app.document_projection.is_some());
        app.replace_documents(vec![document(
            ChangeGroup::Changes,
            "src/lib.rs",
            "one\ntwo\n",
            "one\nTWO\n",
        )]);
        assert!(app.document_projection.is_none());
    }

    #[test]
    fn split_panes_pan_independently_and_divider_drag_is_clamped() {
        let documents = vec![document(
            ChangeGroup::Changes,
            "src/lib.rs",
            "let old_value = 1;\n",
            "let new_value = 2;\n",
        )];
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = DiffApp::new(documents, NORD, ViewMode::Split);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        app.active_region = ActiveRegion::Old;
        app.pan(4);
        assert_eq!((app.old_horizontal_scroll, app.new_horizontal_scroll), (4, 0));
        app.active_region = ActiveRegion::New;
        app.pan(4);
        assert_eq!((app.old_horizontal_scroll, app.new_horizontal_scroll), (4, 4));

        let original = app.regions.divider.expect("split divider");
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: original.x,
            row: original.y,
            modifiers: KeyModifiers::NONE,
        });
        let target = app.regions.content_inner.x + app.regions.content_inner.width * 3 / 4;
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: target,
            row: original.y,
            modifiers: KeyModifiers::NONE,
        });
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: target,
            row: original.y,
            modifiers: KeyModifiers::NONE,
        });
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert!(app.regions.divider.unwrap().x > original.x);
        assert_eq!(app.divider_percent, 75);
    }

    #[test]
    fn auto_mode_changes_projection_on_resize_without_changing_anchor() {
        let documents = vec![document(
            ChangeGroup::Changes,
            "src/lib.rs",
            "one\ntwo\nthree\n",
            "one\nTWO\nthree\n",
        )];
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = DiffApp::new(documents, NORD, ViewMode::Auto);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        assert_eq!(app.last_effective_mode, Some(EffectiveMode::Split));
        let anchor = app.anchor;

        terminal.backend_mut().resize(70, 20);
        terminal.autoresize().unwrap();
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert_eq!(app.last_effective_mode, Some(EffectiveMode::Inline));
        assert_eq!(app.anchor, anchor);
    }

    #[test]
    fn added_and_deleted_files_suppress_the_absent_split_side() {
        for document in [
            document_of_kind(ChangeKind::Added, "src/new.rs", "", "fn added() {}\n"),
            document_of_kind(ChangeKind::Untracked, "src/untracked.rs", "", "let fresh = true;\n"),
            document_of_kind(ChangeKind::Deleted, "src/old.rs", "fn removed() {}\n", ""),
        ] {
            let backend = TestBackend::new(120, 18);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut app = DiffApp::new(vec![document], NORD, ViewMode::Split);

            terminal.draw(|frame| render(frame, &mut app)).unwrap();

            assert_eq!(app.last_effective_mode, Some(EffectiveMode::Single));
            assert!(app.regions.divider.is_none());
            assert!(!screen(&terminal).contains("Split mode needs"));
        }
    }

    #[test]
    fn review_surface_uses_semantic_bars_and_context_labels_not_patch_notation() {
        let old = (0..30).map(|index| format!("value {index}\n")).collect::<String>();
        let new =
            old.replace("value 5\n", "first change\n").replace("value 24\n", "second change\n");
        let document = document(ChangeGroup::Changes, "src/lib.rs", &old, &new);
        let DiffBody::Text(text) = &document.body else {
            panic!("text fixture");
        };
        let app = DiffApp::new(vec![document.clone()], NORD, ViewMode::Inline);
        let rendered = inline_text_lines(text, &app, 100);
        let rows = rendered.iter().map(|line| line_text(&line.line)).collect::<Vec<_>>();

        assert!(rows.iter().any(|line| line.contains("unmodified lines")));
        assert!(rows.iter().any(|line| line.contains('▌')));
        assert!(rows.iter().all(|line| !line.contains("@@")));
        assert!(rendered
            .iter()
            .flat_map(|line| &line.line.spans)
            .all(|span| { !matches!(span.content.as_ref(), "+" | "-" | "+ " | "- ") }));
    }

    #[test]
    fn inline_rows_omit_line_numbers_and_keep_only_the_change_indicator() {
        let document = document(
            ChangeGroup::Changes,
            "src/lib.rs",
            "same\nold\ntail\n",
            "same\nnew\nextra\ntail\n",
        );
        let DiffBody::Text(text) = &document.body else {
            panic!("text fixture");
        };
        let app = DiffApp::new(vec![document.clone()], NORD, ViewMode::Inline);
        let rendered = inline_text_lines(text, &app, 100);
        let source_lines =
            rendered.iter().filter(|line| line.anchor.row.is_some()).collect::<Vec<_>>();

        assert!(!source_lines.is_empty());
        assert!(source_lines.iter().all(|line| {
            line.line
                .spans
                .first()
                .is_some_and(|indicator| UnicodeWidthStr::width(indicator.content.as_ref()) == 2)
        }));
    }

    #[test]
    fn line_number_setting_controls_every_text_projection() {
        let document = document(ChangeGroup::Changes, "src/lib.rs", "same\nold\n", "same\nnew\n");
        let DiffBody::Text(text) = &document.body else {
            panic!("text fixture");
        };
        let always = DiffApp::with_line_numbers(
            vec![document.clone()],
            NORD,
            ViewMode::Inline,
            LineNumbers::Always,
        );
        let inline = inline_text_lines(text, &always, 100);
        assert!(inline
            .iter()
            .filter(|line| line.anchor.row.is_some())
            .all(|line| line_text(&line.line).starts_with("    ")));

        let never = DiffApp::with_line_numbers(
            vec![document.clone()],
            NORD,
            ViewMode::Split,
            LineNumbers::Never,
        );
        let split = split_text_lines(text, &never, 100);
        assert!(split
            .iter()
            .filter(|line| line.anchor.row.is_some())
            .all(|line| !line_text(&line.line).starts_with("    ")));
    }

    #[test]
    fn hunk_navigation_targets_code_after_metadata_headers_are_removed() {
        let old = (0..30).map(|index| format!("value {index}\n")).collect::<String>();
        let new =
            old.replace("value 5\n", "first change\n").replace("value 24\n", "second change\n");
        let document = document(ChangeGroup::Changes, "src/lib.rs", &old, &new);
        let mut app = DiffApp::new(vec![document], NORD, ViewMode::Inline);

        app.select_hunk(1);

        assert_eq!(app.anchor, ReviewAnchor { hunk: 1, row: Some(0) });
    }

    #[test]
    fn tree_presents_untracked_as_added_and_colors_counts_independently() {
        assert_eq!(change_glyph(Some(ChangeKind::Untracked)), "A ");
        let row = TreeRow {
            target: TreeTarget::File(0),
            depth: 1,
            label: "lib.rs".to_owned(),
            additions: 12,
            deletions: 3,
            kind: Some(ChangeKind::Modified),
        };
        let line = tree_line(&row, 40, Style::default(), &HashSet::new(), None, NORD);
        let addition = line.spans.iter().find(|span| span.content == " +12").unwrap();
        let deletion = line.spans.iter().find(|span| span.content == " -3").unwrap();

        assert_eq!(addition.style.fg, Some(NORD.success));
        assert_eq!(deletion.style.fg, Some(NORD.danger));

        let addition_only = TreeRow { deletions: 0, ..row };
        assert!(!line_text(&tree_line(
            &addition_only,
            40,
            Style::default(),
            &HashSet::new(),
            None,
            NORD,
        ))
        .contains("-0"));
    }

    #[test]
    fn hovered_file_replaces_counts_with_its_repository_action() {
        let row = TreeRow {
            target: TreeTarget::File(0),
            depth: 1,
            label: "lib.rs".to_owned(),
            additions: 12,
            deletions: 3,
            kind: Some(ChangeKind::Modified),
        };

        let stage = tree_line(
            &row,
            40,
            Style::default(),
            &HashSet::new(),
            Some(ChangeGroup::Changes),
            NORD,
        );
        assert!(stage.spans.iter().any(|span| span.content == " + "));
        assert!(!line_text(&stage).contains("+12"));
        assert!(!line_text(&stage).contains("-3"));

        let unstage =
            tree_line(&row, 40, Style::default(), &HashSet::new(), Some(ChangeGroup::Staged), NORD);
        assert!(unstage.spans.iter().any(|span| span.content == " - "));
    }

    #[test]
    fn clicking_hover_adornment_selects_file_and_requests_stage_toggle() {
        let documents = vec![
            document(ChangeGroup::Staged, "src/staged.rs", "old\n", "new\n"),
            document(ChangeGroup::Changes, "src/changes.rs", "old\n", "new\n"),
        ];
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = DiffApp::new(documents, NORD, ViewMode::Inline);
        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        let row = app
            .regions
            .tree
            .iter()
            .find_map(|(area, target)| match target {
                TreeTarget::File(1) => Some(*area),
                _ => None,
            })
            .expect("unstaged file row");
        assert_eq!(
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column: row.x,
                row: row.y,
                modifiers: KeyModifiers::NONE,
            }),
            Flow::Continue
        );
        assert_eq!(app.hovered_file, Some(1));

        terminal.draw(|frame| render(frame, &mut app)).unwrap();
        let action = app
            .regions
            .tree_actions
            .iter()
            .find_map(|(area, index)| (*index == 1).then_some(*area))
            .expect("stage action hitbox");
        assert_eq!(
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: action.x + action.width - 1,
                row: action.y,
                modifiers: KeyModifiers::NONE,
            }),
            Flow::ToggleStage
        );
        assert_eq!(app.selected, Some(1));

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: app.regions.content_area.x,
            row: app.regions.content_area.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.hovered_file, None);
    }

    #[test]
    fn tree_compacts_uninterrupted_directory_chains_with_terminal_identity() {
        let documents =
            vec![document(ChangeGroup::Changes, "packages/diffs/src/render.rs", "old\n", "new\n")];
        let mut expanded = directory_keys(&documents).into_iter().collect::<HashSet<_>>();

        let rows = tree_rows(&documents, &expanded);
        let compact = rows.iter().find(|row| row.label == "packages/diffs/src").unwrap();
        assert_eq!(compact.depth, 1);
        assert!(matches!(
            &compact.target,
            TreeTarget::Directory(group, path)
                if *group == ChangeGroup::Changes && path == Path::new("packages/diffs/src")
        ));
        assert!(rows.iter().any(|row| row.label == "render.rs" && row.depth == 2));

        expanded.remove(&(ChangeGroup::Changes, "packages/diffs/src".into()));
        let collapsed = tree_rows(&documents, &expanded);
        assert!(!collapsed.iter().any(|row| row.label == "render.rs"));
    }

    #[test]
    fn tree_compaction_stops_at_mixed_and_branching_directories() {
        let documents = vec![
            document(ChangeGroup::Changes, "src/lib.rs", "old\n", "new\n"),
            document(ChangeGroup::Changes, "src/tools/diff.rs", "old\n", "new\n"),
            document(ChangeGroup::Changes, "src/ui/view.rs", "old\n", "new\n"),
        ];
        let expanded = directory_keys(&documents).into_iter().collect::<HashSet<_>>();
        let rows = tree_rows(&documents, &expanded);

        assert!(rows.iter().any(|row| row.label == "src" && row.depth == 1));
        assert!(rows.iter().any(|row| row.label == "tools" && row.depth == 2));
        assert!(rows.iter().any(|row| row.label == "ui" && row.depth == 2));
        assert!(!rows.iter().any(|row| row.label == "src/tools"));
    }

    #[test]
    fn both_projections_account_for_every_canonical_aligned_row() {
        let document =
            document(ChangeGroup::Changes, "src/lib.rs", "one\ntwo\n", "ONE\nTWO\nthree\n");
        let DiffBody::Text(text) = &document.body else {
            panic!("text fixture");
        };
        let app = DiffApp::new(vec![document.clone()], NORD, ViewMode::Inline);
        let inline = inline_text_lines(text, &app, 100);
        let split = split_text_lines(text, &app, 100);

        for (hunk_index, hunk) in text.hunks.iter().enumerate() {
            for (row_index, row) in hunk.rows.iter().enumerate() {
                let anchor = ReviewAnchor { hunk: hunk_index, row: Some(row_index) };
                let split_count = split.iter().filter(|line| line.anchor == anchor).count();
                let inline_count = inline.iter().filter(|line| line.anchor == anchor).count();
                let expected_inline = match row.kind {
                    RowKind::Context => 1,
                    RowKind::Changed => {
                        usize::from(row.old.is_some()) + usize::from(row.new.is_some())
                    }
                };
                assert_eq!(split_count, 1, "split row {anchor:?}");
                assert_eq!(inline_count, expected_inline, "inline row {anchor:?}");
            }
        }
    }

    #[test]
    fn explicit_split_reports_its_real_minimum_width() {
        let documents = vec![document(ChangeGroup::Changes, "a.rs", "a\n", "b\n")];
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = DiffApp::new(documents, NORD, ViewMode::Split);

        terminal.draw(|frame| render(frame, &mut app)).unwrap();

        assert!(screen(&terminal).contains("Split mode needs"));
    }

    #[test]
    fn changed_rows_use_dedicated_muted_backgrounds() {
        let deleted = deleted_background(NORD);
        let added = added_background(NORD);
        assert_eq!(deleted, DARK_DELETED_BACKGROUND);
        assert_eq!(added, DARK_ADDED_BACKGROUND);
        assert_ne!(deleted, added);

        for background in [deleted, added] {
            let gutter = readable_foreground(NORD.text_muted, Some(background), NORD);
            assert!(contrast_ratio(gutter, background).unwrap() >= 4.5);
        }

        let document = document(
            ChangeGroup::Changes,
            "src/lib.rs",
            "let old_value = 30;\n",
            "let new_value = 35;\n",
        );
        let DiffBody::Text(text) = &document.body else {
            panic!("text fixture");
        };
        let app = DiffApp::new(vec![document.clone()], NORD, ViewMode::Split);
        let mut tinted_spans = 0;
        for rendered in split_text_lines(text, &app, 100) {
            for span in rendered.line.spans {
                if let Some(background) = span.style.bg {
                    tinted_spans += 1;
                    assert!([DARK_ADDED_BACKGROUND, DARK_DELETED_BACKGROUND].contains(&background));
                }
            }
        }
        assert!(tinted_spans > 0);
    }

    #[test]
    fn changed_fragments_retain_their_row_background() {
        let document = document(
            ChangeGroup::Changes,
            "src/lib.rs",
            "let old_value = 30;\n",
            "let new_value = 35;\n",
        );
        let DiffBody::Text(text) = &document.body else {
            panic!("text fixture");
        };
        let app = DiffApp::new(vec![document.clone()], NORD, ViewMode::Inline);
        let mut emphasized_fragments = 0;

        for rendered in inline_text_lines(text, &app, 100) {
            for span in rendered.line.spans {
                if span.style.add_modifier.contains(Modifier::UNDERLINED) {
                    emphasized_fragments += 1;
                    assert!([DARK_ADDED_BACKGROUND, DARK_DELETED_BACKGROUND]
                        .contains(&span.style.bg.unwrap()));
                }
            }
        }
        assert!(emphasized_fragments > 0);
    }
}
