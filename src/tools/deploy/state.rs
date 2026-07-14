use std::{collections::VecDeque, time::Duration};

use super::{
    cloudflare::CloudflareDeployment,
    config::{DeployTarget, LoadedPlan},
    journal::{
        DeployJournal, JournalEntry, JournalOperation, JournalStatus, JournalStep,
        JournalStepStatus, VersionId,
    },
    layout::{DeployLayout, LayoutFrame, SplitDrag, SplitSurface},
    runner::{
        OutputStream, RunEvent, RunOperation, RunOutcome, RunSpec, StepOutcome, TargetOutcome,
    },
};

const OUTPUT_LIMIT: usize = 500;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Browse,
    Versions,
    Review,
    Running,
    Summary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveRegion {
    Primary,
    Secondary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
}

#[derive(Clone, Debug)]
pub enum RunIntent {
    Deploy,
    Rollback { target_index: usize, version: VersionId },
    CloudflarePagesRollback { target_index: usize, deployment: CloudflareDeployment },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersionsSource {
    Journal,
    CloudflarePages,
}

#[derive(Clone, Debug)]
pub enum VersionsState {
    Journal,
    CloudflareLoading,
    CloudflareReady { deployments: Vec<CloudflareDeployment> },
    CloudflareError { message: String },
}

#[derive(Clone, Debug)]
pub struct StepProgress {
    pub name: String,
    pub status: ProgressStatus,
    pub elapsed: Option<Duration>,
    pub failure: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TargetProgress {
    pub id: String,
    pub name: String,
    pub version: VersionId,
    pub status: ProgressStatus,
    pub elapsed: Option<Duration>,
    pub steps: Vec<StepProgress>,
}

#[derive(Clone, Debug)]
pub struct OutputLine {
    pub stream: OutputStream,
    pub text: String,
}

#[derive(Debug)]
pub struct App {
    pub loaded: LoadedPlan,
    pub journal: DeployJournal,
    pub layout: DeployLayout,
    pub layout_frame: LayoutFrame,
    pub layout_drag: Option<SplitDrag>,
    pub phase: Phase,
    pub active_region: ActiveRegion,
    pub primary_scroll: u16,
    pub secondary_scroll: u16,
    pub cursor: usize,
    pub history_cursor: usize,
    pub versions: VersionsState,
    pub selected: Vec<bool>,
    pub intent: Option<RunIntent>,
    pub active_operation: Option<RunOperation>,
    pub progress: Vec<TargetProgress>,
    pub output: VecDeque<OutputLine>,
    pub outcome: Option<RunOutcome>,
    pub run_elapsed: Option<Duration>,
    pub spinner: usize,
    pub notice: Option<String>,
}

impl App {
    pub fn new(loaded: LoadedPlan, journal: DeployJournal, layout: DeployLayout) -> Self {
        let selected = vec![false; loaded.plan.targets.len()];
        Self {
            loaded,
            journal,
            layout,
            layout_frame: LayoutFrame::default(),
            layout_drag: None,
            phase: Phase::Browse,
            active_region: ActiveRegion::Primary,
            primary_scroll: 0,
            secondary_scroll: 0,
            cursor: 0,
            history_cursor: 0,
            versions: VersionsState::Journal,
            selected,
            intent: None,
            active_operation: None,
            progress: Vec::new(),
            output: VecDeque::new(),
            outcome: None,
            run_elapsed: None,
            spinner: 0,
            notice: None,
        }
    }

    pub fn focused_target(&self) -> Option<&DeployTarget> {
        self.loaded.plan.targets.get(self.cursor)
    }

    pub fn set_layout_frame(&mut self, frame: LayoutFrame) {
        self.layout_frame = frame;
    }

    pub fn begin_layout_drag(&mut self, column: u16, row: u16) -> bool {
        let Some(surface) = self.layout_frame.surface else {
            return false;
        };
        if !self.layout_frame.contains_separator(column, row) {
            return false;
        }
        self.layout_drag = Some(SplitDrag { surface, start_ratio: self.layout.ratio(surface) });
        true
    }

