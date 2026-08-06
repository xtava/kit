use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use tokio::sync::{mpsc, watch};
use zeroize::Zeroizing;

use crate::framework::process::{
    CaptureOverflow, CapturePolicy, CommandSpec, ContainmentRequirement, EnvironmentBase,
    InputPolicy, LeaderExit, LeaderExitObservation, OutputPolicy, PrivateBytes, ProcessDeadline,
    ProcessEnvironment, ProcessLabel, ProcessReport, ProcessSpec, ProcessSupervisor,
    TerminationPolicy,
};
use crate::tailscale::{self, TailscaleClient};

use super::model::Readiness;

const CAPTURE_BYTES: NonZeroUsize = NonZeroUsize::new(8 * 1024 * 1024).unwrap();
const CANCEL_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct TailClient {
    tailscale: TailscaleClient,
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
        let tailscale = TailscaleClient::new(processes.clone(), working_directory.clone());
        Self { tailscale, processes, working_directory, executable: OsString::from("tailscale") }
    }

    #[cfg(test)]
    fn with_executable(
        processes: ProcessSupervisor,
        working_directory: PathBuf,
        executable: PathBuf,
    ) -> Self {
        let tailscale = TailscaleClient::with_executable(
            processes.clone(),
            working_directory.clone(),
            &executable,
        );
        Self { tailscale, processes, working_directory, executable: executable.into_os_string() }
    }

    pub async fn readiness(&self) -> Result<Readiness> {
        Ok(self.enrich_readiness(self.tailscale.readiness().await?).await)
    }

    pub fn start_login(
        &self,
    ) -> (mpsc::Receiver<LoginEvent>, watch::Sender<bool>, tokio::task::JoinHandle<()>) {
        let (sender, receiver) = mpsc::channel(8);
        let (mut shared_events, cancel, shared_task) = self.tailscale.start_login();
        let adapter_cancel = cancel.clone();
        let client = self.clone();
        let task = tokio::spawn(async move {
            while let Some(event) = shared_events.recv().await {
                let event = match event {
                    tailscale::LoginEvent::Url(url) => LoginEvent::Url(url.as_str().to_owned()),
                    tailscale::LoginEvent::Ready(status) => {
                        let readiness =
                            client.enrich_readiness(tailscale::Readiness::Ready(status)).await;
                        LoginEvent::Ready(readiness)
                    }
                    tailscale::LoginEvent::Failed(error) => LoginEvent::Failed(error),
                    tailscale::LoginEvent::Cancelled => LoginEvent::Cancelled,
                };
                if sender.send(event).await.is_err() {
                    let _ = adapter_cancel.send(true);
                    break;
                }
            }
            if let Err(error) = shared_task.await {
                let _ = sender
                    .send(LoginEvent::Failed(format!("Tailscale login task failed: {error}")))
                    .await;
            }
        });
        (receiver, cancel, task)
    }

    async fn enrich_readiness(&self, readiness: tailscale::Readiness) -> Readiness {
        let mut readiness = Readiness::from(readiness);
        if let Readiness::Ready { peers, .. } = &mut readiness {
            if let Ok(targets) =
                self.capture("list Taildrop targets", ["file", "cp", "--targets"]).await
            {
                reconcile_targets(peers, &targets);
            }
        }
        readiness
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
        cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        let args = vec![
            OsString::from("file"),
            OsString::from("get"),
            OsString::from("--conflict=rename"),
            OsString::from("--wait"),
            directory.as_os_str().to_owned(),
        ];
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
            biased;
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;

    use crate::framework::process::test_support::{CommandFixture, CommandResponse};

    use super::*;

    const READY_STATUS: &str = r#"{"BackendState":"Running","TailscaleIPs":["100.64.0.1"],"Self":{"ID":"me","DNSName":"desktop.test.ts.net.","HostName":"desktop","OS":"linux","Online":true,"TailscaleIPs":["100.64.0.1"]},"Peer":{"peer":{"ID":"peer","DNSName":"laptop.test.ts.net.","HostName":"laptop","OS":"linux","Online":true,"TailscaleIPs":["100.64.0.2"]}}}"#;

    #[test]
    fn reconciles_authoritative_taildrop_targets_by_ip() {
        let mut peers = vec![super::super::model::Device {
            id: "peer".into(),
            name: "Laptop".into(),
            dns_name: "laptop.example.ts.net".into(),
            operating_system: crate::tailscale::OperatingSystem::Macos,
            online: true,
            addresses: vec!["100.64.0.2".into()],
            taildrop_target: None,
        }];
        reconcile_targets(&mut peers, b"100.64.0.2\tlaptop\n");
        assert_eq!(peers[0].send_target(), Some("100.64.0.2"));
    }

    #[tokio::test]
    async fn fake_cli_proves_readiness_targets_and_private_text_stdin() {
        let mut fixture = CommandFixture::new().unwrap();
        fixture
            .respond(["status", "--json"], CommandResponse::success().stdout(READY_STATUS))
            .unwrap();
        fixture
            .respond(
                ["file", "cp", "--targets"],
                CommandResponse::success().stdout("100.64.0.2\tlaptop\n"),
            )
            .unwrap();
        fixture
            .respond(
                ["file", "cp", "--name=clipboard.txt", "-", "100.64.0.2:"],
                CommandResponse::success().expect_stdin("hello from stdin"),
            )
            .unwrap();
        let processes = ProcessSupervisor::for_test(fixture.root().join("processes")).unwrap();
        let client = TailClient::with_executable(
            processes,
            fixture.root().to_path_buf(),
            fixture.executable(),
        );

        let readiness = client.readiness().await.unwrap();
        let Readiness::Ready { peers, .. } = readiness else {
            panic!(
                "fake CLI was not ready: {readiness:?}; invocations: {:?}",
                fixture.invocations()
            )
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
        let sent = fixture
            .invocations()
            .unwrap()
            .into_iter()
            .find(|invocation| {
                invocation.arguments
                    == ["file", "cp", "--name=clipboard.txt", "-", "100.64.0.2:"]
                        .map(OsString::from)
            })
            .unwrap();
        assert_eq!(sent.stdin, b"hello from stdin");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_transfer_cancellation_returns_only_after_reaping() {
        let mut fixture = CommandFixture::new().unwrap();
        let payload = fixture.root().join("payload.txt");
        fs::write(&payload, "payload").unwrap();
        let arguments = vec![
            OsString::from("file"),
            OsString::from("cp"),
            payload.as_os_str().to_owned(),
            OsString::from("100.64.0.2:"),
        ];
        fixture.respond(arguments.clone(), CommandResponse::hang()).unwrap();
        let processes = ProcessSupervisor::for_test(fixture.root().join("processes")).unwrap();
        let client = TailClient::with_executable(
            processes,
            fixture.root().to_path_buf(),
            fixture.executable(),
        );
        let (cancel, cancel_receiver) = watch::channel(false);
        let transfer = tokio::spawn({
            let client = client.clone();
            let payload = payload.clone();
            async move { client.send_files(&[payload], "100.64.0.2", cancel_receiver).await }
        });
        let invocation =
            fixture.wait_for_invocation(arguments, Duration::from_secs(3)).await.unwrap();
        cancel.send(true).unwrap();
        let error = transfer.await.unwrap().unwrap_err();
        assert!(format!("{error:#}").contains("operation cancelled"));
        assert_eq!(unsafe { libc::kill(invocation.pid as i32, 0) }, -1);
    }
}
