//! Daemon-side rendering: turn live engine data into the text or JSON a [`super::protocol::Reply`]
//! carries. Compact text is the default (cheap for an agent to read); `--json` gives full structure.

use serde::Serialize;
use serde_json::Value;

use crate::cdp::{NetPhase, Target, TargetMetrics, TimelineEvent, Track, WsDir};

fn pretty<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_owned())
}

/// An eval / lens result value.
pub fn value(result: &Value, json: bool) -> String {
    if json {
        return pretty(result);
    }
    match result {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_owned(),
        other => pretty(other),
    }
}

/// The Targets in an Instance, with the selector that addresses each. The top-ranked one is the
/// default ("main") that bare commands hit.
pub fn targets(targets: &[Target], json: bool) -> String {
    if json {
        return pretty(targets);
    }
    if targets.is_empty() {
        return "no inspectable targets".to_owned();
    }
    let mut ranked: Vec<&Target> = targets.iter().collect();
    ranked.sort_by_key(|target| std::cmp::Reverse(target.main_rank()));

    ranked
        .iter()
        .enumerate()
        .map(|(index, target)| {
            let marker = if index == 0 { "* " } else { "  " };
            let label = target_label(target);
            format!("{marker}{:<14} {label}", target.kind.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A Timeline slice. Suppression (the `ignore` list) is applied by the daemon before this — the
/// renderer's only job is to render.
pub fn events(events: &[TimelineEvent], json: bool) -> String {
    if json {
        return pretty(events);
    }
    if events.is_empty() {
        return "(no events in window)".to_owned();
    }
    events.iter().map(event_line).collect::<Vec<_>>().join("\n")
}

#[derive(Serialize)]
struct StatusView<'a> {
    name: &'a str,
    app: &'a str,
    port: u16,
    uptime_ms: u64,
    targets: usize,
    timeline_events: usize,
    tracks: Vec<&'a str>,
}

#[allow(clippy::too_many_arguments)]
pub fn status(
    name: &str,
    app: &str,
    port: u16,
    uptime_ms: u64,
    target_count: usize,
    timeline_events: usize,
    tracks: Vec<&str>,
    json: bool,
) -> String {
    if json {
        return pretty(&StatusView {
            name,
            app,
            port,
            uptime_ms,
            targets: target_count,
            timeline_events,
            tracks: tracks.clone(),
        });
    }
    format!(
        "{name}  {app}  :{port}  up {}  targets {target_count}  timeline {timeline_events}  tracks {}",
        human_ms(uptime_ms),
        tracks.join(",")
    )
}

#[derive(Serialize)]
struct HeapRow {
    target: String,
    #[serde(flatten)]
    metrics: TargetMetrics,
}

pub fn heap(target: &str, metrics: &TargetMetrics, json: bool) -> String {
    if json {
        return pretty(&HeapRow { target: target.to_owned(), metrics: metrics.clone() });
    }
    let heap = metrics.js_heap_kib.map(|kib| format!("{:.1} MiB", kib as f64 / 1024.0));
    format!(
        "{target}: heap {}  dom {}  listeners {}  docs {}",
        heap.as_deref().unwrap_or("?"),
        opt(metrics.dom_nodes),
        opt(metrics.listeners),
        opt(metrics.documents),
    )
}

pub fn ignore(patterns: &[String], json: bool) -> String {
    if json {
        return pretty(patterns);
    }
    if patterns.is_empty() {
        return "no ignore patterns".to_owned();
    }
    patterns.iter().map(|pattern| format!("- {pattern}")).collect::<Vec<_>>().join("\n")
}

/// Render one Timeline event to its canonical one-line form — shared by `tail`, the ignore
/// predicate, and the interactive live pane, so all three read identically.
pub(crate) fn event_line(event: &TimelineEvent) -> String {
    let head = format!("+{:>6}ms [{}]", event.at_ms, truncate(&event.target, 16));
    let body = match &event.track {
        Track::Console(line) => {
            format!("console.{} {}{}", line.level, line.text, location(&line.url, line.line))
        }
        Track::Exception(info) => {
            format!("exception {}{}", info.text, location(&info.url, info.line))
        }
        Track::Log(entry) => {
            format!("log/{} {}{}", entry.source, entry.text, location(&entry.url, entry.line))
        }
        Track::Network(net) => {
            let phase = match net.phase {
                NetPhase::Request => "→",
                NetPhase::Response => "←",
                NetPhase::Finished => "✓",
                NetPhase::Failed => "✗",
            };
            let method = net.method.as_deref().unwrap_or("");
            let status = net.status.map(|code| code.to_string()).unwrap_or_default();
            let what = net.url.as_deref().or(net.error.as_deref()).unwrap_or("");
            format!("net {phase} {method} {status} {what}").split_whitespace().collect::<Vec<_>>().join(" ")
        }
        Track::Ws(frame) => {
            let dir = match frame.dir {
                WsDir::Sent => "↑",
                WsDir::Received => "↓",
                WsDir::Created => "+",
            };
            let len = frame.len.map(|len| format!("{len}b ")).unwrap_or_default();
            let what = frame.preview.as_deref().or(frame.url.as_deref()).unwrap_or("");
            format!("ws {dir} {len}{what}")
        }
    };
    format!("{head} {body}")
}

fn target_label(target: &Target) -> String {
    if !target.title.is_empty() {
        truncate(&target.title, 70)
    } else {
        truncate(&target.url, 70)
    }
}

fn location(url: &Option<String>, line: Option<u64>) -> String {
    match (url, line) {
        (Some(url), Some(line)) => format!(" ({}:{line})", short_url(url)),
        (Some(url), None) => format!(" ({})", short_url(url)),
        _ => String::new(),
    }
}

fn short_url(url: &str) -> String {
    truncate(url.rsplit('/').next().filter(|tail| !tail.is_empty()).unwrap_or(url), 40)
}

fn opt(value: Option<u64>) -> String {
    value.map(|value| value.to_string()).unwrap_or_else(|| "?".to_owned())
}

fn human_ms(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h{}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}
