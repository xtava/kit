use std::ffi::OsString;
use std::fs::{DirBuilder, Metadata};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use directories::ProjectDirs;

use crate::framework::{process::ProcessSupervisor, AtomicFileWriter};

use super::command;
use super::model::{ConsoleServicePlatform, NativeServiceState};

pub const PLATFORM: ConsoleServicePlatform = ConsoleServicePlatform::LinuxSystemdUser;

const UNIT_NAME: &str = "io.xtava.kit.console.agent.service";
const SERVICE_LABEL: &str = "Kit Console agent";

/// Inspect the systemd user manager and the single Kit-owned unit definition.
///
/// Filesystem defects are returned as errors because callers must not turn a potentially unsafe
/// service definition into an ordinary lifecycle state. A reachable manager instead produces an
/// actionable [`NativeServiceState`].
pub async fn inspect(processes: &ProcessSupervisor) -> Result<NativeServiceState> {
    let unit_dir = unit_directory()?;
    if let Some(state) = inspect_unit_directory_chain(&unit_dir)? {
        return Ok(state);
    }
    let unit_path = unit_path()?;
    match std::fs::symlink_metadata(&unit_path) {
        Ok(metadata) => {
            if let Some(state) = ownership_state(&unit_path, &metadata) {
                return Ok(state);
            }
            if let Err(error) = validate_unit_file(&unit_path, &metadata) {
                return Ok(NativeServiceState::Failed { detail: error.to_string() });
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(NativeServiceState::NotInstalled);
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspecting Console service unit {}", unit_path.display())
            });
        }
    }

    let output = match command::run(
        processes,
        "inspect Console systemd user service",
        "systemctl",
        systemctl_args(["show", "--property=LoadState", "--property=ActiveState", "--value"]),
    )
    .await
    {
        Ok(output) => output,
        Err(error) => {
            return Ok(NativeServiceState::Unavailable {
                detail: format!("could not query the systemd user manager: {error:#}"),
            });
        }
    };
    if !output.success {
        return Ok(manager_failure(&output.stderr, &output.stdout));
    }

    let mut values = output.stdout.lines();
    let load = values.next().unwrap_or_default().trim();
    let active = values.next().unwrap_or_default().trim();
    if values.next().is_some() || load.is_empty() || active.is_empty() {
        return Ok(NativeServiceState::Failed {
            detail: format!(
                "systemd returned an invalid state for {UNIT_NAME}: {}",
                one_line(&output.stdout)
            ),
        });
    }

    Ok(match (load, active) {
        ("not-found", _) => NativeServiceState::NotInstalled,
        ("loaded", "active" | "activating" | "reloading") => NativeServiceState::Running,
        ("loaded", "inactive" | "deactivating") => NativeServiceState::Stopped,
        (_, "failed") => {
            NativeServiceState::Failed { detail: format!("systemd reports {UNIT_NAME} as failed") }
        }
        _ => NativeServiceState::Failed {
            detail: format!("systemd reports LoadState={load}, ActiveState={active}"),
        },
    })
}

/// Return whether the safely-owned installed unit invokes exactly `executable console __agent`.
pub async fn definition_matches(_processes: &ProcessSupervisor, executable: &Path) -> Result<bool> {
    let unit_path = unit_path()?;
    let metadata = match std::fs::symlink_metadata(&unit_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspecting Console service unit {}", unit_path.display())
            });
        }
    };
    validate_unit_file(&unit_path, &metadata)?;
    Ok(std::fs::read_to_string(&unit_path)
        .with_context(|| format!("reading Console service unit {}", unit_path.display()))?
        == render_unit(executable)?)
}

/// Atomically publish the native unit and make it the active systemd-user definition.
///
/// The shared service owner establishes that replacement is safe before reaching this adapter.
pub async fn install_and_start(processes: &ProcessSupervisor, executable: &Path) -> Result<()> {
    validate_executable(executable)?;
    let rendered = render_unit(executable)?;
    let unit_dir = unit_directory()?;
    ensure_private_unit_directory(&unit_dir)?;
    let unit_path = unit_dir.join(UNIT_NAME);

    let previous =
        match std::fs::symlink_metadata(&unit_path) {
            Ok(metadata) => {
                validate_unit_file(&unit_path, &metadata)?;
                Some(std::fs::read(&unit_path).with_context(|| {
                    format!("reading Console service unit {}", unit_path.display())
                })?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspecting Console service unit {}", unit_path.display())
                });
            }
        };

    let was_running = matches!(inspect(processes).await?, NativeServiceState::Running);
    if was_running {
        stop(processes).await?;
    }

    let writer = AtomicFileWriter::new(&unit_dir, ".kit-console-agent.lock", ".kit-console-agent");
    let _lock = writer.lock().context("locking the Console systemd unit for replacement")?;
    if let Err(error) = writer.replace(&unit_path, rendered.as_bytes()) {
        if was_running {
            start(processes)
                .await
                .context("restarting unchanged Console service after publication failed")?;
        }
        return Err(error).with_context(|| {
            format!("atomically publishing Console service unit {}", unit_path.display())
        });
    }
    validate_unit_file(
        &unit_path,
        &std::fs::symlink_metadata(&unit_path).with_context(|| {
            format!("inspecting published Console service unit {}", unit_path.display())
        })?,
    )?;

    let activation = async {
        daemon_reload(processes).await?;
        run_systemctl(processes, "enable Console systemd user service", ["enable"]).await?;
        start(processes).await
    }
    .await;
    if let Err(error) = activation {
        return rollback_after_failed_activation(
            processes,
            &writer,
            &unit_path,
            previous,
            was_running,
            error,
        )
        .await;
    }
    Ok(())
}