    pub fn update_layout_drag(&mut self, column: u16) -> bool {
        let Some(drag) = self.layout_drag else {
            return false;
        };
        if self.layout_frame.surface != Some(drag.surface) {
            self.cancel_layout_drag();
            return false;
        }
        let Some(ratio) = self.layout_frame.ratio_for_column(column) else {
            return false;
        };
        let changed = self.layout.ratio(drag.surface) != ratio;
        self.layout.set_ratio(drag.surface, ratio);
        changed
    }

    pub fn finish_layout_drag(&mut self) -> bool {
        let Some(drag) = self.layout_drag.take() else {
            return false;
        };
        self.layout.ratio(drag.surface) != drag.start_ratio
    }

    pub fn cancel_layout_drag(&mut self) -> bool {
        let Some(drag) = self.layout_drag.take() else {
            return false;
        };
        let changed = self.layout.ratio(drag.surface) != drag.start_ratio;
        self.layout.set_ratio(drag.surface, drag.start_ratio);
        changed
    }

    pub fn reset_active_layout(&mut self) -> Option<bool> {
        self.active_split_surface().map(|surface| self.layout.reset(surface))
    }

    pub fn set_active_region(&mut self, region: ActiveRegion) {
        self.active_region = region;
    }

    pub fn scroll_active_region(&mut self, delta: isize, maximum: u16) {
        let scroll = match (self.phase, self.active_region) {
            (Phase::Browse | Phase::Versions, ActiveRegion::Secondary) => {
                &mut self.secondary_scroll
            }
            (Phase::Running, ActiveRegion::Primary) => &mut self.primary_scroll,
            (Phase::Running, ActiveRegion::Secondary) => {
                adjust_scroll(&mut self.secondary_scroll, -delta, maximum);
                return;
            }
            _ => return,
        };
        adjust_scroll(scroll, delta, maximum);
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let Some(last) = self.loaded.plan.targets.len().checked_sub(1) else {
            return;
        };
        self.cursor = (self.cursor as isize + delta).clamp(0, last as isize) as usize;
        self.history_cursor = 0;
        self.secondary_scroll = 0;
    }

    pub fn toggle_focused(&mut self) {
        if let Some(selected) = self.selected.get_mut(self.cursor) {
            *selected = !*selected;
        }
        self.notice = None;
    }

    pub fn toggle_all(&mut self) {
        let select = self.selected.iter().any(|selected| !selected);
        self.selected.fill(select);
        self.notice = None;
    }

    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|selected| **selected).count()
    }

    pub fn selected_step_count(&self) -> usize {
        self.loaded
            .plan
            .targets
            .iter()
            .zip(&self.selected)
            .filter(|(_, selected)| **selected)
            .map(|(target, _)| target.steps.len())
            .sum()
    }

    pub fn selected_targets(&self) -> Vec<DeployTarget> {
        self.loaded
            .plan
            .targets
            .iter()
            .zip(&self.selected)
            .filter(|(_, selected)| **selected)
            .map(|(target, _)| target.clone())
            .collect()
    }

    pub fn review_deploy(&mut self) {
        if self.selected_count() == 0 {
            self.notice = Some("Select at least one Target before continuing.".to_owned());
        } else {
            self.intent = Some(RunIntent::Deploy);
            self.change_phase(Phase::Review);
            self.notice = None;
        }
    }

    pub fn versions_source(&self) -> VersionsSource {
        if self.focused_target().is_some_and(|target| target.backend.is_some()) {
            VersionsSource::CloudflarePages
        } else {
            VersionsSource::Journal
        }
    }

    pub fn open_versions(&mut self) -> VersionsSource {
        self.change_phase(Phase::Versions);
        self.history_cursor = 0;
        self.notice = None;
        let source = self.versions_source();
        self.versions = match source {
            VersionsSource::Journal => VersionsState::Journal,
            VersionsSource::CloudflarePages => VersionsState::CloudflareLoading,
        };
        source
    }

    pub fn set_cloudflare_versions(
        &mut self,
        target_id: String,
        result: Result<Vec<CloudflareDeployment>, String>,
    ) {
        if self.phase != Phase::Versions
            || self.focused_target().is_none_or(|target| target.id != target_id)
        {
            return;
        }
        self.history_cursor = 0;
        self.secondary_scroll = 0;
        self.versions = match result {
            Ok(deployments) => VersionsState::CloudflareReady { deployments },
            Err(message) => VersionsState::CloudflareError { message },
        };
    }

