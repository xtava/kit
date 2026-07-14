use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use super::history::{HistorySeries, HistoryStore};
use super::host::ProcessAction;
use super::model::{
    DetailRequest, DetailRequestKind, DetailSnapshot, ProcessIdentity, ProcessKey, ProcessSample,
    StatsSnapshot,
};
use super::render::UiRegions;
use super::tree::{self, TreeQuery, TreeSort};
use crate::tui::{Direction, LineEditor};

const HISTORY: usize = 120;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SortBy {
    Cpu,
    Memory,
    Pid,
    Name,
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
    pub(super) name: String,
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
}

pub(super) struct StatsApp {
    pub(super) snapshot: Arc<StatsSnapshot>,
    pub(super) detail: Option<Arc<DetailSnapshot>>,
    pub(super) expected_detail: Option<DetailRequest>,
    pub(super) selected: Option<ProcessIdentity>,
    selected_summary: Option<ProcessSample>,
    pub(super) collapsed: HashSet<ProcessIdentity>,
    pub(super) focused_process: Option<ProcessIdentity>,
    pub(super) focused_core: Option<u16>,
    pub(super) sort: SortBy,
    pub(super) descending: bool,
    pub(super) inspector_tab: InspectorTab,
    pub(super) active_region: ActiveRegion,
    pub(super) filter: LineEditor,
    pub(super) filtering: bool,
    pub(super) visible: Vec<VisibleRow>,
    pub(super) row_offset: usize,
    pub(super) viewport_rows: usize,
    pub(super) histories: Vec<VecDeque<u64>>,
    process_history: HistoryStore,
    pub(super) confirm: Option<Confirmation>,
    pub(super) status: Option<String>,
}

impl StatsApp {
    pub(super) fn new(snapshot: Arc<StatsSnapshot>) -> Self {
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
            detail: None,
            expected_detail: None,
            selected,
            selected_summary,
            collapsed: HashSet::new(),
            focused_process: None,
            focused_core: None,
            sort: SortBy::Cpu,
            descending: true,
            inspector_tab: InspectorTab::Overview,
            active_region: ActiveRegion::Processes,
            filter: LineEditor::default(),
            filtering: false,
            visible: Vec::new(),
            row_offset: 0,
            viewport_rows: 0,
            histories: Vec::new(),
            process_history: HistoryStore::default(),
            confirm: None,
            status: None,
        };
        app.record_history();
        app.reproject();
        app
    }

    pub(super) fn ingest(&mut self, snapshot: Arc<StatsSnapshot>) {
        let previous_live = self.selected_process().cloned();
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
        if detail.as_ref().is_some_and(|detail| Some(detail.request) != self.expected_detail) {
            return;
        }
        self.detail = detail;
        self.reproject();
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

    pub(super) fn reproject(&mut self) {
        let core_cpu = self.core_process_cpu();
        let projection = tree::project(
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
        let processes = self
            .snapshot
            .processes
            .iter()
            .map(|process| (process.identity, process))
            .collect::<HashMap<_, _>>();
        self.visible = projection
            .into_iter()
            .filter_map(|row| {
                let process = processes.get(&row.key)?;
                if self.focused_core.is_some() && !core_cpu.contains_key(&process.identity) {
                    return None;
                }
                Some(VisibleRow {
                    key: process.identity,
                    name: process.name.clone(),
                    pid: process.identity.pid(),
                    cpu: core_cpu.get(&process.identity).copied().unwrap_or(process.cpu_percent),
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
    }

    pub(super) fn core_process_cpu(&self) -> HashMap<ProcessIdentity, f32> {
        let mut values = HashMap::new();
        if let Some(core) = self.focused_core {
            let identities = self
                .snapshot
                .processes
                .iter()
                .filter_map(|process| {
                    process.identity.stable_key().map(|key| (key, process.identity))
                })
                .collect::<HashMap<_, _>>();
            for thread in self
                .detail
                .iter()
                .flat_map(|detail| detail.threads().into_iter().flatten())
                .filter(|thread| thread.last_cpu == Some(core))
            {
                if let Some(identity) = identities.get(&thread.process) {
                    *values.entry(*identity).or_insert(0.0) += thread.cpu_percent;
                }
            }
        }
        values
    }

    pub(super) fn selected_process(&self) -> Option<&ProcessSample> {
        let key = self.selected?;
        self.snapshot.processes.iter().find(|process| process.identity == key)
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

    pub(super) fn select_index(&mut self, index: usize) {
        if let Some(key) = self.visible.get(index).map(|row| row.key) {
            self.select_identity(key);
        }
    }

    fn select_identity(&mut self, key: ProcessIdentity) {
        self.selected = Some(key);
        self.selected_summary =
            self.snapshot.processes.iter().find(|process| process.identity == key).cloned();
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
                self.inspector_tab = InspectorTab::Profile;
                self.active_region = ActiveRegion::Inspector;
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
        self.inspector_tab = self.inspector_tab.next(delta);
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
        let parent_pid =
            self.snapshot.processes.iter().find(|process| process.identity == key)?.parent_pid?;
        self.snapshot
            .processes
            .iter()
            .find(|process| process.identity.pid() == parent_pid)
            .map(|process| process.identity)
    }

    pub(super) fn on_confirmation_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                self.confirm = None;
                Action::None
            }
            KeyCode::Char('f') => {
                if let Some(confirm) = &mut self.confirm {
                    confirm.action = ProcessAction::ForceTerminate;
                }
                Action::None
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                let confirm = self.confirm.take().expect("confirmation checked above");
                Action::Process(confirm.key, confirm.action)
            }
            _ => Action::None,
        }
    }

    pub(super) fn on_mouse(&mut self, mouse: MouseEvent, regions: &UiRegions) -> Action {
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
            if regions.confirm_yes.is_some_and(|area| contains(area, point)) {
                let confirm = self.confirm.take().expect("confirmation checked above");
                return Action::Process(confirm.key, confirm.action);
            }
            if regions.confirm_force.is_some_and(|area| contains(area, point)) {
                if let Some(confirm) = &mut self.confirm {
                    confirm.action = ProcessAction::ForceTerminate;
                }
                return Action::None;
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
            self.inspector_tab = *tab;
            self.active_region = ActiveRegion::Inspector;
            return Action::None;
        }
        if regions.profile.is_some_and(|area| contains(area, point)) {
            self.inspector_tab = InspectorTab::Profile;
            self.active_region = ActiveRegion::Inspector;
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
            self.confirm =
                Some(Confirmation { key, action: requested, name: process.name.clone() });
        }
    }
}

fn contains(area: Rect, (x, y): (u16, u16)) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}
