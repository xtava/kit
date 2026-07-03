use serde_json::Value;

pub(super) fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let console_args_redacted = map.get("track").and_then(Value::as_str) == Some("console");
            if console_args_redacted {
                if let Some(Value::Array(args)) = map.get_mut("args") {
                    redact_console_args(args);
                }
            }
            let sensitive_named_value =
                map.get("name").and_then(Value::as_str).is_some_and(sensitive_key);
            for (key, value) in map {
                if console_args_redacted && key == "args" {
                    continue;
                }
                if sensitive_key(key) || (sensitive_named_value && key == "value") {
                    *value = Value::String("[redacted]".to_owned());
                } else {
                    redact_value(value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_value(value);
            }
        }
        Value::String(text) => *text = redact_text(text),
        _ => {}
    }
}

fn redact_console_args(args: &mut [Value]) {
    let mut redact_until_next_key = false;
    for arg in args {
        let Some(map) = arg.as_object_mut() else {
            redact_value(arg);
            continue;
        };
        let original_text = map.get("text").and_then(Value::as_str).map(str::to_owned);
        if let Some(ref text) = original_text {
            let (redacted, keep_redacting) = redact_console_arg_text(text, redact_until_next_key);
            redact_until_next_key = keep_redacting;
            for field in ["text", "value"] {
                if map.get(field).and_then(Value::as_str) == Some(text.as_str()) {
                    map.insert(field.to_owned(), Value::String(redacted.clone()));
                }
            }
        }
        for (key, value) in map {
            if original_text.is_some() && (key == "text" || key == "value") {
                continue;
            }
            redact_value(value);
        }
    }
}

fn redact_console_arg_text(text: &str, redacting: bool) -> (String, bool) {
    let mut redacting = redacting;
    if redacting && looks_like_key_token(text) {
        redacting = false;
    }
    if redacting {
        return (redact_value_token(text), true);
    }
    if !text.chars().any(char::is_whitespace) {
        if let Some(redacted) = redact_inline_sensitive_token(text) {
            return (redacted, false);
        }
    }
    if let Some(key) = sensitive_key_marker(text) {
        return (text.to_owned(), key.contains("auth") || key.contains("authorization"));
    }
    (redact_text(text), false)
}

pub(super) fn redact_text(text: &str) -> String {
    if !text.contains("://") && !text.contains('?') && !has_sensitive_text_pair(text) {
        return text.to_owned();
    }
    let tokens = text.split_whitespace().map(redact_url_token).collect::<Vec<_>>();
    redact_sensitive_text_tokens(&tokens)
}

fn has_sensitive_text_pair(text: &str) -> bool {
    text.split_whitespace().any(|token| {
        sensitive_key_marker(token).is_some() || redact_inline_sensitive_token(token).is_some()
    })
}

fn redact_sensitive_text_tokens(tokens: &[String]) -> String {
    let mut out = Vec::with_capacity(tokens.len());
    let mut redact_next = false;
    let mut redact_until_next_key = false;
    for token in tokens {
        if redact_until_next_key && looks_like_key_token(token) {
            redact_until_next_key = false;
        }
        if redact_until_next_key {
            out.push(redact_value_token(token));
            continue;
        }
        if redact_next {
            out.push(redact_value_token(token));
            redact_next = false;
            continue;
        }
        if let Some(redacted) = redact_inline_sensitive_token(token) {
            out.push(redacted);
            continue;
        }
        if let Some(key) = sensitive_key_marker(token) {
            out.push(token.clone());
            if key.contains("auth") || key.contains("authorization") {
                redact_until_next_key = true;
            } else {
                redact_next = true;
            }
            continue;
        }
        out.push(token.clone());
    }
    out.join(" ")
}

fn redact_inline_sensitive_token(token: &str) -> Option<String> {
    let (index, delimiter) = token
        .find('=')
        .map(|index| (index, '='))
        .into_iter()
        .chain(token.find(':').map(|index| (index, ':')))
        .min_by_key(|(index, _)| *index)?;
    let key = normalize_key_candidate(&token[..index]);
    if key.is_empty() || !sensitive_key(key) {
        return None;
    }
    let value = &token[index + delimiter.len_utf8()..];
    if value.is_empty() {
        return None;
    }
    Some(format!("{}{}{}", &token[..index], delimiter, redact_value_token(value)))
}

fn sensitive_key_marker(token: &str) -> Option<String> {
    let token = token.trim_end_matches(['"', '\'']);
    let key = token.strip_suffix(':')?;
    let key = normalize_key_candidate(key);
    (!key.is_empty() && sensitive_key(key)).then(|| key.to_ascii_lowercase())
}

