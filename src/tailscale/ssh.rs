use std::{
    ffi::OsString,
    fs::{DirBuilder, Metadata},
    net::IpAddr,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use thiserror::Error;

use crate::framework::{
    process::{
        CaptureOverflow, CapturePolicy, CommandSpec, ContainmentRequirement, EnvironmentBase,
        InputPolicy, OutputPolicy, ProcessDeadline, ProcessEnvironment, ProcessLabel, ProcessSpec,
        TerminationPolicy,
    },
    AtomicFileWriter,
};

const MAX_STABLE_NODE_ID_BYTES: usize = 128;
const MAX_UNIX_USER_BYTES: usize = 64;
const MAX_REMOTE_ARGUMENTS: usize = 128;
const MAX_REMOTE_ARGUMENT_BYTES: usize = 4 * 1024;
const MAX_REMOTE_COMMAND_BYTES: usize = 32 * 1024;
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const TERMINATION_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
const CAPTURE_BYTES: std::num::NonZeroUsize = std::num::NonZeroUsize::new(8 * 1024 * 1024).unwrap();

/// A stable Tailscale identity and its current OpenSSH transport address.
///
/// The node ID owns host-key identity. The address is re-resolved from live Tailscale status and
/// may change without changing the host identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TailscaleSshTarget {
    stable_node_id: String,
    unix_user: String,
    tailscale_ip: IpAddr,
}

/// The bounded liveness policy for one of Kit's two OpenSSH process shapes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SshConnectionKind {
    Command,
    Relay,
}

/// One remote program and argv serialized safely for OpenSSH's remote POSIX shell boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteCommand {
    arguments: Vec<String>,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum TailscaleSshError {
    #[error("the stable Tailscale node ID is invalid")]
    StableNodeId,
    #[error("the Unix user is invalid")]
    UnixUser,
    #[error("the address is not a Tailscale IP address")]
    TailscaleIp,
    #[error("a remote command requires at least one argument")]
    EmptyRemoteCommand,
    #[error("the remote command has too many arguments")]
    TooManyRemoteArguments,
    #[error("a remote command argument is invalid or too large")]
    RemoteArgument,
    #[error("the serialized remote command is too large")]
    RemoteCommand,
    #[error("the OpenSSH known-hosts path is invalid")]
    KnownHostsPath,
}

#[derive(Debug, Error)]
pub enum TailscaleSshStateError {
    #[error("the Kit state directory is unavailable")]
    StateDirectory,
    #[error("prepare the private Tailscale SSH state directory {path}")]
    PrepareDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("inspect the Tailscale SSH state path {path}")]
    InspectPath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the Tailscale SSH state path {path} must be an owned private directory")]
    UnsafeDirectory { path: PathBuf },
    #[error("the Tailscale SSH known-hosts path {path} must be an owned private regular file")]
    UnsafeKnownHosts { path: PathBuf },
    #[error("the Tailscale DNS name is invalid")]
    InvalidDnsName,
    #[error("write the private Tailscale SSH transport file {path}")]
    WriteTransport {
        path: PathBuf,
        #[source]
        source: crate::framework::AtomicFileError,
    },
    #[error("set private Tailscale SSH transport permissions on {path}")]
    SetPermissions {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error)]
