use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

use super::actions::{ActionRequest, ActionResult};
use super::contributions::{
    StatsActionContext, StatsActionRegistry, StatsCommand, PROCESS_CONTEXT_MENU,
};
use super::history::{HistorySeries, HistoryStore};
use super::host::ProcessAction;
use super::model::{
    DetailData, DetailRequest, DetailRequestKind, DetailSnapshot, ProcessIdentity, ProcessKey,
    ProcessSample, StatsSnapshot, ThreadSample,
};
use super::render::UiRegions;
use super::tree::{FamilyView, ProcessForest, TreeQuery, TreeSort};
use crate::tui::{
    ActionInvocation, ContextMenu, ContextMenuOutcome, Direction, KeyChord, LineEditor, SplitDrag,
    SplitRatio,
};

const HISTORY: usize = 120;
const RECENT_CPU_SAMPLES: usize = 3;
const DEFAULT_SPLIT_RATIO: SplitRatio = SplitRatio::new(640);

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
            Self::Cpu => "RECENT CPU",
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

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ProcessPressure {
    pub(super) identity: ProcessIdentity,
    pub(super) name: String,
    pub(super) pid: u32,
    pub(super) now_percent: f32,
    pub(super) recent_percent: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CorePressure {
    pub(super) logical_index: u16,
    pub(super) now_percent: f32,
    pub(super) recent_peak_percent: f32,
}

pub(super) struct Confirmation {
    pub(super) key: ProcessKey,
    pub(super) requested: ProcessAction,
    pub(super) name: String,
    pub(super) choices: Vec<ConfirmationChoice>,
    pub(super) choice: ConfirmationChoice,
}

impl Confirmation {
    fn move_choice(&mut self, delta: isize) {
        if self.choices.is_empty() {
            return;
        }
        let index = self
            .choices
            .iter()
            .position(|choice| *choice == self.choice)
            .unwrap_or(self.choices.len() - 1);
        self.choice =
            self.choices[(index as isize + delta).rem_euclid(self.choices.len() as isize) as usize];
    }

    fn select_action(&mut self, action: ProcessAction) {
        let choice = ConfirmationChoice::Action(action);
        if self.choices.contains(&choice) {
            self.choice = choice;
        }
    }
}

pub(super) struct CommandViewer {
    pub(super) name: String,
    pub(super) pid: u32,
    pub(super) command: String,
    pub(super) row_offset: usize,
    pub(super) column_offset: usize,
}

pub(super) enum StatsOverlay {
    ContextMenu(ContextMenu<StatsActionContext>),
    Confirmation(Confirmation),
    CommandViewer(CommandViewer),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ConfirmationChoice {
    Action(ProcessAction),
    Cancel,
}

impl ConfirmationChoice {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Action(ProcessAction::GracefulTerminate) => "End process",
            Self::Action(ProcessAction::ForceTerminate) => "Force terminate",
            Self::Cancel => "Cancel",
        }
    }
}

pub(super) struct StatsApp {
    pub(super) snapshot: Arc<StatsSnapshot>,
    pub(super) registry: StatsActionRegistry,
    pub(super) pointer_enabled: bool,
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
    pub(super) split_ratio: SplitRatio,
    pub(super) split_drag: Option<SplitDrag<()>>,
    pub(super) active_region: ActiveRegion,
    pub(super) filter: LineEditor,
    pub(super) filtering: bool,
    pub(super) visible: Vec<VisibleRow>,
    family_view: Option<FamilyView>,
    pub(super) row_offset: usize,
    pub(super) viewport_rows: usize,
    pub(super) histories: Vec<VecDeque<u64>>,
    process_history: HistoryStore,
    pub(super) overlay: Option<StatsOverlay>,
    pub(super) action_lifecycle: ActionLifecycle,
    pub(super) status: Option<String>,
}

impl StatsApp {
    #[cfg(test)]
    pub(super) fn new(snapshot: Arc<StatsSnapshot>) -> Self {
        Self::new_validated(
            snapshot,
            super::contributions::registry().expect("Stats test contributions must validate"),
            true,
        )
    }

