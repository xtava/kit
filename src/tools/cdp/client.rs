//! The thin client behind every non-daemon `kit cdp` command. It finds the warm Attachment for an
//! Instance selector — lazily spawning the daemon if none is live (`docs/adr/0003`) — sends one
//! [`Query`] over the unix socket, and prints the rendered [`Reply`]. No CDP, no state of its own.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Error, Result};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::sleep;

use tokio::sync::mpsc::{self, Receiver};

use crate::cdp::{self, TrackKind};

use super::protocol::{Command, Frame, LaunchSettings, Query, Reply};
use super::registry::{
    self, GpuMode, LaunchKind, LaunchOwnership, LaunchPhase, LaunchRecord, ProcessIdentity, Record,
    RenderMode,
};

const READY_TRIES: u32 = 60;
const READY_INTERVAL: Duration = Duration::from_millis(100);
const DEVTOOLS_TRIES: u32 = 100;
const DEVTOOLS_INTERVAL: Duration = Duration::from_millis(100);
const ELECTRON_TRIES: u32 = 300;
const ELECTRON_INTERVAL: Duration = Duration::from_millis(100);
const LIVE_FRAME_CAPACITY: usize = 1_024;
const CLOSE_GRACE: Duration = Duration::from_millis(750);
const CLOSE_TERM_GRACE: Duration = Duration::from_secs(2);
const CLOSE_KILL_GRACE: Duration = Duration::from_secs(1);
const CLOSE_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifiedLaunchProcess {
    session_id: u32,
    known: HashMap<u32, ProcessIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProcessObservation {
    Verified(VerifiedLaunchProcess),
    Dead,
    Mismatch(String),
    Unverified { process_may_be_live: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EndpointObservation {
    Current,
    Unreachable,
    Mismatch(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LaunchState {
    Current(VerifiedLaunchProcess),
    Unreachable(VerifiedLaunchProcess),
    EndpointMismatch { process: VerifiedLaunchProcess, reason: String },
    Dead,
    OwnershipMismatch(String),
    Unverified { process_may_be_live: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseAction {
    TerminateWithCdp,
    TerminateOwnedSession,
    CleanupDead,
    RetainRecoveryState,
}

fn close_action(state: &LaunchState) -> CloseAction {
    match state {
        LaunchState::Current(_) => CloseAction::TerminateWithCdp,
        LaunchState::Unreachable(_) | LaunchState::EndpointMismatch { .. } => {
            CloseAction::TerminateOwnedSession
        }
        LaunchState::Dead => CloseAction::CleanupDead,
        LaunchState::OwnershipMismatch(_) | LaunchState::Unverified { .. } => {
            CloseAction::RetainRecoveryState
        }
    }
}

impl LaunchState {
    fn description(&self) -> String {
        match self {
            Self::Current(_) => "current".to_owned(),
            Self::Unreachable(_) => "process verified, CDP endpoint unreachable".to_owned(),
            Self::EndpointMismatch { reason, .. } => {
                format!("process verified, endpoint mismatch: {reason}")
            }
            Self::Dead => "stopped".to_owned(),
            Self::OwnershipMismatch(reason) => format!("ownership mismatch: {reason}"),
            Self::Unverified { process_may_be_live: true } => {
                "live process may remain but launch ownership is unverified".to_owned()
            }
            Self::Unverified { process_may_be_live: false } => {
                "launch ownership is unverified and no owned process can be proven".to_owned()
            }
        }
    }
}

fn classify_launch(process: ProcessObservation, endpoint: EndpointObservation) -> LaunchState {
    match process {
        ProcessObservation::Verified(process) => match endpoint {
            EndpointObservation::Current => LaunchState::Current(process),
            EndpointObservation::Unreachable => LaunchState::Unreachable(process),
            EndpointObservation::Mismatch(reason) => {
                LaunchState::EndpointMismatch { process, reason }
            }
        },
        ProcessObservation::Dead => LaunchState::Dead,
        ProcessObservation::Mismatch(reason) => LaunchState::OwnershipMismatch(reason),
        ProcessObservation::Unverified { process_may_be_live } => {
            LaunchState::Unverified { process_may_be_live }
        }
    }
}

async fn existing_launch(name: &str) -> Result<Option<LaunchRecord>> {
    let Some(record) = registry::read_launch(name) else {
        return Ok(None);
    };
    match record.phase {
        LaunchPhase::Starting => bail!(
            "launched session '{name}' is still in the durable starting phase; wait for its launcher or close it explicitly if startup failed"
        ),
        LaunchPhase::Unknown => bail!(
            "launched session '{name}' has no verified lifecycle phase; recovery state was retained at {}",
            registry::dir().display()
        ),
        LaunchPhase::Ready => {}
    }
    match inspect_launch(&record).await {
        LaunchState::Dead => {
            registry::remove(&record.name);
            registry::remove_launch_profile(&record);
            registry::remove_launch(&record.name);
            Ok(None)
        }
        LaunchState::OwnershipMismatch(reason) => bail!(
            "launched session '{name}' has an ownership mismatch ({reason}); recovery state was retained at {}",
            registry::dir().display()
        ),
        LaunchState::Unverified { process_may_be_live } => bail!(
            "launched session '{name}' has no verifiable process ownership (process may be live: {process_may_be_live}); recovery state was retained at {}",
            registry::dir().display()
        ),
        _ => Ok(Some(record)),
    }
}

pub struct LaunchOptions {
    pub url: String,
    pub name: Option<String>,
    pub browser: Option<PathBuf>,
    pub headless: bool,
    pub fresh: bool,
    pub profile: Option<String>,
    pub keep_profile: bool,
    pub viewport: Option<String>,
    pub startup_capture: bool,
    pub reuse: bool,
    pub replace: bool,
    pub timezone: Option<String>,
    pub locale: Option<String>,
    pub dark: bool,
    pub offline: bool,
    pub throttle: Option<String>,
}

/// How an Electron app is launched and where its renderer CDP port comes from. Unlike a browser
/// launch, the command is arbitrary and the port is wired into the app's own environment (Electron
/// calls `app.commandLine.appendSwitch('remote-debugging-port', …)` itself) or passed as a flag.
pub struct ElectronLaunchOptions {
    /// The command and its arguments, for example `["pnpm", "--filter", "studio", "start:prebuilt"]`.
    pub command: Vec<String>,
    pub name: Option<String>,
    /// Working directory to spawn the command in. Defaults to the current directory.
    pub cwd: Option<PathBuf>,
    /// A fixed renderer CDP port, or `None` to allocate a free one.
    pub cdp_port: Option<u16>,
    /// Env var the app reads the renderer CDP port from, for example `STUDIO_CDP_PORT`.
    pub cdp_env: Option<String>,
    /// Extra `KEY=VALUE` environment entries to set for the spawned process.
    pub env: Vec<String>,
    /// Extra process arguments, with `{cdp_port}` replaced by the resolved port — for apps that take
    /// the Chromium flag directly (`--remote-debugging-port={cdp_port}`) instead of via `--cdp-env`.
    pub electron_args: Vec<String>,
    /// Renderer target to select by title/url substring once the endpoint is up.
    pub renderer_target: Option<String>,
    pub startup_capture: bool,
    pub reuse: bool,
    pub replace: bool,
}

pub enum ProfileOp {
    Ls,
    New { name: String },
    Clone { name: String, from: String },
}

/// Run one command against the warm Attachment for `app`, attaching first if needed. Returns whether
/// the result was a success (drives the process exit code).
pub async fn query(app: Option<&str>, json: bool, command: Command) -> Result<bool> {
    let record = ensure(app, &TrackKind::ALL).await?;
    let reply = send(&record, &Query { command, json }).await?;
    println!("{}", reply.output);
    Ok(reply.ok)
}

/// Resolve the warm Attachment for `app`, lazily attaching with all tracks if none is live. The
/// entry point for the interactive session, which then reuses the returned record for every command.
pub async fn ensure_attached(app: Option<&str>) -> Result<Record> {
    ensure(app, &TrackKind::ALL).await
}

/// Run one command against a known Attachment and return its `Reply` verbatim (no printing) — the
/// interactive session renders the result itself.
pub async fn run_one(record: &Record, command: Command, json: bool) -> Result<Reply> {
    send(record, &Query { command, json }).await
}

/// Open a live Timeline subscription to an Attachment. Sends `Subscribe`, then reads `Frame`s off
/// the socket on a spawned task into the returned channel; the channel closes when the daemon
/// disconnects or the socket dies.
pub async fn subscribe(record: &Record, since_ms: u64) -> Result<Receiver<Frame>> {
    let stream = UnixStream::connect(registry::socket_path(&record.name))
        .await
        .with_context(|| format!("subscribe to attachment '{}'", record.name))?;
    let mut reader = BufReader::new(stream);
    let mut line =
        serde_json::to_string(&Query { command: Command::Subscribe { since_ms }, json: false })?;
    line.push('\n');
    reader.get_mut().write_all(line.as_bytes()).await?;

    let (sender, receiver) = mpsc::channel(LIVE_FRAME_CAPACITY);
    tokio::spawn(async move {
        let mut buffer = String::new();
        loop {
            buffer.clear();
            match reader.read_line(&mut buffer).await {
                Ok(0) | Err(_) => break,
                // A frame that fails to decode is skipped, never fatal — one malformed frame must
                // not silently kill the whole live stream (it once did: a wire-type collision).
                Ok(_) => {
                    if let Some(frame) = decode_frame(buffer.trim()) {
                        // This reader owns only the subscription's unix socket. Waiting for the
                        // bounded local UI queue cannot stall the browser websocket; the daemon's
                        // bounded per-subscriber queue accounts for any upstream pressure.
                        if sender.send(frame).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
    Ok(receiver)
}

/// Decode one subscription line into a [`Frame`], or `None` if it doesn't parse. The seam the
/// reader skips on and the wire-contract tests exercise.
fn decode_frame(line: &str) -> Option<Frame> {
    serde_json::from_str(line).ok()
}

/// `kit cdp attach` — pre-warm an Attachment with a chosen Track set (idempotent).
pub async fn attach(app: Option<&str>, tracks: Vec<TrackKind>, json: bool) -> Result<()> {
    let record = ensure(app, &tracks).await?;
    let reply = send(&record, &Query { command: Command::Status, json }).await?;
    println!("{}", reply.output);
    Ok(())
}

/// `kit cdp detach` — dispose one or all Attachments.
pub async fn detach(app: Option<&str>, all: bool) -> Result<()> {
    let live = registry::reconcile();
    let targets: Vec<Record> = if all {
        live
    } else if let Some(selector) = app {
        live.into_iter().filter(|record| matches(record, selector)).collect()
    } else if live.len() <= 1 {
        live
    } else {
        bail!("multiple attachments — pass --app <selector> or --all");
    };

    if targets.is_empty() {
        println!("no matching attachment");
        return Ok(());
    }
    let mut failures = Vec::new();
    for record in targets {
        match stop_attachment_record(&record).await {
            Ok(()) => println!("detached {}", record.name),
            Err(reason) => failures.push(format!("{}: {reason}", record.name)),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("failed to detach attachment(s):\n  {}", failures.join("\n  "))
    }
}

/// `kit cdp ls` — live Attachments and their health.
pub fn ls(json: bool) -> Result<()> {
    let live = registry::reconcile();
    if json {
        println!("{}", serde_json::to_string_pretty(&live)?);
        return Ok(());
    }
    if live.is_empty() {
        println!("no attachments (a command will attach lazily)");
        return Ok(());
    }
    for record in &live {
        println!(
            "{:<16} {:<12} :{:<6} pid {:<8} up {:<6} tracks {}",
            record.name,
            record.app,
            record.port,
            record.pid,
            human_ms(now_unix_ms().saturating_sub(record.started_at_ms)),
            record.tracks.join(",")
        );
    }
    Ok(())
}

/// `kit cdp gc` — sweep dead Attachments.
pub fn gc(json: bool) -> Result<()> {
    let before: Vec<String> = registry::all().into_iter().map(|record| record.name).collect();
    let after: Vec<String> = registry::reconcile().into_iter().map(|record| record.name).collect();
    let swept: Vec<&String> = before.iter().filter(|name| !after.contains(name)).collect();
    if json {
        println!("{}", serde_json::json!({ "swept": swept, "live": after }));
    } else if swept.is_empty() {
        println!("nothing to sweep ({} live)", after.len());
    } else {
        println!("swept {} dead ({} live)", swept.len(), after.len());
    }
    Ok(())
}

/// `kit cdp` (bare) — one-call orientation: instances available + live attachments.
pub async fn overview(json: bool) -> Result<()> {
    let live = registry::reconcile();
    let launches = active_launches().await;
    let instances = cdp::discover().await;

    if json {
        let instances: Vec<_> = instances
            .iter()
            .map(|instance| {
                serde_json::json!({
                    "name": instance.name(),
                    "app": instance.endpoint.app,
                    "port": instance.endpoint.port,
                    "pid": instance.pid,
                    "worktree": instance.worktree,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "instances": instances, "attachments": live, "launched": launches })
        );
        return Ok(());
    }

    println!("instances:");
    if instances.is_empty() {
        println!("  (none — launch the app with a remote debugging port)");
    }
    for instance in &instances {
        println!(
            "  {:<16} {:<12} :{}",
            instance.name(),
            instance.endpoint.app,
            instance.endpoint.port
        );
    }
    println!("\nattachments:");
    if live.is_empty() {
        println!("  (none — a command will attach lazily)");
    }
    for record in &live {
        println!("  {:<16} {:<12} :{:<6} pid {}", record.name, record.app, record.port, record.pid);
    }
    println!("\nlaunched:");
    if launches.is_empty() {
        println!("  (none)");
    }
    for launch in &launches {
        println!(
            "  {:<16} :{:<6} pid {:<8} {:<8?} {}",
            launch.name, launch.port, launch.browser_pid, launch.phase, launch.url
        );
    }
    Ok(())
}

pub async fn launch(options: LaunchOptions, json: bool) -> Result<()> {
    if options.reuse && options.replace {
        bail!("choose only one of --reuse or --replace");
    }

    let name = session_name(options.name.as_deref(), &options.url)?;
    let existing = existing_launch(&name).await?;
    if let Some(existing) = existing {
        if options.replace {
            close_records([existing].into_iter(), false).await?;
        } else if options.reuse {
            let record = ensure_launch_attached(&existing, &TrackKind::ALL).await?;
            let _ = send(
                &record,
                &Query { command: Command::Configure(settings_from(&options)), json: false },
            )
            .await;
            let reply = send(
                &record,
                &Query {
                    command: Command::Navigate { target: None, url: options.url.clone() },
                    json,
                },
            )
            .await?;
            let mut updated = existing;
            updated.url = options.url.clone();
            updated.viewport = options.viewport.clone();
            updated.timezone = options.timezone.clone();
            updated.locale = options.locale.clone();
            updated.dark = options.dark;
            updated.offline = options.offline;
            updated.throttle = options.throttle.clone();
            let _ = registry::write_launch(&updated);
            println!("{}", reply.output);
            return Ok(());
        } else {
            bail!("launched session '{name}' already exists — pass --reuse or --replace");
        }
    }

    let browser = find_browser(options.browser.as_deref())?;
    let (profile_dir, profile_name, temp_profile) = profile_dir(&name, &options)?;
    std::fs::create_dir_all(&profile_dir)
        .with_context(|| format!("create {}", profile_dir.display()))?;
    clear_profile_launch_state(&profile_dir)?;
    let artifact_dir = registry::artifact_dir(&name);
    std::fs::create_dir_all(&artifact_dir)
        .with_context(|| format!("create {}", artifact_dir.display()))?;
    let browser_log = std::fs::File::create(artifact_dir.join("browser.log"))
        .with_context(|| format!("create {}", artifact_dir.join("browser.log").display()))?;

    let mut command = std::process::Command::new(&browser);
    command
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .stdin(Stdio::null())
        .stdout(Stdio::from(browser_log.try_clone()?))
        .stderr(Stdio::from(browser_log));
    if options.headless {
        command.arg("--headless=new");
    }
    if let Some(viewport) = &options.viewport {
        if let Some((width, height)) = parse_viewport(viewport) {
            command.arg(format!("--window-size={width},{height}"));
        }
    }
    if options.startup_capture {
        command.arg("about:blank");
    } else {
        command.arg(&options.url);
    }
    // Keep launched browsers alive after the short-lived CLI process exits. The daemon is detached
    // the same way; without this, headless Chrome can disappear before the next `kit cdp` command.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let started_after = SystemTime::now();
    let mut child =
        command.spawn().with_context(|| format!("launch browser {}", browser.display()))?;
    let browser_pid = child.id();
    let leader_identity = match registry::process_identity(browser_pid) {
        Some(identity) => identity,
        None => {
            let _ = child.kill();
            remove_temp_profile_dir(&profile_dir, temp_profile, options.keep_profile);
            bail!("read controlled browser process identity for pid {browser_pid}");
        }
    };
    let port = match wait_devtools_port(&profile_dir, started_after).await {
        Ok(port) => port,
        Err(error) => {
            if let Err(cleanup) = abort_spawned_session(leader_identity).await {
                return Err(error.context(format!(
                    "controlled browser cleanup failed ({cleanup}); profile retained at {}",
                    profile_dir.display()
                )));
            }
            remove_temp_profile_dir(&profile_dir, temp_profile, options.keep_profile);
            return Err(error);
        }
    };
    let endpoint = match cdp::browser_endpoint(port).await {
        Some(endpoint) => endpoint,
        None => {
            if let Err(cleanup) = abort_spawned_session(leader_identity).await {
                bail!(
                    "browser did not expose a valid Chrome DevTools endpoint on port {port}; controlled browser cleanup failed ({cleanup}); profile retained at {}",
                    profile_dir.display()
                );
            }
            remove_temp_profile_dir(&profile_dir, temp_profile, options.keep_profile);
            bail!("browser did not expose a valid Chrome DevTools endpoint on port {port}");
        }
    };
    let ownership = match capture_launch_ownership(leader_identity, port) {
        Ok(ownership) => ownership,
        Err(error) => {
            if let Err(cleanup) = abort_spawned_session(leader_identity).await {
                return Err(error.context(format!(
                    "verify controlled browser ownership; cleanup failed ({cleanup}); profile retained at {}",
                    profile_dir.display()
                )));
            }
            remove_temp_profile_dir(&profile_dir, temp_profile, options.keep_profile);
            return Err(error.context("verify controlled browser ownership"));
        }
    };

    let mut launch = LaunchRecord {
        name: name.clone(),
        phase: LaunchPhase::Starting,
        url: options.url.clone(),
        browser: browser.display().to_string(),
        browser_pid,
        ownership: Some(ownership),
        launch_kind: Some(LaunchKind::Chrome),
        render_mode: if options.headless { RenderMode::HeadlessNew } else { RenderMode::Windowed },
        gpu_mode: GpuMode::BrowserDefault,
        port,
        devtools_ws_url: Some(endpoint.ws_url),
        profile_dir,
        profile_name,
        temp_profile,
        keep_profile: options.keep_profile,
        artifact_dir,
        started_at_ms: now_unix_ms(),
        startup_capture: options.startup_capture,
        headless: options.headless,
        viewport: options.viewport.clone(),
        timezone: options.timezone.clone(),
        locale: options.locale.clone(),
        dark: options.dark,
        offline: options.offline,
        throttle: options.throttle.clone(),
    };
    if let Err(error) = registry::write_launch(&launch) {
        return fail_after_launch_ownership(
            &launch,
            None,
            error.context("persist controlled browser ownership before attachment startup"),
            false,
        )
        .await;
    }

    let mut spawned_attachment = None;
    let completion: Result<()> = async {
        let identity = spawn_daemon(&name, &name, port, browser_pid, false, &TrackKind::ALL)?;
        spawned_attachment = Some(identity);
        let record = wait_ready(&name).await?;
        verify_spawned_attachment_record(&record, identity)?;

        require_reply(
            send(
                &record,
                &Query { command: Command::Mark { name: "launch".to_owned() }, json: false },
            )
            .await
            .context("set launch Timeline mark")?,
            "set launch Timeline mark",
        )?;
        require_reply(
            send(
                &record,
                &Query { command: Command::Configure(settings_from(&options)), json: false },
            )
            .await
            .context("configure controlled browser session")?,
            "configure controlled browser session",
        )?;
        if options.startup_capture {
            require_reply(
                send(
                    &record,
                    &Query {
                        command: Command::Navigate { target: None, url: options.url.clone() },
                        json,
                    },
                )
                .await
                .context("navigate controlled browser after capture startup")?,
                "navigate controlled browser after capture startup",
            )?;
        }
        launch.phase = LaunchPhase::Ready;
        registry::write_launch(&launch).context("promote controlled browser launch to ready")?;
        print_launch(&launch, json)
    }
    .await;

    match completion {
        Ok(()) => Ok(()),
        Err(error) => fail_after_launch_ownership(&launch, spawned_attachment, error, true).await,
    }
}

/// `kit cdp launch-electron` — spawn an Electron app, wait for the renderer CDP endpoint it exposes,
/// and attach to it. The app owns its own page, so there is no navigation step: capture runs from
/// daemon-attach onward and the renderer target is selected by `--renderer-target`.
pub async fn launch_electron(options: ElectronLaunchOptions, json: bool) -> Result<()> {
    if options.reuse && options.replace {
        bail!("choose only one of --reuse or --replace");
    }
    let Some(program) = options.command.first().cloned() else {
        bail!("no command to launch — pass the app command after `--`");
    };

    let name = electron_session_name(options.name.as_deref(), &program)?;
    let existing = existing_launch(&name).await?;
    if let Some(existing) = existing {
        if options.replace {
            close_records([existing].into_iter(), false).await?;
        } else if options.reuse {
            let record = ensure_launch_attached(&existing, &TrackKind::ALL).await?;
            let reply = send(&record, &Query { command: Command::Status, json }).await?;
            println!("{}", reply.output);
            return Ok(());
        } else {
            bail!("launched session '{name}' already exists — pass --reuse or --replace");
        }
    }

    let port = match options.cdp_port {
        Some(port) => port,
        None => allocate_cdp_port()?,
    };
    if options.cdp_env.is_none() && options.electron_args.is_empty() {
        bail!("no way to pass the CDP port — set --cdp-env <VAR> or --electron-arg '--remote-debugging-port={{cdp_port}}'");
    }

    let cwd = match options.cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir().context("resolve current directory")?,
    };
    let artifact_dir = registry::artifact_dir(&name);
    std::fs::create_dir_all(&artifact_dir)
        .with_context(|| format!("create {}", artifact_dir.display()))?;
    let app_log = std::fs::File::create(artifact_dir.join("app.log"))
        .with_context(|| format!("create {}", artifact_dir.join("app.log").display()))?;

    let mut command = std::process::Command::new(&program);
    command
        .args(&options.command[1..])
        .args(options.electron_args.iter().map(|arg| arg.replace("{cdp_port}", &port.to_string())))
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(app_log.try_clone()?))
        .stderr(Stdio::from(app_log));
    if let Some(var) = &options.cdp_env {
        command.env(var, port.to_string());
    }
    for entry in &options.env {
        let (key, value) = entry
            .split_once('=')
            .with_context(|| format!("invalid --env '{entry}' — expected KEY=VALUE"))?;
        command.env(key, value);
    }
    // Detach into its own session so the app outlives this short-lived CLI process, exactly like a
    // browser launch — without this the app can die before the next `kit cdp` command runs.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = command.spawn().with_context(|| format!("launch {program}"))?;
    let app_pid = child.id();
    let leader_identity = match registry::process_identity(app_pid) {
        Some(identity) => identity,
        None => {
            let _ = child.kill();
            bail!("read controlled app process identity for pid {app_pid}");
        }
    };
    let endpoint = match wait_electron_endpoint(port).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            if let Err(cleanup) = abort_spawned_session(leader_identity).await {
                return Err(error.context(format!(
                    "controlled Electron cleanup failed ({cleanup}); artifacts retained at {}",
                    artifact_dir.display()
                )));
            }
            return Err(error);
        }
    };
    let ownership = match capture_launch_ownership(leader_identity, port) {
        Ok(ownership) => ownership,
        Err(error) => {
            if let Err(cleanup) = abort_spawned_session(leader_identity).await {
                return Err(error.context(format!(
                    "verify controlled Electron ownership; cleanup failed ({cleanup}); artifacts retained at {}",
                    artifact_dir.display()
                )));
            }
            return Err(error.context("verify controlled Electron ownership"));
        }
    };

    let mut launch = LaunchRecord {
        name: name.clone(),
        phase: LaunchPhase::Starting,
        url: format!("electron://{}", display_command(&options.command)),
        browser: program,
        browser_pid: app_pid,
        ownership: Some(ownership),
        launch_kind: Some(LaunchKind::Electron),
        render_mode: RenderMode::ApplicationManaged,
        gpu_mode: GpuMode::ApplicationManaged,
        port,
        devtools_ws_url: Some(endpoint.ws_url),
        profile_dir: cwd,
        profile_name: None,
        temp_profile: false,
        keep_profile: true,
        artifact_dir,
        started_at_ms: now_unix_ms(),
        startup_capture: options.startup_capture,
        headless: false,
        viewport: None,
        timezone: None,
        locale: None,
        dark: false,
        offline: false,
        throttle: None,
    };
    if let Err(error) = registry::write_launch(&launch) {
        return fail_after_launch_ownership(
            &launch,
            None,
            error.context("persist controlled Electron ownership before attachment startup"),
            false,
        )
        .await;
    }

    let mut spawned_attachment = None;
    let completion: Result<()> = async {
        let identity = spawn_daemon(&name, &name, port, app_pid, true, &TrackKind::ALL)?;
        spawned_attachment = Some(identity);
        let record = wait_ready(&name).await?;
        verify_spawned_attachment_record(&record, identity)?;

        require_reply(
            send(
                &record,
                &Query { command: Command::Mark { name: "launch".to_owned() }, json: false },
            )
            .await
            .context("set Electron launch Timeline mark")?,
            "set Electron launch Timeline mark",
        )?;
        if let Some(target) = &options.renderer_target {
            require_reply(
                send(
                    &record,
                    &Query {
                        command: Command::Eval {
                            target: Some(target.clone()),
                            expr: "location.href".to_owned(),
                        },
                        json: false,
                    },
                )
                .await
                .context("verify requested Electron renderer target")?,
                "verify requested Electron renderer target",
            )?;
        }
        launch.phase = LaunchPhase::Ready;
        registry::write_launch(&launch).context("promote controlled Electron launch to ready")?;
        print_launch(&launch, json)
    }
    .await;

    match completion {
        Ok(()) => Ok(()),
        Err(error) => fail_after_launch_ownership(&launch, spawned_attachment, error, true).await,
    }
}

pub async fn launched(json: bool) -> Result<()> {
    let launches = active_launches().await;
    if json {
        println!("{}", serde_json::to_string_pretty(&launches)?);
        return Ok(());
    }
    if launches.is_empty() {
        println!("no launched sessions");
        return Ok(());
    }
    for launch in &launches {
        println!(
            "{:<16} :{:<6} pid {:<8} {:<8?} profile {}  {}",
            launch.name,
            launch.port,
            launch.browser_pid,
            launch.phase,
            launch.profile_name.as_deref().unwrap_or(if launch.temp_profile {
                "temp"
            } else {
                "custom"
            }),
            launch.url
        );
    }
    Ok(())
}

pub async fn close_launched(name: Option<&str>, all: bool) -> Result<()> {
    let launches = registry::all_launches();
    let targets: Vec<LaunchRecord> = if all {
        launches
    } else if let Some(name) = name {
        launches.into_iter().filter(|record| record.name == name).collect()
    } else if launches.len() <= 1 {
        launches
    } else {
        bail!("multiple launched sessions — pass a name or --all");
    };
    if targets.is_empty() {
        println!("no matching launched session");
        return Ok(());
    }
    close_records(targets.into_iter(), true).await
}

fn capture_launch_ownership(leader: ProcessIdentity, port: u16) -> Result<LaunchOwnership> {
    if leader.session_id != leader.pid || leader.process_group_id != leader.pid {
        bail!("controlled launch pid {} did not enter its own process session/group", leader.pid);
    }
    let endpoint_pid =
        cdp::owner_pid(port).with_context(|| format!("resolve process owning CDP port {port}"))?;
    let endpoint = registry::process_identity(endpoint_pid)
        .with_context(|| format!("read CDP endpoint process identity for pid {endpoint_pid}"))?;
    if endpoint.session_id != leader.session_id {
        bail!(
            "CDP endpoint pid {} belongs to session {}, expected controlled session {}",
            endpoint.pid,
            endpoint.session_id,
            leader.session_id
        );
    }
    Ok(LaunchOwnership { leader, endpoint })
}

fn observe_launch_process(launch: &LaunchRecord) -> ProcessObservation {
    let Some(ownership) = launch.ownership.as_ref() else {
        return ProcessObservation::Unverified {
            process_may_be_live: registry::is_alive(launch.browser_pid)
                || cdp::owner_pid(launch.port).is_some(),
        };
    };

    let stored = owned_identity_records(ownership);
    let current: Vec<ProcessIdentity> =
        stored.iter().filter_map(|expected| registry::process_identity(expected.pid)).collect();
    classify_owned_processes(
        ownership,
        &current,
        registry::processes_in_session(ownership.leader.session_id),
    )
}

fn owned_identity_records(ownership: &LaunchOwnership) -> Vec<ProcessIdentity> {
    let mut stored = vec![ownership.leader];
    if ownership.endpoint.pid != ownership.leader.pid {
        stored.push(ownership.endpoint);
    }
    stored
}

fn classify_owned_processes(
    ownership: &LaunchOwnership,
    current: &[ProcessIdentity],
    members: Vec<ProcessIdentity>,
) -> ProcessObservation {
    let stored = owned_identity_records(ownership);
    let mut exact = false;
    let mut mismatches = Vec::new();
    for expected in &stored {
        if let Some(current) = current.iter().find(|current| current.pid == expected.pid) {
            if current == expected {
                exact = true;
            } else {
                mismatches.push(format!(
                    "pid {} was reused (recorded start {}, current start {})",
                    expected.pid, expected.start_ticks, current.start_ticks
                ));
            }
        }
    }

    if exact {
        return ProcessObservation::Verified(VerifiedLaunchProcess {
            session_id: ownership.leader.session_id,
            known: members.into_iter().map(|identity| (identity.pid, identity)).collect(),
        });
    }
    if members.is_empty() && mismatches.is_empty() {
        ProcessObservation::Dead
    } else {
        let reason = if mismatches.is_empty() {
            format!(
                "recorded leader/endpoint are gone but session {} still has processes",
                ownership.leader.session_id
            )
        } else {
            mismatches.join("; ")
        };
        ProcessObservation::Mismatch(reason)
    }
}

async fn inspect_launch(launch: &LaunchRecord) -> LaunchState {
    let process = observe_launch_process(launch);
    let ProcessObservation::Verified(mut verified) = process else {
        return classify_launch(process, EndpointObservation::Unreachable);
    };
    let Some(expected_ws_url) = launch.devtools_ws_url.as_ref() else {
        return classify_launch(
            ProcessObservation::Verified(verified),
            EndpointObservation::Mismatch("launch record has no websocket identity".to_owned()),
        );
    };
    let Some(endpoint) = cdp::browser_endpoint(launch.port).await else {
        return classify_launch(
            ProcessObservation::Verified(verified),
            EndpointObservation::Unreachable,
        );
    };
    if &endpoint.ws_url != expected_ws_url {
        return classify_launch(
            ProcessObservation::Verified(verified),
            EndpointObservation::Mismatch(format!(
                "port {} now exposes a different browser websocket",
                launch.port
            )),
        );
    }
    let Some(endpoint_pid) = cdp::owner_pid(launch.port) else {
        return classify_launch(
            ProcessObservation::Verified(verified),
            EndpointObservation::Mismatch("CDP port owner could not be resolved".to_owned()),
        );
    };
    let Some(identity) = registry::process_identity(endpoint_pid) else {
        return classify_launch(
            ProcessObservation::Verified(verified),
            EndpointObservation::Mismatch(format!(
                "CDP port owner pid {endpoint_pid} disappeared during inspection"
            )),
        );
    };
    if identity.session_id != verified.session_id {
        let expected_session = verified.session_id;
        return classify_launch(
            ProcessObservation::Verified(verified),
            EndpointObservation::Mismatch(format!(
                "CDP port owner pid {} belongs to session {}, expected {}",
                identity.pid, identity.session_id, expected_session
            )),
        );
    }
    verified.known.insert(identity.pid, identity);
    classify_launch(ProcessObservation::Verified(verified), EndpointObservation::Current)
}

async fn close_records(records: impl Iterator<Item = LaunchRecord>, print: bool) -> Result<()> {
    let mut failures = Vec::new();
    for launch in records {
        let outcome = close_launch_runtime(&launch).await;
        match outcome {
            Ok(()) => {
                registry::remove_launch_profile(&launch);
                registry::remove_launch(&launch.name);
                if print {
                    println!("closed {}", launch.name);
                }
            }
            Err(reason) => failures.push(format!(
                "{}: {reason}; launch/profile recovery state retained at {}",
                launch.name,
                registry::dir().display()
            )),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("failed to close launched session(s):\n  {}", failures.join("\n  "))
    }
}

async fn close_launch_runtime(launch: &LaunchRecord) -> Result<(), String> {
    let state = inspect_launch(launch).await;
    let action = close_action(&state);
    match (action, state) {
        (CloseAction::TerminateWithCdp, LaunchState::Current(process)) => {
            terminate_verified_launch(launch, process, true).await
        }
        (CloseAction::TerminateOwnedSession, LaunchState::Unreachable(process))
        | (
            CloseAction::TerminateOwnedSession,
            LaunchState::EndpointMismatch { process, .. },
        ) => terminate_verified_launch(launch, process, false).await,
        (CloseAction::CleanupDead, LaunchState::Dead) => stop_launch_attachment(launch).await,
        (CloseAction::RetainRecoveryState, LaunchState::OwnershipMismatch(reason)) => Err(format!(
            "ownership mismatch ({reason}); refusing to signal a possibly reused process"
        )),
        (CloseAction::RetainRecoveryState, LaunchState::Unverified { process_may_be_live }) => {
            Err(format!(
                "process ownership is unverified (process may be live: {process_may_be_live}); refusing to signal by pid alone"
            ))
        }
        _ => unreachable!("close action and launch state are derived together"),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LaunchRecoveryEvidence<'a> {
    failure: &'a str,
    launch: &'a LaunchRecord,
}

fn write_launch_recovery_evidence(launch: &LaunchRecord, failure: &str) -> Result<PathBuf> {
    let path = launch.artifact_dir.join("launch-recovery.json");
    let evidence = LaunchRecoveryEvidence { failure, launch };
    let json = serde_json::to_string_pretty(&evidence)?;
    std::fs::write(&path, json).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn combine_cleanup_results(
    browser: Result<(), String>,
    attachment: Result<(), String>,
) -> Result<(), String> {
    match (browser, attachment) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(browser), Ok(())) => Err(format!("controlled session: {browser}")),
        (Ok(()), Err(attachment)) => Err(format!("attachment: {attachment}")),
        (Err(browser), Err(attachment)) => {
            Err(format!("controlled session: {browser}; attachment: {attachment}"))
        }
    }
}

async fn stop_spawned_attachment(
    name: &str,
    identity: Option<ProcessIdentity>,
) -> Result<(), String> {
    let Some(identity) = identity else {
        return Ok(());
    };
    let stopped = abort_spawned_session(identity).await;
    if stopped.is_ok()
        && registry::read(name).is_some_and(|record| {
            record.pid == identity.pid && record.process_start_ticks == Some(identity.start_ticks)
        })
    {
        registry::remove(name);
    }
    stopped
}

async fn fail_after_launch_ownership<T>(
    launch: &LaunchRecord,
    spawned_attachment: Option<ProcessIdentity>,
    failure: Error,
    launch_record_persisted: bool,
) -> Result<T> {
    let failure_text = failure.to_string();
    let evidence = if launch_record_persisted {
        Ok(None)
    } else {
        write_launch_recovery_evidence(launch, &failure_text).map(Some)
    };
    let cleanup = combine_cleanup_results(
        close_launch_runtime(launch).await,
        stop_spawned_attachment(&launch.name, spawned_attachment).await,
    );

    match cleanup {
        Ok(()) => {
            registry::remove_launch_profile(launch);
            registry::remove_launch(&launch.name);
            if let Ok(Some(path)) = evidence {
                let _ = std::fs::remove_file(path);
            }
            Err(failure)
        }
        Err(cleanup) => {
            let ownership = launch
                .ownership
                .as_ref()
                .expect("post-ownership cleanup requires launch ownership");
            let recovery = match evidence {
                Ok(Some(path)) => format!("recovery evidence retained at {}", path.display()),
                Ok(None) => format!(
                    "launch/profile recovery state retained under {}",
                    registry::dir().display()
                ),
                Err(error) => format!(
                    "recovery evidence write also failed ({error}); artifacts retained at {}; owned session {} leader pid {} endpoint pid {}",
                    launch.artifact_dir.display(),
                    ownership.leader.session_id,
                    ownership.leader.pid,
                    ownership.endpoint.pid,
                ),
            };
            Err(failure.context(format!("post-ownership cleanup failed ({cleanup}); {recovery}")))
        }
    }
}

async fn terminate_verified_launch(
    launch: &LaunchRecord,
    mut process: VerifiedLaunchProcess,
    endpoint_current: bool,
) -> Result<(), String> {
    if endpoint_current {
        let daemon_closed = match registry::read(&launch.name).filter(|record| {
            record.port == launch.port
                && record.root_pid == launch.browser_pid
                && registry::record_process_is_current(record)
        }) {
            Some(record) => send(&record, &Query { command: Command::CloseBrowser, json: false })
                .await
                .is_ok_and(|reply| reply.ok),
            None => false,
        };
        if !daemon_closed {
            let _ = close_browser_direct(launch).await;
        }
    }

    if wait_for_session_shutdown(&mut process, CLOSE_GRACE).await? {
        return stop_launch_attachment(launch).await;
    }
    signal_verified_session(&mut process, libc::SIGTERM)?;
    if wait_for_session_shutdown(&mut process, CLOSE_TERM_GRACE).await? {
        return stop_launch_attachment(launch).await;
    }
    signal_verified_session(&mut process, libc::SIGKILL)?;
    if wait_for_session_shutdown(&mut process, CLOSE_KILL_GRACE).await? {
        return stop_launch_attachment(launch).await;
    }

    let remaining = verified_session_members(&mut process)?
        .into_iter()
        .map(|identity| identity.pid.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "verified process session {} did not stop; remaining pid(s): {remaining}",
        process.session_id
    ))
}

/// Tear down a just-spawned controlled session from its exact `setsid` leader identity. This is
/// used for both pre-record launch failure and spawned Attachment cleanup, so descendants are not
/// orphaned by killing only a wrapper pid.
async fn abort_spawned_session(leader: ProcessIdentity) -> Result<(), String> {
    let members = registry::processes_in_session(leader.session_id);
    if members.is_empty() {
        return Ok(());
    }
    if !members.contains(&leader) {
        return Err(format!(
            "session {} remains but its leader identity no longer matches",
            leader.session_id
        ));
    }
    let mut process = VerifiedLaunchProcess {
        session_id: leader.session_id,
        known: members.into_iter().map(|identity| (identity.pid, identity)).collect(),
    };
    signal_verified_session(&mut process, libc::SIGTERM)?;
    if wait_for_session_shutdown(&mut process, CLOSE_TERM_GRACE).await? {
        return Ok(());
    }
    signal_verified_session(&mut process, libc::SIGKILL)?;
    if wait_for_session_shutdown(&mut process, CLOSE_KILL_GRACE).await? {
        Ok(())
    } else {
        Err(format!("controlled session {} did not stop", process.session_id))
    }
}

fn verified_session_members(
    process: &mut VerifiedLaunchProcess,
) -> Result<Vec<ProcessIdentity>, String> {
    let current = registry::processes_in_session(process.session_id);
    if current.is_empty() {
        return Ok(current);
    }
    let anchored = current
        .iter()
        .any(|identity| process.known.get(&identity.pid).is_some_and(|known| known == identity));
    if !anchored {
        return Err(format!(
            "session {} still has processes but none match a recorded identity; refusing further signals",
            process.session_id
        ));
    }
    for identity in &current {
        process.known.insert(identity.pid, *identity);
    }
    Ok(current)
}

fn signal_verified_session(
    process: &mut VerifiedLaunchProcess,
    signal: libc::c_int,
) -> Result<(), String> {
    let members = verified_session_members(process)?;
    let groups: HashSet<u32> = members
        .into_iter()
        .map(|identity| identity.process_group_id)
        .filter(|group| *group > 0)
        .collect();
    for group in groups {
        let result = unsafe { libc::kill(-(group as i32), signal) };
        if result != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Err(format!(
                "signal {signal} to verified process group {group}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

async fn wait_for_session_shutdown(
    process: &mut VerifiedLaunchProcess,
    budget: Duration,
) -> Result<bool, String> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if verified_session_members(process)?.is_empty() {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        sleep(CLOSE_POLL_INTERVAL).await;
    }
}

async fn stop_launch_attachment(launch: &LaunchRecord) -> Result<(), String> {
    let Some(record) = registry::read(&launch.name) else {
        return Ok(());
    };
    if record.port != launch.port || record.root_pid != launch.browser_pid {
        return Err(format!(
            "attachment record ownership mismatch (port/root {}:{}, expected {}:{})",
            record.port, record.root_pid, launch.port, launch.browser_pid
        ));
    }
    stop_attachment_record(&record).await
}

async fn stop_attachment_record(record: &Record) -> Result<(), String> {
    let _ = send(record, &Query { command: Command::Detach, json: false }).await;
    let deadline = tokio::time::Instant::now() + CLOSE_GRACE;
    while registry::record_process_is_current(record) && tokio::time::Instant::now() < deadline {
        sleep(CLOSE_POLL_INTERVAL).await;
    }
    if registry::record_process_is_current(record) {
        let Some(start_ticks) = record.process_start_ticks else {
            return Err(
                "attachment did not stop and its process identity is not verified".to_owned()
            );
        };
        let Some(identity) = registry::process_identity(record.pid) else {
            registry::remove(&record.name);
            return Ok(());
        };
        if identity.start_ticks != start_ticks {
            return Err(format!("attachment pid {} was reused; refusing to signal it", record.pid));
        }
        unsafe { libc::kill(record.pid as i32, libc::SIGTERM) };
        let deadline = tokio::time::Instant::now() + CLOSE_GRACE;
        while registry::record_process_is_current(record) && tokio::time::Instant::now() < deadline
        {
            sleep(CLOSE_POLL_INTERVAL).await;
        }
    }
    if registry::record_process_is_current(record) {
        return Err(format!("attachment pid {} did not stop", record.pid));
    }
    registry::remove(&record.name);
    Ok(())
}

async fn ensure_launch_attached(launch: &LaunchRecord, tracks: &[TrackKind]) -> Result<Record> {
    if launch.phase != LaunchPhase::Ready {
        bail!(
            "launched session '{}' is {:?}, not ready; recovery state was retained",
            launch.name,
            launch.phase
        );
    }
    let state = inspect_launch(launch).await;
    if !matches!(&state, LaunchState::Current(_)) {
        if matches!(&state, LaunchState::Dead) {
            registry::remove(&launch.name);
            registry::remove_launch_profile(launch);
            registry::remove_launch(&launch.name);
        }
        bail!(
            "launched session '{}' cannot be attached: {}; recovery state {}",
            launch.name,
            state.description(),
            if matches!(&state, LaunchState::Dead) { "was cleaned" } else { "was retained" }
        );
    }
    if let Some(record) = registry::read(&launch.name) {
        if send(&record, &Query { command: Command::Ping, json: false }).await.is_ok() {
            return Ok(record);
        }
        registry::remove(&record.name);
    }
    let spawned = spawn_daemon(
        &launch.name,
        &launch.name,
        launch.port,
        launch.browser_pid,
        launch.launch_kind != Some(LaunchKind::Chrome),
        tracks,
    )?;
    wait_for_spawned_attachment(&launch.name, spawned).await
}

async fn close_browser_direct(launch: &LaunchRecord) -> bool {
    let Some(expected_ws_url) = launch.devtools_ws_url.as_ref() else {
        return false;
    };
    let Some(endpoint) = cdp::browser_endpoint(launch.port).await else {
        return false;
    };
    if expected_ws_url != &endpoint.ws_url {
        return false;
    }
    let Ok((conn, _events)) = cdp::CdpConnection::connect(&endpoint.ws_url).await else {
        return false;
    };
    conn.call(None, "Browser.close", serde_json::json!({})).await.is_ok()
}

async fn active_launches() -> Vec<LaunchRecord> {
    let mut live = Vec::new();
    for launch in registry::all_launches() {
        match inspect_launch(&launch).await {
            LaunchState::Dead => {
                registry::remove(&launch.name);
                registry::remove_launch_profile(&launch);
                registry::remove_launch(&launch.name);
            }
            _ => live.push(launch),
        }
    }
    live
}

pub fn profile(op: ProfileOp, json: bool) -> Result<()> {
    match op {
        ProfileOp::Ls => {
            let profiles = list_profiles();
            if json {
                println!("{}", serde_json::to_string_pretty(&profiles)?);
            } else if profiles.is_empty() {
                println!("no profiles");
            } else {
                for profile in profiles {
                    println!("{profile}");
                }
            }
        }
        ProfileOp::New { name } => {
            let dir = named_profile_dir(&name);
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            println!("{}", dir.display());
        }
        ProfileOp::Clone { name, from } => {
            let Some(source) = registry::read_launch(&from) else {
                bail!("no launched session '{from}'");
            };
            let dest = named_profile_dir(&name);
            copy_dir(&source.profile_dir, &dest)?;
            println!("{}", dest.display());
        }
    }
    Ok(())
}

fn settings_from(options: &LaunchOptions) -> LaunchSettings {
    LaunchSettings {
        viewport: options.viewport.clone(),
        timezone: options.timezone.clone(),
        locale: options.locale.clone(),
        dark: options.dark,
        offline: options.offline,
        throttle: options.throttle.clone(),
    }
}

fn print_launch(launch: &LaunchRecord, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(launch)?);
        return Ok(());
    }
    println!("{}", launch.name);
    println!("url        {}", launch.url);
    println!("browser    {}", launch.browser);
    println!("phase      {:?}", launch.phase);
    println!(
        "profile    {}",
        launch.profile_name.as_deref().unwrap_or(if launch.temp_profile {
            "temp"
        } else {
            "custom"
        })
    );
    println!("target     page");
    println!("render     {}", launch.render_mode.as_str());
    println!("gpu        {}", launch.gpu_mode.as_str());
    println!(
        "capture    {}console, exceptions, network, ws, lifecycle",
        if launch.startup_capture { "startup, " } else { "" }
    );
    println!("debug      127.0.0.1:{}", launch.port);
    println!("raw        kit cdp tail --app {}", launch.name);
    Ok(())
}

fn find_browser(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_owned());
    }
    if let Ok(path) = std::env::var("KIT_CDP_BROWSER") {
        return Ok(PathBuf::from(path));
    }
    let candidates = [
        "/Applications/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
    ];
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists() || path.components().count() == 1)
        .context("no Chrome/Chromium browser found — pass --browser or set KIT_CDP_BROWSER")
}

fn session_name(explicit: Option<&str>, url: &str) -> Result<String> {
    let name = explicit.map(str::to_owned).unwrap_or_else(|| default_name(url));
    if is_safe_name(&name) {
        Ok(name)
    } else {
        bail!("unsafe session name '{name}' — use only ASCII letters, numbers, '-' or '_'");
    }
}

fn is_safe_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn profile_dir(name: &str, options: &LaunchOptions) -> Result<(PathBuf, Option<String>, bool)> {
    if options.fresh && options.profile.is_some() {
        bail!("choose either --fresh or --profile, not both");
    }
    if let Some(profile) = &options.profile {
        return Ok((named_profile_dir(profile), Some(profile.clone()), false));
    }
    let dir = registry::temp_profiles_dir().join(format!("{name}-{}", now_unix_ms()));
    Ok((dir, None, true))
}

fn named_profile_dir(name: &str) -> PathBuf {
    registry::profiles_dir().join(sanitize(name))
}

fn clear_profile_launch_state(profile_dir: &Path) -> Result<()> {
    let path = profile_dir.join("DevToolsActivePort");
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove stale {}", path.display())),
    }
}

fn remove_temp_profile_dir(profile_dir: &Path, temp_profile: bool, keep_profile: bool) {
    if temp_profile && !keep_profile && profile_dir.starts_with(registry::temp_profiles_dir()) {
        let _ = std::fs::remove_dir_all(profile_dir);
    }
}

fn list_profiles() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(registry::profiles_dir()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

fn copy_dir(source: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        bail!("profile already exists: {}", dest.display());
    }
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    for entry in std::fs::read_dir(source).with_context(|| format!("read {}", source.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if should_skip_profile_file(&from) {
            continue;
        }
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else if from.is_file() {
            let _ = std::fs::copy(&from, &to);
        }
    }
    Ok(())
}

fn should_skip_profile_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == "DevToolsActivePort" || name.starts_with("Singleton")
}

async fn wait_devtools_port(profile_dir: &Path, started_after: SystemTime) -> Result<u16> {
    let path = profile_dir.join("DevToolsActivePort");
    for _ in 0..DEVTOOLS_TRIES {
        if std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified < started_after)
        {
            sleep(DEVTOOLS_INTERVAL).await;
            continue;
        }
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Some(port) = raw.lines().next().and_then(|line| line.trim().parse().ok()) {
                return Ok(port);
            }
        }
        sleep(DEVTOOLS_INTERVAL).await;
    }
    bail!("browser did not expose DevToolsActivePort in {}", profile_dir.display())
}

/// Grab a free localhost TCP port by binding to `:0` and reading what the OS assigned. The socket is
/// dropped immediately; the app re-binds it microseconds later. A benign race, the standard trick.
fn allocate_cdp_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).context("allocate a free port")?;
    Ok(listener.local_addr().context("read allocated port")?.port())
}

/// Poll the renderer CDP endpoint on a known `port` until the Electron app exposes it. Electron does
/// not write Chrome's `DevToolsActivePort` file, so — unlike a browser launch — we know the port up
/// front and wait on the HTTP `/json/version` handshake instead.
async fn wait_electron_endpoint(port: u16) -> Result<cdp::BrowserEndpoint> {
    for _ in 0..ELECTRON_TRIES {
        if let Some(endpoint) = cdp::browser_endpoint(port).await {
            return Ok(endpoint);
        }
        sleep(ELECTRON_INTERVAL).await;
    }
    bail!("Electron app did not expose a CDP endpoint on port {port} — check the app log")
}

fn electron_session_name(explicit: Option<&str>, program: &str) -> Result<String> {
    let name = explicit.map(str::to_owned).unwrap_or_else(|| default_electron_name(program));
    if is_safe_name(&name) {
        Ok(name)
    } else {
        bail!("unsafe session name '{name}' — use only ASCII letters, numbers, '-' or '_'");
    }
}

fn default_electron_name(program: &str) -> String {
    let stem = Path::new(program).file_stem().and_then(|stem| stem.to_str()).unwrap_or(program);
    sanitize(stem)
}

fn display_command(command: &[String]) -> String {
    command.join(" ")
}

fn parse_viewport(viewport: &str) -> Option<(u64, u64)> {
    let (width, height) = viewport.split_once('x').or_else(|| viewport.split_once('X'))?;
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

fn default_name(url: &str) -> String {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .filter(|part| !part.is_empty())
        .unwrap_or("browser");
    sanitize(host)
}

fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '-' })
        .collect()
}

async fn ensure(app: Option<&str>, tracks: &[TrackKind]) -> Result<Record> {
    if let Some(record) = find(app) {
        return Ok(record);
    }
    if let Some(selector) = app.and_then(registry::read_launch) {
        return ensure_launch_attached(&selector, tracks).await;
    }
    attach_new(app, tracks).await
}

fn find(app: Option<&str>) -> Option<Record> {
    let live = registry::reconcile();
    match app {
        Some(selector) => live.into_iter().find(|record| matches(record, selector)),
        None if live.len() <= 1 => live.into_iter().next(),
        None => live
            .iter()
            .find(|record| record.app.contains("dev"))
            .cloned()
            .or_else(|| live.into_iter().next()),
    }
}

async fn attach_new(app: Option<&str>, tracks: &[TrackKind]) -> Result<Record> {
    let instances = cdp::discover().await;
    if instances.is_empty() {
        bail!("no running CDP instance found — is the app launched with a remote debugging port?");
    }
    let instance = pick(instances, app)?;
    let name = instance.name();
    let selector = app.map(str::to_owned).unwrap_or_else(|| name.clone());

    let spawned =
        spawn_daemon(&name, &selector, instance.endpoint.port, instance.pid, true, tracks)?;
    wait_for_spawned_attachment(&name, spawned).await
}

fn pick(instances: Vec<cdp::Instance>, app: Option<&str>) -> Result<cdp::Instance> {
    match app {
        Some(selector) => instances
            .into_iter()
            .find(|instance| instance.matches(selector))
            .with_context(|| format!("no instance matches '{selector}'")),
        None if instances.len() == 1 => Ok(instances.into_iter().next().unwrap()),
        None => {
            if let Some(dev) =
                instances.iter().find(|instance| instance.endpoint.app.contains("dev")).cloned()
            {
                Ok(dev)
            } else {
                instances.into_iter().next().context("no instance")
            }
        }
    }
}

fn spawn_daemon(
    name: &str,
    selector: &str,
    port: u16,
    root_pid: u32,
    probe_main: bool,
    tracks: &[TrackKind],
) -> Result<ProcessIdentity> {
    std::fs::create_dir_all(registry::dir()).context("create runtime dir")?;
    let exe = std::env::current_exe().context("resolve own path")?;
    let log = std::fs::File::create(registry::log_path(name)).context("open daemon log")?;

    let mut command = std::process::Command::new(exe);
    command
        .arg("cdp")
        .arg("__serve")
        .arg("--name")
        .arg(name)
        .arg("--selector")
        .arg(selector)
        .arg("--port")
        .arg(port.to_string())
        .arg("--root-pid")
        .arg(root_pid.to_string());
    if !probe_main {
        command.arg("--skip-main-probe");
    }
    if !tracks.is_empty() {
        let csv = tracks.iter().map(|track| track.as_str()).collect::<Vec<_>>().join(",");
        command.arg("--track").arg(csv);
    }
    command.stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));

    // Detach into its own session so the daemon outlives this CLI process and its terminal.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().context("spawn cdp daemon")?;
    let pid = child.id();
    let identity = match registry::process_identity(pid) {
        Some(identity) => identity,
        None => {
            let _ = child.kill();
            bail!("read spawned attachment process identity for pid {pid}");
        }
    };
    if identity.session_id != pid || identity.process_group_id != pid {
        let _ = child.kill();
        bail!("spawned attachment pid {pid} did not enter its own process session/group");
    }
    Ok(identity)
}

