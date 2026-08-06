use std::{collections::BTreeMap, net::IpAddr};

use serde::Deserialize;
use thiserror::Error;

/// Stable Tailscale node identity projected from `tailscale status --json`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Node {
    pub id: String,
    pub dns_name: String,
    pub host_name: String,
    pub operating_system: OperatingSystem,
    pub online: bool,
    pub addresses: Vec<IpAddr>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperatingSystem {
    Linux,
    Macos,
    Unsupported(String),
    Unknown,
}

impl OperatingSystem {
    pub fn label(&self) -> &str {
        match self {
            Self::Linux => "Linux",
            Self::Macos => "macOS",
            Self::Unsupported(label) => label,
            Self::Unknown => "unknown OS",
        }
    }

    fn from_tailnet_label(label: String) -> Self {
        match label.to_ascii_lowercase().as_str() {
            "linux" => Self::Linux,
            "macos" | "darwin" => Self::Macos,
            "" => Self::Unknown,
            _ => Self::Unsupported(label),
        }
    }
}

impl Node {
    pub fn display_name(&self) -> &str {
        if !self.host_name.is_empty() {
            &self.host_name
        } else if !self.dns_name.is_empty() {
            &self.dns_name
        } else {
            &self.id
        }
    }

    fn matches_stable_selector(&self, selector: &str, address: Option<IpAddr>) -> bool {
        self.id == selector
            || self.dns_name.eq_ignore_ascii_case(selector.trim_end_matches('.'))
            || address.is_some_and(|address| self.addresses.contains(&address))
    }

    fn matches_dns_label(&self, selector: &str) -> bool {
        self.dns_name.split('.').next().is_some_and(|label| label.eq_ignore_ascii_case(selector))
    }

