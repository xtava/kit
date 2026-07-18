//! CDP endpoint discovery over HTTP: `/json/version` confirms a port is a *Chromium browser*
//! DevTools server (and hands us the browser-level websocket — the stable endpoint an Attachment
//! binds to, per `docs/adr/0002`); `/json` lists its targets.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::Deserialize;

use crate::framework::RepositoryLocator;

use super::target::{Target, TargetKind};
use super::{http, ports};

const PROBE_CONCURRENCY: usize = 32;
const DISCOVERY_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

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
    parse_browser_endpoint(port, &body)
}

async fn probe_browser_endpoint(port: u16) -> Option<BrowserEndpoint> {
    let body = http::get_with_timeout(port, "/json/version", DISCOVERY_PROBE_TIMEOUT).await.ok()?;
    parse_browser_endpoint(port, &body)
}

fn parse_browser_endpoint(port: u16, body: &str) -> Option<BrowserEndpoint> {
    let version: VersionInfo = serde_json::from_str(body).ok()?;
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

/// The Electron main process's V8 inspector — a `node.js/...` endpoint opened with `--inspect`.
/// It serves the same protocol but exposes the Node main, not the browser's windows.
#[derive(Debug, Clone)]
pub struct NodeEndpoint {
    pub port: u16,
    pub ws_url: String,
}

/// Find the node inspector among the ports `pid` is listening on — the `Browser: node.js/...`
/// counterpart to [`browser_endpoint`]. Returns `None` unless the process was launched `--inspect`.
pub async fn node_endpoint(pid: u32) -> Option<NodeEndpoint> {
    let ports = ports::listening_ports(&[pid]).remove(&pid)?;
    for port in ports {
        if !is_node_inspector(port).await {
            continue;
        }
        // The node inspector's ws_url lives in its target list, not /json/version.
        let Ok(body) = http::get(port, "/json/list").await else {
            continue;
        };
        if let Some(ws_url) = serde_json::from_str::<Vec<RawTarget>>(&body)
            .ok()
            .and_then(|targets| targets.into_iter().find_map(|target| target.ws_url))
        {
            return Some(NodeEndpoint { port, ws_url });
        }
    }
    None
}

async fn is_node_inspector(port: u16) -> bool {
    let Ok(body) = http::get(port, "/json/version").await else {
        return false;
    };
    serde_json::from_str::<VersionInfo>(&body)
        .ok()
        .and_then(|version| version.browser)
        .is_some_and(|browser| browser.starts_with("node.js/"))
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

/// A discovered Instance: a confirmed browser endpoint plus the owning process's canonical Git
/// worktree and instance metadata.
#[derive(Debug, Clone)]
pub struct Instance {
    pub endpoint: BrowserEndpoint,
    pub pid: u32,
    pub worktree_root: Option<PathBuf>,
    pub instance_id: Option<u32>,
}

impl Instance {
    /// A collision-resistant attachment name. Friendly labels are presentation only; the canonical
    /// worktree path owns identity.
    pub fn name(&self) -> String {
        match &self.worktree_root {
            Some(root) => format!("{}-{:016x}", self.display_name(), stable_path_hash(root)),
            None => format!("{}-{}", self.display_name(), self.endpoint.port),
        }
    }

    pub fn display_name(&self) -> String {
        self.worktree_root
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| sanitize(&name.to_string_lossy()))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| match self.instance_id {
                Some(id) => format!("{}-{id}", sanitize(&self.endpoint.app)),
                None => sanitize(&self.endpoint.app),
            })
    }

    /// Whether this Instance answers to a selector — internal name, app name, worktree label or
    /// path, instance id, or exact port.
    pub fn matches(&self, selector: &str) -> bool {
        let needle = selector.to_lowercase();
        self.name().to_lowercase() == needle
            || self.endpoint.app.to_lowercase().contains(&needle)
            || self.display_name().to_lowercase().contains(&needle)
            || self
                .worktree_root
                .as_deref()
                .is_some_and(|root| root.to_string_lossy().to_lowercase().contains(&needle))
            || self.instance_id.is_some_and(|id| id.to_string() == selector)
            || self.instance_id.is_some_and(|id| format!("instance-{id}") == needle)
            || self.endpoint.port.to_string() == selector
    }
}

