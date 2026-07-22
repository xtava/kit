use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::framework::process::{
    CaptureOverflow, CapturePolicy, CommandSpec, ContainmentRequirement, EnvironmentBase,
    InputPolicy, LeaderExit, LeaderExitObservation, OutputPolicy, OutputReport, ProcessDeadline,
    ProcessEnvironment, ProcessLabel, ProcessSpec, ProcessSupervisor, TerminationPolicy,
};

const OUTPUT_LIMIT: NonZeroUsize = NonZeroUsize::new(256 * 1024).unwrap();
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub async fn run(
    processes: &ProcessSupervisor,
    label: &str,
    program: impl Into<OsString>,
    arguments: Vec<OsString>,
) -> Result<CommandOutput> {
    let cwd = std::env::current_dir()
        .context("resolving the working directory for Console service management")?;
    run_in(processes, label, program.into(), arguments, cwd).await
}

async fn run_in(
    processes: &ProcessSupervisor,
    label: &str,
    program: OsString,
    arguments: Vec<OsString>,
    working_directory: PathBuf,
) -> Result<CommandOutput> {
    let environment =
        ProcessEnvironment::new(EnvironmentBase::Inherit, BTreeMap::new(), BTreeSet::new())?;
    let command = CommandSpec::new(
        program,
        arguments,
        working_directory,
        environment,
        ProcessLabel::new(label.to_owned())?,
    )?;
    let capture =
        OutputPolicy::Capture(CapturePolicy::new(OUTPUT_LIMIT, CaptureOverflow::FailAndTerminate));
    let spec = ProcessSpec::new(
        command,
        InputPolicy::Closed,
        capture,
        capture,
        ContainmentRequirement::ExplicitProcessGroup,
        ProcessDeadline::After(COMMAND_TIMEOUT),
        TerminationPolicy::new(TERMINATION_GRACE),
    );
    let report = processes
        .spawn(spec)
        .await
        .with_context(|| format!("starting {label}"))?
        .session
        .wait()
        .await
        .map_err(|failure| anyhow!("{label} supervision failed: {:?}", failure.failure))?;
    let success =
        matches!(report.leader_exit, LeaderExitObservation::Observed(LeaderExit::Code(0)));
    Ok(CommandOutput {
        success,
        stdout: captured(report.stdout, label, "stdout")?,
        stderr: captured(report.stderr, label, "stderr")?,
    })
}

fn captured(report: OutputReport, label: &str, stream: &str) -> Result<String> {
    match report {
        OutputReport::Captured(capture) => {
            Ok(String::from_utf8_lossy(capture.bytes.as_ref()).trim().to_owned())
        }
        _ => bail!("{label} {stream} was not captured"),
    }
}
