use crate::tailscale;

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

impl From<tailscale::Node> for Device {
    fn from(node: tailscale::Node) -> Self {
        let addresses =
            node.addresses.into_iter().map(|address| address.to_string()).collect::<Vec<_>>();
        let dns_name = if node.dns_name.is_empty() {
            addresses.first().cloned().unwrap_or_default()
        } else {
            node.dns_name
        };
        Self {
            id: node.id,
            name: node.host_name,
            dns_name,
            os: node.os,
            online: node.online,
            addresses,
            taildrop_target: None,
        }
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

impl From<tailscale::Readiness> for Readiness {
    fn from(readiness: tailscale::Readiness) -> Self {
        match readiness {
            tailscale::Readiness::Ready(status) => Self::Ready {
                local: status.local.into(),
                peers: status.peers.into_iter().map(Device::from).collect(),
            },
            tailscale::Readiness::NeedsLogin => Self::NeedsLogin,
            tailscale::Readiness::CliUnavailable(error) => Self::CliUnavailable(error),
            tailscale::Readiness::DaemonUnavailable(error) => Self::DaemonUnavailable(error),
            tailscale::Readiness::PermissionDenied(error) => Self::PermissionDenied(error),
            tailscale::Readiness::Unsupported(error) => Self::Unsupported(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;

    #[test]
    fn shared_node_adapts_into_taildrop_device_without_transport_policy() {
        let device = Device::from(tailscale::Node {
            id: "peer".into(),
            dns_name: "laptop.test.ts.net".into(),
            host_name: "laptop".into(),
            os: "linux".into(),
            online: true,
            addresses: vec!["100.64.0.2".parse::<IpAddr>().unwrap()],
        });
        assert_eq!(device.name, "laptop");
        assert_eq!(device.addresses, ["100.64.0.2"]);
        assert_eq!(device.taildrop_target, None);
    }
}
