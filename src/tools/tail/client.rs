use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use zeroize::Zeroizing;

use crate::framework::process::{
    CaptureOverflow, CapturePolicy, CommandSpec, ContainmentRequirement, EnvironmentBase,
    InputPolicy, LeaderExit, LeaderExitObservation, OutputPolicy, PrivateBytes, ProcessByteEvent,
    ProcessDeadline, ProcessEnvironment, ProcessLabel, ProcessOutputHandle, ProcessReport,
    ProcessSpec, ProcessSupervisor, StreamPolicy, TerminationPolicy,
};
use tokio::sync::{mpsc, watch};

use super::model::{RawStatus, Readiness};

const CAPTURE_BYTES: NonZeroUsize = NonZeroUsize::new(8 * 1024 * 1024).unwrap();
const CANCEL_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct TailClient {
    processes: ProcessSupervisor,
    working_directory: PathBuf,
    executable: OsString,
}

#[derive(Clone, Debug)]
pub enum LoginEvent {
    Url(String),
    Ready(Readiness),
    Failed(String),
    Cancelled,
}

impl TailClient {
    pub fn new(processes: ProcessSupervisor, working_directory: PathBuf) -> Self {
        Self { processes, working_directory, executable: OsString::from("tailscale") }
    }

    #[cfg(test)]
    fn with_executable(
        processes: ProcessSupervisor,
        working_directory: PathBuf,
        executable: PathBuf,
    ) -> Self {
        Self { processes, working_directory, executable: executable.into_os_string() }
    }