/// Sweep every listening localhost port, keep the ones that are Chromium browser endpoints, and
/// enrich each with its owning process's dev metadata.
pub async fn discover(repositories: &RepositoryLocator) -> Vec<Instance> {
    discover_candidates(repositories, ports::all_listening()).await
}

/// Discover Chromium endpoints owned by processes whose nearest Git root is `worktree_root`.
pub async fn discover_in_worktree(
    repositories: &RepositoryLocator,
    worktree_root: &Path,
) -> Vec<Instance> {
    let pids = process_ids_in_worktree(repositories, worktree_root);
    let mut candidates: Vec<(u16, u32)> = ports::listening_ports(&pids)
        .into_iter()
        .flat_map(|(pid, ports)| ports.into_iter().map(move |port| (port, pid)))
        .collect();
    candidates.sort_unstable();
    candidates.dedup_by_key(|(port, _)| *port);
    discover_candidates(repositories, candidates).await
}

/// Discover one exact port without sweeping every listener.
pub async fn discover_port(repositories: &RepositoryLocator, port: u16) -> Option<Instance> {
    let pid = ports::owner_pid(port)?;
    instance_at(repositories, port, pid).await
}

async fn discover_candidates(
    repositories: &RepositoryLocator,
    candidates: Vec<(u16, u32)>,
) -> Vec<Instance> {
    futures_util::stream::iter(candidates)
        .map(|(port, pid)| async move { instance_at(repositories, port, pid).await })
        .buffer_unordered(PROBE_CONCURRENCY)
        .filter_map(|instance| async move { instance })
        .collect()
        .await
}

async fn instance_at(repositories: &RepositoryLocator, port: u16, pid: u32) -> Option<Instance> {
    let endpoint = probe_browser_endpoint(port).await?;
    Some(Instance {
        endpoint,
        pid,
        worktree_root: worktree_root_of(repositories, pid),
        instance_id: instance_id_of(pid),
    })
}

fn worktree_root_of(repositories: &RepositoryLocator, pid: u32) -> Option<PathBuf> {
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    repositories.nearest_worktree_root(&cwd).ok().map(|root| root.as_path().to_path_buf())
}

#[cfg(test)]
fn nearest_git_root(path: &Path) -> Option<PathBuf> {
    RepositoryLocator::new()
        .nearest_worktree_root(path)
        .ok()
        .map(|root| root.as_path().to_path_buf())
}

fn process_ids_in_worktree(repositories: &RepositoryLocator, worktree_root: &Path) -> Vec<u32> {
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    processes
        .flatten()
        .filter_map(|process| process.file_name().to_string_lossy().parse::<u32>().ok())
        .filter(|pid| worktree_root_of(repositories, *pid).as_deref() == Some(worktree_root))
        .collect()
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

fn stable_path_hash(path: &Path) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    path.as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(OFFSET_BASIS, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(PRIME))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_git_root_prefers_the_nested_worktree_boundary() {
        let root = std::env::temp_dir().join(format!(
            "kit-cdp-worktree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let nested = root.join(".worktrees/feature");
        let package = nested.join("packages/app");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(nested.join(".git"), "gitdir: elsewhere").unwrap();

        assert_eq!(nearest_git_root(&package), Some(nested));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn instance_matches_its_printed_name() {
        let instance = Instance {
            endpoint: BrowserEndpoint {
                port: 9223,
                ws_url: "ws://localhost".to_owned(),
                app: "editor-dev".to_owned(),
                user_agent: String::new(),
            },
            pid: 42,
            worktree_root: Some(PathBuf::from("/repo/.worktrees/driver")),
            instance_id: Some(8),
        };

        assert!(instance.matches(&instance.name()));
        assert!(instance.matches("driver"));
        assert!(instance.matches("8"));
        assert!(instance.matches("9223"));
    }
}
