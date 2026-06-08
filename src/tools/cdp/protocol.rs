//! The wire between the thin client and the warm Attachment daemon: one [`Query`] line in, one
//! [`Reply`] line out, newline-delimited JSON over a unix socket.
//!
//! The daemon owns *all* formatting — it holds the live types, so the client never deserializes a
//! Timeline. A Query says what to do and whether the caller wants JSON; the Reply is rendered output.

use serde::{Deserialize, Serialize};

use crate::cdp::TrackKind;

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
    Tail { since_ms: u64, tracks: Option<Vec<TrackKind>> },
    Eval { target: Option<String>, expr: String },
    Heap { target: Option<String> },
    Snap { target: Option<String>, interactive: bool },
    Click { target: Option<String>, reference: String },
    Fill { target: Option<String>, reference: String, text: String },
    Lens { target: Option<String>, source: String, args: Vec<String> },
    Detach,
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
