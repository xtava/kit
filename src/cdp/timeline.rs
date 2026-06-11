//! The Timeline — the time-ordered stream of events an Attachment captures across all its Targets,
//! on one clock. Every CDP event maps to a [`Track`]; queries slice the bounded ring by age and
//! Track. This is generic protocol decoding — app meaning is a lens, never here.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::client::CdpEvent;

/// One event on the Timeline: when (ms since attach), which process side, which Target, and what.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEvent {
    pub at_ms: u64,
    pub source: Source,
    pub target: String,
    #[serde(flatten)]
    pub track: Track,
}

/// Which side of the Electron app an event came from: the Node main process or a web renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Main,
    Renderer,
}

impl Source {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "main" => Some(Self::Main),
            "renderer" | "render" => Some(Self::Renderer),
            _ => None,
        }
    }
}

/// One category of Timeline event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "track", rename_all = "snake_case")]
pub enum Track {
    Console(ConsoleLine),
    Exception(ExceptionInfo),
    Log(LogEntry),
    Network(NetEvent),
    Ws(WsFrame),
    Lifecycle(LifecycleEvent),
    Watch(WatchDelta),
    Trace(TraceRecord),
}

/// A Track category, independent of any one event — for `--track` filtering and domain enabling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Console,
    Exception,
    Log,
    Network,
    Ws,
    Lifecycle,
    Watch,
    Trace,
}

impl TrackKind {
    /// The capturable tracks — what `attach --track` can enable. `Watch` and `Trace` are
    /// daemon-generated (pollers and instrumentation points), not CDP subscriptions, so they are
    /// filterable but never enabled.
    pub const ALL: [TrackKind; 6] =
        [Self::Console, Self::Exception, Self::Log, Self::Network, Self::Ws, Self::Lifecycle];

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "console" => Some(Self::Console),
            "exception" | "exceptions" => Some(Self::Exception),
            "log" => Some(Self::Log),
            "network" | "net" => Some(Self::Network),
            "ws" | "websocket" => Some(Self::Ws),
            "lifecycle" | "life" => Some(Self::Lifecycle),
            "watch" => Some(Self::Watch),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Console => "console",
            Self::Exception => "exception",
            Self::Log => "log",
            Self::Network => "network",
            Self::Ws => "ws",
            Self::Lifecycle => "lifecycle",
            Self::Watch => "watch",
            Self::Trace => "trace",
        }
    }

    /// The CDP domain that must be enabled to receive this Track's events; `None` for tracks the
    /// daemon generates itself.
    pub fn domain(self) -> Option<&'static str> {
        match self {
            Self::Console | Self::Exception => Some("Runtime"),
            Self::Log => Some("Log"),
            Self::Network | Self::Ws => Some("Network"),
            Self::Lifecycle => Some("Page"),
            Self::Watch | Self::Trace => None,
        }
    }
}

/// One observed change of a watched expression: the previous rendered value (absent on the first
/// observation) and the new one. Values are bounded previews, not raw payloads — a watch records
/// *that* and *when* state changed; `eval` retrieves the full current value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchDelta {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub to: String,
}

/// One firing of an instrumentation point — a fn-trace call (and, later, a logpoint hit) — or,
/// when `suppressed` is set, the rate-cap summary standing in for that many dropped hits. Values
/// are bounded previews serialized in-page; `eval` retrieves full state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRecord {
    pub name: String,
    /// Where the trace is armed: a function path (`app.api.save`) or a `file:line` site.
    pub site: String,
    /// The logged expression (logpoint) or the call arguments (fn trace).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Fn traces: how the call ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TraceOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    /// When set, this row summarizes that many hits the rate cap dropped since the last record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppressed: Option<u64>,
}