/// Start the already-published service without replacing its unit definition.
pub async fn start(processes: &ProcessSupervisor) -> Result<()> {
    run_systemctl(processes, "start Console systemd user service", ["start"]).await
}

/// Stop the service. The shared owner has already applied the session-safety policy.
pub async fn stop(processes: &ProcessSupervisor) -> Result<()> {
    run_systemctl(processes, "stop Console systemd user service", ["stop"]).await
}

fn unit_directory() -> Result<PathBuf> {
    let project = ProjectDirs::from("", "", "kit")
        .context("resolving the Kit Linux user configuration directory")?;
    let config_root = project.config_dir().parent().context(
        "deriving the Linux XDG configuration root from the Kit configuration directory",
    )?;
    Ok(config_root.join("systemd").join("user"))
}

fn unit_path() -> Result<PathBuf> {
    Ok(unit_directory()?.join(UNIT_NAME))
}

fn ensure_private_unit_directory(path: &Path) -> Result<()> {
    let config_dir =
        path.ancestors().nth(2).context("deriving the Linux XDG configuration directory")?;
    ensure_secure_directory(config_dir)?;
    let systemd_dir = config_dir.join("systemd");
    ensure_secure_directory(&systemd_dir)?;
    ensure_secure_directory(path)
}

fn inspect_unit_directory_chain(path: &Path) -> Result<Option<NativeServiceState>> {
    let config_dir =
        path.ancestors().nth(2).context("deriving the Linux XDG configuration directory")?;
    for directory in [config_dir.to_path_buf(), config_dir.join("systemd"), path.to_path_buf()] {
        match std::fs::symlink_metadata(&directory) {
            Ok(metadata) => {
                if let Some(state) = ownership_state(&directory, &metadata) {
                    return Ok(Some(state));
                }
                if let Err(error) = validate_secure_directory(&directory, &metadata) {
                    return Ok(Some(NativeServiceState::Failed { detail: error.to_string() }));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Some(NativeServiceState::NotInstalled));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspecting Console service directory {}", directory.display())
                });
            }
        }
    }
    Ok(None)
}

fn ensure_secure_directory(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_secure_directory(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.recursive(false);
            builder.create(path).with_context(|| {
                format!("creating Console service directory {}", path.display())
            })?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).with_context(
                || format!("securing Console service directory {}", path.display()),
            )?;
            let metadata = std::fs::symlink_metadata(path).with_context(|| {
                format!("inspecting Console service directory {}", path.display())
            })?;
            validate_secure_directory(path, &metadata)
        }
        Err(error) => Err(error)
            .with_context(|| format!("inspecting Console service directory {}", path.display())),
    }
}

fn validate_secure_directory(path: &Path, metadata: &Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!(
            "Console service directory {} must be an owned directory, not a symlink",
            path.display()
        );
    }
    validate_owner_and_mode(path, metadata, 0o022, "directory")
}

fn validate_unit_file(path: &Path, metadata: &Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() || metadata.nlink() != 1
    {
        bail!("Console service unit {} must be one owned regular file", path.display());
    }
    validate_owner_and_mode(path, metadata, 0o077, "unit")
}

fn validate_owner_and_mode(
    path: &Path,
    metadata: &Metadata,
    forbidden_mode: u32,
    kind: &str,
) -> Result<()> {
    let expected_uid = unsafe { libc::geteuid() };
    if metadata.uid() != expected_uid {
        bail!(
            "Console service {kind} {} is owned by uid {}, expected uid {expected_uid}",
            path.display(),
            metadata.uid()
        );
    }
    if metadata.mode() & forbidden_mode != 0 {
        bail!(
            "Console service {kind} {} has insecure permissions {:o}",
            path.display(),
            metadata.mode() & 0o7777
        );
    }
    Ok(())
}

fn ownership_state(path: &Path, metadata: &Metadata) -> Option<NativeServiceState> {
    let expected_uid = unsafe { libc::geteuid() };
    (metadata.uid() != expected_uid).then(|| NativeServiceState::WrongOwner {
        path: path.to_path_buf(),
        expected_uid,
        actual_uid: metadata.uid(),
    })
}

fn validate_executable(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("Console service executable {} must be absolute", path.display());
    }
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting Console service executable {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("Console service executable {} must be a regular file", path.display());
    }
    if metadata.mode() & 0o111 == 0 {
        bail!("Console service executable {} is not executable", path.display());
    }
    Ok(())
}

