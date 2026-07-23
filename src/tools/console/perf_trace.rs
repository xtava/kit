//! Opt-in aggregate performance evidence for the public Console TUI.
//!
//! The recorder is deliberately process-global: a public Console process has one TUI lifetime and
//! publishes one report. When the environment variable is absent, recording is only a `OnceLock`
//! lookup and branch. Workloads use [`input_timer`] so disabled tracing does not read the clock.

use std::array;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure, Context, Result};
use serde::Serialize;

use crate::framework::AtomicFileWriter;

const TRACE_ENV: &str = "KIT_CONSOLE_PERF_TRACE";
const TRACE_SCHEMA: &str = "kit-console-perf-trace-v1";
const LATENCY_BUCKET_COUNT: usize = 32;
const LOCK_FILE: &str = ".kit-console-perf-trace.lock";
const TEMP_PREFIX: &str = ".kit-console-perf-trace";

static TRACE: OnceLock<Option<PerfTrace>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputKind {
    Key,
    Paste,
    Mouse,
    Resize,
}

struct PerfTrace {
    path: PathBuf,
    redraws: AtomicU64,
    snapshots: AtomicU64,
    list_panes: AtomicU64,
    terminal_projections: AtomicU64,
    activity_screen_reads: AtomicU64,
    key_latency: LatencyHistogram,
    paste_latency: LatencyHistogram,
    mouse_latency: LatencyHistogram,
    resize_latency: LatencyHistogram,
}

impl PerfTrace {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            redraws: AtomicU64::new(0),
            snapshots: AtomicU64::new(0),
            list_panes: AtomicU64::new(0),
            terminal_projections: AtomicU64::new(0),
            activity_screen_reads: AtomicU64::new(0),
            key_latency: LatencyHistogram::new(),
            paste_latency: LatencyHistogram::new(),
            mouse_latency: LatencyHistogram::new(),
            resize_latency: LatencyHistogram::new(),
        }
    }

    fn latency(&self, kind: InputKind) -> &LatencyHistogram {
        match kind {
            InputKind::Key => &self.key_latency,
            InputKind::Paste => &self.paste_latency,
            InputKind::Mouse => &self.mouse_latency,
            InputKind::Resize => &self.resize_latency,
        }
    }

    fn snapshot(&self) -> TraceReport {
        TraceReport {
            schema: TRACE_SCHEMA,
            redraws: self.redraws.load(Ordering::Relaxed),
            snapshots: self.snapshots.load(Ordering::Relaxed),
            list_panes: self.list_panes.load(Ordering::Relaxed),
            terminal_projections: self.terminal_projections.load(Ordering::Relaxed),
            activity_screen_reads: self.activity_screen_reads.load(Ordering::Relaxed),
            input_latency: InputLatencyReport {
                key: self.key_latency.snapshot(),
                paste: self.paste_latency.snapshot(),
                mouse: self.mouse_latency.snapshot(),
                resize: self.resize_latency.snapshot(),
            },
        }
    }
}

struct LatencyHistogram {
    count: AtomicU64,
    total_nanoseconds: AtomicU64,
    max_nanoseconds: AtomicU64,
    buckets: [AtomicU64; LATENCY_BUCKET_COUNT],
}

impl LatencyHistogram {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            total_nanoseconds: AtomicU64::new(0),
            max_nanoseconds: AtomicU64::new(0),
            buckets: array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    fn record(&self, elapsed: Duration) {
        let nanoseconds = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_nanoseconds.fetch_add(nanoseconds, Ordering::Relaxed);
        self.max_nanoseconds.fetch_max(nanoseconds, Ordering::Relaxed);
        self.buckets[latency_bucket(elapsed)].fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> LatencyReport {
        LatencyReport {
            count: self.count.load(Ordering::Relaxed),
            total_nanoseconds: self.total_nanoseconds.load(Ordering::Relaxed),
            max_nanoseconds: self.max_nanoseconds.load(Ordering::Relaxed),
            bucket_upper_microseconds: latency_bucket_bounds(),
            bucket_counts: self
                .buckets
                .iter()
                .map(|bucket| bucket.load(Ordering::Relaxed))
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct TraceReport {
    schema: &'static str,
    redraws: u64,
    snapshots: u64,
    list_panes: u64,
    terminal_projections: u64,
    activity_screen_reads: u64,
    input_latency: InputLatencyReport,
}

#[derive(Serialize)]
struct InputLatencyReport {
    key: LatencyReport,
    paste: LatencyReport,
    mouse: LatencyReport,
    resize: LatencyReport,
}

#[derive(Serialize)]
struct LatencyReport {
    count: u64,
    total_nanoseconds: u64,
    max_nanoseconds: u64,
    bucket_upper_microseconds: Vec<u64>,
    bucket_counts: Vec<u64>,
}

pub(crate) fn initialize() -> Result<()> {
    let trace = match std::env::var_os(TRACE_ENV) {
        None => None,
        Some(value) => {
            let path = PathBuf::from(value);
            ensure!(path.is_absolute(), "{TRACE_ENV} must be an absolute path");
            ensure!(path.file_name().is_some(), "{TRACE_ENV} must name a trace file");
            Some(PerfTrace::new(path))
        }
    };
    TRACE.set(trace).map_err(|_| anyhow!("Console performance trace was initialized twice"))
}

pub(crate) fn record_redraw() {
    if let Some(trace) = TRACE.get().and_then(Option::as_ref) {
        trace.redraws.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn record_snapshot() {
    if let Some(trace) = TRACE.get().and_then(Option::as_ref) {
        trace.snapshots.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn record_list_panes() {
    if let Some(trace) = TRACE.get().and_then(Option::as_ref) {
        trace.list_panes.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn record_terminal_projection() {
    if let Some(trace) = TRACE.get().and_then(Option::as_ref) {
        trace.terminal_projections.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn record_activity_screen_read() {
    if let Some(trace) = TRACE.get().and_then(Option::as_ref) {
        trace.activity_screen_reads.fetch_add(1, Ordering::Relaxed);
    }
}

/// Start an input measurement only when tracing is enabled.
pub(crate) fn input_timer() -> Option<Instant> {
    TRACE.get().and_then(Option::as_ref).map(|_| Instant::now())
}

pub(crate) fn record_input_latency(kind: InputKind, started: Option<Instant>) {
    let Some(started) = started else {
        return;
    };
    if let Some(trace) = TRACE.get().and_then(Option::as_ref) {
        trace.latency(kind).record(started.elapsed());
    }
}

pub(crate) fn flush() -> Result<()> {
    let Some(trace) = TRACE.get().and_then(Option::as_ref) else {
        return Ok(());
    };
    let parent =
        trace.path.parent().context("Console performance trace has no parent directory")?;
    let report =
        serde_json::to_vec(&trace.snapshot()).context("serialize Console performance trace")?;
    let writer = AtomicFileWriter::new(parent, LOCK_FILE, TEMP_PREFIX);
    let _lock = writer.lock().context("lock Console performance trace")?;
    writer.replace(&trace.path, &report).context("publish Console performance trace")
}

fn latency_bucket(elapsed: Duration) -> usize {
    let microseconds = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
    if microseconds == 0 {
        return 0;
    }
    (u64::BITS - microseconds.leading_zeros()).min((LATENCY_BUCKET_COUNT - 1) as u32) as usize
}

fn latency_bucket_bounds() -> Vec<u64> {
    (0..LATENCY_BUCKET_COUNT)
        .map(|index| {
            if index == LATENCY_BUCKET_COUNT - 1 {
                u64::MAX
            } else {
                (1_u64 << index).saturating_sub(1)
            }
        })
        .collect()
}