    pub async fn readiness(&self) -> Result<Readiness> {
        let output = self.capture("tailscale status", ["status", "--json"]).await;
        match output {
            Ok(bytes) => {
                let status: RawStatus =
                    serde_json::from_slice(&bytes).context("parse tailscale status")?;
                let mut readiness = status.readiness();
                if let Readiness::Ready { peers, .. } = &mut readiness {
                    if let Ok(targets) =
                        self.capture("list Taildrop targets", ["file", "cp", "--targets"]).await
                    {
                        reconcile_targets(peers, &targets);
                    }
                }
                Ok(readiness)
            }
            Err(error) => Ok(classify_preflight_error(&error)),
        }
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

    pub async fn send_text(
        &self,
        name: &str,
        text: Zeroizing<String>,
        target: &str,
        cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        let args = vec![
            OsString::from("file"),
            OsString::from("cp"),
            OsString::from(format!("--name={name}")),
            OsString::from("-"),
            OsString::from(format!("{target}:")),
        ];
        self.status_cancellable(
            "send Taildrop text",
            args,
            InputPolicy::Once(PrivateBytes::new(text.as_bytes().to_vec())),
            cancel,
        )
        .await
    }

    pub async fn send_files(
        &self,
        paths: &[PathBuf],
        target: &str,
        cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        if paths.is_empty() {
            bail!("no files selected");
        }
        let mut args = vec![OsString::from("file"), OsString::from("cp")];
        args.extend(paths.iter().map(|path| path.as_os_str().to_owned()));
        args.push(OsString::from(format!("{target}:")));
        self.status_cancellable("send Taildrop files", args, InputPolicy::Closed, cancel).await
    }

    pub async fn receive_into(
        &self,
        directory: &Path,
        wait: bool,
        cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        let mut args = vec![
            OsString::from("file"),
            OsString::from("get"),
            OsString::from("--conflict=rename"),
        ];
        if wait {
            args.push(OsString::from("--wait"));
        }
        args.push(directory.as_os_str().to_owned());
        self.status_cancellable("receive Taildrop files", args, InputPolicy::Closed, cancel).await
    }

    async fn capture<const N: usize>(&self, label: &str, args: [&str; N]) -> Result<Vec<u8>> {
        let args = args.into_iter().map(OsString::from).collect();
        let report = self.run(label, args, InputPolicy::Closed, capture(), capture()).await?;
        ensure_success(&report).with_context(|| captured_stderr(&report))?;
        match report.stdout {
            crate::framework::process::OutputReport::Captured(output) => {
                Ok(output.bytes.into_vec())
            }
            _ => bail!("{label} stdout was not captured"),
        }
    }

    async fn status_cancellable(
        &self,
        label: &str,
        args: Vec<OsString>,
        input: InputPolicy,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        let spec = self.process_spec(label, args, input, OutputPolicy::Discard, capture())?;
        let started = self.processes.spawn(spec).await?;
        let control = started.session.control();
        let wait = started.session.wait();
        tokio::pin!(wait);
        let (report, cancelled) = tokio::select! {
            report = &mut wait => (report, false),
            changed = cancel.changed() => {
                let _ = changed;
                control.cancel().await?;
                (wait.await, true)
            }
        };
        let report = report.map_err(|failure| {
            anyhow::anyhow!("{label} supervision failed: {:?}", failure.failure)
        })?;
        if cancelled {
            bail!("operation cancelled");
        }
        ensure_success(&report).with_context(|| captured_stderr(&report))
    }

    async fn run(
        &self,
        label: &str,
        args: Vec<OsString>,
        input: InputPolicy,
        stdout: OutputPolicy,
        stderr: OutputPolicy,
    ) -> Result<ProcessReport> {
        let spec = self.process_spec(label, args, input, stdout, stderr)?;
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
        input: InputPolicy,
        stdout: OutputPolicy,
        stderr: OutputPolicy,
    ) -> Result<ProcessSpec> {
        let environment =
            ProcessEnvironment::new(EnvironmentBase::Inherit, BTreeMap::new(), BTreeSet::new())?;
        let command = CommandSpec::new(
            self.executable.clone(),
            args,
            self.working_directory.clone(),
            environment,
            ProcessLabel::new(label.to_owned())?,
        )?;
        Ok(ProcessSpec::new(
            command,
            input,
            stdout,
            stderr,
            ContainmentRequirement::ExplicitProcessGroup,
            ProcessDeadline::Unlimited,
            TerminationPolicy::new(CANCEL_GRACE),
        ))
    }

    async fn run_login(
        &self,
        sender: mpsc::Sender<LoginEvent>,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        let stream = OutputPolicy::Stream(StreamPolicy::new(NonZeroUsize::new(64 * 1024).unwrap()));
        let spec = self.process_spec(
            "authenticate Tailscale",
            vec![OsString::from("login")],
            InputPolicy::Closed,
            stream,
            stream,
        )?;
        let started = self.processes.spawn(spec).await?;
        let mut stdout = byte_stream(started.stdout)?;
        let mut stderr = byte_stream(started.stderr)?;
        let control = started.session.control();
        let wait = started.session.wait();
        tokio::pin!(wait);
        let mut status_tick = tokio::time::interval(Duration::from_millis(750));
        let mut output = String::new();
        let mut announced_url = None;
        let mut stdout_open = true;
        let mut stderr_open = true;
        loop {
            tokio::select! {
                event = stdout.next(), if stdout_open => stdout_open = append_login_output(event?, &mut output),
                event = stderr.next(), if stderr_open => stderr_open = append_login_output(event?, &mut output),
                report = &mut wait => {
                    let report = report.map_err(|failure| anyhow::anyhow!("login supervision failed: {:?}", failure.failure))?;
                    ensure_success(&report)?;
                    match self.readiness().await? {
                        readiness @ Readiness::Ready { .. } => {
                            let _ = sender.send(LoginEvent::Ready(readiness)).await;
                            return Ok(());
                        }
                        _ => bail!("Tailscale login exited before the device became ready"),
                    }
                }
                _ = status_tick.tick() => {
                    if let Some(url) = login_url(&output) {
                        if announced_url.as_deref() != Some(url.as_str()) {
                            announced_url = Some(url.clone());
                            let _ = sender.send(LoginEvent::Url(url)).await;
                        }
                    }
                    if let readiness @ Readiness::Ready { .. } = self.readiness().await? {
                        let _ = sender.send(LoginEvent::Ready(readiness)).await;
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
}

fn byte_stream(
    handle: ProcessOutputHandle,
) -> Result<crate::framework::process::ProcessByteStream> {
    match handle {
        ProcessOutputHandle::Stream(stream) => Ok(stream),
        _ => bail!("login output was not streamed"),
    }
}

fn append_login_output(event: ProcessByteEvent, output: &mut String) -> bool {
    let ProcessByteEvent::Chunk { bytes, .. } = event else { return false };
    output.push_str(&String::from_utf8_lossy(&bytes));
    if output.len() > 64 * 1024 {
        output.drain(..output.len() - 64 * 1024);
    }
    true
}

fn login_url(output: &str) -> Option<String> {
    let start = output.find("https://login.tailscale.com/")?;
    let candidate = output[start..]
        .chars()
        .take_while(|character| !character.is_whitespace() && !character.is_control())
        .collect::<String>();
    let parsed = url::Url::parse(&candidate).ok()?;
    (parsed.scheme() == "https" && parsed.host_str() == Some("login.tailscale.com"))
        .then_some(candidate)
}

fn capture() -> OutputPolicy {
    OutputPolicy::Capture(CapturePolicy::new(CAPTURE_BYTES, CaptureOverflow::FailAndTerminate))
}

fn ensure_success(report: &ProcessReport) -> Result<()> {
    match report.leader_exit {
        LeaderExitObservation::Observed(LeaderExit::Code(0)) => Ok(()),
        exit => bail!("Tailscale CLI exited with {exit:?}"),
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

fn reconcile_targets(peers: &mut [super::model::Device], output: &[u8]) {
    for line in String::from_utf8_lossy(output).lines() {
        let mut columns = line.split('\t');
        let Some(target) = columns.next().map(str::trim).filter(|target| !target.is_empty()) else {
            continue;
        };
        let name = columns.next().map(str::trim).unwrap_or_default();
        if let Some(peer) = peers.iter_mut().find(|peer| {
            peer.addresses.iter().any(|address| address == target)
                || peer.dns_name.split('.').next() == Some(name)
        }) {
            peer.taildrop_target = Some(target.to_owned());
        }
    }
}

fn classify_preflight_error(error: &anyhow::Error) -> Readiness {
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
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::*;

    #[test]
    fn extracts_login_url_from_mixed_output() {
        assert_eq!(
            login_url("authenticate here:\nhttps://login.tailscale.com/a/abc\nwaiting"),
            Some("https://login.tailscale.com/a/abc".to_owned())
        );
    }

    #[test]
    fn rejects_lookalike_login_hosts() {
        assert_eq!(login_url("https://login.tailscale.com.evil/a"), None);
    }

    #[test]
    fn reconciles_authoritative_taildrop_targets_by_ip() {
        let mut peers = vec![super::super::model::Device {
            id: "peer".into(),
            name: "Laptop".into(),
            dns_name: "laptop.example.ts.net".into(),
            os: "macOS".into(),
            online: true,
            addresses: vec!["100.64.0.2".into()],
            taildrop_target: None,
        }];
        reconcile_targets(&mut peers, b"100.64.0.2\tlaptop\n");
        assert_eq!(peers[0].send_target(), Some("100.64.0.2"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_cli_proves_readiness_targets_and_private_text_stdin() {
        let root = std::env::temp_dir().join(format!("kit-tail-client-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let executable = root.join("tailscale");
        let stdin_path = root.join("stdin");
        let args_path = root.join("args");
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = "status" ]; then
  printf '%s\n' '{{"BackendState":"Running","TailscaleIPs":["100.64.0.1"],"Self":{{"ID":"me","DNSName":"desktop.test.ts.net.","HostName":"desktop","OS":"linux","Online":true,"TailscaleIPs":["100.64.0.1"]}},"Peer":{{"peer":{{"ID":"peer","DNSName":"laptop.test.ts.net.","HostName":"laptop","OS":"linux","Online":true,"TailscaleIPs":["100.64.0.2"]}}}}}}'
elif [ "$1" = "file" ] && [ "$2" = "cp" ] && [ "$3" = "--targets" ]; then
  printf '100.64.0.2\tlaptop\n'
elif [ "$1" = "file" ] && [ "$2" = "cp" ]; then
  cat > '{}'
  printf '%s\n' "$@" > '{}'
else
  exit 64
fi
"#,
            stdin_path.display(),
            args_path.display(),
        );
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let processes = ProcessSupervisor::for_test(root.join("processes")).unwrap();
        let client = TailClient::with_executable(processes, root.clone(), executable);

        let Readiness::Ready { peers, .. } = client.readiness().await.unwrap() else {
            panic!("fake CLI was not ready")
        };
        assert_eq!(peers[0].send_target(), Some("100.64.0.2"));
        let (_cancel, cancel_receiver) = watch::channel(false);
        client
            .send_text(
                "clipboard.txt",
                Zeroizing::new("hello from stdin".into()),
                "100.64.0.2",
                cancel_receiver,
            )
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(stdin_path).unwrap(), "hello from stdin");
        let args = fs::read_to_string(args_path).unwrap();
        assert!(args.contains("--name=clipboard.txt"));
        assert!(args.contains("100.64.0.2:"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_login_stays_alive_until_readiness_and_is_reaped() {
        let root = std::env::temp_dir().join(format!("kit-tail-login-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let executable = root.join("tailscale");
        let ready_path = root.join("ready");
        let pid_path = root.join("pid");
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = "status" ]; then
  if [ -f '{}' ]; then
    printf '%s\n' '{{"BackendState":"Running","TailscaleIPs":["100.64.0.1"],"Self":{{"ID":"me","DNSName":"desktop.test.ts.net.","HostName":"desktop","OS":"linux","Online":true,"TailscaleIPs":["100.64.0.1"]}}}}'
  else
    printf '%s\n' '{{"BackendState":"NeedsLogin"}}'
  fi
elif [ "$1" = "login" ]; then
  printf '%s\n' 'https://login.tailscale.com/a/test-token' >&2
  printf '%s\n' "$$" > '{}'
  while [ ! -f '{}' ]; do sleep 1; done
elif [ "$1" = "file" ] && [ "$2" = "cp" ] && [ "$3" = "--targets" ]; then
  exit 0
else
  exit 64
fi
"#,
            ready_path.display(),
            pid_path.display(),
            ready_path.display(),
        );
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let processes = ProcessSupervisor::for_test(root.join("processes")).unwrap();
        let client = TailClient::with_executable(processes, root.clone(), executable);
        let (mut events, _cancel, task) = client.start_login();

        let url =
            tokio::time::timeout(Duration::from_secs(3), events.recv()).await.unwrap().unwrap();
        assert!(
            matches!(url, LoginEvent::Url(ref url) if url == "https://login.tailscale.com/a/test-token")
        );
        let pid = wait_for_pid(&pid_path).await;
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        fs::write(&ready_path, "ready").unwrap();
        let ready =
            tokio::time::timeout(Duration::from_secs(4), events.recv()).await.unwrap().unwrap();
        assert!(matches!(ready, LoginEvent::Ready(Readiness::Ready { .. })));
        task.await.unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_login_cancellation_is_acknowledged_after_reaping() {
        let root = std::env::temp_dir().join(format!("kit-tail-cancel-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let executable = root.join("tailscale");
        let pid_path = root.join("pid");
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = "status" ]; then
  printf '%s\n' '{{"BackendState":"NeedsLogin"}}'
elif [ "$1" = "login" ]; then
  printf '%s\n' 'https://login.tailscale.com/a/cancel-token'
  printf '%s\n' "$$" > '{}'
  while true; do sleep 1; done
else
  exit 64
fi
"#,
            pid_path.display(),
        );
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let processes = ProcessSupervisor::for_test(root.join("processes")).unwrap();
        let client = TailClient::with_executable(processes, root.clone(), executable);
        let (mut events, cancel, task) = client.start_login();

        let url =
            tokio::time::timeout(Duration::from_secs(3), events.recv()).await.unwrap().unwrap();
        assert!(matches!(url, LoginEvent::Url(_)));
        let pid = fs::read_to_string(&pid_path).unwrap().trim().parse::<i32>().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        cancel.send(true).unwrap();
        let cancelled =
            tokio::time::timeout(Duration::from_secs(4), events.recv()).await.unwrap().unwrap();
        assert!(matches!(cancelled, LoginEvent::Cancelled));
        task.await.unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_transfer_cancellation_returns_only_after_reaping() {
        let root = std::env::temp_dir().join(format!("kit-tail-send-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let executable = root.join("tailscale");
        let pid_path = root.join("pid");
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = "file" ] && [ "$2" = "cp" ]; then
  printf '%s\n' "$$" > '{}'
  while true; do sleep 1; done
else
  exit 64
fi
"#,
            pid_path.display(),
        );
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let processes = ProcessSupervisor::for_test(root.join("processes")).unwrap();
        let client = TailClient::with_executable(processes, root.clone(), executable);
        let payload = root.join("payload.txt");
        fs::write(&payload, "payload").unwrap();
        let (cancel, cancel_receiver) = watch::channel(false);
        let transfer = tokio::spawn({
            let client = client.clone();
            let payload = payload.clone();
            async move { client.send_files(&[payload], "100.64.0.2", cancel_receiver).await }
        });
        let pid = wait_for_pid(&pid_path).await;
        cancel.send(true).unwrap();
        let error = transfer.await.unwrap().unwrap_err();
        assert!(format!("{error:#}").contains("operation cancelled"));
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    async fn wait_for_pid(path: &Path) -> i32 {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(pid) = fs::read_to_string(path).and_then(|contents| {
                    contents.trim().parse().map_err(|error| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                    })
                }) {
                    return pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fake CLI did not publish a valid PID")
    }
}
