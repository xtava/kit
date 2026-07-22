#![allow(dead_code)] // Shared integration support is compiled independently by each test binary.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::future::Future;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};
use wezterm_codec::{
    ControlLeaseAction, ControlLeaseRequest, ControlLeaseResult, EnvironmentFreeCommand, GetLines,
    GetPaneRenderableDimensions, KillPane, NonEmptyProgram, ServiceDrainAction,
    ServiceDrainRequest, SpawnV2, TabSpawnDomain, TabSpawnPlacement,
};
use wezterm_config::UnixDomain;
use wezterm_mux::client::ClientId;
use wezterm_mux::tab::PaneNode;
use wezterm_mux::{RuntimeAdmission, RuntimeRole, DEFAULT_WORKSPACE};
use wezterm_term::TerminalSize;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RPC_TIMEOUT: Duration = Duration::from_secs(3);
const READY_TIMEOUT: Duration = Duration::from_secs(8);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const OUTPUT_LINES: isize = 128;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SessionIdentity {
    pub window_id: usize,
    pub tab_id: usize,
    pub pane_id: usize,
}

pub struct LocalConsoleHarness {
    runtime_root: PathBuf,
    socket: PathBuf,
    log_path: PathBuf,
    child: Option<Child>,
}

impl LocalConsoleHarness {
    pub fn start() -> Result<Self> {
        let runtime_root = PathBuf::from("/tmp").join(format!(
            "kc-live-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir(&runtime_root)
            .with_context(|| format!("create Console runtime root {}", runtime_root.display()))?;
        fs::set_permissions(&runtime_root, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("secure Console runtime root {}", runtime_root.display()))?;
        let socket = runtime_root.join("agent.sock");
        let log_path = runtime_root.join("agent.log");
        let log = fs::File::create(&log_path)
            .with_context(|| format!("create Console agent log {}", log_path.display()))?;
        let child = Command::new(env!("CARGO_BIN_EXE_kit"))
            .args(["console", "__agent"])
            .env("KIT_CONSOLE_RUNTIME_DIR", &runtime_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .context("start foreground Console agent")?;
        let mut harness = Self { runtime_root, socket, log_path, child: Some(child) };
        harness.wait_until_ready()?;
        harness.assert_socket_policy()?;
        Ok(harness)
    }

    pub fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }

    pub fn agent_pid(&self) -> u32 {
        self.child.as_ref().expect("live Console agent").id()
    }

    pub fn diagnostics(&self) -> String {
        fs::read_to_string(&self.log_path)
            .unwrap_or_else(|error| format!("<agent log unavailable: {error}>"))
    }

    pub fn assert_socket_policy(&self) -> Result<()> {
        let metadata = fs::symlink_metadata(&self.socket)
            .with_context(|| format!("inspect Console socket {}", self.socket.display()))?;
        ensure!(metadata.file_type().is_socket(), "Console endpoint is not a Unix socket");
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "Console socket owner differs from the verifier user"
        );
        let socket_mode = metadata.mode() & 0o777;
        ensure!(
            socket_mode & 0o600 == 0o600,
            "Console socket owner lacks read/write access: mode={socket_mode:o}"
        );
        ensure!(
            socket_mode & 0o022 == 0,
            "Console socket permits group/other writes: mode={socket_mode:o}"
        );

        let parent = self.socket.parent().context("Console socket has no parent directory")?;
        let parent_metadata = fs::symlink_metadata(parent)
            .with_context(|| format!("inspect Console socket directory {}", parent.display()))?;
        ensure!(parent_metadata.file_type().is_dir(), "Console socket parent is not a directory");
        ensure!(
            parent_metadata.uid() == unsafe { libc::geteuid() },
            "Console socket directory owner differs from the verifier user"
        );
        ensure!(
            parent_metadata.mode() & 0o077 == 0,
            "Console socket directory is accessible to another user"
        );
        Ok(())
    }

    pub fn observe_session_leader(&self, marker: &str) -> Result<u32> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let rows = process_table()?;
            let parents = rows.iter().map(|row| (row.pid, row.ppid)).collect::<HashMap<_, _>>();
            if let Some(pid) = rows
                .iter()
                .find(|row| {
                    row.command.contains(marker)
                        && descends_from(row.pid, self.agent_pid(), &parents)
                })
                .map(|row| row.pid)
            {
                return Ok(pid);
            }
            if Instant::now() >= deadline {
                bail!(
                    "no Console agent descendant contains marker {marker:?}; agent log={:?}",
                    self.diagnostics()
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn wait_for_files(&self, paths: &[PathBuf]) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let missing = paths.iter().filter(|path| !path.exists()).collect::<Vec<_>>();
            if missing.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("Console shell did not create emission receipts: {missing:?}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn shutdown(&mut self) -> Result<()> {
        let agent_pid = self.child.as_ref().context("Console agent was already stopped")?.id();
        let descendants = descendant_pids(agent_pid).unwrap_or_default();
        signal(agent_pid, libc::SIGTERM).context("send SIGTERM to Console agent")?;

        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        loop {
            if let Some(status) = self
                .child
                .as_mut()
                .context("Console agent was already stopped")?
                .try_wait()
                .context("observe Console agent shutdown")?
            {
                self.child.take();
                if !status.success() {
                    force_kill(&descendants);
                    bail!("Console agent shutdown failed: {status}");
                }
                break;
            }
            if Instant::now() >= deadline {
                force_kill(&descendants);
                bail!("Console agent did not stop within five seconds of SIGTERM");
            }
            thread::sleep(Duration::from_millis(10));
        }

        let cleanup_deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        loop {
            let live =
                descendants.iter().copied().filter(|pid| process_exists(*pid)).collect::<Vec<_>>();
            if !self.socket.exists() && live.is_empty() {
                return Ok(());
            }
            if Instant::now() >= cleanup_deadline {
                force_kill(&live);
                bail!(
                    "Console shutdown left socket={} or descendants={live:?}; agent log={:?}",
                    self.socket.exists(),
                    self.diagnostics()
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_until_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        while !self.socket.exists() {
            if let Some(status) = self
                .child
                .as_mut()
                .context("Console agent child is missing")?
                .try_wait()
                .context("observe Console agent startup")?
            {
                bail!(
                    "Console agent exited before readiness: {status}; log={:?}",
                    self.diagnostics()
                );
            }
            if Instant::now() >= deadline {
                bail!("Console agent did not create its socket; log={:?}", self.diagnostics());
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    fn domain(&self) -> UnixDomain {
        UnixDomain {
            name: "kit-console-live-verifier".to_owned(),
            socket_path: Some(self.socket.clone()),
            no_serve_automatically: true,
            ..Default::default()
        }
    }
}

impl Drop for LocalConsoleHarness {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let descendants = descendant_pids(child.id()).unwrap_or_default();
            let _ = child.kill();
            let _ = child.wait();
            for pid in descendants {
                let _ = signal(pid, libc::SIGKILL);
            }
            self.child.take();
        }
        let _ = fs::remove_dir_all(&self.runtime_root);
    }
}

pub struct HeadlessConsoleClient {
    client: wezterm_client::client::Client,
    _admission: Arc<RuntimeAdmission>,
    _lifecycle: Arc<wezterm_client::client::HeadlessConnectionLifecycle>,
}

impl HeadlessConsoleClient {
    pub async fn connect(harness: &LocalConsoleHarness) -> Result<Self> {
        let admission = RuntimeAdmission::new(RuntimeRole::Client)?;
        let lifecycle = Arc::new(wezterm_client::client::HeadlessConnectionLifecycle::new(
            Arc::clone(&admission),
        ));
        let client = tokio::time::timeout(
            CONNECT_TIMEOUT,
            wezterm_client::client::Client::new_unix_domain_headless(
                Arc::clone(&admission),
                lifecycle.as_ref(),
                None,
                &harness.domain(),
                Some(expected_build_identity()),
                sanitized_client_id(),
                true,
                true,
            ),
        )
        .await
        .context("timed out connecting to foreground Console agent")??;
        Ok(Self { client, _admission: admission, _lifecycle: lifecycle })
    }

    pub async fn spawn_script(&self, script: String) -> Result<SessionIdentity> {
        let response = bounded_rpc(
            "spawning Console session",
            self.client.spawn_v2(SpawnV2 {
                domain: TabSpawnDomain::DefaultDomain,
                placement: TabSpawnPlacement::NewWindow {
                    size: test_terminal_size(),
                    workspace: DEFAULT_WORKSPACE.to_owned(),
                },
                command: EnvironmentFreeCommand::Program {
                    program: NonEmptyProgram::new("/bin/sh".to_owned())?,
                    args: vec!["-c".to_owned(), script],
                },
                command_dir: None,
            }),
        )
        .await?
        .into_inner();
        Ok(SessionIdentity {
            window_id: response.window_id,
            tab_id: response.tab_id,
            pane_id: response.pane_id,
        })
    }

    pub async fn begin_service_drain(&self) -> Result<()> {
        let result = bounded_rpc(
            "beginning Console service drain",
            self.client.service_drain(ServiceDrainRequest { action: ServiceDrainAction::Begin }),
        )
        .await?
        .into_inner();
        ensure!(result.draining, "Console did not enter service drain mode");
        Ok(())
    }

    pub async fn cancel_service_drain(&self) -> Result<()> {
        let result = bounded_rpc(
            "cancelling Console service drain",
            self.client.service_drain(ServiceDrainRequest { action: ServiceDrainAction::Cancel }),
        )
        .await?
        .into_inner();
        ensure!(!result.draining, "Console did not leave service drain mode");
        Ok(())
    }

    pub async fn topology(&self) -> Result<Vec<SessionIdentity>> {
        let response =
            bounded_rpc("listing Console sessions", self.client.list_panes()).await?.into_inner();
        ensure!(
            response.tabs.len() == response.tab_titles.len(),
            "Console returned mismatched tab roots and titles"
        );
        let mut identities = Vec::with_capacity(response.tabs.len());
        for root in response.tabs {
            match root {
                PaneNode::Leaf(pane) => identities.push(SessionIdentity {
                    window_id: pane.window_id,
                    tab_id: pane.tab_id,
                    pane_id: pane.pane_id,
                }),
                PaneNode::Empty => bail!("Console session has no pane"),
                PaneNode::Split { .. } => bail!("Console session contains more than one pane"),
            }
        }
        identities.sort_unstable();
        Ok(identities)
    }

    pub async fn wait_for_topology(
        &self,
        expected: &[SessionIdentity],
    ) -> Result<Vec<SessionIdentity>> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let topology = self.topology().await?;
            if topology == expected {
                return Ok(topology);
            }
            if Instant::now() >= deadline {
                bail!(
                    "Console topology did not converge: expected={expected:?} actual={topology:?}"
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn wait_for_session_count(&self, count: usize) -> Result<Vec<SessionIdentity>> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let topology = self.topology().await?;
            if topology.len() == count {
                return Ok(topology);
            }
            if Instant::now() >= deadline {
                bail!(
                    "Console session count did not converge: expected={count} actual={topology:?}"
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn wait_for_title(&self, tab_id: usize, expected: &str) -> Result<()> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let response = bounded_rpc("reading Console session titles", self.client.list_panes())
                .await?
                .into_inner();
            let title = response
                .tabs
                .iter()
                .position(|root| pane_node_tab_id(root) == Some(tab_id))
                .and_then(|index| response.tab_titles.get(index));
            if title.is_some_and(|title| title == expected) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("Console title did not converge: tab={tab_id} expected={expected:?} actual={title:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn wait_for_dimensions(
        &self,
        pane_id: usize,
        predicate: impl Fn(usize, usize) -> bool,
    ) -> Result<(usize, usize)> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let dimensions = bounded_rpc(
                "reading Console dimensions",
                self.client.get_dimensions(GetPaneRenderableDimensions { pane_id }),
            )
            .await?
            .into_inner()
            .dimensions;
            let actual = (dimensions.cols, dimensions.viewport_rows);
            if predicate(actual.0, actual.1) {
                return Ok(actual);
            }
            if Instant::now() >= deadline {
                bail!("Console dimensions did not converge for pane {pane_id}: actual={actual:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn wait_for_output(&self, pane_id: usize, marker: &str) -> Result<String> {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            let output = visible_pane_text(&self.client, pane_id).await?;
            if output.contains(marker) {
                return Ok(output);
            }
            if Instant::now() >= deadline {
                bail!("pane {pane_id} did not produce {marker:?}; last output={output:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn pane_text(&self, pane_id: usize) -> Result<String> {
        visible_pane_text(&self.client, pane_id).await
    }

    pub async fn close_pane(&self, pane_id: usize) -> Result<()> {
        let control = bounded_rpc(
            "taking control of Console pane",
            self.client
                .control_lease(ControlLeaseRequest { pane_id, action: ControlLeaseAction::Take }),
        )
        .await?
        .into_inner();
        ensure!(
            matches!(
                control,
                ControlLeaseResult::Taken(_)
                    | ControlLeaseResult::Acquired(_)
                    | ControlLeaseResult::AlreadyController(_)
            ),
            "Console verifier could not take pane control: {control:?}"
        );
        bounded_rpc("closing Console pane", self.client.kill_pane(KillPane { pane_id })).await?;
        Ok(())
    }
}

fn pane_node_tab_id(root: &PaneNode) -> Option<usize> {
    match root {
        PaneNode::Leaf(pane) => Some(pane.tab_id),
        PaneNode::Empty => None,
        PaneNode::Split { left, .. } => pane_node_tab_id(left),
    }
}

impl Drop for HeadlessConsoleClient {
    fn drop(&mut self) {
        let _ = self.client.shutdown_and_join();
    }
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

fn test_terminal_size() -> TerminalSize {
    TerminalSize { rows: 32, cols: 120, pixel_width: 0, pixel_height: 0, dpi: 0 }
}

async fn bounded_rpc<T>(
    operation: &'static str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(RPC_TIMEOUT, future)
        .await
        .with_context(|| format!("timed out while {operation}"))?
}

async fn visible_pane_text(
    client: &wezterm_client::client::Client,
    pane_id: usize,
) -> Result<String> {
    let dimensions = bounded_rpc(
        "reading Console dimensions",
        client.get_dimensions(GetPaneRenderableDimensions { pane_id }),
    )
    .await?
    .into_inner()
    .dimensions;
    let end = dimensions
        .physical_top
        .checked_add(dimensions.viewport_rows as isize)
        .context("compute Console viewport end")?;
    let lines = bounded_rpc(
        "reading Console output",
        client.get_lines(GetLines {
            pane_id,
            lines: std::iter::once(end.saturating_sub(OUTPUT_LINES)..end).collect(),
        }),
    )
    .await?
    .into_inner()
    .lines
    .extract_data()
    .0;
    Ok(lines.into_iter().map(|(_, line)| line.as_str().into_owned()).collect::<Vec<_>>().join("\n"))
}

#[derive(Debug)]
struct ProcessRow {
    pid: u32,
    ppid: u32,
    command: String,
}

fn descends_from(pid: u32, ancestor: u32, parents: &HashMap<u32, u32>) -> bool {
    let mut current = pid;
    let mut visited = HashSet::new();
    while visited.insert(current) {
        let Some(parent) = parents.get(&current).copied() else {
            return false;
        };
        if parent == ancestor {
            return true;
        }
        if parent == 0 || parent == current {
            return false;
        }
        current = parent;
    }
    false
}

fn descendant_pids(ancestor: u32) -> Result<Vec<u32>> {
    let rows = process_table()?;
    let parents = rows.iter().map(|row| (row.pid, row.ppid)).collect::<HashMap<_, _>>();
    Ok(rows
        .iter()
        .filter(|row| descends_from(row.pid, ancestor, &parents))
        .map(|row| row.pid)
        .collect())
}

#[cfg(target_os = "linux")]
fn process_table() -> Result<Vec<ProcessRow>> {
    let mut rows = Vec::new();
    for entry in fs::read_dir("/proc").context("read Linux process table")? {
        let entry = entry.context("read Linux process entry")?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let status = match fs::read_to_string(entry.path().join("status")) {
            Ok(status) => status,
            Err(_) => continue,
        };
        let Some(ppid) = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .and_then(|value| value.trim().parse::<u32>().ok())
        else {
            continue;
        };
        let command = fs::read(entry.path().join("cmdline"))
            .unwrap_or_default()
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .map(|field| String::from_utf8_lossy(field))
            .collect::<Vec<_>>()
            .join(" ");
        rows.push(ProcessRow { pid, ppid, command });
    }
    Ok(rows)
}

#[cfg(target_os = "macos")]
fn process_table() -> Result<Vec<ProcessRow>> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
        .context("read macOS process table")?;
    ensure!(output.status.success(), "macOS ps failed: {}", output.status);
    let source = std::str::from_utf8(&output.stdout).context("decode macOS process table")?;
    let mut rows = Vec::new();
    for line in source.lines() {
        let mut fields = line.split_whitespace();
        let Some(pid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(ppid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        rows.push(ProcessRow { pid, ppid, command: fields.collect::<Vec<_>>().join(" ") });
    }
    Ok(rows)
}

fn signal(pid: u32, signal: libc::c_int) -> Result<()> {
    if unsafe { libc::kill(pid as libc::pid_t, signal) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).with_context(|| format!("signal process {pid}"))
    }
}

fn force_kill(pids: &[u32]) {
    for pid in pids {
        let _ = signal(*pid, libc::SIGKILL);
    }
}

fn process_exists(pid: u32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}
