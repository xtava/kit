use std::{
    fs::{DirBuilder, Metadata},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt},
    },
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
#[cfg(target_os = "linux")]
use directories::ProjectDirs;

const RUNTIME_OVERRIDE: &str = "KIT_CONSOLE_RUNTIME_DIR";

pub(crate) fn directory() -> Result<PathBuf> {
    let path = if let Some(runtime_dir) = std::env::var_os(RUNTIME_OVERRIDE) {
        let runtime_dir = PathBuf::from(runtime_dir);
        if !runtime_dir.is_absolute() {
            bail!("{RUNTIME_OVERRIDE} must be an absolute path");
        }
        runtime_dir
    } else {
        platform_directory()?
    };
    validate_socket_path(&path.join("agent.sock"))?;
    Ok(path)
}

#[cfg(target_os = "macos")]
fn platform_directory() -> Result<PathBuf> {
    // Darwin Unix sockets have a 104-byte sockaddr path. The ordinary per-user TMPDIR under
    // /var/folders is too long before Kit adds a filename, so Console owns one short private root.
    let effective_user = unsafe { libc::geteuid() };
    Ok(PathBuf::from("/tmp").join(format!("kit-console-{effective_user}")))
}

#[cfg(target_os = "linux")]
fn platform_directory() -> Result<PathBuf> {
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let runtime_dir = PathBuf::from(runtime_dir);
        if !runtime_dir.is_absolute() {
            bail!("XDG_RUNTIME_DIR must be an absolute path");
        }
        return Ok(runtime_dir.join("kit/console"));
    }
    let effective_user = unsafe { libc::geteuid() };
    let system_runtime = PathBuf::from("/run/user").join(effective_user.to_string());
    match std::fs::symlink_metadata(&system_runtime) {
        Ok(metadata) => {
            validate_owned_private_directory(&system_runtime, &metadata)
                .context("validating the system user runtime directory")?;
            return Ok(system_runtime.join("kit/console"));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspecting system user runtime directory {}", system_runtime.display())
            })
        }
    }
    let project = ProjectDirs::from("", "", "kit").context("resolving Kit runtime directory")?;
    let base = project
        .runtime_dir()
        .or_else(|| project.state_dir())
        .unwrap_or_else(|| project.data_local_dir());
    Ok(base.join("console"))
}

pub(crate) fn prepare(path: &Path) -> Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(path)
        .with_context(|| format!("creating Console runtime directory {}", path.display()))?;
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting Console runtime directory {}", path.display()))?;
    validate_owned_private_directory(path, &metadata)
}

pub(crate) fn validate_owned_private_directory(path: &Path, metadata: &Metadata) -> Result<()> {
    if metadata.file_type().is_symlink() {
        bail!("Console runtime directory {} must not be a symlink", path.display());
    }
    if !metadata.file_type().is_dir() {
        bail!("Console runtime path {} is not a directory", path.display());
    }
    let effective_user = unsafe { libc::geteuid() };
    if metadata.uid() != effective_user {
        bail!(
            "Console runtime directory {} is owned by uid {}, expected uid {}",
            path.display(),
            metadata.uid(),
            effective_user
        );
    }
    let mode = metadata.mode();
    if mode & 0o077 != 0 {
        bail!(
            "Console runtime directory {} has insecure permissions {:o}; group/other access is forbidden",
            path.display(),
            mode & 0o7777
        );
    }
    Ok(())
}

pub(crate) fn validate_socket_path(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    const MAX_SOCKET_PATH_BYTES: usize = 103;
    #[cfg(target_os = "linux")]
    const MAX_SOCKET_PATH_BYTES: usize = 107;

    let bytes = path.as_os_str().as_bytes().len();
    if bytes > MAX_SOCKET_PATH_BYTES {
        bail!(
            "Console socket path {} is {bytes} bytes; this platform permits at most {MAX_SOCKET_PATH_BYTES}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_socket_paths_beyond_the_platform_limit() {
        let path = PathBuf::from("/").join("x".repeat(200));
        assert!(validate_socket_path(&path).unwrap_err().to_string().contains("permits at most"));
    }
}
