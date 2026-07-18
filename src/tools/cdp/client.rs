//! The thin client behind every non-daemon `kit cdp` command. It finds the warm Attachment for an
//! Instance selector — lazily spawning the daemon if none is live (`docs/adr/0003`) — sends one
//! [`Query`] over the unix socket, and prints the rendered [`Reply`]. No CDP, no state of its own.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Error, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::sleep;

use tokio::sync::mpsc::{self, Receiver};

use crate::cdp::{self, TrackKind};
use crate::framework::process::{
    CommandSpec, DetachedControlError, DetachedLaunchTransaction, DetachedLifetimeRequirement,
    DetachedOutputPolicy, DetachedProcessReceipt, DetachedProcessSpec, DetachedProcessStatus,
    DetachedRecordPolicy, DetachedUnavailable, EnvironmentBase, ProcessEnvironment,
    ProcessFailureReport, ProcessLabel, ProcessSupervisor, TerminationPolicy,
};
use crate::framework::RepositoryLocator;

use super::protocol::{Command, Frame, LaunchSettings, Query, Reply};
use super::registry::{self, GpuMode, LaunchKind, LaunchPhase, LaunchRecord, Record, RenderMode};

const READY_TRIES: u32 = 60;
const READY_INTERVAL: Duration = Duration::from_millis(100);
const DEVTOOLS_TRIES: u32 = 100;
const DEVTOOLS_INTERVAL: Duration = Duration::from_millis(100);
const ELECTRON_TRIES: u32 = 300;
const ELECTRON_INTERVAL: Duration = Duration::from_millis(100);
const LIVE_FRAME_CAPACITY: usize = 1_024;
const DETACHED_TERMINATION_GRACE: Duration = Duration::from_secs(2);
const DETACHED_RECORD_POLICY: DetachedRecordPolicy = match DetachedRecordPolicy::new(
    NonZeroU64::new(64 * 1024 * 1024).expect("detached record limit is nonzero"),
) {
    Ok(policy) => policy,
    Err(_) => panic!("CDP record limit exceeds the framework maximum"),
};

#[derive(Clone, Copy)]
pub struct Runtime<'a> {
    processes: &'a ProcessSupervisor,
    repositories: &'a RepositoryLocator,
}

