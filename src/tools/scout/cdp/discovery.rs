//! CDP target discovery over HTTP: `/json/version` confirms an endpoint *is* a DevTools server,
//! `/json` lists its targets. Each raw target classifies into a [`TargetKind`] from its url/type.

use serde::Deserialize;

use super::http;
use crate::tools::scout::model::TargetKind;

#[derive(Debug, Deserialize)]
pub struct RawTarget {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    pub ws_url: Option<String>,
}

/// True when `port` is a *Chromium browser* DevTools endpoint (read-only check — just a GET). The
/// `Chrome/` signature rejects Node `--inspect` ports, which also serve `/json` but expose the main
/// process's V8 context, not the browser's windows.
pub async fn is_cdp(port: u16) -> bool {
    match http::get(port, "/json/version").await {
        Ok(body) => body.contains("webSocketDebuggerUrl") && body.contains("Chrome/"),
        Err(_) => false,
    }
}

pub async fn fetch_targets(port: u16) -> Vec<RawTarget> {
    match http::get(port, "/json").await {
        Ok(body) => serde_json::from_str(&body).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

impl RawTarget {
    /// Targets worth a heap/DOM probe — has a websocket, isn't DevTools itself.
    pub fn is_probeable(&self) -> bool {
        self.ws_url.is_some()
            && !self.url.starts_with("devtools://")
            && matches!(
                self.kind.as_str(),
                "page" | "iframe" | "webview" | "worker" | "shared_worker" | "service_worker"
                    | "background_page"
            )
    }

    pub fn classify(&self) -> TargetKind {
        if self.url.starts_with("vscode-webview://") || self.url.starts_with("chrome-extension://") {
            TargetKind::ExtensionWebview
        } else if let Some(workspace) = workspace_id(&self.url) {
            TargetKind::Workbench { workspace }
        } else if self.url.contains("background-worker") {
            TargetKind::BackgroundWorker
        } else if matches!(self.kind.as_str(), "worker" | "shared_worker" | "service_worker") {
            TargetKind::Worker
        } else if self.kind == "webview" {
            TargetKind::Webview
        } else if matches!(self.kind.as_str(), "page" | "iframe") {
            TargetKind::Page
        } else {
            TargetKind::Other
        }
    }
}

fn workspace_id(url: &str) -> Option<String> {
    let id = url.split("/workspace/").nth(1)?.split('/').next()?;
    (!id.is_empty()).then(|| id.to_owned())
}
