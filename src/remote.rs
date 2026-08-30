//! Reusable remote command execution over authenticated Tailscale SSH.

use std::{num::NonZeroUsize, path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};

use crate::{
    framework::process::{
        CaptureOverflow, CapturePolicy, InputPolicy, LeaderExit, LeaderExitObservation,
        OutputPolicy, OutputReport, PrivateBytes, ProcessDeadline, ProcessSupervisor,
    },
    tailscale::{Readiness, RemoteCommand, TailscaleClient, TailscaleSshTarget},
};

pub const MAX_REMOTE_INPUT_BYTES: usize = 256 * 1024 * 1024;

const MAX_REMOTE_OUTPUT_BYTES: NonZeroUsize =
    NonZeroUsize::new(64 * 1024 * 1024).expect("remote output limit is non-zero");
const STATUS_SCRIPT: &str =
    r#""$@"; status=$?; printf '\036KIT-REMOTE-EXIT:%s:%s\037' "$0" "$status" >&2; exit 0"#;

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
        let status_nonce = uuid::Uuid::new_v4().simple().to_string();
        let mut wrapped_arguments =
            vec!["sh".to_owned(), "-c".to_owned(), STATUS_SCRIPT.to_owned(), status_nonce.clone()];
        wrapped_arguments.extend(request.arguments);
        let command = RemoteCommand::from_arguments(wrapped_arguments.iter().map(String::as_str))?;
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
        let stdout = captured(report.stdout).context("remote stdout was not captured")?;
        let mut stderr =
            captured(report.stderr).context("remote stderr was not captured")?.into_vec();
        if report.leader_exit != LeaderExitObservation::Observed(LeaderExit::Code(0)) {
            bail!(
                "remote transport exited unsuccessfully: {}",
                String::from_utf8_lossy(&stderr).trim()
            );
        }
        let exit = take_remote_exit(&mut stderr, &status_nonce)?;
        Ok(RemoteOutput { stdout, stderr: stderr.into_boxed_slice(), exit })
    }
}

fn take_remote_exit(stderr: &mut Vec<u8>, nonce: &str) -> Result<LeaderExitObservation> {
    let prefix = format!("\u{1e}KIT-REMOTE-EXIT:{nonce}:");
    let start = stderr
        .windows(prefix.len())
        .rposition(|window| window == prefix.as_bytes())
        .context("remote command did not return its exit status")?;
    let status = stderr
        .get(start + prefix.len()..)
        .and_then(|suffix| suffix.strip_suffix(&[0x1f]))
        .context("remote command returned an invalid exit status")?;
    let code = std::str::from_utf8(status)?.parse::<i32>()?;
    if !(0..=255).contains(&code) {
        bail!("remote command returned an invalid exit code");
    }
    stderr.truncate(start);
    Ok(LeaderExitObservation::Observed(LeaderExit::Code(code)))
}

fn captured(output: OutputReport) -> Option<Box<[u8]>> {
    match output {
        OutputReport::Captured(capture) => Some(capture.bytes),
        _ => None,
    }
}
