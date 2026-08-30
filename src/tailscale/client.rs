use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    net::{IpAddr, SocketAddr},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use tokio::sync::{mpsc, watch};

use crate::framework::process::{
    CaptureOverflow, CapturePolicy, CommandSpec, CompletionCause, ContainmentRequirement,
    EnvironmentBase, InputPolicy, LeaderExit, LeaderExitObservation, OutputPolicy,
    ProcessByteEvent, ProcessDeadline, ProcessEnvironment, ProcessLabel, ProcessOutputHandle,
    ProcessReport, ProcessSpec, ProcessSupervisor, StreamPolicy, TerminationPolicy,
};

use super::{
    find_login_url, parse_status, parse_whois, LoginUrl, Readiness, Status, WhoIsIdentity,
};

const CAPTURE_BYTES: NonZeroUsize = NonZeroUsize::new(8 * 1024 * 1024).unwrap();
const LOGIN_OUTPUT_BYTES: usize = 64 * 1024;
const CANCEL_GRACE: Duration = Duration::from_secs(2);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const MACOS_APP_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";
const MACOS_CLI_CANDIDATES: [&str; 3] =
    [MACOS_APP_CLI, "/usr/local/bin/tailscale", "/opt/homebrew/bin/tailscale"];
const MACOS_CLI_MODE: (&str, &str) = ("TAILSCALE_BE_CLI", "1");

#[derive(Clone)]
pub struct TailscaleClient {
    processes: ProcessSupervisor,
    working_directory: PathBuf,
    executable: OsString,
    environment: BTreeMap<OsString, OsString>,
}

#[derive(Clone, Debug)]
pub enum LoginEvent {
    Url(LoginUrl),
    Ready(Status),
    Failed(String),
    Cancelled,
}

impl TailscaleClient {
    pub fn new(processes: ProcessSupervisor, working_directory: PathBuf) -> Self {
        let (executable, environment) = default_invocation();
        Self { processes, working_directory, executable, environment }
    }

    /// Constructs a client around a caller-selected executable for deterministic process fixtures.
    pub fn with_executable(
        processes: ProcessSupervisor,
        working_directory: PathBuf,
        executable: impl AsRef<OsStr>,
    ) -> Self {
        Self {
            processes,
            working_directory,
            executable: executable.as_ref().to_owned(),
            environment: BTreeMap::new(),
        }
    }

    pub async fn readiness(&self) -> Result<Readiness> {
        match self.capture_status().await {
            Ok(bytes) => parse_status(&bytes).context("classify tailscale status"),
            Err(error) => Ok(classify_status_error(&error)),
        }
    }

    pub async fn whois(&self, source: SocketAddr) -> Result<WhoIsIdentity> {
        let bytes = self
            .capture(
                "tailscale whois",
                vec![
                    OsString::from("whois"),
                    OsString::from("--json"),
                    OsString::from("--proto=tcp"),
                    OsString::from(source.to_string()),
                ],
            )
            .await?;
        parse_whois(&bytes).context("project tailscale whois identity")
    }

    /// Builds the canonical CLI command for a caller-supervised tailnet TCP stream.
    pub(crate) fn nc_command(
        &self,
        address: IpAddr,
        port: u16,
        label: impl Into<String>,
    ) -> Result<CommandSpec> {
        if port == 0 {
            bail!("tailscale nc port cannot be zero");
        }
        if !is_tailscale_address(address) {
            bail!("tailscale nc address {address} is outside the Tailscale address ranges");
        }
        self.command(
            label,
            vec![
                OsString::from("nc"),
                OsString::from(address.to_string()),
                OsString::from(port.to_string()),
            ],
        )
    }