async fn wait_ready(name: &str) -> Result<Record> {
    for _ in 0..READY_TRIES {
        if let Some(record) = registry::read(name) {
            if send(&record, &Query { command: Command::Ping, json: false }).await.is_ok() {
                return Ok(record);
            }
        }
        sleep(READY_INTERVAL).await;
    }
    bail!("attachment '{name}' did not come up — see {}", registry::log_path(name).display())
}

async fn wait_for_spawned_attachment(name: &str, identity: ProcessIdentity) -> Result<Record> {
    match wait_ready(name).await {
        Ok(record) => match verify_spawned_attachment_record(&record, identity) {
            Ok(()) => Ok(record),
            Err(error) => match stop_spawned_attachment(name, Some(identity)).await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(error.context(format!(
                    "spawned attachment cleanup failed ({cleanup}); pid {} session {}",
                    identity.pid, identity.session_id
                ))),
            },
        },
        Err(error) => match stop_spawned_attachment(name, Some(identity)).await {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!(
                "spawned attachment cleanup failed ({cleanup}); pid {} session {}",
                identity.pid, identity.session_id
            ))),
        },
    }
}

fn verify_spawned_attachment_record(record: &Record, identity: ProcessIdentity) -> Result<()> {
    if record.pid != identity.pid || record.process_start_ticks != Some(identity.start_ticks) {
        bail!(
            "attachment registry identity mismatch: spawned pid {} start {}, recorded pid {} start {:?}",
            identity.pid,
            identity.start_ticks,
            record.pid,
            record.process_start_ticks
        );
    }
    Ok(())
}

