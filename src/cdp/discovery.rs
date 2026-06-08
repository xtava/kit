//! CDP endpoint discovery over HTTP: `/json/version` confirms a port is a *Chromium browser*
//! DevTools server (and hands us the browser-level websocket — the stable endpoint an Attachment
//! binds to, per `docs/adr/0002`); `/json` lists its targets.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::Deserialize;

use super::target::{Target, TargetKind};
use super::{http, ports};

const PROBE_CONCURRENCY: usize = 32;

/// A confirmed Chromium browser DevTools endpoint.
#[derive(Debug, Clone)]
pub struct BrowserEndpoint {
    pub port: u16,
    /// The browser-level websocket — survives renderer reloads for the life of the main process.
    pub ws_url: String,
    /// e.g. `"myapp-dev"`, parsed from the User-Agent product token.
    pub app: String,
    pub user_agent: String,
}

#[derive(Debug, Deserialize)]
struct VersionInfo {
    #[serde(rename = "Browser")]
    browser: Option<String>,
    #[serde(rename = "User-Agent")]
    user_agent: Option<String>,
    #[serde(rename = "webSocketDebuggerUrl")]
    ws_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTarget {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    ws_url: Option<String>,
}

/// Confirm `port` is a *Chromium browser* endpoint and return it. The `Chrome/` signature rejects
/// Node `--inspect` ports, which also serve `/json` but expose V8, not the browser's windows.
pub async fn browser_endpoint(port: u16) -> Option<BrowserEndpoint> {
    let body = http::get(port, "/json/version").await.ok()?;
    let version: VersionInfo = serde_json::from_str(&body).ok()?;
    let browser = version.browser?;
    if !browser.contains("Chrome/") {
        return None;
    }
    let ws_url = version.ws_url?;
    let user_agent = version.user_agent.unwrap_or_default();
    Some(BrowserEndpoint { port, app: app_name(&user_agent), ws_url, user_agent })
}

/// True when `port` speaks browser-level CDP (a read-only GET).
pub async fn is_cdp(port: u16) -> bool {
    browser_endpoint(port).await.is_some()
}

pub async fn targets(port: u16) -> Result<Vec<Target>> {
    let body = http::get(port, "/json").await.context("fetch /json")?;
    let raw: Vec<RawTarget> = serde_json::from_str(&body).context("parse /json")?;
    Ok(raw
        .into_iter()
        .map(|target| Target {
            kind: TargetKind::parse(&target.kind),
            id: target.id,
            title: target.title,
            url: target.url,
            ws_url: target.ws_url,
        })
        .collect())
}

/// A discovered Instance: a confirmed browser endpoint plus the dev metadata a [`Instance selector`]
/// matches on (worktree, instance id) — read from the owning process.
#[derive(Debug, Clone)]
pub struct Instance {
    pub endpoint: BrowserEndpoint,
    pub pid: u32,
    pub worktree: Option<String>,
    pub instance_id: Option<u32>,
}

impl Instance {
    /// A stable, filesystem-safe name keyed off the most specific identity available.
    pub fn name(&self) -> String {
        match (&self.worktree, self.instance_id) {
            (Some(worktree), _) => sanitize(worktree),
            (None, Some(id)) => format!("{}-{id}", sanitize(&self.endpoint.app)),
            (None, None) => sanitize(&self.endpoint.app),
        }
    }

    /// Whether this Instance answers to a selector — app name, worktree, instance id, or exact port.
    pub fn matches(&self, selector: &str) -> bool {
        let needle = selector.to_lowercase();
        self.endpoint.app.to_lowercase().contains(&needle)
            || self.worktree.as_deref().is_some_and(|w| w.to_lowercase().contains(&needle))
            || self.instance_id.is_some_and(|id| id.to_string() == selector)
            || self.endpoint.port.to_string() == selector
    }
}

/// Sweep every listening localhost port, keep the ones that are Chromium browser endpoints, and
/// enrich each with its owning process's dev metadata.
pub async fn discover() -> Vec<Instance> {
    futures_util::stream::iter(ports::all_listening())
        .map(|(port, pid)| async move {
            let endpoint = browser_endpoint(port).await?;
            Some(Instance {
                endpoint,
                pid,
                worktree: worktree_of(pid),
                instance_id: instance_id_of(pid),
            })
        })
        .buffer_unordered(PROBE_CONCURRENCY)
        .filter_map(|instance| async move { instance })
        .collect()
        .await
}

fn worktree_of(pid: u32) -> Option<String> {
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    let cwd = cwd.to_string_lossy();
    let after = cwd.split(".worktrees/").nth(1)?;
    Some(after.split('/').next()?.to_owned())
}

fn instance_id_of(pid: u32) -> Option<u32> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    environ.split(|&byte| byte == 0).find_map(|entry| {
        std::str::from_utf8(entry).ok()?.strip_prefix("INSTANCE_ID=")?.parse().ok()
    })
}

fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '-' })
        .collect()
}

/// Pull the app's product token out of a CDP User-Agent — the token immediately before `Chrome/`,
/// e.g. `"… myapp-dev/1.4 Chrome/124 Electron/30 …"` → `"myapp-dev"`.
fn app_name(user_agent: &str) -> String {
    let tokens: Vec<&str> = user_agent.split_whitespace().collect();
    tokens
        .iter()
        .position(|token| token.starts_with("Chrome/"))
        .filter(|&index| index > 0)
        .and_then(|index| tokens[index - 1].split_once('/'))
        .map(|(name, _)| name.to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "electron".to_owned())
}
