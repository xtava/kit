//! The Attachment daemon — the warm process behind every `kit cdp` command. It binds to the
//! Instance's browser endpoint, flatten-auto-attaches every Target, captures their events into one
//! Timeline, and answers client queries over a unix socket. It survives reloads (browser endpoint
//! is stable) and restarts (re-discovers by selector), and disposes itself cleanly (`docs/adr/0002`,
//! `docs/adr/0003`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::time::{interval, sleep};

use crate::cdp::{
    self, CdpConnection, CdpEvent, ImageFormat, LogEntry, Source, Target, Timeline, TimelineEvent,
    Track, TrackKind,
};

use super::protocol::{
    Command, Expectation, Frame, IgnoreOp, LaunchSettings, Locator, NetCommand, Query, Reply,
    ScreenshotRequest, Settle, TargetActivity, TimelineQuery, DEFAULT_CAPTURE_TIMEOUT_MS,
};
use super::readiness::{self, DocState, Readiness};
use super::registry::{self, Record};
use super::{checks, format, snapshot};

mod sourcemaps;
mod trace;

const TIMELINE_CAP: usize = 20_000;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const RECONNECT_WINDOW: Duration = Duration::from_secs(180);
const RECONNECT_MIN: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(8);
const WATCHDOG_TICK: Duration = Duration::from_secs(15);

type Shared = Arc<Mutex<State>>;

/// The Attachment's live state. The connection lives here so reconnect can swap it under the lock.
struct State {
    name: String,
    app: String,
    selector: String,
    port: u16,
    root_pid: u32,
    conn: CdpConnection,
    sessions: HashMap<String, Target>,
    /// Per-CDP-session AX refs from the last snapshot (a `snap` or a locator query). Document-
    /// scoped: cleared when the session's Target navigates.
    refs: HashMap<String, Vec<snapshot::RefEntry>>,
    /// Per-session `(at_ms, semantic lines)` of the last *explicit* snap — the `snap --diff`
    /// baseline. Survives navigation deliberately: "the page got replaced" is a real diff.
    snapshots: HashMap<String, (u64, Vec<String>)>,
    timeline: Timeline,
    tracks: Vec<TrackKind>,
    /// Error-domain CDP events (`Runtime.exceptionThrown`, error-level `Log.entryAdded`) the decoder
    /// saw but did not model — surfaced by the `errors` view so an un-decoded error type can't hide.
    undecoded_errors: usize,
    ignore: Vec<String>,
    marks: HashMap<String, u64>,
    settings: LaunchSettings,
    net_rules: Vec<NetRule>,
    /// Active value subscriptions by name. Each owns a poller task that re-evaluates its
    /// expression and emits a `watch` Timeline event on change.
    watches: HashMap<String, WatchState>,
    /// Armed instrumentation points by name, healed by the daemon-wide keeper task.
    traces: HashMap<String, trace::TraceState>,
    /// Sessions where the `__kit_trace__` binding (and Runtime) is armed — independent of the
    /// attach-time track list, because binding notifications only flow on Runtime-enabled
    /// sessions. Cleared with the sessions it describes.
    trace_transport: HashSet<String>,
    /// Sessions where the Debugger domain is enabled — lazily, by the first logpoint, never at
    /// attach. Cleared with the sessions it describes.
    debugger_enabled: HashSet<String>,
    /// Parsed scripts by scriptId, from `Debugger.scriptParsed` — the search space of
    /// `trace find` and the source-map registry's index. Pruned with its session (detach,
    /// navigation, reconnect) and capped.
    scripts: HashMap<String, sourcemaps::ScriptRecord>,
    /// Decoded source maps by scriptId. `None` caches a failed fetch/decode so it isn't retried
    /// on every resolution.
    source_maps: HashMap<String, Option<Arc<cdp::SourceMap>>>,
    /// Live `Subscribe` clients. Each gets every emitted event; senders to dropped clients are
    /// pruned on the next `emit`.
    subscribers: Vec<mpsc::UnboundedSender<TimelineEvent>>,
    start: Instant,
    last_activity: Instant,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
enum NetRule {
    Block { pattern: String },
    Mock { method: String, pattern: String, status: u16, mime: Option<String>, body: String },
}

impl NetRule {
    fn matches(&self, method: &str, url: &str) -> bool {
        match self {
            Self::Block { pattern } => contains_ci(url, pattern),
            Self::Mock { method: rule_method, pattern, .. } => {
                method.eq_ignore_ascii_case(rule_method) && contains_ci(url, pattern)
            }
        }
    }
}

impl State {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// The single path an event enters the Timeline. Every source — renderer, main process, and
    /// markers — funnels through here, so the live fan-out and `tail` can never disagree about
    /// what happened. Suppressed (ignored) events reach neither.
    fn emit(&mut self, mut event: TimelineEvent) {
        // Resolution is a cache hit when the map is already decoded — doing it here keeps live
        // subscribers and the stored ring in agreement; query-time resolution covers the rest.
        self.resolve_exception_event(&mut event);
        if !self.subscribers.is_empty() && !self.is_suppressed(&event) {
            self.subscribers.retain(|client| client.send(event.clone()).is_ok());
        }
        self.timeline.push(event);
    }

    /// Whether an event is hidden by the attachment's `ignore` list — matched on its rendered line,
    /// the same predicate `tail` uses, so suppression is identical everywhere.
    fn is_suppressed(&self, event: &TimelineEvent) -> bool {
        !self.ignore.is_empty() && {
            let line = format::event_line(event);
            self.ignore.iter().any(|pattern| line.contains(pattern.as_str()))
        }
    }

    /// Resolve a Target selector to a `(sessionId, target)` against the live target set, ranked by
    /// the engine's static score *and* live Timeline activity — so a bare selector lands on the
    /// workbench that is actually streaming, not an idle sibling, and re-resolves fresh on every
    /// command (a reload that swaps the target id is invisible to the caller).
    fn resolve(&self, selector: Option<&str>) -> Option<(String, Target)> {
        let targets: Vec<Target> = self.sessions.values().cloned().collect();
        let activity = self.timeline.counts_by_target();
        let chosen = cdp::select_active(&targets, selector, &activity)?;
        self.sessions
            .iter()
            .find(|(_, target)| target.id == chosen.id)
            .map(|(session, target)| (session.clone(), target.clone()))
    }

    fn label(&self, session: &Option<String>) -> String {
        session
            .as_ref()
            .and_then(|session| self.sessions.get(session))
            .map(Target::label)
            .unwrap_or_else(|| "browser".to_owned())
    }

    fn record(&self) -> Record {
        Record {
            name: self.name.clone(),
            app: self.app.clone(),
            selector: self.selector.clone(),
            port: self.port,
            pid: std::process::id(),
            root_pid: self.root_pid,
            started_at_ms: now_unix_ms(),
            tracks: self.tracks.iter().map(|track| track.as_str().to_owned()).collect(),
        }
    }
}

pub async fn serve(
    name: String,
    selector: String,
    port: u16,
    root_pid: u32,
    tracks: Vec<TrackKind>,
) -> Result<()> {
    let endpoint = cdp::browser_endpoint(port).await.context("instance is not a CDP endpoint")?;
    let (conn, events) =
        CdpConnection::connect(&endpoint.ws_url).await.context("connect browser endpoint")?;

    let state: Shared = Arc::new(Mutex::new(State {
        name: name.clone(),
        app: endpoint.app.clone(),
        selector,
        port,
        root_pid,
        conn: conn.clone(),
        sessions: HashMap::new(),
        refs: HashMap::new(),
        timeline: Timeline::new(TIMELINE_CAP),
        tracks: tracks.clone(),
        undecoded_errors: 0,
        ignore: Vec::new(),
        marks: HashMap::new(),
        settings: LaunchSettings::default(),
        net_rules: Vec::new(),
        watches: HashMap::new(),
        traces: HashMap::new(),
        trace_transport: HashSet::new(),
        debugger_enabled: HashSet::new(),
        scripts: HashMap::new(),
        source_maps: HashMap::new(),
        snapshots: HashMap::new(),
        subscribers: Vec::new(),
        start: Instant::now(),
        last_activity: Instant::now(),
    }));

    setup_capture(&conn).await.context("enable target discovery")?;
    registry::write(&state.lock().unwrap().record())?;

    tokio::spawn(main_process_pump(state.clone()));
    tokio::spawn(trace::keeper(state.clone()));

    let socket = registry::socket_path(&name);
    let _ = std::fs::remove_file(&socket);
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
    tokio::spawn(event_pump(state.clone(), events, tracks, shutdown_tx.clone()));

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut watchdog = interval(WATCHDOG_TICK);

    loop {
        tokio::select! {
            accept = listener.accept() => {
                if let Ok((stream, _)) = accept {
                    tokio::spawn(handle_client(stream, state.clone(), shutdown_tx.clone()));
                }
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
            _ = shutdown_rx.recv() => break,
            _ = watchdog.tick() => {
                if state.lock().unwrap().last_activity.elapsed() > IDLE_TIMEOUT {
                    break;
                }
            }
        }
    }

    registry::remove(&name);
    Ok(())
}

async fn setup_capture(conn: &CdpConnection) -> Result<()> {
    conn.call(None, "Target.setDiscoverTargets", json!({ "discover": true })).await?;
    conn.call(
        None,
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true }),
    )
    .await?;
    Ok(())
}

async fn event_pump(
    state: Shared,
    mut events: mpsc::UnboundedReceiver<CdpEvent>,
    tracks: Vec<TrackKind>,
    shutdown: mpsc::Sender<()>,
) {
    loop {
        while let Some(event) = events.recv().await {
            apply_event(&state, &event, &tracks).await;
        }
        // The browser socket closed (a full app restart). Try to re-find and re-bind the Instance.
        match reconnect(&state).await {
            Some(new_events) => {
                events = new_events;
                push_marker(&state, Source::Renderer, "reconnected to instance");
            }
            None => {
                let _ = shutdown.send(()).await;
                return;
            }
        }
    }
}

async fn apply_event(state: &Shared, event: &CdpEvent, tracks: &[TrackKind]) {
    match event.method.as_str() {
        "Target.attachedToTarget" => {
            let (Some(session), Some(info)) = (
                event.params.get("sessionId").and_then(Value::as_str),
                event.params.get("targetInfo"),
            ) else {
                return;
            };
            let target = target_from_info(info);
            // The DevTools frontend is the debugger's own reflection, not part of the app — never
            // capture its console/network into the Timeline.
            if target.url.starts_with("devtools://") {
                return;
            }
            let cascade = matches!(
                target.kind,
                cdp::TargetKind::Page | cdp::TargetKind::Webview | cdp::TargetKind::Iframe
            );
            let (conn, settings, has_rules) = {
                let mut state = state.lock().unwrap();
                state.sessions.insert(session.to_owned(), target);
                (state.conn.clone(), state.settings.clone(), !state.net_rules.is_empty())
            };
            enable_session(&conn, session, tracks, cascade).await;
            apply_session_settings(&conn, session, &settings).await;
            if has_rules {
                enable_fetch(&conn, session).await;
            }
        }
        "Target.detachedFromTarget" => {
            if let Some(session) = event.params.get("sessionId").and_then(Value::as_str) {
                let mut state = state.lock().unwrap();
                state.sessions.remove(session);
                state.refs.remove(session);
                state.snapshots.remove(session);
                state.trace_transport.remove(session);
                state.debugger_enabled.remove(session);
                state.prune_scripts(|record| record.session == session);
            }
        }
        "Runtime.bindingCalled" => trace::ingest_trace_payload(state, event),
        "Debugger.paused" => trace::handle_debugger_pause(state, event).await,
        "Debugger.scriptParsed" => sourcemaps::record_script(state, event),
        "Target.targetInfoChanged" => {
            if let Some(info) = event.params.get("targetInfo") {
                let updated = target_from_info(info);
                let mut state = state.lock().unwrap();
                // A url change is a navigation — invalidate that session's document-scoped refs.
                let navigated: Vec<String> = state
                    .sessions
                    .iter()
                    .filter(|(_, target)| target.id == updated.id && target.url != updated.url)
                    .map(|(session, _)| session.clone())
                    .collect();
                for target in state.sessions.values_mut() {
                    if target.id == updated.id {
                        *target = updated.clone();
                    }
                }
                for session in &navigated {
                    state.refs.remove(session);
                    state.prune_scripts(|record| record.session == *session);
                }
                if !navigated.is_empty() {
                    let at_ms = state.now_ms();
                    state.emit(navigation_marker(at_ms, &updated.url));
                }
            }
        }
        "Fetch.requestPaused" => handle_paused_request(state, event).await,
        _ => {
            let mut state = state.lock().unwrap();
            match Track::from_event(event) {
                Some(track) => {
                    let at_ms = state.now_ms();
                    let label = state.label(&event.session);
                    state.emit(TimelineEvent {
                        at_ms,
                        source: Source::Renderer,
                        target: label,
                        track,
                    });
                }
                // An error-domain event we couldn't decode is invisible to every view — count it so
                // `errors` can disclose the blind spot rather than imply the field is empty.
                None if is_error_domain_event(event) => state.undecoded_errors += 1,
                None => {}
            }
        }
    }
}

