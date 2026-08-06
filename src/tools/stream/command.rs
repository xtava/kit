use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    num::NonZeroUsize,
    path::PathBuf,
    time::Duration,
};

use anyhow::{bail, Context, Result};

use crate::framework::process::{
    CaptureOverflow, CapturePolicy, CommandSpec, ContainmentRequirement, EnvironmentBase,
    InputPolicy, LeaderExit, LeaderExitObservation, OutputPolicy, OutputReport, ProcessDeadline,
    ProcessEnvironment, ProcessLabel, ProcessSpec, ProcessSupervisor, TerminationPolicy,
};

const CAPTURE_BYTES: NonZeroUsize = NonZeroUsize::new(8 * 1024 * 1024).unwrap();
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(super) struct CommandRunner {
    processes: ProcessSupervisor,
    working_directory: PathBuf,
}

pub(super) struct CapturedCommand {
    pub exit: LeaderExitObservation,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CapturedCommand {
    pub(super) fn succeeded(&self) -> bool {
        self.exit == LeaderExitObservation::Observed(LeaderExit::Code(0))
    }

    pub(super) fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_owned()
    }

    pub(super) fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_owned()
    }

    pub(super) fn detail(&self) -> String {
        let stderr = self.stderr_text();
        if stderr.is_empty() {
            self.stdout_text()
        } else {
            stderr
        }
    }
}

impl CommandRunner {
    pub(super) fn new(processes: ProcessSupervisor, working_directory: PathBuf) -> Self {
        Self { processes, working_directory }
    }

    pub(super) async fn capture(
        &self,
        executable: impl Into<OsString>,
        arguments: impl IntoIterator<Item = OsString>,
        label: impl Into<String>,
    ) -> Result<CapturedCommand> {
        self.capture_with_environment(executable, arguments, label, BTreeMap::new()).await
    }

    pub(super) async fn capture_with_environment(
        &self,
        executable: impl Into<OsString>,
        arguments: impl IntoIterator<Item = OsString>,
        label: impl Into<String>,
        environment_values: BTreeMap<OsString, OsString>,
    ) -> Result<CapturedCommand> {
        let environment =
            ProcessEnvironment::new(EnvironmentBase::Inherit, environment_values, BTreeSet::new())?;
        let command = CommandSpec::new(
            executable.into(),
            arguments.into_iter().collect(),
            self.working_directory.clone(),
            environment,
            ProcessLabel::new(label.into())?,
        )?;
        let capture = OutputPolicy::Capture(CapturePolicy::new(
            CAPTURE_BYTES,
            CaptureOverflow::FailAndTerminate,
        ));
        self.capture_spec(ProcessSpec::new(
            command,
            InputPolicy::Closed,
            capture,
            capture,
            ContainmentRequirement::ExplicitProcessGroup,
            ProcessDeadline::After(COMMAND_TIMEOUT),
            TerminationPolicy::new(TERMINATION_GRACE),
        ))
        .await
    }

    pub(super) async fn capture_spec(&self, spec: ProcessSpec) -> Result<CapturedCommand> {
        let report = self.processes.spawn(spec).await?.session.wait().await.map_err(|failure| {
            anyhow::anyhow!("supervised command failed: {:?}", failure.failure)
        })?;
        Ok(CapturedCommand {
            exit: report.leader_exit,
            stdout: captured(report.stdout).context("command stdout was not captured")?,
            stderr: captured(report.stderr).context("command stderr was not captured")?,
        })
    }
}

fn captured(output: OutputReport) -> Result<Vec<u8>> {
    match output {
        OutputReport::Captured(output) => Ok(output.bytes.into_vec()),
        _ => bail!("expected captured process output"),
    }
}