    pub fn history(&self) -> &[JournalEntry] {
        self.focused_target().map(|target| self.journal.entries(&target.id)).unwrap_or_default()
    }

    pub fn selected_history_entry(&self) -> Option<&JournalEntry> {
        let history = self.history();
        history.get(history.len().checked_sub(self.history_cursor + 1)?)
    }

    pub fn selected_cloudflare_deployment(&self) -> Option<&CloudflareDeployment> {
        match &self.versions {
            VersionsState::CloudflareReady { deployments } => deployments.get(self.history_cursor),
            VersionsState::Journal
            | VersionsState::CloudflareLoading
            | VersionsState::CloudflareError { .. } => None,
        }
    }

    pub fn move_history_cursor(&mut self, delta: isize) {
        let count = match &self.versions {
            VersionsState::CloudflareReady { deployments } => deployments.len(),
            VersionsState::Journal => self.history().len(),
            VersionsState::CloudflareLoading | VersionsState::CloudflareError { .. } => 0,
        };
        let Some(last) = count.checked_sub(1) else {
            return;
        };
        self.history_cursor =
            (self.history_cursor as isize + delta).clamp(0, last as isize) as usize;
        self.secondary_scroll = 0;
    }

    pub fn review_rollback(&mut self) {
        if self.versions_source() == VersionsSource::CloudflarePages {
            let Some(deployment) = self.selected_cloudflare_deployment().cloned() else {
                self.notice = Some(match &self.versions {
                    VersionsState::CloudflareLoading => {
                        "Cloudflare Pages Versions are still loading.".to_owned()
                    }
                    VersionsState::CloudflareError { .. } => {
                        "Retry loading Cloudflare Pages Versions before rollback.".to_owned()
                    }
                    _ => "No Cloudflare Pages deployment is available for rollback.".to_owned(),
                });
                return;
            };
            if !deployment.rollback_eligible() {
                self.notice = Some(
                    "Cloudflare Pages can only roll back to a successful production deployment."
                        .to_owned(),
                );
                return;
            }
            self.intent =
                Some(RunIntent::CloudflarePagesRollback { target_index: self.cursor, deployment });
            self.change_phase(Phase::Review);
            self.notice = None;
            return;
        }

        let Some(target) = self.focused_target() else {
            return;
        };
        if target.rollback.is_none() {
            self.notice = Some(format!(
                "Target '{}' has no rollback strategy. Add [targets.rollback] to the deploy config.",
                target.id
            ));
            return;
        }
        let Some(version) = self.selected_history_entry().map(|entry| entry.version.clone()) else {
            self.notice = Some("No recorded Version is available for this Target.".to_owned());
            return;
        };
        let eligible = self.selected_history_entry().is_some_and(|entry| {
            matches!(entry.status, JournalStatus::Success | JournalStatus::RolledBack)
        });
        if !eligible {
            self.notice = Some(
                "Only a successful or rolled-back Version can be selected for rollback.".to_owned(),
            );
            return;
        }

        self.intent = Some(RunIntent::Rollback { target_index: self.cursor, version });
        self.change_phase(Phase::Review);
        self.notice = None;
    }

    pub fn review_targets(&self) -> Vec<DeployTarget> {
        match &self.intent {
            Some(RunIntent::Deploy) => self.selected_targets(),
            Some(RunIntent::Rollback { target_index, .. }) => self
                .loaded
                .plan
                .targets
                .get(*target_index)
                .and_then(|target| {
                    target.rollback_steps().map(|steps| {
                        let mut target = target.clone();
                        target.steps = steps;
                        target
                    })
                })
                .into_iter()
                .collect(),
            Some(RunIntent::CloudflarePagesRollback { target_index, .. }) => {
                self.loaded.plan.targets.get(*target_index).cloned().into_iter().collect()
            }
            None => Vec::new(),
        }
    }