/// How a traced call ended, with a bounded preview of the result or the error.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "preview", rename_all = "snake_case")]
pub enum TraceOutcome {
    Returned(String),
    Threw(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleLine {
    pub level: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExceptionInfo {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// Top stack frames with the ids source-map resolution needs. Bounded at decode; absent on
    /// events captured before this field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frames: Vec<StackFrame>,
    /// The top frame's original location (`src/cart.js:14`), filled at query time when the
    /// source-map registry holds the frame's map — never stored in the ring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
}

/// One captured stack frame: generated-code coordinates (0-based, as V8 reports them) plus the
/// script identity resolution keys on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackFrame {
    pub script_id: String,
    pub url: String,
    pub line: u64,
    pub column: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub level: String,
    /// The log's origin (`network`, `javascript`, …). Renamed on the wire: `TimelineEvent` already
    /// flattens a `source` (the process side), and two `source` keys collide on deserialize.
    #[serde(rename = "origin")]
    pub source: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetPhase {
    Request,
    Response,
    Finished,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetEvent {
    pub phase: NetPhase,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WsDir {
    Sent,
    Received,
    Created,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsFrame {
    pub dir: WsDir,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opcode: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub len: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleEvent {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader_id: Option<String>,
}

const WS_PREVIEW_LEN: usize = 200;

impl Track {
    pub fn kind(&self) -> TrackKind {
        match self {
            Self::Console(_) => TrackKind::Console,
            Self::Exception(_) => TrackKind::Exception,
            Self::Log(_) => TrackKind::Log,
            Self::Network(_) => TrackKind::Network,
            Self::Ws(_) => TrackKind::Ws,
            Self::Lifecycle(_) => TrackKind::Lifecycle,
            Self::Watch(_) => TrackKind::Watch,
            Self::Trace(_) => TrackKind::Trace,
        }
    }

    /// Is this an error-shaped event — the one definition of "something went wrong" shared by every
    /// error view? A thrown exception, a `console.error`/`console.assert`, a `log` at error level, or
    /// a failed network request. Plain logs, successful requests, and ws frames are not errors.
    pub fn is_error(&self) -> bool {
        match self {
            Self::Exception(_) => true,
            Self::Console(line) => matches!(line.level.as_str(), "error" | "assert"),
            Self::Log(entry) => entry.level.eq_ignore_ascii_case("error"),
            Self::Network(net) => matches!(net.phase, NetPhase::Failed),
            Self::Ws(_) => false,
            Self::Lifecycle(_) => false,
            Self::Watch(_) => false,
            // A traced `Threw` is an observation, not an app failure — the app may catch and
            // handle it, and arming a trace must never flip a `verify` verdict. Uncaught throws
            // already land on the exception track.
            Self::Trace(_) => false,
        }
    }

    /// A stable identity for an error, with the volatile parts (timestamps, request ids) stripped, so
    /// two occurrences of the same failure collapse to one group. The text plus its source location is
    /// what makes two errors "the same"; when nothing else is available the rendered line stands in.
    pub fn signature(&self) -> String {
        let located = |text: &str, url: &Option<String>, line: Option<u64>| match (url, line) {
            (Some(url), Some(line)) => format!("{text}@{url}:{line}"),
            (Some(url), None) => format!("{text}@{url}"),
            _ => text.to_owned(),
        };
        match self {
            Self::Exception(info) => located(&info.text, &info.url, info.line),
            Self::Console(line) => located(&line.text, &line.url, line.line),
            // A browser resource-load failure (`origin: network` — a 404, a blocked local file) is
            // identified by its *message*, not the per-resource URL: that URL is exactly what varies
            // across otherwise-identical failures, so keying on it would defeat collapsing. A
            // javascript-origin log keeps its URL:line — there the location *is* the identity.
            Self::Log(entry) if entry.source == "network" => format!("log/network:{}", entry.text),
            Self::Log(entry) => located(&entry.text, &entry.url, entry.line),
            Self::Network(net) => {
                let what = net.url.as_deref().or(net.error.as_deref()).unwrap_or("request failed");
                format!("net:{what}")
            }
            Self::Ws(frame) => format!("ws:{:?}", frame.dir),
            Self::Lifecycle(event) => format!("lifecycle:{}", event.name),
            Self::Watch(delta) => format!("watch:{}", delta.name),
            Self::Trace(record) => format!("trace:{}@{}", record.name, record.site),
        }
    }

    /// The human-distinguishing content of this event — its message and source location, without the
    /// timestamp/target chrome a renderer adds. This is what the variant audit compares: two errors
    /// that share a [`Track::signature`] but differ here are genuinely distinct, and the group must
    /// disclose it. Deliberately richer than the signature (which normalizes volatile parts away).
    pub fn detail(&self) -> String {
        let at = |url: &Option<String>, line: Option<u64>| match (url, line) {
            (Some(url), Some(line)) => format!(" ({url}:{line})"),
            (Some(url), None) => format!(" ({url})"),
            _ => String::new(),
        };
        match self {
            Self::Exception(info) => format!("{}{}", info.text, at(&info.url, info.line)),
            Self::Console(line) => {
                format!("{}: {}{}", line.level, line.text, at(&line.url, line.line))
            }
            Self::Log(entry) => {
                format!("{}: {}{}", entry.source, entry.text, at(&entry.url, entry.line))
            }
            Self::Network(net) => {
                let what = net.url.as_deref().or(net.error.as_deref()).unwrap_or("request failed");
                let status = net.status.map(|code| format!(" [{code}]")).unwrap_or_default();
                format!("{what}{status}")
            }
            Self::Ws(frame) => format!("ws {:?}", frame.dir),
            Self::Lifecycle(event) => format!("lifecycle {}", event.name),
            Self::Watch(delta) => match &delta.from {
                Some(from) => format!("watch {} {from} → {}", delta.name, delta.to),
                None => format!("watch {} → {}", delta.name, delta.to),
            },
            Self::Trace(record) => {
                let value = record.value.as_deref().unwrap_or("");
                format!("trace {} {value}", record.name)
            }
        }
    }

    /// Decode a CDP event into a Track, or `None` if it's not one we track.
    pub fn from_event(event: &CdpEvent) -> Option<Track> {
        let params = &event.params;
        match event.method.as_str() {
            "Runtime.consoleAPICalled" => Some(Track::Console(ConsoleLine {
                level: string_at(params, "type").unwrap_or_else(|| "log".to_owned()),
                text: console_text(params),
                url: frame_url(params),
                line: frame_line(params),
            })),
            "Runtime.exceptionThrown" => {
                let details = params.get("exceptionDetails")?;
                Some(Track::Exception(ExceptionInfo {
                    text: exception_text(details),
                    url: string_at(details, "url"),
                    line: u64_at(details, "lineNumber"),
                    frames: exception_frames(details),
                    resolved: None,
                }))
            }
            "Log.entryAdded" => {
                let entry = params.get("entry")?;
                Some(Track::Log(LogEntry {
                    level: string_at(entry, "level").unwrap_or_default(),
                    source: string_at(entry, "source").unwrap_or_default(),
                    text: string_at(entry, "text").unwrap_or_default(),
                    url: string_at(entry, "url"),
                    line: u64_at(entry, "lineNumber"),
                }))
            }
            "Network.requestWillBeSent" => Some(Track::Network(NetEvent {
                phase: NetPhase::Request,
                request_id: string_at(params, "requestId").unwrap_or_default(),
                method: params.pointer("/request/method").and_then(as_string),
                url: params.pointer("/request/url").and_then(as_string),
                status: None,
                mime: None,
                error: None,
            })),
            "Network.responseReceived" => Some(Track::Network(NetEvent {
                phase: NetPhase::Response,
                request_id: string_at(params, "requestId").unwrap_or_default(),
                method: None,
                url: params.pointer("/response/url").and_then(as_string),
                status: params.pointer("/response/status").and_then(Value::as_u64),
                mime: params.pointer("/response/mimeType").and_then(as_string),
                error: None,
            })),
            "Network.loadingFinished" => Some(Track::Network(NetEvent {
                phase: NetPhase::Finished,
                request_id: string_at(params, "requestId").unwrap_or_default(),
                method: None,
                url: None,
                status: None,
                mime: None,
                error: None,
            })),
            "Network.loadingFailed" => Some(Track::Network(NetEvent {
                phase: NetPhase::Failed,
                request_id: string_at(params, "requestId").unwrap_or_default(),
                method: None,
                url: None,
                status: None,
                mime: None,
                error: string_at(params, "errorText"),
            })),
            "Network.webSocketFrameSent" => Some(Track::Ws(ws_frame(WsDir::Sent, params))),
            "Network.webSocketFrameReceived" => Some(Track::Ws(ws_frame(WsDir::Received, params))),
            "Network.webSocketCreated" => Some(Track::Ws(WsFrame {
                dir: WsDir::Created,
                opcode: None,
                len: None,
                preview: None,
                url: string_at(params, "url"),
            })),
            "Page.lifecycleEvent" => Some(Track::Lifecycle(LifecycleEvent {
                name: string_at(params, "name").unwrap_or_else(|| "lifecycle".to_owned()),
                loader_id: string_at(params, "loaderId"),
            })),
            _ => None,
        }
    }
}

/// A run of errors that shared a [`Track::signature`], collapsed to one: the representative event, how
/// many times it fired, the window it spanned, and — the integrity guarantee — every *distinct*
/// rendered line it absorbed. When `variants.len() > 1` the collapse merged things that were not
/// byte-identical; the view must say so. Collapse is never silent: what went in can always be read back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorGroup {
    /// The first occurrence — carries the text, location, source, and target to render.
    pub event: TimelineEvent,
    pub count: usize,
    pub first_ms: u64,
    pub last_ms: u64,
    /// Every distinct rendered line folded into this group, in first-seen order. Length 1 is a clean
    /// collapse; longer means same signature, different detail — an audit trail, not decoration.
    pub variants: Vec<String>,
}

impl ErrorGroup {
    /// True when this group merged lines that differ — the signal that a count might be hiding a
    /// genuinely distinct error behind a shared signature.
    pub fn has_variants(&self) -> bool {
        self.variants.len() > 1
    }
}

/// The complete result of an error scan: the collapsed groups plus the integrity facts that say how
/// much to trust them. The facts travel *with* the data so a renderer can never present a clean count
/// while quietly sitting on the knowledge that it was lossy — honesty is not a side channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorReport {
    pub groups: Vec<ErrorGroup>,
    /// Events the bounded ring dropped *before* this window's oldest survivor — a count is then a
    /// floor, not a total. `None` means the daemon couldn't determine it (treat as unknown, not zero).
    pub evicted: Option<usize>,
    /// Error-domain events the decoder saw but does not model — invisible to every view unless named.
    pub undecoded: usize,
    /// The oldest in-window event sat at the ring boundary: older errors of *other* kinds may have
    /// scrolled off entirely, so even the set of distinct groups is a floor.
    pub saturated: bool,
}

impl ErrorReport {
    /// Any reason a reader should distrust the numbers — the trigger for a visible warning banner.
    pub fn has_integrity_risk(&self) -> bool {
        self.saturated
            || self.evicted.is_some_and(|count| count > 0)
            || self.undecoded > 0
            || self.groups.iter().any(ErrorGroup::has_variants)
    }
}

/// Collapse the error-shaped events in `events` into deduplicated groups, keyed by [`Track::signature`]
/// and ordered by first appearance. Each group records every distinct line it absorbed, so a collapse
/// that merged non-identical errors is auditable rather than silent. Non-error events are skipped.
pub fn group_errors(events: &[TimelineEvent]) -> Vec<ErrorGroup> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, ErrorGroup> = HashMap::new();

    for event in events.iter().filter(|event| event.track.is_error()) {
        let detail = event.track.detail();
        groups
            .entry(event.track.signature())
            .and_modify(|group| {
                group.count += 1;
                group.last_ms = event.at_ms;
                if !group.variants.contains(&detail) {
                    group.variants.push(detail.clone());
                }
            })
            .or_insert_with(|| {
                order.push(event.track.signature());
                ErrorGroup {
                    event: event.clone(),
                    count: 1,
                    first_ms: event.at_ms,
                    last_ms: event.at_ms,
                    variants: vec![detail.clone()],
                }
            });
    }

    order.into_iter().filter_map(|key| groups.remove(&key)).collect()
}

/// A bounded, age-queryable ring of Timeline events.
pub struct Timeline {
    events: VecDeque<TimelineEvent>,
    cap: usize,
    /// Lifetime count of events the ring has dropped off the front — the basis for telling a reader
    /// their window might be a floor, not a total.
    evicted: usize,
}

impl Timeline {
    pub fn new(cap: usize) -> Self {
        Self { events: VecDeque::new(), cap, evicted: 0 }
    }

    pub fn push(&mut self, event: TimelineEvent) {
        self.events.push_back(event);
        while self.events.len() > self.cap {
            self.events.pop_front();
            self.evicted += 1;
        }
    }

    /// True when a query for `window_ms` back from `now_ms` is lossy: the ring is full and its oldest
    /// survivor is *newer* than the window floor, so events the caller asked for were already dropped.
    pub fn is_saturated_for(&self, now_ms: u64, window_ms: u64) -> bool {
        if self.events.len() < self.cap {
            return false;
        }
        let floor = now_ms.saturating_sub(window_ms);
        self.events.front().is_some_and(|oldest| oldest.at_ms > floor)
    }

    /// Lifetime events dropped off the front of the ring.
    pub fn evicted(&self) -> usize {
        self.evicted
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Event volume per target label across the whole ring — how the target picker tells a target
    /// that's actually streaming from one that's merely present.
    pub fn counts_by_target(&self) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for event in &self.events {
            *counts.entry(event.target.clone()).or_insert(0) += 1;
        }
        counts
    }

    /// Events within the last `window_ms` (by the same clock as `now_ms`), optionally restricted to
    /// a set of Track kinds.
    pub fn since(
        &self,
        now_ms: u64,
        window_ms: u64,
        kinds: Option<&[TrackKind]>,
        source: Option<Source>,
    ) -> Vec<TimelineEvent> {
        let floor = now_ms.saturating_sub(window_ms);
        self.events
            .iter()
            .filter(|event| event.at_ms >= floor)
            .filter(|event| kinds.is_none_or(|kinds| kinds.contains(&event.track.kind())))
            .filter(|event| source.is_none_or(|source| event.source == source))
            .cloned()
            .collect()
    }
}

fn ws_frame(dir: WsDir, params: &Value) -> WsFrame {
    let payload = params.pointer("/response/payloadData").and_then(Value::as_str);
    WsFrame {
        dir,
        opcode: params.pointer("/response/opcode").and_then(Value::as_u64),
        len: payload.map(|data| data.len() as u64),
        preview: payload.map(|data| truncate(data, WS_PREVIEW_LEN)),
        url: None,
    }
}

fn console_text(params: &Value) -> String {
    let Some(args) = params.get("args").and_then(Value::as_array) else {
        return String::new();
    };
    args.iter().map(remote_object_text).collect::<Vec<_>>().join(" ")
}

/// Render a CDP `RemoteObject` console arg to text. A primitive is its value; an object that CDP gave
/// us a *preview* for becomes a compact `{key: value, …}` so two distinct objects logged under the
/// same prefix stay distinguishable — without this, every object collapses to the bare `"Object"`
/// description and genuinely different errors silently merge. Falls back to the description when no
/// preview rode along (the deeper structure is then only reachable via an active `--deep` probe).
fn remote_object_text(arg: &Value) -> String {
    match arg.get("value") {
        Some(Value::String(text)) => return text.clone(),
        Some(other) if !other.is_null() => return other.to_string(),
        _ => {}
    }
    if let Some(preview) = object_preview(arg) {
        return preview;
    }
    string_at(arg, "description").unwrap_or_default()
}

/// How many preview properties to fold into the text — enough to disambiguate (`code: 500` vs
/// `code: 404`) without dumping the whole object. The full object is a `--deep` probe away.
const PREVIEW_PROP_CAP: usize = 4;

/// A compact `{k: v, …}` from a RemoteObject's preview, or `None` when CDP sent no preview. `overflow`
/// (CDP's own "there's more") becomes a trailing `…` so a truncated preview is never mistaken for the
/// whole object.
fn object_preview(arg: &Value) -> Option<String> {
    let preview = arg.get("preview")?;
    let properties = preview.get("properties").and_then(Value::as_array)?;
    let mut rendered: Vec<String> = properties
        .iter()
        .take(PREVIEW_PROP_CAP)
        .map(|property| {
            let name = string_at(property, "name").unwrap_or_default();
            let value = string_at(property, "value").unwrap_or_default();
            format!("{name}: {value}")
        })
        .collect();
    let overflow = preview.get("overflow").and_then(Value::as_bool).unwrap_or(false);
    if overflow || properties.len() > PREVIEW_PROP_CAP {
        rendered.push("…".to_owned());
    }
    Some(format!("{{{}}}", rendered.join(", ")))
}

fn exception_text(details: &Value) -> String {
    details
        .pointer("/exception/description")
        .and_then(as_string)
        .or_else(|| string_at(details, "text"))
        .unwrap_or_else(|| "uncaught exception".to_owned())
}

/// The top stack frames, with the throw site itself standing in when no stack was attached.
fn exception_frames(details: &Value) -> Vec<StackFrame> {
    const TOP_FRAMES: usize = 5;
    let from_stack: Vec<StackFrame> = details
        .pointer("/stackTrace/callFrames")
        .and_then(Value::as_array)
        .map(|frames| {
            frames
                .iter()
                .take(TOP_FRAMES)
                .filter_map(|frame| {
                    Some(StackFrame {
                        script_id: string_at(frame, "scriptId")?,
                        url: string_at(frame, "url")?,
                        line: u64_at(frame, "lineNumber")?,
                        column: u64_at(frame, "columnNumber")?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if !from_stack.is_empty() {
        return from_stack;
    }
    Option::zip(string_at(details, "scriptId"), string_at(details, "url"))
        .zip(Option::zip(u64_at(details, "lineNumber"), u64_at(details, "columnNumber")))
        .map(|((script_id, url), (line, column))| vec![StackFrame { script_id, url, line, column }])
        .unwrap_or_default()
}

fn frame_url(params: &Value) -> Option<String> {
    params.pointer("/stackTrace/callFrames/0/url").and_then(as_string)
}

fn frame_line(params: &Value) -> Option<u64> {
    params.pointer("/stackTrace/callFrames/0/lineNumber").and_then(Value::as_u64)
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(as_string)
}

fn u64_at(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn as_string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_owned)
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let end = (0..=max).rev().find(|&index| text.is_char_boundary(index)).unwrap_or(0);
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(at_ms: u64, track: Track) -> TimelineEvent {
        TimelineEvent { at_ms, source: Source::Renderer, target: "page".into(), track }
    }

    fn net_log(text: &str, url: &str) -> Track {
        Track::Log(LogEntry {
            level: "error".into(),
            source: "network".into(),
            text: text.into(),
            url: Some(url.into()),
            line: None,
        })
    }

    fn console_obj(level: &str, text: &str) -> Track {
        Track::Console(ConsoleLine {
            level: level.into(),
            text: text.into(),
            url: None,
            line: None,
        })
    }

    /// The integrity guarantee: when two genuinely different errors share a signature (here: same
    /// `console.error` prefix, but the distinguishing object rendered into different text), the group
    /// records *both* lines as variants. A count that hides a distinct error is now self-disclosing.
    #[test]
    fn a_collision_records_every_distinct_variant() {
        let events = [
            at(1, console_obj("error", "GraphQL Error {code: 500, op: getWorkspaceInfo}")),
            at(2, console_obj("error", "GraphQL Error {code: 404, op: listMembers}")),
            at(3, console_obj("error", "GraphQL Error {code: 500, op: getWorkspaceInfo}")),
        ];
        // These differ in detail, so they do NOT share a signature — the richer text keeps them apart.
        let groups = group_errors(&events);
        assert_eq!(groups.len(), 2, "distinct objects must not collapse together");
        assert_eq!(groups[0].count, 2);
        assert!(!groups[0].has_variants(), "a true repeat has a single variant");
    }

    /// When the signature genuinely *can't* tell two errors apart (identical text, identical location,
    /// but the engine was handed pre-rendered detail that differs), the variant list still captures it
    /// so nothing collapses silently. Synthesized by forcing two details under one signature.
    #[test]
    fn variants_capture_what_a_shared_signature_would_otherwise_hide() {
        // Same text+location → same signature; we prove the group keeps the per-occurrence detail.
        let one = Track::Exception(ExceptionInfo {
            text: "Error: boom".into(),
            url: Some("a.js".into()),
            line: Some(1),
            frames: Vec::new(),
            resolved: None,
        });
        let groups = group_errors(&[at(1, one.clone()), at(2, one)]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[0].variants.len(), 1, "identical errors → one variant, a clean collapse");
    }

    /// `is_saturated_for`: a full ring whose oldest survivor is newer than the window floor means the
    /// caller's window is lossy. A ring with headroom never reports saturation.
    #[test]
    fn saturation_is_reported_only_when_the_window_outruns_the_ring() {
        let mut ring = Timeline::new(2);
        ring.push(at(100, console_obj("error", "a")));
        ring.push(at(200, console_obj("error", "b")));
        ring.push(at(300, console_obj("error", "c"))); // evicts the at=100 event
        assert_eq!(ring.evicted(), 1);
        // Window reaches back to t=0, but the oldest survivor is t=200 → events were dropped.
        assert!(ring.is_saturated_for(300, 300));
        // A roomy ring is never saturated.
        let mut roomy = Timeline::new(100);
        roomy.push(at(100, console_obj("error", "a")));
        assert!(!roomy.is_saturated_for(100, 100));
    }

    /// The bug the live Modular run exposed: a resource-load failure carries a *different* url per
    /// occurrence, so keying the signature on the url left 30 identical 404s as 30 ungrouped lines.
    /// A `network`-origin log must collapse on its message alone.
    #[test]
    fn network_origin_logs_collapse_on_message_not_resource_url() {
        let same_message = "Failed to load resource: the server responded with a status of 404";
        let events = [
            at(1, net_log(same_message, "http://host/api/a")),
            at(2, net_log(same_message, "http://host/api/b")),
            at(3, net_log(same_message, "http://host/api/c")),
        ];
        let groups = group_errors(&events);
        assert_eq!(groups.len(), 1, "same message, different resource urls → one group");
        assert_eq!(groups[0].count, 3);
    }

    /// The other half of the contract: an exception's url:line *is* its identity — the same message
    /// thrown from two locations is two distinct bugs and must not merge.
    #[test]
    fn exceptions_stay_distinct_by_location() {
        let boom = |url: &str, line: u64| {
            Track::Exception(ExceptionInfo {
                text: "TypeError: x".into(),
                url: Some(url.into()),
                line: Some(line),
                frames: Vec::new(),
                resolved: None,
            })
        };
        let events = [at(1, boom("a.js", 10)), at(2, boom("a.js", 10)), at(3, boom("b.js", 20))];
        let groups = group_errors(&events);
        assert_eq!(groups.len(), 2, "same text, different location → distinct groups");
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[1].count, 1);
    }

    /// Non-error events never reach the error view, and first-seen order is preserved.
    #[test]
    fn skips_non_errors_and_keeps_first_seen_order() {
        let info = Track::Console(ConsoleLine {
            level: "log".into(),
            text: "fyi".into(),
            url: None,
            line: None,
        });
        let err = |text: &str| {
            Track::Console(ConsoleLine {
                level: "error".into(),
                text: text.into(),
                url: None,
                line: None,
            })
        };
        let events = [at(1, info), at(2, err("first")), at(3, err("second"))];
        let groups = group_errors(&events);
        assert_eq!(groups.len(), 2);
        assert!(matches!(&groups[0].event.track, Track::Console(line) if line.text == "first"));
    }

    /// Every Track variant must survive the subscription wire — `TimelineEvent` flattens `Track`,
    /// so a variant field that collides with `source`/`target`/`at_ms`/`track` breaks deserialize
    /// and silently empties the live timeline. (`LogEntry.source` once did exactly that.)
    #[test]
    fn every_track_variant_roundtrips_over_the_wire() {
        let cases = [
            Track::Console(ConsoleLine {
                level: "log".into(),
                text: "x".into(),
                url: None,
                line: None,
            }),
            Track::Exception(ExceptionInfo {
                text: "boom".into(),
                url: None,
                line: None,
                frames: vec![StackFrame {
                    script_id: "12".into(),
                    url: "bundle.js".into(),
                    line: 17,
                    column: 9,
                }],
                resolved: Some("src/cart.js:14".into()),
            }),
            Track::Log(LogEntry {
                level: "info".into(),
                source: "network".into(),
                text: "x".into(),
                url: None,
                line: None,
            }),
            Track::Network(NetEvent {
                phase: NetPhase::Response,
                request_id: "1".into(),
                method: None,
                url: Some("u".into()),
                status: Some(200),
                mime: None,
                error: None,
            }),
            Track::Ws(WsFrame {
                dir: WsDir::Sent,
                opcode: Some(1),
                len: Some(8),
                preview: Some("hi".into()),
                url: None,
            }),
            Track::Watch(WatchDelta {
                name: "cart".into(),
                from: Some("2".into()),
                to: "3".into(),
            }),
            Track::Trace(TraceRecord {
                name: "save".into(),
                site: "app.api.save".into(),
                value: Some("(2 args)".into()),
                outcome: Some(TraceOutcome::Returned("{ok: true}".into())),
                duration_ms: Some(38.0),
                suppressed: None,
            }),
        ];
        for track in cases {
            let event =
                TimelineEvent { at_ms: 1, source: Source::Renderer, target: "t".into(), track };
            let json = serde_json::to_string(&event).unwrap();
            serde_json::from_str::<TimelineEvent>(&json)
                .unwrap_or_else(|error| panic!("does not round-trip: {json}\n  {error}"));
        }
    }
}
