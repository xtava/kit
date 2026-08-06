use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context as _, Result};
use serde::Serialize;

use crate::framework::Context;

use super::{
    annotations::DeployAnnotations,
    artifact::ArtifactIdentity,
    config::LoadedPlan,
    journal::JournalStore,
    layout::DeployLayout,
    orchestration,
    runner::{self, OutputStream, RunEvent, RunOutcome},
    state::{App, ProgressStatus},
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeployRunReport {
    operation: &'static str,
    target_id: String,
    version: String,
    source: Option<DeploySourceReport>,
    artifact: Option<ArtifactIdentity>,
    status: &'static str,
    duration_ms: u64,
    steps: Vec<DeployStepReport>,
    journal_recorded: bool,
    journal_path: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeploySourceReport {
    commit: String,
    dirty: bool,
    content_sha256: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeployStepReport {
    name: String,
    status: &'static str,
    duration_ms: u64,
}

pub async fn run(cx: &Context, loaded: LoadedPlan, target_id: &str) -> Result<RunOutcome> {
    let target = loaded
        .plan
        .targets
        .iter()
        .find(|target| target.id == target_id)
        .cloned()
        .ok_or_else(|| anyhow!("deploy Target '{target_id}' does not exist"))?;
    let journal_store = JournalStore::bootstrap()?;
    let journal = journal_store.load()?;
    let spec = orchestration::prepare_production(&loaded, vec![target], &journal_store).await?;
    let mut state =
        App::new(loaded, journal, DeployAnnotations::default(), DeployLayout::default());
    state.begin_run(&spec);

    let (mut events, cancel, handle) = runner::spawn_with_supervisor(cx.processes.clone(), spec);
    let mut interrupt_armed = true;
    let outcome = loop {
        tokio::select! {
            interrupt = tokio::signal::ctrl_c(), if interrupt_armed => {
                interrupt.context("listen for deploy interruption")?;
                interrupt_armed = false;
                cancel.send(true).map_err(|_| anyhow!("deploy runner stopped before cancellation"))?;
            }
            event = events.recv() => {
                let event =
                    event.ok_or_else(|| anyhow!("deploy runner stopped before reporting a result"))?;
                if let RunEvent::Output { stream, line } = &event {
                    match stream {
                        OutputStream::Stdout | OutputStream::Stderr => eprintln!("{line}"),
                    }
                }
                let finished = match &event {
                    RunEvent::Finished { outcome, .. } => Some(*outcome),
                    _ => None,
                };
                state.ingest(event);
                if let Some(outcome) = finished {
                    break outcome;
                }
            }
        }
    };
    handle.await.context("join deploy runner")?;

    let timestamp_secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| anyhow!("system clock is before the Unix epoch"))?
        .as_secs();
    let entries = state.journal_entries(timestamp_secs);
    let journal_recorded = !entries.is_empty();
    if journal_recorded {
        journal_store.record_many(entries)?;
    }

    let target = state
        .progress
        .first()
        .ok_or_else(|| anyhow!("deploy completed without Target progress"))?;
    let report = DeployRunReport {
        operation: "deploy_production",
        target_id: target.id.clone(),
        version: target.version.0.clone(),
        source: target.source.as_ref().map(|source| DeploySourceReport {
            commit: source.commit.clone(),
            dirty: source.dirty,
            content_sha256: source.content_sha256.clone(),
        }),
        artifact: target.artifact.clone(),
        status: run_outcome_label(outcome),
        duration_ms: duration_ms(state.run_elapsed.unwrap_or_default()),
        steps: target
            .steps
            .iter()
            .map(|step| DeployStepReport {
                name: step.name.clone(),
                status: progress_label(step.status),
                duration_ms: duration_ms(step.elapsed.unwrap_or_default()),
            })
            .collect(),
        journal_recorded,
        journal_path: journal_recorded.then(|| journal_store.path().display().to_string()),
    };
    if cx.out.is_json() {
        cx.out.json(&report)?;
    } else {
        println!("Production deploy {}: {} ({})", report.status, report.target_id, report.version);
        if let Some(path) = &report.journal_path {
            println!("Journal: {path}");
        }
    }
    Ok(outcome)
}

fn run_outcome_label(outcome: RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Succeeded => "success",
        RunOutcome::Failed => "failed",
        RunOutcome::Cancelled => "cancelled",
    }
}

fn progress_label(status: ProgressStatus) -> &'static str {
    match status {
        ProgressStatus::Pending => "pending",
        ProgressStatus::Running => "running",
        ProgressStatus::Succeeded => "success",
        ProgressStatus::Failed => "failed",
        ProgressStatus::Cancelled => "cancelled",
        ProgressStatus::Skipped => "skipped",
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}