    pub fn back_from_review(&mut self) {
        let phase = match self.intent {
            Some(RunIntent::Rollback { .. } | RunIntent::CloudflarePagesRollback { .. }) => {
                Phase::Versions
            }
            Some(RunIntent::Deploy) | None => Phase::Browse,
        };
        self.change_phase(phase);
        self.notice = None;
    }

    pub fn back_to_browse(&mut self) {
        self.change_phase(Phase::Browse);
        self.notice = None;
    }

    pub fn begin_run(&mut self, spec: &RunSpec) {
        self.progress = spec
            .targets
            .iter()
            .map(|run_target| TargetProgress {
                id: run_target.target.id.clone(),
                name: run_target.target.name.clone(),
                version: run_target.version.clone(),
                status: ProgressStatus::Pending,
                elapsed: None,
                steps: run_target
                    .target
                    .steps
                    .iter()
                    .map(|step| StepProgress {
                        name: step.name.clone(),
                        status: ProgressStatus::Pending,
                        elapsed: None,
                        failure: None,
                    })
                    .collect(),
            })
            .collect();
        self.active_operation = Some(spec.operation.clone());
        self.change_phase(Phase::Running);
        self.output.clear();
        self.outcome = None;
        self.run_elapsed = None;
        self.spinner = 0;
    }

    pub fn begin_cloudflare_rollback(
        &mut self,
        target: &DeployTarget,
        deployment: &CloudflareDeployment,
    ) {
        self.progress = vec![TargetProgress {
            id: target.id.clone(),
            name: target.name.clone(),
            version: VersionId(deployment.short_id.clone()),
            status: ProgressStatus::Pending,
            elapsed: None,
            steps: vec![StepProgress {
                name: "Request platform rollback".to_owned(),
                status: ProgressStatus::Pending,
                elapsed: None,
                failure: None,
            }],
        }];
        self.active_operation =
            Some(RunOperation::CloudflarePagesRollback { deployment_id: deployment.id.clone() });
        self.change_phase(Phase::Running);
        self.output.clear();
        self.outcome = None;
        self.run_elapsed = None;
        self.spinner = 0;
        self.notice = None;
    }

    pub fn ingest(&mut self, event: RunEvent) {
        match event {
            RunEvent::TargetStarted { target } => {
                if let Some(target) = self.progress.get_mut(target) {
                    target.status = ProgressStatus::Running;
                }
            }
            RunEvent::StepStarted { target, step } => {
                if let Some(step) =
                    self.progress.get_mut(target).and_then(|target| target.steps.get_mut(step))
                {
                    step.status = ProgressStatus::Running;
                }
            }
            RunEvent::Output { stream, line } => {
                self.push_output(OutputLine { stream, text: line })
            }
            RunEvent::StepFinished { target, step, outcome, elapsed } => {
                if let Some(step) =
                    self.progress.get_mut(target).and_then(|target| target.steps.get_mut(step))
                {
                    step.elapsed = Some(elapsed);
                    match outcome {
                        StepOutcome::Succeeded => step.status = ProgressStatus::Succeeded,
                        StepOutcome::Failed(failure) => {
                            step.status = ProgressStatus::Failed;
                            step.failure = Some(failure);
                        }
                        StepOutcome::Cancelled => step.status = ProgressStatus::Cancelled,
                    }
                }
            }
            RunEvent::TargetFinished { target, outcome, elapsed } => {
                if let Some(target) = self.progress.get_mut(target) {
                    target.elapsed = Some(elapsed);
                    target.status = match outcome {
                        TargetOutcome::Succeeded => ProgressStatus::Succeeded,
                        TargetOutcome::Failed => ProgressStatus::Failed,
                        TargetOutcome::Cancelled => ProgressStatus::Cancelled,
                    };
                }
            }
            RunEvent::Finished { outcome, elapsed } => {
                self.outcome = Some(outcome);
                self.run_elapsed = Some(elapsed);
                self.mark_unvisited();
                self.change_phase(Phase::Summary);
            }
        }
    }

