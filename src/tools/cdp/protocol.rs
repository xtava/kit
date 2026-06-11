//! The wire between the thin client and the warm Attachment daemon, newline-delimited JSON over a
//! unix socket. Two shapes:
//!
//! - **Request/reply** — one [`Query`] in, one [`Reply`] out. The daemon renders the output, so the
//!   one-shot CLI never deserializes a Timeline.
//! - **Subscription** — a [`Command::Subscribe`] in, then a [`Frame`] *stream* out that stays open.
//!   Frames carry structured [`TimelineEvent`]s; the interactive client renders them itself.

use serde::{Deserialize, Serialize};

use crate::cdp::{Source, TargetKind, TimelineEvent, TrackKind};

use super::snapshot;

/// How an interaction names its element: a `@eN` ref from the last snap, or a `role:name` query
/// resolved against a fresh accessibility snapshot at execution time. Refs are fast but
/// document-scoped; queries survive navigation and re-renders.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Locator {
    Ref(String),
    Query { role: Option<String>, name: String },
}

impl Locator {
    /// Parse the CLI grammar: `@e5` / `e5` / `5` → ref; `button:Save` → role-scoped query; bare
    /// text → query across every ref-bearing role. A `:` prefix that isn't a known AX role stays
    /// part of the name, so `click 'Save: all'` still means what it says.
    pub fn parse(text: &str) -> Self {
        let trimmed = text.trim();
        if let Some(rest) = trimmed.strip_prefix('@') {
            return Self::Ref(normalize_ref(rest));
        }
        if is_ref_shaped(trimmed) {
            return Self::Ref(normalize_ref(trimmed));
        }
        if let Some((prefix, name)) = trimmed.split_once(':') {
            if snapshot::is_known_role(prefix.trim()) {
                return Self::Query {
                    role: Some(prefix.trim().to_lowercase()),
                    name: name.trim().to_owned(),
                };
            }
        }
        Self::Query { role: None, name: trimmed.to_owned() }
    }
}

fn is_ref_shaped(text: &str) -> bool {
    let digits = text.strip_prefix('e').unwrap_or(text);
    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
}

fn normalize_ref(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('e') {
        trimmed.to_owned()
    } else {
        format!("e{trimmed}")
    }
}

/// How long an interaction waits for its consequences: Timeline quiet for `idle_ms` ends the
/// window early; `timeout_ms` caps it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Settle {
    pub idle_ms: u64,
    pub timeout_ms: u64,
}

/// A parsed key chord (`Enter`, `Meta+s`) — parsed client-side so a typo fails before the wire.
/// `modifiers` uses the CDP bitmask: Alt 1, Ctrl 2, Meta 4, Shift 8.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyChord {
    pub modifiers: u8,
    pub key: String,
}

impl KeyChord {
    pub fn parse(text: &str) -> Result<Self, String> {
        let parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let (key, modifier_parts) = parts.split_last().ok_or("empty key")?;
        let mut modifiers = 0u8;
        for part in modifier_parts {
            modifiers |= match part.to_lowercase().as_str() {
                "alt" | "option" => 1,
                "ctrl" | "control" => 2,
                "meta" | "cmd" | "command" => 4,
                "shift" => 8,
                other => return Err(format!("unknown modifier '{other}'")),
            };
        }
        let key = canonical_key(key)?;
        Ok(Self { modifiers, key })
    }
}

/// The named keys `press` understands — shared with completion so the offered list is the
/// accepted list.
pub(crate) const NAMED_KEYS: &[&str] = &[
    "Enter",
    "Tab",
    "Escape",
    "Backspace",
    "Delete",
    "Space",
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
    "Home",
    "End",
    "PageUp",
    "PageDown",
];

