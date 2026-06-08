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
}

impl TrackKind {
    pub const ALL: [TrackKind; 5] =
        [Self::Console, Self::Exception, Self::Log, Self::Network, Self::Ws];

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "console" => Some(Self::Console),
            "exception" | "exceptions" => Some(Self::Exception),
            "log" => Some(Self::Log),
            "network" | "net" => Some(Self::Network),
            "ws" | "websocket" => Some(Self::Ws),
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
        }
    }

    /// The CDP domain that must be enabled to receive this Track's events.
    pub fn domain(self) -> &'static str {
        match self {
            Self::Console | Self::Exception => "Runtime",
            Self::Log => "Log",
            Self::Network | Self::Ws => "Network",
        }
    }
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

const WS_PREVIEW_LEN: usize = 200;

impl Track {
    pub fn kind(&self) -> TrackKind {
        match self {
            Self::Console(_) => TrackKind::Console,
            Self::Exception(_) => TrackKind::Exception,
            Self::Log(_) => TrackKind::Log,
            Self::Network(_) => TrackKind::Network,
            Self::Ws(_) => TrackKind::Ws,
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
            _ => None,
        }
    }
}

/// A bounded, age-queryable ring of Timeline events.
pub struct Timeline {
    events: VecDeque<TimelineEvent>,
    cap: usize,
}

impl Timeline {
    pub fn new(cap: usize) -> Self {
        Self { events: VecDeque::new(), cap }
    }

    pub fn push(&mut self, event: TimelineEvent) {
        self.events.push_back(event);
        while self.events.len() > self.cap {
            self.events.pop_front();
        }
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
    args.iter()
        .map(|arg| match arg.get("value") {
            Some(Value::String(text)) => text.clone(),
            Some(other) if !other.is_null() => other.to_string(),
            _ => string_at(arg, "description").unwrap_or_default(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn exception_text(details: &Value) -> String {
    details
        .pointer("/exception/description")
        .and_then(as_string)
        .or_else(|| string_at(details, "text"))
        .unwrap_or_else(|| "uncaught exception".to_owned())
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

    /// Every Track variant must survive the subscription wire — `TimelineEvent` flattens `Track`,
    /// so a variant field that collides with `source`/`target`/`at_ms`/`track` breaks deserialize
    /// and silently empties the live timeline. (`LogEntry.source` once did exactly that.)
    #[test]
    fn every_track_variant_roundtrips_over_the_wire() {
        let cases = [
            Track::Console(ConsoleLine { level: "log".into(), text: "x".into(), url: None, line: None }),
            Track::Exception(ExceptionInfo { text: "boom".into(), url: None, line: None }),
            Track::Log(LogEntry { level: "info".into(), source: "network".into(), text: "x".into(), url: None, line: None }),
            Track::Network(NetEvent { phase: NetPhase::Response, request_id: "1".into(), method: None, url: Some("u".into()), status: Some(200), mime: None, error: None }),
            Track::Ws(WsFrame { dir: WsDir::Sent, opcode: Some(1), len: Some(8), preview: Some("hi".into()), url: None }),
        ];
        for track in cases {
            let event = TimelineEvent { at_ms: 1, source: Source::Renderer, target: "t".into(), track };
            let json = serde_json::to_string(&event).unwrap();
            serde_json::from_str::<TimelineEvent>(&json)
                .unwrap_or_else(|error| panic!("does not round-trip: {json}\n  {error}"));
        }
    }
}