    pub fn journal_entries(&self, timestamp_secs: u64) -> Vec<(String, JournalEntry)> {
        let Some(operation) = &self.active_operation else {
            return Vec::new();
        };
        if matches!(operation, RunOperation::CloudflarePagesRollback { .. }) {
            return Vec::new();
        }
        let journal_operation = match operation {
            RunOperation::Deploy => JournalOperation::Deploy,
            RunOperation::Rollback { selected_version } => {
                JournalOperation::Rollback { selected_version: selected_version.clone() }
            }
            RunOperation::CloudflarePagesRollback { .. } => return Vec::new(),
        };
        self.progress
            .iter()
            .filter(|target| {
                !matches!(target.status, ProgressStatus::Pending | ProgressStatus::Skipped)
                    && self
                        .loaded
                        .plan
                        .targets
                        .iter()
                        .find(|configured| configured.id == target.id)
                        .is_some_and(|configured| configured.backend.is_none())
            })
            .map(|target| {
                let status = match (operation, target.status) {
                    (RunOperation::Rollback { .. }, ProgressStatus::Succeeded) => {
                        JournalStatus::RolledBack
                    }
                    (_, ProgressStatus::Succeeded) => JournalStatus::Success,
                    (_, ProgressStatus::Cancelled) => JournalStatus::Cancelled,
                    _ => JournalStatus::Failed,
                };
                let steps = target
                    .steps
                    .iter()
                    .filter_map(|step| {
                        let status = match step.status {
                            ProgressStatus::Succeeded => JournalStepStatus::Success,
                            ProgressStatus::Failed => JournalStepStatus::Failed,
                            ProgressStatus::Cancelled => JournalStepStatus::Cancelled,
                            ProgressStatus::Pending
                            | ProgressStatus::Running
                            | ProgressStatus::Skipped => return None,
                        };
                        Some(JournalStep {
                            name: step.name.clone(),
                            status,
                            duration_ms: millis(step.elapsed.unwrap_or_default()),
                        })
                    })
                    .collect();
                (
                    target.id.clone(),
                    JournalEntry {
                        version: target.version.clone(),
                        timestamp_secs,
                        operation: journal_operation.clone(),
                        status,
                        duration_ms: millis(target.elapsed.unwrap_or_default()),
                        steps,
                    },
                )
            })
            .collect()
    }

    fn push_output(&mut self, line: OutputLine) {
        let dropped_oldest = self.output.len() == OUTPUT_LIMIT;
        if dropped_oldest {
            self.output.pop_front();
        }
        self.output.push_back(line);
        if self.phase == Phase::Running && self.secondary_scroll > 0 && !dropped_oldest {
            self.secondary_scroll = self.secondary_scroll.saturating_add(1);
        }
    }

    fn active_split_surface(&self) -> Option<SplitSurface> {
        match self.phase {
            Phase::Browse => Some(SplitSurface::Browse),
            Phase::Versions => Some(SplitSurface::Versions),
            Phase::Running => Some(SplitSurface::Running),
            Phase::Review | Phase::Summary => None,
        }
    }

    fn change_phase(&mut self, phase: Phase) {
        self.cancel_layout_drag();
        self.layout_frame = LayoutFrame::default();
        self.phase = phase;
        self.active_region = ActiveRegion::Primary;
        self.primary_scroll = 0;
        self.secondary_scroll = 0;
    }

    fn mark_unvisited(&mut self) {
        for target in &mut self.progress {
            if target.status == ProgressStatus::Pending {
                target.status = ProgressStatus::Skipped;
            }
            for step in &mut target.steps {
                if step.status == ProgressStatus::Pending {
                    step.status = ProgressStatus::Skipped;
                }
            }
        }
    }
}

