use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tokio::sync::watch;

use crate::{
    framework::process::{
        CaptureOverflow, CapturePolicy, CommandSpec, ContainmentRequirement, EnvironmentBase,
        InputPolicy, LeaderExit, LeaderExitObservation, OutputPolicy, ProcessByteEvent,
        ProcessDeadline, ProcessEnvironment, ProcessLabel, ProcessOutputHandle, ProcessSpec,
        ProcessSupervisor, StreamPolicy, TerminationPolicy,
    },
    framework::{start_external, ExternalTarget},
    release::UpdateOutcome,
    tailscale::{find_login_url, LoginEvent, Node, Readiness, Status, TailscaleClient},
};

use super::{
    config::Config,
    service::{ConsoleStatus, RemoteFailureKind},
    transport::{RelayEpochFailure, RelayEpochProvider, RelayTarget, SshRelay},
};

const STATUS_CAPTURE_BYTES: NonZeroUsize = NonZeroUsize::new(1024 * 1024).unwrap();
const STDERR_STREAM_BYTES: NonZeroUsize = NonZeroUsize::new(64 * 1024).unwrap();
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
const TERMINATION_GRACE: Duration = Duration::from_secs(3);

pub(crate) struct RemoteTarget {
    machine: String,
    relay: RelayTarget,
}

pub(crate) struct RemoteRelay {
    relay: SshRelay,
    status: watch::Receiver<Option<ConsoleStatus>>,
}

impl RemoteRelay {
    pub(crate) fn socket_path(&self) -> &std::path::Path {
        self.relay.socket_path()
    }

    pub(crate) fn status_receiver(&self) -> watch::Receiver<Option<ConsoleStatus>> {
        self.status.clone()
    }

    pub(crate) fn latest_status(&self) -> Option<ConsoleStatus> {
        self.status.borrow().clone()
    }

    pub(crate) async fn shutdown(self) -> Result<()> {
        self.relay.shutdown().await.map_err(Into::into)
    }
}

struct RemoteEpochProvider {
    processes: ProcessSupervisor,
    machine: String,
    identity: RelayTarget,
    authenticate_next: AtomicBool,
    status: watch::Sender<Option<ConsoleStatus>>,
}

pub(crate) enum Resolution {
    Ready(RemoteTarget),
    Status(ConsoleStatus),
}

pub(crate) async fn resolve(
    processes: &ProcessSupervisor,
    config: &mut Config,
    selector: &str,
) -> Result<Resolution> {
    let (explicit_user, machine) = split_selector(selector)?;
    let status = match tailnet_status(processes).await? {
        Ok(status) => status,
        Err(status) => return Ok(Resolution::Status(status)),
    };
    let node = status.resolve_peer(machine)?;
    resolve_node(config, node, explicit_user)
}

pub(crate) fn resolve_node(
    config: &mut Config,
    node: &Node,
    explicit_user: Option<&str>,
) -> Result<Resolution> {
    if !node.online {
        return Ok(Resolution::Status(ConsoleStatus::PeerOffline {
            machine: node.display_name().to_owned(),
            action: "bring the machine online and retry".to_owned(),
        }));
    }
    let (user, should_persist) = if let Some(user) = explicit_user {
        (user.to_owned(), true)
    } else if let Some(user) = config.unix_user(&node.id) {
        (user.to_owned(), false)
    } else {
        return Ok(Resolution::Status(ConsoleStatus::NeedsUnixUser {
            machine: node.display_name().to_owned(),
            stable_node_id: node.id.clone(),
            action: format!("retry as USER@{}", node.display_name()),
        }));
    };
    let address = node.addresses.first().copied().context("Tailscale peer has no address")?;
    let relay = RelayTarget::new(node.id.clone(), user.clone(), address)?;
    if should_persist {
        config.set_unix_user(&node.id, &user)?;
    }
    Ok(Resolution::Ready(RemoteTarget { machine: node.display_name().to_owned(), relay }))
}

