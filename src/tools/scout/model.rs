//! scout's data model — the shape of a memory survey.
//!
//! Two planes, correlated:
//!   - **process plane** ([`Process`]): OS truth from `/proc` — PSS per process, classified by [`Role`].
//!   - **target plane** ([`Target`]): app truth from CDP — JS heap / DOM / listeners per window or webview.
//!
//! A [`Survey`] is one snapshot of every Electron instance on the machine, plus system totals.
//! Everything is `Serialize` so `--json` is a derive, not a code path.

use serde::Serialize;

/// One complete sweep: every discovered Electron instance + system memory, at a moment in time.
#[derive(Debug, Clone, Serialize)]
pub struct Survey {
    pub instances: Vec<Instance>,
    pub system: SystemMemory,
    /// Unix-epoch millis the survey was taken; used for live deltas between sweeps.
    pub taken_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemMemory {
    pub total_kib: u64,
    pub available_kib: u64,
    pub swap_total_kib: u64,
    pub swap_used_kib: u64,
}

/// One Electron application: a browser (main) process and the child-process fleet it spawned.
#[derive(Debug, Clone, Serialize)]
pub struct Instance {
    /// Display name, e.g. "modular-canary", "modular-dev". Derived from `--class=` or the binary path.
    pub name: String,
    /// The main/browser process pid — the root of the fleet.
    pub root_pid: u32,
    /// Remote-debugging port, if the instance exposes one.
    /// `None` ⇒ no target plane (e.g. a packaged prod build with no `--remote-debugging-port`).
    pub debug_port: Option<u16>,
    pub processes: Vec<Process>,
    /// CDP targets (windows / webviews / workers). Empty when `debug_port` is `None`.
    pub targets: Vec<Target>,
}

/// A single OS process, with its real (proportional) memory.
#[derive(Debug, Clone, Serialize)]
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    pub role: Role,
    /// Proportional set size — shared pages counted once. The honest "real memory" number,
    /// summed from `/proc/<pid>/smaps_rollup`. Prefer this over `rss` everywhere user-facing.
    pub pss_kib: u64,
    /// Resident set size — over-counts shared pages (≈3× for Electron). Kept only for reference.
    pub rss_kib: u64,
    pub swap_kib: u64,
    pub threads: u16,
    /// The CDP target this process renders, once correlated (best-effort; see `survey`).
    pub target_id: Option<String>,
}

/// What a process *is*, classified from its `/proc/<pid>/cmdline`.
///
/// Gotcha: on Linux, Electron renderers forked from the zygote keep `--type=zygote` in their
/// cmdline (they don't re-exec). So a `--type=zygote` process with a large RSS *is* a renderer;
/// only the tiny, low-RSS ones are real zygote templates. Classify with that in mind.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum Role {
    /// The main / browser process (no `--type`).
    Browser,
    /// A page / iframe renderer (`--type=renderer`, or a forked `--type=zygote` carrying a real heap).
    Renderer,
    /// The GPU process (`--type=gpu-process`).
    Gpu,
    /// A Chromium service process (`--type=utility --utility-sub-type=<kind>.mojom.<…>Service`).
    Utility(UtilityKind),
    /// The file-watcher utility (`--type=fileWatcher`). Called out on its own because it is a frequent
    /// CPU/leak offender — a runaway recursive watch pegs its `libuv-worker` thread.
    FileWatcher,
    /// An idle zygote template (tiny, low-RSS; forks renderers).
    Zygote,
    /// The sandbox broker (`--type=broker`).
    Broker,
    Unknown,
}

/// The flavour of a Chromium `--type=utility` process, from its `--utility-sub-type`.
///
/// `Node` covers every Electron `UtilityProcess` (extension host, shared process, pty host) — they
/// all report `node.mojom.NodeService` and are indistinguishable at the OS layer; the target plane
/// tells them apart.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum UtilityKind {
    Network,
    Storage,
    Audio,
    Node,
    Other(String),
}

/// A CDP target — a window, webview, or worker the app exposes for debugging.
#[derive(Debug, Clone, Serialize)]
pub struct Target {
    pub id: String,
    pub kind: TargetKind,
    pub title: String,
    pub url: String,
    /// `performance.memory.usedJSHeapSize`, in KiB. Cheap and non-pausing (unlike a heap snapshot).
    pub js_heap_kib: Option<u64>,
    /// From `Memory.getDOMCounters`.
    pub dom_nodes: Option<u64>,
    pub listeners: Option<u64>,
    pub documents: Option<u64>,
    /// The OS process rendering this target, once correlated.
    pub pid: Option<u32>,
}

/// The kind of a CDP target, parsed from its url/type.
#[derive(Debug, Clone, Serialize)]
pub enum TargetKind {
    /// A workbench window. `workspace` is the workspace id pulled from the url.
    Workbench { workspace: String },
    /// An extension webview (`vscode-webview://…`). JS-light, but each is its own renderer process —
    /// the cost is process count, not heap.
    ExtensionWebview,
    /// A non-extension webview guest (e.g. an embedded site).
    Webview,
    BackgroundWorker,
    Worker,
    Page,
    Other,
}