fn adjust_scroll(scroll: &mut u16, delta: isize, maximum: u16) {
    *scroll = if delta >= 0 {
        scroll.saturating_add(delta.unsigned_abs().min(u16::MAX as usize) as u16)
    } else {
        scroll.saturating_sub(delta.unsigned_abs().min(u16::MAX as usize) as u16)
    }
    .min(maximum);
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::tools::deploy::{
        cloudflare::{
            CloudflareDeployment, CloudflareEnvironment, CloudflareStage, CloudflareStageStatus,
        },
        config::{DeployAction, DeployStep, DeploymentPlan, RollbackStrategy, TargetBackend},
        journal::{JournalOperation, JournalStatus, TargetJournal},
        runner::RunTargetSpec,
    };

    fn target(id: &str, steps: usize) -> DeployTarget {
        DeployTarget {
            id: id.to_owned(),
            name: id.to_uppercase(),
            description: None,
            working_dir: None,
            env_file: None,
            steps: (0..steps)
                .map(|index| DeployStep {
                    name: format!("Step {index}"),
                    working_dir: None,
                    action: DeployAction::Shell { script: "true".to_owned() },
                })
                .collect(),
            backend: None,
            rollback: Some(RollbackStrategy::Steps {
                steps: vec![DeployStep {
                    name: "Restore".to_owned(),
                    working_dir: None,
                    action: DeployAction::Shell {
                        script: "restore $KIT_DEPLOY_VERSION".to_owned(),
                    },
                }],
            }),
        }
    }

    fn app() -> App {
        App::new(
            LoadedPlan {
                path: PathBuf::from("deploy.toml"),
                base_dir: PathBuf::from("."),
                plan: DeploymentPlan {
                    version: 1,
                    targets: vec![target("one", 2), target("two", 1)],
                },
                environments: Default::default(),
            },
            DeployJournal {
                schema_version: 1,
                targets: vec![TargetJournal {
                    target_id: "one".to_owned(),
                    entries: vec![JournalEntry {
                        version: VersionId("v1".to_owned()),
                        timestamp_secs: 1,
                        operation: JournalOperation::Deploy,
                        status: JournalStatus::Success,
                        duration_ms: 10,
                        steps: Vec::new(),
                    }],
                }],
            },
            DeployLayout::default(),
        )
    }

    fn cloudflare_deployment(environment: CloudflareEnvironment) -> CloudflareDeployment {
        CloudflareDeployment {
            id: "deployment-id-placeholder".to_owned(),
            short_id: "short-id".to_owned(),
            created_on: "2026-01-02T03:04:05Z".to_owned(),
            environment,
            url: "https://placeholder.pages.dev".to_owned(),
            latest_stage: Some(CloudflareStage { status: CloudflareStageStatus::Success }),
            deployment_trigger: None,
        }
    }

    #[test]
    fn selection_preserves_configuration_order() {
        let mut app = app();
        app.move_cursor(1);
        app.toggle_focused();
        app.move_cursor(-1);
        app.toggle_focused();

        let selected = app.selected_targets();
        assert_eq!(
            selected.iter().map(|target| target.id.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!(app.selected_step_count(), 3);
    }

    #[test]
    fn divider_drag_selects_updates_and_cancels_one_surface() {
        let mut app = app();
        let start = app.layout.browse;
        let frame = LayoutFrame::split(
            SplitSurface::Browse,
            ratatui::layout::Rect::new(0, 3, 100, 20),
            start,
        );
        app.set_layout_frame(frame);

        assert!(!app.begin_layout_drag(0, 10));
        assert!(app.begin_layout_drag(frame.separator.x, 10));
        assert!(app.update_layout_drag(70));
        assert_ne!(app.layout.browse, start);
        assert!(app.cancel_layout_drag());
        assert_eq!(app.layout.browse, start);
        assert!(app.layout_drag.is_none());
    }

    #[test]
    fn active_region_scroll_is_local_and_phase_changes_reset_navigation() {
        let mut app = app();
        app.set_active_region(ActiveRegion::Secondary);
        app.scroll_active_region(3, 5);

        assert_eq!(app.active_region, ActiveRegion::Secondary);
        assert_eq!(app.secondary_scroll, 3);
        assert_eq!(app.cursor, 0);

        app.open_versions();

        assert_eq!(app.active_region, ActiveRegion::Primary);
        assert_eq!(app.primary_scroll, 0);
        assert_eq!(app.secondary_scroll, 0);
    }

    #[test]
    fn rollback_selection_uses_focused_targets_recorded_version() {
        let mut app = app();
        app.open_versions();
        app.review_rollback();

        assert_eq!(app.phase, Phase::Review);
        assert!(matches!(
            app.intent,
            Some(RunIntent::Rollback { target_index: 0, ref version }) if version.0 == "v1"
        ));
        assert_eq!(app.review_targets()[0].steps[0].name, "Restore");
    }

    #[test]
    fn versions_source_selects_cloudflare_or_journal_by_target_backend() {
        let mut app = app();
        app.loaded.plan.targets[0].backend = Some(TargetBackend::CloudflarePages {
            account_id: "<account-id>".to_owned(),
            project: "<pages-project>".to_owned(),
            token_env: "CLOUDFLARE_API_TOKEN".to_owned(),
        });

        assert_eq!(app.versions_source(), VersionsSource::CloudflarePages);
        assert_eq!(app.open_versions(), VersionsSource::CloudflarePages);
        assert!(matches!(app.versions, VersionsState::CloudflareLoading));

        app.back_to_browse();
        app.move_cursor(1);
        assert_eq!(app.versions_source(), VersionsSource::Journal);
        assert_eq!(app.open_versions(), VersionsSource::Journal);
        assert!(matches!(app.versions, VersionsState::Journal));
    }

    #[test]
    fn cloudflare_rollback_selection_requires_successful_production_deployment() {
        let mut app = app();
        app.loaded.plan.targets[0].backend = Some(TargetBackend::CloudflarePages {
            account_id: "<account-id>".to_owned(),
            project: "<pages-project>".to_owned(),
            token_env: "CLOUDFLARE_API_TOKEN".to_owned(),
        });
        app.loaded.plan.targets[0].rollback = None;
        app.open_versions();
        app.set_cloudflare_versions(
            "one".to_owned(),
            Ok(vec![cloudflare_deployment(CloudflareEnvironment::Preview)]),
        );
        app.review_rollback();
        assert_eq!(app.phase, Phase::Versions);
        assert!(app.notice.as_deref().is_some_and(|notice| notice.contains("production")));

        app.set_cloudflare_versions(
            "one".to_owned(),
            Ok(vec![cloudflare_deployment(CloudflareEnvironment::Production)]),
        );
        app.review_rollback();
        assert_eq!(app.phase, Phase::Review);
        assert!(matches!(
            app.intent,
            Some(RunIntent::CloudflarePagesRollback { ref deployment, .. })
                if deployment.id == "deployment-id-placeholder"
        ));
    }

    #[test]
    fn review_requires_a_selection() {
        let mut app = app();
        app.review_deploy();
        assert_eq!(app.phase, Phase::Browse);
        assert!(app.notice.as_deref().is_some_and(|notice| notice.contains("at least one")));

        app.toggle_focused();
        app.review_deploy();
        assert_eq!(app.phase, Phase::Review);
    }

    #[test]
    fn failed_run_marks_later_work_skipped_and_builds_journal_entry() {
        let mut app = app();
        let spec = RunSpec {
            base_dir: PathBuf::from("."),
            operation: RunOperation::Deploy,
            targets: vec![RunTargetSpec {
                target: target("one", 2),
                version: VersionId("v2".to_owned()),
                environment: Default::default(),
            }],
        };
        app.begin_run(&spec);
        app.ingest(RunEvent::TargetStarted { target: 0 });
        app.ingest(RunEvent::StepStarted { target: 0, step: 0 });
        app.ingest(RunEvent::StepFinished {
            target: 0,
            step: 0,
            outcome: StepOutcome::Failed("boom".to_owned()),
            elapsed: Duration::from_millis(20),
        });
        app.ingest(RunEvent::TargetFinished {
            target: 0,
            outcome: TargetOutcome::Failed,
            elapsed: Duration::from_millis(25),
        });
        app.ingest(RunEvent::Finished {
            outcome: RunOutcome::Failed,
            elapsed: Duration::from_millis(25),
        });

        assert_eq!(app.phase, Phase::Summary);
        assert_eq!(app.progress[0].steps[0].status, ProgressStatus::Failed);
        assert_eq!(app.progress[0].steps[1].status, ProgressStatus::Skipped);
        let entries = app.journal_entries(7);
        assert_eq!(entries[0].1.status, JournalStatus::Failed);
        assert_eq!(entries[0].1.steps.len(), 1);
    }
}