async fn tailnet_status(processes: &ProcessSupervisor) -> Result<Result<Status, ConsoleStatus>> {
    let client = TailscaleClient::new(processes.clone(), std::env::current_dir()?);
    Ok(match client.readiness().await? {
        Readiness::Ready(status) => Ok(status),
        Readiness::NeedsLogin => {
            Err(ConsoleStatus::NeedsTailscaleLogin { action: "tailscale login".to_owned() })
        }
        Readiness::CliUnavailable(detail) => Err(ConsoleStatus::TailscaleCliUnavailable {
            detail,
            action: "install the Tailscale CLI and retry".to_owned(),
        }),
        Readiness::DaemonUnavailable(detail) => Err(ConsoleStatus::TailscaleDaemonUnavailable {
            detail,
            action: "start Tailscale and retry".to_owned(),
        }),
        Readiness::PermissionDenied(detail) => Err(ConsoleStatus::TailscalePermissionDenied {
            detail,
            action: "restore access to the local Tailscale daemon and retry".to_owned(),
        }),
        Readiness::Unsupported(detail) => Err(ConsoleStatus::TailscaleUnsupported {
            detail,
            action: "update Tailscale and retry".to_owned(),
        }),
    })
}

pub(crate) async fn login(processes: &ProcessSupervisor) -> Result<()> {
    let client = TailscaleClient::new(processes.clone(), std::env::current_dir()?);
    let (mut events, _cancel, owner) = client.start_login();
    let mut outcome = Ok(false);
    while let Some(event) = events.recv().await {
        match event {
            LoginEvent::Url(url) => {
                println!("Authenticate Tailscale: {url}");
                if let Err(error) = async {
                    start_external(processes, ExternalTarget::Url(url.as_str().to_owned()))?
                        .completion()
                        .await
                }
                .await
                {
                    eprintln!("Could not open the Tailscale authentication link: {error:#}");
                }
            }
            LoginEvent::Ready(_) => {
                outcome = Ok(true);
                break;
            }
            LoginEvent::Failed(detail) => {
                outcome = Err(anyhow::anyhow!("Tailscale login failed: {detail}"));
                break;
            }
            LoginEvent::Cancelled => {
                outcome = Err(anyhow::anyhow!("Tailscale login was cancelled"));
                break;
            }
        }
    }
    owner.await.context("joining the Tailscale login owner")?;
    if !outcome? {
        bail!("Tailscale login ended before the device became ready");
    }
    Ok(())
}

pub(crate) async fn status(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
) -> Result<ConsoleStatus> {
    status_with_authentication(processes, target, false).await
}

async fn status_with_authentication(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
    authenticate: bool,
) -> Result<ConsoleStatus> {
    let status =
        invoke_status(processes, target, &["kit", "--json", "console", "status"], authenticate)
            .await?;
    if let ConsoleStatus::Ready { platform, sessions, build } = status {
        let expected = super::build_identity()?;
        if build != expected {
            return Ok(ConsoleStatus::BuildIncompatible {
                platform,
                sessions: Some(sessions),
                expected,
                actual: build,
                action: format!("update Kit on {} and run setup again", target.machine),
            });
        }
        return Ok(ConsoleStatus::Ready { platform, sessions, build });
    }
    Ok(status)
}

pub(crate) async fn setup(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
) -> Result<ConsoleStatus> {
    invoke_status(processes, target, &["kit", "--json", "console", "setup"], false).await
}

pub(crate) async fn stop(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
    force: bool,
) -> Result<ConsoleStatus> {
    let mut arguments = vec!["kit", "--json", "console", "stop"];
    if force {
        arguments.push("--force");
    }
    invoke_status(processes, target, &arguments, false).await
}

pub(crate) async fn update(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
) -> Result<UpdateOutcome> {
    match invoke_bytes(processes, target, &["kit", "--json", "update"], false).await? {
        RemoteCommandResult::Output(bytes) => {
            serde_json::from_slice(&bytes).context("decode typed remote Kit update outcome")
        }
        RemoteCommandResult::Status(status) => bail!("{}", status.text()),
    }
}