async fn send(record: &Record, query: &Query) -> Result<Reply> {
    let stream = UnixStream::connect(registry::socket_path(&record.name))
        .await
        .with_context(|| format!("connect attachment '{}'", record.name))?;
    let mut line = serde_json::to_string(query)?;
    line.push('\n');

    let mut reader = BufReader::new(stream);
    reader.get_mut().write_all(line.as_bytes()).await?;
    let mut response = String::new();
    reader.read_line(&mut response).await.context("read reply")?;
    serde_json::from_str(response.trim()).context("decode reply")
}

fn require_reply(reply: Reply, operation: &str) -> Result<()> {
    if reply.ok {
        Ok(())
    } else {
        bail!("{operation}: {}", reply.output)
    }
}

fn matches(record: &Record, selector: &str) -> bool {
    let needle = selector.to_lowercase();
    record.name.to_lowercase().contains(&needle)
        || record.app.to_lowercase().contains(&needle)
        || record.selector.to_lowercase().contains(&needle)
        || record.port.to_string() == selector
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn human_ms(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verified() -> VerifiedLaunchProcess {
        VerifiedLaunchProcess { session_id: 42, known: HashMap::new() }
    }

    fn process_identity() -> ProcessIdentity {
        ProcessIdentity { pid: 42, start_ticks: 7, process_group_id: 42, session_id: 42 }
    }

    fn attachment_record() -> Record {
        Record {
            name: "test".to_owned(),
            app: "test".to_owned(),
            selector: "test".to_owned(),
            port: 9222,
            pid: 42,
            process_start_ticks: Some(7),
            root_pid: 100,
            started_at_ms: 0,
            tracks: Vec::new(),
        }
    }

    /// The reader's contract: a valid frame decodes, anything malformed becomes `None` so the loop
    /// skips it instead of killing the stream. (Wire-shape coverage lives in `protocol`/`timeline`.)
    #[test]
    fn decode_frame_keeps_valid_drops_garbage() {
        let wire = serde_json::to_string(&Frame::Backfill(vec![])).unwrap();
        assert!(matches!(decode_frame(&wire), Some(Frame::Backfill(_))));

        assert!(decode_frame("not json").is_none());
        assert!(decode_frame("").is_none());
        assert!(decode_frame("{\"Unknown\":1}").is_none());
    }

    #[test]
    fn launch_state_distinguishes_current_unreachable_mismatch_and_dead() {
        assert!(matches!(
            classify_launch(ProcessObservation::Verified(verified()), EndpointObservation::Current,),
            LaunchState::Current(_)
        ));
        assert!(matches!(
            classify_launch(
                ProcessObservation::Verified(verified()),
                EndpointObservation::Unreachable,
            ),
            LaunchState::Unreachable(_)
        ));
        assert!(matches!(
            classify_launch(
                ProcessObservation::Verified(verified()),
                EndpointObservation::Mismatch("different websocket".to_owned()),
            ),
            LaunchState::EndpointMismatch { .. }
        ));
        assert_eq!(
            classify_launch(ProcessObservation::Dead, EndpointObservation::Current),
            LaunchState::Dead
        );
    }

    #[test]
    fn cleanup_decision_retains_ambiguous_ownership_and_terminates_verified_unreachable() {
        assert_eq!(
            close_action(&LaunchState::Unreachable(verified())),
            CloseAction::TerminateOwnedSession
        );
        assert_eq!(close_action(&LaunchState::Dead), CloseAction::CleanupDead);
        assert_eq!(
            close_action(&LaunchState::OwnershipMismatch("pid reused".to_owned())),
            CloseAction::RetainRecoveryState
        );
        assert_eq!(
            close_action(&LaunchState::Unverified { process_may_be_live: true }),
            CloseAction::RetainRecoveryState
        );
    }

    #[test]
    fn post_ownership_cleanup_succeeds_only_when_session_and_attachment_both_stop() {
        assert_eq!(combine_cleanup_results(Ok(()), Ok(())), Ok(()));
        assert_eq!(
            combine_cleanup_results(Err("browser live".to_owned()), Ok(())).unwrap_err(),
            "controlled session: browser live"
        );
        let both =
            combine_cleanup_results(Err("browser live".to_owned()), Err("daemon live".to_owned()))
                .unwrap_err();
        assert!(both.contains("browser live") && both.contains("daemon live"), "{both}");
    }

    #[test]
    fn spawned_attachment_record_must_match_pid_and_start_ticks() {
        let identity = process_identity();
        assert!(verify_spawned_attachment_record(&attachment_record(), identity).is_ok());

        let mut reused = attachment_record();
        reused.process_start_ticks = Some(8);
        assert!(verify_spawned_attachment_record(&reused, identity).is_err());

        let mut legacy = attachment_record();
        legacy.process_start_ticks = None;
        assert!(verify_spawned_attachment_record(&legacy, identity).is_err());
    }

    #[test]
    fn endpoint_identity_keeps_launch_owned_after_wrapper_leader_exits() {
        let leader =
            ProcessIdentity { pid: 41, start_ticks: 6, process_group_id: 41, session_id: 41 };
        let endpoint =
            ProcessIdentity { pid: 42, start_ticks: 7, process_group_id: 41, session_id: 41 };
        let ownership = LaunchOwnership { leader, endpoint };

        let observation = classify_owned_processes(&ownership, &[endpoint], vec![endpoint]);

        assert!(matches!(
            observation,
            ProcessObservation::Verified(VerifiedLaunchProcess { session_id: 41, .. })
        ));
    }
}
