use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
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

const CAPTURE_BYTES: NonZeroUsize = NonZeroUsize::new(1024 * 1024).unwrap();
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
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

    pub(super) fn detail(&self) -> String {
        let stderr = String::from_utf8_lossy(&self.stderr).trim().to_owned();
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
        let environment =
            ProcessEnvironment::new(EnvironmentBase::Inherit, BTreeMap::new(), BTreeSet::new())?;
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
        let report = self
            .processes
            .spawn(ProcessSpec::new(
                command,
                InputPolicy::Closed,
                capture,
                capture,
                ContainmentRequirement::ExplicitProcessGroup,
                ProcessDeadline::After(COMMAND_TIMEOUT),
                TerminationPolicy::new(TERMINATION_GRACE),
            ))
            .await?
            .session
            .wait()
            .await
            .map_err(|failure| {
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

pub(super) fn os_args<const N: usize>(arguments: [&str; N]) -> Vec<OsString> {
    arguments.into_iter().map(OsString::from).collect()
}

pub(super) fn os_args_owned<I, S>(arguments: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    arguments.into_iter().map(|argument| argument.as_ref().to_os_string()).collect()
}