pub(crate) async fn start_relay(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
) -> Result<RemoteRelay> {
    let (status, receiver) = watch::channel(None);
    let provider: Arc<dyn RelayEpochProvider> = Arc::new(RemoteEpochProvider {
        processes: processes.clone(),
        machine: target.machine.clone(),
        identity: target.relay.clone(),
        authenticate_next: AtomicBool::new(true),
        status,
    });
    let relay = SshRelay::start(processes, provider).await?;
    Ok(RemoteRelay { relay, status: receiver })
}

#[async_trait]
impl RelayEpochProvider for RemoteEpochProvider {
    async fn prepare(&self) -> Result<RelayTarget, RelayEpochFailure> {
        match self.prepare_status().await {
            Ok(target) => {
                self.status.send_replace(None);
                Ok(target)
            }
            Err(status) => {
                self.status.send_replace(Some(status));
                Err(RelayEpochFailure::Preflight)
            }
        }
    }
}

impl RemoteEpochProvider {
    async fn prepare_status(&self) -> Result<RelayTarget, ConsoleStatus> {
        let tailnet = tailnet_status(&self.processes)
            .await
            .map_err(|_| transport_failed_for(&self.machine))??;
        let node = tailnet
            .resolve_peer(self.identity.stable_node_id())
            .map_err(|_| transport_failed_for(&self.machine))?;
        if !node.online {
            return Err(ConsoleStatus::PeerOffline {
                machine: node.display_name().to_owned(),
                action: "bring the machine online and retry".to_owned(),
            });
        }
        let address =
            node.addresses.first().copied().ok_or_else(|| transport_failed_for(&self.machine))?;
        let relay = self
            .identity
            .with_tailscale_ip(address)
            .map_err(|_| transport_failed_for(&self.machine))?;
        let target = RemoteTarget { machine: node.display_name().to_owned(), relay: relay.clone() };
        let authenticate = self.authenticate_next.swap(false, Ordering::AcqRel);
        let status = status_with_authentication(&self.processes, &target, authenticate)
            .await
            .map_err(|_| transport_failed_for(&self.machine))?;
        if status.ready() {
            Ok(relay)
        } else {
            Err(status)
        }
    }
}

enum RemoteCommandResult {
    Output(Vec<u8>),
    Status(Box<ConsoleStatus>),
}

impl RemoteCommandResult {
    fn status(status: ConsoleStatus) -> Self {
        Self::Status(Box::new(status))
    }
}

async fn invoke_status(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
    remote_arguments: &[&str],
    authenticate: bool,
) -> Result<ConsoleStatus> {
    match invoke_bytes(processes, target, remote_arguments, authenticate).await? {
        RemoteCommandResult::Output(bytes) => {
            if bytes.is_empty() {
                return Ok(remote_failure(
                    &target.machine,
                    RemoteFailureKind::EmptyOutput,
                    "the remote command exited successfully without JSON output".to_owned(),
                ));
            }
            match serde_json::from_slice(&bytes) {
                Ok(status) => Ok(status),
                Err(error) => Ok(remote_failure(
                    &target.machine,
                    RemoteFailureKind::Decode,
                    format!("decode typed remote Console status: {error}"),
                )),
            }
        }
        RemoteCommandResult::Status(status) => Ok(*status),
    }
}

