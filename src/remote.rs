//! Reusable remote command execution over authenticated Tailscale SSH.

use std::{num::NonZeroUsize, path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};

use crate::{
    framework::process::{
        CaptureOverflow, CapturePolicy, InputPolicy, LeaderExitObservation, OutputPolicy,
        OutputReport, PrivateBytes, ProcessDeadline, ProcessSupervisor,
    },
    tailscale::{Readiness, RemoteCommand, TailscaleClient, TailscaleSshTarget},
};

pub const MAX_REMOTE_INPUT_BYTES: usize = 256 * 1024 * 1024;

const MAX_REMOTE_OUTPUT_BYTES: NonZeroUsize =
    NonZeroUsize::new(64 * 1024 * 1024).expect("remote output limit is non-zero");

/// One command invocation on a tailnet peer.
pub struct RemoteRequest {
    machine: String,
    user: String,
    arguments: Vec<String>,
    input: Vec<u8>,
    timeout: Duration,
}

impl RemoteRequest {
    pub fn new(
        machine: impl Into<String>,
        user: impl Into<String>,
        arguments: Vec<String>,
        input: Vec<u8>,
        timeout: Duration,
    ) -> Result<Self> {
        let machine = machine.into();
        if machine.trim().is_empty() {
            bail!("a remote machine is required");
        }
        if input.len() > MAX_REMOTE_INPUT_BYTES {
            bail!("remote input exceeds the {} MiB limit", MAX_REMOTE_INPUT_BYTES / 1024 / 1024);
        }
        if timeout.is_zero() {
            bail!("remote timeout must be greater than zero");
        }
        RemoteCommand::from_arguments(arguments.iter().map(String::as_str))?;
        Ok(Self { machine, user: user.into(), arguments, input, timeout })
    }
}

/// Raw output and exit evidence returned by a remote command.
pub struct RemoteOutput {
    pub stdout: Box<[u8]>,
    pub stderr: Box<[u8]>,
    pub exit: LeaderExitObservation,
}

/// Executes operations through Kit's canonical Tailscale SSH policy.
#[derive(Clone)]
pub struct RemoteExecutor {
    processes: ProcessSupervisor,
    working_directory: PathBuf,
}

impl RemoteExecutor {
    pub fn new(processes: ProcessSupervisor, working_directory: PathBuf) -> Self {
        Self { processes, working_directory }
    }

    pub async fn execute(&self, request: RemoteRequest) -> Result<RemoteOutput> {
        let client = TailscaleClient::new(self.processes.clone(), self.working_directory.clone());
        let status = match client.readiness().await? {
            Readiness::Ready(status) => status,
            Readiness::NeedsLogin => bail!("Tailscale needs authentication"),
            Readiness::CliUnavailable(detail) => bail!("Tailscale CLI unavailable: {detail}"),
            Readiness::DaemonUnavailable(detail) => bail!("Tailscale daemon unavailable: {detail}"),
            Readiness::PermissionDenied(detail) => bail!("Tailscale permission denied: {detail}"),
            Readiness::Unsupported(detail) => bail!("unsupported Tailscale status: {detail}"),
        };
        let node = status.resolve_peer(&request.machine)?;
        if !node.online {
            bail!("Tailscale peer {} is offline", node.display_name());
        }
        let address = node
            .preferred_address()
            .context("the selected Tailscale peer has no routable address")?;
        let target = TailscaleSshTarget::new(node.id.clone(), request.user, address)?;
        let command = RemoteCommand::from_arguments(request.arguments.iter().map(String::as_str))?;
        let input = if request.input.is_empty() {
            InputPolicy::Closed
        } else {
            InputPolicy::Once(PrivateBytes::new(request.input))
        };
        let capture = OutputPolicy::Capture(CapturePolicy::new(
            MAX_REMOTE_OUTPUT_BYTES,
            CaptureOverflow::FailAndTerminate,
        ));
        let spec = target.command_process_spec(
            &command,
            &self.working_directory,
            format!("run remote operation on {}", node.display_name()),
            ProcessDeadline::After(request.timeout),
            input,
            capture,
            capture,
        )?;
        let report = self.processes.spawn(spec).await?.session.wait().await.map_err(|failure| {
            anyhow::anyhow!("remote command supervision failed: {:?}", failure.failure)
        })?;
        Ok(RemoteOutput {
            stdout: captured(report.stdout).context("remote stdout was not captured")?,
            stderr: captured(report.stderr).context("remote stderr was not captured")?,
            exit: report.leader_exit,
        })
    }
}

fn captured(output: OutputReport) -> Option<Box<[u8]>> {
    match output {
        OutputReport::Captured(capture) => Some(capture.bytes),
        _ => None,
    }
}
