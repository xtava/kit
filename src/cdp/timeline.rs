//! The Timeline — the time-ordered stream of events an Attachment captures across all its Targets,
//! on one clock. Every CDP event maps to a [`Track`]; queries slice the bounded ring by age and
//! Track. This is generic protocol decoding — app meaning is a lens, never here.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::client::CdpEvent;

/// One event on the Timeline: when (ms since attach), which Target, and what.
#[derive(Debug, Clone, Serialize)]
pub struct TimelineEvent {
    pub at_ms: u64,
    pub target: String,
    #[serde(flatten)]
    pub track: Track,
}

/// One category of Timeline event.
#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub struct ConsoleLine {
    pub level: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExceptionInfo {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub level: String,
    pub source: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetPhase {
    Request,
    Response,
    Finished,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WsDir {
    Sent,
    Received,
    Created,
}

#[derive(Debug, Clone, Serialize)]
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

    /// Events within the last `window_ms` (by the same clock as `now_ms`), optionally restricted to
    /// a set of Track kinds.
    pub fn since(&self, now_ms: u64, window_ms: u64, kinds: Option<&[TrackKind]>) -> Vec<TimelineEvent> {
        let floor = now_ms.saturating_sub(window_ms);
        self.events
            .iter()
            .filter(|event| event.at_ms >= floor)
            .filter(|event| kinds.is_none_or(|kinds| kinds.contains(&event.track.kind())))
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
