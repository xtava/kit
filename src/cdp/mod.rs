//! `cdp` — the Chrome DevTools Protocol engine. A peer of `framework` and `tui` (see
//! `docs/adr/0001`): the protocol client, endpoint discovery, port detection, the generic target
//! model, and the Timeline. Both `scout` (memory recon) and `cdp` (the debugger) build on it.
//!
//! The engine is deliberately app-agnostic — it knows Chromium/Electron and the protocol, nothing
//! about any app running on top of it. App meaning lives in a `scout` recon step or a `cdp` lens.

pub(crate) mod base64;
mod client;
mod discovery;
mod http;
mod ports;
mod sourcemap;
mod target;
mod timeline;

pub use client::{
    capture_screenshot, probe_metrics, probe_target, CdpConnection, CdpEvent, ImageFormat, NoFrame,
    TargetMetrics,
};
pub use discovery::{
    browser_endpoint, discover, is_cdp, node_endpoint, targets, BrowserEndpoint, Instance,
    NodeEndpoint,
};
pub use ports::{listening_ports, owner_pid};
pub use sourcemap::{inline_map, resolve_map_url, SourceMap, SourceMatch};
pub use target::{select, select_active, ScoreReason, Target, TargetKind, TargetScore};
pub use timeline::{
    group_errors, ConsoleArg, ConsoleLine, ErrorGroup, ErrorReport, ExceptionInfo, LogEntry,
    NetEvent, NetPhase, Source, Timeline, TimelineEvent, TraceOutcome, TraceRecord, Track,
    TrackKind, WatchDelta, WsDir, WsFrame,
};
