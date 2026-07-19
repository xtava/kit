use serde::Deserialize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub dns_name: String,
    pub os: String,
    pub online: bool,
    pub addresses: Vec<String>,
    pub taildrop_target: Option<String>,
}

impl Device {
    pub fn send_target(&self) -> Option<&str> {
        self.taildrop_target.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Readiness {
    Ready { local: Device, peers: Vec<Device> },
    NeedsLogin,
    CliUnavailable(String),
    DaemonUnavailable(String),
    PermissionDenied(String),
    Unsupported(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct RawStatus {
    #[serde(default)]
    pub backend_state: String,
    #[serde(default)]
    pub tailscale_i_ps: Vec<String>,
    #[serde(rename = "Self")]
    pub local: Option<RawDevice>,
    #[serde(default, rename = "Peer")]
    pub peers: std::collections::BTreeMap<String, RawDevice>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct RawDevice {
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
    tailscale_i_ps: Vec<String>,
}

impl RawDevice {
    fn into_device(self) -> Device {
        let addresses = self.tailscale_i_ps;
        Device {
            id: self.id,
            name: self.host_name,
            dns_name: if self.dns_name.is_empty() {
                addresses.first().cloned().unwrap_or_default()
            } else {
                self.dns_name.trim_end_matches('.').to_owned()
            },
            os: self.os,
            online: self.online,
            addresses,
            taildrop_target: None,
        }
    }
}

impl RawStatus {
    pub fn readiness(self) -> Readiness {
        match self.backend_state.as_str() {
            "NeedsLogin" => return Readiness::NeedsLogin,
            "Running" if self.tailscale_i_ps.is_empty() => return Readiness::NeedsLogin,
            "Running" => {}
            "Stopped" | "NoState" => {
                return Readiness::DaemonUnavailable(format!(
                    "Tailscale backend is {}",
                    self.backend_state
                ));
            }
            state => {
                return Readiness::Unsupported(format!(
                    "unsupported Tailscale backend state {state:?}"
                ));
            }
        }
        let Some(local) = self.local else {
            return Readiness::DaemonUnavailable(
                "Tailscale status did not include this device".into(),
            );
        };
        let mut peers = self.peers.into_values().map(RawDevice::into_device).collect::<Vec<_>>();
        peers.sort_by_key(|peer| (!peer.online, peer.name.to_lowercase()));
        Readiness::Ready { local: local.into_device(), peers }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_requires_an_ip_and_sorts_online_peers_first() {
        let raw: RawStatus = serde_json::from_str(
            r#"{"BackendState":"Running","TailscaleIPs":["100.1.2.3"],"Self":{"ID":"me","DNSName":"me.ts.net.","HostName":"me","OS":"linux","Online":true},"Peer":{"b":{"ID":"b","DNSName":"b.ts.net.","HostName":"B","OS":"macOS","Online":false},"a":{"ID":"a","DNSName":"a.ts.net.","HostName":"A","OS":"linux","Online":true}}}"#,
        )
        .unwrap();
        let Readiness::Ready { local, peers } = raw.readiness() else { panic!("not ready") };
        assert_eq!(local.dns_name, "me.ts.net");
        assert_eq!(peers.iter().map(|peer| peer.name.as_str()).collect::<Vec<_>>(), ["A", "B"]);
    }

    #[test]
    fn running_without_an_ip_still_needs_login() {
        let raw: RawStatus = serde_json::from_str(r#"{"BackendState":"Running"}"#).unwrap();
        assert_eq!(raw.readiness(), Readiness::NeedsLogin);
    }

    #[test]
    fn stopped_backend_is_not_treated_as_a_login_request() {
        let raw: RawStatus = serde_json::from_str(r#"{"BackendState":"Stopped"}"#).unwrap();
        assert!(matches!(raw.readiness(), Readiness::DaemonUnavailable(_)));
    }
}
