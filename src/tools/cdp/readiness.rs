//! `ready` — is the workbench actually there and usable, and why did *this* target win? The engine
//! ranks every Target generically ([`crate::cdp::Target::score`]); this module turns that ranking
//! plus a live document probe into the agent-readable verdict the command prints. The ranking is the
//! pure, tested core ([`rank`]); the daemon supplies the live document state and recent errors.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::cdp::{ScoreReason, Target, TargetKind};

/// The full readiness verdict for an Instance: which Target was selected, the live state of its
/// document, the recent errors against it, and the ranked candidate field with the reason each was
/// chosen or rejected.
#[derive(Debug, Serialize)]
pub struct Readiness {
    pub instance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<DocState>,
    pub candidates: Vec<Candidate>,
    pub recent_errors: Vec<String>,
}

/// The live state of the selected Target's document — the actual "is it ready to drive" signals,
/// decoded from a single generic probe. App-specific bridges (`__testAPI`, workspace/editor state)
/// are deliberately *not* here; they are a lens (`kit cdp lens workbench`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocState {
    pub href: String,
    pub title: String,
    pub ready_state: String,
    pub visibility: String,
    pub focused: bool,
    pub body_text_len: u64,
}

impl DocState {
    /// Whether the document is loaded, shown, and actually populated — a "complete" but blank or
    /// hidden document is *not* a usable workbench.
    pub fn is_usable(&self) -> bool {
        self.ready_state == "complete" && self.visibility == "visible" && self.body_text_len > 0
    }
}

/// One ranked Target in the field, with the breakdown behind its score and a one-line verdict on why
/// it was selected or rejected — the diagnostic that lets an agent see the resolution, not guess it.
#[derive(Debug, Serialize)]
pub struct Candidate {
    pub label: String,
    pub kind: TargetKind,
    pub url: String,
    pub score: i32,
    pub activity: usize,
    pub selected: bool,
    /// Why this candidate won or lost, in one phrase.
    pub verdict: String,
    pub reasons: Vec<ScoreReason>,
}

/// Rank a selector-filtered Target set exactly as [`crate::cdp::select_active`] resolves it — static
/// score, then live activity, then id — and annotate each with the verdict that explains the order.
/// The winner is the top candidate; everything else carries the reason it lost to it.
pub fn rank(
    targets: &[Target],
    activity: &HashMap<String, usize>,
    selector: Option<&str>,
) -> Vec<Candidate> {
    let needle = selector.filter(|selector| *selector != "main").map(str::to_owned);
    let mut ranked: Vec<&Target> = targets
        .iter()
        .filter(|target| match &needle {
            None => true,
            Some(needle) => target.matches(needle),
        })
        .collect();
    ranked.sort_by_key(|target| std::cmp::Reverse(sort_key(target, activity)));

    let winner = ranked.first().map(|target| (target.score().total, activity_of(target, activity)));
    ranked
        .iter()
        .enumerate()
        .map(|(index, target)| {
            let score = target.score();
            let count = activity_of(target, activity);
            let selected = index == 0;
            Candidate {
                verdict: verdict_for(selected, &score.reasons, score.total, count, winner),
                label: target.label(),
                kind: target.kind,
                url: target.url.clone(),
                score: score.total,
                activity: count,
                selected,
                reasons: score.reasons,
            }
        })
        .collect()
}

fn sort_key(target: &Target, activity: &HashMap<String, usize>) -> (i32, usize, String) {
    (target.score().total, activity_of(target, activity), target.id.clone())
}

fn activity_of(target: &Target, activity: &HashMap<String, usize>) -> usize {
    activity.get(&target.label()).copied().unwrap_or(0)
}

/// The one-phrase reason a candidate is where it is: selected, dragged down by its strongest negative
/// signal, idle next to an active equal, or simply outscored.
fn verdict_for(
    selected: bool,
    reasons: &[ScoreReason],
    score: i32,
    activity: usize,
    winner: Option<(i32, usize)>,
) -> String {
    if selected {
        return match activity {
            0 => "selected — highest workbench score".to_owned(),
            count => format!("selected — highest score, streaming ({count} events)"),
        };
    }
    if let Some(worst) =
        reasons.iter().filter(|reason| reason.points < 0).min_by_key(|reason| reason.points)
    {
        return worst.why.clone();
    }
    match winner {
        Some((top, top_activity)) if score == top && activity < top_activity => {
            "idle — the selected target is the active one".to_owned()
        }
        Some((top, _)) if score < top => format!("lower workbench score ({score} vs {top})"),
        _ => "outranked".to_owned(),
    }
}

