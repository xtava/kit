use std::collections::HashMap;
use std::time::Duration;

use futures_util::future::join_all;
use serde_json::{json, Value};
use tokio::time::timeout;

use crate::cdp::{CdpConnection, ConsoleLine, Track};

const OBJECT_CAPTURE_TIMEOUT_MS: u64 = 250;
const OBJECT_ARG_CAP: usize = 8;
const OBJECT_CAPTURE_WINDOW_MS: u64 = 1_000;
const OBJECT_CAPTURE_RATE_PER_PAGE: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CapturePlan {
    allowed: usize,
    within_event_cap: usize,
}

#[derive(Debug, Clone, Copy)]
struct CaptureWindow {
    started_at_ms: u64,
    used: usize,
}

/// Per-page object-snapshot policy. Preview/text capture is unaffected; only page-side
/// `Runtime.callFunctionOn` enrichment spends this budget.
#[derive(Default)]
pub(super) struct EnrichmentBudget {
    pages: HashMap<String, CaptureWindow>,
}

impl EnrichmentBudget {
    pub(super) fn plan(&mut self, page: &str, now_ms: u64, track: &Track) -> CapturePlan {
        let requested = object_request_count(track);
        let within_event_cap = requested.min(OBJECT_ARG_CAP);
        self.pages.retain(|_, window| {
            now_ms >= window.started_at_ms
                && now_ms.saturating_sub(window.started_at_ms) < OBJECT_CAPTURE_WINDOW_MS
        });
        let window = self
            .pages
            .entry(page.to_owned())
            .or_insert(CaptureWindow { started_at_ms: now_ms, used: 0 });
        if now_ms < window.started_at_ms
            || now_ms.saturating_sub(window.started_at_ms) >= OBJECT_CAPTURE_WINDOW_MS
        {
            *window = CaptureWindow { started_at_ms: now_ms, used: 0 };
        }
        let remaining = OBJECT_CAPTURE_RATE_PER_PAGE.saturating_sub(window.used);
        let allowed = within_event_cap.min(remaining);
        window.used = window.used.saturating_add(allowed);
        CapturePlan { allowed, within_event_cap }
    }
}

const OBJECT_SNAPSHOT_JS: &str = r#"function() {
  const MAX_DEPTH = 3;
  const MAX_PROPS = 40;
  const MAX_ARRAY = 40;
  const MAX_STRING = 4000;
  const seen = new WeakSet();

  function clip(text) {
    text = String(text);
    return text.length > MAX_STRING ? text.slice(0, MAX_STRING) + "...[truncated]" : text;
  }

  function primitive(value) {
    const kind = typeof value;
    if (value === null || kind === "number" || kind === "boolean" || kind === "string") {
      return kind === "string" ? clip(value) : value;
    }
    if (kind === "undefined") return "[undefined]";
    if (kind === "bigint") return value.toString() + "n";
    if (kind === "symbol") return clip(String(value));
    if (kind === "function") return "[Function " + (value.name || "anonymous") + "]";
    return null;
  }

  function readDataProperty(value, key, depth) {
    let descriptor;
    try {
      descriptor = Object.getOwnPropertyDescriptor(value, key);
    } catch (error) {
      return "[Property error: " + (error && error.message ? error.message : String(error)) + "]";
    }
    if (!descriptor) return "[empty]";
    if ("value" in descriptor) return snap(descriptor.value, depth + 1);
    return "[Accessor]";
  }

  function copyEnumerableProps(value, out, depth) {
    let keys;
    try {
      keys = Object.keys(value);
    } catch (error) {
      out.__kit_error__ = "keys: " + (error && error.message ? error.message : String(error));
      return out;
    }
    for (const key of keys.slice(0, MAX_PROPS)) {
      out[key] = readDataProperty(value, key, depth);
    }
    if (keys.length > MAX_PROPS) {
      out.__kit_truncated__ = (keys.length - MAX_PROPS) + " more keys";
    }
    return out;
  }

  function snap(value, depth) {
    const plain = primitive(value);
    if (plain !== null) return plain;
    if (seen.has(value)) return "[Circular]";
    if (depth >= MAX_DEPTH) return "[MaxDepth]";
    seen.add(value);

    if (value instanceof Date) return Number.isNaN(value.valueOf()) ? "[Invalid Date]" : value.toISOString();
    if (value instanceof RegExp) return String(value);
    if (value instanceof Error) {
      const out = {
        __type: value.name || "Error",
        message: clip(value.message || ""),
      };
      if (typeof value.stack === "string") out.stack = clip(value.stack);
      return copyEnumerableProps(value, out, depth);
    }
    if (Array.isArray(value)) {
      const out = [];
      const limit = Math.min(value.length, MAX_ARRAY);
      for (let i = 0; i < limit; i++) {
        out.push(readDataProperty(value, String(i), depth));
      }
      if (value.length > MAX_ARRAY) out.push("[... " + (value.length - MAX_ARRAY) + " more items]");
      return out;
    }
    if (value instanceof Map) {
      const entries = [];
      let count = 0;
      for (const entry of value.entries()) {
        if (count >= MAX_PROPS) break;
        entries.push([snap(entry[0], depth + 1), snap(entry[1], depth + 1)]);
        count++;
      }
      const out = { __type: "Map", size: value.size, entries };
      if (value.size > MAX_PROPS) out.__kit_truncated__ = (value.size - MAX_PROPS) + " more entries";
      return out;
    }
    if (value instanceof Set) {
      const values = [];
      let count = 0;
      for (const item of value.values()) {
        if (count >= MAX_PROPS) break;
        values.push(snap(item, depth + 1));
        count++;
      }
      const out = { __type: "Set", size: value.size, values };
      if (value.size > MAX_PROPS) out.__kit_truncated__ = (value.size - MAX_PROPS) + " more values";
      return out;
    }

    return copyEnumerableProps(value, {}, depth);
  }

  return snap(this, 0);
}"#;

