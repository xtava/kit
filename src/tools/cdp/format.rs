//! Daemon-side rendering: turn live engine data into the text or JSON a [`super::protocol::Reply`]
//! carries. Compact text is the default (cheap for an agent to read); `--json` gives full structure.

use serde::Serialize;
use serde_json::Value;

use crate::cdp::{
    ErrorGroup, ErrorReport, NetPhase, Target, TargetKind, TargetMetrics, TimelineEvent, Track,
    WsDir,
};

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
        let rows: Vec<TargetView> = targets.iter().map(TargetView::from).collect();
        return pretty(&rows);
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
            let label = truncate(&target.label(), 70);
            let meta = target_meta(target);
            format!("{marker}{:<14} {label}{meta}", target.kind.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TargetView {
    id: String,
    kind: TargetKind,
    title: String,
    url: String,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    extension_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    purpose: Option<String>,
}

impl From<&Target> for TargetView {
    fn from(target: &Target) -> Self {
        Self {
            id: target.id.clone(),
            kind: target.kind,
            title: target.title.clone(),
            url: target.url.clone(),
            label: target.label(),
            extension_id: query_param(&target.url, "extensionId"),
            purpose: query_param(&target.url, "purpose"),
        }
    }
}

fn target_meta(target: &Target) -> String {
    let extension = query_param(&target.url, "extensionId");
    let purpose = query_param(&target.url, "purpose");
    match (extension, purpose) {
        (None, None) => String::new(),
        (Some(extension), None) => format!("  ext:{extension}"),
        (None, Some(purpose)) => format!("  purpose:{purpose}"),
        (Some(extension), Some(purpose)) => format!("  ext:{extension} purpose:{purpose}"),
    }
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if name == key && !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    None
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

/// Render an [`ErrorReport`] — the low-context "what's broken" view. Compact by default: one line per
/// group, tagged `(N×)`. The integrity contract is that the view can never *look* clean while being
/// lossy — any risk (a group that merged differing lines, ring eviction, undecoded events) raises a
/// visible `⚠` banner. `explain` expands each group's absorbed variants so a collapse is fully
/// auditable. The grouping itself is the engine's ([`group_errors`]); this only renders.
pub fn errors(report: &ErrorReport, explain: bool, json: bool) -> String {
    if json {
        return pretty(report);
    }
    let mut out: Vec<String> = Vec::new();
    if let Some(banner) = integrity_banner(report) {
        out.push(banner);
    }
    if report.groups.is_empty() {
        out.push("(no errors in window)".to_owned());
        return out.join("\n");
    }
    for group in &report.groups {
        out.push(error_group_line(group));
        if explain && group.has_variants() {
            for variant in &group.variants {
                out.push(format!("    ├ {variant}"));
            }
        }
    }
    out.join("\n")
}

/// One group's headline: its representative line, a `(N×)` count, and — when the collapse merged lines
/// that differ — a `⚠ K variants` flag that says "this count may be hiding distinct errors; --explain".
fn error_group_line(group: &ErrorGroup) -> String {
    let line = event_line(&group.event);
    let count = if group.count > 1 { format!(" ({}×)", group.count) } else { String::new() };
    let variants = if group.has_variants() {
        format!(" ⚠ {} variants", group.variants.len())
    } else {
        String::new()
    };
    format!("{line}{count}{variants}")
}

/// The warning banner shown above the groups whenever the numbers can't be fully trusted — the heart
/// of "never hide an issue by mistake." `None` when the report is clean (no banner, no noise).
fn integrity_banner(report: &ErrorReport) -> Option<String> {
    if !report.has_integrity_risk() {
        return None;
    }
    let mut reasons: Vec<String> = Vec::new();
    if report.saturated {
        reasons.push(
            "window saturated — older errors scrolled off the ring; counts are a floor, pre-warm or widen --since".to_owned(),
        );
    } else if report.evicted.is_some_and(|count| count > 0) {
        reasons.push(format!(
            "{} events dropped from the ring over this attachment's life",
            report.evicted.unwrap_or(0)
        ));
    }
    if report.undecoded > 0 {
        reasons.push(format!(
            "{} error-domain events could not be decoded and are not shown",
            report.undecoded
        ));
    }
    let collisions = report.groups.iter().filter(|group| group.has_variants()).count();
    if collisions > 0 {
        reasons.push(format!(
            "{collisions} group(s) merged differing errors — see ⚠ below; run --explain to expand"
        ));
    }
    Some(format!("⚠ error view may be lossy:\n{}", indent_reasons(&reasons)))
}

fn indent_reasons(reasons: &[String]) -> String {
    reasons.iter().map(|reason| format!("  · {reason}")).collect::<Vec<_>>().join("\n")
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
            format!("net {phase} {method} {status} {what}")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::{ExceptionInfo, Source};

    fn group(text: &str, count: usize, variants: &[&str]) -> ErrorGroup {
        ErrorGroup {
            event: TimelineEvent {
                at_ms: 1,
                source: Source::Renderer,
                target: "main".to_owned(),
                track: Track::Exception(ExceptionInfo {
                    text: text.to_owned(),
                    url: None,
                    line: None,
                }),
            },
            count,
            first_ms: 1,
            last_ms: count as u64,
            variants: variants.iter().map(|variant| (*variant).to_owned()).collect(),
        }
    }

    fn clean(groups: Vec<ErrorGroup>) -> ErrorReport {
        ErrorReport { groups, evicted: Some(0), undecoded: 0, saturated: false }
    }

    #[test]
    fn a_clean_collapse_has_no_banner_and_a_count() {
        let report = clean(vec![group("boom", 3, &["boom"])]);
        let rendered = errors(&report, false, false);
        assert_eq!(rendered.lines().count(), 1, "no banner on a clean report:\n{rendered}");
        assert!(rendered.contains("boom") && rendered.contains("(3×)"), "{rendered}");
        assert!(!rendered.contains('⚠'), "{rendered}");
    }

    #[test]
    fn a_collision_is_flagged_inline_and_bannered() {
        // One group that absorbed two genuinely different lines must never look clean.
        let report = clean(vec![group("GraphQL Error", 5, &["code: 500", "code: 404"])]);
        let rendered = errors(&report, false, false);
        assert!(rendered.contains("⚠ error view may be lossy"), "banner missing:\n{rendered}");
        assert!(rendered.contains("⚠ 2 variants"), "inline flag missing:\n{rendered}");
    }

    #[test]
    fn explain_expands_the_absorbed_variants() {
        let report = clean(vec![group("GraphQL Error", 5, &["code: 500", "code: 404"])]);
        let rendered = errors(&report, true, false);
        assert!(rendered.contains("├ code: 500") && rendered.contains("├ code: 404"), "{rendered}");
    }

    #[test]
    fn eviction_and_undecoded_each_raise_the_banner() {
        let evicted = ErrorReport {
            groups: vec![group("boom", 1, &["boom"])],
            evicted: Some(412),
            undecoded: 0,
            saturated: false,
        };
        assert!(errors(&evicted, false, false).contains("412 events dropped"));

        let undecoded = ErrorReport {
            groups: vec![group("boom", 1, &["boom"])],
            evicted: Some(0),
            undecoded: 3,
            saturated: false,
        };
        assert!(errors(&undecoded, false, false).contains("3 error-domain events"));
    }

    #[test]
    fn empty_window_is_explicit() {
        let rendered = errors(&clean(vec![]), false, false);
        assert!(rendered.contains("(no errors in window)"), "{rendered}");
    }
}