async fn invoke_bytes(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
    remote_arguments: &[&str],
    authenticate: bool,
) -> Result<RemoteCommandResult> {
    let arguments = target.relay.ssh_arguments(remote_arguments)?;
    let environment =
        ProcessEnvironment::new(EnvironmentBase::Inherit, BTreeMap::new(), BTreeSet::new())?;
    let command = CommandSpec::new(
        OsString::from("ssh"),
        arguments,
        std::env::current_dir()?,
        environment,
        ProcessLabel::new(format!("inspect Console on {}", target.machine))?,
    )?;
    let stdout = OutputPolicy::Capture(CapturePolicy::new(
        STATUS_CAPTURE_BYTES,
        CaptureOverflow::FailAndTerminate,
    ));
    let spec = ProcessSpec::new(
        command,
        InputPolicy::Closed,
        stdout,
        OutputPolicy::Stream(StreamPolicy::new(STDERR_STREAM_BYTES)),
        ContainmentRequirement::ExplicitProcessGroup,
        ProcessDeadline::After(COMMAND_TIMEOUT),
        TerminationPolicy::new(TERMINATION_GRACE),
    );
    let started = match processes.spawn(spec).await {
        Ok(started) => started,
        Err(error) => {
            let detail = format!("start supervised remote Console command: {error:#}");
            let kind = if detail.to_ascii_lowercase().contains("no such file")
                || detail.to_ascii_lowercase().contains("not found")
            {
                RemoteFailureKind::OpenSshUnavailable
            } else {
                RemoteFailureKind::Supervision
            };
            return Ok(RemoteCommandResult::status(remote_failure(&target.machine, kind, detail)));
        }
    };
    let mut stderr = match started.stderr {
        ProcessOutputHandle::Stream(stderr) => stderr,
        _ => {
            let control = started.session.control();
            let _ = control.cancel().await;
            let _ = started.session.wait().await;
            bail!("remote Console stderr was not streamed")
        }
    };
    let control = started.session.control();
    let wait = started.session.wait();
    tokio::pin!(wait);
    let mut stderr_bytes = Vec::new();
    let mut opened_authentication = false;
    let report = loop {
        tokio::select! {
            event = stderr.next() => match event {
                Ok(ProcessByteEvent::Chunk { bytes, .. }) => {
                    stderr_bytes.extend_from_slice(&bytes);
                    if stderr_bytes.len() > STDERR_STREAM_BYTES.get() {
                        let excess = stderr_bytes.len() - STDERR_STREAM_BYTES.get();
                        stderr_bytes.drain(..excess);
                    }
                    if let Some(status) = ssh_authentication_status(target, &stderr_bytes) {
                        if !authenticate {
                            let _ = control.cancel().await;
                            let _ = wait.await;
                            return Ok(RemoteCommandResult::status(status));
                        }
                        if !opened_authentication {
                            let ConsoleStatus::NeedsSshAuthentication { url, .. } = &status else {
                                unreachable!("the authentication detector returns one status")
                            };
                            println!("Authenticate remote Console: {url}");
                            if let Err(error) = async {
                                start_external(processes, ExternalTarget::Url(url.clone()))?
                                    .completion()
                                    .await
                            }
                            .await
                            {
                                let _ = control.cancel().await;
                                let _ = wait.await;
                                eprintln!("Could not open the authentication link: {error:#}");
                                return Ok(RemoteCommandResult::status(status));
                            }
                            opened_authentication = true;
                        }
                    }
                }
                Ok(ProcessByteEvent::End) => break wait.await,
                Err(_) => {
                    let _ = control.cancel().await;
                    let _ = wait.await;
                    return Ok(RemoteCommandResult::status(transport_failed(target)));
                }
            },
            report = &mut wait => break report,
        }
    };
    let report = match report {
        Ok(report) => report,
        Err(failure) => {
            if let Some(status) = ssh_authentication_status(target, &stderr_bytes) {
                return Ok(RemoteCommandResult::status(status));
            }
            let detail = format!("remote command supervision failed: {:?}", failure.failure);
            let kind = if detail.to_ascii_lowercase().contains("deadline")
                || detail.to_ascii_lowercase().contains("timed out")
            {
                RemoteFailureKind::Timeout
            } else {
                RemoteFailureKind::Supervision
            };
            return Ok(RemoteCommandResult::status(remote_failure(&target.machine, kind, detail)));
        }
    };
    if report.leader_exit != LeaderExitObservation::Observed(LeaderExit::Code(0)) {
        if let Some(status) = ssh_authentication_status(target, &stderr_bytes) {
            return Ok(RemoteCommandResult::status(status));
        }
        let detail = String::from_utf8_lossy(&stderr_bytes).trim().to_owned();
        let kind = ssh_exit_failure_kind(&detail);
        return Ok(RemoteCommandResult::status(remote_failure(
            &target.machine,
            kind,
            if detail.is_empty() {
                format!("remote command exited with {:?}", report.leader_exit)
            } else {
                detail
            },
        )));
    }
    let crate::framework::process::OutputReport::Captured(stdout) = report.stdout else {
        bail!("remote Console status stdout was not captured")
    };
    Ok(RemoteCommandResult::Output(stdout.bytes.into_vec()))
}

