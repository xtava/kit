use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    num::NonZeroUsize,
    path::PathBuf,
    time::Duration,
};

use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time,
};

use crate::framework::process::{
    CommandSpec, CompletionCause, ContainmentRequirement, EnvironmentBase, InputPolicy, LeaderExit,
    LeaderExitObservation, OutputPolicy, ProcessByteEvent, ProcessControl, ProcessEnvironment,
    ProcessLabel, ProcessOutputHandle, ProcessReport, ProcessSpec, ProcessSupervisor,
    StartedProcess, StreamPolicy, TerminationPolicy,
};

use super::config::{DeployAction, DeployTarget};
use super::environment::TargetEnvironment;
use super::journal::VersionId;

const CANCEL_GRACE: Duration = Duration::from_secs(2);
const OUTPUT_IN_FLIGHT_BYTES: NonZeroUsize = NonZeroUsize::new(256 * 1024).unwrap();
const LINE_FRAGMENT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug)]
pub enum RunEvent {
    TargetStarted { target: usize },
    StepStarted { target: usize, step: usize },
    Output { stream: OutputStream, line: String },
    StepFinished { target: usize, step: usize, outcome: StepOutcome, elapsed: Duration },
    TargetFinished { target: usize, outcome: TargetOutcome, elapsed: Duration },
    Finished { outcome: RunOutcome, elapsed: Duration },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StepOutcome {
    Succeeded,
    Failed(String),
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct RunSpec {
    pub base_dir: PathBuf,
    pub operation: RunOperation,
    pub targets: Vec<RunTargetSpec>,
}

#[derive(Clone, Debug)]
pub struct RunTargetSpec {
    pub target: DeployTarget,
    pub version: VersionId,
    pub branch: Option<String>,
    pub environment: TargetEnvironment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunOperation {
    Deploy,
    Rollback { selected_version: VersionId },
    CloudflarePagesRollback { deployment_id: String },
}

struct StepExecution<'a> {
    processes: &'a ProcessSupervisor,
    action: &'a DeployAction,
    working_dir: &'a std::path::Path,
    version: &'a VersionId,
    branch: Option<&'a str>,
    environment: &'a TargetEnvironment,
    event_tx: &'a mpsc::Sender<RunEvent>,
    cancel_rx: &'a mut watch::Receiver<bool>,
}

pub fn spawn_with_supervisor(
    processes: ProcessSupervisor,
    spec: RunSpec,
) -> (mpsc::Receiver<RunEvent>, watch::Sender<bool>, JoinHandle<()>) {
    let (event_tx, event_rx) = mpsc::channel(256);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let handle = tokio::spawn(run(processes, spec, event_tx, cancel_rx));
    (event_rx, cancel_tx, handle)
}

#[cfg(test)]
fn spawn(spec: RunSpec) -> (mpsc::Receiver<RunEvent>, watch::Sender<bool>, JoinHandle<()>) {
    spawn_with_supervisor(
        ProcessSupervisor::bootstrap().expect("bootstrap process supervisor for deploy test"),
        spec,
    )
}

async fn run(
    processes: ProcessSupervisor,
    spec: RunSpec,
    event_tx: mpsc::Sender<RunEvent>,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let run_start = time::Instant::now();
    let mut final_outcome = RunOutcome::Succeeded;

    'targets: for (target_index, run_target) in spec.targets.iter().enumerate() {
        let target = &run_target.target;
        let target_start = time::Instant::now();
        if send(&event_tx, RunEvent::TargetStarted { target: target_index }).await.is_err() {
            return;
        }
        if *cancel_rx.borrow() {
            final_outcome = RunOutcome::Cancelled;
            let _ = send(
                &event_tx,
                RunEvent::TargetFinished {
                    target: target_index,
                    outcome: TargetOutcome::Cancelled,
                    elapsed: target_start.elapsed(),
                },
            )
            .await;
            break;
        }

        for (step_index, step) in target.steps.iter().enumerate() {
            if *cancel_rx.borrow() {
                final_outcome = RunOutcome::Cancelled;
                let _ = send(
                    &event_tx,
                    RunEvent::TargetFinished {
                        target: target_index,
                        outcome: TargetOutcome::Cancelled,
                        elapsed: target_start.elapsed(),
                    },
                )
                .await;
                break 'targets;
            }

            if send(&event_tx, RunEvent::StepStarted { target: target_index, step: step_index })
                .await
                .is_err()
            {
                return;
            }

            let step_start = time::Instant::now();
            let working_dir = resolve_working_dir(&spec.base_dir, target, step_index);
            let outcome = execute(StepExecution {
                processes: &processes,
                action: &step.action,
                working_dir: &working_dir,
                version: &run_target.version,
                branch: run_target.branch.as_deref(),
                environment: &run_target.environment,
                event_tx: &event_tx,
                cancel_rx: &mut cancel_rx,
            })
            .await;
            let elapsed = step_start.elapsed();
            let terminal_outcome = outcome.clone();
            if send(
                &event_tx,
                RunEvent::StepFinished { target: target_index, step: step_index, outcome, elapsed },
            )
            .await
            .is_err()
            {
                return;
            }

            match terminal_outcome {
                StepOutcome::Succeeded => {}
                StepOutcome::Failed(_) => {
                    final_outcome = RunOutcome::Failed;
                    let _ = send(
                        &event_tx,
                        RunEvent::TargetFinished {
                            target: target_index,
                            outcome: TargetOutcome::Failed,
                            elapsed: target_start.elapsed(),
                        },
                    )
                    .await;
                    break 'targets;
                }
                StepOutcome::Cancelled => {
                    final_outcome = RunOutcome::Cancelled;
                    let _ = send(
                        &event_tx,
                        RunEvent::TargetFinished {
                            target: target_index,
                            outcome: TargetOutcome::Cancelled,
                            elapsed: target_start.elapsed(),
                        },
                    )
                    .await;
                    break 'targets;
                }
            }
        }

        if send(
            &event_tx,
            RunEvent::TargetFinished {
                target: target_index,
                outcome: TargetOutcome::Succeeded,
                elapsed: target_start.elapsed(),
            },
        )
        .await
        .is_err()
        {
            return;
        }
    }

    let _ = send(
        &event_tx,
        RunEvent::Finished { outcome: final_outcome, elapsed: run_start.elapsed() },
    )
    .await;
}

async fn send(
    sender: &mpsc::Sender<RunEvent>,
    event: RunEvent,
) -> Result<(), mpsc::error::SendError<RunEvent>> {
    sender.send(event).await
}

fn resolve_working_dir(
    base_dir: &std::path::Path,
    target: &DeployTarget,
    step_index: usize,
) -> PathBuf {
    let configured = target
        .steps
        .get(step_index)
        .and_then(|step| step.working_dir.as_ref())
        .or(target.working_dir.as_ref());
    match configured {
        Some(path) if path.is_absolute() => path.clone(),
        Some(path) => base_dir.join(path),
        None => base_dir.to_path_buf(),
    }
}

async fn execute(execution: StepExecution<'_>) -> StepOutcome {
    let StepExecution {
        processes,
        action,
        working_dir,
        version,
        branch,
        environment,
        event_tx,
        cancel_rx,
    } = execution;
    if !working_dir.is_dir() {
        return StepOutcome::Failed(format!(
            "working directory does not exist: {}",
            working_dir.display()
        ));
    }

    let working_dir = match working_dir.canonicalize() {
        Ok(working_dir) => working_dir,
        Err(error) => {
            return StepOutcome::Failed(format!(
                "resolve working directory {}: {error}",
                working_dir.display()
            ));
        }
    };
    let (program, arguments) = command_for(action, version, branch);
    let mut values = environment
        .child_values()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect::<BTreeMap<_, _>>();
    values.insert(OsString::from("KIT_DEPLOY_VERSION"), OsString::from(&version.0));
    values.insert(OsString::from("KIT_DEPLOY_REF"), OsString::from(&version.0));
    if let Some(branch) = branch {
        values.insert(OsString::from("KIT_DEPLOY_BRANCH"), OsString::from(branch));
    }
    let process_environment =
        match ProcessEnvironment::new(EnvironmentBase::Inherit, values, BTreeSet::new()) {
            Ok(environment) => environment,
            Err(error) => {
                return StepOutcome::Failed(format!("prepare action environment: {error}"));
            }
        };
    let label = ProcessLabel::new("deploy step".to_owned()).expect("static process label is valid");
    let command =
        match CommandSpec::new(program, arguments, working_dir, process_environment, label) {
            Ok(command) => command,
            Err(error) => return StepOutcome::Failed(format!("prepare action: {error}")),
        };
    let stream = OutputPolicy::Stream(StreamPolicy::new(OUTPUT_IN_FLIGHT_BYTES));
    let process = ProcessSpec::new(
        command,
        InputPolicy::Closed,
        stream,
        stream,
        ContainmentRequirement::CompleteTree,
        crate::framework::process::ProcessDeadline::Unlimited,
        TerminationPolicy::new(CANCEL_GRACE),
    );
    let started = match processes.spawn(process).await {
        Ok(started) => started,
        Err(error) => return StepOutcome::Failed(format!("start action: {error}")),
    };
    let StartedProcess { session, input: _, stdout, stderr } = started;
    let stdout_task = stream_lines(stdout, OutputStream::Stdout, event_tx.clone());
    let stderr_task = stream_lines(stderr, OutputStream::Stderr, event_tx.clone());
    let control = session.control();
    let wait = session.wait();
    tokio::pin!(wait);
    let cancellation = wait_for_cancellation(cancel_rx);
    tokio::pin!(cancellation);
    let report = tokio::select! {
        report = &mut wait => report,
        () = &mut cancellation => {
            if let Err(message) = acknowledge_cancellation(&control).await {
                let _ = event_tx.send(RunEvent::Output {
                    stream: OutputStream::Stderr,
                    line: format!("kit: {message}"),
                }).await;
            }
            wait.await
        }
    };
    let output_result = join_output(stdout_task, stderr_task).await;
    if let Err(error) = output_result {
        return StepOutcome::Failed(error);
    }
    match report {
        Ok(report) => outcome_from_report(report),
        Err(report) => StepOutcome::Failed(supervision_failure(report)),
    }
}

async fn acknowledge_cancellation(control: &ProcessControl) -> Result<(), String> {
    control
        .cancel()
        .await
        .map(|_| ())
        .map_err(|error| format!("action cancellation was not acknowledged: {error}"))
}

fn supervision_failure(report: crate::framework::process::ProcessFailureReport) -> String {
    let reason = match report.failure {
        crate::framework::process::ProcessFailureKind::InputIo => {
            "the action input stream failed".to_owned()
        }
        crate::framework::process::ProcessFailureKind::OutputIo { stream } => {
            format!("the action {stream:?} stream failed")
        }
        crate::framework::process::ProcessFailureKind::OutputLimitExceeded { stream } => {
            format!("the action {stream:?} stream exceeded its output limit")
        }
        crate::framework::process::ProcessFailureKind::RequiredConsumerLost { stream } => {
            format!("the action {stream:?} output consumer stopped")
        }
        crate::framework::process::ProcessFailureKind::ContainmentLost => {
            "the action process-tree owner was lost".to_owned()
        }
        crate::framework::process::ProcessFailureKind::TerminationUnconfirmed => {
            "the action process tree did not terminate conclusively".to_owned()
        }
        crate::framework::process::ProcessFailureKind::OwnerTaskFailed => {
            "the action supervisor stopped unexpectedly".to_owned()
        }
    };
    format!("action supervision failed: {reason}")
}

fn command_for(
    action: &DeployAction,
    version: &VersionId,
    branch: Option<&str>,
) -> (OsString, Vec<OsString>) {
    match action {
        DeployAction::Command { program, args } => (
            OsString::from(program),
            args.iter().map(|arg| OsString::from(substitute(arg, version, branch))).collect(),
        ),
        DeployAction::Shell { script } => (
            OsString::from("sh"),
            vec![OsString::from("-c"), OsString::from(substitute(script, version, branch))],
        ),
    }
}

fn substitute(value: &str, version: &VersionId, branch: Option<&str>) -> String {
    let substituted = value.replace("{{version}}", &version.0).replace("{{ref}}", &version.0);
    match branch {
        Some(branch) => substituted.replace("{{branch}}", branch),
        None => substituted,
    }
}

fn stream_lines(
    output: ProcessOutputHandle,
    stream: OutputStream,
    event_tx: mpsc::Sender<RunEvent>,
) -> JoinHandle<Result<(), String>> {
    tokio::spawn(async move {
        let ProcessOutputHandle::Stream(mut output) = output else {
            return Err("process supervisor returned a non-stream output handle".to_owned());
        };
        let mut pending = Vec::new();
        loop {
            match output.next().await.map_err(|error| error.to_string())? {
                ProcessByteEvent::Chunk { bytes, .. } => {
                    pending.extend_from_slice(bytes.as_ref());
                    emit_complete_lines(&mut pending, stream, &event_tx).await?;
                }
                ProcessByteEvent::End => {
                    if !pending.is_empty() {
                        let line = decode_line(&pending);
                        event_tx
                            .send(RunEvent::Output { stream, line })
                            .await
                            .map_err(|_| "deploy output consumer closed".to_owned())?;
                    }
                    break;
                }
            }
        }
        Ok(())
    })
}

async fn emit_complete_lines(
    pending: &mut Vec<u8>,
    stream: OutputStream,
    event_tx: &mpsc::Sender<RunEvent>,
) -> Result<(), String> {
    while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
        let mut line = pending.drain(..=newline).collect::<Vec<_>>();
        line.pop();
        event_tx
            .send(RunEvent::Output { stream, line: decode_line(&line) })
            .await
            .map_err(|_| "deploy output consumer closed".to_owned())?;
    }
    while pending.len() >= LINE_FRAGMENT_BYTES {
        let fragment = pending.drain(..LINE_FRAGMENT_BYTES).collect::<Vec<_>>();
        event_tx
            .send(RunEvent::Output { stream, line: decode_line(&fragment) })
            .await
            .map_err(|_| "deploy output consumer closed".to_owned())?;
    }
    Ok(())
}

