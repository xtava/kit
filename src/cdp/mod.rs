//! `cdp` — the Chrome DevTools Protocol engine. A peer of `framework` and `tui` (see
//! `docs/adr/0001`): the protocol client, endpoint discovery, port detection, the generic target
//! model, and the Timeline. Both `scout` (memory recon) and `cdp` (the debugger) build on it.
//!
//! The engine is deliberately app-agnostic — it knows Chromium/Electron and the protocol, nothing
//! about any app running on top of it. App meaning lives in a `scout` recon step or a `cdp` lens.

mod client;
mod discovery;
mod http;
mod ports;
mod target;
mod timeline;

pub use client::{probe_metrics, probe_target, CdpConnection, CdpEvent, TargetMetrics};
pub use discovery::{
    browser_endpoint, discover, is_cdp, node_endpoint, targets, BrowserEndpoint, Instance,
    NodeEndpoint,
};
pub use ports::listening_ports;
pub use target::{select, select_active, ScoreReason, Target, TargetKind, TargetScore};
pub use timeline::{
    group_errors, ConsoleLine, ErrorGroup, ErrorReport, ExceptionInfo, LogEntry, NetEvent,
    NetPhase, Source, Timeline, TimelineEvent, Track, TrackKind, WatchDelta, WsDir, WsFrame,
};