    pub fn start_login(
        &self,
    ) -> (mpsc::Receiver<LoginEvent>, watch::Sender<bool>, tokio::task::JoinHandle<()>) {
        let (sender, receiver) = mpsc::channel(8);
        let (cancel, cancel_receiver) = watch::channel(false);
        let client = self.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = client.run_login(sender.clone(), cancel_receiver).await {
                let _ = sender.send(LoginEvent::Failed(format!("{error:#}"))).await;
            }
        });
        (receiver, cancel, task)
    }

    async fn capture_status(&self) -> Result<Vec<u8>> {
        self.capture("tailscale status", vec![OsString::from("status"), OsString::from("--json")])
            .await
    }

    async fn capture(&self, label: &str, args: Vec<OsString>) -> Result<Vec<u8>> {
        let report = self
            .run(
                label,
                args,
                OutputPolicy::Capture(CapturePolicy::new(
                    CAPTURE_BYTES,
                    CaptureOverflow::FailAndTerminate,
                )),
                OutputPolicy::Capture(CapturePolicy::new(
                    CAPTURE_BYTES,
                    CaptureOverflow::FailAndTerminate,
                )),
            )
            .await?;
        ensure_success(&report).with_context(|| captured_stderr(&report))?;
        match report.stdout {
            crate::framework::process::OutputReport::Captured(output) => {
                Ok(output.bytes.into_vec())
            }
            _ => bail!("{label} stdout was not captured"),
        }
    }

    async fn run_login(
        &self,
        sender: mpsc::Sender<LoginEvent>,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        let stream = OutputPolicy::Stream(StreamPolicy::new(
            NonZeroUsize::new(LOGIN_OUTPUT_BYTES).expect("non-zero login output bound"),
        ));
        let spec = self.process_spec(
            "authenticate Tailscale",
            vec![OsString::from("login")],
            stream,
            stream,
            ProcessDeadline::Unlimited,
        )?;
        let started = self.processes.spawn(spec).await?;
        let mut stdout = byte_stream(started.stdout)?;
        let mut stderr = byte_stream(started.stderr)?;
        let control = started.session.control();
        let wait = started.session.wait();
        tokio::pin!(wait);
        let mut status_tick = tokio::time::interval(Duration::from_millis(750));
        let mut stdout_output = Vec::new();
        let mut stderr_output = Vec::new();
        let mut announced_url = None;
        let mut stdout_open = true;
        let mut stderr_open = true;
        loop {
            tokio::select! {
                event = stdout.next(), if stdout_open => {
                    stdout_open = append_login_output(event?, &mut stdout_output);
                }
                event = stderr.next(), if stderr_open => {
                    stderr_open = append_login_output(event?, &mut stderr_output);
                }
                report = &mut wait => {
                    let report = report.map_err(|failure| {
                        anyhow::anyhow!("login supervision failed: {:?}", failure.failure)
                    })?;
                    ensure_success(&report)?;
                    match self.readiness().await? {
                        Readiness::Ready(status) => {
                            let _ = sender.send(LoginEvent::Ready(status)).await;
                            return Ok(());
                        }
                        _ => bail!("Tailscale login exited before the device became ready"),
                    }
                }
                _ = status_tick.tick() => {
                    let url = find_login_url(&String::from_utf8_lossy(&stdout_output))
                        .or_else(|| find_login_url(&String::from_utf8_lossy(&stderr_output)));
                    if let Some(url) = url {
                        if announced_url.as_ref() != Some(&url) {
                            announced_url = Some(url.clone());
                            let _ = sender.send(LoginEvent::Url(url)).await;
                        }
                    }
                    if let Readiness::Ready(status) = self.readiness().await? {
                        let _ = sender.send(LoginEvent::Ready(status)).await;
                        let _ = control.cancel().await;
                        let _ = wait.await;
                        return Ok(());
                    }
                }
                changed = cancel.changed() => {
                    let _ = changed;
                    control.cancel().await?;
                    let _ = wait.await;
                    let _ = sender.send(LoginEvent::Cancelled).await;
                    return Ok(());
                }
            }
        }
    }

    async fn run(
        &self,
        label: &str,
        args: Vec<OsString>,
        stdout: OutputPolicy,
        stderr: OutputPolicy,
    ) -> Result<ProcessReport> {
        let spec = self.process_spec(
            label,
            args,
            stdout,
            stderr,
            ProcessDeadline::After(COMMAND_TIMEOUT),
        )?;
        self.processes
            .spawn(spec)
            .await?
            .session
            .wait()
            .await
            .map_err(|failure| anyhow::anyhow!("{label} supervision failed: {:?}", failure.failure))
    }

    fn process_spec(
        &self,
        label: &str,
        args: Vec<OsString>,
        stdout: OutputPolicy,
        stderr: OutputPolicy,
        deadline: ProcessDeadline,
    ) -> Result<ProcessSpec> {
        let command = self.command(label, args)?;
        Ok(ProcessSpec::new(
            command,
            InputPolicy::Closed,
            stdout,
            stderr,
            ContainmentRequirement::ExplicitProcessGroup,
            deadline,
            TerminationPolicy::new(CANCEL_GRACE),
        ))
    }

    fn command(&self, label: impl Into<String>, args: Vec<OsString>) -> Result<CommandSpec> {
        let environment = ProcessEnvironment::new(
            EnvironmentBase::Inherit,
            self.environment.clone(),
            BTreeSet::new(),
        )?;
        Ok(CommandSpec::new(
            self.executable.clone(),
            args,
            self.working_directory.clone(),
            environment,
            ProcessLabel::new(label.into())?,
        )?)
    }
}