/// Whether a CDP event belongs to a domain that carries errors (`Runtime.exceptionThrown`, an
/// error-level `Log.entryAdded`) — the events whose silent loss the `errors` view must own up to.
fn is_error_domain_event(event: &CdpEvent) -> bool {
    match event.method.as_str() {
        "Runtime.exceptionThrown" => true,
        "Log.entryAdded" => {
            event.params.pointer("/entry/level").and_then(Value::as_str) == Some("error")
        }
        _ => false,
    }
}

async fn enable_session(conn: &CdpConnection, session: &str, tracks: &[TrackKind], cascade: bool) {
    let mut domains: Vec<&str> = tracks.iter().filter_map(|track| track.domain()).collect();
    domains.sort_unstable();
    domains.dedup();
    for domain in domains {
        let _ = conn.call(Some(session), &format!("{domain}.enable"), json!({})).await;
    }
    if tracks.contains(&TrackKind::Lifecycle) {
        let _ = conn
            .call(Some(session), "Page.setLifecycleEventsEnabled", json!({ "enabled": true }))
            .await;
    }
    if cascade {
        let _ = conn
            .call(
                Some(session),
                "Target.setAutoAttach",
                json!({ "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true }),
            )
            .await;
    }
}

async fn apply_session_settings(conn: &CdpConnection, session: &str, settings: &LaunchSettings) {
    if let Some(viewport) = settings.viewport.as_deref().and_then(parse_viewport) {
        let (width, height) = viewport;
        let _ = conn
            .call(
                Some(session),
                "Emulation.setDeviceMetricsOverride",
                json!({ "width": width, "height": height, "deviceScaleFactor": 1, "mobile": false }),
            )
            .await;
    } else {
        let _ = conn.call(Some(session), "Emulation.clearDeviceMetricsOverride", json!({})).await;
    }
    let _ = conn
        .call(
            Some(session),
            "Emulation.setTimezoneOverride",
            json!({ "timezoneId": settings.timezone.as_deref().unwrap_or("") }),
        )
        .await;
    let _ = conn
        .call(
            Some(session),
            "Emulation.setLocaleOverride",
            json!({ "locale": settings.locale.as_deref().unwrap_or("") }),
        )
        .await;
    let features = if settings.dark {
        json!([{ "name": "prefers-color-scheme", "value": "dark" }])
    } else {
        json!([])
    };
    let _ = conn
        .call(Some(session), "Emulation.setEmulatedMedia", json!({ "features": features }))
        .await;
    let (latency, download, upload) = match settings.throttle.as_deref() {
        Some("slow-3g") => (400, 50_000, 50_000),
        Some("fast-3g") => (150, 180_000, 84_375),
        _ => (0, -1, -1),
    };
    let _ = conn
        .call(
            Some(session),
            "Network.emulateNetworkConditions",
            json!({
                "offline": settings.offline,
                "latency": latency,
                "downloadThroughput": download,
                "uploadThroughput": upload,
            }),
        )
        .await;
}

async fn enable_fetch(conn: &CdpConnection, session: &str) {
    let _ = conn
        .call(
            Some(session),
            "Fetch.enable",
            json!({ "patterns": [{ "urlPattern": "*", "requestStage": "Request" }] }),
        )
        .await;
}

async fn handle_paused_request(state: &Shared, event: &CdpEvent) {
    let Some(request_id) = event.params.get("requestId").and_then(Value::as_str) else {
        return;
    };
    let url = event.params.pointer("/request/url").and_then(Value::as_str).unwrap_or("");
    let method = event.params.pointer("/request/method").and_then(Value::as_str).unwrap_or("GET");
    let (conn, rule) = {
        let state = state.lock().unwrap();
        (state.conn.clone(), state.net_rules.iter().find(|rule| rule.matches(method, url)).cloned())
    };
    match rule {
        Some(NetRule::Block { .. }) => {
            let _ = conn
                .call(
                    event.session.as_deref(),
                    "Fetch.failRequest",
                    json!({ "requestId": request_id, "errorReason": "BlockedByClient" }),
                )
                .await;
        }
        Some(NetRule::Mock { status, mime, body, .. }) => {
            let _ = conn
                .call(
                    event.session.as_deref(),
                    "Fetch.fulfillRequest",
                    json!({
                        "requestId": request_id,
                        "responseCode": status,
                        "responseHeaders": [{ "name": "content-type", "value": mime.unwrap_or_else(|| "application/json".to_owned()) }],
                        "body": cdp::base64::encode(body.as_bytes()),
                    }),
                )
                .await;
        }
        None => {
            let _ = conn
                .call(
                    event.session.as_deref(),
                    "Fetch.continueRequest",
                    json!({ "requestId": request_id }),
                )
                .await;
        }
    }
}

const MAIN_LABEL: &str = "main";

/// Fold the Electron main process's V8 inspector (`--inspect`) into the Timeline, labeled `main`.
/// Silent and idempotent when the main isn't inspectable — it just keeps trying within the window.
async fn main_process_pump(state: Shared) {
    let mut delay = RECONNECT_MIN;
    let mut deadline = Instant::now() + RECONNECT_WINDOW;
    loop {
        let root_pid = state.lock().unwrap().root_pid;
        match connect_main(root_pid).await {
            Some(mut events) => {
                push_marker(&state, Source::Main, "main process console attached");
                while let Some(event) = events.recv().await {
                    if let Some(track) = Track::from_event(&event) {
                        let mut state = state.lock().unwrap();
                        let at_ms = state.now_ms();
                        state.emit(TimelineEvent {
                            at_ms,
                            source: Source::Main,
                            target: MAIN_LABEL.to_owned(),
                            track,
                        });
                    }
                }
                delay = RECONNECT_MIN;
                deadline = Instant::now() + RECONNECT_WINDOW;
            }
            None => {
                if Instant::now() >= deadline {
                    return;
                }
                sleep(delay).await;
                delay = (delay * 2).min(RECONNECT_MAX);
            }
        }
    }
}

async fn connect_main(root_pid: u32) -> Option<mpsc::UnboundedReceiver<CdpEvent>> {
    let endpoint = cdp::node_endpoint(root_pid).await?;
    let (conn, events) = CdpConnection::connect(&endpoint.ws_url).await.ok()?;
    // Node's V8 inspector implements Runtime (console + exceptions) but not the browser-only Log domain.
    conn.call(None, "Runtime.enable", json!({})).await.ok()?;
    Some(events)
}

async fn reconnect(state: &Shared) -> Option<mpsc::UnboundedReceiver<CdpEvent>> {
    let selector = state.lock().unwrap().selector.clone();
    let deadline = Instant::now() + RECONNECT_WINDOW;
    let mut delay = RECONNECT_MIN;

    while Instant::now() < deadline {
        sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_MAX);

        let Some(instance) = discover_matching(&selector).await else {
            continue;
        };
        let Ok((conn, events)) = CdpConnection::connect(&instance.endpoint.ws_url).await else {
            continue;
        };
        if setup_capture(&conn).await.is_err() {
            continue;
        }

        let record = {
            let mut state = state.lock().unwrap();
            state.conn = conn;
            state.port = instance.endpoint.port;
            state.root_pid = instance.pid;
            state.app = instance.endpoint.app.clone();
            state.sessions.clear();
            state.refs.clear();
            state.snapshots.clear();
            state.trace_transport.clear();
            state.debugger_enabled.clear();
            state.scripts.clear();
            state.source_maps.clear();
            state.record()
        };
        let _ = registry::write(&record);
        return Some(events);
    }
    None
}

async fn discover_matching(selector: &str) -> Option<cdp::Instance> {
    cdp::discover()
        .await
        .into_iter()
        .filter(|instance| instance.matches(selector))
        .min_by_key(|instance| instance.endpoint.port)
}

async fn handle_client(stream: UnixStream, state: Shared, shutdown: mpsc::Sender<()>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_err() || line.trim().is_empty() {
        return;
    }

    let query = match serde_json::from_str::<Query>(line.trim()) {
        Ok(query) => query,
        Err(error) => {
            write_reply(&mut reader, &Reply::fail(format!("bad query: {error}"))).await;
            return;
        }
    };
    state.lock().unwrap().last_activity = Instant::now();

    // A subscription is not request/reply — it streams frames and holds the socket open.
    if let Command::Subscribe { since_ms } = query.command {
        stream_subscription(reader, state, since_ms).await;
        return;
    }

    let reply = dispatch(&state, query.command, query.json, &shutdown).await;
    write_reply(&mut reader, &reply).await;
}

async fn write_reply(reader: &mut BufReader<UnixStream>, reply: &Reply) {
    let mut payload = serde_json::to_string(reply)
        .unwrap_or_else(|_| String::from("{\"ok\":false,\"output\":\"encode error\"}"));
    payload.push('\n');
    let _ = reader.get_mut().write_all(payload.as_bytes()).await;
}

/// Serve a live subscription: register a sender, send the backfill window, then forward every
/// emitted event until the client disconnects (a failed write). The sender is pruned from `State`
/// by the next `emit` once this returns and the receiver drops.
async fn stream_subscription(mut reader: BufReader<UnixStream>, state: Shared, since_ms: u64) {
    let (sender, mut receiver) = mpsc::unbounded_channel::<TimelineEvent>();
    let backfill = {
        let mut state = state.lock().unwrap();
        let now = state.now_ms();
        let events: Vec<TimelineEvent> = state
            .timeline
            .since(now, since_ms, None, None)
            .into_iter()
            .filter(|event| !state.is_suppressed(event))
            .collect();
        state.subscribers.push(sender);
        events
    };

    if write_frame(&mut reader, &Frame::Backfill(backfill)).await.is_err() {
        return;
    }
    while let Some(event) = receiver.recv().await {
        if write_frame(&mut reader, &Frame::Event(event)).await.is_err() {
            break;
        }
    }
}

async fn write_frame(reader: &mut BufReader<UnixStream>, frame: &Frame) -> std::io::Result<()> {
    let Ok(mut payload) = serde_json::to_string(frame) else {
        return Ok(());
    };
    payload.push('\n');
    reader.get_mut().write_all(payload.as_bytes()).await
}

async fn dispatch(
    state: &Shared,
    command: Command,
    json: bool,
    shutdown: &mpsc::Sender<()>,
) -> Reply {
    match command {
        Command::Ping => Reply::ok("pong"),
        Command::Status => status_reply(state, json),
        Command::Targets => targets_reply(state, json),
        Command::Tail(query) => tail_reply(state, query, json),
        Command::Brief { query, tail, groups } => brief_reply(state, query, tail, groups, json),
        Command::Errors { query, explain, resolve } => {
            errors_reply(state, query, explain, resolve, json).await
        }
        Command::Configure(settings) => configure_reply(state, settings).await,
        Command::Navigate { target, url } => navigate_reply(state, target, url).await,
        Command::LaunchLog => launch_log_reply(state, json),
        Command::State { visual } => state_reply(state, visual, json).await,
        Command::Mark { name } => mark_reply(state, name, json),
        Command::After { mark, idle_ms, timeout_ms } => {
            after_reply(state, mark, idle_ms, timeout_ms, json).await
        }
        Command::Bundle { since, include, include_secrets } => {
            bundle_reply(state, since, include, include_secrets, json)
        }
        Command::Net(command) => net_reply(state, command, json).await,
        Command::Eval { target, expr } => {
            run_in_target(state, target, json, |conn, session| evaluate(conn, session, expr)).await
        }
        Command::Ready { target } => ready_reply(state, target, json).await,
        Command::WaitFor { target, expr, timeout_ms } => {
            wait_reply(state, target, expr, timeout_ms, json).await
        }
        Command::Expect { expectation, within_ms } => {
            expect_reply(state, expectation, within_ms, json).await
        }
        Command::Verify { target, window } => verify_reply(state, target, window, json).await,
        Command::Watch(op) => watch_reply(state, op, json),
        Command::Trace(op) => trace::trace_reply(state, op, json).await,
        Command::Press { target, chord, settle } => {
            press_reply(state, target, chord, settle, json).await
        }
        Command::Select { target, locator, option, settle } => {
            select_reply(state, target, locator, option, settle, json).await
        }
        Command::Do { steps } => do_reply(state, steps, json, shutdown).await,
        Command::Lens { target, source, args } => {
            let expr = wrap_lens(&source, &args, &lens_context(state));
            run_in_target(state, target, json, |conn, session| evaluate(conn, session, expr)).await
        }
        Command::ExtensionBundle { target, source, extension_id, query } => {
            extension_bundle_reply(state, target, source, extension_id, query, json).await
        }
        Command::Ignore(op) => ignore_reply(state, op, json),
        Command::TargetList => target_list_reply(state),
        Command::Refs { target } => refs_reply(state, target).await,
        Command::Heap { target } => heap_reply(state, target, json).await,
        Command::Snap { target, interactive, diff } => {
            snap_reply(state, target, interactive, diff, json).await
        }
        Command::Screenshot { target, request } => {
            screenshot_reply(state, target, request, json).await
        }
        Command::Click { target, locator, settle } => {
            click_reply(state, target, locator, settle, json).await
        }
        Command::Fill { target, locator, text, settle } => {
            fill_reply(state, target, locator, text, settle, json).await
        }
        Command::CloseBrowser => {
            let conn = state.lock().unwrap().conn.clone();
            let reply = match conn.call(None, "Browser.close", json!({})).await {
                Ok(_) => Reply::ok("browser closed"),
                Err(error) => Reply::fail(error.to_string()),
            };
            let _ = shutdown.send(()).await;
            reply
        }
        Command::Detach => {
            let _ = shutdown.send(()).await;
            Reply::ok("detached")
        }
        // Intercepted in `handle_client` before dispatch; never reaches here.
        Command::Subscribe { .. } => Reply::fail("subscribe is a streaming command"),
    }
}

fn status_reply(state: &Shared, json: bool) -> Reply {
    let state = state.lock().unwrap();
    Reply::ok(format::status(
        &state.name,
        &state.app,
        state.port,
        state.now_ms(),
        state.sessions.len(),
        state.timeline.len(),
        state.tracks.iter().map(|track| track.as_str()).collect(),
        json,
    ))
}

fn targets_reply(state: &Shared, json: bool) -> Reply {
    let state = state.lock().unwrap();
    let targets: Vec<Target> = state.sessions.values().cloned().collect();
    Reply::ok(format::targets(&targets, json))
}

/// The picker's data source: every Target joined with its Timeline event volume, sorted active-first
/// (then by how main-window-like it is). Always JSON — the client renders it.
fn target_list_reply(state: &Shared) -> Reply {
    let state = state.lock().unwrap();
    let counts = state.timeline.counts_by_target();
    let mut rows: Vec<(TargetActivity, i32)> = state
        .sessions
        .values()
        .map(|target| {
            let label = target.label();
            let events = counts.get(&label).copied().unwrap_or(0);
            let activity = TargetActivity {
                label,
                kind: target.kind,
                title: target.title.clone(),
                url: target.url.clone(),
                events,
                extension_id: query_param(&target.url, "extensionId"),
                purpose: query_param(&target.url, "purpose"),
            };
            (activity, target.main_rank())
        })
        .collect();
    rows.sort_by(|a, b| b.0.events.cmp(&a.0.events).then(b.1.cmp(&a.1)));

    let list: Vec<TargetActivity> = rows.into_iter().map(|(activity, _)| activity).collect();
    match serde_json::to_string(&list) {
        Ok(json) => Reply::ok(json),
        Err(error) => Reply::fail(format!("encode targets: {error}")),
    }
}

/// The stored refs for a target's session, as JSON — completion data, not for humans.
/// Elements for completion: a *fresh* accessibility snapshot, not the stored table — suggestions
/// must reflect the screen as it is now, and refreshing the table means the `@eN` numbers offered
/// are exactly the ones a subsequent click resolves.
async fn refs_reply(state: &Shared, target: Option<String>) -> Reply {
    let Some((conn, session)) = session_for(state, target.as_deref()) else {
        return Reply::fail(no_target());
    };
    match fresh_refs(state, &conn, &session).await {
        Ok(refs) => match serde_json::to_string(&refs) {
            Ok(json) => Reply::ok(json),
            Err(error) => Reply::fail(format!("encode refs: {error}")),
        },
        Err(error) => Reply::fail(error),
    }
}

fn tail_reply(state: &Shared, query: TimelineQuery, json: bool) -> Reply {
    match collect_timeline(state, &query) {
        Ok(events) => Reply::ok(format::events(&events, json)),
        Err(error) => Reply::fail(error),
    }
}

fn brief_reply(
    state: &Shared,
    query: TimelineQuery,
    tail: usize,
    groups: usize,
    json: bool,
) -> Reply {
    match collect_brief_timeline(state, &query) {
        Ok((events, meta)) => Reply::ok(format::brief(&events, meta, tail, groups, json)),
        Err(error) => Reply::fail(error),
    }
}

async fn errors_reply(
    state: &Shared,
    query: TimelineQuery,
    explain: bool,
    resolve: bool,
    json: bool,
) -> Reply {
    if resolve {
        sourcemaps::prime_error_maps(state, &query).await;
    }
    let guard = state.lock().unwrap();
    let now = guard.now_ms();
    let since_ms = match query_window_ms(&guard, now, &query) {
        Ok(window) => window,
        Err(error) => return Reply::fail(error),
    };
    let saturated = guard.timeline.is_saturated_for(now, since_ms);
    let evicted = guard.timeline.evicted();
    let undecoded = guard.undecoded_errors;
    drop(guard);

    let events = match collect_timeline(state, &query) {
        Ok(events) => events,
        Err(error) => return Reply::fail(error),
    };
    let report = cdp::ErrorReport {
        groups: cdp::group_errors(&events),
        evicted: Some(evicted),
        undecoded,
        saturated,
    };
    Reply::ok(format::errors(&report, explain, json))
}

fn collect_timeline(state: &Shared, query: &TimelineQuery) -> Result<Vec<TimelineEvent>, String> {
    let state = state.lock().unwrap();
    let now = state.now_ms();
    let since_ms = query_window_ms(&state, now, query)?;
    let mut events: Vec<TimelineEvent> = state
        .timeline
        .since(now, since_ms, query.tracks.as_deref(), query.source)
        .into_iter()
        .filter(|event| !state.is_suppressed(event))
        .filter(|event| event_matches(event, query))
        .collect();

    if let Some(limit) = query.limit {
        if events.len() > limit {
            events = events.split_off(events.len() - limit);
        }
    }

    state.resolve_exception_sites(&mut events);
    Ok(events)
}

fn collect_brief_timeline(
    state: &Shared,
    query: &TimelineQuery,
) -> Result<(Vec<TimelineEvent>, format::BriefMeta), String> {
    let state = state.lock().unwrap();
    let now = state.now_ms();
    let since_ms = query_window_ms(&state, now, query)?;
    let saturated = state.timeline.is_saturated_for(now, since_ms);
    let evicted = state.timeline.evicted();
    let undecoded = state.undecoded_errors;
    let mut matching_events = 0;
    let mut suppressed_by_ignore = 0;
    let mut events = Vec::new();

    for event in state.timeline.since(now, since_ms, query.tracks.as_deref(), query.source) {
        if !event_matches(&event, query) {
            continue;
        }
        matching_events += 1;
        if state.is_suppressed(&event) {
            suppressed_by_ignore += 1;
        } else {
            events.push(event);
        }
    }

    let mut clipped_by_limit = 0;
    if let Some(limit) = query.limit {
        if events.len() > limit {
            clipped_by_limit = events.len() - limit;
            events = events.split_off(clipped_by_limit);
        }
    }

    state.resolve_exception_sites(&mut events);
    Ok((
        events,
        format::BriefMeta {
            window_ms: since_ms,
            matching_events,
            suppressed_by_ignore,
            clipped_by_limit,
            evicted: Some(evicted),
            undecoded,
            saturated,
        },
    ))
}

fn query_window_ms(state: &State, now: u64, query: &TimelineQuery) -> Result<u64, String> {
    let Some(mark) = query.since_mark.as_ref() else {
        return Ok(query.since_ms);
    };
    let Some(at_ms) = state.marks.get(mark) else {
        return Err(format!("unknown mark '{mark}'"));
    };
    Ok(now.saturating_sub(*at_ms))
}

async fn configure_reply(state: &Shared, settings: LaunchSettings) -> Reply {
    let (conn, sessions) = {
        let mut state = state.lock().unwrap();
        state.settings = settings.clone();
        (state.conn.clone(), state.sessions.keys().cloned().collect::<Vec<_>>())
    };
    for session in sessions {
        apply_session_settings(&conn, &session, &settings).await;
    }
    Reply::ok("configured")
}

async fn navigate_reply(state: &Shared, target: Option<String>, url: String) -> Reply {
    let resolved = {
        let state = state.lock().unwrap();
        state.resolve(target.as_deref()).map(|(session, _)| (state.conn.clone(), session))
    };
    let Some((conn, session)) = resolved else {
        return Reply::fail(no_target());
    };
    push_marker(state, Source::Renderer, &format!("navigate {url}"));
    match conn.call(Some(&session), "Page.navigate", json!({ "url": url })).await {
        Ok(_) => Reply::ok("navigated"),
        Err(error) => Reply::fail(error.to_string()),
    }
}

fn launch_log_reply(state: &Shared, json: bool) -> Reply {
    let query = TimelineQuery {
        since_ms: 10_000,
        since_mark: Some("launch".to_owned()),
        tracks: None,
        source: None,
        target: None,
        grep: None,
        extension: None,
        limit: None,
    };
    let events = match collect_timeline(state, &query) {
        Ok(events) => events,
        Err(error) => return Reply::fail(error),
    };
    if json {
        return Reply::ok(serde_json::to_string_pretty(&events).unwrap_or_default());
    }
    let mut lines: Vec<String> = events.iter().map(format::event_line).collect();
    if lines.is_empty() {
        lines.push("(no launch events captured)".to_owned());
    }
    lines.push("raw    kit cdp tail --since-mark launch".to_owned());
    Reply::ok(lines.join("\n"))
}

async fn state_reply(state: &Shared, visual: bool, json: bool) -> Reply {
    let (name, resolved, recent_errors, failed_network, settings, rules, launch, instrumented) = {
        let state = state.lock().unwrap();
        let now = state.now_ms();
        let trace_hits: u64 = state.traces.values().map(|entry| entry.hits).sum();
        let instrumented = (state.watches.len(), state.traces.len(), trace_hits);
        let resolved =
            state.resolve(None).map(|(session, target)| (state.conn.clone(), session, target));
        let recent_errors = state
            .timeline
            .since(now, 60_000, None, None)
            .into_iter()
            .filter(|event| event.track.is_error())
            .rev()
            .take(5)
            .collect::<Vec<_>>();
        let failed_network = state
            .timeline
            .since(now, 60_000, Some(&[TrackKind::Network]), None)
            .into_iter()
            .filter(|event| event.track.is_error())
            .rev()
            .take(5)
            .collect::<Vec<_>>();
        (
            state.name.clone(),
            resolved,
            recent_errors,
            failed_network,
            state.settings.clone(),
            state.net_rules.clone(),
            registry::read_launch(&state.name),
            instrumented,
        )
    };

    let mut document = None;
    let mut focus = None;
    let mut screenshot = None;
    let mut target_label = "none".to_owned();
    if let Some((conn, session, target)) = resolved {
        target_label = target.label();
        document = probe_document(&conn, &session).await;
        focus = evaluate(conn.clone(), session.clone(), FOCUS_PROBE.to_owned()).await.ok();
        if visual {
            let path = registry::artifact_dir(&name).join("latest.png");
            let budget = Duration::from_millis(DEFAULT_CAPTURE_TIMEOUT_MS);
            if write_screenshot(&conn, &session, &path, ImageFormat::Png, None, false, budget)
                .await
                .is_ok()
            {
                screenshot = Some(path.display().to_string());
            }
        }
    }

    let value = json!({
        "name": name,
        "target": target_label,
        "document": document,
        "focus": focus,
        "screenshot": screenshot,
        "recentErrors": recent_errors,
        "failedNetwork": failed_network,
        "settings": settings,
        "netRules": rules,
        "launch": launch,
        "watches": instrumented.0,
        "traces": { "armed": instrumented.1, "hits": instrumented.2 },
        "rawCommands": {
            "tail": format!("kit cdp tail --app {name}"),
            "errors": format!("kit cdp errors --explain --app {name}")
        }
    });
    if json {
        return Reply::ok(serde_json::to_string_pretty(&value).unwrap_or_default());
    }

    let mut out = Vec::new();
    out.push(format!("target    {target_label}"));
    if let Some(doc) = value.get("document") {
        let ready = doc.get("readyState").and_then(Value::as_str).unwrap_or("?");
        let visible = doc.get("visibility").and_then(Value::as_str).unwrap_or("?");
        out.push(format!("ready     {ready}, {visible}"));
    }
    if recent_errors.is_empty() {
        out.push("errors    none".to_owned());
    } else {
        out.push(format!(
            "errors    {} recent  raw: kit cdp errors --explain --app {name}",
            recent_errors.len()
        ));
    }
    if let Some(event) = failed_network.first() {
        out.push(format!(
            "network   {}  raw: kit cdp net show {} --app {name}",
            format::event_line(event),
            network_request_id(event).unwrap_or("?")
        ));
    }
    if let Some(focus) = value.get("focus") {
        out.push(format!("focus     {}", compact_value(focus)));
    }
    if let Some(path) = value.get("screenshot").and_then(Value::as_str) {
        out.push(format!("screen    {path}"));
    }
    out.push(format!("rules     {}", rules.len()));
    if instrumented.0 > 0 || instrumented.1 > 0 {
        out.push(format!(
            "instr     {} watch(es) · {} trace(s), {} hit(s)",
            instrumented.0, instrumented.1, instrumented.2
        ));
    }
    out.push(format!("raw       kit cdp tail --app {name}"));
    Reply::ok(out.join("\n"))
}

fn mark_reply(state: &Shared, name: String, json: bool) -> Reply {
    let mut state = state.lock().unwrap();
    let at_ms = state.now_ms();
    state.marks.insert(name.clone(), at_ms);
    state.emit(TimelineEvent {
        at_ms,
        source: Source::Renderer,
        target: "kit".to_owned(),
        track: Track::Log(LogEntry {
            level: "info".to_owned(),
            source: "kit".to_owned(),
            text: format!("mark {name}"),
            url: None,
            line: None,
        }),
    });
    if json {
        Reply::ok(json!({ "mark": name, "atMs": at_ms }).to_string())
    } else {
        Reply::ok(format!("marked {name} at +{at_ms}ms"))
    }
}

/// Poll until the Timeline has been quiet for `idle_ms`, or `timeout_ms` has elapsed.
/// Returns true when the timeout ended the wait — the Timeline was still moving.
async fn wait_for_quiet(state: &Shared, idle_ms: u64, timeout_ms: u64) -> bool {
    let started = Instant::now();
    let mut last_len = state.lock().unwrap().timeline.len();
    let mut idle_started = Instant::now();
    loop {
        sleep(Duration::from_millis(50)).await;
        let len = state.lock().unwrap().timeline.len();
        if len != last_len {
            last_len = len;
            idle_started = Instant::now();
        }
        if idle_started.elapsed() >= Duration::from_millis(idle_ms) {
            return false;
        }
        if started.elapsed() >= Duration::from_millis(timeout_ms) {
            return true;
        }
    }
}

async fn after_reply(
    state: &Shared,
    mark: String,
    idle_ms: u64,
    timeout_ms: u64,
    json: bool,
) -> Reply {
    let timed_out = wait_for_quiet(state, idle_ms, timeout_ms).await;

    let (events, ended) = {
        let state = state.lock().unwrap();
        let Some(&at_ms) = state.marks.get(&mark) else {
            return Reply::fail(format!("unknown mark '{mark}'"));
        };
        let now = state.now_ms();
        let events = state
            .timeline
            .since(now, now.saturating_sub(at_ms), None, None)
            .into_iter()
            .filter(|event| !state.is_suppressed(event))
            .collect::<Vec<_>>();
        let ended = if timed_out {
            format!("timeout after {timeout_ms}ms")
        } else {
            format!("idle after {idle_ms}ms")
        };
        (events, ended)
    };
    let errors = events.iter().filter(|event| event.track.is_error()).count();
    let network = events.iter().filter(|event| matches!(event.track, Track::Network(_))).count();
    let recent: Vec<String> = events.iter().rev().take(8).rev().map(format::event_line).collect();
    let value = json!({
        "mark": mark,
        "ended": ended,
        "events": events.len(),
        "errors": errors,
        "network": network,
        "recent": recent,
        "rawCommand": format!("kit cdp tail --since-mark {mark}")
    });
    if json {
        return Reply::ok(serde_json::to_string_pretty(&value).unwrap_or_default());
    }
    let mut out = vec![
        format!("after {mark}"),
        format!("ended     {ended}"),
        format!("events    {} total, {errors} errors, {network} network", events.len()),
    ];
    for line in recent {
        out.push(format!("event     {line}"));
    }
    out.push(format!("raw       kit cdp tail --since-mark {mark}"));
    Reply::ok(out.join("\n"))
}

/// How often `wait`/`expect` re-check their condition.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// One active value subscription: what it evaluates, what it last saw, and the poller that owns
/// it. `task` is `None` only during the insert→spawn handoff.
struct WatchState {
    target: Option<String>,
    expr: String,
    interval_ms: u64,
    last: Option<Value>,
    changes: u64,
    task: Option<tokio::task::JoinHandle<()>>,
}

fn watch_reply(state: &Shared, op: super::protocol::WatchOp, json: bool) -> Reply {
    use super::protocol::WatchOp;
    match op {
        WatchOp::Add { name, target, expr, interval_ms } => {
            let interval_ms = interval_ms.max(50);
            {
                let mut guard = state.lock().unwrap();
                if guard.watches.contains_key(&name) {
                    return Reply::fail(format!(
                        "watch '{name}' already exists — `watch rm {name}` first"
                    ));
                }
                // Insert before spawning so the poller's first tick always finds its entry.
                guard.watches.insert(
                    name.clone(),
                    WatchState {
                        target: target.clone(),
                        expr: expr.clone(),
                        interval_ms,
                        last: None,
                        changes: 0,
                        task: None,
                    },
                );
            }
            let task =
                tokio::spawn(watch_loop(state.clone(), name.clone(), target, expr, interval_ms));
            match state.lock().unwrap().watches.get_mut(&name) {
                Some(watch) => watch.task = Some(task),
                // A racing rm/clear removed it between insert and spawn — honor the removal.
                None => task.abort(),
            }
            Reply::ok(format!("watching '{name}' every {interval_ms}ms"))
        }
        WatchOp::Ls => {
            let guard = state.lock().unwrap();
            if json {
                let rows: Vec<Value> = guard
                    .watches
                    .iter()
                    .map(|(name, watch)| {
                        json!({
                            "name": name,
                            "expr": watch.expr,
                            "target": watch.target,
                            "intervalMs": watch.interval_ms,
                            "changes": watch.changes,
                            "last": watch.last,
                        })
                    })
                    .collect();
                return Reply::ok(serde_json::to_string_pretty(&rows).unwrap_or_default());
            }
            if guard.watches.is_empty() {
                return Reply::ok(
                    "no watches — add one with `watch add <name> '<expr>'`".to_owned(),
                );
            }
            let mut rows: Vec<String> = guard
                .watches
                .iter()
                .map(|(name, watch)| {
                    let last = watch
                        .last
                        .as_ref()
                        .map(value_preview)
                        .unwrap_or_else(|| "(pending)".to_owned());
                    format!(
                        "{name:<16} every {:>5}ms · {} change(s) · {} = {last}",
                        watch.interval_ms, watch.changes, watch.expr
                    )
                })
                .collect();
            rows.sort();
            Reply::ok(rows.join("\n"))
        }
        WatchOp::Rm { name } => match state.lock().unwrap().watches.remove(&name) {
            Some(watch) => {
                if let Some(task) = watch.task {
                    task.abort();
                }
                Reply::ok(format!("watch '{name}' removed"))
            }
            None => Reply::fail(format!("no watch '{name}'")),
        },
        WatchOp::Clear => {
            let removed: Vec<(String, WatchState)> =
                state.lock().unwrap().watches.drain().collect();
            for (_, watch) in &removed {
                if let Some(task) = &watch.task {
                    task.abort();
                }
            }
            Reply::ok(format!("{} watch(es) removed", removed.len()))
        }
    }
}

/// The poller behind one watch: evaluate, compare, emit on change. It re-resolves its target
/// every tick (so it survives reloads), skips ticks whose eval fails (a target mid-reload is not
/// a change), and exits when its entry disappears.
async fn watch_loop(
    state: Shared,
    name: String,
    target: Option<String>,
    expr: String,
    interval_ms: u64,
) {
    loop {
        if let Some((conn, session)) = session_for(&state, target.as_deref()) {
            let label = state.lock().unwrap().label(&Some(session.clone()));
            if let Ok(value) = evaluate(conn, session, expr.clone()).await {
                let mut guard = state.lock().unwrap();
                let Some(watch) = guard.watches.get_mut(&name) else {
                    return;
                };
                if watch.last.as_ref() != Some(&value) {
                    let from = watch.last.as_ref().map(value_preview);
                    let to = value_preview(&value);
                    watch.last = Some(value);
                    watch.changes += 1;
                    let at_ms = guard.now_ms();
                    guard.emit(TimelineEvent {
                        at_ms,
                        source: Source::Renderer,
                        target: label,
                        track: Track::Watch(cdp::WatchDelta { name: name.clone(), from, to }),
                    });
                }
            }
        }
        sleep(Duration::from_millis(interval_ms)).await;
    }
}

/// A bounded rendering of a watched value — deltas record *that* state changed; `eval` fetches
/// the full value.
fn value_preview(value: &Value) -> String {
    const CAP: usize = 80;
    let text = checks::render_value(value);
    if text.chars().count() <= CAP {
        return text;
    }
    let kept: String = text.chars().take(CAP - 1).collect();
    format!("{kept}…")
}

/// Resolve `(conn, session)` fresh — polling loops re-resolve every pass so a target that
/// reloaded mid-wait keeps being probed instead of erroring on a dead session.
fn session_for(state: &Shared, target: Option<&str>) -> Option<(CdpConnection, String)> {
    let state = state.lock().unwrap();
    state.resolve(target).map(|(session, _)| (state.conn.clone(), session))
}

async fn wait_reply(
    state: &Shared,
    target: Option<String>,
    expr: String,
    timeout_ms: u64,
    json: bool,
) -> Reply {
    let started = Instant::now();
    loop {
        let last = match session_for(state, target.as_deref()) {
            Some((conn, session)) => match evaluate(conn, session, expr.clone()).await {
                Ok(value) if checks::truthy(&value) => {
                    let elapsed = started.elapsed().as_millis() as u64;
                    let output = if json {
                        json!({ "satisfied": true, "elapsedMs": elapsed, "value": value })
                            .to_string()
                    } else {
                        format!(
                            "wait      satisfied after {elapsed}ms\nvalue     {}",
                            checks::render_value(&value)
                        )
                    };
                    return Reply::ok(output);
                }
                Ok(value) => checks::render_value(&value),
                Err(error) => format!("eval error: {error}"),
            },
            None => no_target(),
        };
        if started.elapsed() >= Duration::from_millis(timeout_ms) {
            let output = if json {
                json!({ "satisfied": false, "timeoutMs": timeout_ms, "last": last }).to_string()
            } else {
                format!("wait      NOT satisfied within {timeout_ms}ms\nlast      {last}")
            };
            return Reply::fail(output);
        }
        sleep(POLL_INTERVAL).await;
    }
}

async fn expect_reply(
    state: &Shared,
    expectation: Expectation,
    within_ms: u64,
    json: bool,
) -> Reply {
    match expectation {
        Expectation::Text { target, needle } => {
            let label = format!("text {needle:?} on screen");
            let check = move |value: &Value| {
                value
                    .as_str()
                    .is_some_and(|text| text.to_lowercase().contains(&needle.to_lowercase()))
            };
            expect_probe(state, target, TEXT_PROBE.to_owned(), check, label, within_ms, json).await
        }
        Expectation::Eval { target, expr, equals, contains } => {
            let label = match (&equals, &contains) {
                (Some(expected), _) => format!("eval {expr} == {expected}"),
                (None, Some(text)) => format!("eval {expr} contains {text:?}"),
                (None, None) => format!("eval {expr} truthy"),
            };
            let check = move |value: &Value| match (&equals, &contains) {
                (Some(expected), _) => checks::value_equals(value, expected),
                (None, Some(text)) => checks::render_value(value).contains(text.as_str()),
                (None, None) => checks::truthy(value),
            };
            expect_probe(state, target, expr.clone(), check, label, within_ms, json).await
        }
        Expectation::Net { pattern, status, query } => {
            expect_net(state, pattern, status, query, within_ms, json).await
        }
        Expectation::NoErrors { query } => expect_no_errors(state, query, json),
    }
}

const TEXT_PROBE: &str = "document.body.innerText";

/// The shared shape of a polled page expectation: evaluate `probe`, test the value, report
/// satisfied-after or not-within, with the last value seen as evidence.
async fn expect_probe(
    state: &Shared,
    target: Option<String>,
    probe: String,
    check: impl Fn(&Value) -> bool,
    label: String,
    within_ms: u64,
    json: bool,
) -> Reply {
    let started = Instant::now();
    loop {
        let last = match session_for(state, target.as_deref()) {
            Some((conn, session)) => match evaluate(conn, session, probe.clone()).await {
                Ok(value) if check(&value) => {
                    let elapsed = started.elapsed().as_millis() as u64;
                    let output = if json {
                        json!({ "expect": label, "satisfied": true, "elapsedMs": elapsed })
                            .to_string()
                    } else {
                        format!("expect    {label} — satisfied after {elapsed}ms")
                    };
                    return Reply::ok(output);
                }
                Ok(value) => checks::render_value(&value),
                Err(error) => format!("eval error: {error}"),
            },
            None => no_target(),
        };
        if started.elapsed() >= Duration::from_millis(within_ms) {
            // Whole-page text is too big to echo as evidence; everything else shows what it saw.
            let evidence =
                if probe == TEXT_PROBE { String::new() } else { format!("\nlast      {last}") };
            let output = if json {
                json!({ "expect": label, "satisfied": false, "withinMs": within_ms, "last": last })
                    .to_string()
            } else {
                format!("expect    {label} — NOT satisfied within {within_ms}ms{evidence}")
            };
            return Reply::fail(output);
        }
        sleep(POLL_INTERVAL).await;
    }
}

async fn expect_net(
    state: &Shared,
    pattern: String,
    status: Option<String>,
    query: TimelineQuery,
    within_ms: u64,
    json: bool,
) -> Reply {
    let label = match &status {
        Some(status) => format!("net {pattern} → {status}"),
        None => format!("net {pattern} answered"),
    };
    let started = Instant::now();
    loop {
        let events = match collect_timeline(state, &query) {
            Ok(events) => events,
            Err(error) => return Reply::fail(error),
        };
        let matched = events.iter().find(|event| match &event.track {
            Track::Network(net) => checks::net_matches(net, &pattern, status.as_deref()),
            _ => false,
        });
        if let Some(event) = matched {
            let elapsed = started.elapsed().as_millis() as u64;
            let output = if json {
                json!({ "expect": label, "satisfied": true, "elapsedMs": elapsed, "event": event })
                    .to_string()
            } else {
                format!(
                    "expect    {label} — satisfied after {elapsed}ms\nevent     {}",
                    format::event_line(event)
                )
            };
            return Reply::ok(output);
        }
        if started.elapsed() >= Duration::from_millis(within_ms) {
            // Near-misses — same URL, different answer — are the diagnosis, so show them.
            let mut near: Vec<String> = events
                .iter()
                .filter(|event| match &event.track {
                    Track::Network(net) => checks::net_matches(net, &pattern, None),
                    _ => false,
                })
                .map(format::event_line)
                .collect();
            near = near.split_off(near.len().saturating_sub(4));
            let output = if json {
                json!({ "expect": label, "satisfied": false, "withinMs": within_ms, "near": near })
                    .to_string()
            } else {
                let mut out =
                    vec![format!("expect    {label} — NOT satisfied within {within_ms}ms")];
                for line in &near {
                    out.push(format!("saw       {line}"));
                }
                out.join("\n")
            };
            return Reply::fail(output);
        }
        sleep(POLL_INTERVAL).await;
    }
}

fn expect_no_errors(state: &Shared, query: TimelineQuery, json: bool) -> Reply {
    let events = match collect_timeline(state, &query) {
        Ok(events) => events,
        Err(error) => return Reply::fail(error),
    };
    let groups = cdp::group_errors(&events);
    if json {
        let value = json!({
            "expect": "no-errors",
            "satisfied": groups.is_empty(),
            "windowEvents": events.len(),
            "errorGroups": groups,
        });
        let rendered = serde_json::to_string_pretty(&value).unwrap_or_default();
        return if groups.is_empty() { Reply::ok(rendered) } else { Reply::fail(rendered) };
    }
    if groups.is_empty() {
        return Reply::ok(format!(
            "expect    no-errors — clean ({} events in window)",
            events.len()
        ));
    }
    let mut out = vec![format!("expect    no-errors — {} error group(s) in window", groups.len())];
    for group in groups.iter().take(6) {
        out.push(format!("error     {} ({}×)", group.event.track.detail(), group.count));
    }
    out.push("raw       kit cdp errors --explain".to_owned());
    Reply::fail(out.join("\n"))
}

async fn verify_reply(
    state: &Shared,
    target: Option<String>,
    window: Option<TimelineQuery>,
    json: bool,
) -> Reply {
    let (query, label) = match window {
        Some(query) => {
            let label = match &query.since_mark {
                Some(mark) => format!("since mark {mark}"),
                None => format!("last {}ms", query.since_ms),
            };
            (query, label)
        }
        None => {
            let has_action = state.lock().unwrap().marks.contains_key(LAST_ACTION_MARK);
            let query = TimelineQuery {
                since_ms: 30_000,
                since_mark: has_action.then(|| LAST_ACTION_MARK.to_owned()),
                tracks: None,
                source: None,
                target: None,
                grep: None,
                extension: None,
                limit: None,
            };
            let label = if has_action {
                "since last action".to_owned()
            } else {
                "last 30s (no actions yet)".to_owned()
            };
            (query, label)
        }
    };

    let events = match collect_timeline(state, &query) {
        Ok(events) => events,
        Err(error) => return Reply::fail(error),
    };
    let groups = cdp::group_errors(&events);
    let requests = events
        .iter()
        .filter(|event| {
            matches!(&event.track, Track::Network(net) if matches!(net.phase, cdp::NetPhase::Request))
        })
        .count();
    // "Failed" means failed to the app: a dead request *or* an error-status response — the
    // network line must agree with the error groups, not hide a 500 behind "0 failed".
    let failed_requests = events
        .iter()
        .filter(|event| match &event.track {
            Track::Network(net) => {
                event.track.is_error()
                    || matches!(net.phase, cdp::NetPhase::Response)
                        && net.status.is_some_and(|status| status >= 400)
            }
            _ => false,
        })
        .count();

    let document = match session_for(state, target.as_deref()) {
        Some((conn, session)) => probe_document(&conn, &session).await,
        None => None,
    };
    let ready = document.as_ref().map(|doc| doc.ready_state.as_str()).unwrap_or("unreachable");
    let ready_ok = ready == "complete";
    let pass = ready_ok && groups.is_empty();

    if json {
        let value = json!({
            "verdict": if pass { "pass" } else { "fail" },
            "window": label,
            "events": events.len(),
            "ready": ready,
            "errorGroups": groups,
            "requests": requests,
            "failedRequests": failed_requests,
        });
        let rendered = serde_json::to_string_pretty(&value).unwrap_or_default();
        return if pass { Reply::ok(rendered) } else { Reply::fail(rendered) };
    }

    let mut out = vec![
        format!(
            "verify    {} · {label} · {} events",
            if pass { "PASS" } else { "FAIL" },
            events.len()
        ),
        format!("ready     {ready}"),
    ];
    if groups.is_empty() {
        out.push("errors    none".to_owned());
    } else {
        out.push(format!("errors    {} group(s)", groups.len()));
        for group in groups.iter().take(6) {
            out.push(format!("          {} ({}×)", group.event.track.detail(), group.count));
        }
    }
    out.push(format!("network   {requests} requests, {failed_requests} failed"));
    if !pass {
        out.push(
            "raw       kit cdp errors --explain · kit cdp tail --since-mark last-action".to_owned(),
        );
    }
    if pass {
        Reply::ok(out.join("\n"))
    } else {
        Reply::fail(out.join("\n"))
    }
}

fn bundle_reply(
    state: &Shared,
    since: Option<String>,
    include: Vec<String>,
    include_secrets: bool,
    json: bool,
) -> Reply {
    let (name, events, settings, rules, launch) = {
        let state = state.lock().unwrap();
        let now = state.now_ms();
        let window = match since.as_ref() {
            Some(mark) => match state.marks.get(mark) {
                Some(at_ms) => now.saturating_sub(*at_ms),
                None => return Reply::fail(format!("unknown mark '{mark}'")),
            },
            None => 60_000,
        };
        (
            state.name.clone(),
            state.timeline.since(now, window, None, None),
            state.settings.clone(),
            state.net_rules.clone(),
            registry::read_launch(&state.name),
        )
    };
    let dir = registry::artifact_dir(&name).join(format!("bundle-{}", now_unix_ms()));
    if let Err(error) =
        write_bundle(&dir, &events, &settings, &rules, &launch, &include, include_secrets)
    {
        return Reply::fail(error.to_string());
    }
    if json {
        Reply::ok(json!({ "bundle": dir, "events": events.len() }).to_string())
    } else {
        Reply::ok(format!("bundle {}\nevents {}", dir.display(), events.len()))
    }
}

async fn net_reply(state: &Shared, command: NetCommand, json: bool) -> Reply {
    match command {
        NetCommand::Failed { query } => {
            let events = match collect_timeline(state, &query) {
                Ok(events) => events,
                Err(error) => return Reply::fail(error),
            }
            .into_iter()
            .filter(|event| event.track.is_error())
            .collect::<Vec<_>>();
            Reply::ok(format::events(&events, json))
        }
        NetCommand::Slow { query } => net_slow_reply(state, &query, json),
        NetCommand::Show { request_id } => net_show_reply(state, &request_id, json),
        NetCommand::Block { pattern } => {
            let (conn, sessions) = {
                let mut state = state.lock().unwrap();
                state.net_rules.push(NetRule::Block { pattern });
                (state.conn.clone(), state.sessions.keys().cloned().collect::<Vec<_>>())
            };
            for session in sessions {
                enable_fetch(&conn, &session).await;
            }
            net_rules_reply(state, json)
        }
        NetCommand::Mock { method, pattern, body, status, mime } => {
            let (conn, sessions) = {
                let mut state = state.lock().unwrap();
                state.net_rules.push(NetRule::Mock { method, pattern, body, status, mime });
                (state.conn.clone(), state.sessions.keys().cloned().collect::<Vec<_>>())
            };
            for session in sessions {
                enable_fetch(&conn, &session).await;
            }
            net_rules_reply(state, json)
        }
        NetCommand::Rules => net_rules_reply(state, json),
        NetCommand::RulesClear => {
            let (conn, sessions) = {
                let mut state = state.lock().unwrap();
                state.net_rules.clear();
                (state.conn.clone(), state.sessions.keys().cloned().collect::<Vec<_>>())
            };
            for session in sessions {
                let _ = conn.call(Some(&session), "Fetch.disable", json!({})).await;
            }
            net_rules_reply(state, json)
        }
    }
}

fn event_matches(event: &TimelineEvent, query: &TimelineQuery) -> bool {
    if query.target.as_ref().is_some_and(|needle| !contains_ci(&event.target, needle)) {
        return false;
    }

    let line = format::event_line(event);
    if query.grep.as_ref().is_some_and(|needle| !contains_ci(&line, needle)) {
        return false;
    }

    if query
        .extension
        .as_ref()
        .is_some_and(|needle| !contains_ci(&event.target, needle) && !contains_ci(&line, needle))
    {
        return false;
    }

    true
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

async fn extension_bundle_reply(
    state: &Shared,
    target: Option<String>,
    source: String,
    extension_id: String,
    mut query: TimelineQuery,
    json: bool,
) -> Reply {
    query.extension = Some(extension_id.clone());
    let lens = {
        let expr = wrap_lens(&source, std::slice::from_ref(&extension_id), &lens_context(state));
        run_in_target(state, target, true, |conn, session| evaluate(conn, session, expr)).await
    };

    let doctor = match lens {
        Reply { ok: true, output } => {
            serde_json::from_str::<Value>(&output).unwrap_or_else(|_| json!({ "raw": output }))
        }
        Reply { output, .. } => {
            return Reply::fail(output);
        }
    };

    let timeline = match collect_timeline(state, &query) {
        Ok(timeline) => timeline,
        Err(error) => return Reply::fail(error),
    };
    let bundle = json!({
        "extensionId": extension_id,
        "doctor": doctor,
        "timeline": timeline,
        "timelineQuery": query,
    });
    Reply::ok(format::value(&bundle, json))
}

fn ignore_reply(state: &Shared, op: IgnoreOp, json: bool) -> Reply {
    let mut state = state.lock().unwrap();
    match op {
        IgnoreOp::Add(pattern) => {
            if !state.ignore.contains(&pattern) {
                state.ignore.push(pattern);
            }
        }
        IgnoreOp::Clear => state.ignore.clear(),
        IgnoreOp::List => {}
    }
    Reply::ok(format::ignore(&state.ignore, json))
}

async fn heap_reply(state: &Shared, target: Option<String>, json: bool) -> Reply {
    let resolved = {
        let state = state.lock().unwrap();
        state
            .resolve(target.as_deref())
            .map(|(session, target)| (state.conn.clone(), session, target))
    };
    let Some((conn, session, target)) = resolved else {
        return Reply::fail(no_target());
    };
    let metrics = cdp::probe_metrics(&conn, Some(&session)).await;
    Reply::ok(format::heap(&target.label(), &metrics, json))
}

/// How far back `ready` looks for fatal errors, and how many it shows — recent failures that would
/// explain a workbench that loaded but doesn't work.
const READY_ERROR_WINDOW_MS: u64 = 60_000;
const READY_ERROR_CAP: usize = 8;

/// A generic document-readiness probe — no app knowledge, just the DOM signals that say whether a
/// target is loaded, shown, and populated. App state (the `__testAPI` bridge, workspace/editor) is a
/// lens, never here.
const READY_PROBE: &str = "(()=>{const b=document.body;return{\
    href:location.href,title:document.title,readyState:document.readyState,\
    visibility:document.visibilityState,focused:document.hasFocus(),\
    bodyTextLen:((b&&b.innerText)||'').trim().length};})()";

const FOCUS_PROBE: &str = "(()=>{const el=document.activeElement;if(!el)return null;\
const label=el.getAttribute('aria-label')||el.innerText||el.value||el.placeholder||'';\
return{tag:el.tagName.toLowerCase(),role:el.getAttribute('role'),label:String(label).trim().slice(0,120)}})()";

async fn ready_reply(state: &Shared, target: Option<String>, json: bool) -> Reply {
    let (name, candidates, resolved, recent_errors) = {
        let state = state.lock().unwrap();
        let targets: Vec<Target> = state.sessions.values().cloned().collect();
        let activity = state.timeline.counts_by_target();
        let candidates = readiness::rank(&targets, &activity, target.as_deref());
        let resolved =
            state.resolve(target.as_deref()).map(|(session, _)| (state.conn.clone(), session));
        let now = state.now_ms();
        let recent_errors = state
            .timeline
            .since(now, READY_ERROR_WINDOW_MS, Some(&[TrackKind::Exception]), None)
            .iter()
            .rev()
            .take(READY_ERROR_CAP)
            .rev()
            // One line per error — the readiness glance wants "what failed", not full stacks.
            .map(|event| format::event_line(event).lines().next().unwrap_or_default().to_owned())
            .collect();
        (state.name.clone(), candidates, resolved, recent_errors)
    };

    let document = match resolved {
        Some((conn, session)) => probe_document(&conn, &session).await,
        None => None,
    };

    Reply::ok(readiness::render(
        &Readiness { instance: name, document, candidates, recent_errors },
        json,
    ))
}

/// Run the generic readiness probe in a target and decode it. A failed probe (a target mid-reload,
/// say) degrades to `None` rather than failing the whole verdict — the candidate table still prints.
async fn probe_document(conn: &CdpConnection, session: &str) -> Option<DocState> {
    let value = evaluate(conn.clone(), session.to_owned(), READY_PROBE.to_owned()).await.ok()?;
    serde_json::from_value(value).ok()
}

/// How many added/removed lines a text diff prints before counting the rest.
const DIFF_LINE_CAP: usize = 40;

async fn snap_reply(
    state: &Shared,
    target: Option<String>,
    interactive: bool,
    diff: bool,
    json: bool,
) -> Reply {
    let resolved = {
        let state = state.lock().unwrap();
        state.resolve(target.as_deref()).map(|(session, _)| (state.conn.clone(), session))
    };
    let Some((conn, session)) = resolved else {
        return Reply::fail(no_target());
    };

    let _ = conn.call(Some(&session), "Accessibility.enable", json!({})).await;
    let tree = match conn.call(Some(&session), "Accessibility.getFullAXTree", json!({})).await {
        Ok(tree) => tree,
        Err(error) => return Reply::fail(error.to_string()),
    };

    let snap = snapshot::build(&tree, interactive);
    let previous = {
        let mut state = state.lock().unwrap();
        let at_ms = state.now_ms();
        state.refs.insert(session.clone(), snap.refs.clone());
        state.snapshots.insert(session, (at_ms, snap.semantic.clone()))
    };

    if !diff {
        return if json {
            Reply::ok(serde_json::to_string_pretty(&snap).unwrap_or_default())
        } else {
            Reply::ok(format!("{}\n[{} refs]", snap.text, snap.refs.len()))
        };
    }

    let Some((baseline_ms, baseline)) = previous else {
        return Reply::ok("no previous snap to diff against — this snap is now the baseline");
    };
    let changes = snapshot::diff(&baseline, &snap.semantic);
    if json {
        let value = json!({
            "baselineAtMs": baseline_ms,
            "added": changes.added,
            "removed": changes.removed,
            "unchanged": changes.unchanged,
        });
        return Reply::ok(serde_json::to_string_pretty(&value).unwrap_or_default());
    }
    if changes.added.is_empty() && changes.removed.is_empty() {
        return Reply::ok(format!(
            "no UI changes since +{baseline_ms}ms ({} nodes unchanged)",
            changes.unchanged
        ));
    }
    let mut out = vec![format!("snap diff vs +{baseline_ms}ms")];
    for line in changes.removed.iter().take(DIFF_LINE_CAP) {
        out.push(format!("- {line}"));
    }
    if changes.removed.len() > DIFF_LINE_CAP {
        out.push(format!("… and {} more removed", changes.removed.len() - DIFF_LINE_CAP));
    }
    for line in changes.added.iter().take(DIFF_LINE_CAP) {
        out.push(format!("+ {line}"));
    }
    if changes.added.len() > DIFF_LINE_CAP {
        out.push(format!("… and {} more added", changes.added.len() - DIFF_LINE_CAP));
    }
    out.push(format!(
        "{} added · {} removed · {} unchanged",
        changes.added.len(),
        changes.removed.len(),
        changes.unchanged
    ));
    Reply::ok(out.join("\n"))
}

/// The Timeline mark every interaction (re)sets, so `after`, `tail --since-mark`, and `verify`
/// can bound themselves to "since the last thing I did" without inventing names.
const LAST_ACTION_MARK: &str = "last-action";

/// A locator resolved to a live element: the connection/session to drive plus the matched ref
/// entry, which carries the element's identity for output.
struct Element {
    conn: CdpConnection,
    session: String,
    entry: snapshot::RefEntry,
}

/// Resolve a locator against a target. A `@ref` hits the last snapshot's table; a role:name query
/// takes a fresh accessibility snapshot and refreshes that table, so the `@ref` it prints stays
/// valid for follow-up commands.
async fn resolve_element(
    state: &Shared,
    target: Option<String>,
    locator: &Locator,
) -> Result<Element, String> {
    let resolved = {
        let state = state.lock().unwrap();
        state.resolve(target.as_deref()).map(|(session, _)| (state.conn.clone(), session))
    };
    let Some((conn, session)) = resolved else {
        return Err(no_target());
    };
    let entry = match locator {
        Locator::Ref(reference) => state
            .lock()
            .unwrap()
            .refs
            .get(&session)
            .and_then(|refs| refs.iter().find(|entry| entry.reference == *reference).cloned())
            .ok_or_else(|| {
                format!(
                    "unknown ref '@{reference}' — refs are document-scoped; run `kit cdp snap` \
                     again or use a role:name locator"
                )
            })?,
        Locator::Query { role, name } => {
            let refs = fresh_refs(state, &conn, &session).await?;
            snapshot::find(&refs, role.as_deref(), name)?.clone()
        }
    };
    Ok(Element { conn, session, entry })
}

/// Take a fresh accessibility snapshot for a session and refresh its stored ref table.
async fn fresh_refs(
    state: &Shared,
    conn: &CdpConnection,
    session: &str,
) -> Result<Vec<snapshot::RefEntry>, String> {
    let _ = conn.call(Some(session), "Accessibility.enable", json!({})).await;
    let tree = conn
        .call(Some(session), "Accessibility.getFullAXTree", json!({}))
        .await
        .map_err(|error| error.to_string())?;
    let snap = snapshot::build(&tree, true);
    state.lock().unwrap().refs.insert(session.to_owned(), snap.refs.clone());
    Ok(snap.refs)
}

/// Record an interaction on the Timeline — the action sits on the same clock as its consequences
/// — and (re)set the `last-action` mark. Returns the action's timestamp.
fn action_marker(state: &Shared, description: &str) -> u64 {
    add_mark(state, LAST_ACTION_MARK, description)
}

/// Insert a named mark and record it on the Timeline, so the moment is both addressable
/// (`--since-mark`) and visible in every slice.
fn add_mark(state: &Shared, name: &str, description: &str) -> u64 {
    let mut state = state.lock().unwrap();
    let at_ms = state.now_ms();
    state.marks.insert(name.to_owned(), at_ms);
    state.emit(TimelineEvent {
        at_ms,
        source: Source::Renderer,
        target: "kit".to_owned(),
        track: Track::Log(LogEntry {
            level: "info".to_owned(),
            source: "kit".to_owned(),
            text: description.to_owned(),
            url: None,
            line: None,
        }),
    });
    at_ms
}

/// The mark a `do` batch sets at its start — the whole run's evidence window.
const DO_MARK: &str = "do-start";

/// Run a step batch: each step dispatches through the same handler as its standalone command,
/// output condenses to a line per passing step, and the first failure stops the run with its full
/// evidence plus what was skipped.
async fn do_reply(
    state: &Shared,
    steps: Vec<super::protocol::Step>,
    json: bool,
    shutdown: &mpsc::Sender<()>,
) -> Reply {
    add_mark(state, DO_MARK, &format!("do: {} steps", steps.len()));
    let total = steps.len();
    let mut rendered: Vec<String> = Vec::new();
    let mut results: Vec<Value> = Vec::new();
    let mut failed_at: Option<usize> = None;
    let mut remaining: Vec<String> = Vec::new();

    let mut queue = steps.into_iter().enumerate();
    for (index, step) in queue.by_ref() {
        let number = index + 1;
        // Box the recursive dispatch future: do → dispatch → do would otherwise be infinitely sized.
        let reply = Box::pin(dispatch(state, *step.command, json, shutdown)).await;
        if json {
            let output = serde_json::from_str::<Value>(&reply.output)
                .unwrap_or(Value::String(reply.output.clone()));
            results.push(json!({
                "step": number,
                "line": step.line,
                "ok": reply.ok,
                "output": output,
            }));
        }
        if reply.ok {
            rendered.push(format!("✓ {number}  {}", condense(&reply.output)));
        } else {
            rendered.push(format!("✗ {number}  {}", step.line));
            for line in reply.output.lines() {
                rendered.push(format!("     {line}"));
            }
            failed_at = Some(number);
            break;
        }
    }
    for (_, step) in queue {
        remaining.push(step.line);
    }

    if json {
        let value = json!({
            "steps": results,
            "total": total,
            "failedAt": failed_at,
            "skipped": remaining,
            "rawCommand": format!("kit cdp tail --since-mark {DO_MARK}"),
        });
        let output = serde_json::to_string_pretty(&value).unwrap_or_default();
        return if failed_at.is_none() { Reply::ok(output) } else { Reply::fail(output) };
    }

    for line in &remaining {
        rendered.push(format!("○ -  {line} (skipped)"));
    }
    let verdict = match failed_at {
        None => format!("do        PASS · {total}/{total} steps"),
        Some(number) => format!(
            "do        FAIL at step {number} · {} passed · {} skipped",
            number - 1,
            remaining.len()
        ),
    };
    rendered.push(verdict);
    rendered.push(format!("raw       kit cdp tail --since-mark {DO_MARK}"));
    let output = rendered.join("\n");
    if failed_at.is_none() {
        Reply::ok(output)
    } else {
        Reply::fail(output)
    }
}

/// One line for a passing step: its reply's headline, plus the settle summary when there is one,
/// padding squeezed out.
fn condense(output: &str) -> String {
    let mut lines = output.lines();
    let first = squeeze(lines.next().unwrap_or_default());
    match lines.next() {
        Some(second) if second.starts_with("settled") => {
            format!("{first} · {}", squeeze(second.trim_start_matches("settled")))
        }
        _ => first,
    }
}

fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

async fn click_reply(
    state: &Shared,
    target: Option<String>,
    locator: Locator,
    settle: Option<Settle>,
    json: bool,
) -> Reply {
    let element = match resolve_element(state, target, &locator).await {
        Ok(element) => element,
        Err(error) => return Reply::fail(error),
    };
    let _ = element.conn.call(Some(&element.session), "Page.bringToFront", json!({})).await;
    // Real mouse events hit-test against compositor frames, which a hidden/occluded window never
    // produces — dispatch stalls ~5s per event. When the window stays hidden after bringToFront,
    // fall back to a synthetic `el.click()` and say so (synthetic events skip pointer handlers).
    let visible = page_visible(&element).await;
    let started_ms = action_marker(state, &format!("click {}", element.entry.display()));
    let delivery = if visible {
        click_at(&element.conn, &element.session, element.entry.backend).await
    } else {
        click_js(&element.conn, &element.session, element.entry.backend).await
    };
    if let Err(error) = delivery {
        return Reply::fail(stale_element_hint(error, &locator));
    }
    let mut headline = format!("clicked {}", element.entry.display());
    if !visible {
        headline.push_str(" (synthetic — window hidden)");
    }
    action_outcome(state, headline, started_ms, settle, json).await
}

async fn page_visible(element: &Element) -> bool {
    evaluate(element.conn.clone(), element.session.clone(), "document.visibilityState".to_owned())
        .await
        .is_ok_and(|value| value.as_str() == Some("visible"))
}

async fn click_js(conn: &CdpConnection, session: &str, backend: i64) -> Result<(), String> {
    let object = resolve_object(conn, session, backend).await?;
    conn.call(
        Some(session),
        "Runtime.callFunctionOn",
        json!({
            "objectId": object,
            "functionDeclaration":
                "function(){ this.scrollIntoView({block:'center',inline:'center'}); this.click(); }",
        }),
    )
    .await
    .map_err(|error| error.to_string())?;
    Ok(())
}

async fn fill_reply(
    state: &Shared,
    target: Option<String>,
    locator: Locator,
    text: String,
    settle: Option<Settle>,
    json: bool,
) -> Reply {
    let element = match resolve_element(state, target, &locator).await {
        Ok(element) => element,
        Err(error) => return Reply::fail(error),
    };
    let _ = element.conn.call(Some(&element.session), "Page.bringToFront", json!({})).await;
    let started_ms = action_marker(state, &format!("fill {} ← {text:?}", element.entry.display()));
    if let Err(error) = fill_at(&element.conn, &element.session, element.entry.backend, &text).await
    {
        return Reply::fail(stale_element_hint(error, &locator));
    }
    let headline = format!("filled {} with {text:?}", element.entry.display());
    action_outcome(state, headline, started_ms, settle, json).await
}

async fn press_reply(
    state: &Shared,
    target: Option<String>,
    chord: super::protocol::KeyChord,
    settle: Option<Settle>,
    json: bool,
) -> Reply {
    let Some((conn, session)) = session_for(state, target.as_deref()) else {
        return Reply::fail(no_target());
    };
    let _ = conn.call(Some(&session), "Page.bringToFront", json!({})).await;
    let display = chord_display(&chord);
    let started_ms = action_marker(state, &format!("press {display}"));
    if let Err(error) = dispatch_chord(&conn, &session, &chord).await {
        return Reply::fail(error);
    }
    action_outcome(state, format!("pressed {display}"), started_ms, settle, json).await
}

fn chord_display(chord: &super::protocol::KeyChord) -> String {
    let mut parts = Vec::new();
    if chord.modifiers & 2 != 0 {
        parts.push("Ctrl");
    }
    if chord.modifiers & 1 != 0 {
        parts.push("Alt");
    }
    if chord.modifiers & 8 != 0 {
        parts.push("Shift");
    }
    if chord.modifiers & 4 != 0 {
        parts.push("Meta");
    }
    parts.push(&chord.key);
    parts.join("+")
}

/// Key events go to the focused frame directly (no compositor hit-testing), so unlike clicks
/// they are safe on hidden windows.
async fn dispatch_chord(
    conn: &CdpConnection,
    session: &str,
    chord: &super::protocol::KeyChord,
) -> Result<(), String> {
    let (code, virtual_code, text) = key_details(&chord.key);
    let base = json!({
        "modifiers": chord.modifiers,
        "key": chord.key,
        "code": code,
        "windowsVirtualKeyCode": virtual_code,
    });
    let mut down = base.clone();
    down["type"] = json!(if text.is_some() { "keyDown" } else { "rawKeyDown" });
    if let Some(text) = &text {
        down["text"] = json!(text);
    }
    let mut up = base;
    up["type"] = json!("keyUp");
    for event in [down, up] {
        conn.call(Some(session), "Input.dispatchKeyEvent", event)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// CDP key event details for a canonical key name: its `code`, virtual key code, and the text it
/// types (committing keys like Enter and Tab carry text; pure navigation keys don't).
fn key_details(key: &str) -> (String, i64, Option<String>) {
    match key {
        "Enter" => ("Enter".to_owned(), 13, Some("\r".to_owned())),
        "Tab" => ("Tab".to_owned(), 9, Some("\t".to_owned())),
        "Escape" => ("Escape".to_owned(), 27, None),
        "Backspace" => ("Backspace".to_owned(), 8, None),
        "Delete" => ("Delete".to_owned(), 46, None),
        "Space" => ("Space".to_owned(), 32, Some(" ".to_owned())),
        "ArrowUp" => ("ArrowUp".to_owned(), 38, None),
        "ArrowDown" => ("ArrowDown".to_owned(), 40, None),
        "ArrowLeft" => ("ArrowLeft".to_owned(), 37, None),
        "ArrowRight" => ("ArrowRight".to_owned(), 39, None),
        "Home" => ("Home".to_owned(), 36, None),
        "End" => ("End".to_owned(), 35, None),
        "PageUp" => ("PageUp".to_owned(), 33, None),
        "PageDown" => ("PageDown".to_owned(), 34, None),
        // A single character: code KeyX for letters, text is the character itself.
        other => {
            let upper = other.to_uppercase();
            let code = if other.chars().all(|ch| ch.is_ascii_alphabetic()) {
                format!("Key{upper}")
            } else {
                String::new()
            };
            let virtual_code = upper.chars().next().map(|ch| ch as i64).unwrap_or(0);
            (code, virtual_code, Some(other.to_owned()))
        }
    }
}

const SELECT_OPTION: &str = "function(label){\
    const options = Array.from(this.options || []);\
    const found = options.find(option =>\
        option.label.trim() === label || option.value === label || option.textContent.trim() === label);\
    if (!found) return { ok: false, options: options.map(option => option.label.trim()) };\
    this.value = found.value;\
    this.dispatchEvent(new Event('input', { bubbles: true }));\
    this.dispatchEvent(new Event('change', { bubbles: true }));\
    return { ok: true, value: found.value };\
}";

async fn select_reply(
    state: &Shared,
    target: Option<String>,
    locator: Locator,
    option: String,
    settle: Option<Settle>,
    json: bool,
) -> Reply {
    let element = match resolve_element(state, target, &locator).await {
        Ok(element) => element,
        Err(error) => return Reply::fail(error),
    };
    let started_ms =
        action_marker(state, &format!("select {option:?} in {}", element.entry.display()));
    let object = match resolve_object(&element.conn, &element.session, element.entry.backend).await
    {
        Ok(object) => object,
        Err(error) => return Reply::fail(stale_element_hint(error, &locator)),
    };
    let result = element
        .conn
        .call(
            Some(&element.session),
            "Runtime.callFunctionOn",
            json!({
                "objectId": object,
                "functionDeclaration": SELECT_OPTION,
                "arguments": [{ "value": option }],
                "returnByValue": true,
            }),
        )
        .await;
    let value = match result {
        Ok(value) => value.pointer("/result/value").cloned().unwrap_or(Value::Null),
        Err(error) => return Reply::fail(error.to_string()),
    };
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        let options: Vec<String> = value
            .pointer("/options")
            .and_then(Value::as_array)
            .map(|options| options.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();
        return Reply::fail(format!(
            "no option {:?} in {} — options: {}",
            option,
            element.entry.display(),
            options.join(", ")
        ));
    }
    let headline = format!("selected {option:?} in {}", element.entry.display());
    action_outcome(state, headline, started_ms, settle, json).await
}

/// A ref that resolved but no longer drives a node usually means the document changed underneath
/// it; say so instead of leaking a bare protocol error.
fn stale_element_hint(error: String, locator: &Locator) -> String {
    match locator {
        Locator::Ref(_) => format!(
            "{error} — refs are document-scoped; re-run `kit cdp snap` or use a role:name locator"
        ),
        Locator::Query { .. } => error,
    }
}

/// The settle half of an interaction: wait for the Timeline to go quiet, then report what the
/// action caused — counts first, the recent lines, and the raw escape hatch.
async fn action_outcome(
    state: &Shared,
    headline: String,
    started_ms: u64,
    settle: Option<Settle>,
    json: bool,
) -> Reply {
    let Some(settle) = settle else {
        return if json {
            Reply::ok(json!({ "action": headline }).to_string())
        } else {
            Reply::ok(headline)
        };
    };
    let timed_out = wait_for_quiet(state, settle.idle_ms, settle.timeout_ms).await;
    let events: Vec<TimelineEvent> = {
        let state = state.lock().unwrap();
        let now = state.now_ms();
        state
            .timeline
            .since(now, now.saturating_sub(started_ms), None, None)
            .into_iter()
            .filter(|event| event.target != "kit" && !state.is_suppressed(event))
            .collect()
    };
    let ended = if timed_out {
        format!("still active after {}ms", settle.timeout_ms)
    } else {
        format!("idle after {}ms", settle.idle_ms)
    };
    let errors = events.iter().filter(|event| event.track.is_error()).count();
    let network = events.iter().filter(|event| matches!(event.track, Track::Network(_))).count();
    let recent: Vec<String> = events.iter().rev().take(6).rev().map(format::event_line).collect();

    if json {
        let value = json!({
            "action": headline,
            "settled": ended,
            "events": events.len(),
            "errors": errors,
            "network": network,
            "recent": recent,
            "rawCommand": format!("kit cdp tail --since-mark {LAST_ACTION_MARK}"),
        });
        return Reply::ok(serde_json::to_string_pretty(&value).unwrap_or_default());
    }
    let mut out = vec![
        headline,
        format!(
            "settled   {ended} · {} events · {errors} errors · {network} network",
            events.len()
        ),
    ];
    for line in recent {
        out.push(format!("event     {line}"));
    }
    out.push(format!("raw       kit cdp tail --since-mark {LAST_ACTION_MARK}"));
    Reply::ok(out.join("\n"))
}

async fn click_at(conn: &CdpConnection, session: &str, backend: i64) -> Result<(), String> {
    let (x, y) = match box_center(conn, session, backend).await {
        Ok(center) => center,
        Err(_) => {
            if let Ok(object) = resolve_object(conn, session, backend).await {
                let _ = conn
                    .call(
                        Some(session),
                        "Runtime.callFunctionOn",
                        json!({ "objectId": object, "functionDeclaration": "function(){ this.scrollIntoView({block:'center',inline:'center'}); }" }),
                    )
                    .await;
            }
            box_center(conn, session, backend).await?
        }
    };
    for kind in ["mouseMoved", "mousePressed", "mouseReleased"] {
        conn.call(
            Some(session),
            "Input.dispatchMouseEvent",
            json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
        )
        .await
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

async fn fill_at(
    conn: &CdpConnection,
    session: &str,
    backend: i64,
    text: &str,
) -> Result<(), String> {
    let object = resolve_object(conn, session, backend).await?;
    let call = |declaration: &'static str| {
        conn.call(
            Some(session),
            "Runtime.callFunctionOn",
            json!({ "objectId": object, "functionDeclaration": declaration }),
        )
    };
    call("function(){ this.focus && this.focus(); if ('value' in this) { this.value=''; } else if (this.isContentEditable) { this.textContent=''; } }")
        .await
        .map_err(|error| error.to_string())?;
    conn.call(Some(session), "Input.insertText", json!({ "text": text }))
        .await
        .map_err(|error| error.to_string())?;
    call("function(){ this.dispatchEvent(new Event('input',{bubbles:true})); this.dispatchEvent(new Event('change',{bubbles:true})); }")
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn box_center(
    conn: &CdpConnection,
    session: &str,
    backend: i64,
) -> Result<(f64, f64), String> {
    let model = conn
        .call(Some(session), "DOM.getBoxModel", json!({ "backendNodeId": backend }))
        .await
        .map_err(|error| error.to_string())?;
    let quad = model
        .pointer("/model/content")
        .and_then(Value::as_array)
        .ok_or_else(|| "element has no box (not rendered?)".to_owned())?;
    if quad.len() < 8 {
        return Err("malformed box model".to_owned());
    }
    let at = |index: usize| quad[index].as_f64().unwrap_or(0.0);
    Ok(((at(0) + at(2) + at(4) + at(6)) / 4.0, (at(1) + at(3) + at(5) + at(7)) / 4.0))
}

async fn resolve_object(
    conn: &CdpConnection,
    session: &str,
    backend: i64,
) -> Result<String, String> {
    let resolved = conn
        .call(Some(session), "DOM.resolveNode", json!({ "backendNodeId": backend }))
        .await
        .map_err(|error| error.to_string())?;
    resolved
        .pointer("/object/objectId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "could not resolve node".to_owned())
}

fn navigation_marker(at_ms: u64, url: &str) -> TimelineEvent {
    TimelineEvent {
        at_ms,
        source: Source::Renderer,
        target: "kit".to_owned(),
        track: Track::Log(LogEntry {
            level: "info".to_owned(),
            source: "kit".to_owned(),
            text: format!("navigated → {url}"),
            url: None,
            line: None,
        }),
    }
}

/// Resolve a target, hand its `(conn, session)` to `run`, and format the resulting value. Keeps the
/// state lock from being held across the await.
async fn run_in_target<F, Fut>(state: &Shared, target: Option<String>, json: bool, run: F) -> Reply
where
    F: FnOnce(CdpConnection, String) -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    let resolved = {
        let state = state.lock().unwrap();
        state.resolve(target.as_deref()).map(|(session, _)| (state.conn.clone(), session))
    };
    let Some((conn, session)) = resolved else {
        return Reply::fail(no_target());
    };
    match run(conn, session).await {
        Ok(value) => Reply::ok(format::value(&value, json)),
        Err(error) => Reply::fail(error),
    }
}

async fn evaluate(conn: CdpConnection, session: String, expr: String) -> Result<Value, String> {
    let result = conn
        .call(
            Some(&session),
            "Runtime.evaluate",
            json!({ "expression": expr, "returnByValue": true, "awaitPromise": true }),
        )
        .await
        .map_err(|error| error.to_string())?;

    if let Some(details) = result.get("exceptionDetails") {
        return Err(exception_message(details));
    }
    Ok(result.pointer("/result/value").cloned().unwrap_or(Value::Null))
}

/// Render `exceptionDetails` as a sentence. `exception.description` usually carries
/// "TypeError: …\n    at …" and stands alone — but a thrown non-Error leaves only a class name
/// ("Object") or a bare stack, so compose the engine's `text` with whatever identity the
/// exception carries; the message must always say *what* was thrown, not just where.
fn exception_message(details: &Value) -> String {
    let description = details.pointer("/exception/description").and_then(Value::as_str);
    if let Some(description) = description {
        let first = description.lines().next().unwrap_or_default().trim_start();
        if first.contains(": ") && !first.starts_with("at ") {
            return description.to_owned();
        }
    }
    let text = details.get("text").and_then(Value::as_str).unwrap_or("evaluation failed");
    let what = details
        .pointer("/exception/className")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| details.pointer("/exception/value").map(compact_value))
        .unwrap_or_default();
    let stack = description
        .filter(|description| description.contains('\n') || description.starts_with("at "))
        .map(|stack| format!("\n{stack}"))
        .unwrap_or_default();
    if what.is_empty() || text.ends_with(&what) {
        format!("{text}{stack}")
    } else {
        format!("{text} {what}{stack}")
    }
}

/// A lens runs as `(async function(args, kit){ <source> })(<args>, <context>)` — the script gets
/// `args`, live Target metadata, may `await` app promises, and `return`s a JSON-serializable
/// value (the evaluate call awaits the wrapper's promise).
fn wrap_lens(source: &str, args: &[String], context: &Value) -> String {
    let args_json = serde_json::to_string(args).unwrap_or_else(|_| "[]".to_owned());
    let context_json = serde_json::to_string(context).unwrap_or_else(|_| "{}".to_owned());
    format!("(async function(args, kit){{ {source} }})({args_json}, {context_json})")
}

fn lens_context(state: &Shared) -> Value {
    let state = state.lock().unwrap();
    let counts = state.timeline.counts_by_target();
    let targets: Vec<Value> = state
        .sessions
        .values()
        .map(|target| {
            let label = target.label();
            let events = counts.get(&label).copied().unwrap_or(0);
            json!({
                "label": label,
                "kind": target.kind.as_str(),
                "title": target.title.clone(),
                "url": target.url.clone(),
                "events": events,
                "extensionId": query_param(&target.url, "extensionId"),
                "purpose": query_param(&target.url, "purpose"),
            })
        })
        .collect();
    json!({
        "instance": state.name,
        "app": state.app,
        "port": state.port,
        "uptimeMs": state.now_ms(),
        "targets": targets,
    })
}

async fn screenshot_reply(
    state: &Shared,
    target: Option<String>,
    request: ScreenshotRequest,
    json: bool,
) -> Reply {
    let (name, resolved) = {
        let state = state.lock().unwrap();
        let resolved = state
            .resolve(target.as_deref())
            .map(|(session, target)| (state.conn.clone(), session, target));
        (state.name.clone(), resolved)
    };
    let Some((conn, session, target)) = resolved else {
        return Reply::fail(no_target());
    };

    if request.raise {
        if let Err(error) = conn.call(Some(&session), "Page.bringToFront", json!({})).await {
            return Reply::fail(format!("bring '{}' to front: {error}", target.label()));
        }
    }

    let path = request.out.unwrap_or_else(|| {
        registry::artifact_dir(&name).join(format!(
            "shot-{}.{}",
            now_unix_ms(),
            request.format.as_str()
        ))
    });
    let bytes = match write_screenshot(
        &conn,
        &session,
        &path,
        request.format,
        request.quality,
        request.full,
        Duration::from_millis(request.timeout_ms),
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(error) => {
            // A no-frame failure is recoverable by the caller — name the remedy; other failures
            // (a dead session, a write error) speak for themselves.
            let hint = if error.downcast_ref::<cdp::NoFrame>().is_some() && !request.raise {
                " — retry once it has painted, or pass --raise to foreground the window"
            } else {
                ""
            };
            return Reply::fail(format!("screenshot '{}': {error:#}{hint}", target.label()));
        }
    };

    if json {
        let value = json!({
            "target": target.label(),
            "path": path,
            "format": request.format.as_str(),
            "bytes": bytes,
        });
        return Reply::ok(serde_json::to_string_pretty(&value).unwrap_or_default());
    }
    Reply::ok(format!("target    {}\nscreen    {}", target.label(), path.display()))
}

/// Capture a Target and write the image to `path`, creating parent directories. Returns the byte
/// count written. `budget` caps the wait for a frame — see [`cdp::capture_screenshot`].
async fn write_screenshot(
    conn: &CdpConnection,
    session: &str,
    path: &Path,
    format: ImageFormat,
    quality: Option<u8>,
    full: bool,
    budget: Duration,
) -> Result<usize> {
    let image = cdp::capture_screenshot(conn, Some(session), format, quality, full, budget).await?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(path, &image).with_context(|| format!("write {}", path.display()))?;
    Ok(image.len())
}

fn compact_value(value: &Value) -> String {
    match value {
        Value::Null => "none".to_owned(),
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "unknown".to_owned()),
    }
}

fn network_request_id(event: &TimelineEvent) -> Option<&str> {
    match &event.track {
        Track::Network(net) => Some(&net.request_id),
        _ => None,
    }
}

fn net_show_reply(state: &Shared, request_id: &str, json: bool) -> Reply {
    let events: Vec<TimelineEvent> = {
        let state = state.lock().unwrap();
        let now = state.now_ms();
        state
            .timeline
            .since(now, 10 * 60_000, Some(&[TrackKind::Network]), None)
            .into_iter()
            .filter(|event| network_request_id(event) == Some(request_id))
            .collect()
    };
    Reply::ok(format::events(&events, json))
}

fn net_slow_reply(state: &Shared, query: &TimelineQuery, json: bool) -> Reply {
    #[derive(Default)]
    struct Row {
        first_ms: u64,
        last_ms: u64,
        count: usize,
        method: Option<String>,
        url: Option<String>,
        status: Option<u64>,
        error: Option<String>,
    }

    let events = match collect_timeline(state, query) {
        Ok(events) => events,
        Err(error) => return Reply::fail(error),
    };
    let mut rows: HashMap<String, Row> = HashMap::new();
    for event in events {
        let Track::Network(net) = event.track else {
            continue;
        };
        let row = rows.entry(net.request_id).or_insert_with(|| Row {
            first_ms: event.at_ms,
            last_ms: event.at_ms,
            count: 0,
            method: None,
            url: None,
            status: None,
            error: None,
        });
        row.first_ms = row.first_ms.min(event.at_ms);
        row.last_ms = row.last_ms.max(event.at_ms);
        row.count += 1;
        row.method = row.method.take().or(net.method);
        row.url = row.url.take().or(net.url);
        row.status = row.status.or(net.status);
        row.error = row.error.take().or(net.error);
    }

    let mut rows: Vec<(String, Row)> = rows.into_iter().collect();
    rows.sort_by_key(|(_, row)| std::cmp::Reverse(row.last_ms.saturating_sub(row.first_ms)));
    rows.truncate(20);

    if json {
        let value: Vec<Value> = rows
            .into_iter()
            .map(|(request_id, row)| {
                json!({
                    "requestId": request_id,
                    "durationMs": row.last_ms.saturating_sub(row.first_ms),
                    "events": row.count,
                    "method": row.method,
                    "url": row.url,
                    "status": row.status,
                    "error": row.error,
                    "rawCommand": format!("kit cdp net show {request_id}"),
                })
            })
            .collect();
        return Reply::ok(serde_json::to_string_pretty(&value).unwrap_or_default());
    }

    if rows.is_empty() {
        return Reply::ok("no network requests".to_owned());
    }
    Reply::ok(
        rows.into_iter()
            .map(|(request_id, row)| {
                let duration = row.last_ms.saturating_sub(row.first_ms);
                let method = row.method.as_deref().unwrap_or("?");
                let status = row
                    .status
                    .map(|status| status.to_string())
                    .or(row.error)
                    .unwrap_or_else(|| "-".to_owned());
                let url = row.url.as_deref().unwrap_or("(unknown url)");
                format!(
                    "{duration:>5}ms {method:<6} {status:<18} {url}  req:{request_id} raw: kit cdp net show {request_id}"
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn net_rules_reply(state: &Shared, json: bool) -> Reply {
    let rules = state.lock().unwrap().net_rules.clone();
    if json {
        return Reply::ok(serde_json::to_string_pretty(&rules).unwrap_or_default());
    }
    if rules.is_empty() {
        return Reply::ok("no network rules".to_owned());
    }
    Reply::ok(
        rules
            .iter()
            .map(|rule| match rule {
                NetRule::Block { pattern } => format!("block {pattern}"),
                NetRule::Mock { method, pattern, status, mime, .. } => format!(
                    "mock {method} {pattern} {status} {}",
                    mime.as_deref().unwrap_or("application/json")
                ),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn write_bundle(
    dir: &PathBuf,
    events: &[TimelineEvent],
    settings: &LaunchSettings,
    rules: &[NetRule],
    launch: &Option<registry::LaunchRecord>,
    include: &[String],
    include_secrets: bool,
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let redacted_note = if include_secrets { "none" } else { "cookies/auth/storage/bodies" };
    std::fs::write(
        dir.join("summary.md"),
        format!(
            "# CDP Bundle\n\nEvents: {}\nSecrets redacted: {redacted_note}\nIncludes: {}\n",
            events.len(),
            if include.is_empty() { "default".to_owned() } else { include.join(",") }
        ),
    )?;
    let mut timeline = serde_json::to_value(events)?;
    if !include_secrets {
        redact_value(&mut timeline);
    }
    std::fs::write(dir.join("timeline.json"), serde_json::to_string_pretty(&timeline)?)?;
    let mut errors = events
        .iter()
        .filter(|event| event.track.is_error())
        .map(format::event_line)
        .collect::<Vec<_>>()
        .join("\n");
    if !include_secrets {
        errors = redact_text(&errors);
    }
    std::fs::write(dir.join("errors.txt"), errors)?;
    let network: Vec<&TimelineEvent> =
        events.iter().filter(|event| matches!(event.track, Track::Network(_))).collect();
    let mut network = serde_json::to_value(&network)?;
    if !include_secrets {
        redact_value(&mut network);
    }
    std::fs::write(
        dir.join("network.har"),
        serde_json::to_string_pretty(&json!({
            "log": {
                "version": "1.2",
                "creator": { "name": "kit cdp", "version": env!("CARGO_PKG_VERSION") },
                "entries": network,
            }
        }))?,
    )?;
    std::fs::create_dir_all(dir.join("screenshots"))?;
    std::fs::create_dir_all(dir.join("snapshots"))?;
    let mut environment = json!({
        "settings": settings,
        "netRules": rules,
        "launch": launch,
    });
    if !include_secrets {
        redact_value(&mut environment);
    }
    std::fs::write(dir.join("environment.json"), serde_json::to_string_pretty(&environment)?)?;
    std::fs::write(
        dir.join("redactions.json"),
        serde_json::to_string_pretty(&json!({
            "includeSecrets": include_secrets,
            "redacted": redacted_note,
        }))?,
    )?;
    Ok(())
}

fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if sensitive_key(key) {
                    *value = Value::String("[redacted]".to_owned());
                } else {
                    redact_value(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_value(value);
            }
        }
        Value::String(text) => *text = redact_text(text),
        _ => {}
    }
}

fn redact_text(text: &str) -> String {
    if !text.contains("://") && !text.contains('?') {
        return text.to_owned();
    }
    text.split_whitespace().map(redact_url_token).collect::<Vec<_>>().join(" ")
}

fn redact_url_token(token: &str) -> String {
    let Some((base, query_and_fragment)) = token.split_once('?') else {
        return token.to_owned();
    };
    let (query, fragment) = query_and_fragment.split_once('#').unwrap_or((query_and_fragment, ""));
    let query = query
        .split('&')
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if sensitive_key(key) {
                format!("{key}=[redacted]")
            } else if value.is_empty() {
                key.to_owned()
            } else {
                format!("{key}={value}")
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    if fragment.is_empty() {
        format!("{base}?{query}")
    } else {
        format!("{base}?{query}#{fragment}")
    }
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    if [
        "body",
        "postdata",
        "post_data",
        "requestbody",
        "request_body",
        "responsebody",
        "response_body",
    ]
    .contains(&key.as_str())
    {
        return true;
    }
    [
        "authorization",
        "auth",
        "cookie",
        "credential",
        "password",
        "passwd",
        "secret",
        "token",
        "apikey",
        "api_key",
        "accesskey",
        "access_key",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

fn parse_viewport(viewport: &str) -> Option<(u64, u64)> {
    let (width, height) = viewport.split_once('x').or_else(|| viewport.split_once('X'))?;
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if name == key && !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    None
}

fn push_marker(state: &Shared, source: Source, text: &str) {
    let mut state = state.lock().unwrap();
    let at_ms = state.now_ms();
    state.emit(TimelineEvent {
        at_ms,
        source,
        target: "kit".to_owned(),
        track: Track::Log(LogEntry {
            level: "info".to_owned(),
            source: "kit".to_owned(),
            text: text.to_owned(),
            url: None,
            line: None,
        }),
    });
}

fn target_from_info(info: &Value) -> Target {
    Target {
        id: string_at(info, "targetId"),
        kind: cdp::TargetKind::parse(&string_at(info, "type")),
        title: string_at(info, "title"),
        url: string_at(info, "url"),
        ws_url: None,
    }
}

fn string_at(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn no_target() -> String {
    "no target matched (try `kit cdp targets` to list them)".to_owned()
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}
