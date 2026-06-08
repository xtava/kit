//! The wire between the thin client and the warm Attachment daemon, newline-delimited JSON over a
//! unix socket. Two shapes:
//!
//! - **Request/reply** — one [`Query`] in, one [`Reply`] out. The daemon renders the output, so the
//!   one-shot CLI never deserializes a Timeline.
//! - **Subscription** — a [`Command::Subscribe`] in, then a [`Frame`] *stream* out that stays open.
//!   Frames carry structured [`TimelineEvent`]s; the interactive client renders them itself.

use serde::{Deserialize, Serialize};

use crate::cdp::{Source, TargetKind, TimelineEvent, TrackKind};

#[derive(Debug, Serialize, Deserialize)]
pub struct Query {
    pub command: Command,
    /// The caller asked for `--json`; the daemon renders structured JSON instead of compact text.
    pub json: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Command {
    Ping,
    Status,
    Targets,
    Tail { since_ms: u64, tracks: Option<Vec<TrackKind>>, source: Option<Source> },
    Eval { target: Option<String>, expr: String },
    Heap { target: Option<String> },
    Snap { target: Option<String>, interactive: bool },
    Click { target: Option<String>, reference: String },
    Fill { target: Option<String>, reference: String, text: String },
    Lens { target: Option<String>, source: String, args: Vec<String> },
    Ignore(IgnoreOp),
    /// The target picker's data source: every Target joined with its Timeline event volume, ranked
    /// active-first. Returns a JSON `Vec<TargetActivity>` in the [`Reply`] output.
    TargetList,
    /// Open a live subscription: the daemon replies with a [`Frame`] stream (not a [`Reply`]) and
    /// holds the socket open, pushing each new Timeline event until the client disconnects.
    Subscribe { since_ms: u64 },
    Detach,
}

/// A Target annotated with how much it is actually streaming — what the picker ranks and filters by.
/// `events` counts the target's events currently held in the daemon's Timeline ring.
#[derive(Debug, Serialize, Deserialize)]
pub struct TargetActivity {
    pub label: String,
    pub kind: TargetKind,
    pub title: String,
    pub url: String,
    pub events: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum IgnoreOp {
    Add(String),
    List,
    Clear,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    /// `false` makes the client exit non-zero — the command ran but the *result* is a failure
    /// (a thrown eval, a selector that matched nothing).
    pub ok: bool,
    /// Rendered output, ready to print verbatim (no trailing newline).
    pub output: String,
}

impl Reply {
    pub fn ok(output: impl Into<String>) -> Self {
        Self { ok: true, output: output.into() }
    }

    pub fn fail(output: impl Into<String>) -> Self {
        Self { ok: false, output: output.into() }
    }
}

/// One frame on a [`Command::Subscribe`] stream. The opening frame is a [`Frame::Backfill`] of the
/// recent window; every frame after is a single live [`Frame::Event`].
#[derive(Debug, Serialize, Deserialize)]
pub enum Frame {
    Backfill(Vec<TimelineEvent>),
    Event(TimelineEvent),
}
