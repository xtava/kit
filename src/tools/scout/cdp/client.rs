//! The per-target CDP probe: connect a websocket, read JS heap + DOM counters, disconnect.
//!
//! Cheap and non-pausing — `Runtime.evaluate` of `performance.memory` and `Memory.getDOMCounters`,
//! never a heap snapshot. `tokio-tungstenite` sends no `Origin` header, which is exactly what
//! Chromium's CDP socket requires (it rejects any request that carries one).

use std::time::Duration;

use anyhow::{bail, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const PROBE_TIMEOUT: Duration = Duration::from_secs(6);

pub struct TargetMetrics {
    pub js_heap_kib: Option<u64>,
    pub dom_nodes: Option<u64>,
    pub listeners: Option<u64>,
    pub documents: Option<u64>,
}

impl TargetMetrics {
    fn empty() -> Self {
        Self { js_heap_kib: None, dom_nodes: None, listeners: None, documents: None }
    }
}

/// Probe one target. A target that refuses the connection or a domain (workers lack DOM counters)
/// degrades to `None` fields rather than failing the survey.
pub async fn probe(ws_url: &str) -> TargetMetrics {
    match timeout(PROBE_TIMEOUT, probe_inner(ws_url)).await {
        Ok(Ok(metrics)) => metrics,
        _ => TargetMetrics::empty(),
    }
}

async fn probe_inner(ws_url: &str) -> Result<TargetMetrics> {
    let (mut ws, _) = connect_async(ws_url).await?;

    let heap = call(
        &mut ws,
        1,
        "Runtime.evaluate",
        json!({
            "expression": "(performance.memory ? performance.memory.usedJSHeapSize : 0)",
            "returnByValue": true
        }),
    )
    .await?;
    let js_heap_kib = heap
        .pointer("/result/result/value")
        .and_then(Value::as_u64)
        .map(|bytes| bytes / 1024);

    let (documents, dom_nodes, listeners) =
        match call(&mut ws, 2, "Memory.getDOMCounters", json!({})).await {
            Ok(dom) => (
                dom.pointer("/result/documents").and_then(Value::as_u64),
                dom.pointer("/result/nodes").and_then(Value::as_u64),
                dom.pointer("/result/jsEventListeners").and_then(Value::as_u64),
            ),
            Err(_) => (None, None, None),
        };

    let _ = ws.close(None).await;
    Ok(TargetMetrics { js_heap_kib, dom_nodes, listeners, documents })
}

async fn call(ws: &mut Ws, id: i64, method: &str, params: Value) -> Result<Value> {
    let request = json!({ "id": id, "method": method, "params": params });
    ws.send(Message::Text(request.to_string())).await?;

    while let Some(message) = ws.next().await {
        if let Message::Text(text) = message? {
            let value: Value = serde_json::from_str(text.as_str())?;
            if value.get("id").and_then(Value::as_i64) == Some(id) {
                if let Some(error) = value.get("error") {
                    bail!("cdp error: {error}");
                }
                return Ok(value);
            }
        }
    }
    bail!("websocket closed before response")
}
