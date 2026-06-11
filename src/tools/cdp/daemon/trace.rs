//! The trace shell: arms fn wrappers and never-pausing logpoints in live targets, heals them
//! across reloads and script churn, and turns their binding payloads into Timeline rows. The
//! pure halves — the in-page JS templates and the payload decoder — live in the sibling `trace`
//! engine module; everything here is I/O around the daemon's `State`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::sleep;

use crate::cdp::{CdpConnection, CdpEvent, Source, TimelineEvent, TraceRecord, Track};

use super::super::protocol::{Reply, TraceOp};
use super::super::trace;
use super::sourcemaps::{self, short};
use super::{
    add_mark, evaluate, exception_message, no_target, push_marker, session_for, Shared, State,
};

/// How often the keeper heals armed traces, and the most traces one attachment may arm.
const KEEPER_TICK: Duration = Duration::from_secs(1);
const MAX_TRACES: usize = 32;

/// How many `trace find` hits are returned before asking for a narrower query, and the shortest
/// query worth searching a bundle for.
const FIND_HIT_CAP: usize = 20;
const FIND_MIN_QUERY: usize = 3;

/// One armed instrumentation point: where it lives, how it's capped, and what it has seen.
/// Healing belongs to the daemon-wide [`keeper`], not to per-trace tasks.
pub(super) struct TraceState {
    target: Option<String>,
    site: String,
    rate: u64,
    pub(super) hits: u64,
    suppressed: u64,
    /// Page-side binding-call failures — payloads that never reached the daemon. Nonzero means
    /// hits were lost in transport, and `trace ls` says so.
    emit_fails: u64,
    /// Timeline clock of the last recorded hit — what lets `ls` separate "armed, awaiting the
    /// first hit" from "has been firing".
    last_hit_ms: Option<u64>,
    /// Why the keeper currently cannot keep this trace armed, when it can't.
    stalled: Option<String>,
    kind: TraceKind,
}

#[derive(Clone)]
enum TraceKind {
    Fn {
        path: Vec<String>,
    },
    Logpoint {
        location: trace::PointLocation,
        condition: String,
        /// `None` only while the initial arm is in flight — the keeper must not adopt it yet.
        armed: Option<Armed>,
        /// A script re-parse may have moved the code under the breakpoint; the keeper re-arms
        /// and refreshes the readback.
        dirty: bool,
        rearms: u64,
    },
}

/// Where a logpoint actually lives right now — V8's answer, not an echo of the request.
#[derive(Clone)]
struct Armed {
    session: String,
    breakpoint: String,
    /// How many parsed scripts the breakpoint resolved into. Zero is honest: registered by URL,
    /// waiting for a matching script.
    sites: usize,
    bound: Option<BoundSite>,
    /// Original-source text of the requested line, when the map carried `sourcesContent` — the
    /// proof of what code the breakpoint is on.
    snippet: Option<String>,
}

/// The first location V8 resolved, 1-based for display.
#[derive(Clone)]
struct BoundSite {
    url: String,
    line: u32,
    column: u32,
}

impl BoundSite {
    fn display(&self) -> String {
        if self.column > 1 {
            format!("{}:{}:{}", short(&self.url), self.line, self.column)
        } else {
            format!("{}:{}", short(&self.url), self.line)
        }
    }

    /// Whether this is the site the user asked for — anything else gets an explicit `→` so the
    /// arm is a readback, not an echo.
    fn matches_request(&self, location: &trace::PointLocation) -> bool {
        self.line == location.line && short(&self.url) == short(&location.url)
    }
}

