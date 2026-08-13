use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use super::client::SessionId;

const CONFIRMED_IDLE_OBSERVATIONS: u8 = 3;
const CONFIRMED_IDENTITY_MISSES: u8 = 6;
const IDENTIFIED_INSPECTION_INTERVAL: Duration = Duration::from_millis(300);
const UNIDENTIFIED_INSPECTION_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentKind {
    Claude,
    Codex,
}

impl AgentKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentActivity {
    Unknown,
    Idle,
    Working,
    NeedsAttention,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentDetection {
    pub kind: AgentKind,
    pub activity: AgentActivity,
    visible_idle: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AgentPresentation {
    pub kind: AgentKind,
    pub activity: AgentActivity,
    pub seen: bool,
}

pub struct AgentEvidence<'a> {
    pub foreground_process_name: Option<&'a str>,
    pub title: &'a str,
    pub screen: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DetectionUpdate {
    Preserve,
    Replace(Option<AgentDetection>),
}

pub fn detect(evidence: AgentEvidence<'_>) -> DetectionUpdate {
    let title = evidence.title.trim();
    let screen = evidence.screen.to_ascii_lowercase();
    let Some(kind) = identify_agent(evidence.foreground_process_name, title, &screen) else {
        return DetectionUpdate::Replace(None);
    };
    if preserves_previous_state(kind, &screen) {
        return DetectionUpdate::Preserve;
    }
    let (activity, visible_idle) = match kind {
        AgentKind::Claude => detect_claude(title, &screen),
        AgentKind::Codex => detect_codex(title, &screen),
    };
    DetectionUpdate::Replace(Some(AgentDetection { kind, activity, visible_idle }))
}

fn preserves_previous_state(kind: AgentKind, screen: &str) -> bool {
    match kind {
        AgentKind::Claude => {
            (screen.contains("showing detailed transcript")
                && contains_any(screen, &["ctrl+o", "ctrl+e", "↑↓ scroll", "? for shortcuts"]))
                || (screen.contains("select model")
                    && screen.contains("enter to set as default")
                    && screen.contains("esc to cancel"))
        }
        AgentKind::Codex => {
            contains_any(screen, &["↑/↓ to scroll", "pgup/pgdn to", "home/end to jump"])
                && contains_any(screen, &["esc to edit prev", "esc/← to edit prev"])
        }
    }
}

fn identify_agent(process_name: Option<&str>, title: &str, screen: &str) -> Option<AgentKind> {
    if let Some(process_name) = process_name {
        let process = Path::new(process_name)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(process_name)
            .trim_end_matches(".exe");
        let mut tokens = process.split(|character: char| !character.is_ascii_alphanumeric());
        if tokens.clone().any(|token| token.eq_ignore_ascii_case("claude")) {
            return Some(AgentKind::Claude);
        }
        if tokens.any(|token| token.eq_ignore_ascii_case("codex")) {
            return Some(AgentKind::Codex);
        }
    }

    if title.contains("Action Required") {
        return Some(AgentKind::Codex);
    }
    if title.starts_with('✳') {
        return Some(AgentKind::Claude);
    }
    if title.chars().next().is_some_and(is_braille) {
        return Some(if screen.contains("esc to interrupt") {
            AgentKind::Codex
        } else {
            AgentKind::Claude
        });
    }
    None
}

fn detect_claude(title: &str, screen: &str) -> (AgentActivity, bool) {
    let bottom = bottom_non_empty_lines(screen, 5);
    if bottom.contains("/btw") && bottom.contains("esc to close") {
        return (AgentActivity::Working, false);
    }
    let live = after_last_horizontal_rule(screen);
    let blocked = contains_any(
        live,
        &[
            "waiting for permission",
            "do you want to allow this connection?",
            "tab to amend",
            "ctrl+e to explain",
            "review your answers",
            "run a dynamic workflow?",
        ],
    ) || (live.contains("do you want to proceed?") && live.contains("esc to cancel"))
        || (live.contains("enter to select") && live.contains("esc to cancel"));
    if blocked {
        return (AgentActivity::NeedsAttention, false);
    }
    if title.chars().next().is_some_and(is_braille) {
        return (AgentActivity::Working, false);
    }
    let visible_idle = title.starts_with('✳')
        || screen.lines().rev().take(8).any(|line| line.trim_start().starts_with('❯'));
    if visible_idle {
        (AgentActivity::Idle, true)
    } else {
        (AgentActivity::Unknown, false)
    }
}

fn detect_codex(title: &str, screen: &str) -> (AgentActivity, bool) {
    let live = after_last_codex_prompt(screen);
    let blocked = title.contains("Action Required")
        || contains_any(
            live,
            &[
                "press enter to confirm or esc to cancel",
                "enter to submit answer",
                "enter to submit all",
                "allow command?",
                "[y/n]",
                "yes (y)",
            ],
        );
    if blocked {
        return (AgentActivity::NeedsAttention, false);
    }
    let title_has_spinner = title.chars().any(is_braille);
    let screen_has_working = screen
        .lines()
        .rev()
        .take(5)
        .any(|line| line.contains("working (") && line.contains("esc to interrupt"));
    if title_has_spinner || screen_has_working {
        return (AgentActivity::Working, false);
    }
    if !title.is_empty() {
        (AgentActivity::Idle, false)
    } else {
        (AgentActivity::Unknown, false)
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn after_last_codex_prompt(screen: &str) -> &str {
    after_last_matching_line(screen, |line| line == "›" || line.starts_with("› "))
}

fn after_last_horizontal_rule(screen: &str) -> &str {
    after_last_matching_line(screen, |line| {
        let line = line.trim();
        !line.is_empty() && line.chars().all(|character| character == '─')
    })
}

fn after_last_matching_line(screen: &str, mut predicate: impl FnMut(&str) -> bool) -> &str {
    let mut result = screen;
    let mut offset = 0;
    for segment in screen.split_inclusive('\n') {
        if predicate(segment.trim_end_matches('\n')) {
            result = &screen[(offset + segment.len()).min(screen.len())..];
        }
        offset += segment.len();
    }
    result
}

fn bottom_non_empty_lines(screen: &str, count: usize) -> &str {
    let mut start = screen.len();
    let mut remaining = count;
    for (index, _) in screen.rmatch_indices('\n') {
        let line = &screen[index + 1..start];
        if !line.trim().is_empty() {
            remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                return &screen[index + 1..];
            }
        }
        start = index;
    }
    screen
}

fn is_braille(character: char) -> bool {
    ('\u{2800}'..='\u{28ff}').contains(&character)
}

#[derive(Clone, Copy, Debug)]
struct TrackedAgent {
    kind: AgentKind,
    activity: AgentActivity,
    seen: bool,
    pending_idle_observations: u8,
}

pub struct AgentFingerprint<'a> {
    pub content_sequence: Option<usize>,
    pub foreground_process_name: Option<&'a str>,
    pub title: &'a str,
}

pub struct ActivityObservation {
    pub presentation: Option<AgentPresentation>,
    pub revisit: bool,
    pub transition: Option<AgentTransition>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentTransition {
    Ready(AgentKind),
}

struct TrackedSession {
    evidence_initialized: bool,
    content_sequence: Option<usize>,
    foreground_process_name: Option<String>,
    title: String,
    last_inspection: Option<Instant>,
    missing_identity_observations: u8,
    detection: Option<AgentDetection>,
    agent: Option<TrackedAgent>,
}

#[derive(Default)]
pub struct ActivityTracker {
    sessions: HashMap<SessionId, TrackedSession>,
}

impl ActivityTracker {
    pub fn clear(&mut self) {
        self.sessions.clear();
    }

    pub fn observe_with(
        &mut self,
        session_id: SessionId,
        fingerprint: AgentFingerprint<'_>,
        selected: bool,
        now: Instant,
        detect: impl FnOnce() -> DetectionUpdate,
    ) -> ActivityObservation {
        let session = self.sessions.entry(session_id).or_insert_with(|| TrackedSession {
            evidence_initialized: false,
            content_sequence: fingerprint.content_sequence,
            foreground_process_name: fingerprint.foreground_process_name.map(str::to_owned),
            title: fingerprint.title.to_owned(),
            last_inspection: None,
            missing_identity_observations: 0,
            detection: None,
            agent: None,
        });
        let identity_changed = session.foreground_process_name.as_deref()
            != fingerprint.foreground_process_name
            || session.title != fingerprint.title;
        let content_changed = session.content_sequence != fingerprint.content_sequence;
        let inspection_interval = if session.detection.is_some() {
            IDENTIFIED_INSPECTION_INTERVAL
        } else {
            UNIDENTIFIED_INSPECTION_INTERVAL
        };
        let inspection_due = session
            .last_inspection
            .is_none_or(|last| now.saturating_duration_since(last) >= inspection_interval);
        let must_inspect = !session.evidence_initialized
            || identity_changed
            || ((content_changed || session.missing_identity_observations > 0) && inspection_due);
        let mut revisit =
            (content_changed || session.missing_identity_observations > 0) && !must_inspect;
        if must_inspect {
            session.evidence_initialized = true;
            session.content_sequence = fingerprint.content_sequence;
            session.foreground_process_name =
                fingerprint.foreground_process_name.map(str::to_owned);
            fingerprint.title.clone_into(&mut session.title);
            session.last_inspection = Some(now);
            match detect() {
                DetectionUpdate::Preserve => {
                    session.missing_identity_observations = 0;
                }
                DetectionUpdate::Replace(Some(detection)) => {
                    session.missing_identity_observations = 0;
                    session.detection = Some(detection);
                }
                DetectionUpdate::Replace(None) if session.detection.is_some() => {
                    session.missing_identity_observations =
                        session.missing_identity_observations.saturating_add(1);
                    if session.missing_identity_observations >= CONFIRMED_IDENTITY_MISSES {
                        session.missing_identity_observations = 0;
                        session.detection = None;
                    } else {
                        revisit = true;
                    }
                }
                DetectionUpdate::Replace(None) => {
                    session.missing_identity_observations = 0;
                    session.detection = None;
                }
            }
        }

        let Some(detection) = session.detection else {
            session.agent = None;
            return ActivityObservation { presentation: None, revisit, transition: None };
        };
        let mut transition = None;
        let tracked = session.agent.get_or_insert(TrackedAgent {
            kind: detection.kind,
            activity: detection.activity,
            seen: true,
            pending_idle_observations: 0,
        });

        if tracked.kind != detection.kind {
            *tracked = TrackedAgent {
                kind: detection.kind,
                activity: detection.activity,
                seen: true,
                pending_idle_observations: 0,
            };
        } else if should_hold_idle(*tracked, detection) {
            tracked.pending_idle_observations = tracked.pending_idle_observations.saturating_add(1);
            if tracked.pending_idle_observations >= CONFIRMED_IDLE_OBSERVATIONS {
                transition = apply_activity(tracked, detection.activity, selected);
            } else {
                revisit = true;
            }
        } else {
            transition = apply_activity(tracked, detection.activity, selected);
        }

        if selected {
            tracked.seen = true;
        }
        ActivityObservation {
            presentation: Some(AgentPresentation {
                kind: tracked.kind,
                activity: tracked.activity,
                seen: tracked.seen,
            }),
            revisit,
            transition,
        }
    }

    pub fn retain(&mut self, mut is_live: impl FnMut(SessionId) -> bool) {
        self.sessions.retain(|session_id, _| is_live(*session_id));
    }
}

fn should_hold_idle(tracked: TrackedAgent, detection: AgentDetection) -> bool {
    tracked.activity == AgentActivity::Working
        && detection.activity == AgentActivity::Idle
        && !detection.visible_idle
        && tracked.pending_idle_observations < CONFIRMED_IDLE_OBSERVATIONS
}

fn apply_activity(
    tracked: &mut TrackedAgent,
    activity: AgentActivity,
    selected: bool,
) -> Option<AgentTransition> {
    let previous = tracked.activity;
    tracked.activity = activity;
    tracked.pending_idle_observations = 0;
    tracked.seen = match activity {
        AgentActivity::NeedsAttention => false,
        AgentActivity::Working | AgentActivity::Unknown => true,
        AgentActivity::Idle if previous != AgentActivity::Idle => selected,
        AgentActivity::Idle => tracked.seen || selected,
    };
    (previous == AgentActivity::Working && activity == AgentActivity::Idle)
        .then_some(AgentTransition::Ready(tracked.kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence<'a>(process: &'a str, title: &'a str, screen: &'a str) -> AgentEvidence<'a> {
        AgentEvidence { foreground_process_name: Some(process), title, screen }
    }

    fn detected(evidence: AgentEvidence<'_>) -> Option<AgentDetection> {
        match detect(evidence) {
            DetectionUpdate::Replace(detection) => detection,
            DetectionUpdate::Preserve => panic!("fixture unexpectedly preserved prior state"),
        }
    }

    fn observe(
        tracker: &mut ActivityTracker,
        session_id: SessionId,
        content_sequence: usize,
        detection: Option<AgentDetection>,
        selected: bool,
    ) -> Option<AgentPresentation> {
        observe_result(tracker, session_id, content_sequence, detection, selected).presentation
    }

    fn observe_result(
        tracker: &mut ActivityTracker,
        session_id: SessionId,
        content_sequence: usize,
        detection: Option<AgentDetection>,
        selected: bool,
    ) -> ActivityObservation {
        let now = Instant::now() + Duration::from_secs(content_sequence as u64);
        tracker.observe_with(
            session_id,
            AgentFingerprint {
                content_sequence: Some(content_sequence),
                foreground_process_name: Some("agent"),
                title: "agent",
            },
            selected,
            now,
            || DetectionUpdate::Replace(detection),
        )
    }

    #[test]
    fn codex_action_required_is_attention() {
        let detection = detected(evidence("/opt/codex", "Action Required", "allow command?"));
        assert_eq!(
            detection,
            Some(AgentDetection {
                kind: AgentKind::Codex,
                activity: AgentActivity::NeedsAttention,
                visible_idle: false,
            })
        );
    }

    #[test]
    fn claude_prompt_is_explicit_idle() {
        let detection = detected(evidence("claude", "✳ project", "  ❯ "));
        assert_eq!(
            detection,
            Some(AgentDetection {
                kind: AgentKind::Claude,
                activity: AgentActivity::Idle,
                visible_idle: true,
            })
        );
    }

    #[test]
    fn stale_transcript_without_agent_evidence_is_ignored() {
        let detection = detected(AgentEvidence {
            foreground_process_name: Some("zsh"),
            title: "project",
            screen: "previous output: allow command?",
        });
        assert_eq!(detection, None);
    }

    #[test]
    fn foreground_process_identifies_claude_with_a_generic_title() {
        let detection = detected(evidence("/opt/bin/Claude", "project", "  ❯ "));
        assert_eq!(
            detection.map(|detection| (detection.kind, detection.activity)),
            Some((AgentKind::Claude, AgentActivity::Idle))
        );
    }

    #[test]
    fn foreground_process_identifies_codex_with_a_generic_title() {
        let detection =
            detected(evidence("/opt/bin/CODEX", "project", "working (2s) esc to interrupt"));
        assert_eq!(
            detection.map(|detection| (detection.kind, detection.activity)),
            Some((AgentKind::Codex, AgentActivity::Working))
        );
    }

    #[test]
    fn generic_codex_title_is_not_explicit_completion_evidence() {
        let detection = detected(evidence("codex", "project", "")).unwrap();
        assert_eq!(detection.activity, AgentActivity::Idle);
        assert!(!detection.visible_idle);
    }

    #[test]
    fn transcript_and_model_overlays_preserve_previous_state() {
        assert_eq!(
            detect(evidence(
                "codex",
                "project",
                "↑/↓ to scroll · pgup/pgdn to move · esc to edit prev",
            )),
            DetectionUpdate::Preserve
        );
        assert_eq!(
            detect(
                evidence("claude", "project", "showing detailed transcript · ctrl+o to toggle",)
            ),
            DetectionUpdate::Preserve
        );
        assert_eq!(
            detect(evidence(
                "claude",
                "project",
                "select model\nenter to set as default\nesc to cancel",
            )),
            DetectionUpdate::Preserve
        );
    }

    #[test]
    fn stale_blocker_text_before_live_regions_is_ignored() {
        let codex = detected(evidence("codex", "project", "allow command?\n›\nready")).unwrap();
        assert_eq!(codex.activity, AgentActivity::Idle);

        let claude = detected(evidence(
            "claude",
            "✳ project",
            "do you want to proceed? esc to cancel\n────────\n  ❯ ",
        ))
        .unwrap();
        assert_eq!(claude.activity, AgentActivity::Idle);
    }

    #[test]
    fn claude_btw_overlay_is_working() {
        let detection = detected(evidence("claude", "project", "/btw explain this\nesc to close"));
        assert_eq!(detection.unwrap().activity, AgentActivity::Working);
    }

    #[test]
    fn background_completion_remains_unseen_until_selected() {
        let mut tracker = ActivityTracker::default();
        let working = AgentDetection {
            kind: AgentKind::Codex,
            activity: AgentActivity::Working,
            visible_idle: false,
        };
        let idle = AgentDetection {
            kind: AgentKind::Codex,
            activity: AgentActivity::Idle,
            visible_idle: true,
        };

        observe(&mut tracker, 7, 1, Some(working), false);
        let done = observe(&mut tracker, 7, 2, Some(idle), false).unwrap();
        assert_eq!(done.activity, AgentActivity::Idle);
        assert!(!done.seen);

        let acknowledged = observe(&mut tracker, 7, 2, Some(idle), true).unwrap();
        assert_eq!(acknowledged.activity, AgentActivity::Idle);
        assert!(acknowledged.seen);
    }

    #[test]
    fn plain_idle_requires_repeated_observation_after_working() {
        let mut tracker = ActivityTracker::default();
        let working = AgentDetection {
            kind: AgentKind::Codex,
            activity: AgentActivity::Working,
            visible_idle: false,
        };
        let plain_idle = AgentDetection {
            kind: AgentKind::Codex,
            activity: AgentActivity::Idle,
            visible_idle: false,
        };

        observe(&mut tracker, 11, 1, Some(working), false);
        for _ in 0..2 {
            let presentation = observe(&mut tracker, 11, 2, Some(plain_idle), false).unwrap();
            assert_eq!(presentation.activity, AgentActivity::Working);
        }
        let presentation = observe(&mut tracker, 11, 2, Some(plain_idle), false).unwrap();
        assert_eq!(presentation.activity, AgentActivity::Idle);
        assert!(!presentation.seen);
    }

    #[test]
    fn ready_transition_emits_once_after_confirmed_completion() {
        let mut tracker = ActivityTracker::default();
        let working = AgentDetection {
            kind: AgentKind::Codex,
            activity: AgentActivity::Working,
            visible_idle: false,
        };
        let plain_idle = AgentDetection {
            kind: AgentKind::Codex,
            activity: AgentActivity::Idle,
            visible_idle: false,
        };

        let initial = observe_result(&mut tracker, 12, 1, Some(working), false);
        assert_eq!(initial.transition, None);
        for observation in 0..2 {
            let held = observe_result(&mut tracker, 12, 2, Some(plain_idle), false);
            assert_eq!(held.transition, None, "observation {observation} was not confirmed");
        }
        let ready = observe_result(&mut tracker, 12, 2, Some(plain_idle), false);
        assert_eq!(ready.transition, Some(AgentTransition::Ready(AgentKind::Codex)));

        let repeated = observe_result(&mut tracker, 12, 2, Some(plain_idle), false);
        assert_eq!(repeated.transition, None);
        let selected = observe_result(&mut tracker, 12, 2, Some(plain_idle), true);
        assert_eq!(selected.transition, None);
    }

    #[test]
    fn initial_idle_does_not_emit_ready_transition() {
        let mut tracker = ActivityTracker::default();
        let idle = AgentDetection {
            kind: AgentKind::Claude,
            activity: AgentActivity::Idle,
            visible_idle: true,
        };

        let initial = observe_result(&mut tracker, 13, 1, Some(idle), false);
        assert_eq!(initial.transition, None);
        let repeated = observe_result(&mut tracker, 13, 2, Some(idle), false);
        assert_eq!(repeated.transition, None);
    }

    #[test]
    fn attention_is_visible_even_for_selected_session() {
        let mut tracker = ActivityTracker::default();
        let blocked = AgentDetection {
            kind: AgentKind::Claude,
            activity: AgentActivity::NeedsAttention,
            visible_idle: false,
        };

        let presentation = observe(&mut tracker, 9, 1, Some(blocked), true).unwrap();
        assert_eq!(presentation.activity, AgentActivity::NeedsAttention);
        assert!(presentation.seen);
    }

    #[test]
    fn unchanged_evidence_reuses_detection_without_reading_screen_again() {
        use std::cell::Cell;

        let mut tracker = ActivityTracker::default();
        let detections = Cell::new(0);
        let fingerprint = || AgentFingerprint {
            content_sequence: Some(4),
            foreground_process_name: Some("zsh"),
            title: "project",
        };

        tracker.observe_with(3, fingerprint(), false, Instant::now(), || {
            detections.set(detections.get() + 1);
            DetectionUpdate::Replace(None)
        });
        tracker.observe_with(3, fingerprint(), false, Instant::now(), || {
            detections.set(detections.get() + 1);
            DetectionUpdate::Replace(None)
        });

        assert_eq!(detections.get(), 1);
    }

    #[test]
    fn unidentified_content_is_inspected_no_faster_than_every_500_milliseconds() {
        use std::cell::Cell;

        let mut tracker = ActivityTracker::default();
        let detections = Cell::new(0);
        let started = Instant::now();
        let fingerprint = |content_sequence| AgentFingerprint {
            content_sequence: Some(content_sequence),
            foreground_process_name: Some("zsh"),
            title: "project",
        };
        let detect_none = || {
            detections.set(detections.get() + 1);
            DetectionUpdate::Replace(None)
        };

        tracker.observe_with(3, fingerprint(1), false, started, detect_none);
        let deferred = tracker.observe_with(
            3,
            fingerprint(2),
            false,
            started + Duration::from_millis(499),
            detect_none,
        );
        assert!(deferred.revisit);
        assert_eq!(detections.get(), 1);

        let due = tracker.observe_with(
            3,
            fingerprint(2),
            false,
            started + Duration::from_millis(500),
            detect_none,
        );
        assert!(!due.revisit);
        assert_eq!(detections.get(), 2);
    }

    #[test]
    fn identified_content_is_inspected_no_faster_than_every_300_milliseconds() {
        use std::cell::Cell;

        let mut tracker = ActivityTracker::default();
        let detections = Cell::new(0);
        let started = Instant::now();
        let fingerprint = |content_sequence| AgentFingerprint {
            content_sequence: Some(content_sequence),
            foreground_process_name: Some("codex"),
            title: "working",
        };
        let working = AgentDetection {
            kind: AgentKind::Codex,
            activity: AgentActivity::Working,
            visible_idle: false,
        };
        let detect_working = || {
            detections.set(detections.get() + 1);
            DetectionUpdate::Replace(Some(working))
        };

        tracker.observe_with(3, fingerprint(1), false, started, detect_working);
        let deferred = tracker.observe_with(
            3,
            fingerprint(2),
            false,
            started + Duration::from_millis(299),
            detect_working,
        );
        assert!(deferred.revisit);
        assert_eq!(detections.get(), 1);

        let due = tracker.observe_with(
            3,
            fingerprint(2),
            false,
            started + Duration::from_millis(300),
            detect_working,
        );
        assert!(!due.revisit);
        assert_eq!(detections.get(), 2);
    }

    #[test]
    fn process_identity_change_is_inspected_immediately() {
        use std::cell::Cell;

        let mut tracker = ActivityTracker::default();
        let detections = Cell::new(0);
        let started = Instant::now();
        let fingerprint = |process| AgentFingerprint {
            content_sequence: Some(1),
            foreground_process_name: Some(process),
            title: "project",
        };
        let detect_none = || {
            detections.set(detections.get() + 1);
            DetectionUpdate::Replace(None)
        };

        tracker.observe_with(3, fingerprint("zsh"), false, started, detect_none);
        tracker.observe_with(
            3,
            fingerprint("codex"),
            false,
            started + Duration::from_millis(1),
            detect_none,
        );

        assert_eq!(detections.get(), 2);
    }

    #[test]
    fn known_agent_survives_five_transient_identity_misses() {
        let mut tracker = ActivityTracker::default();
        let started = Instant::now();
        let working = AgentDetection {
            kind: AgentKind::Codex,
            activity: AgentActivity::Working,
            visible_idle: false,
        };
        let fingerprint = |process| AgentFingerprint {
            content_sequence: Some(1),
            foreground_process_name: Some(process),
            title: "project",
        };
        tracker.observe_with(3, fingerprint("codex"), false, started, || {
            DetectionUpdate::Replace(Some(working))
        });

        for miss in 1..CONFIRMED_IDENTITY_MISSES {
            let observation = tracker.observe_with(
                3,
                fingerprint("zsh"),
                false,
                started + Duration::from_millis(1 + u64::from(miss - 1) * 300),
                || DetectionUpdate::Replace(None),
            );
            assert_eq!(
                observation.presentation.map(|agent| agent.activity),
                Some(AgentActivity::Working)
            );
            assert!(observation.revisit);
        }

        let cleared = tracker.observe_with(
            3,
            fingerprint("zsh"),
            false,
            started + Duration::from_millis(1 + u64::from(CONFIRMED_IDENTITY_MISSES - 1) * 300),
            || DetectionUpdate::Replace(None),
        );
        assert_eq!(cleared.presentation, None);
        assert!(!cleared.revisit);
    }
}
