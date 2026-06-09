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
    Tail(TimelineQuery),
    /// Error-shaped Timeline events, deduplicated into counted groups — the low-context "what's
    /// broken" view. Same filters as [`Command::Tail`]; the daemon collapses duplicates and reports
    /// the integrity facts (variants absorbed, ring eviction, undecoded events) alongside them.
    Errors {
        query: TimelineQuery,
        /// Expand each group's absorbed variants instead of one representative line — the audit view.
        explain: bool,
    },
    Eval {
        target: Option<String>,
        expr: String,
    },
    /// Readiness verdict: resolve the workbench Target, probe its live document state, and report the
    /// ranked candidate field with the reason each was chosen or rejected.
    Ready {
        target: Option<String>,
    },
    Heap {
        target: Option<String>,
    },
    Snap {
        target: Option<String>,
        interactive: bool,
    },
    Click {
        target: Option<String>,
        reference: String,
    },
    Fill {
        target: Option<String>,
        reference: String,
        text: String,
    },
    Lens {
        target: Option<String>,
        source: String,
        args: Vec<String>,
    },
    ExtensionBundle {
        target: Option<String>,
        source: String,
        extension_id: String,
        query: TimelineQuery,
    },
    Ignore(IgnoreOp),
    /// The target picker's data source: every Target joined with its Timeline event volume, ranked
    /// active-first. Returns a JSON `Vec<TargetActivity>` in the [`Reply`] output.
    TargetList,
    /// Open a live subscription: the daemon replies with a [`Frame`] stream (not a [`Reply`]) and
    /// holds the socket open, pushing each new Timeline event until the client disconnects.
    Subscribe {
        since_ms: u64,
    },
    Detach,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineQuery {
    pub since_ms: u64,
    pub tracks: Option<Vec<TrackKind>>,
    pub source: Option<Source>,
    pub target: Option<String>,
    pub grep: Option<String>,
    pub extension: Option<String>,
    pub limit: Option<usize>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::{LogEntry, Source, TargetKind, TimelineEvent, Track, TrackKind};

    /// Serialize then deserialize a wire value; panic with the offending JSON if it doesn't survive.
    /// Every type crossing the daemon↔client socket must pass — a field collision or renamed variant
    /// otherwise breaks the stream *silently*. (Track-variant coverage lives with the model, in
    /// `timeline`; here we guard the envelopes.)
    fn survives<T: serde::Serialize + serde::de::DeserializeOwned>(value: &T) {
        let json = serde_json::to_string(value).unwrap();
        serde_json::from_str::<T>(&json)
            .unwrap_or_else(|error| panic!("does not round-trip: {json}\n  {error}"));
    }

    #[test]
    fn every_command_survives_the_request_wire() {
        let target = Some("main".to_owned());
        let commands = [
            Command::Ping,
            Command::Status,
            Command::Targets,
            Command::TargetList,
            Command::Detach,
            Command::Tail(TimelineQuery {
                since_ms: 5000,
                tracks: Some(vec![TrackKind::Network]),
                source: Some(Source::Main),
                target: Some("workspace".to_owned()),
                grep: Some("extension".to_owned()),
                extension: Some("modular.local-sdk-view-showcase".to_owned()),
                limit: Some(25),
            }),
            Command::Errors {
                query: TimelineQuery {
                    since_ms: 60_000,
                    tracks: None,
                    source: None,
                    target: None,
                    grep: None,
                    extension: None,
                    limit: None,
                },
                explain: true,
            },
            Command::Eval { target: target.clone(), expr: "1+1".to_owned() },
            Command::Ready { target: target.clone() },
            Command::Heap { target: target.clone() },
            Command::Snap { target: target.clone(), interactive: true },
            Command::Click { target: target.clone(), reference: "e1".to_owned() },
            Command::Fill {
                target: target.clone(),
                reference: "e1".to_owned(),
                text: "x".to_owned(),
            },
            Command::Lens {
                target: target.clone(),
                source: "return 1".to_owned(),
                args: vec!["a".to_owned()],
            },
            Command::ExtensionBundle {
                target,
                source: "return 1".to_owned(),
                extension_id: "modular.example".to_owned(),
                query: TimelineQuery {
                    since_ms: 30_000,
                    tracks: None,
                    source: None,
                    target: None,
                    grep: None,
                    extension: Some("modular.example".to_owned()),
                    limit: Some(200),
                },
            },
            Command::Ignore(IgnoreOp::Add("noise".to_owned())),
            Command::Subscribe { since_ms: 30_000 },
        ];
        for command in commands {
            survives(&Query { command, json: false });
        }
    }

    #[test]
    fn response_envelopes_survive_the_wire() {
        survives(&Reply::ok("done"));
        survives(&TargetActivity {
            label: "workspace".to_owned(),
            kind: TargetKind::Page,
            title: "Workspace".to_owned(),
            url: "app://x".to_owned(),
            events: 42,
            extension_id: None,
            purpose: None,
        });
        // A Log event is the collision-prone one; wrapping it in both Frame shapes guards the
        // envelope without re-testing every track (that's `timeline`'s job).
        let event = TimelineEvent {
            at_ms: 1,
            source: Source::Renderer,
            target: "t".to_owned(),
            track: Track::Log(LogEntry {
                level: "info".to_owned(),
                source: "network".to_owned(),
                text: "x".to_owned(),
                url: None,
                line: None,
            }),
        };
        survives(&Frame::Backfill(vec![event.clone()]));
        survives(&Frame::Event(event));
    }
}