fn looks_like_key_token(token: &str) -> bool {
    let token = token.trim_end_matches(['"', '\'']);
    token.ends_with(':') || token.contains('=')
}

fn normalize_key_candidate(key: &str) -> &str {
    key.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
}

fn redact_value_token(token: &str) -> String {
    let value_start = token.trim_start_matches(['"', '\'']);
    let prefix_len = token.len() - value_start.len();
    let value_end = value_start.trim_end_matches([',', ';', '}', ']', ')', '"', '\'']);
    let suffix_len = value_start.len() - value_end.len();
    let prefix = &token[..prefix_len];
    let suffix = &token[token.len() - suffix_len..];
    format!("{prefix}[redacted]{suffix}")
}

fn redact_url_token(token: &str) -> String {
    let Some((base, query_and_fragment)) = token.split_once('?') else {
        return token.to_owned();
    };
    let (query, fragment) = query_and_fragment.split_once('#').unwrap_or((query_and_fragment, ""));
    let query = query
        .split('&')
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if sensitive_key(key) {
                format!("{key}=[redacted]")
            } else if value.is_empty() {
                key.to_owned()
            } else {
                format!("{key}={value}")
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    if fragment.is_empty() {
        format!("{base}?{query}")
    } else {
        format!("{base}?{query}#{fragment}")
    }
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    if [
        "body",
        "postdata",
        "post_data",
        "requestbody",
        "request_body",
        "responsebody",
        "response_body",
    ]
    .contains(&key.as_str())
    {
        return true;
    }
    [
        "authorization",
        "auth",
        "cookie",
        "credential",
        "password",
        "passwd",
        "secret",
        "token",
        "apikey",
        "api_key",
        "accesskey",
        "access_key",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;

    #[test]
    fn redacts_cdp_preview_name_value_pairs() {
        let mut value = json!({
            "track": "console",
            "args": [{
                "preview": {
                    "properties": [
                        { "name": "token", "type": "string", "value": "secret-value" },
                        { "name": "ok", "type": "string", "value": "visible" }
                    ]
                },
                "snapshot": {
                    "token": "secret-value",
                    "ok": "visible"
                }
            }]
        });

        redact_value(&mut value);

        assert_eq!(
            value["args"][0]["preview"]["properties"][0]["value"],
            Value::String("[redacted]".to_owned())
        );
        assert_eq!(value["args"][0]["preview"]["properties"][1]["value"], "visible");
        assert_eq!(value["args"][0]["snapshot"]["token"], Value::String("[redacted]".to_owned()));
        assert_eq!(value["args"][0]["snapshot"]["ok"], "visible");
    }

    #[test]
    fn redacts_sensitive_console_text_pairs() {
        assert_eq!(redact_text("plain   console\ntext"), "plain   console\ntext");
        assert_eq!(
            redact_text(
                "OBJ {token: secret-value, auth=abc123 password:\"pw\" ok: visible} \
                 Authorization: Bearer abc123 ok2: visible \
                 https://example.test/?token=abc&ok=1"
            ),
            "OBJ {token: [redacted], auth=[redacted] password:\"[redacted]\" ok: visible} \
             Authorization: [redacted] [redacted] ok2: visible \
             https://example.test/?token=[redacted]&ok=1"
        );
    }

    #[test]
    fn redacts_sensitive_values_across_console_args() {
        let mut value = json!({
            "track": "console",
            "text": "OBJ-CAPTURE {token: secret-value} Authorization: Bearer abc123",
            "args": [
                { "text": "OBJ-CAPTURE", "value": "OBJ-CAPTURE", "type": "string" },
                { "text": "{token: secret-value}", "type": "object" },
                { "text": "Authorization:", "value": "Authorization:", "type": "string" },
                { "text": "Bearer", "value": "Bearer", "type": "string" },
                { "text": "abc123", "value": "abc123", "type": "string" }
            ]
        });

        redact_value(&mut value);

        assert_eq!(
            value["text"],
            "OBJ-CAPTURE {token: [redacted]} Authorization: [redacted] [redacted]"
        );
        assert_eq!(value["args"][0]["value"], "OBJ-CAPTURE");
        assert_eq!(value["args"][1]["text"], "{token: [redacted]}");
        assert_eq!(value["args"][2]["value"], "Authorization:");
        assert_eq!(value["args"][3]["text"], Value::String("[redacted]".to_owned()));
        assert_eq!(value["args"][3]["value"], Value::String("[redacted]".to_owned()));
        assert_eq!(value["args"][4]["text"], Value::String("[redacted]".to_owned()));
        assert_eq!(value["args"][4]["value"], Value::String("[redacted]".to_owned()));
    }
}
