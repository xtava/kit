//! The generic CDP target model — a window, webview, or worker — and the [`Target selector`] that
//! names one without ever exposing the volatile CDP `targetId` to a caller (see `docs/adr/0002`).
//!
//! This model is deliberately *generic*: no `Workbench`/workspace flavour lives here. App-specific
//! meaning is a `scout` recon concern or a [`crate::cdp`] lens — never the engine's.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// One debuggable surface inside an Instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub id: String,
    pub kind: TargetKind,
    pub title: String,
    pub url: String,
    /// The per-target DevTools websocket. `None` for targets that don't expose one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ws_url: Option<String>,
}

/// How "main-window-like" a target is, with the signed signals that produced the total — so a caller
/// can explain *why* a target won or lost, not just print a number.
#[derive(Debug, Clone, Serialize)]
pub struct TargetScore {
    pub total: i32,
    pub reasons: Vec<ScoreReason>,
}

/// One signed contribution to a [`TargetScore`]: a human-readable signal and the points it carried.
#[derive(Debug, Clone, Serialize)]
pub struct ScoreReason {
    pub points: i32,
    pub why: String,
}

/// What a target *is*, from CDP's `type`. Pure protocol vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Page,
    Iframe,
    Webview,
    Worker,
    SharedWorker,
    ServiceWorker,
    BackgroundPage,
    Other,
}

impl TargetKind {
    pub fn parse(cdp_type: &str) -> Self {
        match cdp_type {
            "page" => Self::Page,
            "iframe" => Self::Iframe,
            "webview" => Self::Webview,
            "worker" => Self::Worker,
            "shared_worker" => Self::SharedWorker,
            "service_worker" => Self::ServiceWorker,
            "background_page" => Self::BackgroundPage,
            _ => Self::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Page => "page",
            Self::Iframe => "iframe",
            Self::Webview => "webview",
            Self::Worker => "worker",
            Self::SharedWorker => "shared_worker",
            Self::ServiceWorker => "service_worker",
            Self::BackgroundPage => "background_page",
            Self::Other => "other",
        }
    }
}

impl Target {
    /// Targets worth attaching to or probing — has a websocket and isn't DevTools itself.
    pub fn is_inspectable(&self) -> bool {
        self.ws_url.is_some()
            && !self.url.starts_with("devtools://")
            && self.kind != TargetKind::Other
    }

    /// Whether this target answers to a [`Target selector`] — `id` prefix, kind name, or a
    /// case-insensitive substring of its title or url.
    pub fn matches(&self, selector: &str) -> bool {
        let needle = selector.to_lowercase();
        self.id.starts_with(selector)
            || self.kind.as_str() == needle
            || self.title.to_lowercase().contains(&needle)
            || self.url.to_lowercase().contains(&needle)
    }

    /// The canonical client-facing label for a target — its title, or its url when untitled. This is
    /// the identity the Timeline keys events on and that activity is counted against.
    pub fn label(&self) -> String {
        if self.title.is_empty() {
            self.url.clone()
        } else {
            self.title.clone()
        }
    }

    /// How "main-window-like" a target is, higher is better, with the breakdown that produced it.
    /// A real page beats a worker; a titled page beats a blank one; an app-origin window beats a
    /// stray web page; DevTools, extension hosts, and webview shells sink to the bottom — they are
    /// tooling surfaces, never the workbench a debugger is after.
    pub fn score(&self) -> TargetScore {
        let mut reasons = Vec::new();
        let mut note = |points: i32, why: &str| {
            if points != 0 {
                reasons.push(ScoreReason { points, why: why.to_owned() });
            }
        };

        match self.kind {
            TargetKind::Page => note(100, "page (a top-level window)"),
            TargetKind::Webview => note(10, "webview (an embedded surface)"),
            TargetKind::Iframe => note(5, "iframe"),
            TargetKind::Worker | TargetKind::SharedWorker => note(-80, "worker (no DOM)"),
            TargetKind::ServiceWorker => note(-90, "service worker (no DOM)"),
            TargetKind::BackgroundPage => note(-90, "background page"),
            TargetKind::Other => note(-60, "non-visual target"),
        }

        let url = self.url.to_lowercase();
        let title = self.title.to_lowercase();

        if !self.title.is_empty() {
            note(10, "has a title");
        }
        if url.contains("devtools") || title.contains("devtools") {
            note(-1000, "devtools surface (the debugger's own reflection)");
        }
        if title.contains("background") || url.contains("background") {
            note(-80, "a background worker page");
        }
        if self.url == "about:blank" || self.url.is_empty() {
            note(-60, "blank / empty document");
        }

        // The url scheme is the strongest static signal of *what* a surface is: an app-origin custom
        // scheme is almost always the workbench; the Chromium/Electron tooling schemes are chrome,
        // not the app. This stays generic protocol vocabulary — no app name lives here.
        if let Some((scheme, rest)) = self.url.split_once("://") {
            match scheme.to_lowercase().as_str() {
                "chrome-extension" => note(-200, "a chrome-extension host"),
                "vscode-webview" | "vscode-file" => note(-150, "a vscode webview shell"),
                "http" | "https" | "about" | "chrome" | "devtools" | "data" | "blob" => {}
                scheme if !rest.is_empty() => note(25, &format!("an app-origin scheme `{scheme}`")),
                _ => {}
            }
        }

        TargetScore { total: reasons.iter().map(|reason| reason.points).sum(), reasons }
    }