    pub(super) fn new_validated(
        snapshot: Arc<StatsSnapshot>,
        registry: StatsActionRegistry,
        pointer_enabled: bool,
    ) -> Self {
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
            registry,
            pointer_enabled,
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
            split_ratio: DEFAULT_SPLIT_RATIO,
            split_drag: None,
            active_region: ActiveRegion::Processes,
            filter: LineEditor::default(),
            filtering: false,
            visible: Vec::new(),
            family_view: None,
            row_offset: 0,
            viewport_rows: 0,
            histories: Vec::new(),
            process_history: HistoryStore::default(),
            overlay: None,
            action_lifecycle: ActionLifecycle::Idle,
            status: None,
        };
        app.record_history();
        app.reproject();
        app
    }

    pub(super) fn ingest(&mut self, snapshot: Arc<StatsSnapshot>) {
        let viewport_anchor = if self.row_offset > 0 {
            self.visible.get(self.row_offset).map(|row| row.key)
        } else {
            None
        };
        let previous_live = self.selected_process().cloned();
        self.forest = ProcessForest::new(&snapshot.processes);
        self.snapshot = snapshot;
        if let Some(live) = self.selected_process().cloned() {
            self.selected_summary = Some(live);
        } else if previous_live.is_some() {
            self.selected_summary = previous_live;
        }
        let unavailable_overlay_target = match self.overlay.as_ref() {
            Some(StatsOverlay::ContextMenu(menu)) => Some(menu.context().identity),
            Some(StatsOverlay::Confirmation(confirm)) => Some(ProcessIdentity::stable(confirm.key)),
            Some(StatsOverlay::CommandViewer(_)) | None => None,
        }
        .filter(|identity| self.process(*identity).is_none());
        if let Some(identity) = unavailable_overlay_target {
            self.overlay = None;
            self.status = Some(format!(
                "Target unavailable: PID {} is no longer the same process",
                identity.pid()
            ));
        }
        self.record_history();
        self.reproject();
        if let Some(anchor) = viewport_anchor {
            if let Some(index) = self.visible.iter().position(|row| row.key == anchor) {
                self.row_offset = index;
            }
        }
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
        let ordering_processes =
            (self.sort == SortBy::Cpu && self.focused_core.is_none()).then(|| {
                self.snapshot
                    .processes
                    .iter()
                    .cloned()
                    .map(|mut process| {
                        process.cpu_percent = self.recent_cpu_score(&process);
                        process
                    })
                    .collect::<Vec<_>>()
            });
        let projection = self.forest.project(
            ordering_processes.as_deref().unwrap_or(&self.snapshot.processes),
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

    pub(super) fn pressure_sources(&self, limit: usize) -> Vec<ProcessPressure> {
        let mut sources = self
            .snapshot
            .processes
            .iter()
            .map(|process| ProcessPressure {
                identity: process.identity,
                name: process.name.clone(),
                pid: process.identity.pid(),
                now_percent: process.cpu_percent,
                recent_percent: self.recent_cpu_score(process),
            })
            .collect::<Vec<_>>();
        sources.sort_by(|left, right| {
            right
                .recent_percent
                .total_cmp(&left.recent_percent)
                .then_with(|| right.now_percent.total_cmp(&left.now_percent))
                .then_with(|| left.identity.cmp(&right.identity))
        });
        sources.truncate(limit);
        sources
    }

    pub(super) fn core_pressure(&self, limit: usize) -> Vec<CorePressure> {
        let mut cores = self
            .snapshot
            .system
            .cpus
            .iter()
            .enumerate()
            .map(|(index, cpu)| {
                let recent_peak_percent = self
                    .histories
                    .get(index)
                    .into_iter()
                    .flat_map(|history| history.iter().rev().take(RECENT_CPU_SAMPLES))
                    .copied()
                    .max()
                    .map(|peak| peak as f32)
                    .unwrap_or(cpu.usage_percent);
                CorePressure {
                    logical_index: cpu.logical_index,
                    now_percent: cpu.usage_percent,
                    recent_peak_percent,
                }
            })
            .collect::<Vec<_>>();
        cores.sort_by(|left, right| {
            right
                .recent_peak_percent
                .total_cmp(&left.recent_peak_percent)
                .then_with(|| right.now_percent.total_cmp(&left.now_percent))
                .then_with(|| left.logical_index.cmp(&right.logical_index))
        });
        cores.truncate(limit);
        cores
    }

    fn recent_cpu_score(&self, process: &ProcessSample) -> f32 {
        process
            .identity
            .stable_key()
            .and_then(|key| self.process_history.recent_cpu_average(key, RECENT_CPU_SAMPLES))
            .unwrap_or(process.cpu_percent)
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
        self.row_offset = 0;
        self.reproject();
    }

    fn jump_to_top(&mut self) {
        self.row_offset = 0;
        self.select_index(0);
    }

    pub(super) fn on_event(&mut self, event: Event, regions: &UiRegions) -> Action {
        if matches!(&event, Event::Key(key) if is_ctrl_c(*key)) {
            return Action::Quit;
        }
        if self.overlay.is_some() {
            return self.on_overlay_event(event, regions);
        }
        match event {
            Event::Key(key) => self.on_key(key, regions),
            Event::Mouse(mouse) => self.on_mouse(mouse, regions),
            Event::Resize(_, _) => {
                self.split_drag = None;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn on_overlay_event(&mut self, event: Event, regions: &UiRegions) -> Action {
        let handler: fn(&mut Self, Event, &UiRegions) -> Action = match self.overlay.as_ref() {
            Some(StatsOverlay::ContextMenu(_)) => Self::on_context_menu_event,
            Some(StatsOverlay::Confirmation(_)) => Self::on_confirmation_event,
            Some(StatsOverlay::CommandViewer(_)) => Self::on_command_viewer_event,
            None => unreachable!("overlay state checked before dispatch"),
        };
        handler(self, event, regions)
    }

    fn on_context_menu_event(&mut self, event: Event, regions: &UiRegions) -> Action {
        self.split_drag = None;
        let Some(layout) = regions.context_menu.as_ref() else {
            return Action::None;
        };
        let outcome = match self.overlay.as_mut() {
            Some(StatsOverlay::ContextMenu(menu)) => menu.on_event(event, layout),
            _ => unreachable!("context-menu handler dispatched for another overlay"),
        };
        match outcome {
            ContextMenuOutcome::Captured => Action::None,
            ContextMenuOutcome::Dismissed => {
                self.overlay = None;
                Action::None
            }
            ContextMenuOutcome::Unavailable { reason, .. } => {
                self.status = Some(format!("Action unavailable: {reason}"));
                Action::None
            }
            ContextMenuOutcome::Invoke(invocation) => {
                self.overlay = None;
                self.invoke_action(invocation)
            }
        }
    }

    fn on_confirmation_event(&mut self, event: Event, regions: &UiRegions) -> Action {
        match event {
            Event::Key(key) => self.on_confirmation_key(key),
            Event::Mouse(mouse) => {
                self.split_drag = None;
                if !self.pointer_enabled || mouse.kind != MouseEventKind::Down(MouseButton::Left) {
                    return Action::None;
                }
                let point = (mouse.column, mouse.row);
                if let Some((_, choice)) =
                    regions.confirmation_choices.iter().find(|(area, _)| contains(*area, point))
                {
                    return self.activate_confirmation(*choice);
                }
                Action::None
            }
            Event::Resize(_, _) => {
                self.split_drag = None;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn on_command_viewer_event(&mut self, event: Event, regions: &UiRegions) -> Action {
        match event {
            Event::Key(key) => self.on_command_viewer_key(key, regions),
            Event::Mouse(mouse) => {
                self.split_drag = None;
                let point = (mouse.column, mouse.row);
                if self.pointer_enabled
                    && mouse.kind == MouseEventKind::Down(MouseButton::Left)
                    && regions.command_close.is_some_and(|area| contains(area, point))
                {
                    self.overlay = None;
                }
                Action::None
            }
            Event::Resize(_, _) => {
                self.split_drag = None;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn on_key(&mut self, key: KeyEvent, regions: &UiRegions) -> Action {
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
        if let (Some(chord), Some(identity)) = (KeyChord::from_event(key), self.selected) {
            let context = self.action_context(identity);
            if let Some(invocation) = self.registry.resolve_keybinding(chord, context) {
                return self.invoke_action(invocation);
            }
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
            KeyCode::Home => {
                if self.active_region == ActiveRegion::Processes {
                    self.jump_to_top();
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
            KeyCode::Char('<') if regions.split.is_some() => {
                self.split_ratio = self.split_ratio.adjusted(-50);
                Action::None
            }
            KeyCode::Char('>') if regions.split.is_some() => {
                self.split_ratio = self.split_ratio.adjusted(50);
                Action::None
            }
            KeyCode::Char('=') if regions.split.is_some() => {
                self.split_ratio = DEFAULT_SPLIT_RATIO;
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
                self.overlay = None;
                Action::None
            }
            KeyCode::Char('f') => {
                if let Some(StatsOverlay::Confirmation(confirm)) = self.overlay.as_mut() {
                    confirm.select_action(ProcessAction::ForceTerminate);
                }
                Action::None
            }
            KeyCode::Left | KeyCode::BackTab => {
                if let Some(StatsOverlay::Confirmation(confirm)) = self.overlay.as_mut() {
                    confirm.move_choice(-1);
                }
                Action::None
            }
            KeyCode::Right | KeyCode::Tab => {
                if let Some(StatsOverlay::Confirmation(confirm)) = self.overlay.as_mut() {
                    confirm.move_choice(1);
                }
                Action::None
            }
            KeyCode::Char('y') => {
                let Some(StatsOverlay::Confirmation(confirm)) = self.overlay.as_ref() else {
                    return Action::None;
                };
                self.activate_confirmation(ConfirmationChoice::Action(confirm.requested))
            }
            KeyCode::Enter => {
                let Some(StatsOverlay::Confirmation(confirm)) = self.overlay.as_ref() else {
                    return Action::None;
                };
                let choice = confirm.choice;
                self.activate_confirmation(choice)
            }
            _ => Action::None,
        }
    }

    fn activate_confirmation(&mut self, choice: ConfirmationChoice) -> Action {
        if choice == ConfirmationChoice::Cancel {
            self.overlay = None;
            return Action::None;
        }
        let Some(StatsOverlay::Confirmation(confirm)) = self.overlay.take() else {
            return Action::None;
        };
        if !confirm.choices.contains(&choice) {
            self.status =
                Some("Action unavailable: confirmation choice is no longer offered".into());
            return Action::None;
        }
        let ConfirmationChoice::Action(action) = choice else {
            unreachable!("cancel handled before taking confirmation")
        };
        if self.process(ProcessIdentity::stable(confirm.key)).is_none() {
            self.status = Some(format!(
                "Target unavailable: PID {} is no longer the same process",
                confirm.key.pid
            ));
            return Action::None;
        }
        if let ActionLifecycle::Running(request) = self.action_lifecycle {
            self.status = Some(format!("Action already running for PID {}", request.key.pid));
            return Action::None;
        }
        let capability = process_action_capability(self.snapshot.host, action);
        if let Some(reason) = capability.reason() {
            self.status = Some(format!("Action unavailable: {reason}"));
            return Action::None;
        }
        Action::Process(confirm.key, action)
    }

    fn on_command_viewer_key(&mut self, key: KeyEvent, regions: &UiRegions) -> Action {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.overlay = None;
            return Action::None;
        }
        let Some(StatsOverlay::CommandViewer(viewer)) = self.overlay.as_mut() else {
            return Action::None;
        };
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

    fn show_command(&mut self, identity: ProcessIdentity) {
        let Some(process) = self.process(identity) else {
            self.target_unavailable(identity);
            return;
        };
        let viewer = CommandViewer {
            name: process.name.clone(),
            pid: process.identity.pid(),
            command: process.command.clone(),
            row_offset: 0,
            column_offset: 0,
        };
        self.overlay = Some(StatsOverlay::CommandViewer(viewer));
    }

    fn on_mouse(&mut self, mouse: MouseEvent, regions: &UiRegions) -> Action {
        let point = (mouse.column, mouse.row);
        if !self.pointer_enabled {
            self.split_drag = None;
            return Action::None;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Right) => {
                self.split_drag = None;
                if let Some(row) = regions.rows.iter().find(|row| contains(row.area, point)) {
                    self.active_region = ActiveRegion::Processes;
                    self.select_identity(row.identity);
                    self.open_context_menu(
                        row.identity,
                        Position { x: mouse.column, y: mouse.row },
                    );
                } else if regions.inspector.is_some_and(|area| contains(area, point)) {
                    self.active_region = ActiveRegion::Inspector;
                    if let Some((process, _)) = self.selected_inspection() {
                        let identity = process.identity;
                        self.open_context_menu(
                            identity,
                            Position { x: mouse.column, y: mouse.row },
                        );
                    }
                }
                return Action::None;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.split_drag = regions.split.and_then(|split| {
                    SplitDrag::begin((), split, self.split_ratio, mouse.column, mouse.row)
                });
                if self.split_drag.is_some() {
                    return Action::None;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.split_drag.is_some() => {
                if let Some(ratio) = self.split_drag.and_then(|drag| {
                    regions.split.and_then(|split| drag.ratio_for_column((), split, mouse.column))
                }) {
                    self.split_ratio = ratio;
                }
                return Action::None;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.split_drag = None;
                return Action::None;
            }
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
            _ => return Action::None,
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
        if let Some(action) =
            regions.inline_actions.iter().find(|action| contains(action.area, point))
        {
            let context = self.action_context(action.identity);
            return self.invoke_action(ActionInvocation::new(action.action, context));
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
        if let Some(row) = regions.rows.iter().find(|row| contains(row.area, point)) {
            self.select_identity(row.identity);
            return Action::None;
        }
        Action::None
    }

    fn open_context_menu(&mut self, identity: ProcessIdentity, anchor: Position) {
        let context = self.action_context(identity);
        let items = self.registry.resolve_menu(PROCESS_CONTEXT_MENU, &context);
        self.overlay = ContextMenu::open(anchor, context, items).map(StatsOverlay::ContextMenu);
    }

    pub(super) fn action_context(&self, identity: ProcessIdentity) -> StatsActionContext {
        StatsActionContext {
            identity,
            is_live: self.process(identity).is_some(),
            inspector_tab: self.inspector_tab,
            host: self.snapshot.host,
            action_running: matches!(self.action_lifecycle, ActionLifecycle::Running(_)),
        }
    }

    pub(super) fn invoke_action(
        &mut self,
        invocation: ActionInvocation<StatsActionContext>,
    ) -> Action {
        let command = match self.registry.command_for(&invocation) {
            Ok(command) => command,
            Err(error) => {
                self.status = Some(error.to_string());
                return Action::None;
            }
        };
        if matches!(self.overlay.as_ref(), Some(StatsOverlay::ContextMenu(_))) {
            self.overlay = None;
        }
        let identity = invocation.context.identity;
        match command {
            StatsCommand::ViewCommand => self.show_command(identity),
            StatsCommand::OpenProfile => self.show_profile(identity),
            StatsCommand::RequestTerminate(requested) => {
                self.request_confirmation(identity, requested)
            }
        }
        Action::None
    }

    fn show_profile(&mut self, identity: ProcessIdentity) {
        if self.selected != Some(identity) {
            self.select_identity(identity);
        }
        self.set_inspector_tab(InspectorTab::Profile);
        self.active_region = ActiveRegion::Inspector;
        self.overlay = None;
    }

    pub(super) fn request_confirmation(
        &mut self,
        identity: ProcessIdentity,
        requested: ProcessAction,
    ) {
        if let ActionLifecycle::Running(request) = self.action_lifecycle {
            self.status = Some(format!("Action already running for PID {}", request.key.pid));
            return;
        }
        let capability = process_action_capability(self.snapshot.host, requested);
        if let Some(reason) = capability.reason() {
            self.status = Some(format!("Action unavailable: {reason}"));
            return;
        }
        let Some(process) = self.process(identity) else {
            self.target_unavailable(identity);
            return;
        };
        let Some(key) = process.identity.stable_key() else {
            self.status = Some("Action unavailable: process identity is snapshot-only".into());
            return;
        };
        let confirmation = Confirmation {
            key,
            requested,
            name: process.name.clone(),
            choices: confirmation_choices(requested, self.snapshot.host),
            choice: ConfirmationChoice::Cancel,
        };
        self.overlay = Some(StatsOverlay::Confirmation(confirmation));
    }

    fn target_unavailable(&mut self, identity: ProcessIdentity) {
        self.overlay = None;
        self.status = Some(format!(
            "Target unavailable: PID {} is no longer the same process",
            identity.pid()
        ));
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

fn confirmation_choices(
    requested: ProcessAction,
    host: super::model::HostCapabilities,
) -> Vec<ConfirmationChoice> {
    let mut choices = Vec::with_capacity(3);
    if process_action_capability(host, requested).reason().is_none() {
        choices.push(ConfirmationChoice::Action(requested));
    }
    if requested == ProcessAction::GracefulTerminate && host.force_terminate.reason().is_none() {
        choices.push(ConfirmationChoice::Action(ProcessAction::ForceTerminate));
    }
    choices.push(ConfirmationChoice::Cancel);
    choices
}

fn process_action_capability(
    host: super::model::HostCapabilities,
    action: ProcessAction,
) -> super::model::CapabilityState {
    match action {
        ProcessAction::GracefulTerminate => host.graceful_terminate,
        ProcessAction::ForceTerminate => host.force_terminate,
    }
}

fn is_ctrl_c(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
}

fn contains(area: Rect, (x, y): (u16, u16)) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}
