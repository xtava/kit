#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::fs;
use std::future::Future;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, ensure, Context, Result};
use wezterm_codec::{
    EnvironmentFreeCommand, GetLines, GetPaneRenderableDimensions, NonEmptyProgram, SpawnV2,
    TabSpawnDomain, TabSpawnPlacement,
};
use wezterm_config::UnixDomain;
use wezterm_mux::client::ClientId;
use wezterm_mux::DEFAULT_WORKSPACE;
use wezterm_runtime_admission::{
    ByteClass, CountClass, RetainedClass, RuntimeAdmission, RuntimeRole, MAX_ATTACHMENTS,
    MAX_INBOUND_REQUESTS_PER_ATTACHMENT, MAX_PANE_INPUT_BYTES_TOTAL,
    MAX_RETAINED_STATE_BYTES_TOTAL, MAX_TABS,
};
use wezterm_term::TerminalSize;

const TEST_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TEST_RPC_TIMEOUT: Duration = Duration::from_secs(3);
const RSS_TEST_TIMEOUT: Duration = Duration::from_secs(75);
const RSS_MARKER_TIMEOUT: Duration = Duration::from_secs(10);
const RSS_SCROLLBACK_LINES_PER_SESSION: usize = 512;
const RSS_OUTPUT_SETTLE_LINES: isize = 64;
const RSS_BASELINE_TOLERANCE_BYTES: usize = 128 * 1024 * 1024;
const ONE_PIXEL_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScL0WQAAAABJRU5ErkJggg==";

struct IsolatedAgent {
    runtime_root: std::path::PathBuf,
    socket: std::path::PathBuf,
    log_path: std::path::PathBuf,
    child: Option<Child>,
}

impl IsolatedAgent {
    fn start() -> Self {
        Self::start_with_env(&[])
    }