fn ssh_authentication_status(target: &RemoteTarget, stderr: &[u8]) -> Option<ConsoleStatus> {
    let stderr = String::from_utf8_lossy(stderr);
    let url = find_login_url(&stderr)?;
    Some(ConsoleStatus::NeedsSshAuthentication {
        machine: target.machine.clone(),
        url: url.as_str().to_owned(),
        action: "open the link, authenticate, then retry".to_owned(),
    })
}

fn ssh_exit_failure_kind(stderr: &str) -> RemoteFailureKind {
    let stderr = stderr.to_ascii_lowercase();
    if [
        "connection refused",
        "connection timed out",
        "operation timed out",
        "no route to host",
        "could not resolve hostname",
        "connection closed",
    ]
    .iter()
    .any(|pattern| stderr.contains(pattern))
    {
        RemoteFailureKind::Transport
    } else {
        RemoteFailureKind::RemoteCommand
    }
}

fn transport_failed(target: &RemoteTarget) -> ConsoleStatus {
    remote_failure(
        &target.machine,
        RemoteFailureKind::Transport,
        "OpenSSH could not reach the machine".to_owned(),
    )
}

fn transport_failed_for(machine: &str) -> ConsoleStatus {
    remote_failure(
        machine,
        RemoteFailureKind::Transport,
        "OpenSSH could not reach the machine".to_owned(),
    )
}

fn remote_failure(machine: &str, kind: RemoteFailureKind, detail: String) -> ConsoleStatus {
    ConsoleStatus::RemoteFailure {
        machine: machine.to_owned(),
        kind,
        detail,
        action: "inspect the details, correct the failing layer, and retry".to_owned(),
    }
}

fn split_selector(selector: &str) -> Result<(Option<&str>, &str)> {
    let selector = selector.trim();
    if selector.is_empty() {
        bail!("Console machine selector cannot be empty");
    }
    match selector.split_once('@') {
        Some((user, machine))
            if !user.is_empty() && !machine.is_empty() && !machine.contains('@') =>
        {
            Ok((Some(user), machine))
        }
        Some(_) => bail!("use USER@MACHINE with exactly one non-empty user and machine"),
        None => Ok((None, selector)),
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::{
        split_selector, ssh_authentication_status, ssh_exit_failure_kind, ConsoleStatus,
        RelayTarget, RemoteFailureKind, RemoteTarget,
    };

    #[test]
    fn selector_accepts_machine_or_explicit_user_at_machine() {
        assert_eq!(split_selector("tvxm").unwrap(), (None, "tvxm"));
        assert_eq!(split_selector("tvx@tvxm").unwrap(), (Some("tvx"), "tvxm"));
        assert!(split_selector("@tvxm").is_err());
        assert!(split_selector("tvx@").is_err());
        assert!(split_selector("a@b@c").is_err());
    }

    #[test]
    fn only_a_strict_tailscale_login_url_becomes_authentication_state() {
        let target = RemoteTarget {
            machine: "mac".to_owned(),
            relay: RelayTarget::new("node-1", "tvx", "100.64.0.2".parse::<IpAddr>().unwrap())
                .unwrap(),
        };
        let status = ssh_authentication_status(
            &target,
            b"authenticate: https://login.tailscale.com/a/verified-token\n",
        );
        assert!(matches!(
            status,
            Some(ConsoleStatus::NeedsSshAuthentication { machine, url, .. })
                if machine == "mac"
                    && url == "https://login.tailscale.com/a/verified-token"
        ));
        assert!(ssh_authentication_status(
            &target,
            b"authenticate: https://login.tailscale.com.evil/a/token\n"
        )
        .is_none());
    }

    #[test]
    fn ssh_exit_diagnostics_separate_transport_from_remote_command_failures() {
        assert_eq!(
            ssh_exit_failure_kind("ssh: connect to host 100.64.0.2: Connection refused"),
            RemoteFailureKind::Transport
        );
        assert_eq!(
            ssh_exit_failure_kind("Error: Decode limit exceeded for containers: 65536"),
            RemoteFailureKind::RemoteCommand
        );
    }
}
