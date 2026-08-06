use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use wezterm_codec::BuildIdentity;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConsoleRecovery {
    AuthenticateTailscale,
    InstallTailscale,
    StartTailscale,
    RestoreTailscaleAccess,
    UpdateTailscale,
    BringPeerOnline,
    RetryWithUnixUser { machine: String },
    AuthenticateSsh,
    InspectAndRetry,
    UpdateRemoteKit,
    InstallKit,
    RunSetup,
    RestoreServiceManager,
    RemoveForeignServiceDefinition,
    InspectServiceLog,
    RemoveRejectedSocket,
    CloseSessions,
    Retry,
}

impl std::fmt::Display for ConsoleRecovery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::AuthenticateTailscale => "tailscale login".to_owned(),
            Self::InstallTailscale => "install the Tailscale CLI and retry".to_owned(),
            Self::StartTailscale => "start Tailscale and retry".to_owned(),
            Self::RestoreTailscaleAccess => {
                "restore access to the local Tailscale daemon and retry".to_owned()
            }
            Self::UpdateTailscale => "update Tailscale and retry".to_owned(),
            Self::BringPeerOnline => "bring the machine online and retry".to_owned(),
            Self::RetryWithUnixUser { machine } => format!("retry as USER@{machine}"),
            Self::AuthenticateSsh => "open the link, authenticate, then retry".to_owned(),
            Self::InspectAndRetry => {
                "inspect the details, correct the failing layer, and retry".to_owned()
            }
            Self::UpdateRemoteKit => "update Kit on the machine and run setup again".to_owned(),
            Self::InstallKit => "install Kit and configure Console".to_owned(),
            Self::RunSetup => "kit console setup".to_owned(),
            Self::RestoreServiceManager => {
                "restore the logged-in user service manager, then run kit console status".to_owned()
            }
            Self::RemoveForeignServiceDefinition => {
                "remove the foreign service definition, then run kit console setup".to_owned()
            }
            Self::InspectServiceLog => {
                "inspect the Console service log, then run kit console setup".to_owned()
            }
            Self::RemoveRejectedSocket => {
                "stop Console, remove only the rejected owned socket, and run setup".to_owned()
            }
            Self::CloseSessions => {
                "close the active sessions, then run kit console setup again".to_owned()
            }
            Self::Retry => {
                "wait for the current Console operation to finish, then retry".to_owned()
            }
        };
        formatter.write_str(&text)
    }
}

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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteFailureKind {
    OpenSshUnavailable,
    HostKeyMismatch,
    Transport,
    RemoteCommand,
    EmptyOutput,
    Decode,
    Timeout,
    Supervision,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsoleStage {
    Transport,
    RemoteCommand,
    Decode,
    Supervision,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum ConsoleStatus {
    NeedsTailscaleLogin,
    TailscaleCliUnavailable {
        detail: String,
    },
    TailscaleDaemonUnavailable {
        detail: String,
    },
    TailscalePermissionDenied {
        detail: String,
    },
    TailscaleUnsupported {
        detail: String,
    },
    PeerOffline {
        machine: String,
    },
    NeedsUnixUser {
        machine: String,
        stable_node_id: String,
    },
    NeedsSshAuthentication {
        machine: String,
        url: String,
    },
    RemoteFailure {
        machine: String,
        stage: ConsoleStage,
        kind: RemoteFailureKind,
        detail: String,
    },
    KitUnavailable {
        machine: String,
    },
    NotInstalled {
        platform: ConsoleServicePlatform,
    },
    Stopped {
        platform: ConsoleServicePlatform,
    },
    ServiceFailed {
        platform: ConsoleServicePlatform,
        detail: String,
    },
    ServiceUnavailable {
        platform: ConsoleServicePlatform,
        detail: String,
    },
    WrongOwner {
        platform: ConsoleServicePlatform,
        path: PathBuf,
        expected_uid: u32,
        actual_uid: u32,
    },
    SocketMissing {
        platform: ConsoleServicePlatform,
        path: PathBuf,
    },
    SocketStale {
        platform: ConsoleServicePlatform,
        path: PathBuf,
        detail: String,
    },
    SocketRejected {
        platform: ConsoleServicePlatform,
        path: PathBuf,
        detail: String,
    },
    CodecIncompatible {
        platform: ConsoleServicePlatform,
        server_version: String,
        server_codec: usize,
    },
    BuildIncompatible {
        platform: ConsoleServicePlatform,
        sessions: Option<usize>,
        expected: BuildIdentity,
        actual: BuildIdentity,
    },
    ActivationDeferred {
        platform: ConsoleServicePlatform,
        sessions: usize,
    },
    RepairBusy {
        platform: ConsoleServicePlatform,
    },
    MuxUnavailable {
        platform: ConsoleServicePlatform,
        detail: String,
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
        let fact = match self {
            Self::NeedsTailscaleLogin => "Tailscale authentication required".to_owned(),
            Self::TailscaleCliUnavailable { detail } => {
                format!("Tailscale CLI unavailable\n{detail}")
            }
            Self::TailscaleDaemonUnavailable { detail } => {
                format!("Tailscale daemon unavailable\n{detail}")
            }
            Self::TailscalePermissionDenied { detail } => {
                format!("Tailscale permission denied\n{detail}")
            }
            Self::TailscaleUnsupported { detail } => {
                format!("unsupported Tailscale state\n{detail}")
            }
            Self::PeerOffline { machine } => format!("offline — {machine}"),
            Self::NeedsUnixUser { machine, stable_node_id } => {
                format!("Unix user required — {machine} ({stable_node_id})")
            }
            Self::NeedsSshAuthentication { machine, url } => {
                format!("SSH authentication required — {machine}\n{url}")
            }
            Self::RemoteFailure { machine, kind, detail, .. } => {
                format!("{} — {machine}\n{detail}", kind.label())
            }
            Self::KitUnavailable { machine } => format!("Kit is not installed — {machine}"),
            Self::NotInstalled { platform } => format!("not installed — {}", platform.label()),
            Self::Stopped { platform } => format!("stopped — {}", platform.label()),
            Self::ServiceFailed { platform, detail } => {
                format!("failed — {}\n{detail}", platform.label())
            }
            Self::ServiceUnavailable { platform, detail } => {
                format!("unavailable — {}\n{detail}", platform.label())
            }
            Self::WrongOwner { platform, path, expected_uid, actual_uid } => format!(
                "rejected — {}\n{} is owned by uid {actual_uid}; expected uid {expected_uid}",
                platform.label(),
                path.display()
            ),
            Self::SocketMissing { platform, path } => format!(
                "starting — {}\nagent socket {} is missing",
                platform.label(),
                path.display()
            ),
            Self::SocketStale { platform, path, detail } => format!(
                "stale — {} is stopped\nowned socket {} remains\n{detail}",
                platform.label(),
                path.display()
            ),
            Self::SocketRejected { platform, path, detail } => format!(
                "rejected — {}\nagent socket {}: {detail}",
                platform.label(),
                path.display()
            ),
            Self::CodecIncompatible { platform, server_version, server_codec } => format!(
                "incompatible — {}\nserver {server_version} uses codec {server_codec}",
                platform.label()
            ),
            Self::BuildIncompatible { platform, sessions, expected, actual } => {
                let sessions = sessions
                    .map(|sessions| format!("\nactive sessions: {sessions}"))
                    .unwrap_or_default();
                format!(
                    "update required — {}{sessions}\nexpected {expected:?}\nactual   {actual:?}",
                    platform.label()
                )
            }
            Self::ActivationDeferred { platform, sessions } => {
                format!("activation deferred — {}\nactive sessions: {sessions}", platform.label())
            }
            Self::RepairBusy { platform } => format!("repair busy — {}", platform.label()),
            Self::MuxUnavailable { platform, detail } => {
                format!("unavailable — {} mux\n{detail}", platform.label())
            }
            Self::Ready { platform, sessions, .. } => {
                let suffix = if *sessions == 1 { "session" } else { "sessions" };
                format!("ready — {} — {sessions} {suffix}", platform.label())
            }
        };
        match self.recovery() {
            Some(recovery) => format!("{fact}\nnext: {recovery}"),
            None => fact,
        }
    }

    pub(crate) fn recovery(&self) -> Option<ConsoleRecovery> {
        match self {
            Self::NeedsTailscaleLogin => Some(ConsoleRecovery::AuthenticateTailscale),
            Self::TailscaleCliUnavailable { .. } => Some(ConsoleRecovery::InstallTailscale),
            Self::TailscaleDaemonUnavailable { .. } => Some(ConsoleRecovery::StartTailscale),
            Self::TailscalePermissionDenied { .. } => Some(ConsoleRecovery::RestoreTailscaleAccess),
            Self::TailscaleUnsupported { .. } => Some(ConsoleRecovery::UpdateTailscale),
            Self::PeerOffline { .. } => Some(ConsoleRecovery::BringPeerOnline),
            Self::NeedsUnixUser { machine, .. } => {
                Some(ConsoleRecovery::RetryWithUnixUser { machine: machine.clone() })
            }
            Self::NeedsSshAuthentication { .. } => Some(ConsoleRecovery::AuthenticateSsh),
            Self::RemoteFailure {
                kind:
                    RemoteFailureKind::HostKeyMismatch
                    | RemoteFailureKind::Transport
                    | RemoteFailureKind::Timeout,
                ..
            } => Some(ConsoleRecovery::BringPeerOnline),
            Self::RemoteFailure {
                kind:
                    RemoteFailureKind::RemoteCommand
                    | RemoteFailureKind::EmptyOutput
                    | RemoteFailureKind::Decode,
                ..
            } => Some(ConsoleRecovery::RunSetup),
            Self::RemoteFailure { .. } => Some(ConsoleRecovery::InspectAndRetry),
            Self::KitUnavailable { .. } => Some(ConsoleRecovery::InstallKit),
            Self::NotInstalled { .. }
            | Self::Stopped { .. }
            | Self::ServiceFailed { .. }
            | Self::SocketMissing { .. }
            | Self::SocketStale { .. } => Some(ConsoleRecovery::RunSetup),
            Self::ServiceUnavailable { .. } => Some(ConsoleRecovery::RestoreServiceManager),
            Self::WrongOwner { .. } => Some(ConsoleRecovery::RemoveForeignServiceDefinition),
            Self::SocketRejected { .. } => Some(ConsoleRecovery::RemoveRejectedSocket),
            Self::CodecIncompatible { .. } => Some(ConsoleRecovery::InspectServiceLog),
            Self::BuildIncompatible { sessions: Some(0), .. } => {
                Some(ConsoleRecovery::UpdateRemoteKit)
            }
            Self::BuildIncompatible { .. } | Self::ActivationDeferred { .. } => {
                Some(ConsoleRecovery::CloseSessions)
            }
            Self::RepairBusy { .. } => Some(ConsoleRecovery::Retry),
            Self::MuxUnavailable { .. } => Some(ConsoleRecovery::RunSetup),
            Self::Ready { .. } => None,
        }
    }
}

impl RemoteFailureKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::OpenSshUnavailable => "OpenSSH unavailable",
            Self::HostKeyMismatch => "remote host key changed",
            Self::Transport => "connection failed",
            Self::RemoteCommand => "remote command failed",
            Self::EmptyOutput => "remote command returned no status",
            Self::Decode => "remote status could not be decoded",
            Self::Timeout => "remote command timed out",
            Self::Supervision => "remote command supervision failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConsoleServicePlatform, ConsoleStatus, RemoteFailureKind};

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

    #[test]
    fn remote_failure_wire_state_preserves_the_failing_layer_and_evidence() {
        let status = ConsoleStatus::RemoteFailure {
            machine: "tvxm".to_owned(),
            stage: super::ConsoleStage::RemoteCommand,
            kind: RemoteFailureKind::RemoteCommand,
            detail: "Decode limit exceeded".to_owned(),
        };
        let encoded = serde_json::to_string(&status).unwrap();
        let decoded = serde_json::from_str::<ConsoleStatus>(&encoded).unwrap();

        assert_eq!(decoded, status);
        assert!(status.text().contains("remote command failed"));
        assert!(status.text().contains("Decode limit exceeded"));
    }
}