/// Render the verdict for a human/agent — compact text by default, full structure under `--json`.
pub fn render(readiness: &Readiness, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(readiness).unwrap_or_else(|_| "null".to_owned());
    }

    let selected = readiness.candidates.iter().find(|candidate| candidate.selected);
    let mut out = vec![format!("ready  {}", readiness.instance)];

    match (selected, &readiness.document) {
        (None, _) => {
            out.push("  target     none — no inspectable target in this instance".to_owned())
        }
        (Some(target), document) => {
            out.push(format!("  target     {}   {}", target.label, target.url));
            match document {
                Some(doc) => {
                    let focus = if doc.focused { "focused" } else { "unfocused" };
                    out.push(format!(
                        "  document   {} · {} · {} · {} chars{}",
                        doc.ready_state,
                        doc.visibility,
                        focus,
                        doc.body_text_len,
                        if doc.is_usable() { "" } else { "   ⚠ not a usable workbench yet" },
                    ));
                }
                None => out.push(
                    "  document   unreachable (probe failed — target may be mid-reload)".to_owned(),
                ),
            }
        }
    }

    out.push(match readiness.recent_errors.len() {
        0 => "  errors     none recently".to_owned(),
        n => format!("  errors     {n} recent\n{}", indent(&readiness.recent_errors)),
    });
    out.push(
        "  app state  kit cdp lens workbench   (workspace · editor · __testAPI bridge)".to_owned(),
    );

    out.push(format!("\ncandidates ({})", readiness.candidates.len()));
    for candidate in &readiness.candidates {
        let marker = if candidate.selected { "★" } else { " " };
        out.push(format!(
            "  {marker} {:<38} {:<14} {:>5}  {:>7}  {}",
            truncate(&candidate.label, 38),
            candidate.kind.as_str(),
            candidate.score,
            activity_cell(candidate.activity),
            candidate.verdict,
        ));
    }
    out.join("\n")
}

fn indent(lines: &[String]) -> String {
    lines.iter().map(|line| format!("               {line}")).collect::<Vec<_>>().join("\n")
}

fn activity_cell(activity: usize) -> String {
    match activity {
        0 => "idle".to_owned(),
        n if n < 1000 => n.to_string(),
        n => format!("{:.1}k", n as f64 / 1000.0),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str, kind: TargetKind, title: &str, url: &str) -> Target {
        Target {
            id: id.into(),
            kind,
            title: title.into(),
            url: url.into(),
            ws_url: Some("ws://x".into()),
        }
    }

    fn field() -> Vec<Target> {
        vec![
            target(
                "AA",
                TargetKind::Page,
                "All docs · modular",
                "modular://modular-app/workspace/c0d6/all",
            ),
            target("BB", TargetKind::Page, "modular", "modular://modular-app/workspace/18ac/all"),
            target(
                "CC",
                TargetKind::Page,
                "background",
                "modular://modular-app/background-worker.html",
            ),
            target("EE", TargetKind::Page, "about:blank", "about:blank"),
            target("GG", TargetKind::ServiceWorker, "sw", "vscode-webview://abc/service-worker.js"),
        ]
    }

    #[test]
    fn the_active_workspace_is_selected_and_explained() {
        let mut activity = HashMap::new();
        activity.insert("modular".to_owned(), 1200);
        let ranked = rank(&field(), &activity, None);

        let winner = &ranked[0];
        assert!(winner.selected);
        assert_eq!(winner.label, "modular");
        assert!(winner.verdict.contains("streaming"), "got: {}", winner.verdict);
    }

    #[test]
    fn the_idle_sibling_is_rejected_for_being_idle_not_for_its_score() {
        let mut activity = HashMap::new();
        activity.insert("modular".to_owned(), 1200);
        let ranked = rank(&field(), &activity, None);

        let sibling =
            ranked.iter().find(|candidate| candidate.label == "All docs · modular").unwrap();
        assert!(!sibling.selected);
        assert!(sibling.verdict.contains("idle"), "got: {}", sibling.verdict);
    }

    #[test]
    fn noise_targets_are_rejected_by_their_strongest_negative_signal() {
        let ranked = rank(&field(), &HashMap::new(), None);
        let blank = ranked.iter().find(|candidate| candidate.url == "about:blank").unwrap();
        assert!(blank.verdict.contains("blank"), "got: {}", blank.verdict);
        let worker =
            ranked.iter().find(|candidate| candidate.kind == TargetKind::ServiceWorker).unwrap();
        assert!(!worker.selected);
    }

    #[test]
    fn a_complete_but_blank_document_is_not_usable() {
        let blank = DocState {
            href: "about:blank".into(),
            title: String::new(),
            ready_state: "complete".into(),
            visibility: "visible".into(),
            focused: true,
            body_text_len: 0,
        };
        assert!(!blank.is_usable());
    }
}
