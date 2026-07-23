use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::client::SessionId;

const CONFIRMED_IDLE_OBSERVATIONS: u8 = 3;

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

impl AgentPresentation {
    pub const fn status_label(self) -> &'static str {
        match (self.activity, self.seen) {
            (AgentActivity::NeedsAttention, _) => "needs input",
            (AgentActivity::Working, _) => "working",
            (AgentActivity::Idle, false) => "done",
            (AgentActivity::Idle, true) => "idle",
            (AgentActivity::Unknown, _) => "active",
        }
    }
}

pub struct AgentEvidence<'a> {
    pub foreground_process_name: Option<&'a str>,
    pub title: &'a str,
    pub screen: &'a str,
}

pub fn detect(evidence: AgentEvidence<'_>) -> Option<AgentDetection> {
    let title = evidence.title.trim();
    let screen = evidence.screen.to_ascii_lowercase();
    let kind = identify_agent(evidence.foreground_process_name, title, &screen)?;
    let (activity, visible_idle) = match kind {
        AgentKind::Claude => detect_claude(title, &screen),
        AgentKind::Codex => detect_codex(title, &screen),
    };
    Some(AgentDetection { kind, activity, visible_idle })
}

fn identify_agent(process_name: Option<&str>, title: &str, screen: &str) -> Option<AgentKind> {
    if let Some(process_name) = process_name {
        let process = Path::new(process_name)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(process_name)
            .trim_end_matches(".exe")
            .to_ascii_lowercase();
        let tokens = process
            .split(|character: char| !character.is_ascii_alphanumeric())
            .collect::<HashSet<_>>();
        if tokens.contains("claude") {
            return Some(AgentKind::Claude);
        }
        if tokens.contains("codex") {
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
    let blocked = contains_any(
        screen,
        &[
            "waiting for permission",
            "do you want to allow this connection?",
            "tab to amend",
            "ctrl+e to explain",
            "review your answers",
            "run a dynamic workflow?",
        ],
    ) || (screen.contains("do you want to proceed?")
        && screen.contains("esc to cancel"))
        || (screen.contains("enter to select") && screen.contains("esc to cancel"));
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
    let blocked = title.contains("Action Required")
        || contains_any(
            screen,
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
        (AgentActivity::Idle, true)
    } else {
        (AgentActivity::Unknown, false)
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
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

#[derive(Default)]
pub struct ActivityTracker {
    sessions: HashMap<SessionId, TrackedAgent>,
}

impl ActivityTracker {
    pub fn observe(
        &mut self,
        session_id: SessionId,
        detection: Option<AgentDetection>,
        selected: bool,
    ) -> Option<AgentPresentation> {
        let Some(detection) = detection else {
            self.sessions.remove(&session_id);
            return None;
        };
        let tracked = self.sessions.entry(session_id).or_insert(TrackedAgent {
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
                apply_activity(tracked, detection.activity, selected);
            }
        } else {
            apply_activity(tracked, detection.activity, selected);
        }

        if selected {
            tracked.seen = true;
        }
        Some(AgentPresentation {
            kind: tracked.kind,
            activity: tracked.activity,
            seen: tracked.seen,
        })
    }

    pub fn retain(&mut self, session_ids: impl IntoIterator<Item = SessionId>) {
        let live = session_ids.into_iter().collect::<HashSet<_>>();
        self.sessions.retain(|session_id, _| live.contains(session_id));
    }
}

fn should_hold_idle(tracked: TrackedAgent, detection: AgentDetection) -> bool {
    tracked.activity == AgentActivity::Working
        && detection.activity == AgentActivity::Idle
        && !detection.visible_idle
        && tracked.pending_idle_observations < CONFIRMED_IDLE_OBSERVATIONS
}

fn apply_activity(tracked: &mut TrackedAgent, activity: AgentActivity, selected: bool) {
    let previous = tracked.activity;
    tracked.activity = activity;
    tracked.pending_idle_observations = 0;
    tracked.seen = match activity {
        AgentActivity::NeedsAttention => false,
        AgentActivity::Working | AgentActivity::Unknown => true,
        AgentActivity::Idle if previous != AgentActivity::Idle => selected,
        AgentActivity::Idle => tracked.seen || selected,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence<'a>(process: &'a str, title: &'a str, screen: &'a str) -> AgentEvidence<'a> {
        AgentEvidence { foreground_process_name: Some(process), title, screen }
    }

    #[test]
    fn codex_action_required_is_attention() {
        let detection = detect(evidence("/opt/codex", "Action Required", "allow command?"));
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
        let detection = detect(evidence("claude", "✳ project", "  ❯ "));
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
        let detection = detect(AgentEvidence {
            foreground_process_name: Some("zsh"),
            title: "project",
            screen: "previous output: allow command?",
        });
        assert_eq!(detection, None);
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

        tracker.observe(7, Some(working), false);
        let done = tracker.observe(7, Some(idle), false).unwrap();
        assert_eq!(done.status_label(), "done");
        assert!(!done.seen);

        let acknowledged = tracker.observe(7, Some(idle), true).unwrap();
        assert_eq!(acknowledged.status_label(), "idle");
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

        tracker.observe(11, Some(working), false);
        for _ in 0..2 {
            let presentation = tracker.observe(11, Some(plain_idle), false).unwrap();
            assert_eq!(presentation.activity, AgentActivity::Working);
        }
        let presentation = tracker.observe(11, Some(plain_idle), false).unwrap();
        assert_eq!(presentation.activity, AgentActivity::Idle);
        assert!(!presentation.seen);
    }

    #[test]
    fn attention_is_visible_even_for_selected_session() {
        let mut tracker = ActivityTracker::default();
        let blocked = AgentDetection {
            kind: AgentKind::Claude,
            activity: AgentActivity::NeedsAttention,
            visible_idle: false,
        };

        let presentation = tracker.observe(9, Some(blocked), true).unwrap();
        assert_eq!(presentation.activity, AgentActivity::NeedsAttention);
        assert!(presentation.seen);
        assert_eq!(presentation.status_label(), "needs input");
    }
}
