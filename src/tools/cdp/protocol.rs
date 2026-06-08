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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::{
        ConsoleLine, ExceptionInfo, LogEntry, NetEvent, NetPhase, Source, TargetKind, TimelineEvent,
        Track, TrackKind, WsDir, WsFrame,
    };

    /// Serialize then deserialize a wire value; panic with the offending JSON if it doesn't survive.
    /// Every type that crosses the daemon↔client socket must pass this — a flattened field collision
    /// or a renamed variant otherwise breaks the stream *silently*.
    fn survives<T: serde::Serialize + serde::de::DeserializeOwned>(value: &T) {
        let json = serde_json::to_string(value).unwrap();
        serde_json::from_str::<T>(&json)
            .unwrap_or_else(|error| panic!("does not round-trip: {json}\n  {error}"));
    }

    fn query(command: Command) -> Query {
        Query { command, json: false }
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
            Command::Tail { since_ms: 5000, tracks: Some(vec![TrackKind::Network]), source: Some(Source::Main) },
            Command::Eval { target: target.clone(), expr: "1+1".to_owned() },
            Command::Heap { target: target.clone() },
            Command::Snap { target: target.clone(), interactive: true },
            Command::Click { target: target.clone(), reference: "e1".to_owned() },
            Command::Fill { target: target.clone(), reference: "e1".to_owned(), text: "x".to_owned() },
            Command::Lens { target, source: "return 1".to_owned(), args: vec!["a".to_owned()] },
            Command::Ignore(IgnoreOp::Add("noise".to_owned())),
            Command::Ignore(IgnoreOp::List),
            Command::Ignore(IgnoreOp::Clear),
            Command::Subscribe { since_ms: 30_000 },
        ];
        for command in commands {
            survives(&query(command));
        }
    }

    #[test]
    fn reply_and_target_activity_survive() {
        survives(&Reply::ok("done"));
        survives(&Reply::fail("nope"));
        survives(&TargetActivity {
            label: "workspace".to_owned(),
            kind: TargetKind::Page,
            title: "Workspace".to_owned(),
            url: "app://x".to_owned(),
            events: 42,
        });
    }

    /// The response wire: a backfill carrying *every* track variant — the precise frame whose decode
    /// failure once emptied the live timeline.
    #[test]
    fn a_backfill_of_all_track_variants_survives() {
        let event = |track| TimelineEvent { at_ms: 1, source: Source::Renderer, target: "t".to_owned(), track };
        let backfill = Frame::Backfill(vec![
            event(Track::Console(ConsoleLine { level: "log".to_owned(), text: "x".to_owned(), url: None, line: None })),
            event(Track::Exception(ExceptionInfo { text: "boom".to_owned(), url: None, line: None })),
            event(Track::Log(LogEntry { level: "info".to_owned(), source: "network".to_owned(), text: "x".to_owned(), url: None, line: None })),
            event(Track::Network(NetEvent { phase: NetPhase::Response, request_id: "1".to_owned(), method: None, url: None, status: Some(200), mime: None, error: None })),
            event(Track::Ws(WsFrame { dir: WsDir::Sent, opcode: Some(1), len: Some(8), preview: None, url: None })),
        ]);
        survives(&backfill);
    }
}