fn decode_line(bytes: &[u8]) -> String {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

async fn join_output(
    stdout: JoinHandle<Result<(), String>>,
    stderr: JoinHandle<Result<(), String>>,
) -> Result<(), String> {
    for task in [stdout, stderr] {
        task.await.map_err(|error| format!("output task failed: {error}"))??;
    }
    Ok(())
}

async fn wait_for_cancellation(cancel_rx: &mut watch::Receiver<bool>) {
    loop {
        if *cancel_rx.borrow() {
            return;
        }
        if cancel_rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

fn outcome_from_report(report: ProcessReport) -> StepOutcome {
    if report.completion == CompletionCause::Cancelled {
        return StepOutcome::Cancelled;
    }
    match report.leader_exit {
        LeaderExitObservation::Observed(LeaderExit::Code(0)) => StepOutcome::Succeeded,
        LeaderExitObservation::Observed(LeaderExit::Code(code)) => {
            StepOutcome::Failed(format!("action exited with status {code}"))
        }
        LeaderExitObservation::Observed(LeaderExit::Signal(signal)) => {
            StepOutcome::Failed(format!("action terminated by signal {}", signal.get()))
        }
        LeaderExitObservation::NotObserved => {
            StepOutcome::Failed("action completed without an observed process leader".to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::deploy::{config::DeployStep, environment::parse_dotenv};

    fn target(action: DeployAction) -> DeployTarget {
        DeployTarget {
            id: "test".to_owned(),
            name: "Test".to_owned(),
            description: None,
            working_dir: None,
            env_file: None,
            steps: vec![DeployStep { name: "Run".to_owned(), working_dir: None, action }],
            backend: None,
            rollback: None,
        }
    }

    async fn final_outcome(spec: RunSpec) -> RunOutcome {
        let (mut events, _cancel, handle) = spawn(spec);
        let mut outcome = RunOutcome::Failed;
        while let Some(event) = events.recv().await {
            if let RunEvent::Finished { outcome: finished, .. } = event {
                outcome = finished;
            }
        }
        let _ = handle.await;
        outcome
    }

    async fn output_for(environment: TargetEnvironment, variable: &str) -> (RunOutcome, String) {
        let spec = RunSpec {
            base_dir: std::env::temp_dir(),
            operation: RunOperation::Deploy,
            targets: vec![RunTargetSpec {
                target: target(DeployAction::Shell {
                    script: format!("printf '%s' \"${variable}\""),
                }),
                version: VersionId("abc123".to_owned()),
                branch: None,
                environment,
            }],
        };
        let (mut events, _cancel, handle) = spawn(spec);
        let mut outcome = RunOutcome::Failed;
        let mut output = String::new();
        while let Some(event) = events.recv().await {
            match event {
                RunEvent::Output { stream: OutputStream::Stdout, line } => output.push_str(&line),
                RunEvent::Finished { outcome: finished, .. } => outcome = finished,
                _ => {}
            }
        }
        let _ = handle.await;
        (outcome, output)
    }

    #[tokio::test]
    async fn runs_successful_command() {
        let spec = RunSpec {
            base_dir: std::env::temp_dir(),
            operation: RunOperation::Deploy,
            targets: vec![RunTargetSpec {
                target: target(DeployAction::Command {
                    program: "sh".to_owned(),
                    args: vec!["-c".to_owned(), "printf ok".to_owned()],
                }),
                version: VersionId("abc123".to_owned()),
                branch: None,
                environment: TargetEnvironment::default(),
            }],
        };

        assert_eq!(final_outcome(spec).await, RunOutcome::Succeeded);
    }

    #[tokio::test]
    async fn stops_after_failed_command() {
        let spec = RunSpec {
            base_dir: std::env::temp_dir(),
            operation: RunOperation::Deploy,
            targets: vec![RunTargetSpec {
                target: target(DeployAction::Shell { script: "exit 7".to_owned() }),
                version: VersionId("abc123".to_owned()),
                branch: None,
                environment: TargetEnvironment::default(),
            }],
        };

        assert_eq!(final_outcome(spec).await, RunOutcome::Failed);
    }

    #[tokio::test]
    async fn cancellation_terminates_the_active_process_group() {
        let spec = RunSpec {
            base_dir: std::env::temp_dir(),
            operation: RunOperation::Deploy,
            targets: vec![RunTargetSpec {
                target: target(DeployAction::Shell { script: "sleep 30".to_owned() }),
                version: VersionId("abc123".to_owned()),
                branch: None,
                environment: TargetEnvironment::default(),
            }],
        };
        let (mut events, cancel, handle) = spawn(spec);
        let mut outcome = None;
        while let Some(event) = events.recv().await {
            if matches!(event, RunEvent::StepStarted { .. }) {
                let _ = cancel.send(true);
            }
            if let RunEvent::Finished { outcome: finished, .. } = event {
                outcome = Some(finished);
            }
        }
        let _ = handle.await;

        assert_eq!(outcome, Some(RunOutcome::Cancelled));
    }

    #[test]
    fn substitutes_selected_version_and_branch_in_arguments_and_scripts() {
        let version = VersionId("release-42".to_owned());
        assert_eq!(substitute("--ref={{ref}}", &version, None), "--ref=release-42");
        assert_eq!(substitute("restore {{version}}", &version, None), "restore release-42");
        assert_eq!(
            substitute("--branch={{branch}}", &version, Some("worker-verify")),
            "--branch=worker-verify"
        );
        assert_eq!(substitute("--branch={{branch}}", &version, None), "--branch={{branch}}");
    }

    #[tokio::test]
    async fn injects_target_environment_into_step_process() -> Result<(), Box<dyn std::error::Error>>
    {
        const VARIABLE: &str = "KIT_DEPLOY_FILE_INJECTION_TEST_7D4A";
        let environment = parse_dotenv(&format!("{VARIABLE}=from-file"))?;

        let (outcome, output) = output_for(environment, VARIABLE).await;

        assert_eq!(outcome, RunOutcome::Succeeded);
        assert_eq!(output, "from-file");
        Ok(())
    }

    #[tokio::test]
    async fn process_environment_overrides_step_file_value(
    ) -> Result<(), Box<dyn std::error::Error>> {
        const VARIABLE: &str = "KIT_DEPLOY_PROCESS_PRECEDENCE_TEST_2C91";
        let previous = std::env::var_os(VARIABLE);
        std::env::set_var(VARIABLE, "from-process");
        let environment = parse_dotenv(&format!("{VARIABLE}=from-file"))?;

        let result = output_for(environment, VARIABLE).await;
        match previous {
            Some(value) => std::env::set_var(VARIABLE, value),
            None => std::env::remove_var(VARIABLE),
        }

        assert_eq!(result.0, RunOutcome::Succeeded);
        assert_eq!(result.1, "from-process");
        Ok(())
    }
}