pub(super) async fn trace_reply(state: &Shared, op: TraceOp, json: bool) -> Reply {
    match op {
        TraceOp::Fn { name, target, path, rate } => {
            let segments = match trace::parse_fn_path(&path) {
                Ok(segments) => segments,
                Err(error) => return Reply::fail(error),
            };
            let name = name.unwrap_or_else(|| trace::default_fn_name(&path));
            let rate = rate.clamp(trace::RATE_FLOOR, trace::RATE_CEIL);
            let entry = TraceState {
                target: target.clone(),
                site: path.clone(),
                rate,
                hits: 0,
                suppressed: 0,
                emit_fails: 0,
                last_hit_ms: None,
                stalled: None,
                kind: TraceKind::Fn { path: segments.clone() },
            };
            if let Err(error) = reserve_trace(state, &name, entry) {
                return Reply::fail(error);
            }
            // Install synchronously so an arming failure lands in this reply, not a keeper log.
            let Some((conn, session)) = session_for(state, target.as_deref()) else {
                state.lock().unwrap().traces.remove(&name);
                return Reply::fail(format!("trace '{name}' not armed — {}", no_target()));
            };
            if let Err(error) = install_fn(state, &conn, &session, &name, &segments, rate).await {
                state.lock().unwrap().traces.remove(&name);
                return Reply::fail(format!(
                    "trace '{name}' not armed — {}",
                    redirect_module_scoped(&error)
                ));
            }
            // An rm that raced the install already ran its restore; wire ordering guarantees
            // this one lands after the install it undoes.
            if !state.lock().unwrap().traces.contains_key(&name) {
                let _ = evaluate(conn, session, trace::restore_fn_js(&name, &segments)).await;
                return Reply::fail(format!("trace '{name}' was removed while arming"));
            }
            add_mark(state, &format!("trace-{name}"), &format!("trace {name} armed"));
            if json {
                return Reply::ok(
                    json!({
                        "name": name,
                        "kind": "fn",
                        "site": path,
                        "rate": rate,
                        "readCommand": read_command(&name),
                    })
                    .to_string(),
                );
            }
            Reply::ok(format!(
                "tracing '{name}' — {path} wrapped, rate cap {rate}/s\n\
                 read      {}\n\
                 note      calls through saved references are not seen; thenables return as derived promises",
                read_command(&name)
            ))
        }
        TraceOp::Logpoint { name, target, location, expr, when, rate } => {
            let location = match trace::parse_location(&location) {
                Ok(location) => location,
                Err(error) => return Reply::fail(error),
            };
            let name = name.unwrap_or_else(|| trace::default_point_name(&location));
            let rate = rate.clamp(trace::RATE_FLOOR, trace::RATE_CEIL);
            let condition =
                trace::logpoint_condition(&name, expr.as_deref(), when.as_deref(), rate);
            let entry = TraceState {
                target: target.clone(),
                site: location.display(),
                rate,
                hits: 0,
                suppressed: 0,
                emit_fails: 0,
                last_hit_ms: None,
                stalled: None,
                kind: TraceKind::Logpoint {
                    location: location.clone(),
                    condition: condition.clone(),
                    armed: None,
                    dirty: false,
                    rearms: 0,
                },
            };
            if let Err(error) = reserve_trace(state, &name, entry) {
                return Reply::fail(error);
            }
            let armed = match arm_logpoint(
                state,
                &name,
                target.as_deref(),
                &location,
                &condition,
                expr.as_deref(),
                when.as_deref(),
            )
            .await
            {
                Ok(armed) => armed,
                Err(error) => {
                    state.lock().unwrap().traces.remove(&name);
                    return Reply::fail(format!("trace '{name}' not armed — {error}"));
                }
            };
            let kept = {
                let mut guard = state.lock().unwrap();
                match guard.traces.get_mut(&name) {
                    Some(entry) => {
                        if let TraceKind::Logpoint { armed: slot, .. } = &mut entry.kind {
                            *slot = Some(armed.clone());
                        }
                        true
                    }
                    None => false,
                }
            };
            // An rm raced the arm — retire the fresh breakpoint instead of leaking it.
            if !kept {
                let conn = state.lock().unwrap().conn.clone();
                let _ = conn
                    .call(
                        Some(&armed.session),
                        "Debugger.removeBreakpoint",
                        json!({ "breakpointId": armed.breakpoint }),
                    )
                    .await;
                return Reply::fail(format!("trace '{name}' was removed while arming"));
            }
            add_mark(state, &format!("trace-{name}"), &format!("trace {name} armed"));
            if json {
                return Reply::ok(
                    json!({
                        "name": name,
                        "kind": "logpoint",
                        "site": location.display(),
                        "bound": armed.bound.as_ref().map(bound_json),
                        "sites": armed.sites,
                        "snippet": armed.snippet,
                        "rate": rate,
                        "readCommand": read_command(&name),
                    })
                    .to_string(),
                );
            }
            let placement = match (&armed.bound, armed.sites) {
                (_, 0) => format!(
                    "{} — 0 sites; no parsed script matches yet (re-arms automatically; check: kit cdp trace ls)",
                    location.display()
                ),
                (Some(bound), sites) if !bound.matches_request(&location) => {
                    format!("{} → {} ({})", location.display(), bound.display(), sites_word(sites))
                }
                (_, sites) => format!("{} ({})", location.display(), sites_word(sites)),
            };
            let mut out = format!("logpoint '{name}' at {placement}, rate cap {rate}/s");
            if let Some(snippet) = &armed.snippet {
                out.push_str(&format!("\nline      {snippet}"));
            }
            out.push_str(&format!("\nread      {}", read_command(&name)));
            out.push_str(
                "\nnote      Debugger enabled — `debugger;` statements now pause and are auto-resumed unless DevTools is open",
            );
            Reply::ok(out)
        }
        TraceOp::Find { target, text } => find_reply(state, target, text, json).await,
        TraceOp::Ls => ls_reply(state, json),
        TraceOp::Rm { name } => {
            // Bind before matching — a scrutinee temporary would hold the lock across the await.
            let removed = state.lock().unwrap().traces.remove(&name);
            match removed {
                Some(entry) => {
                    let status = disarm_trace(state, &name, &entry).await;
                    Reply::ok(format!("trace '{name}' removed — {status}"))
                }
                None => Reply::fail(format!("no trace '{name}'")),
            }
        }
        TraceOp::Clear => {
            let removed: Vec<(String, TraceState)> = state.lock().unwrap().traces.drain().collect();
            for (name, entry) in &removed {
                let _ = disarm_trace(state, name, entry).await;
            }
            Reply::ok(format!("{} trace(s) removed", removed.len()))
        }
    }
}

