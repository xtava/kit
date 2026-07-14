use std::{path::PathBuf, process::Stdio, time::Duration};

use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, watch},
    task::JoinHandle,
    time,
};

use super::config::{DeployAction, DeployTarget};
use super::environment::TargetEnvironment;
use super::journal::VersionId;

const CANCEL_GRACE: Duration = Duration::from_secs(2);
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

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

pub fn spawn(spec: RunSpec) -> (mpsc::Receiver<RunEvent>, watch::Sender<bool>, JoinHandle<()>) {
    let (event_tx, event_rx) = mpsc::channel(256);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let handle = tokio::spawn(run(spec, event_tx, cancel_rx));
    (event_rx, cancel_tx, handle)
}

async fn run(
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
            let outcome = execute(
                &step.action,
                &working_dir,
                &run_target.version,
                run_target.branch.as_deref(),
                &run_target.environment,
                &event_tx,
                &mut cancel_rx,
            )
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

async fn execute(
    action: &DeployAction,
    working_dir: &std::path::Path,
    version: &VersionId,
    branch: Option<&str>,
    environment: &TargetEnvironment,
    event_tx: &mpsc::Sender<RunEvent>,
    cancel_rx: &mut watch::Receiver<bool>,
) -> StepOutcome {
    if !working_dir.is_dir() {
        return StepOutcome::Failed(format!(
            "working directory does not exist: {}",
            working_dir.display()
        ));
    }

    let mut command = command_for(action, version, branch);
    command
        .current_dir(working_dir)
        .envs(environment.child_values())
        .env("KIT_DEPLOY_VERSION", &version.0)
        .env("KIT_DEPLOY_REF", &version.0);
    if let Some(branch) = branch {
        command.env("KIT_DEPLOY_BRANCH", branch);
    }
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return StepOutcome::Failed(format!("start action: {error}")),
    };
    let stdout_task = child
        .stdout
        .take()
        .map(|stdout| stream_lines(stdout, OutputStream::Stdout, event_tx.clone()));
    let stderr_task = child
        .stderr
        .take()
        .map(|stderr| stream_lines(stderr, OutputStream::Stderr, event_tx.clone()));

    let status = tokio::select! {
        status = child.wait() => status.map_err(|error| format!("wait for action: {error}")),
        changed = cancel_rx.changed() => {
            if changed.is_ok() && *cancel_rx.borrow() {
                terminate(&mut child).await;
                join_output(stdout_task, stderr_task).await;
                return StepOutcome::Cancelled;
            }
            child.wait().await.map_err(|error| format!("wait for action: {error}"))
        }
    };

    join_output(stdout_task, stderr_task).await;
    match status {
        Ok(status) if status.success() => StepOutcome::Succeeded,
        Ok(status) => StepOutcome::Failed(match status.code() {
            Some(code) => format!("action exited with status {code}"),
            None => "action terminated by signal".to_owned(),
        }),
        Err(error) => StepOutcome::Failed(error),
    }
}

fn command_for(action: &DeployAction, version: &VersionId, branch: Option<&str>) -> Command {
    match action {
        DeployAction::Command { program, args } => {
            let mut command = Command::new(program);
            command.args(args.iter().map(|arg| substitute(arg, version, branch)));
            command
        }
        DeployAction::Shell { script } => {
            let mut command = Command::new("sh");
            command.arg("-c").arg(substitute(script, version, branch));
            command
        }
    }
}

fn substitute(value: &str, version: &VersionId, branch: Option<&str>) -> String {
    let substituted = value.replace("{{version}}", &version.0).replace("{{ref}}", &version.0);
    match branch {
        Some(branch) => substituted.replace("{{branch}}", branch),
        None => substituted,
    }
}

fn stream_lines<R>(
    reader: R,
    stream: OutputStream,
    event_tx: mpsc::Sender<RunEvent>,
) -> JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if event_tx.send(RunEvent::Output { stream, line }).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = event_tx
                        .send(RunEvent::Output {
                            stream: OutputStream::Stderr,
                            line: format!("kit: could not read action output: {error}"),
                        })
                        .await;
                    break;
                }
            }
        }
    })
}

async fn join_output(stdout: Option<JoinHandle<()>>, stderr: Option<JoinHandle<()>>) {
    if let Some(stdout) = stdout {
        finish_output(stdout).await;
    }
    if let Some(stderr) = stderr {
        finish_output(stderr).await;
    }
}

async fn finish_output(mut handle: JoinHandle<()>) {
    if time::timeout(OUTPUT_DRAIN_TIMEOUT, &mut handle).await.is_err() {
        handle.abort();
        let _ = handle.await;
    }
}

async fn terminate(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }

    if time::timeout(CANCEL_GRACE, child.wait()).await.is_ok() {
        return;
    }

    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }
    let _ = child.wait().await;
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
