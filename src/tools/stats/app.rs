use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use super::actions::{ActionRequest, ActionResult};
use super::history::{HistorySeries, HistoryStore};
use super::host::ProcessAction;
use super::model::{
    DetailData, DetailRequest, DetailRequestKind, DetailSnapshot, ProcessIdentity, ProcessKey,
    ProcessSample, StatsSnapshot, ThreadSample,
};
use super::render::UiRegions;
use super::tree::{FamilyView, ProcessForest, TreeQuery, TreeSort};
use crate::tui::{Direction, LineEditor};

const HISTORY: usize = 120;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SortBy {
    Cpu,
    Memory,
    Pid,
    Name,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ThreadSortBy {
    Cpu,
    Accumulated,
    Tid,
    Name,
}

impl ThreadSortBy {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Accumulated => "TOTAL",
            Self::Tid => "TID",
            Self::Name => "NAME",
        }
    }
}

impl SortBy {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Memory => "RAM",
            Self::Pid => "PID",
            Self::Name => "NAME",
        }
    }
}

pub(super) enum Action {
    None,
    Quit,
    Process(ProcessKey, ProcessAction),
}

#[derive(Debug)]
pub(super) enum ActionLifecycle {
    Idle,
    Running(ActionRequest),
    Succeeded(ActionRequest),
    Failed { request: ActionRequest, message: String },
}