fn read_command(name: &str) -> String {
    format!("kit cdp tail --track trace --since-mark trace-{name}")
}

fn sites_word(sites: usize) -> String {
    if sites == 1 {
        "1 site".to_owned()
    } else {
        format!("{sites} sites")
    }
}

fn bound_json(bound: &BoundSite) -> Value {
    json!({ "url": bound.url, "line": bound.line, "column": bound.column })
}

/// A `trace fn` path that isn't reachable is, in a bundled app, almost always a module-scoped
/// function — point the user at the tool that does reach those.
fn redirect_module_scoped(error: &str) -> String {
    if error.contains("not reachable") || error.contains("no property") {
        format!(
            "{error}\n\
             hint      module-scoped functions aren't on globalThis — a logpoint reaches them:\n\
             hint      `trace add <file:line> '([...arguments])'` · find the line: `trace find '<name>('`"
        )
    } else {
        error.to_owned()
    }
}

fn ls_reply(state: &Shared, json: bool) -> Reply {
    let guard = state.lock().unwrap();
    let now_ms = guard.now_ms();
    if json {
        let rows: Vec<Value> = guard
            .traces
            .iter()
            .map(|(name, entry)| {
                let (kind, sites, bound, rearms) = match &entry.kind {
                    TraceKind::Fn { .. } => ("fn", None, None, 0),
                    TraceKind::Logpoint { armed, rearms, .. } => (
                        "logpoint",
                        armed.as_ref().map(|armed| armed.sites),
                        armed.as_ref().and_then(|armed| armed.bound.as_ref()).map(bound_json),
                        *rearms,
                    ),
                };
                json!({
                    "name": name,
                    "kind": kind,
                    "site": entry.site,
                    "target": entry.target,
                    "rate": entry.rate,
                    "hits": entry.hits,
                    "suppressed": entry.suppressed,
                    "emitFailures": entry.emit_fails,
                    "lastHitMs": entry.last_hit_ms,
                    "stalled": entry.stalled,
                    "sites": sites,
                    "bound": bound,
                    "rearms": rearms,
                })
            })
            .collect();
        return Reply::ok(serde_json::to_string_pretty(&rows).unwrap_or_default());
    }
    if guard.traces.is_empty() {
        return Reply::ok(
            "no traces — arm one with `trace fn '<path>'` or `trace add <file:line>`".to_owned(),
        );
    }
    let mut rows: Vec<String> =
        guard.traces.iter().map(|(name, entry)| ls_row(now_ms, name, entry)).collect();
    rows.sort();
    Reply::ok(rows.join("\n"))
}

fn ls_row(now_ms: u64, name: &str, entry: &TraceState) -> String {
    let site = match &entry.kind {
        TraceKind::Fn { .. } => format!("fn {}", entry.site),
        TraceKind::Logpoint { location, armed, .. } => {
            let mut site = format!("log {}", entry.site);
            if let Some(bound) = armed.as_ref().and_then(|armed| armed.bound.as_ref()) {
                if !bound.matches_request(location) {
                    site.push_str(&format!(" → {}", bound.display()));
                }
            }
            site
        }
    };
    let mut bits = vec![site];
    if let Some(stalled) = &entry.stalled {
        bits.push(format!("⚠ stalled: {stalled}"));
    } else if entry.hits == 0 && entry.suppressed == 0 {
        match &entry.kind {
            TraceKind::Logpoint { armed: Some(armed), .. } if armed.sites == 0 => {
                bits.push("0 sites — no parsed script matches".to_owned());
            }
            TraceKind::Logpoint { armed: None, .. } => bits.push("arming".to_owned()),
            _ => bits.push("armed, awaiting first hit".to_owned()),
        }
    } else {
        let mut hits = format!("{} hit(s)", entry.hits);
        if let Some(last) = entry.last_hit_ms {
            hits.push_str(&format!(" · last {}", age(now_ms, last)));
        }
        bits.push(hits);
    }
    if entry.suppressed > 0 {
        bits.push(format!("{} suppressed", entry.suppressed));
    }
    if entry.emit_fails > 0 {
        bits.push(format!("⚠ {} lost in transport", entry.emit_fails));
    }
    if let TraceKind::Logpoint { rearms, .. } = &entry.kind {
        if *rearms > 0 {
            bits.push(format!("re-armed {rearms}×"));
        }
    }
    bits.push(format!("cap {}/s", entry.rate));
    format!("{name:<16} {}", bits.join(" · "))
}

fn age(now_ms: u64, then_ms: u64) -> String {
    let delta = now_ms.saturating_sub(then_ms);
    match delta {
        0..=999 => "just now".to_owned(),
        1_000..=59_999 => format!("{}s ago", delta / 1_000),
        60_000..=3_599_999 => format!("{}m ago", delta / 60_000),
        _ => format!("{}h ago", delta / 3_600_000),
    }
}

