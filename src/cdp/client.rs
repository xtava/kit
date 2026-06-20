//! The CDP websocket connection: one socket, many in-flight calls routed by id, and an event
//! stream. A browser-level connection drives every target by `sessionId` (flatten auto-attach);
//! a per-target connection calls with `session = None`. Cheap to clone — clones share the socket.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use super::base64;

const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(6);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

/// One CDP event: a `method` notification, tagged with the `session` (target) it came from in
/// flatten mode (`None` on a per-target or browser-level connection).
#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub session: Option<String>,
    pub params: Value,
}

/// A live CDP websocket connection. Clones share the socket; the event stream ends when it closes.
#[derive(Clone)]
pub struct CdpConnection {
    outgoing: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

impl CdpConnection {
    /// Connect to a DevTools websocket. Returns the connection and the event stream.
    pub async fn connect(ws_url: &str) -> Result<(Self, mpsc::UnboundedReceiver<CdpEvent>)> {
        let (ws, _) = connect_async(ws_url).await.context("cdp websocket connect")?;
        let (sink, stream) = ws.split();

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (outgoing_tx, outgoing_rx) = mpsc::unbounded_channel::<Message>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<CdpEvent>();

        tokio::spawn(write_loop(sink, outgoing_rx));
        tokio::spawn(read_loop(stream, Arc::clone(&pending), event_tx));

        let connection =
            Self { outgoing: outgoing_tx, pending, next_id: Arc::new(AtomicU64::new(1)) };
        Ok((connection, event_rx))
    }

    /// Send a command and await its result. `session` targets a flatten-attached target.
    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        self.call_within(session, method, params, CALL_TIMEOUT).await
    }

    /// Send a command and await its result, bounded by `deadline` instead of the default. A call
    /// that overruns yields a typed [`CallTimeout`] (not a generic error, so callers can tell a
    /// no-response from a protocol failure) and stops tracking its id, so a reply that lands after
    /// the deadline is discarded rather than leaking a pending slot.
    pub async fn call_within(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        deadline: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut request = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            request["sessionId"] = Value::from(session);
        }

        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        if self.outgoing.send(Message::Text(request.to_string())).is_err() {
            self.pending.lock().unwrap().remove(&id);
            bail!("cdp connection closed");
        }

        match timeout(deadline, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => bail!("cdp connection closed before response"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(CallTimeout { method: method.to_owned(), after: deadline.as_secs_f32() }.into())
            }
        }
    }
}

/// A CDP call that did not return within its deadline — a distinct error from a protocol failure so
/// a caller can recognise a non-responding target rather than string-matching a message.
#[derive(Debug, thiserror::Error)]
#[error("cdp call '{method}' timed out after {after:.1}s")]
pub struct CallTimeout {
    pub method: String,
    pub after: f32,
}

/// `Page.captureScreenshot` returned no frame before its budget elapsed: the target is painting
/// nothing — a background page, a minimized or occluded window, or a renderer mid-reload. A distinct
/// error so a tight capture loop fails fast (and the shell can suggest a remedy) instead of blocking
/// on the generic call timeout.
#[derive(Debug, thiserror::Error)]
#[error("no frame within {budget:.1}s — the target produced no frame (a background page, a minimized or occluded window, or a renderer mid-reload)")]
pub struct NoFrame {
    pub budget: f32,
}

async fn write_loop(
    mut sink: SplitSink<Ws, Message>,
    mut outgoing: mpsc::UnboundedReceiver<Message>,
) {
    while let Some(message) = outgoing.recv().await {
        if sink.send(message).await.is_err() {
            break;
        }
    }
}

async fn read_loop(
    mut stream: SplitStream<Ws>,
    pending: Pending,
    events: mpsc::UnboundedSender<CdpEvent>,
) {
    while let Some(Ok(message)) = stream.next().await {
        let Message::Text(text) = message else { continue };
        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else { continue };

        if let Some(id) = value.get("id").and_then(Value::as_u64) {
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let _ = tx.send(response_result(&value));
            }
        } else if let Some(method) = value.get("method").and_then(Value::as_str) {
            let event = CdpEvent {
                method: method.to_owned(),
                session: value.get("sessionId").and_then(Value::as_str).map(str::to_owned),
                params: value.get("params").cloned().unwrap_or(Value::Null),
            };
            if events.send(event).is_err() {
                break;
            }
        }
    }
    // Socket closed: fail every in-flight call so awaiters don't hang.
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err(anyhow!("cdp connection closed")));
    }
}