fn default_invocation() -> (OsString, BTreeMap<OsString, OsString>) {
    select_default_invocation(cfg!(target_os = "macos"), Path::is_file)
}

fn select_default_invocation(
    macos: bool,
    exists: impl Fn(&Path) -> bool,
) -> (OsString, BTreeMap<OsString, OsString>) {
    if !macos {
        return (OsString::from("tailscale"), BTreeMap::new());
    }
    let executable = MACOS_CLI_CANDIDATES
        .iter()
        .map(Path::new)
        .find(|candidate| exists(candidate))
        .map(Path::as_os_str)
        .unwrap_or_else(|| OsStr::new("tailscale"))
        .to_owned();
    let environment = [(OsString::from(MACOS_CLI_MODE.0), OsString::from(MACOS_CLI_MODE.1))]
        .into_iter()
        .collect();
    (executable, environment)
}

pub(crate) fn is_tailscale_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let [first, second, _, _] = address.octets();
            first == 100 && (64..=127).contains(&second)
        }
        IpAddr::V6(address) => address.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

fn byte_stream(
    handle: ProcessOutputHandle,
) -> Result<crate::framework::process::ProcessByteStream> {
    match handle {
        ProcessOutputHandle::Stream(stream) => Ok(stream),
        _ => bail!("login output was not streamed"),
    }
}

fn append_login_output(event: ProcessByteEvent, output: &mut Vec<u8>) -> bool {
    let ProcessByteEvent::Chunk { bytes, .. } = event else { return false };
    output.extend_from_slice(&bytes);
    if output.len() > LOGIN_OUTPUT_BYTES {
        output.drain(..output.len() - LOGIN_OUTPUT_BYTES);
    }
    true
}

fn ensure_success(report: &ProcessReport) -> Result<()> {
    match (report.completion, report.leader_exit) {
        (CompletionCause::Natural, LeaderExitObservation::Observed(LeaderExit::Code(0))) => Ok(()),
        (completion, exit) => {
            bail!("Tailscale CLI completed as {completion:?} with {exit:?}")
        }
    }
}

fn captured_stderr(report: &ProcessReport) -> String {
    match &report.stderr {
        crate::framework::process::OutputReport::Captured(output) => {
            String::from_utf8_lossy(&output.bytes).trim().to_owned()
        }
        _ => String::new(),
    }
}

