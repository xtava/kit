use std::{
    num::NonZeroUsize,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tokio::sync::watch;

use crate::{
    framework::process::{
        CaptureOverflow, CapturePolicy, InputPolicy, LeaderExit, LeaderExitObservation,
        OutputPolicy, PrivateBytes, ProcessByteEvent, ProcessOutputHandle, ProcessSupervisor,
        StreamPolicy,
    },
    framework::{start_external, ExternalTarget},
    release::{ReleaseUpdater, UpdateOutcome},
    tailscale::{
        find_login_url, prepare_known_hosts_file, LoginEvent, Node, Readiness, RemoteCommand,
        SshConnectionKind, Status, TailscaleClient, TailscaleSshTarget,
    },
};

use super::{
    config::Config,
    service::{ConsoleStage, ConsoleStatus, RemoteFailureKind},
    transport::{
        PreparedRelayEpoch, RelayEpochFailure, RelayEpochProvider, SshRelay, SshRelayError,
    },
};

const STATUS_CAPTURE_BYTES: NonZeroUsize = NonZeroUsize::new(1024 * 1024).unwrap();
const STDERR_STREAM_BYTES: NonZeroUsize = NonZeroUsize::new(64 * 1024).unwrap();
const MAX_RELEASE_BINARY_BYTES: u64 = 128 * 1024 * 1024;
static NEXT_BOOTSTRAP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct RemoteConnection {
    machine: String,
    relay: TailscaleSshTarget,
    known_hosts_file: PathBuf,
}

pub(crate) struct RemoteTarget {
    connection: RemoteConnection,
    remote_kit: String,
}

impl RemoteTarget {
    pub(crate) fn persist_unix_user(&self, config: &mut Config) -> Result<()> {
        config.set_unix_user(
            self.connection.relay.stable_node_id(),
            self.connection.relay.unix_user(),
        )
    }
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
    identity: TailscaleSshTarget,
    known_hosts_file: PathBuf,
    remote_kit: String,
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
    resolve_node(processes, config, node, explicit_user).await
}

pub(crate) async fn resolve_node(
    processes: &ProcessSupervisor,
    config: &Config,
    node: &Node,
    explicit_user: Option<&str>,
) -> Result<Resolution> {
    if !node.online {
        return Ok(Resolution::Status(ConsoleStatus::PeerOffline {
            machine: node.display_name().to_owned(),
        }));
    }
    let user = if let Some(user) = explicit_user {
        user.to_owned()
    } else if let Some(user) = config.unix_user(&node.id) {
        user.to_owned()
    } else {
        return Ok(Resolution::Status(ConsoleStatus::NeedsUnixUser {
            machine: node.display_name().to_owned(),
            stable_node_id: node.id.clone(),
        }));
    };
    let address = node.addresses.first().copied().context("Tailscale peer has no address")?;
    let relay = TailscaleSshTarget::new(node.id.clone(), user.clone(), address)?;
    let known_hosts_file = prepare_known_hosts_file()?;
    let connection =
        RemoteConnection { machine: node.display_name().to_owned(), relay, known_hosts_file };
    let remote_kit = resolve_remote_kit(processes, &connection).await?;
    Ok(Resolution::Ready(RemoteTarget { connection, remote_kit }))
}

async fn resolve_remote_kit(
    processes: &ProcessSupervisor,
    connection: &RemoteConnection,
) -> Result<String> {
    let result = invoke_connection_bytes_with_input(
        processes,
        connection,
        &["sh", "-c", "printf '%s\\n' \"$HOME\""],
        false,
        InputPolicy::Closed,
    )
    .await?;
    let home = match result {
        RemoteCommandResult::Output(bytes) => {
            String::from_utf8(bytes).context("decode the remote home directory")?
        }
        RemoteCommandResult::Status(status) => bail!("{}", status.text()),
    };
    let home = home.trim();
    let path = Path::new(home);
    if home.is_empty()
        || home.chars().any(char::is_control)
        || !path.is_absolute()
        || path == Path::new("/")
        || path.components().any(|component| {
            matches!(component, std::path::Component::ParentDir | std::path::Component::CurDir)
        })
    {
        bail!("the remote account returned an invalid home directory");
    }
    path.join(".local/bin/kit")
        .to_str()
        .map(str::to_owned)
        .context("the remote Kit path is not UTF-8")
}

async fn tailnet_status(processes: &ProcessSupervisor) -> Result<Result<Status, ConsoleStatus>> {
    let client = TailscaleClient::new(processes.clone(), std::env::current_dir()?);
    Ok(match client.readiness().await? {
        Readiness::Ready(status) => Ok(status),
        Readiness::NeedsLogin => Err(ConsoleStatus::NeedsTailscaleLogin),
        Readiness::CliUnavailable(detail) => Err(ConsoleStatus::TailscaleCliUnavailable { detail }),
        Readiness::DaemonUnavailable(detail) => {
            Err(ConsoleStatus::TailscaleDaemonUnavailable { detail })
        }
        Readiness::PermissionDenied(detail) => {
            Err(ConsoleStatus::TailscalePermissionDenied { detail })
        }
        Readiness::Unsupported(detail) => Err(ConsoleStatus::TailscaleUnsupported { detail }),
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
    let status = invoke_status(
        processes,
        target,
        &[target.remote_kit.as_str(), "--json", "console", "status"],
        authenticate,
    )
    .await?;
    Ok(status)
}

pub(crate) async fn setup(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
) -> Result<ConsoleStatus> {
    let status = invoke_status(
        processes,
        target,
        &[target.remote_kit.as_str(), "--json", "console", "setup"],
        false,
    )
    .await?;
    if matches!(status, ConsoleStatus::KitUnavailable { .. }) {
        install_latest(processes, target).await?;
        return invoke_status(
            processes,
            target,
            &[target.remote_kit.as_str(), "--json", "console", "setup"],
            false,
        )
        .await;
    }
    Ok(status)
}

pub(crate) async fn stop(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
    force: bool,
) -> Result<ConsoleStatus> {
    let mut arguments = vec![target.remote_kit.as_str(), "--json", "console", "stop"];
    if force {
        arguments.push("--force");
    }
    invoke_status(processes, target, &arguments, false).await
}

pub(crate) async fn restart(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
    force: bool,
) -> Result<ConsoleStatus> {
    let mut arguments = vec![target.remote_kit.as_str(), "--json", "console", "restart"];
    if force {
        arguments.push("--force");
    }
    invoke_status(processes, target, &arguments, false).await
}

pub(crate) async fn update(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
) -> Result<UpdateOutcome> {
    match invoke_bytes(processes, target, &[target.remote_kit.as_str(), "--json", "update"], false)
        .await?
    {
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
        machine: target.connection.machine.clone(),
        identity: target.connection.relay.clone(),
        known_hosts_file: target.connection.known_hosts_file.clone(),
        remote_kit: target.remote_kit.clone(),
        authenticate_next: AtomicBool::new(true),
        status,
    });
    let relay = match SshRelay::start(processes, provider).await {
        Ok(relay) => relay,
        Err(error) => return Err(initial_relay_error(&receiver, error)),
    };
    Ok(RemoteRelay { relay, status: receiver })
}

fn initial_relay_error(
    status: &watch::Receiver<Option<ConsoleStatus>>,
    error: SshRelayError,
) -> anyhow::Error {
    match status.borrow().clone() {
        Some(status) => anyhow::anyhow!(status.text()),
        None => error.into(),
    }
}

#[async_trait]
impl RelayEpochProvider for RemoteEpochProvider {
    async fn prepare(&self) -> Result<PreparedRelayEpoch, RelayEpochFailure> {
        match self.prepare_status().await {
            Ok(target) => {
                self.status.send_replace(None);
                let remote_command = RemoteCommand::from_arguments([
                    target.remote_kit.as_str(),
                    "console",
                    "__bridge",
                ])
                .map_err(|_| RelayEpochFailure::Start)?;
                let arguments = target
                    .connection
                    .relay
                    .openssh_arguments(
                        SshConnectionKind::Relay,
                        &remote_command,
                        &target.connection.known_hosts_file,
                    )
                    .map_err(|_| RelayEpochFailure::Start)?;
                Ok(PreparedRelayEpoch::new(arguments))
            }
            Err(status) => {
                self.status.send_replace(Some(status));
                Err(RelayEpochFailure::Preflight)
            }
        }
    }
}

impl RemoteEpochProvider {
    async fn prepare_status(&self) -> Result<RemoteTarget, ConsoleStatus> {
        let tailnet = tailnet_status(&self.processes)
            .await
            .map_err(|_| transport_failed_for(&self.machine))??;
        let node = tailnet
            .resolve_peer(self.identity.stable_node_id())
            .map_err(|_| transport_failed_for(&self.machine))?;
        if !node.online {
            return Err(ConsoleStatus::PeerOffline { machine: node.display_name().to_owned() });
        }
        let address =
            node.addresses.first().copied().ok_or_else(|| transport_failed_for(&self.machine))?;
        let relay = self
            .identity
            .with_tailscale_ip(address)
            .map_err(|_| transport_failed_for(&self.machine))?;
        let connection = RemoteConnection {
            machine: node.display_name().to_owned(),
            relay,
            known_hosts_file: self.known_hosts_file.clone(),
        };
        let target = RemoteTarget { connection, remote_kit: self.remote_kit.clone() };
        let authenticate = self.authenticate_next.swap(false, Ordering::AcqRel);
        let status = status_with_authentication(&self.processes, &target, authenticate)
            .await
            .map_err(|_| transport_failed_for(&self.machine))?;
        if status.ready() {
            Ok(target)
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
                    &target.connection.machine,
                    RemoteFailureKind::EmptyOutput,
                    "the remote command exited successfully without JSON output".to_owned(),
                ));
            }
            match serde_json::from_slice(&bytes) {
                Ok(status) => Ok(status),
                Err(error) => Ok(remote_failure(
                    &target.connection.machine,
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
    invoke_connection_bytes_with_input(
        processes,
        &target.connection,
        remote_arguments,
        authenticate,
        InputPolicy::Closed,
    )
    .await
}

async fn invoke_bytes_with_input(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
    remote_arguments: &[&str],
    authenticate: bool,
    input: InputPolicy,
) -> Result<RemoteCommandResult> {
    invoke_connection_bytes_with_input(
        processes,
        &target.connection,
        remote_arguments,
        authenticate,
        input,
    )
    .await
}

async fn invoke_connection_bytes_with_input(
    processes: &ProcessSupervisor,
    connection: &RemoteConnection,
    remote_arguments: &[&str],
    authenticate: bool,
    input: InputPolicy,
) -> Result<RemoteCommandResult> {
    let remote_command = RemoteCommand::from_arguments(remote_arguments.iter().copied())?;
    let stdout = OutputPolicy::Capture(CapturePolicy::new(
        STATUS_CAPTURE_BYTES,
        CaptureOverflow::FailAndTerminate,
    ));
    let spec = connection.relay.command_process_spec(
        &remote_command,
        &std::env::current_dir()?,
        format!("inspect Console on {}", connection.machine),
        input,
        stdout,
        OutputPolicy::Stream(StreamPolicy::new(STDERR_STREAM_BYTES)),
    )?;
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
            return Ok(RemoteCommandResult::status(remote_failure(
                &connection.machine,
                kind,
                detail,
            )));
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
                    if let Some(status) =
                        ssh_authentication_status(&connection.machine, &stderr_bytes)
                    {
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
                    return Ok(RemoteCommandResult::status(transport_failed_for(
                        &connection.machine,
                    )));
                }
            },
            report = &mut wait => break report,
        }
    };
    let report = match report {
        Ok(report) => report,
        Err(failure) => {
            if let Some(status) = ssh_authentication_status(&connection.machine, &stderr_bytes) {
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
            return Ok(RemoteCommandResult::status(remote_failure(
                &connection.machine,
                kind,
                detail,
            )));
        }
    };
    if report.leader_exit != LeaderExitObservation::Observed(LeaderExit::Code(0)) {
        if let Some(status) = ssh_authentication_status(&connection.machine, &stderr_bytes) {
            return Ok(RemoteCommandResult::status(status));
        }
        let detail = String::from_utf8_lossy(&stderr_bytes).trim().to_owned();
        if remote_arguments.first().is_some_and(|command| command.ends_with("kit"))
            && kit_command_missing(&detail)
        {
            return Ok(RemoteCommandResult::status(ConsoleStatus::KitUnavailable {
                machine: connection.machine.clone(),
            }));
        }
        let kind = ssh_exit_failure_kind(&detail);
        return Ok(RemoteCommandResult::status(remote_failure(
            &connection.machine,
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

async fn install_latest(processes: &ProcessSupervisor, target: &RemoteTarget) -> Result<()> {
    let identity = super::build_identity()?;
    if identity.source_dirty == Some(true) {
        bail!(
            "this Kit build has uncommitted source changes and has no exact release artifact; \
             install a released Kit build locally before bootstrapping a peer"
        );
    }
    let release_target = release_target(processes, target).await?;
    let staged = StagedBinary::download(&identity.version, &release_target).await?;
    let bytes = tokio::fs::read(staged.path())
        .await
        .with_context(|| format!("read staged Kit binary {}", staged.path().display()))?;
    let remote_bin = Path::new(&target.remote_kit)
        .parent()
        .and_then(Path::to_str)
        .context("resolve the remote Kit bin directory")?
        .to_owned();
    let installing = format!("{}.installing", target.remote_kit);
    let dd_output = format!("of={installing}");

    require_remote_success(
        invoke_bytes(processes, target, &["mkdir", "-p", remote_bin.as_str()], false).await?,
        "create the remote Kit bin directory",
    )?;
    require_remote_success(
        invoke_bytes_with_input(
            processes,
            target,
            &["dd", dd_output.as_str(), "bs=65536"],
            false,
            InputPolicy::Once(PrivateBytes::new(bytes)),
        )
        .await?,
        "transfer the verified Kit binary",
    )?;
    require_remote_success(
        invoke_bytes(processes, target, &["chmod", "755", installing.as_str()], false).await?,
        "mark the remote Kit binary executable",
    )?;
    require_remote_success(
        invoke_bytes(
            processes,
            target,
            &["mv", installing.as_str(), target.remote_kit.as_str()],
            false,
        )
        .await?,
        "publish the remote Kit binary",
    )
}

async fn release_target(processes: &ProcessSupervisor, target: &RemoteTarget) -> Result<String> {
    let operating_system = remote_text(processes, target, &["uname", "-s"]).await?;
    let architecture = remote_text(processes, target, &["uname", "-m"]).await?;
    match (operating_system.as_str(), architecture.as_str()) {
        ("Darwin", "arm64" | "aarch64") => Ok("aarch64-apple-darwin".to_owned()),
        ("Darwin", "x86_64") => Ok("x86_64-apple-darwin".to_owned()),
        ("Linux", "aarch64" | "arm64") => Ok("aarch64-unknown-linux-gnu".to_owned()),
        ("Linux", "x86_64") => Ok("x86_64-unknown-linux-gnu".to_owned()),
        _ => {
            bail!("Kit has no release binary for remote platform {operating_system} {architecture}")
        }
    }
}

async fn remote_text(
    processes: &ProcessSupervisor,
    target: &RemoteTarget,
    arguments: &[&str],
) -> Result<String> {
    match invoke_bytes(processes, target, arguments, false).await? {
        RemoteCommandResult::Output(bytes) => {
            let text = String::from_utf8(bytes).context("decode remote platform output")?;
            let text = text.trim();
            if text.is_empty() {
                bail!("remote platform command returned no output");
            }
            Ok(text.to_owned())
        }
        RemoteCommandResult::Status(status) => bail!("{}", status.text()),
    }
}

fn require_remote_success(result: RemoteCommandResult, operation: &str) -> Result<()> {
    match result {
        RemoteCommandResult::Output(_) => Ok(()),
        RemoteCommandResult::Status(status) => bail!("{operation}: {}", status.text()),
    }
}

fn kit_command_missing(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("command not found: kit")
        || stderr.contains("kit: command not found")
        || stderr.contains("kit: not found")
        || stderr.contains(".local/bin/kit: no such file")
}

struct StagedBinary {
    directory: PathBuf,
    path: PathBuf,
}

impl StagedBinary {
    async fn download(version: &str, target: &str) -> Result<Self> {
        let directory = super::runtime::directory()?.join(format!(
            "bootstrap-{}-{}",
            std::process::id(),
            NEXT_BOOTSTRAP.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory)
            .with_context(|| format!("create Kit bootstrap directory {}", directory.display()))?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect Kit bootstrap directory {}", directory.display()))?;
        let path = directory.join("kit");
        if let Err(error) =
            ReleaseUpdater::new().download_verified_binary(version, target, &path).await
        {
            let _ = std::fs::remove_dir_all(&directory);
            return Err(error);
        }
        let metadata = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("inspect staged Kit binary {}", path.display()))?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_RELEASE_BINARY_BYTES {
            let _ = std::fs::remove_dir_all(&directory);
            bail!("the staged Kit release binary has an invalid size");
        }
        Ok(Self { directory, path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagedBinary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn ssh_authentication_status(machine: &str, stderr: &[u8]) -> Option<ConsoleStatus> {
    let stderr = String::from_utf8_lossy(stderr);
    let url = find_login_url(&stderr)?;
    Some(ConsoleStatus::NeedsSshAuthentication {
        machine: machine.to_owned(),
        url: url.as_str().to_owned(),
    })
}

fn ssh_exit_failure_kind(stderr: &str) -> RemoteFailureKind {
    let stderr = stderr.to_ascii_lowercase();
    if stderr.contains("remote host identification has changed")
        || stderr.contains("host key verification failed")
    {
        RemoteFailureKind::HostKeyMismatch
    } else if [
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

fn transport_failed_for(machine: &str) -> ConsoleStatus {
    remote_failure(
        machine,
        RemoteFailureKind::Transport,
        "OpenSSH could not reach the machine".to_owned(),
    )
}

fn remote_failure(machine: &str, kind: RemoteFailureKind, detail: String) -> ConsoleStatus {
    let stage = match kind {
        RemoteFailureKind::OpenSshUnavailable
        | RemoteFailureKind::HostKeyMismatch
        | RemoteFailureKind::Transport => ConsoleStage::Transport,
        RemoteFailureKind::RemoteCommand | RemoteFailureKind::EmptyOutput => {
            ConsoleStage::RemoteCommand
        }
        RemoteFailureKind::Decode => ConsoleStage::Decode,
        RemoteFailureKind::Timeout | RemoteFailureKind::Supervision => ConsoleStage::Supervision,
    };
    ConsoleStatus::RemoteFailure { machine: machine.to_owned(), stage, kind, detail }
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
    use super::{
        initial_relay_error, split_selector, ssh_authentication_status, ssh_exit_failure_kind,
        ConsoleStatus, RemoteFailureKind, SshRelayError,
    };

    #[test]
    fn selector_accepts_machine_or_explicit_user_at_machine() {
        assert_eq!(split_selector("remote-node").unwrap(), (None, "remote-node"));
        assert_eq!(
            split_selector("remote-user@remote-node").unwrap(),
            (Some("remote-user"), "remote-node")
        );
        assert!(split_selector("@remote-node").is_err());
        assert!(split_selector("remote-user@").is_err());
        assert!(split_selector("a@b@c").is_err());
    }

    #[test]
    fn only_a_strict_tailscale_login_url_becomes_authentication_state() {
        let status = ssh_authentication_status(
            "mac",
            b"authenticate: https://login.tailscale.com/a/verified-token\n",
        );
        assert!(matches!(
            status,
            Some(ConsoleStatus::NeedsSshAuthentication { machine, url, .. })
                if machine == "mac"
                    && url == "https://login.tailscale.com/a/verified-token"
        ));
        assert!(ssh_authentication_status(
            "mac",
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
            ssh_exit_failure_kind("@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @"),
            RemoteFailureKind::HostKeyMismatch
        );
        assert_eq!(
            ssh_exit_failure_kind("Error: Decode limit exceeded for containers: 65536"),
            RemoteFailureKind::RemoteCommand
        );
    }

    #[test]
    fn initial_relay_failure_preserves_the_typed_preflight_status() {
        let (_sender, receiver) = tokio::sync::watch::channel(Some(ConsoleStatus::PeerOffline {
            machine: "ari-mac-1".to_owned(),
        }));
        let error = initial_relay_error(&receiver, SshRelayError::InitialPreflight);
        let message = error.to_string();

        assert!(message.contains("offline — ari-mac-1"));
        assert!(!message.contains("initial Console relay preflight failed"));
    }
}