/// Search the resolved session's parsed scripts for a literal string — live coordinates for
/// `trace add`, from the code that is actually executing, so they can never be stale.
async fn find_reply(state: &Shared, target: Option<String>, text: String, json: bool) -> Reply {
    if text.chars().count() < FIND_MIN_QUERY {
        return Reply::fail(format!("query too short — at least {FIND_MIN_QUERY} characters"));
    }
    let Some((conn, session)) = session_for(state, target.as_deref()) else {
        return Reply::fail(no_target());
    };
    match ensure_debugger(state, &conn, &session).await {
        // The first enable replays scriptParsed for already-parsed scripts — let it land.
        Ok(true) => sleep(sourcemaps::SCRIPT_BACKLOG_BEAT).await,
        Ok(false) => {}
        Err(error) => return Reply::fail(error),
    }
    let mut scripts: Vec<(String, String)> = {
        let guard = state.lock().unwrap();
        guard
            .scripts
            .iter()
            .filter(|(_, record)| record.session == session)
            .map(|(id, record)| (id.clone(), record.url.clone()))
            .collect()
    };
    scripts.sort_by(|a, b| a.1.cmp(&b.1));
    let searched = scripts.len();

    let mut hits: Vec<(String, u64, String)> = Vec::new();
    let mut truncated = false;
    'scripts: for (script_id, url) in scripts {
        let result = conn
            .call(
                Some(&session),
                "Debugger.searchInContent",
                json!({ "scriptId": script_id, "query": text, "caseSensitive": true, "isRegex": false }),
            )
            .await;
        let Ok(result) = result else {
            continue;
        };
        for hit in result.get("result").and_then(Value::as_array).into_iter().flatten() {
            let line = hit.get("lineNumber").and_then(Value::as_u64).unwrap_or(0) + 1;
            let content = hit.get("lineContent").and_then(Value::as_str).unwrap_or("").trim();
            let content: String = content.chars().take(100).collect();
            hits.push((url.clone(), line, content));
            if hits.len() >= FIND_HIT_CAP {
                truncated = true;
                break 'scripts;
            }
        }
    }

    if json {
        let rows: Vec<Value> = hits
            .iter()
            .map(|(url, line, text)| json!({ "url": url, "line": line, "text": text }))
            .collect();
        return Reply::ok(
            json!({ "query": text, "scriptsSearched": searched, "truncated": truncated, "hits": rows })
                .to_string(),
        );
    }
    if hits.is_empty() {
        return Reply::ok(format!(
            "no matches for {text:?} in {searched} parsed script(s) — \
             the search is case-sensitive and literal"
        ));
    }
    let mut out: Vec<String> =
        hits.iter().map(|(url, line, text)| format!("{url}:{line}  {text}")).collect();
    if truncated {
        out.push(format!("… capped at {FIND_HIT_CAP} hits — narrow the query"));
    }
    out.push("arm       kit cdp trace add '<url:line>' '<expr>'".to_owned());
    Reply::ok(out.join("\n"))
}

/// Claim a trace name and the capacity slot, atomically — inserted before arming so a payload
/// arriving mid-install finds its entry.
fn reserve_trace(state: &Shared, name: &str, entry: TraceState) -> Result<(), String> {
    trace::validate_name(name)?;
    let mut guard = state.lock().unwrap();
    if guard.traces.contains_key(name) {
        return Err(format!("trace '{name}' already exists — `trace rm {name}` first"));
    }
    if guard.traces.len() >= MAX_TRACES {
        return Err(format!("{MAX_TRACES} traces armed — rm one first"));
    }
    guard.traces.insert(name.to_owned(), entry);
    Ok(())
}

/// Arm the binding transport and run the install script in one session. Trace transport is
/// independent of the attach-time track list (binding notifications only flow on Runtime-enabled
/// sessions), so arming enables Runtime explicitly — idempotent and cheap.
async fn install_fn(
    state: &Shared,
    conn: &CdpConnection,
    session: &str,
    name: &str,
    segments: &[String],
    rate: u64,
) -> Result<(), String> {
    ensure_trace_transport(state, conn, session).await?;
    let script = trace::install_fn_js(name, segments, rate);
    let status = evaluate(conn.clone(), session.to_owned(), script).await?;
    match status.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(()),
        _ => Err(status
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("install returned no status")
            .to_owned()),
    }
}

