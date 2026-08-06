use tokio::task::JoinSet;

use super::{
    AddRequest, App, DoctorReport, ProjectId, ProjectReport, SyncController, SyncedProject,
};

pub(super) enum Operation {
    Refresh { quiet: bool },
    Add(AddRequest),
    TogglePause { selector: String, paused: bool },
    Flush { selector: String },
    Doctor { selector: Option<String> },
    Remove { selector: String },
}

pub(super) enum OperationOutcome {
    Reports {
        reports: Vec<ProjectReport>,
        notice: Option<String>,
        select: Option<ProjectId>,
        quiet: bool,
    },
    Upsert {
        report: ProjectReport,
        notice: String,
        select: bool,
    },
    Removed {
        project: SyncedProject,
        notice: String,
    },
    Doctor(DoctorReport),
    Failed(String),
}

pub(super) enum OperationState {
    Idle,
    Refreshing { pending: Option<Operation> },
    Foreground,
}

impl OperationState {
    pub(super) const fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    pub(super) const fn is_busy(&self) -> bool {
        matches!(self, Self::Foreground | Self::Refreshing { pending: Some(_) })
    }

    pub(super) fn complete(&mut self) -> Option<Operation> {
        match std::mem::replace(self, Self::Idle) {
            Self::Refreshing { pending: Some(operation) } => {
                *self = Self::Foreground;
                Some(operation)
            }
            Self::Idle | Self::Refreshing { pending: None } | Self::Foreground => None,
        }
    }
}

pub(super) fn start_operation(
    app: &mut App,
    controller: SyncController,
    operation: Operation,
    operations: &mut JoinSet<OperationOutcome>,
) {
    let background = matches!(operation, Operation::Refresh { quiet: true });
    match (&mut app.operation, background) {
        (OperationState::Idle, true) => {
            app.operation = OperationState::Refreshing { pending: None };
        }
        (OperationState::Idle, false) => {
            app.operation = OperationState::Foreground;
        }
        (OperationState::Refreshing { pending }, false) if pending.is_none() => {
            *pending = Some(operation);
            return;
        }
        (OperationState::Refreshing { .. } | OperationState::Foreground, _) => return,
    }
    spawn_operation(controller, operation, operations);
}

pub(super) fn spawn_operation(
    controller: SyncController,
    operation: Operation,
    operations: &mut JoinSet<OperationOutcome>,
) {
    operations.spawn(perform_operation(controller, operation));
}

async fn perform_operation(controller: SyncController, operation: Operation) -> OperationOutcome {
    match operation {
        Operation::Refresh { quiet } => match controller.status(None).await {
            Ok(reports) => OperationOutcome::Reports {
                reports,
                notice: (!quiet).then(|| "Refreshed".to_owned()),
                select: None,
                quiet,
            },
            Err(error) => OperationOutcome::Failed(format!("{error:#}")),
        },
        Operation::Add(request) => match controller.add(request).await {
            Ok(report) => OperationOutcome::Upsert {
                report,
                notice: "Synced Project created".to_owned(),
                select: true,
            },
            Err(error) => OperationOutcome::Failed(format!("{error:#}")),
        },
        Operation::TogglePause { selector, paused } => {
            let result = if paused {
                controller.resume(&selector).await
            } else {
                controller.pause(&selector).await
            };
            match result {
                Ok(report) => OperationOutcome::Upsert {
                    report,
                    notice: if paused {
                        "Synchronization resumed"
                    } else {
                        "Synchronization paused"
                    }
                    .to_owned(),
                    select: false,
                },
                Err(error) => OperationOutcome::Failed(format!("{error:#}")),
            }
        }
        Operation::Flush { selector } => match controller.flush(&selector).await {
            Ok(report) => OperationOutcome::Upsert {
                report,
                notice: "Synchronization complete".to_owned(),
                select: false,
            },
            Err(error) => OperationOutcome::Failed(format!("{error:#}")),
        },
        Operation::Doctor { selector } => match controller.doctor(selector.as_deref()).await {
            Ok(report) => OperationOutcome::Doctor(report),
            Err(error) => OperationOutcome::Failed(format!("{error:#}")),
        },
        Operation::Remove { selector } => match controller.remove(&selector).await {
            Ok(project) => OperationOutcome::Removed {
                project,
                notice: "Synced Project removed; files preserved".to_owned(),
            },
            Err(error) => OperationOutcome::Failed(format!("{error:#}")),
        },
    }
}