    fn start_with_env(environment: &[(&str, &str)]) -> Self {
        let runtime_root = std::env::temp_dir().join(format!(
            "kit-console-headless-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&runtime_root).expect("create isolated Console runtime root");
        let socket = runtime_root.join("kit/console/agent.sock");
        let log_path = runtime_root.join("agent.log");
        let log = fs::File::create(&log_path).expect("create isolated Console agent log");
        let mut command = Command::new(env!("CARGO_BIN_EXE_kit"));
        command
            .args(["console", "__agent"])
            .env("XDG_RUNTIME_DIR", &runtime_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        for (key, value) in environment {
            command.env(key, value);
        }
        let child = command.spawn().expect("start isolated Console agent");
        let mut agent = Self { runtime_root, socket, log_path, child: Some(child) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !agent.socket.exists() && Instant::now() < deadline {
            if let Some(status) =
                agent.child.as_mut().expect("live child").try_wait().expect("observe Console agent")
            {
                panic!("Console agent exited before readiness: {status}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(agent.socket.exists(), "Console agent did not create its socket");
        agent
    }

    fn process_id(&self) -> u32 {
        self.child.as_ref().expect("live child").id()
    }

    fn diagnostics(&self) -> String {
        fs::read_to_string(&self.log_path)
            .unwrap_or_else(|error| format!("<log unavailable: {error}>"))
    }

    fn shutdown_result(&mut self) -> Result<()> {
        let signal_result = unsafe {
            libc::kill(
                self.child.as_mut().context("isolated Console agent was already stopped")?.id()
                    as libc::pid_t,
                libc::SIGTERM,
            )
        };
        if signal_result != 0 {
            bail!("send SIGTERM to Console agent: {}", std::io::Error::last_os_error());
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = self
                .child
                .as_mut()
                .context("isolated Console agent was already stopped")?
                .try_wait()
                .context("observe Console agent shutdown")?;
            if let Some(status) = status {
                self.child.take();
                ensure!(status.success(), "Console agent shutdown failed: {status}");
                ensure!(!self.socket.exists(), "Console agent left its socket behind");
                return Ok(());
            }
            if Instant::now() >= deadline {
                if let Some(child) = self.child.as_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                self.child.take();
                bail!("Console agent did not stop within five seconds of SIGTERM");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn shutdown(mut self) {
        self.shutdown_result().expect("shut down isolated Console agent");
    }
}

impl Drop for IsolatedAgent {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.runtime_root);
    }
}

#[test]
fn embedded_wezterm_headless() {
    IsolatedAgent::start().shutdown();
}

fn expected_build_identity() -> wezterm_codec::BuildIdentity {
    wezterm_codec::BuildIdentity {
        product: "kit-console".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        source_revision: Some(env!("KIT_SOURCE_REVISION").to_owned()),
        source_dirty: Some(env!("KIT_SOURCE_DIRTY") == "true"),
        embedded_wezterm_revision: Some(env!("KIT_WEZTERM_REVISION").to_owned()),
    }
}

fn sanitized_client_id() -> ClientId {
    ClientId { ssh_auth_sock: None, ..ClientId::new() }
}

fn isolated_domain(agent: &IsolatedAgent) -> UnixDomain {
    UnixDomain {
        name: "kit-console-verifier".to_owned(),
        socket_path: Some(agent.socket.clone()),
        no_serve_automatically: true,
        ..Default::default()
    }
}

struct HeadlessClient {
    client: wezterm_client::client::Client,
    _admission: Arc<RuntimeAdmission>,
    lifecycle: Arc<wezterm_client::client::HeadlessConnectionLifecycle>,
}

impl HeadlessClient {
    async fn connect(agent: &IsolatedAgent) -> Result<Self> {
        let admission = RuntimeAdmission::new(RuntimeRole::Client)?;
        let lifecycle = Arc::new(wezterm_client::client::HeadlessConnectionLifecycle::new(
            Arc::clone(&admission),
        ));
        let domain = isolated_domain(agent);
        let client = tokio::time::timeout(
            TEST_CONNECT_TIMEOUT,
            wezterm_client::client::Client::new_unix_domain_headless(
                Arc::clone(&admission),
                &lifecycle,
                None,
                &domain,
                Some(expected_build_identity()),
                sanitized_client_id(),
                true,
                true,
            ),
        )
        .await
        .context("timed out connecting to isolated Console agent")??;
        Ok(Self { client, _admission: admission, lifecycle })
    }
}

impl Drop for HeadlessClient {
    fn drop(&mut self) {
        let _ = self.client.shutdown_and_join();
    }
}

fn test_terminal_size() -> TerminalSize {
    TerminalSize { rows: 48, cols: 160, pixel_width: 0, pixel_height: 0, dpi: 0 }
}

async fn bounded_rpc<T>(
    operation: &'static str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(TEST_RPC_TIMEOUT, future)
        .await
        .with_context(|| format!("timed out while {operation}"))?
}

async fn spawn_program(
    client: &wezterm_client::client::Client,
    command: EnvironmentFreeCommand,
) -> Result<usize> {
    let response = bounded_rpc(
        "spawning isolated Console terminal",
        client.spawn_v2(SpawnV2 {
            domain: TabSpawnDomain::DefaultDomain,
            placement: TabSpawnPlacement::NewWindow {
                size: test_terminal_size(),
                workspace: DEFAULT_WORKSPACE.to_owned(),
            },
            command,
            command_dir: None,
        }),
    )
    .await?
    .into_inner();
    Ok(response.pane_id)
}

async fn visible_pane_text(
    client: &wezterm_client::client::Client,
    pane_id: usize,
) -> Result<String> {
    let dimensions = bounded_rpc(
        "reading isolated Console terminal dimensions",
        client.get_dimensions(GetPaneRenderableDimensions { pane_id }),
    )
    .await?
    .into_inner()
    .dimensions;
    let end = dimensions
        .physical_top
        .checked_add(dimensions.viewport_rows as isize)
        .context("computing isolated Console viewport range")?;
    let start = end.saturating_sub(RSS_OUTPUT_SETTLE_LINES);
    let lines = bounded_rpc(
        "reading isolated Console terminal output",
        client.get_lines(GetLines { pane_id, lines: std::iter::once(start..end).collect() }),
    )
    .await?
    .into_inner()
    .lines
    .extract_data()
    .0;
    Ok(lines.into_iter().map(|(_, line)| line.as_str().into_owned()).collect::<Vec<_>>().join("\n"))
}

async fn wait_for_marker(
    client: &wezterm_client::client::Client,
    pane_id: usize,
    marker: &str,
) -> Result<String> {
    let deadline = Instant::now() + RSS_MARKER_TIMEOUT;
    loop {
        let output = visible_pane_text(client, pane_id).await?;
        if output.contains(marker) {
            return Ok(output);
        }
        if Instant::now() >= deadline {
            bail!(
                "terminal pane {pane_id} did not produce marker {marker:?}; \
                 last viewport={output:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn shell_program(script: String) -> Result<EnvironmentFreeCommand> {
    Ok(EnvironmentFreeCommand::Program {
        program: NonEmptyProgram::new("/bin/sh".to_owned())?,
        args: vec!["-c".to_owned(), script],
    })
}

#[cfg(target_os = "linux")]
fn process_rss_bytes(pid: u32) -> Result<usize> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))
        .with_context(|| format!("read Linux RSS for isolated Console agent {pid}"))?;
    let kilobytes = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .context("isolated Console agent did not report VmRSS")?
        .parse::<usize>()
        .context("parse isolated Console agent VmRSS")?;
    kilobytes.checked_mul(1024).context("isolated Console agent RSS overflow")
}

#[cfg(target_os = "macos")]
fn process_rss_bytes(pid: u32) -> Result<usize> {
    let output = Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .with_context(|| format!("read macOS RSS for isolated Console agent {pid}"))?;
    ensure!(output.status.success(), "macOS ps failed for isolated Console agent {pid}");
    let kilobytes = std::str::from_utf8(&output.stdout)
        .context("decode macOS ps output")?
        .trim()
        .parse::<usize>()
        .context("parse macOS isolated Console agent RSS")?;
    kilobytes.checked_mul(1024).context("isolated Console agent RSS overflow")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedded_wezterm_protocol_bootstrap() {
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true).expect("initialize headless WezTerm config");
    let agent = IsolatedAgent::start();
    let domain = isolated_domain(&agent);
    let expected = expected_build_identity();

    let mismatch_admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
    let mismatch_lifecycle =
        wezterm_client::client::HeadlessConnectionLifecycle::new(Arc::clone(&mismatch_admission));
    let mut mismatch = expected.clone();
    mismatch.source_dirty = mismatch.source_dirty.map(|dirty| !dirty);
    let mismatch_error = wezterm_client::client::Client::new_unix_domain_headless(
        mismatch_admission,
        &mismatch_lifecycle,
        None,
        &domain,
        Some(mismatch),
        sanitized_client_id(),
        true,
        true,
    )
    .await
    .err()
    .expect("mismatched build identity must fail");
    assert!(mismatch_error.root_cause().is::<wezterm_client::client::BuildIdentityMismatch>());

    let admission = RuntimeAdmission::new(RuntimeRole::Client).unwrap();
    let lifecycle =
        wezterm_client::client::HeadlessConnectionLifecycle::new(Arc::clone(&admission));
    let client = wezterm_client::client::Client::new_unix_domain_headless(
        admission,
        &lifecycle,
        None,
        &domain,
        Some(expected),
        sanitized_client_id(),
        true,
        true,
    )
    .await
    .expect("matching build identity must activate the client");
    assert_eq!(client.initial_server_version().codec_vers, wezterm_codec::CODEC_VERSION);
    let _ = client.shutdown_and_join();
    drop(client);
    agent.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedded_wezterm_native_spawn_environment() -> Result<()> {
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true).context("initialize headless WezTerm config")?;

    let mut agent = IsolatedAgent::start_with_env(&[
        ("HOME", "/tmp"),
        ("PATH", "/kit-console-server-path:/usr/bin:/bin"),
        ("SSH_AUTH_SOCK", "/kit-console-server-ssh-auth.sock"),
    ]);
    let mut client = None;
    let work = async {
        let connected = HeadlessClient::connect(&agent)
            .await
            .context("connect native environment verifier client")?;
        client = Some(connected);
        let connected = client.as_ref().context("retain native environment Console client")?;
        let pane_id = match spawn_program(
            &connected.client,
            shell_program(
                "printf 'HOME=<%s>\\nPATH=<%s>\\nSSH_AUTH_SOCK=<%s>\\nSSH_AUTH_TARGET=<%s>\\nKIT_ENV_DONE\\n' \\
                 \"$HOME\" \"$PATH\" \"$SSH_AUTH_SOCK\" \\
                 \"$(readlink \"$SSH_AUTH_SOCK\")\"; /bin/sleep 2"
                    .to_owned(),
            )?,
        )
        .await
        {
            Ok(pane_id) => pane_id,
            Err(error) => {
                let lifecycle = connected.lifecycle.try_recv();
                let outcome = connected.client.shutdown_and_join();
                bail!(
                    "native shell spawn failed: {error:#}; lifecycle={lifecycle:?}; \
                     outcome={outcome:?}"
                )
            }
        };
        let output = match wait_for_marker(&connected.client, pane_id, "KIT_ENV_DONE").await {
            Ok(output) => output,
            Err(error) => {
                let lifecycle = connected.lifecycle.try_recv();
                let outcome = connected.client.shutdown_and_join();
                bail!(
                    "native shell output failed: {error:#}; lifecycle={lifecycle:?}; \
                     outcome={outcome:?}; agent_log={:?}",
                    agent.diagnostics()
                )
            }
        };
        ensure!(
            output.contains("HOME=</tmp>"),
            "spawned shell did not receive the agent HOME: {output:?}"
        );
        ensure!(
            output.contains("PATH=</kit-console-server-path:/usr/bin:/bin>"),
            "spawned shell did not receive the agent PATH: {output:?}"
        );
        let proxy_prefix =
            format!("SSH_AUTH_SOCK=<{}/wezterm/agent.", agent.runtime_root.display());
        ensure!(
            output.contains(&proxy_prefix),
            "spawned shell did not receive its agent-owned SSH proxy: {output:?}"
        );
        ensure!(
            output.contains("SSH_AUTH_TARGET=</kit-console-server-ssh-auth.sock>"),
            "spawned shell SSH proxy did not target the agent's native credentials: {output:?}"
        );
        Ok(())
    };
    let work_result = match tokio::time::timeout(Duration::from_secs(20), work).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("native spawn environment verifier exceeded its 20-second timeout")),
    };
    drop(client);
    let shutdown_result = agent.shutdown_result();
    match (work_result, shutdown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (work, shutdown) => bail!(
            "native environment verifier failed: work={work:?}; shutdown={shutdown:?}; \
             agent_log={:?}",
            agent.diagnostics()
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedded_wezterm_short_lived_pane_preserves_attachment() -> Result<()> {
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true).context("initialize headless WezTerm config")?;

    let mut agent = IsolatedAgent::start();
    let connected = HeadlessClient::connect(&agent).await?;
    spawn_program(
        &connected.client,
        shell_program(
            "i=0; while [ \"$i\" -lt 32 ]; do printf 'departing-%s\\n' \"$i\"; \
             i=$((i + 1)); done"
                .to_owned(),
        )?,
    )
    .await?;

    tokio::time::sleep(Duration::from_millis(250)).await;
    let pane_id = spawn_program(
        &connected.client,
        shell_program("printf 'KIT_ATTACHMENT_ALIVE\\n'; /bin/sleep 2".to_owned())?,
    )
    .await?;
    let output = wait_for_marker(&connected.client, pane_id, "KIT_ATTACHMENT_ALIVE").await?;
    ensure!(output.contains("KIT_ATTACHMENT_ALIVE"));

    drop(connected);
    agent.shutdown_result()
}

fn hostile_terminal_program(session: usize) -> Result<EnvironmentFreeCommand> {
    shell_program(format!(
        r#"printf '\033]2;kit-rss-session-{session}\007'
printf '\033]8;;https://example.invalid/kit-rss/{session}\033\\linked-{session}\033]8;;\033\\\n'
printf '\033_Gf=100,a=T,t=d;{ONE_PIXEL_PNG_BASE64}\033\\'
i=0
while [ "$i" -lt {RSS_SCROLLBACK_LINES_PER_SESSION} ]; do
    printf 'scrollback-{session}-%04d-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n' "$i"
    i=$((i + 1))
done
# Force a parser boundary after the hostile image/output batch. Aggregate retained-state admission
# may reject an image batch before allocation; the later marker proves that rejection leaves the
# pane and its parser usable.
/bin/sleep 1
printf 'KIT_RSS_DONE_{session}\n'
/bin/sleep 30
"#
    ))
}

/// Runs only when explicitly requested because it starts the complete production Console session
/// budget. Console exposes one pane per session and rejects splits, so `MAX_TABS` is its
/// user-reachable terminal count; generic `MAX_PANES` saturation belongs to mux admission tests.
/// The threshold is the agent's observed baseline plus the server's retained terminal and
/// PTY-input admission budgets, with a fixed native allocator/thread tolerance.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "native bounded RSS saturation proof; runs the full production Console session budget"]
async fn embedded_wezterm_hostile_rss() -> Result<()> {
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true).context("initialize headless WezTerm config")?;

    let mut agent = IsolatedAgent::start();
    let mut client = None;
    let work = async {
        let baseline_rss = process_rss_bytes(agent.process_id())?;
        let connected = HeadlessClient::connect(&agent).await?;
        client = Some(connected);
        let connected = client.as_ref().context("retain hostile RSS Console client")?;

        let mut panes = Vec::with_capacity(MAX_TABS);
        for session in 0..MAX_TABS {
            panes.push((
                session,
                spawn_program(&connected.client, hostile_terminal_program(session)?).await?,
            ));
        }
        ensure!(
            panes.len() == MAX_TABS,
            "Console did not create its complete externally reachable session budget"
        );
        let overflow = spawn_program(&connected.client, hostile_terminal_program(MAX_TABS)?).await;
        ensure!(
            overflow.is_err(),
            "Console accepted a session above its externally reachable MAX_TABS budget"
        );

        for (session, pane_id) in panes {
            let marker = format!("KIT_RSS_DONE_{session}");
            wait_for_marker(&connected.client, pane_id, &marker).await?;
        }

        let rss = process_rss_bytes(agent.process_id())?;
        let admission_envelope = MAX_RETAINED_STATE_BYTES_TOTAL
            .checked_add(MAX_PANE_INPUT_BYTES_TOTAL)
            .context("compute Console RSS admission envelope")?;
        let rss_limit = baseline_rss
            .checked_add(admission_envelope)
            .and_then(|bytes| bytes.checked_add(RSS_BASELINE_TOLERANCE_BYTES))
            .context("compute Console RSS threshold")?;
        ensure!(
            rss <= rss_limit,
            "Console RSS exceeded its bounded production envelope: rss={rss} baseline={baseline_rss} \
             admission={admission_envelope} tolerance={RSS_BASELINE_TOLERANCE_BYTES} limit={rss_limit}"
        );
        Ok(())
    };
    let work_result = match tokio::time::timeout(RSS_TEST_TIMEOUT, work).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("hostile Console RSS verifier exceeded its 75-second timeout")),
    };
    drop(client);
    let shutdown_result = agent.shutdown_result();
    work_result?;
    shutdown_result
}

#[test]
fn embedded_wezterm_memory_bounds() {
    let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
    let retained = admission
        .try_retained(RetainedClass::ServerTerminal, MAX_RETAINED_STATE_BYTES_TOTAL)
        .unwrap();
    assert!(admission.try_retained(RetainedClass::ServerTerminal, 1).is_err());
    drop(retained);

    let bytes =
        admission.try_bytes(ByteClass::DecodeWorking, ByteClass::DecodeWorking.capacity()).unwrap();
    assert!(admission.try_bytes(ByteClass::DecodeWorking, 1).is_err());
    drop(bytes);
}

#[test]
fn embedded_wezterm_dispatch_bounds() {
    let admission = RuntimeAdmission::new(RuntimeRole::Server).unwrap();
    let attachments =
        (0..MAX_ATTACHMENTS).map(|_| admission.try_attachment().unwrap()).collect::<Vec<_>>();
    assert!(admission.try_attachment().is_err());

    let inbound = (0..MAX_INBOUND_REQUESTS_PER_ATTACHMENT)
        .map(|_| attachments[0].try_inbound().unwrap())
        .collect::<Vec<_>>();
    assert!(attachments[0].try_inbound().is_err());
    assert_eq!(
        admission.count_usage(CountClass::InboundRequest),
        MAX_INBOUND_REQUESTS_PER_ATTACHMENT
    );
    drop(inbound);
    drop(attachments);
    assert_eq!(admission.count_usage(CountClass::Attachment), 0);
    assert_eq!(admission.count_usage(CountClass::InboundRequest), 0);
}