/// Compile-check the user's expressions, then arm the never-pausing breakpoint — directly or
/// through the source-map registry. Resolution may legitimately be zero sites (no matching
/// script parsed yet): that is reported, not failed — by-URL breakpoints arm automatically when
/// a matching script appears.
async fn arm_logpoint(
    state: &Shared,
    name: &str,
    target: Option<&str>,
    location: &trace::PointLocation,
    condition: &str,
    expr: Option<&str>,
    when: Option<&str>,
) -> Result<Armed, String> {
    let Some((conn, session)) = session_for(state, target) else {
        return Err(no_target());
    };
    ensure_trace_transport(state, &conn, &session).await?;
    // A previous trace with this name may have left counters in the page (the daemon restarted,
    // or rm couldn't reach the context) — a fresh arm starts from zero.
    let _ = evaluate(conn.clone(), session.clone(), trace::clear_state_js(name)).await;
    let debugger_was_cold = ensure_debugger(state, &conn, &session).await?;
    if let Some(expr) = expr {
        compile_check(&conn, &session, &format!("({expr})"), "expression").await?;
    }
    if let Some(when) = when {
        compile_check(&conn, &session, &format!("({when})"), "--when condition").await?;
    }
    // The pieces compiling is not enough: `1); (2` is two valid statements and a broken argument
    // list, and V8 treats an uncompilable condition as no-break — the silently-dead trace this
    // check exists to prevent. The assembled condition is a complete expression; compile exactly
    // what will be armed.
    compile_check(&conn, &session, condition, "assembled trace condition").await?;
    let mut armed = set_logpoint(state, &conn, &session, location, condition).await?;
    if armed.sites == 0 && debugger_was_cold {
        // The first enable replays scriptParsed for already-parsed scripts; the registry may
        // still be filling. One beat, one retry — then pending is the honest answer.
        sleep(sourcemaps::SCRIPT_BACKLOG_BEAT).await;
        let _ = conn
            .call(
                Some(&session),
                "Debugger.removeBreakpoint",
                json!({ "breakpointId": armed.breakpoint }),
            )
            .await;
        armed = set_logpoint(state, &conn, &session, location, condition).await?;
    }
    Ok(armed)
}

/// A user expression must be syntactically valid *before* it is spliced into the breakpoint
/// condition: V8 treats an uncompilable condition as no-break, which would leave the trace
/// silently dead forever — the worst failure for a tool whose job is "did my code run".
async fn compile_check(
    conn: &CdpConnection,
    session: &str,
    source: &str,
    what: &str,
) -> Result<(), String> {
    let result = conn
        .call(
            Some(session),
            "Runtime.compileScript",
            json!({ "expression": source, "sourceURL": "kit://trace", "persistScript": false }),
        )
        .await
        .map_err(|error| error.to_string())?;
    if let Some(details) = result.get("exceptionDetails") {
        return Err(format!("{what} does not compile: {}", exception_message(details)));
    }
    Ok(())
}

async fn ensure_trace_transport(
    state: &Shared,
    conn: &CdpConnection,
    session: &str,
) -> Result<(), String> {
    if state.lock().unwrap().trace_transport.contains(session) {
        return Ok(());
    }
    conn.call(Some(session), "Runtime.enable", json!({}))
        .await
        .map_err(|error| error.to_string())?;
    conn.call(Some(session), "Runtime.addBinding", json!({ "name": "__kit_trace__" }))
        .await
        .map_err(|error| error.to_string())?;
    state.lock().unwrap().trace_transport.insert(session.to_owned());
    Ok(())
}

/// Enable the Debugger domain for one session, lazily — it costs nothing until the first
/// logpoint, and enabling it is what makes `debugger;` statements pause (see
/// [`handle_debugger_pause`] for how that is kept harmless). Returns whether this call enabled
/// it: enabling replays `scriptParsed` for already-parsed scripts, and that backlog arrives
/// asynchronously — a caller about to consult the registry should give it a beat.
pub(super) async fn ensure_debugger(
    state: &Shared,
    conn: &CdpConnection,
    session: &str,
) -> Result<bool, String> {
    if state.lock().unwrap().debugger_enabled.contains(session) {
        return Ok(false);
    }
    conn.call(Some(session), "Debugger.enable", json!({}))
        .await
        .map_err(|error| error.to_string())?;
    state.lock().unwrap().debugger_enabled.insert(session.to_owned());
    Ok(true)
}

/// Mark armed logpoints whose ground may have shifted under a freshly-parsed script: a zero-site
/// logpoint re-checks against every new script (the one it names may have just arrived), and a
/// bound one re-arms when its script's URL stem re-parses — an HMR rebuild on the same URL, or a
/// dev server rotating `?t=` query strings. The keeper heals `dirty` on its next tick.
pub(super) fn note_script_parsed(state: &mut State, url: &str) {
    let parsed_stem = stem(url);
    for entry in state.traces.values_mut() {
        if let TraceKind::Logpoint { armed: Some(armed), dirty, .. } = &mut entry.kind {
            let moved = armed.bound.as_ref().is_some_and(|bound| stem(&bound.url) == parsed_stem);
            if armed.sites == 0 || moved {
                *dirty = true;
            }
        }
    }
}