fn render_unit(executable: &Path) -> Result<String> {
    let executable =
        executable.to_str().context("Console service executable path is not valid UTF-8")?;
    if executable.chars().any(|character| matches!(character, '\n' | '\r' | '\0')) {
        bail!("Console service executable path contains an unsafe control character");
    }
    let executable = systemd_quote(executable);
    Ok(format!(
        "[Unit]\nDescription={SERVICE_LABEL}\n\n[Service]\nType=exec\nExecStart={executable} console __agent\nWorkingDirectory=%h\nRuntimeDirectory=kit/console\nRuntimeDirectoryMode=0700\nUMask=0077\nRestart=on-failure\nRestartSec=2\nKillMode=control-group\nTimeoutStopSec=10\nNoNewPrivileges=true\n\n[Install]\nWantedBy=default.target\n"
    ))
}

fn systemd_quote(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn systemctl_args<const N: usize>(arguments: [&str; N]) -> Vec<OsString> {
    let mut values = Vec::with_capacity(N + 2);
    values.push(OsString::from("--user"));
    values.extend(arguments.into_iter().map(OsString::from));
    values.push(OsString::from(UNIT_NAME));
    values
}

async fn daemon_reload(processes: &ProcessSupervisor) -> Result<()> {
    let label = "reload Console systemd user service definitions";
    let output = command::run(
        processes,
        label,
        "systemctl",
        vec![OsString::from("--user"), OsString::from("daemon-reload")],
    )
    .await?;
    if output.success {
        Ok(())
    } else {
        bail!("{label} failed: {}", command_failure_detail(&output.stderr, &output.stdout));
    }
}

async fn run_systemctl<const N: usize>(
    processes: &ProcessSupervisor,
    label: &str,
    arguments: [&str; N],
) -> Result<()> {
    let output = command::run(processes, label, "systemctl", systemctl_args(arguments)).await?;
    if output.success {
        return Ok(());
    }
    bail!("{label} failed: {}", command_failure_detail(&output.stderr, &output.stdout));
}

fn manager_failure(stderr: &str, stdout: &str) -> NativeServiceState {
    let detail = command_failure_detail(stderr, stdout);
    if detail.contains("Failed to connect to bus")
        || detail.contains("No medium found")
        || detail.contains("not been booted")
        || detail.contains("Transport endpoint is not connected")
    {
        NativeServiceState::Unavailable { detail }
    } else {
        NativeServiceState::Failed { detail }
    }
}

fn command_failure_detail(stderr: &str, stdout: &str) -> String {
    if !stderr.trim().is_empty() {
        one_line(stderr)
    } else if !stdout.trim().is_empty() {
        one_line(stdout)
    } else {
        String::from("systemctl exited without diagnostic output")
    }
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

async fn rollback_after_failed_activation(
    processes: &ProcessSupervisor,
    writer: &AtomicFileWriter,
    unit_path: &Path,
    previous: Option<Vec<u8>>,
    was_running: bool,
    activation_error: anyhow::Error,
) -> Result<()> {
    let newly_installed = previous.is_none();
    if newly_installed {
        let _ =
            run_systemctl(processes, "disable failed Console systemd user service", ["disable"])
                .await;
    }
    match previous {
        Some(previous) => writer
            .replace(unit_path, &previous)
            .with_context(|| format!("restoring Console service unit {}", unit_path.display()))?,
        None => std::fs::remove_file(unit_path).with_context(|| {
            format!("removing failed Console service unit {}", unit_path.display())
        })?,
    }
    daemon_reload(processes).await.context("reloading restored Console service definition")?;
    if was_running {
        start(processes).await.context("restarting restored Console service")?;
    }
    Err(activation_error)
        .context("activating Console systemd user service; restored previous definition")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_direct_agent_invocation_without_a_shell() {
        let rendered = render_unit(Path::new("/opt/Kit Console/kit")).unwrap();
        assert!(rendered.contains("ExecStart=\"/opt/Kit Console/kit\" console __agent"));
        assert!(rendered.contains("Restart=on-failure"));
        assert!(!rendered.contains("/bin/sh"));
        assert!(!rendered.contains("SSH_AUTH_SOCK"));
    }

    #[test]
    fn systemctl_arguments_are_exact_and_unit_scoped() {
        assert_eq!(systemctl_args(["start"]), ["--user", "start", UNIT_NAME].map(OsString::from));
        assert_eq!(
            systemctl_args(["show", "--property=LoadState", "--property=ActiveState", "--value"]),
            [
                "--user",
                "show",
                "--property=LoadState",
                "--property=ActiveState",
                "--value",
                UNIT_NAME,
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn unavailable_manager_is_not_reported_as_a_stopped_service() {
        assert!(matches!(
            manager_failure("Failed to connect to bus: No medium found", ""),
            NativeServiceState::Unavailable { .. }
        ));
    }
}
