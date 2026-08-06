use std::{num::NonZeroUsize, path::PathBuf};

use thiserror::Error;

use crate::{
    framework::process::{
        CaptureOverflow, CapturePolicy, InputPolicy, LeaderExitObservation, OutputPolicy,
        OutputReport, ProcessFailureKind, ProcessStartError, ProcessSupervisor,
    },
    tailscale::{
        find_login_url, LoginUrl, Node, RemoteCommand, TailscaleSshError, TailscaleSshStateError,
        TailscaleSshTarget,
    },
};

use super::model::RemoteEndpoint;

const OUTPUT_BYTES: usize = 1024 * 1024;
const DIRECTORY_READY: &str = "kit-sync-directory:ready";
const DIRECTORY_MISSING: &str = "kit-sync-directory:missing";
const DIRECTORY_PROBE: &str =
    "if [ -d \"$1\" ]; then printf 'kit-sync-directory:ready\\n'; else printf \
     'kit-sync-directory:missing\\n'; fi";

#[derive(Clone)]
pub struct RemoteProbe {
    processes: ProcessSupervisor,
    working_directory: PathBuf,
}

impl RemoteProbe {
    pub fn new(processes: ProcessSupervisor, working_directory: PathBuf) -> Self {
        Self { processes, working_directory }
    }

    pub async fn require_directory(
        &self,
        node: &Node,
        endpoint: &RemoteEndpoint,
    ) -> Result<(), RemoteProbeError> {
        let root = endpoint.root().to_str().ok_or(RemoteProbeError::PathEncoding)?;
        let address = node.addresses.first().copied().ok_or(RemoteProbeError::Address)?;
        let target = TailscaleSshTarget::new(node.id.clone(), endpoint.unix_user(), address)?;
        let remote_command =
            RemoteCommand::from_arguments(["sh", "-c", DIRECTORY_PROBE, "kit-sync-probe", root])?;
        let capture = OutputPolicy::Capture(CapturePolicy::new(
            NonZeroUsize::new(OUTPUT_BYTES).expect("remote probe output bound is non-zero"),
            CaptureOverflow::FailAndTerminate,
        ));
        let spec = target.command_process_spec(
            &remote_command,
            &self.working_directory,
            format!("inspect remote Synced Project root on {}", node.display_name()),
            InputPolicy::Closed,
            capture,
            capture,
        )?;
        let report = self
            .processes
            .spawn(spec)
            .await?
            .session
            .wait()
            .await
            .map_err(|failure| RemoteProbeError::Supervision(failure.failure))?;
        let stdout = captured(&report.stdout).ok_or(RemoteProbeError::OutputUnavailable)?;
        let stderr = captured(&report.stderr).ok_or(RemoteProbeError::OutputUnavailable)?;
        let detail = String::from_utf8_lossy(stderr).trim().to_owned();
        if let Some(url) = find_login_url(&detail) {
            return Err(RemoteProbeError::AuthenticationRequired {
                machine: node.display_name().to_owned(),
                url,
            });
        }
        match String::from_utf8_lossy(stdout).trim() {
            DIRECTORY_READY => Ok(()),
            DIRECTORY_MISSING => Err(RemoteProbeError::DirectoryMissing {
                machine: node.display_name().to_owned(),
                path: endpoint.root().to_path_buf(),
            }),
            output => Err(RemoteProbeError::CommandFailed {
                exit: report.leader_exit,
                detail: if detail.is_empty() {
                    format!("remote probe returned unexpected output {output:?}")
                } else {
                    detail
                },
            }),
        }
    }
}

fn captured(output: &OutputReport) -> Option<&[u8]> {
    match output {
        OutputReport::Captured(capture) => Some(&capture.bytes),
        _ => None,
    }
}

#[derive(Debug, Error)]
pub enum RemoteProbeError {
    #[error("remote Tailscale node has no transport address")]
    Address,
    #[error("remote synchronization path is not UTF-8")]
    PathEncoding,
    #[error(
        "OpenSSH authentication is required for {machine}; open {url} and retry `kit sync add`"
    )]
    AuthenticationRequired { machine: String, url: LoginUrl },
    #[error("remote synchronization root {} does not exist on {machine}", path.display())]
    DirectoryMissing { machine: String, path: PathBuf },
    #[error("remote synchronization probe exited unsuccessfully ({exit:?}): {detail}")]
    CommandFailed { exit: LeaderExitObservation, detail: String },
    #[error("remote synchronization probe output is unavailable")]
    OutputUnavailable,
    #[error("start remote synchronization probe")]
    Start(#[from] ProcessStartError),
    #[error("remote synchronization probe supervision failed: {0:?}")]
    Supervision(ProcessFailureKind),
    #[error(transparent)]
    Environment(#[from] crate::framework::process::ProcessEnvironmentError),
    #[error(transparent)]
    Command(#[from] crate::framework::process::CommandSpecError),
    #[error(transparent)]
    Label(#[from] crate::framework::process::ProcessLabelError),
    #[error(transparent)]
    TailscaleSsh(#[from] TailscaleSshError),
    #[error(transparent)]
    TailscaleSshState(#[from] TailscaleSshStateError),
    #[error(transparent)]
    TailscaleSshProcess(#[from] crate::tailscale::TailscaleSshProcessError),
}
