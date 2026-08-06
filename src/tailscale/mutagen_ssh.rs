use std::path::{Path, PathBuf};

use super::ssh::{
    prepare_known_hosts_file_in, prepare_private_directory, prepare_state_directory, write_private,
    TailscaleSshStateError, TailscaleSshTarget,
};

/// Kit's private OpenSSH boundary for Mutagen.
///
/// Mutagen invokes `ssh` and `scp` itself, so it cannot use Kit's process supervisor. This
/// permanent adapter pins both programs to the same authentication and host-identity policy as
/// Kit's supervised SSH commands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MutagenSshTransport {
    executable_directory: PathBuf,
    host: String,
}

/// Prepares the sole OpenSSH transport that Mutagen is allowed to discover.
pub fn prepare_mutagen_ssh_transport(
    target: &TailscaleSshTarget,
    dns_name: &str,
) -> Result<MutagenSshTransport, TailscaleSshStateError> {
    let directory = prepare_state_directory()?;
    prepare_mutagen_ssh_transport_in(&directory, target, dns_name)
}

fn prepare_mutagen_ssh_transport_in(
    directory: &Path,
    target: &TailscaleSshTarget,
    dns_name: &str,
) -> Result<MutagenSshTransport, TailscaleSshStateError> {
    if !valid_dns_name(dns_name) {
        return Err(TailscaleSshStateError::InvalidDnsName);
    }
    let transport = prepare_mutagen_ssh_directory_in(directory)?;
    let profiles = directory.join("profiles");
    prepare_private_directory(&profiles)?;

    let host = format!("kit-node-{}-{}", target.stable_node_id(), target.unix_user());
    let profile = profiles.join(&host);
    let contents = format!(
        "Host {host} {dns_name}\n\
         \x20 HostName {dns_name}\n\
         \x20 User {}\n\
         \x20 HostKeyAlias kit-node-{}\n",
        target.unix_user(),
        target.stable_node_id(),
    );
    write_private(&profiles, &profile, "profiles.lock", ".profile", contents, false)?;

    Ok(MutagenSshTransport { executable_directory: transport, host })
}

/// Prepares Mutagen's private executable search directory and deny-by-default SSH policy.
pub fn prepare_mutagen_ssh_directory() -> Result<PathBuf, TailscaleSshStateError> {
    let directory = prepare_state_directory()?;
    prepare_mutagen_ssh_directory_in(&directory)
}

fn prepare_mutagen_ssh_directory_in(directory: &Path) -> Result<PathBuf, TailscaleSshStateError> {
    prepare_private_directory(directory)?;
    let known_hosts = prepare_known_hosts_file_in(directory)?;
    let executable_directory = directory.join("mutagen");
    let profiles = directory.join("profiles");
    prepare_private_directory(&executable_directory)?;
    prepare_private_directory(&profiles)?;

    let config = executable_directory.join("config");
    let include = profiles.join("*");
    let contents = format!(
        "Include {}\n\
         Host *\n\
         \x20 RequestTTY no\n\
         \x20 ForwardAgent no\n\
         \x20 IdentityAgent none\n\
         \x20 IdentityFile none\n\
         \x20 IdentitiesOnly yes\n\
         \x20 PubkeyAuthentication no\n\
         \x20 PasswordAuthentication no\n\
         \x20 KbdInteractiveAuthentication no\n\
         \x20 GSSAPIAuthentication no\n\
         \x20 HostbasedAuthentication no\n\
         \x20 BatchMode yes\n\
         \x20 ClearAllForwardings yes\n\
         \x20 PermitLocalCommand no\n\
         \x20 ControlMaster no\n\
         \x20 ControlPath none\n\
         \x20 ProxyCommand none\n\
         \x20 ProxyJump none\n\
         \x20 ConnectTimeout 10\n\
         \x20 ServerAliveInterval 5\n\
         \x20 ServerAliveCountMax 1\n\
         \x20 GlobalKnownHostsFile none\n\
         \x20 UserKnownHostsFile {}\n\
         \x20 UpdateHostKeys no\n\
         \x20 VerifyHostKeyDNS no\n\
         \x20 StrictHostKeyChecking accept-new\n",
        openssh_quote(&include),
        openssh_quote(&known_hosts),
    );
    write_private(&executable_directory, &config, "config.lock", ".config", contents, false)?;
    for program in ["ssh", "scp"] {
        let path = executable_directory.join(program);
        let script = format!(
            "#!/bin/sh\nexec /usr/bin/{program} -F {} \"$@\"\n",
            shell_quote(&config.to_string_lossy())
        );
        write_private(
            &executable_directory,
            &path,
            &format!("{program}.lock"),
            &format!(".{program}"),
            script,
            true,
        )?;
    }
    Ok(executable_directory)
}

fn openssh_quote(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn valid_dns_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.is_ascii()
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

impl MutagenSshTransport {
    pub fn executable_directory(&self) -> &Path {
        &self.executable_directory
    }

    pub fn host(&self) -> &str {
        &self.host
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn transport_cannot_inherit_personal_ssh_authentication() {
        let state = std::env::temp_dir().join(format!(
            "kit-mutagen-ssh-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let target =
            TailscaleSshTarget::new("node-1", "remote-user", "100.64.0.2".parse().unwrap())
                .unwrap();

        let transport =
            prepare_mutagen_ssh_transport_in(&state, &target, "remote.test.ts.net").unwrap();
        let config = std::fs::read_to_string(transport.executable_directory().join("config"))
            .expect("read generated OpenSSH config");
        let profile =
            std::fs::read_to_string(state.join("profiles").join(transport.host())).unwrap();

        assert!(config.contains("Host *"));
        for disabled in [
            "IdentityAgent none",
            "IdentityFile none",
            "PubkeyAuthentication no",
            "PasswordAuthentication no",
            "KbdInteractiveAuthentication no",
            "GSSAPIAuthentication no",
            "HostbasedAuthentication no",
            "BatchMode yes",
        ] {
            assert!(config.contains(disabled), "missing {disabled}");
        }
        assert!(profile.contains("Host kit-node-node-1-remote-user remote.test.ts.net"));
        assert!(profile.contains("HostKeyAlias kit-node-node-1"));
        for program in ["ssh", "scp"] {
            let path = transport.executable_directory().join(program);
            let script = std::fs::read_to_string(&path).unwrap();
            assert!(script.contains(&format!("exec /usr/bin/{program} -F ")));
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o700);
        }

        std::fs::remove_dir_all(state).unwrap();
    }
}