fn stem(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// One `Debugger.setBreakpointByUrl` request, named: how the URL matches and where.
struct BreakpointSpec {
    url_key: &'static str,
    url: String,
    line: u32,
    column: Option<u32>,
}

/// Set the never-pausing breakpoint for a logpoint: directly when a parsed script matches the
/// URL, through the source-map registry when none does. The pending direct breakpoint is retired
/// on *both* registry outcomes — a mapped arm would double-fire it if a literally-named script
/// appeared, and a resolution error would leak it, wedging the location until the session dies.
async fn set_logpoint(
    state: &Shared,
    conn: &CdpConnection,
    session: &str,
    location: &trace::PointLocation,
    condition: &str,
) -> Result<Armed, String> {
    let spec = match trace::url_match(location) {
        trace::UrlMatch::Exact(url) => BreakpointSpec {
            url_key: "url",
            url,
            line: location.line - 1,
            column: location.column.map(|column| column.saturating_sub(1)),
        },
        trace::UrlMatch::Regex(regex) => BreakpointSpec {
            url_key: "urlRegex",
            url: regex,
            line: location.line - 1,
            column: location.column.map(|column| column.saturating_sub(1)),
        },
    };
    let direct = set_breakpoint(state, conn, session, location, spec, condition, None).await?;
    if direct.sites > 0 {
        return Ok(direct);
    }
    let retire = |breakpoint: String| async move {
        let _ = conn
            .call(Some(session), "Debugger.removeBreakpoint", json!({ "breakpointId": breakpoint }))
            .await;
    };
    let site = match sourcemaps::resolve_via_maps(state, session, location).await {
        Ok(site) => site,
        Err(error) => {
            retire(direct.breakpoint).await;
            return Err(error);
        }
    };
    let Some(site) = site else {
        return Ok(direct);
    };
    retire(direct.breakpoint).await;
    let spec = BreakpointSpec {
        url_key: "url",
        url: site.script_url,
        line: site.line,
        column: Some(site.column),
    };
    set_breakpoint(state, conn, session, location, spec, condition, site.snippet).await
}

async fn set_breakpoint(
    state: &Shared,
    conn: &CdpConnection,
    session: &str,
    location: &trace::PointLocation,
    spec: BreakpointSpec,
    condition: &str,
    snippet: Option<String>,
) -> Result<Armed, String> {
    let mut params = json!({ "lineNumber": spec.line, "condition": condition });
    params[spec.url_key] = spec.url.into();
    if let Some(column) = spec.column {
        params["columnNumber"] = column.into();
    }
    let result =
        conn.call(Some(session), "Debugger.setBreakpointByUrl", params).await.map_err(|error| {
            // V8 allows one breakpoint per location — name the trace that holds it.
            if error.to_string().contains("already exists") {
                let holder = state.lock().unwrap().traces.iter().find_map(|(name, entry)| {
                    match &entry.kind {
                        TraceKind::Logpoint { location: armed, .. }
                            if armed.url == location.url && armed.line == location.line =>
                        {
                            Some(name.clone())
                        }
                        _ => None,
                    }
                });
                return match holder {
                    Some(name) => {
                        format!("location already traced by '{name}' — `trace rm {name}` first")
                    }
                    None => "another debugger holds a breakpoint at this location".to_owned(),
                };
            }
            error.to_string()
        })?;
    let Some(breakpoint) = result.get("breakpointId").and_then(Value::as_str) else {
        return Err("breakpoint did not arm — no id returned".to_owned());
    };
    let locations = result.get("locations").and_then(Value::as_array).cloned().unwrap_or_default();
    let bound = locations.first().and_then(|resolved| {
        let line = resolved.get("lineNumber").and_then(Value::as_u64)? as u32;
        let column = resolved.get("columnNumber").and_then(Value::as_u64).unwrap_or(0) as u32;
        let url = resolved
            .get("scriptId")
            .and_then(Value::as_str)
            .and_then(|id| state.lock().unwrap().scripts.get(id).map(|record| record.url.clone()))
            .unwrap_or_else(|| location.url.clone());
        Some(BoundSite { url, line: line + 1, column: column + 1 })
    });
    Ok(Armed {
        session: session.to_owned(),
        breakpoint: breakpoint.to_owned(),
        sites: locations.len(),
        bound,
        snippet,
    })
}

/// The daemon's one keeper: every second, heal whatever a reload, target recreation, reconnect,
/// or script re-parse destroyed, and flush suppression counts a silent page would never report.
/// Pull-style like `watch_loop` — it re-reads the registry every tick, so no push-side
/// bookkeeping can go stale, and rm/clear need no task to abort.
pub(super) async fn keeper(state: Shared) {
    // Page-side emit-failure counters reset with their context; track the last reading per trace
    // and accumulate deltas, so a post-reload failure is never hidden behind an old high water.
    let mut last_emit_fails: HashMap<String, u64> = HashMap::new();
    loop {
        sleep(KEEPER_TICK).await;
        let plans: Vec<(String, Option<String>, u64, TraceKind)> = {
            let guard = state.lock().unwrap();
            last_emit_fails.retain(|name, _| guard.traces.contains_key(name));
            guard
                .traces
                .iter()
                .map(|(name, entry)| {
                    (name.clone(), entry.target.clone(), entry.rate, entry.kind.clone())
                })
                .collect()
        };
        if plans.is_empty() {
            continue;
        }

        // One batched probe per resolved session: liveness for fn wrappers, plus the suppression
        // counts and transport failures of every trace living there.
        let mut resolved: HashMap<String, (CdpConnection, String)> = HashMap::new();
        let mut by_session: HashMap<String, (CdpConnection, Vec<String>)> = HashMap::new();
        for (name, target, _, _) in &plans {
            let Some((conn, session)) = session_for(&state, target.as_deref()) else {
                continue;
            };
            by_session
                .entry(session.clone())
                .or_insert_with(|| (conn.clone(), Vec::new()))
                .1
                .push(name.clone());
            resolved.insert(name.clone(), (conn, session));
        }
        let mut installed: HashMap<String, bool> = HashMap::new();
        for (session, (conn, names)) in &by_session {
            if ensure_trace_transport(&state, conn, session).await.is_err() {
                continue;
            }
            let probe = trace::probe_js(names);
            let Ok(report) = evaluate(conn.clone(), session.clone(), probe).await else {
                continue;
            };
            for name in names {
                let Some(status) = report
                    .get(name)
                    .cloned()
                    .and_then(|value| serde_json::from_value::<trace::ProbeStatus>(value).ok())
                else {
                    continue;
                };
                installed.insert(name.clone(), status.installed);
                if status.flush > 0 {
                    emit_suppressed(&state, name, session, status.flush);
                }
                let last = last_emit_fails.get(name).copied().unwrap_or(0);
                let delta = if status.emit_fails >= last {
                    status.emit_fails - last
                } else {
                    status.emit_fails
                };
                last_emit_fails.insert(name.clone(), status.emit_fails);
                if delta > 0 {
                    if let Some(entry) = state.lock().unwrap().traces.get_mut(name) {
                        entry.emit_fails = entry.emit_fails.saturating_add(delta);
                    }
                }
            }
        }

        for (name, _, rate, kind) in plans {
            let Some((conn, session)) = resolved.get(&name).cloned() else {
                continue;
            };
            match kind {
                // A missing wrapper means the context was replaced — re-install. The 1s tick is
                // the bounded retry; calls landing before the next successful install are not
                // observed.
                TraceKind::Fn { path } => {
                    if installed.get(&name).copied().unwrap_or(false) {
                        set_stalled(&state, &name, None);
                        continue;
                    }
                    let outcome = install_fn(&state, &conn, &session, &name, &path, rate).await;
                    set_stalled(&state, &name, outcome.err());
                }
                // The initial arm is still in flight in trace_reply — not ours yet.
                TraceKind::Logpoint { armed: None, .. } => {}
                // By-URL breakpoints survive reloads within a session; what they don't survive
                // is the *session* changing or the script under them re-parsing (`dirty`).
                // Re-arm — through the registry when needed — and retire the old breakpoint.
                TraceKind::Logpoint { location, condition, armed: Some(armed), dirty, .. } => {
                    if !dirty && armed.session == session {
                        continue;
                    }
                    if ensure_debugger(&state, &conn, &session).await.is_err() {
                        continue;
                    }
                    let _ = conn
                        .call(
                            Some(&armed.session),
                            "Debugger.removeBreakpoint",
                            json!({ "breakpointId": armed.breakpoint }),
                        )
                        .await;
                    match set_logpoint(&state, &conn, &session, &location, &condition).await {
                        Ok(new_armed) => {
                            let drifted = dirty
                                && (new_armed
                                    .bound
                                    .as_ref()
                                    .map(|bound| (bound.line, bound.column)))
                                    != (armed
                                        .bound
                                        .as_ref()
                                        .map(|bound| (bound.line, bound.column)));
                            let mut guard = state.lock().unwrap();
                            if let Some(entry) = guard.traces.get_mut(&name) {
                                entry.stalled = None;
                                if let TraceKind::Logpoint { armed: slot, dirty, rearms, .. } =
                                    &mut entry.kind
                                {
                                    *slot = Some(new_armed);
                                    *dirty = false;
                                    *rearms += 1;
                                }
                            }
                            drop(guard);
                            if drifted {
                                push_marker(
                                    &state,
                                    Source::Renderer,
                                    &format!(
                                        "trace '{name}' re-armed after script re-parse — site moved; check `trace ls`"
                                    ),
                                );
                            }
                        }
                        Err(error) => set_stalled(&state, &name, Some(error)),
                    }
                }
            }
        }
    }
}

fn set_stalled(state: &Shared, name: &str, why: Option<String>) {
    if let Some(entry) = state.lock().unwrap().traces.get_mut(name) {
        entry.stalled = why;
    }
}

fn emit_suppressed(state: &Shared, name: &str, session: &str, count: u64) {
    let mut guard = state.lock().unwrap();
    let at_ms = guard.now_ms();
    let label = guard.label(&Some(session.to_owned()));
    let Some(entry) = guard.traces.get_mut(name) else {
        return;
    };
    entry.suppressed = entry.suppressed.saturating_add(count);
    let site = entry.site.clone();
    guard.emit(TimelineEvent {
        at_ms,
        source: Source::Renderer,
        target: label,
        track: Track::Trace(TraceRecord {
            name: name.to_owned(),
            site,
            value: None,
            outcome: None,
            duration_ms: None,
            suppressed: Some(count),
        }),
    });
}

/// Undo a trace's instrumentation, reporting exactly what happened. Fn traces: put the original
/// back — unless the app replaced the function since, in which case clobbering its newer code
/// would be worse than leaving the wrapper. Logpoints: remove the breakpoint and the in-page
/// state.
async fn disarm_trace(state: &Shared, name: &str, entry: &TraceState) -> String {
    match &entry.kind {
        TraceKind::Fn { path } => {
            let Some((conn, session)) = session_for(state, entry.target.as_deref()) else {
                return "restore skipped (no live target)".to_owned();
            };
            let script = trace::restore_fn_js(name, path);
            match evaluate(conn, session, script).await {
                Ok(status) => match status.get("status").and_then(Value::as_str) {
                    Some("restored") => "original restored".to_owned(),
                    Some("replaced") => {
                        "function was replaced after wrapping; left in place".to_owned()
                    }
                    Some("missing") => "wrapper not present (context reloaded)".to_owned(),
                    Some("blocked") => {
                        "restore blocked (object frozen) — wrapper left in place".to_owned()
                    }
                    _ => "restore returned no status".to_owned(),
                },
                Err(error) => format!("restore failed: {error}"),
            }
        }
        TraceKind::Logpoint { armed, .. } => {
            let Some(armed) = armed else {
                return "breakpoint was never armed".to_owned();
            };
            let conn = state.lock().unwrap().conn.clone();
            let removed = conn
                .call(
                    Some(&armed.session),
                    "Debugger.removeBreakpoint",
                    json!({ "breakpointId": armed.breakpoint }),
                )
                .await;
            let _ = evaluate(conn, armed.session.clone(), trace::clear_state_js(name)).await;
            match removed {
                Ok(_) => "breakpoint removed".to_owned(),
                Err(_) => "breakpoint already gone (session ended)".to_owned(),
            }
        }
    }
}

/// kit's breakpoint conditions always return false, so a pause that names a kit breakpoint is an
/// anomaly to recover from instantly. Other pauses — `debugger;` statements, a human's manual
/// pause — are theirs *if* a DevTools frontend is attached; with kit as the only debugger, an
/// unresumed pause hangs the very app kit promised never to touch, so it is resumed and said.
pub(super) async fn handle_debugger_pause(state: &Shared, event: &CdpEvent) {
    let Some(session) = event.session.as_deref() else {
        return;
    };
    let hit: Vec<&str> = event
        .params
        .get("hitBreakpoints")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let (conn, kit_ids) = {
        let guard = state.lock().unwrap();
        let ids: HashSet<String> = guard
            .traces
            .values()
            .filter_map(|entry| match &entry.kind {
                TraceKind::Logpoint { armed: Some(armed), .. } => Some(armed.breakpoint.clone()),
                _ => None,
            })
            .collect();
        (guard.conn.clone(), ids)
    };
    let ours = !hit.is_empty() && hit.iter().all(|id| kit_ids.contains(*id));
    if !ours && devtools_frontend_present(&conn).await {
        // The frontend check is browser-global (CDP doesn't say which session has one), so this
        // may be leaving a different window frozen — say so instead of staying silent.
        push_marker(
            state,
            Source::Renderer,
            "debugger pause left for DevTools — that target is frozen until resumed there",
        );
        return;
    }
    let _ = conn.call(Some(session), "Debugger.resume", json!({})).await;
    let what = if ours {
        "kit breakpoint paused (bug) — resumed"
    } else {
        "debugger pause auto-resumed — open DevTools to actually pause"
    };
    push_marker(state, Source::Renderer, what);
}

async fn devtools_frontend_present(conn: &CdpConnection) -> bool {
    let Ok(result) = conn.call(None, "Target.getTargets", json!({})).await else {
        return false;
    };
    result.pointer("/targetInfos").and_then(Value::as_array).is_some_and(|targets| {
        targets.iter().any(|target| {
            target
                .get("url")
                .and_then(Value::as_str)
                .is_some_and(|url| url.starts_with("devtools://"))
        })
    })
}

/// Decode one `__kit_trace__` binding payload into a Timeline row. The binding is callable by
/// page code, so undecodable or unknown-trace payloads are dropped — forged rows must not enter
/// the evidence stream.
pub(super) fn ingest_trace_payload(state: &Shared, event: &CdpEvent) {
    if event.params.get("name").and_then(Value::as_str) != Some("__kit_trace__") {
        return;
    }
    let Some(raw) = event.params.get("payload").and_then(Value::as_str) else {
        return;
    };
    let Some(payload) = trace::decode_payload(raw) else {
        return;
    };
    let mut guard = state.lock().unwrap();
    let at_ms = guard.now_ms();
    let label = guard.label(&event.session);
    let Some(entry) = guard.traces.get_mut(&payload.name) else {
        return;
    };
    match payload.suppressed {
        // The counts come from the page — saturate rather than trust them with an overflow.
        Some(count) => entry.suppressed = entry.suppressed.saturating_add(count),
        None => {
            entry.hits = entry.hits.saturating_add(1);
            entry.last_hit_ms = Some(at_ms);
        }
    }
    let site = entry.site.clone();
    guard.emit(TimelineEvent {
        at_ms,
        source: Source::Renderer,
        target: label,
        track: Track::Trace(TraceRecord {
            name: payload.name,
            site,
            value: payload.value,
            outcome: payload.outcome,
            duration_ms: payload.duration_ms,
            suppressed: payload.suppressed,
        }),
    });
}
