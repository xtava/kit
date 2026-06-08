//! The generic CDP target model — a window, webview, or worker — and the [`Target selector`] that
//! names one without ever exposing the volatile CDP `targetId` to a caller (see `docs/adr/0002`).
//!
//! This model is deliberately *generic*: no `Workbench`/workspace flavour lives here. App-specific
//! meaning is a `scout` recon concern or a [`crate::cdp`] lens — never the engine's.

use serde::Serialize;

/// One debuggable surface inside an Instance.
#[derive(Debug, Clone, Serialize)]
pub struct Target {
    pub id: String,
    pub kind: TargetKind,
    pub title: String,
    pub url: String,
    /// The per-target DevTools websocket. `None` for targets that don't expose one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ws_url: Option<String>,
}

/// What a target *is*, from CDP's `type`. Pure protocol vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

    fn is_worker(self) -> bool {
        matches!(self, Self::Worker | Self::SharedWorker | Self::ServiceWorker)
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

    /// How "main-window-like" a target is, higher is better — used to resolve a bare selector to
    /// the window a human means. A real page beats a worker; a titled page beats a blank one;
    /// DevTools/shell chrome sinks to the bottom.
    pub fn main_rank(&self) -> i32 {
        let mut rank = 0;
        match self.kind {
            TargetKind::Page => rank += 100,
            TargetKind::Webview => rank += 40,
            TargetKind::Iframe => rank += 20,
            _ if self.kind.is_worker() => rank -= 50,
            _ => {}
        }
        if !self.title.is_empty() {
            rank += 10;
        }
        let lower = self.url.to_lowercase();
        if lower.contains("devtools") || self.title.to_lowercase().contains("devtools") {
            rank -= 1000;
        }
        if self.title.to_lowercase().contains("background") || lower.contains("background") {
            rank -= 60;
        }
        if self.url == "about:blank" || self.url.is_empty() {
            rank -= 40;
        }
        // An app-origin window (a custom scheme like `app://`, `file://`) is what a debugger is
        // almost always after — outrank a stray loaded web page.
        if let Some((scheme, rest)) = self.url.split_once("://") {
            let web = matches!(scheme, "http" | "https" | "about" | "chrome" | "chrome-extension" | "devtools");
            if !web && !rest.is_empty() {
                rank += 15;
            }
        }
        rank
    }
}

/// Resolve a [`Target selector`] against a live target set. `None` (or `"main"`) picks the most
/// main-window-like target; otherwise the highest-ranked target that [`Target::matches`].
pub fn select<'a>(targets: &'a [Target], selector: Option<&str>) -> Option<&'a Target> {
    match selector {
        None | Some("main") => targets.iter().max_by_key(|target| target.main_rank()),
        Some(selector) => targets
            .iter()
            .filter(|target| target.matches(selector))
            .max_by_key(|target| target.main_rank()),
    }
}