fn classify_status_error(error: &anyhow::Error) -> Readiness {
    let message = format!("{error:#}");
    let normalized = message.to_lowercase();
    if normalized.contains("no such file") || normalized.contains("not found") {
        Readiness::CliUnavailable(message)
    } else if normalized.contains("permission denied") || normalized.contains("access is denied") {
        Readiness::PermissionDenied(message)
    } else {
        Readiness::DaemonUnavailable(message)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::time::Duration;

    use crate::framework::process::test_support::{CommandFixture, CommandResponse};

    use super::*;

    const READY_STATUS: &str = r#"{"BackendState":"Running","TailscaleIPs":["100.64.0.1"],"Self":{"ID":"me","UserID":1001,"DNSName":"desktop.test.ts.net.","HostName":"desktop","OS":"linux","Online":true,"TailscaleIPs":["100.64.0.1"]},"Peer":{"peer":{"ID":"peer","UserID":2002,"DNSName":"laptop.test.ts.net.","HostName":"laptop","OS":"linux","Online":true,"TailscaleIPs":["100.64.0.2"]}}}"#;
    const NEEDS_LOGIN_STATUS: &str = r#"{"BackendState":"NeedsLogin"}"#;

    #[tokio::test]
    async fn process_fixture_projects_status_json_through_the_shared_owner() {
        let mut fixture = CommandFixture::new().unwrap();
        fixture
            .respond(["status", "--json"], CommandResponse::success().stdout(READY_STATUS))
            .unwrap();
        let processes = ProcessSupervisor::for_test(fixture.root().join("processes")).unwrap();
        let client = TailscaleClient::with_executable(
            processes,
            fixture.root().to_path_buf(),
            fixture.executable(),
        );

        let Readiness::Ready(status) = client.readiness().await.unwrap() else {
            panic!("fixture was not ready")
        };
        let peer = status.resolve_peer("laptop").unwrap();
        assert_eq!(peer.id, "peer");
        assert_eq!(peer.user_id, Some(2002));
    }

    #[tokio::test]
    async fn process_fixture_invokes_exact_whois_command_and_projects_identity() {
        let mut fixture = CommandFixture::new().unwrap();
        fixture
            .respond(
                ["whois", "--json", "--proto=tcp", "100.64.0.2:4242"],
                CommandResponse::success().stdout(
                    r#"{"Node":{"StableID":"node-source","User":2002},"UserProfile":{"LoginName":"alice@example.com","DisplayName":"Alice"},"CapMap":{"kit.console.connect":[]}}"#,
                ),
            )
            .unwrap();
        let processes = ProcessSupervisor::for_test(fixture.root().join("processes")).unwrap();
        let client = TailscaleClient::with_executable(
            processes,
            fixture.root().to_path_buf(),
            fixture.executable(),
        );

        let identity = client.whois("100.64.0.2:4242".parse().unwrap()).await.unwrap();

        assert_eq!(identity.stable_node_id, "node-source");
        assert_eq!(identity.user_id, Some(2002));
        assert!(identity.has_capability("kit.console.connect"));
        assert_eq!(fixture.invocations().unwrap().len(), 1);
    }

    #[test]
    fn nc_command_uses_selected_executable_and_exact_tailnet_endpoint_arguments() {
        let fixture = CommandFixture::new().unwrap();
        let executable = fixture.executable();
        let working_directory = fixture.root().to_path_buf();
        let processes = ProcessSupervisor::for_test(fixture.root().join("processes")).unwrap();
        let client = TailscaleClient::with_executable(
            processes,
            working_directory.clone(),
            executable.clone(),
        );

        let command = client
            .nc_command("fd7a:115c:a1e0::2".parse().unwrap(), 4242, "connect Console agent")
            .unwrap();

        assert_eq!(command.program, executable.into_os_string());
        assert_eq!(command.arguments, ["nc", "fd7a:115c:a1e0::2", "4242"].map(OsString::from));
        assert_eq!(command.working_directory, working_directory);
        assert_eq!(command.label.as_str(), "connect Console agent");
    }

    #[test]
    fn nc_command_rejects_zero_port_and_non_tailscale_addresses() {
        let fixture = CommandFixture::new().unwrap();
        let processes = ProcessSupervisor::for_test(fixture.root().join("processes")).unwrap();
        let client = TailscaleClient::with_executable(
            processes,
            fixture.root().to_path_buf(),
            fixture.executable(),
        );

        assert!(client.nc_command("100.64.0.2".parse().unwrap(), 0, "invalid port").is_err());
        assert!(client
            .nc_command("192.168.1.2".parse().unwrap(), 4242, "invalid address")
            .is_err());
    }

    #[test]
    fn macos_default_invocation_prefers_the_gui_app_cli_and_enables_cli_mode() {
        let (executable, environment) =
            select_default_invocation(true, |candidate| candidate == Path::new(MACOS_APP_CLI));

        assert_eq!(executable, OsString::from(MACOS_APP_CLI));
        assert_eq!(environment.get(OsStr::new(MACOS_CLI_MODE.0)), Some(&OsString::from("1")));
    }

    #[test]
    fn non_macos_default_invocation_uses_path_without_environment_injection() {
        let (executable, environment) = select_default_invocation(false, |_| true);

        assert_eq!(executable, OsString::from("tailscale"));
        assert!(environment.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn login_lifecycle_announces_url_then_reaps_after_readiness() {
        let mut fixture = CommandFixture::new().unwrap();
        fixture
            .respond(
                ["login"],
                CommandResponse::hang().stderr("https://login.tailscale.com/a/test-token\n"),
            )
            .unwrap();
        fixture
            .respond(["status", "--json"], CommandResponse::success().stdout(NEEDS_LOGIN_STATUS))
            .unwrap();
        fixture
            .respond(["status", "--json"], CommandResponse::success().stdout(READY_STATUS))
            .unwrap();
        let processes = ProcessSupervisor::for_test(fixture.root().join("processes")).unwrap();
        let client = TailscaleClient::with_executable(
            processes,
            fixture.root().to_path_buf(),
            fixture.executable(),
        );
        let (mut events, _cancel, task) = client.start_login();

        let url =
            tokio::time::timeout(Duration::from_secs(3), events.recv()).await.unwrap().unwrap();
        assert!(
            matches!(url, LoginEvent::Url(ref url) if url.as_str() == "https://login.tailscale.com/a/test-token")
        );
        let login = fixture.wait_for_invocation(["login"], Duration::from_secs(3)).await.unwrap();
        assert_eq!(unsafe { libc::kill(login.pid as i32, 0) }, 0);
        let ready =
            tokio::time::timeout(Duration::from_secs(4), events.recv()).await.unwrap().unwrap();
        assert!(matches!(ready, LoginEvent::Ready(_)));
        task.await.unwrap();
        assert_eq!(unsafe { libc::kill(login.pid as i32, 0) }, -1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn login_cancellation_is_acknowledged_only_after_reaping() {
        let mut fixture = CommandFixture::new().unwrap();
        fixture
            .respond(
                ["login"],
                CommandResponse::hang().stdout("https://login.tailscale.com/a/cancel-token\n"),
            )
            .unwrap();
        fixture
            .respond(["status", "--json"], CommandResponse::success().stdout(NEEDS_LOGIN_STATUS))
            .unwrap();
        let processes = ProcessSupervisor::for_test(fixture.root().join("processes")).unwrap();
        let client = TailscaleClient::with_executable(
            processes,
            fixture.root().to_path_buf(),
            fixture.executable(),
        );
        let (mut events, cancel, task) = client.start_login();

        let url =
            tokio::time::timeout(Duration::from_secs(3), events.recv()).await.unwrap().unwrap();
        assert!(matches!(url, LoginEvent::Url(_)));
        let login = fixture.wait_for_invocation(["login"], Duration::from_secs(3)).await.unwrap();
        cancel.send(true).unwrap();
        let cancelled =
            tokio::time::timeout(Duration::from_secs(4), events.recv()).await.unwrap().unwrap();
        assert!(matches!(cancelled, LoginEvent::Cancelled));
        task.await.unwrap();
        assert_eq!(unsafe { libc::kill(login.pid as i32, 0) }, -1);
    }
}
