use std::{ffi::OsString, net::IpAddr};

use thiserror::Error;

use crate::framework::process::{LeaderExitObservation, ProcessFailureKind};

const MAX_STABLE_NODE_ID_BYTES: usize = 128;
const MAX_SSH_USER_BYTES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RelayTarget {
    stable_node_id: String,
    ssh_user: String,
    tailscale_ip: IpAddr,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum RelayTargetError {
    #[error("the stable Tailscale node ID is invalid")]
    StableNodeId,
    #[error("the SSH user is invalid")]
    SshUser,
    #[error("the address is not a Tailscale IP address")]
    TailscaleIp,
    #[error("a remote command argument is not an injection-safe OpenSSH token")]
    RemoteArgument,
}

impl RelayTarget {
    pub(crate) fn new(
        stable_node_id: impl Into<String>,
        ssh_user: impl Into<String>,
        tailscale_ip: IpAddr,
    ) -> Result<Self, RelayTargetError> {
        let stable_node_id = stable_node_id.into();
        if !valid_identifier(&stable_node_id, MAX_STABLE_NODE_ID_BYTES, false) {
            return Err(RelayTargetError::StableNodeId);
        }

        let ssh_user = ssh_user.into();
        if !valid_identifier(&ssh_user, MAX_SSH_USER_BYTES, true) {
            return Err(RelayTargetError::SshUser);
        }
        if !is_tailscale_ip(tailscale_ip) {
            return Err(RelayTargetError::TailscaleIp);
        }

        Ok(Self { stable_node_id, ssh_user, tailscale_ip })
    }

    /// Build the one canonical OpenSSH policy for Console transport and preflight commands.
    ///
    /// OpenSSH ultimately serializes its trailing command into a remote shell command. Restricting
    /// every token to a closed safe alphabet prevents a caller from turning a remote argument into
    /// shell syntax while still supporting Kit flags and subcommands.
    pub(crate) fn ssh_arguments(
        &self,
        remote_arguments: &[&str],
    ) -> Result<Vec<OsString>, RelayTargetError> {
        if remote_arguments.is_empty() || remote_arguments.iter().any(|value| !safe_remote(value)) {
            return Err(RelayTargetError::RemoteArgument);
        }

        let host_key_alias = format!("HostKeyAlias=kit-console-{}", self.stable_node_id);
        let destination = format!("{}@{}", self.ssh_user, self.tailscale_ip);
        let mut arguments = [
            "-T",
            "-o",
            "RequestTTY=no",
            "-o",
            "ForwardAgent=no",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "PermitLocalCommand=no",
            "-o",
            "ControlMaster=no",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            "StrictHostKeyChecking=accept-new",
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
        arguments.push(OsString::from("-o"));
        arguments.push(OsString::from(host_key_alias));
        arguments.push(OsString::from("--"));
        arguments.push(OsString::from(destination));
        arguments.extend(remote_arguments.iter().map(|value| OsString::from(*value)));
        Ok(arguments)
    }

    pub(crate) fn stable_node_id(&self) -> &str {
        &self.stable_node_id
    }

    pub(crate) fn with_tailscale_ip(&self, tailscale_ip: IpAddr) -> Result<Self, RelayTargetError> {
        Self::new(self.stable_node_id.clone(), self.ssh_user.clone(), tailscale_ip)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RelayEpochOutcome {
    pub epoch: u64,
    pub kind: RelayEpochOutcomeKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RelayEpochOutcomeKind {
    TransportExited { exit: LeaderExitObservation },
    LocalDisconnected,
    Cancelled,
    Failed(RelayEpochFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RelayEpochFailure {
    Preflight,
    Start,
    LocalIo,
    TransportInput,
    TransportOutput,
    Supervision(ProcessFailureKind),
}

fn valid_identifier(value: &str, max_bytes: usize, allow_leading_underscore: bool) -> bool {
    if value.is_empty() || value.len() > max_bytes || !value.is_ascii() {
        return false;
    }
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else { return false };
    if !first.is_ascii_alphanumeric() && !(allow_leading_underscore && first == b'_') {
        return false;
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn safe_remote(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.is_ascii()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':' | b'=')
        })
}

fn is_tailscale_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let [first, second, _, _] = address.octets();
            first == 100 && (64..=127).contains(&second)
        }
        IpAddr::V6(address) => address.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, net::IpAddr};

    use super::{RelayTarget, RelayTargetError};

    #[test]
    fn arguments_apply_the_closed_console_ssh_policy() {
        let target =
            RelayTarget::new("n2abc_DEF-9", "tvx", "100.100.20.30".parse::<IpAddr>().unwrap())
                .unwrap();

        let arguments = target.ssh_arguments(&["kit", "console", "__bridge"]).unwrap();

        assert_eq!(
            arguments,
            [
                "-T",
                "-o",
                "RequestTTY=no",
                "-o",
                "ForwardAgent=no",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "ControlMaster=no",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "HostKeyAlias=kit-console-n2abc_DEF-9",
                "--",
                "tvx@100.100.20.30",
                "kit",
                "console",
                "__bridge",
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn target_rejects_argument_injection_and_non_tailscale_addresses() {
        assert_eq!(
            RelayTarget::new("node;touch", "tvx", "100.64.0.1".parse().unwrap()),
            Err(RelayTargetError::StableNodeId)
        );
        assert_eq!(
            RelayTarget::new("node", "-oProxyCommand=bad", "100.64.0.1".parse().unwrap()),
            Err(RelayTargetError::SshUser)
        );
        assert_eq!(
            RelayTarget::new("node", "tvx", "192.168.1.2".parse().unwrap()),
            Err(RelayTargetError::TailscaleIp)
        );

        let target = RelayTarget::new("node", "tvx", "fd7a:115c:a1e0::1".parse().unwrap()).unwrap();
        assert_eq!(
            target.ssh_arguments(&["kit", "console;whoami"]),
            Err(RelayTargetError::RemoteArgument)
        );
    }
}