impl<'a> Runtime<'a> {
    pub const fn new(
        processes: &'a ProcessSupervisor,
        repositories: &'a RepositoryLocator,
    ) -> Self {
        Self { processes, repositories }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum LaunchState {
    Running,
    Stopping,
    Completed,
    InfrastructureFailure(ProcessFailureReport),
    AuthorityUnavailable(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DetachedState {
    Running,
    Stopping,
    Completed,
    InfrastructureFailure(ProcessFailureReport),
}

async fn existing_launch(runtime: Runtime<'_>, name: &str) -> Result<Option<LaunchRecord>> {
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
    match inspect_launch(runtime, &record).await {
        LaunchState::Completed => {
            close_launch_runtime(runtime, &record).await.map_err(Error::msg)?;
            registry::remove(&record.name);
            registry::remove_launch_profile(&record);
            registry::remove_launch(&record.name);
            Ok(None)
        }
        LaunchState::InfrastructureFailure(report) => {
            let failure = detached_failure_message(&report);
            close_launch_runtime(runtime, &record).await.map_err(Error::msg)?;
            registry::remove(&record.name);
            registry::remove_launch_profile(&record);
            registry::remove_launch(&record.name);
            bail!("launched session '{name}' ended with {failure}; terminal state was cleaned")
        }
        LaunchState::AuthorityUnavailable(reason) => bail!(
            "launched session '{name}' cannot prove detached authority ({reason}); recovery state retained at {}",
            registry::dir().display()
        ),
        LaunchState::Running | LaunchState::Stopping => Ok(Some(record)),
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
pub async fn query(
    runtime: Runtime<'_>,
    app: Option<&str>,
    json: bool,
    command: Command,
) -> Result<bool> {
    let record = ensure(runtime, app, &TrackKind::ALL).await?;
    let reply = send(&record, &Query { command, json }).await?;
    println!("{}", reply.output);
    Ok(reply.ok)
}

/// Resolve the warm Attachment for `app`, lazily attaching with all tracks if none is live. The
/// entry point for the interactive session, which then reuses the returned record for every command.
pub async fn ensure_attached(runtime: Runtime<'_>, app: Option<&str>) -> Result<Record> {
    ensure(runtime, app, &TrackKind::ALL).await
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
pub async fn attach(
    runtime: Runtime<'_>,
    app: Option<&str>,
    tracks: Vec<TrackKind>,
    json: bool,
) -> Result<()> {
    let record = ensure(runtime, app, &tracks).await?;
    let reply = send(&record, &Query { command: Command::Status, json }).await?;
    println!("{}", reply.output);
    Ok(())
}

/// `kit cdp detach` — dispose one or all Attachments.
pub async fn detach(runtime: Runtime<'_>, app: Option<&str>, all: bool) -> Result<()> {
    let live = reconcile_attachments(runtime).await?;
    let targets: Vec<Record> = if all {
        live
    } else if let Some(selector) = app {
        select_record(
            live.into_iter().filter(|record| matches(record, selector)).collect(),
            Some(selector),
        )?
        .into_iter()
        .collect()
    } else {
        let cwd = std::env::current_dir().context("resolve working directory")?;
        let root = runtime.repositories.nearest_worktree_root(&cwd)?;
        let candidates = live
            .into_iter()
            .filter(|record| record.worktree_root.as_deref() == Some(root.as_path()))
            .collect();
        select_record(candidates, None)?.into_iter().collect()
    };

    if targets.is_empty() {
        println!("no matching attachment");
        return Ok(());
    }
    let mut failures = Vec::new();
    for record in targets {
        match stop_attachment_record(runtime, &record).await {
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
pub async fn ls(runtime: Runtime<'_>, json: bool) -> Result<()> {
    let live = reconcile_attachments(runtime).await?;
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
            "{:<16} {:<12} :{:<6} up {:<6} tracks {}",
            record.name,
            record.app,
            record.port,
            human_ms(now_unix_ms().saturating_sub(record.started_at_ms)),
            record.tracks.join(",")
        );
    }
    Ok(())
}

/// `kit cdp gc` — sweep dead Attachments.
pub async fn gc(runtime: Runtime<'_>, json: bool) -> Result<()> {
    let before: Vec<String> = registry::all().into_iter().map(|record| record.name).collect();
    let after: Vec<String> =
        reconcile_attachments(runtime).await?.into_iter().map(|record| record.name).collect();
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
pub async fn overview(runtime: Runtime<'_>, json: bool) -> Result<()> {
    let live = reconcile_attachments(runtime).await?;
    let launches = active_launches(runtime).await?;
    let instances = cdp::discover(runtime.repositories).await;

    if json {
        let instances: Vec<_> = instances
            .iter()
            .map(|instance| {
                serde_json::json!({
                    "name": instance.display_name(),
                    "attachmentName": instance.name(),
                    "app": instance.endpoint.app,
                    "port": instance.endpoint.port,
                    "pid": instance.pid,
                    "worktreeRoot": instance.worktree_root,
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
            instance.display_name(),
            instance.endpoint.app,
            instance.endpoint.port
        );
    }
    println!("\nattachments:");
    if live.is_empty() {
        println!("  (none — a command will attach lazily)");
    }
    for record in &live {
        println!("  {:<16} {:<12} :{}", record.name, record.app, record.port);
    }
    println!("\nlaunched:");
    if launches.is_empty() {
        println!("  (none)");
    }
    for launch in &launches {
        println!("  {:<16} :{:<6} {:<8?} {}", launch.name, launch.port, launch.phase, launch.url);
    }
    Ok(())
}

pub async fn launch(runtime: Runtime<'_>, options: LaunchOptions, json: bool) -> Result<()> {
    if options.reuse && options.replace {
        bail!("choose only one of --reuse or --replace");
    }

    let name = session_name(options.name.as_deref(), &options.url)?;
    let existing = existing_launch(runtime, &name).await?;
    if let Some(existing) = existing {
        if options.replace {
            close_records(runtime, [existing].into_iter(), false).await?;
        } else if options.reuse {
            let record = ensure_launch_attached(runtime, &existing, &TrackKind::ALL).await?;
            require_reply(
                send(
                    &record,
                    &Query { command: Command::Configure(settings_from(&options)), json: false },
                )
                .await
                .context("configure reused controlled browser session")?,
                "configure reused controlled browser session",
            )?;
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
            registry::write_launch(&updated)
                .context("persist reused controlled browser session metadata")?;
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
    let mut arguments = vec![
        OsString::from("--remote-debugging-address=127.0.0.1"),
        OsString::from("--remote-debugging-port=0"),
        OsString::from(format!("--user-data-dir={}", profile_dir.display())),
        OsString::from("--no-first-run"),
        OsString::from("--no-default-browser-check"),
    ];
    if options.headless {
        arguments.push(OsString::from("--headless=new"));
    }
    if let Some(viewport) = &options.viewport {
        if let Some((width, height)) = parse_viewport(viewport) {
            arguments.push(OsString::from(format!("--window-size={width},{height}")));
        }
    }
    if options.startup_capture {
        arguments.push(OsString::from("about:blank"));
    } else {
        arguments.push(OsString::from(&options.url));
    }
    let started_after = SystemTime::now();
    let transaction = match runtime
        .processes
        .launch_detached(detached_spec(
            browser.clone(),
            arguments,
            profile_dir.clone(),
            BTreeMap::new(),
            "cdp browser",
        )?)
        .await
    {
        Ok(transaction) => transaction,
        Err(error) => {
            remove_temp_profile_dir(&profile_dir, temp_profile, options.keep_profile);
            return Err(error).with_context(|| format!("launch browser {}", browser.display()));
        }
    };
    let mut launch = LaunchRecord {
        name: name.clone(),
        phase: LaunchPhase::Starting,
        url: options.url.clone(),
        browser: browser.display().to_string(),
        process_receipt: transaction.receipt().encode(),
        root_pid: 0,
        launch_kind: Some(LaunchKind::Chrome),
        render_mode: if options.headless { RenderMode::HeadlessNew } else { RenderMode::Windowed },
        gpu_mode: GpuMode::BrowserDefault,
        port: 0,
        devtools_ws_url: None,
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
        return Err(rollback_unpublished_detached(
            transaction,
            error.context("persist controlled browser launch receipt before attachment startup"),
            || {
                registry::remove_launch(&name);
                remove_temp_profile_dir(
                    &launch.profile_dir,
                    launch.temp_profile,
                    launch.keep_profile,
                );
            },
        )
        .await);
    }
    if let Err(error) = transaction.commit() {
        let message = error.to_string();
        return Err(rollback_unpublished_detached(
            error.into_transaction(),
            Error::msg(message),
            || {
                registry::remove_launch(&name);
                remove_temp_profile_dir(
                    &launch.profile_dir,
                    launch.temp_profile,
                    launch.keep_profile,
                );
            },
        )
        .await);
    }

    let startup = async {
        let port = wait_devtools_port(&launch.profile_dir, started_after).await?;
        let endpoint = cdp::browser_endpoint(port).await.ok_or_else(|| {
            Error::msg(format!(
                "browser did not expose a valid Chrome DevTools endpoint on port {port}"
            ))
        })?;
        let root_pid = cdp::owner_pid(port)
            .with_context(|| format!("resolve browser process serving CDP port {port}"))?;
        Ok::<_, Error>((port, endpoint, root_pid))
    }
    .await;
    let (port, endpoint, root_pid) = match startup {
        Ok(startup) => startup,
        Err(error) => return fail_after_launch(runtime, &launch, error).await,
    };
    launch.port = port;
    launch.root_pid = root_pid;
    launch.devtools_ws_url = Some(endpoint.ws_url);
    if let Err(error) = registry::write_launch(&launch) {
        return fail_after_launch(
            runtime,
            &launch,
            error.context("persist controlled browser endpoint after detached launch commit"),
        )
        .await;
    }

    let completion: Result<()> = async {
        let record = spawn_daemon(
            runtime,
            SpawnDaemonOptions {
                name: &name,
                selector: &name,
                worktree_root: None,
                port,
                root_pid,
                probe_main: false,
                tracks: &TrackKind::ALL,
            },
        )
        .await?;

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
        Err(error) => fail_after_launch(runtime, &launch, error).await,
    }
}

/// `kit cdp launch-electron` — spawn an Electron app, wait for the renderer CDP endpoint it exposes,
/// and attach to it. The app owns its own page, so there is no navigation step: capture runs from
/// daemon-attach onward and the renderer target is selected by `--renderer-target`.
pub async fn launch_electron(
    runtime: Runtime<'_>,
    options: ElectronLaunchOptions,
    json: bool,
) -> Result<()> {
    if options.reuse && options.replace {
        bail!("choose only one of --reuse or --replace");
    }
    let Some(program) = options.command.first().cloned() else {
        bail!("no command to launch — pass the app command after `--`");
    };

    let name = electron_session_name(options.name.as_deref(), &program)?;
    let existing = existing_launch(runtime, &name).await?;
    if let Some(existing) = existing {
        if options.replace {
            close_records(runtime, [existing].into_iter(), false).await?;
        } else if options.reuse {
            let record = ensure_launch_attached(runtime, &existing, &TrackKind::ALL).await?;
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
    let mut arguments = options.command[1..].iter().map(OsString::from).collect::<Vec<_>>();
    arguments.extend(
        options
            .electron_args
            .iter()
            .map(|arg| OsString::from(arg.replace("{cdp_port}", &port.to_string()))),
    );
    let mut environment = BTreeMap::new();
    if let Some(var) = &options.cdp_env {
        environment.insert(OsString::from(var), OsString::from(port.to_string()));
    }
    for entry in &options.env {
        let (key, value) = entry
            .split_once('=')
            .with_context(|| format!("invalid --env '{entry}' — expected KEY=VALUE"))?;
        environment.insert(OsString::from(key), OsString::from(value));
    }
    let transaction = runtime
        .processes
        .launch_detached(detached_spec(
            PathBuf::from(&program),
            arguments,
            cwd.clone(),
            environment,
            "cdp Electron",
        )?)
        .await
        .with_context(|| format!("launch {program}"))?;
    let mut launch = LaunchRecord {
        name: name.clone(),
        phase: LaunchPhase::Starting,
        url: format!("electron://{}", display_command(&options.command)),
        browser: program,
        process_receipt: transaction.receipt().encode(),
        root_pid: 0,
        launch_kind: Some(LaunchKind::Electron),
        render_mode: RenderMode::ApplicationManaged,
        gpu_mode: GpuMode::ApplicationManaged,
        port,
        devtools_ws_url: None,
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
        return Err(rollback_unpublished_detached(
            transaction,
            error.context("persist controlled Electron launch receipt before attachment startup"),
            || registry::remove_launch(&name),
        )
        .await);
    }
    if let Err(error) = transaction.commit() {
        let message = error.to_string();
        return Err(rollback_unpublished_detached(
            error.into_transaction(),
            Error::msg(message),
            || registry::remove_launch(&name),
        )
        .await);
    }

    let startup = async {
        let endpoint = wait_electron_endpoint(port).await?;
        let root_pid = cdp::owner_pid(port)
            .with_context(|| format!("resolve Electron process serving CDP port {port}"))?;
        Ok::<_, Error>((endpoint, root_pid))
    }
    .await;
    let (endpoint, root_pid) = match startup {
        Ok(startup) => startup,
        Err(error) => return fail_after_launch(runtime, &launch, error).await,
    };
    launch.root_pid = root_pid;
    launch.devtools_ws_url = Some(endpoint.ws_url);
    if let Err(error) = registry::write_launch(&launch) {
        return fail_after_launch(
            runtime,
            &launch,
            error.context("persist controlled Electron endpoint after detached launch commit"),
        )
        .await;
    }

    let completion: Result<()> = async {
        let record = spawn_daemon(
            runtime,
            SpawnDaemonOptions {
                name: &name,
                selector: &name,
                worktree_root: None,
                port,
                root_pid,
                probe_main: true,
                tracks: &TrackKind::ALL,
            },
        )
        .await?;

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
        Err(error) => fail_after_launch(runtime, &launch, error).await,
    }
}

pub async fn launched(runtime: Runtime<'_>, json: bool) -> Result<()> {
    let launches = active_launches(runtime).await?;
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
            "{:<16} :{:<6} {:<8?} profile {}  {}",
            launch.name,
            launch.port,
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

pub async fn close_launched(runtime: Runtime<'_>, name: Option<&str>, all: bool) -> Result<()> {
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
    close_records(runtime, targets.into_iter(), true).await
}

async fn inspect_launch(runtime: Runtime<'_>, launch: &LaunchRecord) -> LaunchState {
    let receipt = match decode_receipt(&launch.process_receipt) {
        Ok(receipt) => receipt,
        Err(error) => return LaunchState::AuthorityUnavailable(error.to_string()),
    };
    match reconcile_detached(runtime, &receipt).await {
        Ok(DetachedState::Running) => LaunchState::Running,
        Ok(DetachedState::Stopping) => LaunchState::Stopping,
        Ok(DetachedState::Completed) => LaunchState::Completed,
        Ok(DetachedState::InfrastructureFailure(report)) => {
            LaunchState::InfrastructureFailure(report)
        }
        Err(reason) => LaunchState::AuthorityUnavailable(reason),
    }
}

async fn close_records(
    runtime: Runtime<'_>,
    records: impl Iterator<Item = LaunchRecord>,
    print: bool,
) -> Result<()> {
    let mut failures = Vec::new();
    for launch in records {
        let outcome = close_launch_runtime(runtime, &launch).await;
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

async fn close_launch_runtime(runtime: Runtime<'_>, launch: &LaunchRecord) -> Result<(), String> {
    match inspect_launch(runtime, launch).await {
        LaunchState::Completed | LaunchState::InfrastructureFailure(_) => {
            stop_launch_attachment(runtime, launch).await?;
            let receipt =
                decode_receipt(&launch.process_receipt).map_err(|error| error.to_string())?;
            forget_detached(runtime, &receipt).await
        }
        LaunchState::AuthorityUnavailable(reason) => Err(reason),
        LaunchState::Running | LaunchState::Stopping => {
            if let Some(record) = registry::read(&launch.name) {
                let _ = send(&record, &Query { command: Command::CloseBrowser, json: false }).await;
            } else {
                let _ = close_browser_direct(launch).await;
            }
            let receipt =
                decode_receipt(&launch.process_receipt).map_err(|error| error.to_string())?;
            stop_detached(runtime, &receipt).await?;
            stop_launch_attachment(runtime, launch).await?;
            forget_detached(runtime, &receipt).await
        }
    }
}

async fn fail_after_launch<T>(
    runtime: Runtime<'_>,
    launch: &LaunchRecord,
    failure: Error,
) -> Result<T> {
    match close_launch_runtime(runtime, launch).await {
        Ok(()) => {
            registry::remove_launch_profile(launch);
            registry::remove_launch(&launch.name);
            Err(failure)
        }
        Err(cleanup) => Err(failure.context(format!(
            "post-launch cleanup failed ({cleanup}); recovery state retained under {}",
            registry::dir().display()
        ))),
    }
}

async fn stop_launch_attachment(runtime: Runtime<'_>, launch: &LaunchRecord) -> Result<(), String> {
    let Some(record) = registry::read(&launch.name) else {
        return Ok(());
    };
    if record.port != launch.port || record.root_pid != launch.root_pid {
        return Err(format!(
            "attachment endpoint mismatch (port/root {}:{}, expected {}:{})",
            record.port, record.root_pid, launch.port, launch.root_pid
        ));
    }
    stop_attachment_record(runtime, &record).await
}

async fn stop_attachment_record(runtime: Runtime<'_>, record: &Record) -> Result<(), String> {
    let _ = send(record, &Query { command: Command::Detach, json: false }).await;
    let receipt = decode_receipt(&record.daemon_receipt).map_err(|error| error.to_string())?;
    stop_detached(runtime, &receipt).await?;
    forget_detached(runtime, &receipt).await?;
    registry::remove(&record.name);
    Ok(())
}

async fn ensure_launch_attached(
    runtime: Runtime<'_>,
    launch: &LaunchRecord,
    tracks: &[TrackKind],
) -> Result<Record> {
    if launch.phase != LaunchPhase::Ready {
        bail!(
            "launched session '{}' is {:?}, not ready; recovery state was retained",
            launch.name,
            launch.phase
        );
    }
    match inspect_launch(runtime, launch).await {
        LaunchState::Running | LaunchState::Stopping => {}
        LaunchState::Completed => {
            close_launch_runtime(runtime, launch).await.map_err(Error::msg)?;
            registry::remove(&launch.name);
            registry::remove_launch_profile(launch);
            registry::remove_launch(&launch.name);
            bail!(
                "launched session '{}' completed before it could be attached; terminal state was cleaned",
                launch.name
            );
        }
        LaunchState::InfrastructureFailure(report) => {
            let failure = detached_failure_message(&report);
            close_launch_runtime(runtime, launch).await.map_err(Error::msg)?;
            registry::remove(&launch.name);
            registry::remove_launch_profile(launch);
            registry::remove_launch(&launch.name);
            bail!(
                "launched session '{}' cannot be attached because {failure}; terminal state was cleaned",
                launch.name
            );
        }
        LaunchState::AuthorityUnavailable(reason) => bail!(
            "launched session '{}' cannot prove detached authority ({reason}); recovery state was retained at {}",
            launch.name,
            registry::dir().display()
        ),
    }
    if let Some(record) = registry::read(&launch.name) {
        if send(&record, &Query { command: Command::Ping, json: false }).await.is_ok() {
            return Ok(record);
        }
        stop_attachment_record(runtime, &record).await.map_err(Error::msg)?;
    }
    spawn_daemon(
        runtime,
        SpawnDaemonOptions {
            name: &launch.name,
            selector: &launch.name,
            worktree_root: None,
            port: launch.port,
            root_pid: launch.root_pid,
            probe_main: launch.launch_kind != Some(LaunchKind::Chrome),
            tracks,
        },
    )
    .await
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

async fn active_launches(runtime: Runtime<'_>) -> Result<Vec<LaunchRecord>> {
    let mut live = Vec::new();
    for launch in registry::all_launches() {
        match inspect_launch(runtime, &launch).await {
            LaunchState::Completed => {
                close_launch_runtime(runtime, &launch).await.map_err(Error::msg)?;
                registry::remove(&launch.name);
                registry::remove_launch_profile(&launch);
                registry::remove_launch(&launch.name);
            }
            LaunchState::InfrastructureFailure(report) => {
                let failure = detached_failure_message(&report);
                close_launch_runtime(runtime, &launch).await.map_err(Error::msg)?;
                registry::remove(&launch.name);
                registry::remove_launch_profile(&launch);
                registry::remove_launch(&launch.name);
                bail!(
                    "launched session '{}' ended with {failure}; terminal state was cleaned",
                    launch.name
                );
            }
            LaunchState::Running | LaunchState::Stopping => live.push(launch),
            LaunchState::AuthorityUnavailable(reason) => bail!(
                "launched session '{}' cannot prove detached authority ({reason}); recovery state retained at {}",
                launch.name,
                registry::dir().display()
            ),
        }
    }
    Ok(live)
}

fn detached_spec(
    program: PathBuf,
    arguments: Vec<OsString>,
    working_directory: PathBuf,
    values: BTreeMap<OsString, OsString>,
    label: &str,
) -> Result<DetachedProcessSpec> {
    let environment = ProcessEnvironment::new(EnvironmentBase::Inherit, values, BTreeSet::new())?;
    let command = CommandSpec::new(
        program.into_os_string(),
        arguments,
        working_directory,
        environment,
        ProcessLabel::new(label.to_owned())?,
    )?;
    Ok(DetachedProcessSpec::new(
        command,
        DetachedOutputPolicy::Record(DETACHED_RECORD_POLICY),
        DetachedOutputPolicy::Record(DETACHED_RECORD_POLICY),
        DetachedLifetimeRequirement::InvocationIndependent,
        TerminationPolicy::new(DETACHED_TERMINATION_GRACE),
    ))
}

fn decode_receipt(encoded: &str) -> Result<DetachedProcessReceipt> {
    DetachedProcessReceipt::decode(encoded).context("decode persisted detached-process receipt")
}

fn detached_control_message(error: DetachedControlError) -> String {
    format!("detached process control unavailable: {error}")
}

async fn stop_detached(
    runtime: Runtime<'_>,
    receipt: &DetachedProcessReceipt,
) -> Result<(), String> {
    runtime.processes.stop_detached(receipt).await.map(|_| ()).map_err(detached_control_message)
}

async fn forget_detached(
    runtime: Runtime<'_>,
    receipt: &DetachedProcessReceipt,
) -> Result<(), String> {
    runtime.processes.forget_detached(receipt).await.map_err(detached_control_message)
}

async fn reconcile_detached(
    runtime: Runtime<'_>,
    receipt: &DetachedProcessReceipt,
) -> Result<DetachedState, String> {
    match runtime.processes.inspect_detached(receipt).await {
        Ok(DetachedProcessStatus::Running) => Ok(DetachedState::Running),
        Ok(DetachedProcessStatus::Stopping) => Ok(DetachedState::Stopping),
        Ok(DetachedProcessStatus::Completed(_)) => {
            forget_detached(runtime, receipt).await?;
            Ok(DetachedState::Completed)
        }
        Ok(DetachedProcessStatus::Failed(report)) => {
            Ok(DetachedState::InfrastructureFailure(report))
        }
        Err(DetachedControlError::Unavailable(DetachedUnavailable::DurableStorageUnavailable)) => {
            // A prior reconciliation may have released framework storage before its CDP registry
            // removal became durable. Prove both the run directory and exact systemd authority are
            // gone through the idempotent release transition before calling that state completed.
            runtime
                .processes
                .forget_detached(receipt)
                .await
                .map(|()| DetachedState::Completed)
                .map_err(detached_control_message)
        }
        Err(error) => Err(detached_control_message(error)),
    }
}

fn detached_failure_message(report: &ProcessFailureReport) -> String {
    format!(
        "detached run {} had infrastructure failure {:?} (leader {:?}, termination {:?}, stdout {:?}, stderr {:?})",
        report.run_id,
        report.failure,
        report.leader_exit,
        report.termination,
        report.stdout,
        report.stderr
    )
}

async fn rollback_unpublished_detached(
    transaction: DetachedLaunchTransaction,
    cause: Error,
    on_confirmed: impl FnOnce(),
) -> Error {
    match transaction.rollback(cause).await {
        Ok(cause) => {
            on_confirmed();
            cause
        }
        Err(error) => {
            let (cause, receipt, rollback_error) = error.into_parts();
            cause.context(format!(
                "detached launch rollback could not prove termination ({rollback_error}); recovery receipt: {}",
                receipt.encode()
            ))
        }
    }
}

async fn reconcile_attachments(runtime: Runtime<'_>) -> Result<Vec<Record>> {
    let mut live = Vec::new();
    for record in registry::all() {
        let receipt = decode_receipt(&record.daemon_receipt).with_context(|| {
            format!("attachment '{}' has an invalid detached-process receipt", record.name)
        })?;
        match reconcile_detached(runtime, &receipt).await {
            Ok(DetachedState::Running | DetachedState::Stopping) => live.push(record),
            Ok(DetachedState::Completed) => {
                registry::remove(&record.name);
            }
            Ok(DetachedState::InfrastructureFailure(report)) => {
                let failure = detached_failure_message(&report);
                forget_detached(runtime, &receipt).await.map_err(Error::msg)?;
                registry::remove(&record.name);
                bail!(
                    "attachment '{}' ended with {failure}; terminal state was cleaned",
                    record.name
                );
            }
            Err(reason) => bail!(
                "attachment '{}' cannot prove detached authority ({}); recovery state retained at {}",
                record.name,
                reason,
                registry::dir().display()
            ),
        }
    }
    Ok(live)
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

async fn ensure(runtime: Runtime<'_>, app: Option<&str>, tracks: &[TrackKind]) -> Result<Record> {
    if let Some(record) = find(runtime, app).await? {
        return Ok(record);
    }
    if let Some(selector) = app.and_then(registry::read_launch) {
        return ensure_launch_attached(runtime, &selector, tracks).await;
    }
    attach_new(runtime, app, tracks).await
}

async fn find(runtime: Runtime<'_>, app: Option<&str>) -> Result<Option<Record>> {
    let live = reconcile_attachments(runtime).await?;
    let candidates = match app {
        Some(selector) => {
            live.into_iter().filter(|record| matches(record, selector)).collect::<Vec<_>>()
        }
        None => match runtime.repositories.nearest_worktree_root(&std::env::current_dir()?).ok() {
            Some(root) => live
                .into_iter()
                .filter(|record| record.worktree_root.as_deref() == Some(root.as_path()))
                .collect(),
            None => live,
        },
    };
    select_record(candidates, app)
}

async fn attach_new(
    runtime: Runtime<'_>,
    app: Option<&str>,
    tracks: &[TrackKind],
) -> Result<Record> {
    let current_worktree = if app.is_none() {
        runtime.repositories.nearest_worktree_root(&std::env::current_dir()?).ok()
    } else {
        None
    };
    let current_worktree_path = current_worktree.as_ref().map(|root| root.as_path());
    let instances = match (app, current_worktree_path) {
        (Some(selector), _) => match selector.parse::<u16>() {
            Ok(port) => match cdp::discover_port(runtime.repositories, port).await {
                Some(instance) => vec![instance],
                None => cdp::discover(runtime.repositories)
                    .await
                    .into_iter()
                    .filter(|instance| instance.matches(selector))
                    .collect(),
            },
            Err(_) => cdp::discover(runtime.repositories)
                .await
                .into_iter()
                .filter(|instance| instance.matches(selector))
                .collect(),
        },
        (None, Some(worktree_root)) => {
            cdp::discover_in_worktree(runtime.repositories, worktree_root).await
        }
        (None, None) => cdp::discover(runtime.repositories).await,
    };
    let instance = select_instance(instances, app, current_worktree_path)?;
    let name = instance.name();
    let selector = app.map(str::to_owned).unwrap_or_else(|| name.clone());

    replace_conflicting_attachment(runtime, &instance).await?;
    spawn_daemon(
        runtime,
        SpawnDaemonOptions {
            name: &name,
            selector: &selector,
            worktree_root: instance.worktree_root.as_deref(),
            port: instance.endpoint.port,
            root_pid: instance.pid,
            probe_main: true,
            tracks,
        },
    )
    .await
}

fn select_record(candidates: Vec<Record>, selector: Option<&str>) -> Result<Option<Record>> {
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(candidates.into_iter().next()),
        _ => bail!(
            "multiple live attachments match {}:\n{}",
            selector.map_or("the default instance scope".to_owned(), |value| format!("'{value}'")),
            format_records(&candidates)
        ),
    }
}

fn select_instance(
    instances: Vec<cdp::Instance>,
    selector: Option<&str>,
    worktree_root: Option<&Path>,
) -> Result<cdp::Instance> {
    match instances.len() {
        0 => match (selector, worktree_root) {
            (Some(selector), _) => bail!("no running CDP instance matches '{selector}'"),
            (None, Some(root)) => bail!(
                "no running CDP instance belongs to the current Git worktree {}",
                root.display()
            ),
            (None, None) => {
                bail!("no running CDP instance found — is the app using a remote debugging port?")
            }
        },
        1 => Ok(instances.into_iter().next().unwrap()),
        _ => bail!(
            "multiple running CDP instances match {}:\n{}",
            selector
                .map(|value| format!("'{value}'"))
                .or_else(|| worktree_root.map(|root| root.display().to_string()))
                .unwrap_or_else(|| "this command".to_owned()),
            format_instances(&instances)
        ),
    }
}

fn format_records(records: &[Record]) -> String {
    records
        .iter()
        .map(|record| format!("  {} ({}, port {})", record.name, record.app, record.port))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_instances(instances: &[cdp::Instance]) -> String {
    instances
        .iter()
        .map(|instance| {
            format!(
                "  {} ({}, port {})",
                instance.display_name(),
                instance.endpoint.app,
                instance.endpoint.port
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn replace_conflicting_attachment(
    runtime: Runtime<'_>,
    instance: &cdp::Instance,
) -> Result<()> {
    let conflicting = reconcile_attachments(runtime).await?.into_iter().find(|record| {
        record.port == instance.endpoint.port
            && record.root_pid == instance.pid
            && record.worktree_root.as_deref() != instance.worktree_root.as_deref()
    });
    if let Some(record) = conflicting {
        stop_attachment_record(runtime, &record)
            .await
            .map_err(Error::msg)
            .context("replace conflicting attachment for CDP endpoint")?;
    }
    Ok(())
}

struct SpawnDaemonOptions<'a> {
    name: &'a str,
    selector: &'a str,
    worktree_root: Option<&'a Path>,
    port: u16,
    root_pid: u32,
    probe_main: bool,
    tracks: &'a [TrackKind],
}

async fn spawn_daemon(runtime: Runtime<'_>, options: SpawnDaemonOptions<'_>) -> Result<Record> {
    let SpawnDaemonOptions { name, selector, worktree_root, port, root_pid, probe_main, tracks } =
        options;
    std::fs::create_dir_all(registry::dir()).context("create runtime dir")?;
    let exe = std::env::current_exe().context("resolve own path")?;
    let mut arguments = vec![
        OsString::from("cdp"),
        OsString::from("__serve"),
        OsString::from("--name"),
        OsString::from(name),
        OsString::from("--selector"),
        OsString::from(selector),
        OsString::from("--port"),
        OsString::from(port.to_string()),
        OsString::from("--root-pid"),
        OsString::from(root_pid.to_string()),
    ];
    if let Some(worktree_root) = worktree_root {
        arguments.extend([OsString::from("--worktree-root"), worktree_root.as_os_str().to_owned()]);
    }
    if !probe_main {
        arguments.push(OsString::from("--skip-main-probe"));
    }
    if !tracks.is_empty() {
        let csv = tracks.iter().map(|track| track.as_str()).collect::<Vec<_>>().join(",");
        arguments.extend([OsString::from("--track"), OsString::from(csv)]);
    }
    let working_directory =
        std::env::current_dir().context("resolve cdp daemon working directory")?;
    let transaction = runtime
        .processes
        .launch_detached(detached_spec(
            exe,
            arguments,
            working_directory,
            BTreeMap::new(),
            "cdp attachment daemon",
        )?)
        .await
        .context("spawn cdp attachment daemon")?;
    let endpoint = match cdp::browser_endpoint(port).await {
        Some(endpoint) => endpoint,
        None => {
            return Err(rollback_unpublished_detached(
                transaction,
                Error::msg(format!(
                    "CDP endpoint on port {port} disappeared before attachment startup"
                )),
                || {},
            )
            .await);
        }
    };
    let record = Record {
        name: name.to_owned(),
        selector: selector.to_owned(),
        app: endpoint.app,
        worktree_root: worktree_root.map(Path::to_path_buf),
        port,
        daemon_receipt: transaction.receipt().encode(),
        root_pid,
        started_at_ms: now_unix_ms(),
        tracks: tracks.iter().map(|track| track.as_str().to_owned()).collect(),
    };
    if let Err(error) = registry::write(&record) {
        return Err(rollback_unpublished_detached(
            transaction,
            error.context("persist attachment detached receipt before waiting for readiness"),
            || {},
        )
        .await);
    }
    let receipt = match transaction.commit() {
        Ok(receipt) => receipt,
        Err(error) => {
            let message = error.to_string();
            return Err(rollback_unpublished_detached(
                error.into_transaction(),
                Error::msg(message),
                || registry::remove(name),
            )
            .await);
        }
    };
    match wait_ready(&record).await {
        Ok(()) => Ok(record),
        Err(error) => match stop_detached(runtime, &receipt).await {
            Ok(()) => match forget_detached(runtime, &receipt).await {
                Ok(()) => {
                    registry::remove(name);
                    Err(error)
                }
                Err(cleanup) => Err(error.context(format!(
                    "attachment cleanup failed ({cleanup}); recovery state retained at {}",
                    registry::dir().display()
                ))),
            },
            Err(cleanup) => Err(error.context(format!(
                "attachment cleanup failed ({cleanup}); recovery state retained at {}",
                registry::dir().display()
            ))),
        },
    }
}

async fn wait_ready(record: &Record) -> Result<()> {
    for _ in 0..READY_TRIES {
        if send(record, &Query { command: Command::Ping, json: false }).await.is_ok() {
            return Ok(());
        }
        sleep(READY_INTERVAL).await;
    }
    bail!("attachment '{}' did not come up", record.name)
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
    fn persisted_receipts_reject_legacy_pid_authority() {
        assert!(decode_receipt(r#"{"pid":42,"startTicks":7}"#).is_err());
    }
}
