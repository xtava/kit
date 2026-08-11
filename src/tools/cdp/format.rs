//! Daemon-side rendering: turn live engine data into the text or JSON a [`super::protocol::Reply`]
//! carries. Compact text is the default (cheap for an agent to read); `--json` gives full structure.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::cdp::{
    group_errors, ErrorGroup, ErrorReport, EventIngressSnapshot, NetPhase, PerformanceReport,
    Target, TargetKind, TargetMetrics, TimelineEvent, TraceOutcome, TraceRecord, Track, WsDir,
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

/// Integrity and accounting facts captured before compacting a Timeline into a [`brief`].
#[derive(Clone, Copy)]
pub struct BriefMeta {
    pub window_ms: u64,
    pub matching_events: usize,
    pub suppressed_by_ignore: usize,
    pub clipped_by_limit: usize,
    pub evicted: Option<usize>,
    pub undecoded: usize,
    pub ingress: EventIngressSnapshot,
    pub saturated: bool,
}

/// Agent-safe Timeline compression. This is deliberately a presentation layer: the raw Timeline stays
/// in the daemon ring, while this view spends context on error groups, repeated non-error groups, a
/// short raw tail, and explicit omission counts.
pub fn brief(
    events: &[TimelineEvent],
    meta: BriefMeta,
    tail: usize,
    groups: usize,
    json: bool,
) -> String {
    let report = build_brief_report(events, meta, tail, groups);
    if json {
        return pretty(&report);
    }

    let mut out: Vec<String> = Vec::new();
    let hidden = hidden_suffix(&report);
    out.push(format!(
        "brief: {} matching event(s), {} visible after ignore{hidden}",
        report.matching_events, report.visible_events
    ));
    if report.suppressed_by_ignore > 0 {
        out.push(format!(
            "⚠ {} matching event(s) hidden by ignore; `kit cdp ignore --clear` re-includes them",
            report.suppressed_by_ignore
        ));
    }
    if report.clipped_by_limit > 0 {
        out.push(format!(
            "⚠ {} visible event(s) clipped by --limit before briefing",
            report.clipped_by_limit
        ));
    }
    if let Some(banner) = integrity_banner(&report.errors) {
        out.push(banner);
    }

    out.push("errors".to_owned());
    if report.errors.groups.is_empty() {
        out.push("  (no errors in visible window)".to_owned());
    } else {
        for group in &report.errors.groups {
            out.push(format!("  {}", error_group_line(group)));
        }
    }

    out.push("repeated non-errors".to_owned());
    if report.repeated.is_empty() {
        out.push("  (no repeated non-error groups)".to_owned());
    } else {
        for group in &report.repeated {
            out.push(format!("  {}", brief_group_line(group)));
        }
    }

    out.push("recent raw tail".to_owned());
    if report.recent.is_empty() {
        out.push("  (no visible events in window)".to_owned());
    } else {
        for event in &report.recent {
            out.push(format!("  {}", event_line(event)));
        }
    }

    out.push("omissions".to_owned());
    let mut omissions = Vec::new();
    if report.omitted.raw_rows > 0 {
        omissions.push(format!("{} older raw row(s) not shown verbatim", report.omitted.raw_rows));
    }
    if report.omitted.older_one_off_non_errors > 0 {
        omissions.push(format!(
            "⚠ {} older one-off non-error row(s) are only counted, not summarized",
            report.omitted.older_one_off_non_errors
        ));
    }
    if report.omitted.repeated_groups > 0 {
        omissions.push(format!(
            "{} repeated group(s) / {} event(s) not shown because of --groups",
            report.omitted.repeated_groups, report.omitted.repeated_events
        ));
    }
    if omissions.is_empty() {
        out.push("  none".to_owned());
    } else {
        out.extend(omissions.into_iter().map(|line| format!("  · {line}")));
    }
    out.push(
        "raw escape hatch: `kit cdp tail` with the same filters; `kit cdp errors --explain` for error variants"
            .to_owned(),
    );

    out.join("\n")
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BriefReport {
    window_ms: u64,
    matching_events: usize,
    visible_events: usize,
    suppressed_by_ignore: usize,
    clipped_by_limit: usize,
    errors: ErrorReport,
    repeated: Vec<BriefGroup>,
    omitted: BriefOmission,
    recent: Vec<TimelineEvent>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BriefGroup {
    count: usize,
    first_ms: u64,
    last_ms: u64,
    sample_line: String,
    sample: TimelineEvent,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BriefOmission {
    raw_rows: usize,
    older_one_off_non_errors: usize,
    repeated_groups: usize,
    repeated_events: usize,
}

fn build_brief_report(
    events: &[TimelineEvent],
    meta: BriefMeta,
    tail: usize,
    groups: usize,
) -> BriefReport {
    let all_repeated = repeated_non_errors(events);
    let repeated: Vec<BriefGroup> = all_repeated.iter().take(groups).cloned().collect();
    let omitted_repeated = &all_repeated[repeated.len()..];

    let recent_start = events.len().saturating_sub(tail);
    let recent = events.iter().skip(recent_start).cloned().collect();
    let counts = non_error_counts(events);
    let older_one_off_non_errors = events
        .iter()
        .take(recent_start)
        .filter(|event| !event.track.is_error())
        .filter(|event| counts.get(&brief_key(event)).copied().unwrap_or(0) == 1)
        .count();

    BriefReport {
        window_ms: meta.window_ms,
        matching_events: meta.matching_events,
        visible_events: events.len(),
        suppressed_by_ignore: meta.suppressed_by_ignore,
        clipped_by_limit: meta.clipped_by_limit,
        errors: ErrorReport {
            groups: group_errors(events),
            evicted: meta.evicted,
            undecoded: meta.undecoded,
            ingress: meta.ingress,
            saturated: meta.saturated,
        },
        repeated,
        omitted: BriefOmission {
            raw_rows: events.len().saturating_sub(tail),
            older_one_off_non_errors,
            repeated_groups: omitted_repeated.len(),
            repeated_events: omitted_repeated.iter().map(|group| group.count).sum(),
        },
        recent,
    }
}

fn hidden_suffix(report: &BriefReport) -> String {
    let hidden = report
        .omitted
        .raw_rows
        .saturating_add(report.suppressed_by_ignore)
        .saturating_add(report.clipped_by_limit);
    if hidden == 0 {
        String::new()
    } else {
        format!("; {hidden} row(s) not printed raw")
    }
}

fn repeated_non_errors(events: &[TimelineEvent]) -> Vec<BriefGroup> {
    let mut groups: HashMap<String, BriefGroup> = HashMap::new();
    for event in events.iter().filter(|event| !event.track.is_error()) {
        let key = brief_key(event);
        let entry = groups.entry(key).or_insert_with(|| BriefGroup {
            count: 0,
            first_ms: event.at_ms,
            last_ms: event.at_ms,
            sample_line: event_line(event),
            sample: event.clone(),
        });
        entry.count += 1;
        entry.last_ms = event.at_ms;
    }
    let mut repeated: Vec<BriefGroup> =
        groups.into_values().filter(|group| group.count > 1).collect();
    repeated.sort_by(|a, b| b.count.cmp(&a.count).then(b.last_ms.cmp(&a.last_ms)));
    repeated
}

fn non_error_counts(events: &[TimelineEvent]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for event in events.iter().filter(|event| !event.track.is_error()) {
        *counts.entry(brief_key(event)).or_insert(0) += 1;
    }
    counts
}

fn brief_key(event: &TimelineEvent) -> String {
    format!("{:?}|{}|{}", event.source, event.target, event_body(event))
}

fn brief_group_line(group: &BriefGroup) -> String {
    let range = if group.first_ms == group.last_ms {
        format!("+{}ms", group.first_ms)
    } else {
        format!("+{}..+{}ms", group.first_ms, group.last_ms)
    };
    format!("{} ({}×, {range})", group.sample_line, group.count)
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
    if report.ingress.dropped.total() > 0 {
        reasons.push(format!(
            "{} CDP events dropped at bounded ingress ({} error, {} control, {} normal); peak backlog {}",
            report.ingress.dropped.total(),
            report.ingress.dropped.errors,
            report.ingress.dropped.control,
            report.ingress.dropped.normal,
            report.ingress.peak_backlog,
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
    ingress: EventIngressSnapshot,
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
    ingress: EventIngressSnapshot,
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
            ingress,
            tracks: tracks.clone(),
        });
    }
    format!(
        "{name}  {app}  :{port}  up {}  targets {target_count}  timeline {timeline_events}  ingress peak {} dropped {} (err {} ctl {} normal {})  tracks {}",
        human_ms(uptime_ms),
        ingress.peak_backlog,
        ingress.dropped.total(),
        ingress.dropped.errors,
        ingress.dropped.control,
        ingress.dropped.normal,
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

#[derive(Serialize)]
struct PerformanceView<'a> {
    target: &'a str,
    profile: &'a Path,
    report_path: &'a Path,
    #[serde(flatten)]
    report: &'a PerformanceReport,
}

pub fn performance(
    target: &str,
    profile: &Path,
    report_path: &Path,
    report: &PerformanceReport,
    json: bool,
) -> String {
    if json {
        return pretty(&PerformanceView { target, profile, report_path, report });
    }

    let wall_seconds = report.wall_time_ms as f64 / 1_000.0;
    let metric = |name: &str| report.metric_deltas.get(name).copied().unwrap_or(0.0);
    let percent = |seconds: f64| {
        if wall_seconds == 0.0 {
            0.0
        } else {
            seconds * 100.0 / wall_seconds
        }
    };
    let main = report.performance_metrics_error.as_ref().map_or_else(
        || {
            format!(
                "main       task {:.3}s ({:.1}%) · script {:.3}s ({:.1}%) · layout {:.3}s · style {:.3}s",
                metric("TaskDuration"),
                percent(metric("TaskDuration")),
                metric("ScriptDuration"),
                percent(metric("ScriptDuration")),
                metric("LayoutDuration"),
                metric("RecalcStyleDuration"),
            )
        },
        |error| format!("main       unavailable · {error}"),
    );
    let layers = report.layers.as_ref().map_or_else(
        || {
            format!(
                "layers     unavailable · {}",
                report.layer_metrics_error.as_deref().unwrap_or("no snapshot")
            )
        },
        |layers| {
            format!(
                "layers     {} total · {} drawing · {} DOM-mapped",
                layers.total, layers.drawing, layers.dom_mapped
            )
        },
    );
    let mut lines = vec![
        format!("target     {target}"),
        format!("window     {:.3}s wall · {}µs samples", wall_seconds, report.sampling_interval_us),
        main,
        layers,
        format!(
            "memory     heap {} → {} · nodes {} → {} · listeners {} → {} · docs {} → {}",
            heap_size(report.before.js_heap_kib),
            heap_size(report.after.js_heap_kib),
            opt(report.before.dom_nodes),
            opt(report.after.dom_nodes),
            opt(report.before.listeners),
            opt(report.after.listeners),
            opt(report.before.documents),
            opt(report.after.documents),
        ),
        format!("profile    {}", profile.display()),
        format!("report     {}", report_path.display()),
        "hot self".to_owned(),
    ];
    lines.extend(report.cpu.hotspots.iter().map(|hotspot| {
        format!(
            "  {:>6.2}%  {:>8.1}ms  {}{}",
            hotspot.self_percent,
            hotspot.self_time_us as f64 / 1_000.0,
            hotspot.function_name,
            location(&hotspot.url, hotspot.line),
        )
    }));
    lines.join("\n")
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
    format!("{head} {}", event_body(event))
}

fn event_body(event: &TimelineEvent) -> String {
    match &event.track {
        Track::Console(line) => {
            format!("console.{} {}{}", line.level, line.text, location(&line.url, line.line))
        }
        Track::Exception(info) => {
            // The resolved site goes on the headline — V8's description embeds the whole stack,
            // and a marker after five frames of bundle noise is a marker nobody sees.
            let mut text = info.text.clone();
            if let Some(site) = &info.resolved {
                let marker = format!(" → {site}");
                match text.find('\n') {
                    Some(at) => text.insert_str(at, &marker),
                    None => text.push_str(&marker),
                }
            }
            format!("exception {text}{}", location(&info.url, info.line))
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
        Track::Lifecycle(event) => format!("life {}", event.name),
        Track::Watch(delta) => match &delta.from {
            Some(from) => format!("watch {} {from} → {}", delta.name, delta.to),
            None => format!("watch {} → {}", delta.name, delta.to),
        },
        Track::Trace(record) => trace_line(record),
    }
}

/// `trace save (2 args) → {ok: true} 38ms` · `trace save (1 arg) ✗ TypeError…` · the rate-cap
/// summary reads as a warning so suppression is never mistaken for silence.
fn trace_line(record: &TraceRecord) -> String {
    if let Some(count) = record.suppressed {
        return format!("trace {} ⚠ {count} hit(s) suppressed (rate cap)", record.name);
    }
    let mut line = format!("trace {}", record.name);
    if let Some(value) = &record.value {
        line.push(' ');
        line.push_str(value);
    }
    match &record.outcome {
        Some(TraceOutcome::Returned(preview)) => line.push_str(&format!(" → {preview}")),
        Some(TraceOutcome::Threw(preview)) => line.push_str(&format!(" ✗ {preview}")),
        None => {}
    }
    if let Some(ms) = record.duration_ms {
        line.push_str(&format!(" {:.0}ms", ms));
    }
    line
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

fn heap_size(kib: Option<u64>) -> String {
    kib.map(|kib| format!("{:.1} MiB", kib as f64 / 1024.0)).unwrap_or_else(|| "?".to_owned())
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
    use crate::cdp::{ConsoleLine, EventDropCounts, ExceptionInfo, Source};

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
                    frames: Vec::new(),
                    resolved: None,
                }),
            },
            count,
            first_ms: 1,
            last_ms: count as u64,
            variants: variants.iter().map(|variant| (*variant).to_owned()).collect(),
        }
    }

    fn clean(groups: Vec<ErrorGroup>) -> ErrorReport {
        ErrorReport {
            groups,
            evicted: Some(0),
            undecoded: 0,
            ingress: EventIngressSnapshot::default(),
            saturated: false,
        }
    }

    fn meta(matching_events: usize) -> BriefMeta {
        BriefMeta {
            window_ms: 10_000,
            matching_events,
            suppressed_by_ignore: 0,
            clipped_by_limit: 0,
            evicted: Some(0),
            undecoded: 0,
            ingress: EventIngressSnapshot::default(),
            saturated: false,
        }
    }

    fn console(at_ms: u64, text: &str) -> TimelineEvent {
        TimelineEvent {
            at_ms,
            source: Source::Renderer,
            target: "main".to_owned(),
            track: Track::Console(ConsoleLine {
                level: "log".to_owned(),
                text: text.to_owned(),
                args: Vec::new(),
                url: None,
                line: None,
            }),
        }
    }

    fn exception(at_ms: u64, text: &str) -> TimelineEvent {
        TimelineEvent {
            at_ms,
            source: Source::Renderer,
            target: "main".to_owned(),
            track: Track::Exception(ExceptionInfo {
                text: text.to_owned(),
                url: None,
                line: None,
                frames: Vec::new(),
                resolved: None,
            }),
        }
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
            ingress: EventIngressSnapshot::default(),
            saturated: false,
        };
        assert!(errors(&evicted, false, false).contains("412 events dropped"));

        let undecoded = ErrorReport {
            groups: vec![group("boom", 1, &["boom"])],
            evicted: Some(0),
            undecoded: 3,
            ingress: EventIngressSnapshot::default(),
            saturated: false,
        };
        assert!(errors(&undecoded, false, false).contains("3 error-domain events"));
    }

    #[test]
    fn bounded_ingress_loss_is_part_of_error_integrity_not_a_side_channel() {
        let mut report = clean(Vec::new());
        report.ingress = EventIngressSnapshot {
            dropped: EventDropCounts { control: 2, errors: 3, normal: 40 },
            peak_backlog: 2_816,
        };
        let rendered = errors(&report, false, false);
        assert!(rendered.contains("45 CDP events dropped at bounded ingress"), "{rendered}");
        assert!(rendered.contains("3 error"), "{rendered}");
        assert!(rendered.contains("peak backlog 2816"), "{rendered}");
    }

    #[test]
    fn empty_window_is_explicit() {
        let rendered = errors(&clean(vec![]), false, false);
        assert!(rendered.contains("(no errors in window)"), "{rendered}");
    }

    #[test]
    fn brief_groups_repeated_noise_and_discloses_older_one_offs() {
        let events = vec![
            console(1, "hmr heartbeat"),
            console(2, "one-off setup"),
            console(3, "hmr heartbeat"),
            exception(4, "boom"),
            console(5, "hmr heartbeat"),
            console(6, "recent status"),
        ];

        let rendered = brief(&events, meta(events.len()), 2, 1, false);

        assert!(rendered.contains("errors"), "{rendered}");
        assert!(rendered.contains("boom"), "{rendered}");
        assert!(rendered.contains("hmr heartbeat") && rendered.contains("(3×"), "{rendered}");
        assert!(rendered.contains("older one-off non-error"), "{rendered}");
        assert!(rendered.contains("raw escape hatch"), "{rendered}");
    }

    #[test]
    fn brief_reports_ignore_and_limit_blind_spots() {
        let mut facts = meta(5);
        facts.suppressed_by_ignore = 2;
        facts.clipped_by_limit = 1;

        let rendered = brief(&[console(1, "visible")], facts, 12, 8, false);

        assert!(rendered.contains("hidden by ignore"), "{rendered}");
        assert!(rendered.contains("clipped by --limit"), "{rendered}");
    }
}