impl ActionLifecycle {
    pub(super) fn status(&self) -> Option<String> {
        match self {
            Self::Idle => None,
            Self::Running(request) => {
                Some(format!("Requesting {} for PID {}…", request.action.label(), request.key.pid))
            }
            Self::Succeeded(request) => {
                Some(format!("Requested {} for PID {}", request.action.label(), request.key.pid))
            }
            Self::Failed { request, message } => Some(format!(
                "Could not {} PID {}: {message}",
                request.action.label(),
                request.key.pid
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DetailIntent {
    Request(DetailRequest),
    Clear,
}

impl DetailIntent {
    pub(super) fn request(self) -> Option<DetailRequest> {
        match self {
            Self::Request(request) => Some(request),
            Self::Clear => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InspectorTab {
    Overview,
    Family,
    Threads,
    Resources,
    Profile,
}

impl InspectorTab {
    pub(super) const ALL: [Self; 5] =
        [Self::Overview, Self::Family, Self::Threads, Self::Resources, Self::Profile];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Overview => "OVERVIEW",
            Self::Family => "FAMILY",
            Self::Threads => "THREADS",
            Self::Resources => "RESOURCES",
            Self::Profile => "PROFILE",
        }
    }

    pub(super) fn next(self, delta: isize) -> Self {
        let index = Self::ALL.iter().position(|tab| *tab == self).unwrap_or_default();
        Self::ALL[(index as isize + delta).rem_euclid(Self::ALL.len() as isize) as usize]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ActiveRegion {
    Processes,
    Inspector,
}

pub(super) struct VisibleRow {
    pub(super) key: ProcessIdentity,
    pub(super) pid: u32,
    pub(super) cpu: f32,
    pub(super) memory: u64,
    pub(super) depth: u16,
    pub(super) family_cpu: f32,
    pub(super) family_memory: u64,
    pub(super) has_children: bool,
    pub(super) hidden_descendants: usize,
    pub(super) is_match: bool,
    pub(super) is_context: bool,
}

pub(super) struct Confirmation {
    pub(super) key: ProcessKey,
    pub(super) action: ProcessAction,
    pub(super) name: String,
    pub(super) choice: ConfirmationChoice,
}

pub(super) struct CommandViewer {
    pub(super) name: String,
    pub(super) pid: u32,
    pub(super) command: String,
    pub(super) row_offset: usize,
    pub(super) column_offset: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConfirmationChoice {
    Confirm,
    Force,
    Cancel,
}

impl ConfirmationChoice {
    fn next(self, delta: isize) -> Self {
        const ALL: [ConfirmationChoice; 3] =
            [ConfirmationChoice::Confirm, ConfirmationChoice::Force, ConfirmationChoice::Cancel];
        let index = ALL.iter().position(|choice| *choice == self).unwrap_or_default();
        ALL[(index as isize + delta).rem_euclid(ALL.len() as isize) as usize]
    }
}

pub(super) struct StatsApp {
    pub(super) snapshot: Arc<StatsSnapshot>,
    forest: ProcessForest,
    pub(super) detail: Option<Arc<DetailSnapshot>>,
    pub(super) expected_detail: Option<DetailRequest>,
    next_detail_request_id: u64,
    pub(super) selected: Option<ProcessIdentity>,
    selected_summary: Option<ProcessSample>,
    pub(super) collapsed: HashSet<ProcessIdentity>,
    pub(super) focused_process: Option<ProcessIdentity>,
    pub(super) focused_core: Option<u16>,
    pub(super) sort: SortBy,
    pub(super) descending: bool,
    pub(super) inspector_tab: InspectorTab,
    pub(super) family_cursor: usize,
    pub(super) thread_cursor: usize,
    pub(super) thread_offset: usize,
    pub(super) thread_sort: ThreadSortBy,
    pub(super) thread_descending: bool,
    pub(super) active_region: ActiveRegion,
    pub(super) filter: LineEditor,
    pub(super) filtering: bool,
    pub(super) visible: Vec<VisibleRow>,
    family_view: Option<FamilyView>,
    pub(super) row_offset: usize,
    pub(super) viewport_rows: usize,
    pub(super) histories: Vec<VecDeque<u64>>,
    process_history: HistoryStore,
    pub(super) confirm: Option<Confirmation>,
    pub(super) command_viewer: Option<CommandViewer>,
    pub(super) action_lifecycle: ActionLifecycle,
    pub(super) status: Option<String>,
}

impl StatsApp {
    pub(super) fn new(snapshot: Arc<StatsSnapshot>) -> Self {
        let forest = ProcessForest::new(&snapshot.processes);
        let selected = snapshot
            .processes
            .iter()
            .max_by(|left, right| left.cpu_percent.total_cmp(&right.cpu_percent))
            .map(|process| process.identity);
        let selected_summary = selected.and_then(|selected| {
            snapshot.processes.iter().find(|process| process.identity == selected).cloned()
        });
        let mut app = Self {
            snapshot,
            forest,
            detail: None,
            expected_detail: None,
            next_detail_request_id: 1,
            selected,
            selected_summary,
            collapsed: HashSet::new(),
            focused_process: None,
            focused_core: None,
            sort: SortBy::Cpu,
            descending: true,
            inspector_tab: InspectorTab::Overview,
            family_cursor: 0,
            thread_cursor: 0,
            thread_offset: 0,
            thread_sort: ThreadSortBy::Cpu,
            thread_descending: true,
            active_region: ActiveRegion::Processes,
            filter: LineEditor::default(),
            filtering: false,
            visible: Vec::new(),
            family_view: None,
            row_offset: 0,
            viewport_rows: 0,
            histories: Vec::new(),
            process_history: HistoryStore::default(),
            confirm: None,
            command_viewer: None,
            action_lifecycle: ActionLifecycle::Idle,
            status: None,
        };
        app.record_history();
        app.reproject();
        app
    }

    pub(super) fn ingest(&mut self, snapshot: Arc<StatsSnapshot>) {
        let previous_live = self.selected_process().cloned();
        self.forest = ProcessForest::new(&snapshot.processes);
        self.snapshot = snapshot;
        if let Some(live) = self.selected_process().cloned() {
            self.selected_summary = Some(live);
        } else if previous_live.is_some() {
            self.selected_summary = previous_live;
        }
        if self.selected.is_some_and(|selected| {
            !self.snapshot.processes.iter().any(|process| process.identity == selected)
        }) {
            self.confirm = None;
        }
        self.row_offset = 0;
        self.record_history();
        self.reproject();
    }

    pub(super) fn ingest_detail(&mut self, detail: Option<Arc<DetailSnapshot>>) {
        if detail.as_ref().is_some_and(|detail| Some(detail.request()) != self.expected_detail) {
            return;
        }
        let affects_projection = detail
            .as_deref()
            .is_some_and(|detail| matches!(detail.detail, DetailData::Core { .. }));
        self.detail = detail;
        if affects_projection {
            self.reproject();
        }
    }

    pub(super) fn record_history(&mut self) {
        let protected = [self.selected, self.focused_process]
            .into_iter()
            .flatten()
            .filter_map(ProcessIdentity::stable_key)
            .collect::<Vec<_>>();
        if !self.process_history.record(&self.snapshot, protected) {
            return;
        }
        self.histories.resize_with(self.snapshot.system.cpus.len(), VecDeque::new);
        for (history, cpu) in self.histories.iter_mut().zip(&self.snapshot.system.cpus) {
            if history.len() == HISTORY {
                history.pop_front();
            }
            history.push_back(cpu.usage_percent.round() as u64);
        }
    }

    pub(super) fn detail_kind(&self) -> Option<DetailRequestKind> {
        if let Some(core) = self.focused_core {
            Some(DetailRequestKind::Core { logical_index: core })
        } else {
            let process = self.selected_process()?.identity.stable_key()?;
            match self.inspector_tab {
                InspectorTab::Threads if self.snapshot.host.threads.reason().is_none() => {
                    Some(DetailRequestKind::Threads { process })
                }
                InspectorTab::Resources if self.snapshot.host.resources.reason().is_none() => {
                    Some(DetailRequestKind::Resources { process })
                }
                _ => None,
            }
        }
    }

    pub(super) fn reconcile_detail_intent(&mut self) -> Option<DetailIntent> {
        let desired = self.detail_kind();
        if self.expected_detail.map(|request| request.kind) == desired {
            return None;
        }
        let projection_used_old_core_detail = self.focused_core.is_some()
            || self
                .detail
                .as_deref()
                .is_some_and(|detail| matches!(detail.detail, DetailData::Core { .. }));
        let request = desired.map(|kind| {
            let request = DetailRequest { request_id: self.next_detail_request_id, kind };
            self.next_detail_request_id = self
                .next_detail_request_id
                .checked_add(1)
                .expect("detail request identifier exhausted");
            request
        });
        self.expected_detail = request;
        self.detail = None;
        if projection_used_old_core_detail {
            self.reproject();
        }
        Some(request.map_or(DetailIntent::Clear, DetailIntent::Request))
    }

    pub(super) fn reproject(&mut self) {
        let core_cpu = self.core_process_cpu();
        let projection = self.forest.project(
            &self.snapshot.processes,
            TreeQuery {
                collapsed: &self.collapsed,
                focus: self.focused_process,
                filter: self.filter.value(),
                sort: match self.sort {
                    SortBy::Cpu => TreeSort::Cpu,
                    SortBy::Memory => TreeSort::Memory,
                    SortBy::Pid => TreeSort::Pid,
                    SortBy::Name => TreeSort::Name,
                },
                descending: self.descending,
            },
        );
        self.visible = projection
            .into_iter()
            .filter_map(|row| {
                let process = self.forest.process(&self.snapshot.processes, row.key)?;
                if core_cpu
                    .as_ref()
                    .is_some_and(|core_cpu| !core_cpu.contains_key(&process.identity))
                {
                    return None;
                }
                Some(VisibleRow {
                    key: process.identity,
                    pid: process.identity.pid(),
                    cpu: core_cpu
                        .as_ref()
                        .and_then(|core_cpu| core_cpu.get(&process.identity))
                        .copied()
                        .unwrap_or(process.cpu_percent),
                    memory: process.rss_bytes,
                    depth: row.depth,
                    family_cpu: row.family_cpu_percent,
                    family_memory: row.family_memory_bytes,
                    has_children: row.has_children,
                    hidden_descendants: row.hidden_descendants,
                    is_match: row.is_match,
                    is_context: row.is_context,
                })
            })
            .collect();
        if self.selected.is_none() {
            if let Some(key) = self.visible.first().map(|row| row.key) {
                self.select_identity(key);
            }
        }
        self.row_offset = self.row_offset.min(self.visible.len().saturating_sub(1));
        self.refresh_family_view();
    }

    pub(super) fn core_process_cpu(&self) -> Option<HashMap<ProcessIdentity, f32>> {
        let mut values = HashMap::new();
        let core = self.focused_core?;
        let detail = self.detail.as_deref()?;
        let DetailData::Core { logical_index, outcome } = &detail.detail else { return None };
        if *logical_index != core {
            return None;
        }
        let threads = outcome.value()?;
        let identities = self
            .snapshot
            .processes
            .iter()
            .filter_map(|process| process.identity.stable_key().map(|key| (key, process.identity)))
            .collect::<HashMap<_, _>>();
        for thread in threads.iter().filter(|thread| {
            matches!(thread.last_cpu, super::model::Observed::Value(last_cpu) if last_cpu == core)
        }) {
            if let Some(identity) = identities.get(&thread.process) {
                if let Some(cpu) = thread.cpu_percent.value() {
                    *values.entry(*identity).or_insert(0.0) += *cpu;
                }
            }
        }
        Some(values)
    }

    pub(super) fn selected_process(&self) -> Option<&ProcessSample> {
        self.process(self.selected?)
    }

    pub(super) fn process(&self, key: ProcessIdentity) -> Option<&ProcessSample> {
        self.forest.process(&self.snapshot.processes, key)
    }

    pub(super) fn selected_inspection(&self) -> Option<(&ProcessSample, bool)> {
        self.selected_process().map(|process| (process, true)).or_else(|| {
            let selected = self.selected?;
            self.selected_summary
                .as_ref()
                .filter(|process| process.identity == selected)
                .map(|process| (process, false))
        })
    }

    pub(super) fn selected_history(&self) -> Option<&HistorySeries> {
        self.selected
            .and_then(ProcessIdentity::stable_key)
            .and_then(|key| self.process_history.get(key))
    }

    pub(super) fn selected_family(&self) -> Option<&FamilyView> {
        self.family_view.as_ref()
    }

    pub(super) fn select_index(&mut self, index: usize) {
        if let Some(key) = self.visible.get(index).map(|row| row.key) {
            self.select_identity(key);
        }
    }

    fn select_identity(&mut self, key: ProcessIdentity) {
        if self.selected != Some(key) {
            self.family_cursor = 0;
            self.thread_cursor = 0;
            self.thread_offset = 0;
        }
        self.selected = Some(key);
        self.selected_summary =
            self.snapshot.processes.iter().find(|process| process.identity == key).cloned();
        self.refresh_family_view();
    }

    fn refresh_family_view(&mut self) {
        self.family_view = if self.inspector_tab == InspectorTab::Family {
            self.selected
                .and_then(|selected| self.forest.family_view(selected, &self.snapshot.processes))
        } else {
            None
        };
        self.family_cursor = self.family_cursor.min(self.family_row_count().saturating_sub(1));
    }

    pub(super) fn family_row_count(&self) -> usize {
        self.family_view.as_ref().map_or(0, |family| {
            family.direct_children.len()
                + family.hot_descendants.len()
                + family.memory_descendants.len()
        })
    }

    pub(super) fn family_row_key(&self, index: usize) -> Option<ProcessIdentity> {
        let family = self.family_view.as_ref()?;
        family
            .direct_children
            .iter()
            .chain(&family.hot_descendants)
            .chain(&family.memory_descendants)
            .nth(index)
            .map(|member| member.key)
    }

    pub(super) fn move_selection(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let current = self
            .selected
            .and_then(|selected| self.visible.iter().position(|row| row.key == selected))
            .unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.visible.len() as isize - 1) as usize;
        self.select_index(next);
        if next < self.row_offset {
            self.row_offset = next;
        } else if self.viewport_rows > 0 && next >= self.row_offset + self.viewport_rows {
            self.row_offset = next + 1 - self.viewport_rows;
        }
    }

    pub(super) fn toggle_branch(&mut self) {
        let Some(key) = self.selected else { return };
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.reproject();
    }

    pub(super) fn focus_core(&mut self, delta: isize) {
        let count = self.snapshot.system.cpus.len();
        if count == 0 {
            return;
        }
        let current = self.focused_core.map_or(if delta < 0 { 0 } else { count - 1 }, usize::from);
        let next = (current as isize + delta).rem_euclid(count as isize) as u16;
        self.focused_core = Some(next);
        self.reproject();
    }

    pub(super) fn set_sort(&mut self, sort: SortBy) {
        if self.sort == sort {
            self.descending = !self.descending;
        } else {
            self.sort = sort;
            self.descending = !matches!(sort, SortBy::Name | SortBy::Pid);
        }
        self.reproject();
    }

    pub(super) fn on_event(&mut self, event: Event, regions: &UiRegions) -> Action {
        match event {
            Event::Key(key) => self.on_key(key, regions),
            Event::Mouse(mouse) => self.on_mouse(mouse, regions),
            _ => Action::None,
        }
    }

    pub(super) fn on_key(&mut self, key: KeyEvent, regions: &UiRegions) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        if self.confirm.is_some() {
            return self.on_confirmation_key(key);
        }
        if self.command_viewer.is_some() {
            return self.on_command_viewer_key(key, regions);
        }
        if self.filtering {
            match key.code {
                KeyCode::Enter => self.filtering = false,
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                    self.reproject();
                }
                _ => {
                    self.filter.apply_key(key);
                    self.reproject();
                }
            }
            return Action::None;
        }
        if let Some(action) = self.on_inspector_table_key(key, regions) {
            return action;
        }
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc => {
                if self.active_region == ActiveRegion::Inspector {
                    self.active_region = ActiveRegion::Processes;
                } else if self.focused_process.take().is_none()
                    && self.focused_core.take().is_none()
                {
                    return Action::Quit;
                }
                self.reproject();
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.active_region == ActiveRegion::Processes {
                    self.move_selection(-1);
                }
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.active_region == ActiveRegion::Processes {
                    self.move_selection(1);
                }
                Action::None
            }
            KeyCode::PageUp => {
                if self.active_region == ActiveRegion::Processes {
                    self.move_selection(-10);
                }
                Action::None
            }
            KeyCode::PageDown => {
                if self.active_region == ActiveRegion::Processes {
                    self.move_selection(10);
                }
                Action::None
            }
            KeyCode::Enter => {
                self.active_region = ActiveRegion::Inspector;
                Action::None
            }
            KeyCode::Right => {
                match self.active_region {
                    ActiveRegion::Processes => {
                        if !self.expand_selected() {
                            self.move_region(Direction::Right, regions);
                        }
                    }
                    ActiveRegion::Inspector => self.move_inspector_tab(1),
                }
                Action::None
            }
            KeyCode::Char(' ') => {
                self.toggle_branch();
                Action::None
            }
            KeyCode::Left => {
                match self.active_region {
                    ActiveRegion::Processes => {
                        if !self.collapse_or_select_parent() {
                            self.move_region(Direction::Left, regions);
                        }
                    }
                    ActiveRegion::Inspector if self.inspector_tab == InspectorTab::Overview => {
                        self.move_region(Direction::Left, regions)
                    }
                    ActiveRegion::Inspector => self.move_inspector_tab(-1),
                }
                Action::None
            }
            KeyCode::Tab => {
                self.move_tab(1, regions);
                Action::None
            }
            KeyCode::BackTab => {
                self.move_tab(-1, regions);
                Action::None
            }
            KeyCode::Char('/') => {
                self.filtering = true;
                Action::None
            }
            KeyCode::Char('c') => {
                self.focused_core = None;
                self.reproject();
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
            KeyCode::Char('f') => {
                self.focused_process =
                    if self.focused_process == self.selected { None } else { self.selected };
                self.reproject();
                Action::None
            }
            KeyCode::Char('p') => {
                self.set_inspector_tab(InspectorTab::Profile);
                self.active_region = ActiveRegion::Inspector;
                Action::None
            }
            KeyCode::Char('v') if self.inspector_tab == InspectorTab::Overview => {
                self.open_command_viewer();
                Action::None
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                self.open_confirmation(ProcessAction::GracefulTerminate);
                Action::None
            }
            KeyCode::Char('X') => {
                self.open_confirmation(ProcessAction::ForceTerminate);
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

    fn on_inspector_table_key(&mut self, key: KeyEvent, regions: &UiRegions) -> Option<Action> {
        if self.active_region != ActiveRegion::Inspector {
            return None;
        }
        match (self.inspector_tab, key.code) {
            (InspectorTab::Family, KeyCode::Up | KeyCode::Char('k')) => {
                self.family_cursor = self.family_cursor.saturating_sub(1);
                Some(Action::None)
            }
            (InspectorTab::Family, KeyCode::Down | KeyCode::Char('j')) => {
                self.family_cursor = self
                    .family_cursor
                    .saturating_add(1)
                    .min(self.family_row_count().saturating_sub(1));
                Some(Action::None)
            }
            (InspectorTab::Family, KeyCode::PageUp) => {
                self.family_cursor =
                    self.family_cursor.saturating_sub(regions.family_rows.len().max(1));
                Some(Action::None)
            }
            (InspectorTab::Family, KeyCode::PageDown) => {
                self.family_cursor = self
                    .family_cursor
                    .saturating_add(regions.family_rows.len().max(1))
                    .min(self.family_row_count().saturating_sub(1));
                Some(Action::None)
            }
            (InspectorTab::Family, KeyCode::Enter) => {
                if let Some(key) = self.family_row_key(self.family_cursor) {
                    self.select_identity(key);
                }
                Some(Action::None)
            }
            (InspectorTab::Threads, KeyCode::Up | KeyCode::Char('k')) => {
                self.move_thread_cursor(-1, regions.thread_rows.len());
                Some(Action::None)
            }
            (InspectorTab::Threads, KeyCode::Down | KeyCode::Char('j')) => {
                self.move_thread_cursor(1, regions.thread_rows.len());
                Some(Action::None)
            }
            (InspectorTab::Threads, KeyCode::PageUp) => {
                self.move_thread_cursor(
                    -(regions.thread_rows.len() as isize),
                    regions.thread_rows.len(),
                );
                Some(Action::None)
            }
            (InspectorTab::Threads, KeyCode::PageDown) => {
                self.move_thread_cursor(
                    regions.thread_rows.len() as isize,
                    regions.thread_rows.len(),
                );
                Some(Action::None)
            }
            (InspectorTab::Threads, KeyCode::Char('1')) => {
                self.set_thread_sort(ThreadSortBy::Cpu);
                Some(Action::None)
            }
            (InspectorTab::Threads, KeyCode::Char('2')) => {
                self.set_thread_sort(ThreadSortBy::Accumulated);
                Some(Action::None)
            }
            (InspectorTab::Threads, KeyCode::Char('3')) => {
                self.set_thread_sort(ThreadSortBy::Tid);
                Some(Action::None)
            }
            (InspectorTab::Threads, KeyCode::Char('4')) => {
                self.set_thread_sort(ThreadSortBy::Name);
                Some(Action::None)
            }
            _ => None,
        }
    }

    fn move_thread_cursor(&mut self, delta: isize, viewport_rows: usize) {
        let count = self.sorted_threads().len();
        if count == 0 {
            self.thread_cursor = 0;
            self.thread_offset = 0;
            return;
        }
        self.thread_cursor = (self.thread_cursor as isize + delta)
            .clamp(0, count.saturating_sub(1) as isize) as usize;
        if self.thread_cursor < self.thread_offset {
            self.thread_offset = self.thread_cursor;
        } else if viewport_rows > 0 && self.thread_cursor >= self.thread_offset + viewport_rows {
            self.thread_offset = self.thread_cursor + 1 - viewport_rows;
        }
    }

    fn set_thread_sort(&mut self, sort: ThreadSortBy) {
        if self.thread_sort == sort {
            self.thread_descending = !self.thread_descending;
        } else {
            self.thread_sort = sort;
            self.thread_descending = !matches!(sort, ThreadSortBy::Tid | ThreadSortBy::Name);
        }
        self.thread_cursor = 0;
        self.thread_offset = 0;
    }

    pub(super) fn sorted_threads(&self) -> Vec<&ThreadSample> {
        let Some(process) = self.selected.and_then(ProcessIdentity::stable_key) else {
            return Vec::new();
        };
        let mut rows = self
            .detail
            .iter()
            .flat_map(|detail| detail.threads().into_iter().flatten())
            .filter(|thread| thread.process == process)
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            let primary = match self.thread_sort {
                ThreadSortBy::Cpu => {
                    observed_f32(&left.cpu_percent).total_cmp(&observed_f32(&right.cpu_percent))
                }
                ThreadSortBy::Accumulated => observed_f64(&left.accumulated_cpu_seconds)
                    .total_cmp(&observed_f64(&right.accumulated_cpu_seconds)),
                ThreadSortBy::Tid => left.tid.cmp(&right.tid),
                ThreadSortBy::Name => observed_name(&left.name).cmp(observed_name(&right.name)),
            };
            (if self.thread_descending { primary.reverse() } else { primary })
                .then_with(|| left.tid.cmp(&right.tid))
        });
        rows
    }

    pub(super) fn expand_selected(&mut self) -> bool {
        let Some(key) = self.selected else { return false };
        let has_children = self.visible.iter().any(|row| row.key == key && row.has_children);
        if has_children && self.collapsed.remove(&key) {
            self.reproject();
            true
        } else {
            false
        }
    }

    pub(super) fn collapse_or_select_parent(&mut self) -> bool {
        let Some(key) = self.selected else { return false };
        let expanded = self
            .visible
            .iter()
            .any(|row| row.key == key && row.has_children && !self.collapsed.contains(&key));
        if expanded {
            self.collapsed.insert(key);
        } else if let Some(parent) = self.parent_key(key) {
            self.select_identity(parent);
        } else {
            return false;
        }
        self.reproject();
        true
    }

    pub(super) fn move_inspector_tab(&mut self, delta: isize) {
        self.set_inspector_tab(self.inspector_tab.next(delta));
    }

    pub(super) fn set_inspector_tab(&mut self, tab: InspectorTab) {
        self.inspector_tab = tab;
        self.refresh_family_view();
    }

    pub(super) fn move_region(&mut self, direction: Direction, regions: &UiRegions) {
        if let Some(region) = regions.navigation().neighbor(self.active_region, direction) {
            self.active_region = region;
            return;
        }
        match (self.active_region, direction) {
            (ActiveRegion::Processes, Direction::Right) => {
                self.active_region = ActiveRegion::Inspector
            }
            (ActiveRegion::Inspector, Direction::Left) => {
                self.active_region = ActiveRegion::Processes
            }
            _ => {}
        }
    }

    pub(super) fn move_tab(&mut self, delta: isize, regions: &UiRegions) {
        let navigation = regions.navigation();
        let next = if delta < 0 {
            navigation.previous(self.active_region)
        } else {
            navigation.next(self.active_region)
        };
        self.active_region =
            next.filter(|next| *next != self.active_region).unwrap_or(match self.active_region {
                ActiveRegion::Processes => ActiveRegion::Inspector,
                ActiveRegion::Inspector => ActiveRegion::Processes,
            });
    }

    pub(super) fn parent_key(&self, key: ProcessIdentity) -> Option<ProcessIdentity> {
        self.forest.parent(key)
    }

    pub(super) fn on_confirmation_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                self.confirm = None;
                Action::None
            }
            KeyCode::Char('f') => {
                if let Some(confirm) = &mut self.confirm {
                    confirm.choice = ConfirmationChoice::Force;
                }
                Action::None
            }
            KeyCode::Left | KeyCode::BackTab => {
                if let Some(confirm) = &mut self.confirm {
                    confirm.choice = confirm.choice.next(-1);
                }
                Action::None
            }
            KeyCode::Right | KeyCode::Tab => {
                if let Some(confirm) = &mut self.confirm {
                    confirm.choice = confirm.choice.next(1);
                }
                Action::None
            }
            KeyCode::Char('y') => self.confirm_action(ConfirmationChoice::Confirm),
            KeyCode::Enter => {
                let choice = self.confirm.as_ref().expect("confirmation checked above").choice;
                self.confirm_action(choice)
            }
            _ => Action::None,
        }
    }

    fn confirm_action(&mut self, choice: ConfirmationChoice) -> Action {
        if choice == ConfirmationChoice::Cancel {
            self.confirm = None;
            return Action::None;
        }
        let confirm = self.confirm.take().expect("confirmation checked above");
        let action = if choice == ConfirmationChoice::Force {
            ProcessAction::ForceTerminate
        } else {
            confirm.action
        };
        Action::Process(confirm.key, action)
    }

    fn on_command_viewer_key(&mut self, key: KeyEvent, regions: &UiRegions) -> Action {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.command_viewer = None;
            return Action::None;
        }
        let Some(viewer) = &mut self.command_viewer else { return Action::None };
        let viewport = regions.command_content.unwrap_or_default();
        let height = viewport.height.max(1) as usize;
        let width = viewport.width.max(1) as usize;
        let line_count = viewer.command.lines().count().max(1);
        let longest =
            viewer.command.lines().map(|line| line.chars().count()).max().unwrap_or_default();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                viewer.row_offset = viewer.row_offset.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                viewer.row_offset =
                    viewer.row_offset.saturating_add(1).min(line_count.saturating_sub(height))
            }
            KeyCode::PageUp => viewer.row_offset = viewer.row_offset.saturating_sub(height),
            KeyCode::PageDown => {
                viewer.row_offset =
                    viewer.row_offset.saturating_add(height).min(line_count.saturating_sub(height))
            }
            KeyCode::Left | KeyCode::Char('h') => {
                viewer.column_offset = viewer.column_offset.saturating_sub(4)
            }
            KeyCode::Right | KeyCode::Char('l') => {
                viewer.column_offset =
                    viewer.column_offset.saturating_add(4).min(longest.saturating_sub(width))
            }
            KeyCode::Home => {
                viewer.row_offset = 0;
                viewer.column_offset = 0;
            }
            _ => {}
        }
        Action::None
    }

    fn open_command_viewer(&mut self) {
        if let Some((process, _)) = self.selected_inspection() {
            self.command_viewer = Some(CommandViewer {
                name: process.name.clone(),
                pid: process.identity.pid(),
                command: process.command.clone(),
                row_offset: 0,
                column_offset: 0,
            });
        }
    }

    pub(super) fn on_mouse(&mut self, mouse: MouseEvent, regions: &UiRegions) -> Action {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                match (self.active_region, self.inspector_tab) {
                    (ActiveRegion::Inspector, InspectorTab::Family) => {
                        self.family_cursor = self.family_cursor.saturating_sub(3)
                    }
                    (ActiveRegion::Inspector, InspectorTab::Threads) => {
                        self.move_thread_cursor(-3, regions.thread_rows.len())
                    }
                    _ => self.move_selection(-3),
                }
                return Action::None;
            }
            MouseEventKind::ScrollDown => {
                match (self.active_region, self.inspector_tab) {
                    (ActiveRegion::Inspector, InspectorTab::Family) => {
                        self.family_cursor = self
                            .family_cursor
                            .saturating_add(3)
                            .min(self.family_row_count().saturating_sub(1))
                    }
                    (ActiveRegion::Inspector, InspectorTab::Threads) => {
                        self.move_thread_cursor(3, regions.thread_rows.len())
                    }
                    _ => self.move_selection(3),
                }
                return Action::None;
            }
            MouseEventKind::Down(MouseButton::Left) => {}
            _ => return Action::None,
        }
        let point = (mouse.column, mouse.row);
        if self.command_viewer.is_some() {
            if regions.command_close.is_some_and(|area| contains(area, point)) {
                self.command_viewer = None;
            }
            return Action::None;
        }
        if self.confirm.is_some() {
            if regions.confirm_yes.is_some_and(|area| contains(area, point)) {
                return self.confirm_action(ConfirmationChoice::Confirm);
            }
            if regions.confirm_force.is_some_and(|area| contains(area, point)) {
                return self.confirm_action(ConfirmationChoice::Force);
            }
            if regions.confirm_cancel.is_some_and(|area| contains(area, point)) {
                self.confirm = None;
            }
            return Action::None;
        }
        if let Some(region) = regions.navigation().hit_test(mouse.column, mouse.row) {
            self.active_region = region;
        }
        if regions.back.is_some_and(|area| contains(area, point)) {
            self.active_region = ActiveRegion::Processes;
            return Action::None;
        }
        if let Some((_, tab)) = regions.tabs.iter().find(|(area, _)| contains(*area, point)) {
            self.set_inspector_tab(*tab);
            self.active_region = ActiveRegion::Inspector;
            return Action::None;
        }
        if let Some((_, index, key)) =
            regions.family_rows.iter().find(|(area, _, _)| contains(*area, point))
        {
            self.family_cursor = *index;
            self.select_identity(*key);
            self.set_inspector_tab(InspectorTab::Family);
            self.active_region = ActiveRegion::Inspector;
            return Action::None;
        }
        if let Some((_, index)) =
            regions.thread_rows.iter().find(|(area, _)| contains(*area, point))
        {
            self.thread_cursor = *index;
            return Action::None;
        }
        if regions.profile.is_some_and(|area| contains(area, point)) {
            self.set_inspector_tab(InspectorTab::Profile);
            self.active_region = ActiveRegion::Inspector;
            return Action::None;
        }
        if regions.command_open.is_some_and(|area| contains(area, point)) {
            self.open_command_viewer();
            return Action::None;
        }
        if let Some((_, key)) = regions.disclosures.iter().find(|(area, _)| contains(*area, point))
        {
            self.select_identity(*key);
            self.toggle_branch();
            return Action::None;
        }
        if let Some((_, core)) = regions.cores.iter().find(|(area, _)| contains(*area, point)) {
            self.focused_core = if self.focused_core == Some(*core) { None } else { Some(*core) };
            self.reproject();
            return Action::None;
        }
        if let Some((_, sort)) = regions.headers.iter().find(|(area, _)| contains(*area, point)) {
            self.set_sort(*sort);
            return Action::None;
        }
        if let Some((_, index)) = regions.rows.iter().find(|(area, _)| contains(*area, point)) {
            self.select_index(*index);
            return Action::None;
        }
        if regions.end_process.is_some_and(|area| contains(area, point)) {
            self.open_confirmation(ProcessAction::GracefulTerminate);
        }
        Action::None
    }

    pub(super) fn open_confirmation(&mut self, requested: ProcessAction) {
        if let ActionLifecycle::Running(request) = self.action_lifecycle {
            self.status = Some(format!("Action already running for PID {}", request.key.pid));
            return;
        }
        let capability = match requested {
            ProcessAction::GracefulTerminate => self.snapshot.host.graceful_terminate,
            ProcessAction::ForceTerminate => self.snapshot.host.force_terminate,
        };
        if let Some(reason) = capability.reason() {
            self.status = Some(format!("Action unavailable: {reason}"));
            return;
        }
        if let Some(process) = self.selected_process() {
            let Some(key) = process.identity.stable_key() else {
                self.status = Some("Action unavailable: process identity is snapshot-only".into());
                return;
            };
            self.confirm = Some(Confirmation {
                key,
                action: requested,
                name: process.name.clone(),
                choice: ConfirmationChoice::Cancel,
            });
        }
    }

    pub(super) fn action_started(&mut self, request: ActionRequest) {
        self.status = None;
        self.action_lifecycle = ActionLifecycle::Running(request);
    }

    pub(super) fn action_finished(&mut self, result: ActionResult) {
        self.action_lifecycle = match result.result {
            Ok(()) => ActionLifecycle::Succeeded(result.request),
            Err(error) => {
                ActionLifecycle::Failed { request: result.request, message: error.to_string() }
            }
        };
    }
}

fn observed_f32(value: &super::model::Observed<f32>) -> f32 {
    value.value().copied().unwrap_or(f32::NEG_INFINITY)
}

fn observed_f64(value: &super::model::Observed<f64>) -> f64 {
    value.value().copied().unwrap_or(f64::NEG_INFINITY)
}

fn observed_name(value: &super::model::Observed<String>) -> &str {
    value.value().map(String::as_str).unwrap_or("")
}

fn contains(area: Rect, (x, y): (u16, u16)) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}