fn response_result(value: &Value) -> Result<Value> {
    if let Some(error) = value.get("error") {
        bail!("cdp error: {error}");
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

/// JS heap + DOM counters for one target. Cheap and non-pausing — `performance.memory` and
/// `Memory.getDOMCounters`, never a heap snapshot. Absent fields degrade to `None`.
#[derive(Debug, Clone, Serialize)]
pub struct TargetMetrics {
    pub js_heap_kib: Option<u64>,
    pub dom_nodes: Option<u64>,
    pub listeners: Option<u64>,
    pub documents: Option<u64>,
}

impl TargetMetrics {
    pub fn empty() -> Self {
        Self { js_heap_kib: None, dom_nodes: None, listeners: None, documents: None }
    }
}

/// Probe one target over an existing connection (browser-level: pass its `session`).
pub async fn probe_metrics(connection: &CdpConnection, session: Option<&str>) -> TargetMetrics {
    let heap = connection
        .call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "(performance.memory ? performance.memory.usedJSHeapSize : 0)",
                "returnByValue": true
            }),
        )
        .await
        .ok();
    let js_heap_kib = heap
        .as_ref()
        .and_then(|value| value.pointer("/result/value"))
        .and_then(Value::as_u64)
        .map(|bytes| bytes / 1024);

    let dom = connection.call(session, "Memory.getDOMCounters", json!({})).await.ok();
    TargetMetrics {
        js_heap_kib,
        documents: dom
            .as_ref()
            .and_then(|value| value.pointer("/documents"))
            .and_then(Value::as_u64),
        dom_nodes: dom.as_ref().and_then(|value| value.pointer("/nodes")).and_then(Value::as_u64),
        listeners: dom
            .as_ref()
            .and_then(|value| value.pointer("/jsEventListeners"))
            .and_then(Value::as_u64),
    }
}

/// Screenshot encoding — the CDP `format` parameter, and the file extension to save under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Png,
    Jpeg,
    Webp,
}

impl ImageFormat {
    /// Parse a user-facing name; `jpg` is accepted for `jpeg`.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text.to_ascii_lowercase().as_str() {
            "png" => Ok(Self::Png),
            "jpeg" | "jpg" => Ok(Self::Jpeg),
            "webp" => Ok(Self::Webp),
            other => Err(format!("unknown image format '{other}' — png, jpeg, or webp")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
        }
    }
}

/// Capture a screenshot of one target via `Page.captureScreenshot`, decoded to image bytes.
/// `quality` applies to the lossy formats only; `full_page` captures beyond the viewport.
///
/// The call returns the instant the compositor produces a frame, so `budget` is the only thing
/// between a tight capture loop and a target that never paints: on overrun it fails fast with a
/// typed [`NoFrame`] rather than blocking on the generic call timeout.
pub async fn capture_screenshot(
    connection: &CdpConnection,
    session: Option<&str>,
    format: ImageFormat,
    quality: Option<u8>,
    full_page: bool,
    budget: Duration,
) -> Result<Vec<u8>> {
    let mut params = json!({ "format": format.as_str(), "captureBeyondViewport": full_page });
    if let Some(quality) = quality {
        params["quality"] = Value::from(quality);
    }
    let reply = connection
        .call_within(session, "Page.captureScreenshot", params, budget)
        .await
        .map_err(|error| match error.downcast::<CallTimeout>() {
            Ok(_) => NoFrame { budget: budget.as_secs_f32() }.into(),
            Err(error) => error,
        })?;
    let encoded =
        reply.get("data").and_then(Value::as_str).context("screenshot reply carried no image")?;
    base64::decode(encoded).map_err(anyhow::Error::msg).context("decode screenshot")
}

/// Probe a target by its own websocket — opens a connection, probes, drops it. A connection
/// failure degrades to empty metrics rather than failing the caller.
pub async fn probe_target(ws_url: &str) -> TargetMetrics {
    let probe = async {
        match CdpConnection::connect(ws_url).await {
            Ok((connection, _events)) => probe_metrics(&connection, None).await,
            Err(_) => TargetMetrics::empty(),
        }
    };
    timeout(PROBE_TIMEOUT, probe).await.unwrap_or_else(|_| TargetMetrics::empty())
}