    fn matches_host_name(&self, selector: &str) -> bool {
        self.host_name.eq_ignore_ascii_case(selector)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Status {
    pub local: Node,
    pub peers: Vec<Node>,
}

impl Status {
    /// Resolves one exact peer identity without prefix, substring, or first-match guessing.
    pub fn resolve_peer(&self, selector: &str) -> Result<&Node, PeerSelectorError> {
        let selector = selector.trim();
        if selector.is_empty() {
            return Err(PeerSelectorError::Empty);
        }
        let address = selector.parse::<IpAddr>().ok();
        let matches = [
            Node::matches_stable_selector as fn(&Node, &str, Option<IpAddr>) -> bool,
            |peer: &Node, selector: &str, _| peer.matches_dns_label(selector),
            |peer: &Node, selector: &str, _| peer.matches_host_name(selector),
        ]
        .into_iter()
        .map(|matches| {
            self.peers.iter().filter(|peer| matches(peer, selector, address)).collect::<Vec<_>>()
        })
        .find(|matches| !matches.is_empty())
        .unwrap_or_default();
        match matches.as_slice() {
            [] => Err(PeerSelectorError::NotFound { selector: selector.to_owned() }),
            [peer] => Ok(peer),
            peers => {
                let mut candidates = peers
                    .iter()
                    .map(|peer| {
                        if peer.dns_name.is_empty() {
                            peer.display_name().to_owned()
                        } else {
                            peer.dns_name.clone()
                        }
                    })
                    .collect::<Vec<_>>();
                candidates.sort_unstable();
                candidates.dedup();
                Err(PeerSelectorError::Ambiguous { selector: selector.to_owned(), candidates })
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Readiness {
    Ready(Status),
    NeedsLogin,
    CliUnavailable(String),
    DaemonUnavailable(String),
    PermissionDenied(String),
    Unsupported(String),
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PeerSelectorError {
    #[error("Tailscale peer selector cannot be empty")]
    Empty,
    #[error("no Tailscale peer exactly matches {selector:?}")]
    NotFound { selector: String },
    #[error("Tailscale peer selector {selector:?} is ambiguous: {candidates:?}")]
    Ambiguous { selector: String, candidates: Vec<String> },
}

#[derive(Debug, Error)]
pub enum StatusParseError {
    #[error("parse tailscale status JSON")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawStatus {
    #[serde(default)]
    backend_state: String,
    #[serde(default, rename = "TailscaleIPs")]
    tailscale_ips: Vec<IpAddr>,
    #[serde(rename = "Self")]
    local: Option<RawNode>,
    #[serde(default, rename = "Peer")]
    peers: BTreeMap<String, RawNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawNode {
    #[serde(default, rename = "ID")]
    id: String,
    #[serde(default, rename = "DNSName")]
    dns_name: String,
    #[serde(default)]
    host_name: String,
    #[serde(default, rename = "OS")]
    os: String,
    #[serde(default)]
    online: bool,
    #[serde(default, rename = "TailscaleIPs")]
    tailscale_ips: Vec<IpAddr>,
}

impl RawNode {
    fn into_node(self) -> Node {
        Node {
            id: self.id,
            dns_name: self.dns_name.trim_end_matches('.').to_owned(),
            host_name: self.host_name,
            operating_system: OperatingSystem::from_tailnet_label(self.os),
            online: self.online,
            addresses: self.tailscale_ips,
        }
    }
}

pub fn parse_status(bytes: &[u8]) -> Result<Readiness, StatusParseError> {
    let raw: RawStatus = serde_json::from_slice(bytes)?;
    Ok(classify(raw))
}

fn classify(raw: RawStatus) -> Readiness {
    match raw.backend_state.as_str() {
        "NeedsLogin" => return Readiness::NeedsLogin,
        "Running" if raw.tailscale_ips.is_empty() => return Readiness::NeedsLogin,
        "Running" => {}
        "Stopped" | "NoState" => {
            return Readiness::DaemonUnavailable(format!(
                "Tailscale backend is {}",
                raw.backend_state
            ));
        }
        state => {
            return Readiness::Unsupported(format!(
                "unsupported Tailscale backend state {state:?}"
            ));
        }
    }
    let Some(local) = raw.local else {
        return Readiness::DaemonUnavailable(
            "Tailscale status did not include this device".to_owned(),
        );
    };
    let mut local = local.into_node();
    local.addresses = raw.tailscale_ips;
    let mut peers = raw.peers.into_values().map(RawNode::into_node).collect::<Vec<_>>();
    peers.sort_by_key(|peer| (!peer.online, peer.display_name().to_lowercase()));
    Readiness::Ready(Status { local, peers })
}

#[cfg(test)]
mod tests {
    use super::*;

    const READY_STATUS: &[u8] = br#"{"BackendState":"Running","TailscaleIPs":["100.64.0.1"],"Self":{"ID":"me","DNSName":"desktop.test.ts.net.","HostName":"desktop","OS":"linux","Online":true},"Peer":{"one":{"ID":"peer-one","DNSName":"shared.one.ts.net.","HostName":"shared","OS":"linux","Online":true,"TailscaleIPs":["100.64.0.2"],"sshHostKeys":["ssh-ed25519 AAAA"]},"two":{"ID":"peer-two","DNSName":"shared.two.ts.net.","HostName":"shared","OS":"macOS","Online":false,"TailscaleIPs":["fd7a:115c:a1e0::2"]}}}"#;

    #[test]
    fn running_status_projects_stable_nodes_and_root_local_address() {
        let Readiness::Ready(status) = parse_status(READY_STATUS).unwrap() else {
            panic!("status was not ready")
        };
        assert_eq!(status.local.dns_name, "desktop.test.ts.net");
        assert_eq!(status.local.addresses[0].to_string(), "100.64.0.1");
        assert_eq!(status.peers[0].id, "peer-one");
        assert_eq!(status.peers[1].operating_system, OperatingSystem::Macos);
    }

    #[test]
    fn readiness_distinguishes_login_daemon_and_unknown_backend_states() {
        assert_eq!(parse_status(br#"{"BackendState":"Running"}"#).unwrap(), Readiness::NeedsLogin);
        assert!(matches!(
            parse_status(br#"{"BackendState":"Stopped"}"#).unwrap(),
            Readiness::DaemonUnavailable(_)
        ));
        assert!(matches!(
            parse_status(br#"{"BackendState":"Starting"}"#).unwrap(),
            Readiness::Unsupported(_)
        ));
    }

    #[test]
    fn peer_resolution_is_exact_case_tolerant_and_ambiguity_safe() {
        let Readiness::Ready(status) = parse_status(READY_STATUS).unwrap() else {
            panic!("status was not ready")
        };
        assert_eq!(status.resolve_peer("SHARED.ONE.TS.NET.").unwrap().id, "peer-one");
        assert_eq!(status.resolve_peer("100.64.0.2").unwrap().id, "peer-one");
        assert_eq!(status.resolve_peer("FD7A:115C:A1E0::2").unwrap().id, "peer-two");
        assert!(matches!(
            status.resolve_peer("shared"),
            Err(PeerSelectorError::Ambiguous { candidates, .. }) if candidates.len() == 2
        ));
        assert!(matches!(status.resolve_peer("share"), Err(PeerSelectorError::NotFound { .. })));
    }
}
