//! The thin client behind every non-daemon `kit cdp` command. It finds the warm Attachment for an
//! Instance selector — lazily spawning the daemon if none is live (`docs/adr/0003`) — sends one
//! [`Query`] over the unix socket, and prints the rendered [`Reply`]. No CDP, no state of its own.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::sleep;

use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::cdp::{self, TrackKind};

use super::protocol::{Command, Frame, LaunchSettings, Query, Reply};
use super::registry::{self, LaunchRecord, Record};

const READY_TRIES: u32 = 60;
const READY_INTERVAL: Duration = Duration::from_millis(100);
const DEVTOOLS_TRIES: u32 = 100;
const DEVTOOLS_INTERVAL: Duration = Duration::from_millis(100);
const ELECTRON_TRIES: u32 = 300;
const ELECTRON_INTERVAL: Duration = Duration::from_millis(100);

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
pub async fn subscribe(record: &Record, since_ms: u64) -> Result<UnboundedReceiver<Frame>> {
    let stream = UnixStream::connect(registry::socket_path(&record.name))
        .await
        .with_context(|| format!("subscribe to attachment '{}'", record.name))?;
    let mut reader = BufReader::new(stream);
    let mut line =
        serde_json::to_string(&Query { command: Command::Subscribe { since_ms }, json: false })?;
    line.push('\n');
    reader.get_mut().write_all(line.as_bytes()).await?;

    let (sender, receiver) = mpsc::unbounded_channel();
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
                        if sender.send(frame).is_err() {
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
    for record in &targets {
        let _ = send(record, &Query { command: Command::Detach, json: false }).await;
        if registry::is_alive(record.pid) {
            unsafe { libc::kill(record.pid as i32, libc::SIGTERM) };
        }
        registry::remove(&record.name);
        println!("detached {}", record.name);
    }
    Ok(())
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
            "  {:<16} :{:<6} pid {:<8} {}",
            launch.name, launch.port, launch.browser_pid, launch.url
        );
    }
    Ok(())
}