    /// The bare workbench-likeness total — the sort key for ranking targets when the breakdown isn't
    /// needed.
    pub fn main_rank(&self) -> i32 {
        self.score().total
    }
}

/// Resolve a [`Target selector`] to the best target, breaking score ties by live Timeline activity
/// and then target id. The static [`Target::score`] decides which *kind* of surface wins (a
/// workbench page over a worker or a blank tab); among equally workbench-like siblings — two open
/// workspaces, say — the one actually streaming events wins; the id is the final, stable tiebreak so
/// resolution never flickers between two idle equals. `activity` maps a [`Target::label`] to its
/// event count (an empty map degrades this to pure static ranking).
pub fn select_active<'a>(
    targets: &'a [Target],
    selector: Option<&str>,
    activity: &HashMap<String, usize>,
) -> Option<&'a Target> {
    let needle = selector.filter(|selector| *selector != "main").map(str::to_owned);
    targets
        .iter()
        .filter(|target| match &needle {
            None => true,
            Some(needle) => target.matches(needle),
        })
        .max_by_key(|target| {
            (
                target.score().total,
                activity.get(&target.label()).copied().unwrap_or(0),
                target.id.as_str(),
            )
        })
}

/// Static resolution with no live activity — the most workbench-like target, ties broken by id.
pub fn select<'a>(targets: &'a [Target], selector: Option<&str>) -> Option<&'a Target> {
    select_active(targets, selector, &HashMap::new())
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

    /// The live Modular instance: two open workspaces, a background-worker page, a stray web page, a
    /// blank tab, and a crowd of vscode-webview / extension-host shells and workers.
    fn modular_targets() -> Vec<Target> {
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
            target("DD", TargetKind::Page, "Google", "https://www.google.com/"),
            target("EE", TargetKind::Page, "about:blank", "about:blank"),
            target("FF", TargetKind::Iframe, "ext", "vscode-webview://abc/index.html"),
            target("GG", TargetKind::ServiceWorker, "sw", "vscode-webview://abc/service-worker.js"),
            target("HH", TargetKind::ServiceWorker, "bg", "chrome-extension://x/background.js"),
        ]
    }

    #[test]
    fn a_workbench_page_outranks_every_shell_worker_and_blank() {
        let targets = modular_targets();
        let chosen = select(&targets, None).expect("a target");
        assert_eq!(chosen.kind, TargetKind::Page);
        assert!(
            chosen.url.contains("/workspace/"),
            "picked {} ({})",
            chosen.url,
            chosen.score().total
        );
        // Every shell / worker / blank surface scores below any real workspace page.
        let workspace = chosen.score().total;
        for noise in ["EE", "FF", "GG", "HH"] {
            let target = targets.iter().find(|target| target.id == noise).unwrap();
            assert!(
                target.score().total < workspace,
                "{} should lose to the workspace",
                target.url
            );
        }
    }

    #[test]
    fn a_background_worker_page_loses_to_a_real_workspace() {
        let targets = modular_targets();
        let background = targets.iter().find(|target| target.id == "CC").unwrap();
        let workspace = targets.iter().find(|target| target.id == "BB").unwrap();
        assert!(background.score().total < workspace.score().total);
    }

    #[test]
    fn activity_breaks_the_tie_between_sibling_workspaces() {
        let targets = modular_targets();
        // Both workspaces tie on static score — only live activity tells the live one from the idle.
        let mut activity = HashMap::new();
        activity.insert("modular".to_owned(), 1200); // the focused workspace (id BB) is streaming
        let chosen = select_active(&targets, None, &activity).expect("a target");
        assert_eq!(chosen.id, "BB", "the active workspace must win the tie");
    }

    #[test]
    fn resolution_is_deterministic_when_two_targets_are_dead_equal() {
        // No activity, identical score: the id tiebreak makes the choice stable, never HashMap-random.
        let targets = modular_targets();
        let first = select(&targets, None).map(|target| target.id.clone());
        for _ in 0..50 {
            assert_eq!(select(&targets, None).map(|target| target.id.clone()), first);
        }
    }

    #[test]
    fn a_selector_filters_before_ranking() {
        let targets = modular_targets();
        let chosen = select(&targets, Some("google")).expect("a match");
        assert_eq!(chosen.url, "https://www.google.com/");
    }

    #[test]
    fn the_score_breakdown_explains_the_total() {
        let workspace =
            target("BB", TargetKind::Page, "modular", "modular://modular-app/workspace/18ac/all");
        let score = workspace.score();
        assert_eq!(score.total, score.reasons.iter().map(|reason| reason.points).sum::<i32>());
        assert!(score.reasons.iter().any(|reason| reason.why.contains("app-origin")));
    }
}
