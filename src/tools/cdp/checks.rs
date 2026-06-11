//! The pure half of `wait`/`expect`/`verify`: condition predicates over values the daemon already
//! holds. No I/O — the daemon shells run the polling loops and hand data in.

use serde_json::Value;

use crate::cdp::{NetEvent, NetPhase};

/// JS-style truthiness on an eval result that came back `returnByValue`. `undefined` and `NaN`
/// arrive as `null`; objects and arrays are always truthy.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// Compare an eval result against an expectation written on a command line: JSON when it parses
/// (`3`, `true`, `["a"]`), otherwise the raw text compared against the result's string form.
pub fn value_equals(actual: &Value, expected_raw: &str) -> bool {
    if let Ok(expected) = serde_json::from_str::<Value>(expected_raw) {
        return *actual == expected;
    }
    actual.as_str() == Some(expected_raw)
}

/// Render an eval result the way a human typed it: bare strings lose their quotes, everything
/// else is compact JSON.
pub fn render_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Whether a network event satisfies `pattern` (URL substring, case-insensitive) and `status`.
/// Only response/failed phases can match — a request that never answered satisfies nothing.
pub fn net_matches(event: &NetEvent, pattern: &str, status: Option<&str>) -> bool {
    let url_matches = event
        .url
        .as_deref()
        .is_some_and(|url| url.to_lowercase().contains(&pattern.to_lowercase()));
    if !url_matches {
        return false;
    }
    match event.phase {
        NetPhase::Response => status.is_none_or(|status| status_matches(event.status, status)),
        NetPhase::Failed => status.is_some_and(|status| status.eq_ignore_ascii_case("fail")),
        NetPhase::Request | NetPhase::Finished => false,
    }
}

/// Match a response status against the CLI vocabulary: an exact code (`404`), a class (`2xx`),
/// `ok` (< 400), or `fail` (>= 400).
fn status_matches(status: Option<u64>, want: &str) -> bool {
    let Some(status) = status else {
        return false;
    };
    let want = want.trim().to_lowercase();
    match want.as_str() {
        "ok" => status < 400,
        "fail" => status >= 400,
        _ => match want.strip_suffix("xx") {
            Some(class) => class.parse::<u64>().is_ok_and(|class| status / 100 == class),
            None => want.parse::<u64>().is_ok_and(|code| status == code),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthiness_follows_js() {
        assert!(!truthy(&Value::Null));
        assert!(!truthy(&serde_json::json!(false)));
        assert!(!truthy(&serde_json::json!(0)));
        assert!(!truthy(&serde_json::json!("")));
        assert!(truthy(&serde_json::json!("no")));
        assert!(truthy(&serde_json::json!([])));
        assert!(truthy(&serde_json::json!({})));
    }

    /// `--equals 3` must compare as a number, `--equals saved` as text — the command line carries
    /// no type information, so JSON-parseable input wins and raw text is the fallback.
    #[test]
    fn equals_prefers_json_then_falls_back_to_text() {
        assert!(value_equals(&serde_json::json!(3), "3"));
        assert!(!value_equals(&serde_json::json!("3"), "3"), "a JS string is not the number 3");
        assert!(value_equals(&serde_json::json!("saved"), "saved"));
        assert!(value_equals(&serde_json::json!(["a"]), "[\"a\"]"));
    }

    fn response(url: &str, status: u64) -> NetEvent {
        NetEvent {
            phase: NetPhase::Response,
            request_id: "r1".into(),
            method: None,
            url: Some(url.into()),
            status: Some(status),
            mime: None,
            error: None,
        }
    }

    #[test]
    fn net_matching_covers_classes_codes_and_failures() {
        let saved = response("http://x/api/save", 200);
        assert!(net_matches(&saved, "/API/Save", None));
        assert!(net_matches(&saved, "/api/save", Some("2xx")));
        assert!(net_matches(&saved, "/api/save", Some("200")));
        assert!(net_matches(&saved, "/api/save", Some("ok")));
        assert!(!net_matches(&saved, "/api/save", Some("fail")));
        assert!(!net_matches(&saved, "/api/other", None));

        let broken = response("http://x/api/save", 500);
        assert!(net_matches(&broken, "/api/save", Some("fail")));
        assert!(net_matches(&broken, "/api/save", Some("5xx")));
        assert!(!net_matches(&broken, "/api/save", Some("2xx")));

        let dead = NetEvent {
            phase: NetPhase::Failed,
            request_id: "r2".into(),
            method: None,
            url: Some("http://x/api/save".into()),
            status: None,
            mime: None,
            error: Some("net::ERR_CONNECTION_REFUSED".into()),
        };
        assert!(net_matches(&dead, "/api/save", Some("fail")), "a failed load is a failure");
        assert!(!net_matches(&dead, "/api/save", Some("2xx")));
    }

    /// A request that never answered satisfies nothing — `expect net` waits for *evidence*, and a
    /// pending request is not evidence.
    #[test]
    fn pending_requests_never_match() {
        let pending = NetEvent {
            phase: NetPhase::Request,
            request_id: "r3".into(),
            method: Some("POST".into()),
            url: Some("http://x/api/save".into()),
            status: None,
            mime: None,
            error: None,
        };
        assert!(!net_matches(&pending, "/api/save", None));
    }
}
