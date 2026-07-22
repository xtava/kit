use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use wezterm_codec::BuildIdentity;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsoleServicePlatform {
    LinuxSystemdUser,
    MacosLaunchAgent,
}

impl ConsoleServicePlatform {
    pub const fn label(self) -> &'static str {
        match self {
            Self::LinuxSystemdUser => "systemd user service",
            Self::MacosLaunchAgent => "macOS LaunchAgent",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeServiceState {
    NotInstalled,
    Stopped,
    Running,
    Failed { detail: String },
    Unavailable { detail: String },
    WrongOwner { path: PathBuf, expected_uid: u32, actual_uid: u32 },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum ConsoleStatus {
    NeedsTailscaleLogin {
        action: String,
    },
    TailscaleCliUnavailable {
        detail: String,
        action: String,
    },
    TailscaleDaemonUnavailable {
        detail: String,
        action: String,
    },
    TailscalePermissionDenied {
        detail: String,
        action: String,
    },
    TailscaleUnsupported {
        detail: String,
        action: String,
    },
    PeerOffline {
        machine: String,
        action: String,
    },
    NeedsUnixUser {
        machine: String,
        stable_node_id: String,
        action: String,
    },
    NeedsSshAuthentication {
        machine: String,
        url: String,
        action: String,
    },
    TransportFailed {
        machine: String,
        action: String,
    },
    NotInstalled {
        platform: ConsoleServicePlatform,
        action: String,
    },
    Stopped {
        platform: ConsoleServicePlatform,
        action: String,
    },
    ServiceFailed {
        platform: ConsoleServicePlatform,
        detail: String,
        action: String,
    },
    ServiceUnavailable {
        platform: ConsoleServicePlatform,
        detail: String,
        action: String,
    },
    WrongOwner {
        platform: ConsoleServicePlatform,
        path: PathBuf,
        expected_uid: u32,
        actual_uid: u32,
        action: String,
    },
    SocketMissing {
        platform: ConsoleServicePlatform,
        path: PathBuf,
        action: String,
    },
    SocketStale {
        platform: ConsoleServicePlatform,
        path: PathBuf,
        detail: String,
        action: String,
    },
    SocketRejected {
        platform: ConsoleServicePlatform,
        path: PathBuf,
        detail: String,
        action: String,
    },
    CodecIncompatible {
        platform: ConsoleServicePlatform,
        server_version: String,
        server_codec: usize,
        action: String,
    },
    BuildIncompatible {
        platform: ConsoleServicePlatform,
        expected: BuildIdentity,
        actual: BuildIdentity,
        action: String,
    },
    MuxUnavailable {
        platform: ConsoleServicePlatform,
        detail: String,
        action: String,
    },
    Ready {
        platform: ConsoleServicePlatform,
        sessions: usize,
        build: BuildIdentity,
    },
}

impl ConsoleStatus {
    pub const fn ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }

    pub fn text(&self) -> String {
        match self {
            Self::NeedsTailscaleLogin { action } => {
                format!("Tailscale authentication required\nnext: {action}")
            }
            Self::TailscaleCliUnavailable { detail, action } => {
                format!("Tailscale CLI unavailable\n{detail}\nnext: {action}")
            }
            Self::TailscaleDaemonUnavailable { detail, action } => {
                format!("Tailscale daemon unavailable\n{detail}\nnext: {action}")
            }
            Self::TailscalePermissionDenied { detail, action } => {
                format!("Tailscale permission denied\n{detail}\nnext: {action}")
            }
            Self::TailscaleUnsupported { detail, action } => {
                format!("unsupported Tailscale state\n{detail}\nnext: {action}")
            }
            Self::PeerOffline { machine, action } => {
                format!("offline — {machine}\nnext: {action}")
            }
            Self::NeedsUnixUser { machine, stable_node_id, action } => format!(
                "Unix user required — {machine} ({stable_node_id})\nnext: {action}"
            ),
            Self::NeedsSshAuthentication { machine, url, action } => {
                format!("SSH authentication required — {machine}\n{url}\nnext: {action}")
            }
            Self::TransportFailed { machine, action } => {
                format!("transport failed — {machine}\nnext: {action}")
            }
            Self::NotInstalled { platform, action } => {
                format!("not installed — {}\nnext: {action}", platform.label())
            }
            Self::Stopped { platform, action } => {
                format!("stopped — {}\nnext: {action}", platform.label())
            }
            Self::ServiceFailed { platform, detail, action } => {
                format!("failed — {}\n{detail}\nnext: {action}", platform.label())
            }
            Self::ServiceUnavailable { platform, detail, action } => {
                format!("unavailable — {}\n{detail}\nnext: {action}", platform.label())
            }
            Self::WrongOwner { platform, path, expected_uid, actual_uid, action } => format!(
                "rejected — {}\n{} is owned by uid {actual_uid}; expected uid {expected_uid}\nnext: {action}",
                platform.label(),
                path.display()
            ),
            Self::SocketMissing { platform, path, action } => format!(
                "starting — {}\nagent socket {} is missing\nnext: {action}",
                platform.label(),
                path.display()
            ),
            Self::SocketStale { platform, path, detail, action } => format!(
                "stale — {} is stopped\nowned socket {} remains\n{detail}\nnext: {action}",
                platform.label(),
                path.display()
            ),
            Self::SocketRejected { platform, path, detail, action } => format!(
                "rejected — {}\nagent socket {}: {detail}\nnext: {action}",
                platform.label(),
                path.display()
            ),
            Self::CodecIncompatible {
                platform,
                server_version,
                server_codec,
                action,
            } => format!(
                "incompatible — {}\nserver {server_version} uses codec {server_codec}\nnext: {action}",
                platform.label()
            ),
            Self::BuildIncompatible { platform, expected, actual, action } => format!(
                "update required — {}\nexpected {expected:?}\nactual   {actual:?}\nnext: {action}",
                platform.label()
            ),
            Self::MuxUnavailable { platform, detail, action } => {
                format!("unavailable — {} mux\n{detail}\nnext: {action}", platform.label())
            }
            Self::Ready { platform, sessions, .. } => {
                let suffix = if *sessions == 1 { "session" } else { "sessions" };
                format!("ready — {} — {sessions} {suffix}", platform.label())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ConsoleServicePlatform;

    #[test]
    fn service_platform_wire_values_decode_on_every_supported_client() {
        assert_eq!(
            serde_json::from_str::<ConsoleServicePlatform>("\"linux-systemd-user\"").unwrap(),
            ConsoleServicePlatform::LinuxSystemdUser
        );
        assert_eq!(
            serde_json::from_str::<ConsoleServicePlatform>("\"macos-launch-agent\"").unwrap(),
            ConsoleServicePlatform::MacosLaunchAgent
        );
    }
}