pub async fn launch(options: LaunchOptions, json: bool) -> Result<()> {
    if options.reuse && options.replace {
        bail!("choose only one of --reuse or --replace");
    }

    let name = session_name(options.name.as_deref(), &options.url)?;
    let existing = match registry::read_launch(&name) {
        Some(record) if launch_is_current(&record).await => Some(record),
        Some(record) => {
            registry::remove(&record.name);
            registry::remove_launch_profile(&record);
            registry::remove_launch(&record.name);
            None
        }
        None => None,
    };
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
    let port = match wait_devtools_port(&profile_dir, started_after).await {
        Ok(port) => port,
        Err(error) => {
            let _ = child.kill();
            return Err(error);
        }
    };
    let endpoint = match cdp::browser_endpoint(port).await {
        Some(endpoint) => endpoint,
        None => {
            let _ = child.kill();
            remove_temp_profile_dir(&profile_dir, temp_profile, options.keep_profile);
            bail!("browser did not expose a valid Chrome DevTools endpoint on port {port}");
        }
    };

    spawn_daemon(&name, &name, port, browser_pid, &TrackKind::ALL)?;
    let record = wait_ready(&name).await?;

    let settings = settings_from(&options);
    let _ =
        send(&record, &Query { command: Command::Mark { name: "launch".to_owned() }, json: false })
            .await;
    let _ = send(&record, &Query { command: Command::Configure(settings), json: false }).await;
    if options.startup_capture {
        let reply = send(
            &record,
            &Query { command: Command::Navigate { target: None, url: options.url.clone() }, json },
        )
        .await?;
        if !reply.ok {
            let _ = send(&record, &Query { command: Command::CloseBrowser, json: false }).await;
            if registry::is_alive(browser_pid) {
                unsafe { libc::kill(browser_pid as i32, libc::SIGTERM) };
            }
            registry::remove(&name);
            remove_temp_profile_dir(&profile_dir, temp_profile, options.keep_profile);
            bail!("{}", reply.output);
        }
    }

    let launch = LaunchRecord {
        name: name.clone(),
        url: options.url.clone(),
        browser: browser.display().to_string(),
        browser_pid,
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
    registry::write_launch(&launch)?;
    print_launch(&launch, json)
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
    let existing = match registry::read_launch(&name) {
        Some(record) if launch_is_current(&record).await => Some(record),
        Some(record) => {
            registry::remove(&record.name);
            registry::remove_launch(&record.name);
            None
        }
        None => None,
    };
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
    let endpoint = match wait_electron_endpoint(port).await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            let _ = child.kill();
            return Err(error);
        }
    };

    spawn_daemon(&name, &name, port, app_pid, &TrackKind::ALL)?;
    let record = wait_ready(&name).await?;

    let _ =
        send(&record, &Query { command: Command::Mark { name: "launch".to_owned() }, json: false })
            .await;
    if let Some(target) = &options.renderer_target {
        let _ = send(
            &record,
            &Query {
                command: Command::Eval {
                    target: Some(target.clone()),
                    expr: "location.href".to_owned(),
                },
                json: false,
            },
        )
        .await;
    }

    let launch = LaunchRecord {
        name: name.clone(),
        url: format!("electron://{}", display_command(&options.command)),
        browser: program,
        browser_pid: app_pid,
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
    registry::write_launch(&launch)?;
    print_launch(&launch, json)
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
            "{:<16} :{:<6} pid {:<8} profile {}  {}",
            launch.name,
            launch.port,
            launch.browser_pid,
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

async fn close_records(records: impl Iterator<Item = LaunchRecord>, print: bool) -> Result<()> {
    for launch in records {
        let mut browser_closed = false;
        let launch_current = launch_is_current(&launch).await;
        if launch_current {
            if let Some(record) =
                registry::read(&launch.name).filter(|record| record.port == launch.port)
            {
                browser_closed =
                    send(&record, &Query { command: Command::CloseBrowser, json: false })
                        .await
                        .is_ok_and(|reply| reply.ok);
                if !browser_closed {
                    let _ = send(&record, &Query { command: Command::Detach, json: false }).await;
                }
                if registry::is_alive(record.pid) {
                    unsafe { libc::kill(record.pid as i32, libc::SIGTERM) };
                }
                registry::remove(&record.name);
            }
        }
        if launch_current && !browser_closed {
            browser_closed = close_browser_direct(&launch).await;
        }
        if browser_closed {
            sleep(Duration::from_millis(150)).await;
        }
        if launch_current && registry::is_alive(launch.browser_pid) {
            unsafe { libc::kill(launch.browser_pid as i32, libc::SIGTERM) };
        }
        registry::remove_launch_profile(&launch);
        registry::remove_launch(&launch.name);
        if print {
            println!("closed {}", launch.name);
        }
    }
    Ok(())
}

async fn ensure_launch_attached(launch: &LaunchRecord, tracks: &[TrackKind]) -> Result<Record> {
    if !launch_is_current(launch).await {
        registry::remove(&launch.name);
        registry::remove_launch_profile(launch);
        registry::remove_launch(&launch.name);
        bail!("launched session '{}' is not running", launch.name);
    }
    if let Some(record) = registry::read(&launch.name) {
        if send(&record, &Query { command: Command::Ping, json: false }).await.is_ok() {
            return Ok(record);
        }
        registry::remove(&record.name);
    }
    spawn_daemon(&launch.name, &launch.name, launch.port, launch.browser_pid, tracks)?;
    wait_ready(&launch.name).await
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
        if launch_is_current(&launch).await {
            live.push(launch);
        } else {
            registry::remove(&launch.name);
            registry::remove_launch_profile(&launch);
            registry::remove_launch(&launch.name);
        }
    }
    live
}

async fn launch_is_current(launch: &LaunchRecord) -> bool {
    let Some(expected_ws_url) = launch.devtools_ws_url.as_ref() else {
        return false;
    };
    let Some(endpoint) = cdp::browser_endpoint(launch.port).await else {
        return false;
    };
    expected_ws_url == &endpoint.ws_url
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
    println!(
        "profile    {}",
        launch.profile_name.as_deref().unwrap_or(if launch.temp_profile {
            "temp"
        } else {
            "custom"
        })
    );
    println!("target     page");
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

    spawn_daemon(&name, &selector, instance.endpoint.port, instance.pid, tracks)?;
    wait_ready(&name).await
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
    tracks: &[TrackKind],
) -> Result<()> {
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
    command.spawn().context("spawn cdp daemon")?;
    Ok(())
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
}
