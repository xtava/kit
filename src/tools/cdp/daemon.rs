//! The Attachment daemon — the warm process behind every `kit cdp` command. It binds to the
//! Instance's browser endpoint, flatten-auto-attaches every Target, captures their events into one
//! Timeline, and answers client queries over a unix socket. It survives reloads (browser endpoint
//! is stable) and restarts (re-discovers by selector), and disposes itself cleanly (`docs/adr/0002`,
//! `docs/adr/0003`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::time::{interval, sleep};

use crate::cdp::{self, CdpConnection, CdpEvent, LogEntry, Target, Timeline, TimelineEvent, Track, TrackKind};

use super::protocol::{Command, Query, Reply};
use super::registry::{self, Record};
use super::{format, snapshot};

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
    /// Per-CDP-session AX refs (`e1` → backend DOM node id) from the last `snap`. Document-scoped:
    /// cleared when the session's Target navigates.
    refs: HashMap<String, HashMap<String, i64>>,
    timeline: Timeline,
    tracks: Vec<TrackKind>,
    start: Instant,
    last_activity: Instant,
}

impl State {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Resolve a Target selector to a `(sessionId, target)` against the live target set.
    fn resolve(&self, selector: Option<&str>) -> Option<(String, Target)> {
        let targets: Vec<Target> = self.sessions.values().cloned().collect();
        let chosen = cdp::select(&targets, selector)?;
        self.sessions
            .iter()
            .find(|(_, target)| target.id == chosen.id)
            .map(|(session, target)| (session.clone(), target.clone()))
    }

    fn label(&self, session: &Option<String>) -> String {
        session
            .as_ref()
            .and_then(|session| self.sessions.get(session))
            .map(target_label)
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

pub async fn serve(name: String, selector: String, port: u16, root_pid: u32, tracks: Vec<TrackKind>) -> Result<()> {
    let endpoint = cdp::browser_endpoint(port).await.context("instance is not a CDP endpoint")?;
    let (conn, events) = CdpConnection::connect(&endpoint.ws_url).await.context("connect browser endpoint")?;

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
        start: Instant::now(),
        last_activity: Instant::now(),
    }));

    setup_capture(&conn).await.context("enable target discovery")?;
    registry::write(&state.lock().unwrap().record())?;

    let socket = registry::socket_path(&name);
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("bind {}", socket.display()))?;

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

async fn event_pump(state: Shared, mut events: mpsc::UnboundedReceiver<CdpEvent>, tracks: Vec<TrackKind>, shutdown: mpsc::Sender<()>) {
    loop {
        while let Some(event) = events.recv().await {
            apply_event(&state, &event, &tracks).await;
        }
        // The browser socket closed (a full app restart). Try to re-find and re-bind the Instance.
        match reconnect(&state).await {
            Some(new_events) => {
                events = new_events;
                push_marker(&state, "reconnected to instance");
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
            state.lock().unwrap().sessions.insert(session.to_owned(), target);
            let conn = state.lock().unwrap().conn.clone();
            enable_session(&conn, session, tracks, cascade).await;
        }
        "Target.detachedFromTarget" => {
            if let Some(session) = event.params.get("sessionId").and_then(Value::as_str) {
                let mut state = state.lock().unwrap();
                state.sessions.remove(session);
                state.refs.remove(session);
            }
        }
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
                }
                if !navigated.is_empty() {
                    let at_ms = state.now_ms();
                    state.timeline.push(navigation_marker(at_ms, &updated.url));
                }
            }
        }
        _ => {
            if let Some(track) = Track::from_event(event) {
                let mut state = state.lock().unwrap();
                let at_ms = state.now_ms();
                let label = state.label(&event.session);
                state.timeline.push(TimelineEvent { at_ms, target: label, track });
            }
        }
    }
}