/// Canonicalize a key name; anything not in [`NAMED_KEYS`] must be a single character.
fn canonical_key(key: &str) -> Result<String, String> {
    if let Some(name) = NAMED_KEYS.iter().find(|name| name.eq_ignore_ascii_case(key)) {
        return Ok((*name).to_owned());
    }
    if key.chars().count() == 1 {
        return Ok(key.to_owned());
    }
    Err(format!("unknown key '{key}' — named keys: {}", NAMED_KEYS.join(", ")))
}

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
    /// Agent-safe Timeline compression: errors stay grouped with integrity facts, repeated non-error
    /// rows collapse to counted groups, a short raw tail anchors recency, and omissions are counted.
    Brief {
        query: TimelineQuery,
        tail: usize,
        groups: usize,
    },
    /// Error-shaped Timeline events, deduplicated into counted groups — the low-context "what's
    /// broken" view. Same filters as [`Command::Tail`]; the daemon collapses duplicates and reports
    /// the integrity facts (variants absorbed, ring eviction, undecoded events) alongside them.
    Errors {
        query: TimelineQuery,
        /// Expand each group's absorbed variants instead of one representative line — the audit view.
        explain: bool,
        /// Force-load source maps for the error frames first (enabling Debugger where needed),
        /// so stacks resolve to original files even on a cold registry.
        resolve: bool,
    },
    Navigate {
        target: Option<String>,
        url: String,
    },
    Configure(LaunchSettings),
    LaunchLog,
    State {
        visual: bool,
    },
    Mark {
        name: String,
    },
    After {
        mark: String,
        idle_ms: u64,
        timeout_ms: u64,
    },
    Bundle {
        since: Option<String>,
        include: Vec<String>,
        include_secrets: bool,
    },
    Net(NetCommand),
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
        /// Diff against the previous explicit snap instead of printing the tree; this snap
        /// becomes the new baseline either way.
        diff: bool,
    },
    Click {
        target: Option<String>,
        locator: Locator,
        settle: Option<Settle>,
    },
    Fill {
        target: Option<String>,
        locator: Locator,
        text: String,
        settle: Option<Settle>,
    },
    /// Press a key chord into the focused element of a Target.
    Press {
        target: Option<String>,
        chord: KeyChord,
        settle: Option<Settle>,
    },
    /// Choose a select/combobox option by visible label or value.
    Select {
        target: Option<String>,
        locator: Locator,
        option: String,
        settle: Option<Settle>,
    },
    /// Run steps sequentially in the daemon, stopping at the first failure. One round trip for a
    /// whole interaction sequence; every step's evidence lands on the same Timeline.
    Do {
        steps: Vec<Step>,
    },
    /// Block until a JS expression evaluates truthy in a Target, polling until `timeout_ms`.
    WaitFor {
        target: Option<String>,
        expr: String,
        timeout_ms: u64,
    },
    /// Assert one condition against the live app, polling until satisfied or `within_ms` elapses.
    Expect {
        expectation: Expectation,
        within_ms: u64,
    },
    /// Composite verdict: document ready, no error-shaped events, no failed requests in the
    /// window. `window: None` means "since the last interaction, or the last 30s if none".
    Verify {
        target: Option<String>,
        window: Option<TimelineQuery>,
    },
    /// Manage value subscriptions: daemon-side pollers that record a `watch` Timeline event
    /// whenever an expression's value changes.
    Watch(WatchOp),
    /// Manage instrumentation points: live function wrappers that record a `trace` Timeline
    /// event on every call — args, outcome, duration — without pausing or rebuilding.
    Trace(TraceOp),
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
    /// The stored element refs from the target's last accessibility snapshot, as JSON — the
    /// completion data source for locator positions.
    Refs {
        target: Option<String>,
    },
    /// Open a live subscription: the daemon replies with a [`Frame`] stream (not a [`Reply`]) and
    /// holds the socket open, pushing each new Timeline event until the client disconnects.
    Subscribe {
        since_ms: u64,
    },
    CloseBrowser,
    Detach,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineQuery {
    pub since_ms: u64,
    pub since_mark: Option<String>,
    pub tracks: Option<Vec<TrackKind>>,
    pub source: Option<Source>,
    pub target: Option<String>,
    pub grep: Option<String>,
    pub extension: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchSettings {
    pub viewport: Option<String>,
    pub timezone: Option<String>,
    pub locale: Option<String>,
    pub dark: bool,
    pub offline: bool,
    pub throttle: Option<String>,
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

/// One step of a [`Command::Do`] batch: the source line as the user wrote it (for reporting) plus
/// its parsed command. Steps are parsed client-side so the daemon never sees raw text.
#[derive(Debug, Serialize, Deserialize)]
pub struct Step {
    pub line: String,
    pub command: Box<Command>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum WatchOp {
    Add { name: String, target: Option<String>, expr: String, interval_ms: u64 },
    Ls,
    Rm { name: String },
    Clear,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum TraceOp {
    /// Wrap the live function at a dotted `path` (`app.api.save`) in the resolved Target.
    Fn {
        name: Option<String>,
        target: Option<String>,
        path: String,
        rate: u64,
    },
    /// Logpoint: a never-pausing breakpoint at `location` (`file.js:line[:col]`) that records
    /// `expr` (gated by `when`) on every pass.
    Logpoint {
        name: Option<String>,
        target: Option<String>,
        location: String,
        expr: Option<String>,
        when: Option<String>,
        rate: u64,
    },
    /// Search the resolved Target's parsed scripts for a literal string — live `url:line`
    /// coordinates for `Logpoint`.
    Find {
        target: Option<String>,
        text: String,
    },
    Ls,
    Rm {
        name: String,
    },
    Clear,
}

/// One assertable condition for [`Command::Expect`].
#[derive(Debug, Serialize, Deserialize)]
pub enum Expectation {
    /// The rendered page text contains `needle` (case-insensitive).
    Text { target: Option<String>, needle: String },
    /// An eval result satisfies the check: `equals` (JSON compare), `contains` (rendered text),
    /// or plain truthiness when neither is given.
    Eval { target: Option<String>, expr: String, equals: Option<String>, contains: Option<String> },
    /// The Timeline window holds a network response whose URL contains `pattern` and whose status
    /// matches `status` (`2xx`, `404`, `ok`, `fail`; default any).
    Net { pattern: String, status: Option<String>, query: TimelineQuery },
    /// The Timeline window holds no error-shaped events.
    NoErrors { query: TimelineQuery },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum NetCommand {
    Failed { query: TimelineQuery },
    Slow { query: TimelineQuery },
    Show { request_id: String },
    Block { pattern: String },
    Mock { method: String, pattern: String, body: String, status: u16, mime: Option<String> },
    Rules,
    RulesClear,
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
            Command::CloseBrowser,
            Command::Detach,
            Command::Tail(TimelineQuery {
                since_ms: 5000,
                tracks: Some(vec![TrackKind::Network]),
                source: Some(Source::Main),
                target: Some("workspace".to_owned()),
                since_mark: None,
                grep: Some("extension".to_owned()),
                extension: Some("modular.local-sdk-view-showcase".to_owned()),
                limit: Some(25),
            }),
            Command::Brief {
                query: TimelineQuery {
                    since_ms: 5000,
                    tracks: None,
                    source: None,
                    target: None,
                    since_mark: None,
                    grep: None,
                    extension: None,
                    limit: Some(100),
                },
                tail: 12,
                groups: 8,
            },
            Command::Errors {
                query: TimelineQuery {
                    since_ms: 60_000,
                    tracks: None,
                    source: None,
                    target: None,
                    since_mark: None,
                    grep: None,
                    extension: None,
                    limit: None,
                },
                explain: true,
                resolve: true,
            },
            Command::Navigate { target: target.clone(), url: "http://localhost:3000".to_owned() },
            Command::Configure(LaunchSettings {
                viewport: Some("1440x1000".to_owned()),
                timezone: Some("America/New_York".to_owned()),
                locale: Some("fr-FR".to_owned()),
                dark: true,
                offline: false,
                throttle: Some("slow-3g".to_owned()),
            }),
            Command::LaunchLog,
            Command::State { visual: true },
            Command::Mark { name: "before".to_owned() },
            Command::After { mark: "before".to_owned(), idle_ms: 500, timeout_ms: 5000 },
            Command::Bundle {
                since: Some("before".to_owned()),
                include: vec!["har".to_owned()],
                include_secrets: false,
            },
            Command::Net(NetCommand::Failed {
                query: TimelineQuery {
                    since_ms: 10_000,
                    tracks: Some(vec![TrackKind::Network]),
                    source: None,
                    target: None,
                    since_mark: Some("before".to_owned()),
                    grep: None,
                    extension: None,
                    limit: None,
                },
            }),
            Command::Net(NetCommand::Slow {
                query: TimelineQuery {
                    since_ms: 10_000,
                    tracks: Some(vec![TrackKind::Network]),
                    source: None,
                    target: None,
                    since_mark: None,
                    grep: Some("/api".to_owned()),
                    extension: None,
                    limit: Some(20),
                },
            }),
            Command::Net(NetCommand::Show { request_id: "req_1".to_owned() }),
            Command::Eval { target: target.clone(), expr: "1+1".to_owned() },
            Command::Ready { target: target.clone() },
            Command::Heap { target: target.clone() },
            Command::Snap { target: target.clone(), interactive: true, diff: true },
            Command::Click {
                target: target.clone(),
                locator: Locator::Ref("e1".to_owned()),
                settle: Some(Settle { idle_ms: 300, timeout_ms: 2000 }),
            },
            Command::Fill {
                target: target.clone(),
                locator: Locator::Query {
                    role: Some("textbox".to_owned()),
                    name: "Name".to_owned(),
                },
                text: "x".to_owned(),
                settle: None,
            },
            Command::Lens {
                target: target.clone(),
                source: "return 1".to_owned(),
                args: vec!["a".to_owned()],
            },
            Command::ExtensionBundle {
                target: target.clone(),
                source: "return 1".to_owned(),
                extension_id: "modular.example".to_owned(),
                query: TimelineQuery {
                    since_ms: 30_000,
                    tracks: None,
                    source: None,
                    target: None,
                    since_mark: None,
                    grep: None,
                    extension: Some("modular.example".to_owned()),
                    limit: Some(200),
                },
            },
            Command::Ignore(IgnoreOp::Add("noise".to_owned())),
            Command::Refs { target: Some("main".to_owned()) },
            Command::WaitFor {
                target: target.clone(),
                expr: "!document.querySelector('.spinner')".to_owned(),
                timeout_ms: 5_000,
            },
            Command::Expect {
                expectation: Expectation::Text {
                    target: target.clone(),
                    needle: "Saved".to_owned(),
                },
                within_ms: 2_000,
            },
            Command::Expect {
                expectation: Expectation::Net {
                    pattern: "/api/save".to_owned(),
                    status: Some("2xx".to_owned()),
                    query: TimelineQuery {
                        since_ms: 10_000,
                        since_mark: Some("last-action".to_owned()),
                        tracks: None,
                        source: None,
                        target: None,
                        grep: None,
                        extension: None,
                        limit: None,
                    },
                },
                within_ms: 5_000,
            },
            Command::Verify { target: target.clone(), window: None },
            Command::Press {
                target: target.clone(),
                chord: KeyChord { modifiers: 4, key: "s".to_owned() },
                settle: None,
            },
            Command::Select {
                target,
                locator: Locator::Query {
                    role: Some("combobox".to_owned()),
                    name: "Flavor".to_owned(),
                },
                option: "chocolate".to_owned(),
                settle: Some(Settle { idle_ms: 300, timeout_ms: 2000 }),
            },
            Command::Watch(WatchOp::Add {
                name: "cart".to_owned(),
                target: None,
                expr: "document.querySelectorAll('.cart-item').length".to_owned(),
                interval_ms: 300,
            }),
            Command::Trace(TraceOp::Fn {
                name: Some("save".to_owned()),
                target: None,
                path: "app.api.save".to_owned(),
                rate: 20,
            }),
            Command::Trace(TraceOp::Logpoint {
                name: None,
                target: None,
                location: "renderer.js:93".to_owned(),
                expr: Some("({ counter })".to_owned()),
                when: Some("counter > 2".to_owned()),
                rate: 20,
            }),
            Command::Trace(TraceOp::Find {
                target: None,
                text: "groupId: options.group.id".to_owned(),
            }),
            Command::Do {
                steps: vec![Step {
                    line: "click 'button:Save'".to_owned(),
                    command: Box::new(Command::Click {
                        target: None,
                        locator: Locator::Query {
                            role: Some("button".to_owned()),
                            name: "Save".to_owned(),
                        },
                        settle: Some(Settle { idle_ms: 300, timeout_ms: 2000 }),
                    }),
                }],
            },
            Command::Subscribe { since_ms: 30_000 },
        ];
        for command in commands {
            survives(&Query { command, json: false });
        }
    }

    #[test]
    fn key_chords_parse_modifiers_and_reject_typos() {
        let enter = KeyChord::parse("Enter").unwrap();
        assert_eq!((enter.modifiers, enter.key.as_str()), (0, "Enter"));
        let save = KeyChord::parse("Meta+s").unwrap();
        assert_eq!((save.modifiers, save.key.as_str()), (4, "s"));
        let all = KeyChord::parse("ctrl+shift+escape").unwrap();
        assert_eq!((all.modifiers, all.key.as_str()), (10, "Escape"));
        assert!(KeyChord::parse("Enterr").is_err());
        assert!(KeyChord::parse("Hyper+x").is_err());
    }

    #[test]
    fn locator_grammar_separates_refs_roles_and_bare_names() {
        assert!(matches!(Locator::parse("@e5"), Locator::Ref(reference) if reference == "e5"));
        assert!(matches!(Locator::parse("e12"), Locator::Ref(reference) if reference == "e12"));
        assert!(matches!(Locator::parse("@7"), Locator::Ref(reference) if reference == "e7"));
        assert!(matches!(
            Locator::parse("button:Save settings"),
            Locator::Query { role: Some(role), name } if role == "button" && name == "Save settings"
        ));
        assert!(matches!(
            Locator::parse("Save settings"),
            Locator::Query { role: None, name } if name == "Save settings"
        ));
        // A colon after a non-role prefix is part of the name, not a role separator.
        assert!(matches!(
            Locator::parse("Save: all documents"),
            Locator::Query { role: None, name } if name == "Save: all documents"
        ));
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
