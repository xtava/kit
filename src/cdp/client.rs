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
use serde::Serialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

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

        let connection = Self { outgoing: outgoing_tx, pending, next_id: Arc::new(AtomicU64::new(1)) };
        Ok((connection, event_rx))
    }

    /// Send a command and await its result. `session` targets a flatten-attached target.
    pub async fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
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

        match timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => bail!("cdp connection closed before response"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("cdp call '{method}' timed out")
            }
        }
    }
}

async fn write_loop(mut sink: SplitSink<Ws, Message>, mut outgoing: mpsc::UnboundedReceiver<Message>) {
    while let Some(message) = outgoing.recv().await {
        if sink.send(message).await.is_err() {
            break;
        }
    }
}

async fn read_loop(mut stream: SplitStream<Ws>, pending: Pending, events: mpsc::UnboundedSender<CdpEvent>) {
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
        documents: dom.as_ref().and_then(|value| value.pointer("/documents")).and_then(Value::as_u64),
        dom_nodes: dom.as_ref().and_then(|value| value.pointer("/nodes")).and_then(Value::as_u64),
        listeners: dom
            .as_ref()
            .and_then(|value| value.pointer("/jsEventListeners"))
            .and_then(Value::as_u64),
    }
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