async fn enable_session(conn: &CdpConnection, session: &str, tracks: &[TrackKind], cascade: bool) {
    let mut domains: Vec<&str> = tracks.iter().map(|track| track.domain()).collect();
    domains.sort_unstable();
    domains.dedup();
    for domain in domains {
        let _ = conn.call(Some(session), &format!("{domain}.enable"), json!({})).await;
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

    let reply = match serde_json::from_str::<Query>(line.trim()) {
        Ok(query) => {
            state.lock().unwrap().last_activity = Instant::now();
            dispatch(&state, query.command, query.json, &shutdown).await
        }
        Err(error) => Reply::fail(format!("bad query: {error}")),
    };

    let mut payload = serde_json::to_string(&reply).unwrap_or_else(|_| String::from("{\"ok\":false,\"output\":\"encode error\"}"));
    payload.push('\n');
    let _ = reader.get_mut().write_all(payload.as_bytes()).await;
}

async fn dispatch(state: &Shared, command: Command, json: bool, shutdown: &mpsc::Sender<()>) -> Reply {
    match command {
        Command::Ping => Reply::ok("pong"),
        Command::Status => status_reply(state, json),
        Command::Targets => targets_reply(state, json),
        Command::Tail { since_ms, tracks } => tail_reply(state, since_ms, tracks, json),
        Command::Eval { target, expr } => {
            run_in_target(state, target, json, |conn, session| evaluate(conn, session, expr)).await
        }
        Command::Lens { target, source, args } => {
            let expr = wrap_lens(&source, &args);
            run_in_target(state, target, json, |conn, session| evaluate(conn, session, expr)).await
        }
        Command::Heap { target } => heap_reply(state, target, json).await,
        Command::Snap { target, interactive } => snap_reply(state, target, interactive, json).await,
        Command::Click { target, reference } => click_reply(state, target, reference).await,
        Command::Fill { target, reference, text } => fill_reply(state, target, reference, text).await,
        Command::Detach => {
            let _ = shutdown.send(()).await;
            Reply::ok("detached")
        }
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

fn tail_reply(state: &Shared, since_ms: u64, tracks: Option<Vec<TrackKind>>, json: bool) -> Reply {
    let state = state.lock().unwrap();
    let now = state.now_ms();
    let events = state.timeline.since(now, since_ms, tracks.as_deref());
    Reply::ok(format::events(&events, json))
}

async fn heap_reply(state: &Shared, target: Option<String>, json: bool) -> Reply {
    let resolved = {
        let state = state.lock().unwrap();
        state.resolve(target.as_deref()).map(|(session, target)| (state.conn.clone(), session, target))
    };
    let Some((conn, session, target)) = resolved else {
        return Reply::fail(no_target());
    };
    let metrics = cdp::probe_metrics(&conn, Some(&session)).await;
    Reply::ok(format::heap(&target_label(&target), &metrics, json))
}

async fn snap_reply(state: &Shared, target: Option<String>, interactive: bool, json: bool) -> Reply {
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
    {
        let mut state = state.lock().unwrap();
        let refs = snap.refs.iter().map(|entry| (entry.reference.clone(), entry.backend)).collect();
        state.refs.insert(session, refs);
    }

    if json {
        Reply::ok(serde_json::to_string_pretty(&snap).unwrap_or_default())
    } else {
        Reply::ok(format!("{}\n[{} refs]", snap.text, snap.refs.len()))
    }
}

async fn click_reply(state: &Shared, target: Option<String>, reference: String) -> Reply {
    let key = norm_ref(&reference);
    let Some((conn, session, backend)) = locate(state, target, &key) else {
        return Reply::fail(no_target());
    };
    let Some(backend) = backend else {
        return Reply::fail(format!("unknown ref '{reference}' — run `kit cdp snap` first"));
    };
    match click_at(&conn, &session, backend).await {
        Ok(()) => Reply::ok(format!("clicked @{key}")),
        Err(error) => Reply::fail(error),
    }
}

async fn fill_reply(state: &Shared, target: Option<String>, reference: String, text: String) -> Reply {
    let key = norm_ref(&reference);
    let Some((conn, session, backend)) = locate(state, target, &key) else {
        return Reply::fail(no_target());
    };
    let Some(backend) = backend else {
        return Reply::fail(format!("unknown ref '{reference}' — run `kit cdp snap` first"));
    };
    match fill_at(&conn, &session, backend, &text).await {
        Ok(()) => Reply::ok(format!("filled @{key}")),
        Err(error) => Reply::fail(error),
    }
}

/// Resolve `(conn, session, backend-node-for-ref)` for an interaction, without holding the lock.
fn locate(state: &Shared, target: Option<String>, ref_key: &str) -> Option<(CdpConnection, String, Option<i64>)> {
    let state = state.lock().unwrap();
    let (session, _) = state.resolve(target.as_deref())?;
    let backend = state.refs.get(&session).and_then(|refs| refs.get(ref_key).copied());
    Some((state.conn.clone(), session, backend))
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

async fn fill_at(conn: &CdpConnection, session: &str, backend: i64, text: &str) -> Result<(), String> {
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
    conn.call(Some(session), "Input.insertText", json!({ "text": text })).await.map_err(|error| error.to_string())?;
    call("function(){ this.dispatchEvent(new Event('input',{bubbles:true})); this.dispatchEvent(new Event('change',{bubbles:true})); }")
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn box_center(conn: &CdpConnection, session: &str, backend: i64) -> Result<(f64, f64), String> {
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

async fn resolve_object(conn: &CdpConnection, session: &str, backend: i64) -> Result<String, String> {
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

fn norm_ref(reference: &str) -> String {
    let trimmed = reference.trim().trim_start_matches('@');
    match trimmed.strip_prefix('e') {
        Some(rest) if !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()) => trimmed.to_owned(),
        _ => format!("e{trimmed}"),
    }
}

fn navigation_marker(at_ms: u64, url: &str) -> TimelineEvent {
    TimelineEvent {
        at_ms,
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
        let message = details
            .pointer("/exception/description")
            .and_then(Value::as_str)
            .or_else(|| details.get("text").and_then(Value::as_str))
            .unwrap_or("evaluation failed");
        return Err(message.to_owned());
    }
    Ok(result.pointer("/result/value").cloned().unwrap_or(Value::Null))
}

/// A lens runs as `(function(args){ <source> })(<args>)` — the script gets `args` and `return`s a
/// JSON-serializable value.
fn wrap_lens(source: &str, args: &[String]) -> String {
    let args_json = serde_json::to_string(args).unwrap_or_else(|_| "[]".to_owned());
    format!("(function(args){{ {source} }})({args_json})")
}

fn push_marker(state: &Shared, text: &str) {
    let mut state = state.lock().unwrap();
    let at_ms = state.now_ms();
    state.timeline.push(TimelineEvent {
        at_ms,
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

fn target_label(target: &Target) -> String {
    if target.title.is_empty() {
        target.url.clone()
    } else {
        target.title.clone()
    }
}

fn string_at(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn no_target() -> String {
    "no target matched (try `kit cdp targets` to list them)".to_owned()
}

fn now_unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_millis() as u64).unwrap_or(0)
}