pub enum TailscaleSshProcessError {
    #[error(transparent)]
    State(#[from] TailscaleSshStateError),
    #[error(transparent)]
    Policy(#[from] TailscaleSshError),
    #[error(transparent)]
    Environment(#[from] crate::framework::process::ProcessEnvironmentError),
    #[error(transparent)]
    Command(#[from] crate::framework::process::CommandSpecError),
    #[error(transparent)]
    Label(#[from] crate::framework::process::ProcessLabelError),
}

/// Prepares the private known-host store used by every Kit Tailscale SSH connection.
pub fn prepare_known_hosts_file() -> Result<PathBuf, TailscaleSshStateError> {
    let directory = prepare_state_directory()?;
    prepare_known_hosts_file_in(&directory)
}

pub(super) fn prepare_state_directory() -> Result<PathBuf, TailscaleSshStateError> {
    let project = ProjectDirs::from("", "", "kit").ok_or(TailscaleSshStateError::StateDirectory)?;
    let base = project.state_dir().unwrap_or_else(|| project.data_local_dir());
    let directory = base.join("tailscale-ssh");
    prepare_private_directory(&directory)?;
    Ok(directory)
}

pub(super) fn prepare_private_directory(directory: &Path) -> Result<(), TailscaleSshStateError> {
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(directory).map_err(|source| TailscaleSshStateError::PrepareDirectory {
        path: directory.to_path_buf(),
        source,
    })?;
    let mut metadata = std::fs::symlink_metadata(directory).map_err(|source| {
        TailscaleSshStateError::InspectPath { path: directory.to_path_buf(), source }
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(TailscaleSshStateError::UnsafeDirectory { path: directory.to_path_buf() });
    }
    if metadata.mode() & 0o077 != 0 {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).map_err(
            |source| TailscaleSshStateError::SetPermissions {
                path: directory.to_path_buf(),
                source,
            },
        )?;
        metadata = std::fs::symlink_metadata(directory).map_err(|source| {
            TailscaleSshStateError::InspectPath { path: directory.to_path_buf(), source }
        })?;
    }
    if !private_owned_directory(&metadata) {
        return Err(TailscaleSshStateError::UnsafeDirectory { path: directory.to_path_buf() });
    }
    Ok(())
}

pub(super) fn prepare_known_hosts_file_in(
    directory: &Path,
) -> Result<PathBuf, TailscaleSshStateError> {
    let known_hosts = directory.join("known_hosts");
    match std::fs::symlink_metadata(&known_hosts) {
        Ok(metadata) if private_owned_file(&metadata) => {}
        Ok(_) => {
            return Err(TailscaleSshStateError::UnsafeKnownHosts { path: known_hosts });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
            match options.open(&known_hosts) {
                Ok(_) => {}
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                    let metadata = std::fs::symlink_metadata(&known_hosts).map_err(|source| {
                        TailscaleSshStateError::InspectPath { path: known_hosts.clone(), source }
                    })?;
                    if !private_owned_file(&metadata) {
                        return Err(TailscaleSshStateError::UnsafeKnownHosts { path: known_hosts });
                    }
                }
                Err(source) => {
                    return Err(TailscaleSshStateError::InspectPath { path: known_hosts, source });
                }
            }
        }
        Err(source) => {
            return Err(TailscaleSshStateError::InspectPath { path: known_hosts, source });
        }
    }
    Ok(known_hosts)
}

pub(super) fn write_private(
    directory: &Path,
    path: &Path,
    lock_name: &str,
    temp_prefix: &str,
    contents: String,
    executable: bool,
) -> Result<(), TailscaleSshStateError> {
    let writer = AtomicFileWriter::new(directory, lock_name, temp_prefix);
    let lock = writer.lock().map_err(|source| TailscaleSshStateError::WriteTransport {
        path: path.to_path_buf(),
        source,
    })?;
    writer.replace(path, contents.as_bytes()).map_err(|source| {
        TailscaleSshStateError::WriteTransport { path: path.to_path_buf(), source }
    })?;
    drop(lock);
    if executable {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
            |source| TailscaleSshStateError::SetPermissions { path: path.to_path_buf(), source },
        )?;
    }
    Ok(())
}

impl TailscaleSshTarget {
    pub fn new(
        stable_node_id: impl Into<String>,
        unix_user: impl Into<String>,
        tailscale_ip: IpAddr,
    ) -> Result<Self, TailscaleSshError> {
        let stable_node_id = stable_node_id.into();
        if !valid_identifier(&stable_node_id, MAX_STABLE_NODE_ID_BYTES, false) {
            return Err(TailscaleSshError::StableNodeId);
        }

        let unix_user = unix_user.into();
        if !valid_identifier(&unix_user, MAX_UNIX_USER_BYTES, true) {
            return Err(TailscaleSshError::UnixUser);
        }
        if !is_tailscale_ip(tailscale_ip) {
            return Err(TailscaleSshError::TailscaleIp);
        }

        Ok(Self { stable_node_id, unix_user, tailscale_ip })
    }

    pub fn stable_node_id(&self) -> &str {
        &self.stable_node_id
    }

    pub fn unix_user(&self) -> &str {
        &self.unix_user
    }

    pub fn with_tailscale_ip(&self, tailscale_ip: IpAddr) -> Result<Self, TailscaleSshError> {
        Self::new(self.stable_node_id.clone(), self.unix_user.clone(), tailscale_ip)
    }

    /// Builds the sole supervised process shape for bounded Tailscale SSH commands.
    pub fn command_process_spec(
        &self,
        command: &RemoteCommand,
        working_directory: &Path,
        label: impl Into<String>,
        deadline: ProcessDeadline,
        input: InputPolicy,
        stdout: OutputPolicy,
        stderr: OutputPolicy,
    ) -> Result<ProcessSpec, TailscaleSshProcessError> {
        let known_hosts = prepare_known_hosts_file()?;
        let arguments =
            self.openssh_arguments(SshConnectionKind::Command, command, &known_hosts)?;
        let environment = ProcessEnvironment::new(
            EnvironmentBase::Inherit,
            Default::default(),
            Default::default(),
        )?;
        let command = CommandSpec::new(
            OsString::from("ssh"),
            arguments,
            working_directory.to_path_buf(),
            environment,
            ProcessLabel::new(label.into())?,
        )?;
        Ok(ProcessSpec::new(
            command,
            input,
            stdout,
            stderr,
            ContainmentRequirement::ExplicitProcessGroup,
            deadline,
            TerminationPolicy::new(TERMINATION_GRACE),
        ))
    }

    pub fn captured_command_process_spec(
        &self,
        command: &RemoteCommand,
        working_directory: &Path,
        label: impl Into<String>,
    ) -> Result<ProcessSpec, TailscaleSshProcessError> {
        let capture = OutputPolicy::Capture(CapturePolicy::new(
            CAPTURE_BYTES,
            CaptureOverflow::FailAndTerminate,
        ));
        self.command_process_spec(
            command,
            working_directory,
            label,
            ProcessDeadline::After(COMMAND_TIMEOUT),
            InputPolicy::Closed,
            capture,
            capture,
        )
    }

    /// Builds Kit's sole secure OpenSSH argument policy.
    pub fn openssh_arguments(
        &self,
        kind: SshConnectionKind,
        command: &RemoteCommand,
        known_hosts_file: &Path,
    ) -> Result<Vec<OsString>, TailscaleSshError> {
        if !known_hosts_file.is_absolute()
            || known_hosts_file.as_os_str().is_empty()
            || known_hosts_file.as_os_str().to_string_lossy().chars().any(char::is_control)
        {
            return Err(TailscaleSshError::KnownHostsPath);
        }
        let (server_alive_interval, server_alive_count) = match kind {
            SshConnectionKind::Command => (5, 1),
            SshConnectionKind::Relay => (15, 3),
        };
        let server_alive_interval = format!("ServerAliveInterval={server_alive_interval}");
        let server_alive_count = format!("ServerAliveCountMax={server_alive_count}");
        let host_key_alias = format!("HostKeyAlias=kit-node-{}", self.stable_node_id);
        let user_known_hosts_file =
            format!("UserKnownHostsFile={}", known_hosts_file.to_string_lossy());
        let destination = format!("{}@{}", self.unix_user, self.tailscale_ip);
        let mut arguments = vec![
            OsString::from("-F"),
            OsString::from("none"),
            OsString::from("-T"),
            OsString::from("-o"),
            OsString::from("RequestTTY=no"),
            OsString::from("-o"),
            OsString::from("ForwardAgent=no"),
            OsString::from("-o"),
            OsString::from("IdentityAgent=none"),
            OsString::from("-o"),
            OsString::from("IdentityFile=none"),
            OsString::from("-o"),
            OsString::from("IdentitiesOnly=yes"),
            OsString::from("-o"),
            OsString::from("PubkeyAuthentication=no"),
            OsString::from("-o"),
            OsString::from("PasswordAuthentication=no"),
            OsString::from("-o"),
            OsString::from("KbdInteractiveAuthentication=no"),
            OsString::from("-o"),
            OsString::from("GSSAPIAuthentication=no"),
            OsString::from("-o"),
            OsString::from("HostbasedAuthentication=no"),
            OsString::from("-o"),
            OsString::from("BatchMode=yes"),
            OsString::from("-o"),
            OsString::from("ClearAllForwardings=yes"),
            OsString::from("-o"),
            OsString::from("PermitLocalCommand=no"),
            OsString::from("-o"),
            OsString::from("ControlMaster=no"),
            OsString::from("-o"),
            OsString::from("ControlPath=none"),
            OsString::from("-o"),
            OsString::from("ProxyCommand=none"),
            OsString::from("-o"),
            OsString::from("ProxyJump=none"),
            OsString::from("-o"),
            OsString::from("ConnectTimeout=10"),
            OsString::from("-o"),
            OsString::from(server_alive_interval),
            OsString::from("-o"),
            OsString::from(server_alive_count),
            OsString::from("-o"),
            OsString::from("GlobalKnownHostsFile=none"),
            OsString::from("-o"),
            OsString::from(user_known_hosts_file),
            OsString::from("-o"),
            OsString::from("UpdateHostKeys=no"),
            OsString::from("-o"),
            OsString::from("VerifyHostKeyDNS=no"),
            OsString::from("-o"),
            OsString::from("StrictHostKeyChecking=accept-new"),
            OsString::from("-o"),
            OsString::from(host_key_alias),
            OsString::from("--"),
            OsString::from(destination),
        ];
        arguments.push(OsString::from(command.serialize()?));
        Ok(arguments)
    }
}

fn private_owned_directory(metadata: &Metadata) -> bool {
    !metadata.file_type().is_symlink()
        && metadata.is_dir()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0
}

fn private_owned_file(metadata: &Metadata) -> bool {
    !metadata.file_type().is_symlink()
        && metadata.is_file()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0
}

impl RemoteCommand {
    pub fn from_arguments<'a>(
        arguments: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, TailscaleSshError> {
        let arguments = arguments.into_iter().map(str::to_owned).collect::<Vec<_>>();
        if arguments.is_empty() {
            return Err(TailscaleSshError::EmptyRemoteCommand);
        }
        if arguments.len() > MAX_REMOTE_ARGUMENTS {
            return Err(TailscaleSshError::TooManyRemoteArguments);
        }
        if arguments.iter().any(|argument| {
            argument.len() > MAX_REMOTE_ARGUMENT_BYTES
                || argument.chars().any(|character| character.is_control())
        }) {
            return Err(TailscaleSshError::RemoteArgument);
        }
        Ok(Self { arguments })
    }

    fn serialize(&self) -> Result<String, TailscaleSshError> {
        let mut command = String::new();
        for (index, argument) in self.arguments.iter().enumerate() {
            if index > 0 {
                command.push(' ');
            }
            command.push('\'');
            command.push_str(&argument.replace('\'', "'\\''"));
            command.push('\'');
            if command.len() > MAX_REMOTE_COMMAND_BYTES {
                return Err(TailscaleSshError::RemoteCommand);
            }
        }
        Ok(command)
    }
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
    use std::path::Path;

    use super::{RemoteCommand, SshConnectionKind, TailscaleSshTarget};

    #[test]
    fn openssh_policy_allows_only_tailscale_identity_authentication() {
        let target =
            TailscaleSshTarget::new("node-1", "remote-user", "100.64.0.2".parse().unwrap())
                .unwrap();
        let command = RemoteCommand::from_arguments(["kit", "console", "status"]).unwrap();
        let arguments = target
            .openssh_arguments(
                SshConnectionKind::Command,
                &command,
                Path::new("/private/kit/known_hosts"),
            )
            .unwrap();
        let arguments =
            arguments.iter().map(|argument| argument.to_string_lossy()).collect::<Vec<_>>();

        for forbidden_authentication in [
            "IdentityAgent=none",
            "IdentityFile=none",
            "IdentitiesOnly=yes",
            "PubkeyAuthentication=no",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "GSSAPIAuthentication=no",
            "HostbasedAuthentication=no",
            "BatchMode=yes",
        ] {
            assert!(arguments.iter().any(|argument| argument == forbidden_authentication));
        }
        assert_eq!(&arguments[..2], ["-F", "none"]);
        assert!(arguments
            .iter()
            .any(|argument| argument == "UserKnownHostsFile=/private/kit/known_hosts"));
        assert!(arguments.iter().any(|argument| argument == "GlobalKnownHostsFile=none"));
        assert_eq!(arguments.last().unwrap(), "'kit' 'console' 'status'");
    }
}