pub async fn enrich(
    conn: &CdpConnection,
    session: Option<&str>,
    track: &mut Track,
    plan: CapturePlan,
) {
    let Track::Console(line) = track else {
        return;
    };

    let requests = planned_requests(line, plan);

    if requests.is_empty() {
        return;
    }

    let session = session.map(str::to_owned);
    let captures = requests
        .iter()
        .map(|(_, object_id)| {
            let conn = conn.clone();
            let session = session.clone();
            let object_id = object_id.clone();
            async move { capture_object_snapshot(&conn, session.as_deref(), &object_id).await }
        })
        .collect::<Vec<_>>();

    match timeout(Duration::from_millis(OBJECT_CAPTURE_TIMEOUT_MS), join_all(captures)).await {
        Ok(results) => {
            for ((index, _), result) in requests.into_iter().zip(results) {
                match result {
                    Ok(snapshot) => line.args[index].snapshot = Some(snapshot),
                    Err(error) => line.args[index].snapshot_error = Some(error),
                }
            }
        }
        Err(_) => {
            for (index, _) in requests {
                line.args[index].snapshot_error =
                    Some(format!("object capture timed out after {OBJECT_CAPTURE_TIMEOUT_MS}ms"));
            }
        }
    }
}

fn planned_requests(line: &mut ConsoleLine, plan: CapturePlan) -> Vec<(usize, String)> {
    let mut requests = Vec::new();
    let mut object_ordinal = 0;
    for (index, arg) in line.args.iter_mut().enumerate() {
        let Some(object_id) = arg.remote_object_id.take() else {
            continue;
        };
        if object_ordinal >= plan.within_event_cap {
            arg.snapshot_error = Some("object capture skipped: argument cap reached".to_owned());
        } else if object_ordinal >= plan.allowed {
            arg.snapshot_error = Some(format!(
                "object capture skipped: per-page rate cap reached ({OBJECT_CAPTURE_RATE_PER_PAGE}/s)"
            ));
        } else {
            requests.push((index, object_id));
        }
        object_ordinal += 1;
    }
    requests
}

fn object_request_count(track: &Track) -> usize {
    match track {
        Track::Console(line) => {
            line.args.iter().filter(|arg| arg.remote_object_id.is_some()).count()
        }
        _ => 0,
    }
}

async fn capture_object_snapshot(
    conn: &CdpConnection,
    session: Option<&str>,
    object_id: &str,
) -> Result<Value, String> {
    let result = conn
        .call_within(
            session,
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": OBJECT_SNAPSHOT_JS,
                "returnByValue": true,
                "silent": true,
                "awaitPromise": false,
            }),
            Duration::from_millis(OBJECT_CAPTURE_TIMEOUT_MS),
        )
        .await
        .map_err(|error| error.to_string())?;
    if let Some(details) = result.get("exceptionDetails") {
        return Err(super::exception_message(details));
    }
    let remote = result.get("result").ok_or_else(|| "missing Runtime result".to_owned())?;
    if let Some(value) = remote.get("value") {
        return Ok(value.clone());
    }
    if let Some(value) = remote.get("unserializableValue").and_then(Value::as_str) {
        return Ok(Value::String(value.to_owned()));
    }
    if let Some(description) = remote.get("description").and_then(Value::as_str) {
        return Ok(Value::String(description.to_owned()));
    }
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::ConsoleArg;

    fn console_with_objects(count: usize) -> Track {
        Track::Console(ConsoleLine {
            level: "log".to_owned(),
            text: "objects".to_owned(),
            args: (0..count)
                .map(|index| ConsoleArg {
                    kind: "object".to_owned(),
                    text: format!("object {index}"),
                    subtype: None,
                    value: None,
                    unserializable_value: None,
                    description: None,
                    preview: None,
                    snapshot: None,
                    snapshot_error: None,
                    remote_object_id: Some(format!("object-{index}")),
                })
                .collect(),
            url: None,
            line: None,
        })
    }

    #[test]
    fn budget_is_per_page_resets_each_second_and_preserves_normal_console_fidelity() {
        let mut budget = EnrichmentBudget::default();
        let one = console_with_objects(1);
        for at_ms in 0..OBJECT_CAPTURE_RATE_PER_PAGE as u64 {
            assert_eq!(budget.plan("page-a", at_ms, &one).allowed, 1);
        }
        assert_eq!(budget.plan("page-a", 20, &one).allowed, 0);
        assert_eq!(budget.plan("page-b", 20, &one).allowed, 1);
        assert_eq!(budget.plan("page-a", OBJECT_CAPTURE_WINDOW_MS, &one).allowed, 1);
    }

    #[test]
    fn skipped_objects_are_annotated_by_rate_and_argument_caps() {
        let mut track = console_with_objects(10);
        let Track::Console(line) = &mut track else { unreachable!() };
        let requests =
            planned_requests(line, CapturePlan { allowed: 2, within_event_cap: OBJECT_ARG_CAP });
        assert_eq!(requests.len(), 2);
        assert!(line.args[2].snapshot_error.as_deref().unwrap().contains("per-page rate cap"));
        assert!(line.args[8].snapshot_error.as_deref().unwrap().contains("argument cap"));
        assert!(line.args.iter().all(|arg| arg.remote_object_id.is_none()));
    }
}
