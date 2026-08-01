use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{bail, Context as _, Result};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::{
    fs::File,
    io::AsyncReadExt as _,
    process::Command,
    time,
};

const GIT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceIdentity {
    pub commit: String,
    pub dirty: bool,
    pub content_sha256: String,
}

pub async fn inspect(
    working_dir: &Path,
    additional_roots: &[PathBuf],
) -> Result<Option<SourceIdentity>> {
    let Some(mut source) = inspect_root(working_dir).await? else {
        if additional_roots.is_empty() {
            return Ok(None);
        }
        bail!(
            "additional source roots require a Git working directory: {}",
            working_dir.display()
        );
    };
    if additional_roots.is_empty() {
        return Ok(Some(source));
    }

    let mut digest = Sha256::new();
    hash_field(&mut digest, b"primary-root", source.content_sha256.as_bytes());
    for root in additional_roots {
        let additional = inspect_root(root)
            .await
            .with_context(|| format!("inspect additional source root {}", root.display()))?
            .with_context(|| {
                format!("additional source root is not a Git worktree: {}", root.display())
            })?;
        source.dirty |= additional.dirty;
        hash_field(&mut digest, b"additional-root", additional.content_sha256.as_bytes());
    }
    source.content_sha256 = format!("{:x}", digest.finalize());
    Ok(Some(source))
}

async fn inspect_root(working_dir: &Path) -> Result<Option<SourceIdentity>> {
    let Some(commit) = git_commit(working_dir).await? else {
        return Ok(None);
    };
    let status = git_output(
        working_dir,
        ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await?;
    let tracked_diff =
        git_output(working_dir, ["diff", "--binary", "--no-ext-diff", "HEAD", "--"]).await?;
    let untracked =
        git_output(working_dir, ["ls-files", "--others", "--exclude-standard", "-z"]).await?;

    let mut digest = Sha256::new();
    hash_field(&mut digest, b"commit", commit.as_bytes());
    hash_field(&mut digest, b"status", &status);
    hash_field(&mut digest, b"tracked-diff", &tracked_diff);
    for raw_path in untracked.split(|byte| *byte == 0).filter(|path| !path.is_empty()) {
        hash_untracked(&mut digest, working_dir, raw_path).await?;
    }

    Ok(Some(SourceIdentity {
        commit,
        dirty: !status.is_empty(),
        content_sha256: format!("{:x}", digest.finalize()),
    }))
}

async fn git_commit(working_dir: &Path) -> Result<Option<String>> {
    let output = time::timeout(
        GIT_TIMEOUT,
        Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .current_dir(working_dir)
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .context("git commit inspection timed out")?
    .context("inspect git commit")?;
    if !output.status.success() {
        return Ok(None);
    }
    let commit = String::from_utf8(output.stdout)
        .context("git commit was not UTF-8")?
        .trim()
        .to_owned();
    if !(7..=64).contains(&commit.len()) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("git returned an invalid commit identity");
    }
    Ok(Some(commit))
}

async fn git_output<const N: usize>(working_dir: &Path, arguments: [&str; N]) -> Result<Vec<u8>> {
    let output = time::timeout(
        GIT_TIMEOUT,
        Command::new("git")
            .args(arguments)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .current_dir(working_dir)
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .context("git source inspection timed out")?
    .context("inspect git source")?;
    if !output.status.success() {
        bail!("git source inspection failed with status {}", output.status);
    }
    Ok(output.stdout)
}

async fn hash_untracked(
    digest: &mut Sha256,
    working_dir: &Path,
    raw_path: &[u8],
) -> Result<()> {
    hash_field(digest, b"untracked-path", raw_path);
    let path = working_dir.join(path_from_git(raw_path)?);
    let metadata = tokio::fs::symlink_metadata(&path)
        .await
        .with_context(|| format!("inspect untracked source {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        let target = tokio::fs::read_link(&path)
            .await
            .with_context(|| format!("read untracked symlink {}", path.display()))?;
        hash_field(digest, b"symlink", os_bytes(target.as_os_str()).as_ref());
        return Ok(());
    }
    if !metadata.is_file() {
        bail!("untracked source is not a file or symlink: {}", path.display());
    }

    digest.update(b"file\0");
    digest.update(metadata.len().to_le_bytes());
    let mut file =
        File::open(&path).await.with_context(|| format!("read untracked source {}", path.display()))?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .with_context(|| format!("read untracked source {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(())
}

fn hash_field(digest: &mut Sha256, label: &[u8], value: &[u8]) {
    digest.update(label);
    digest.update([0]);
    digest.update(value.len().to_le_bytes());
    digest.update(value);
}

#[cfg(unix)]
fn path_from_git(raw: &[u8]) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;

    Ok(PathBuf::from(OsString::from_vec(raw.to_vec())))
}

#[cfg(not(unix))]
fn path_from_git(raw: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(String::from_utf8(raw.to_vec()).context("git path was not UTF-8")?))
}

#[cfg(unix)]
fn os_bytes(value: &std::ffi::OsStr) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt as _;

    std::borrow::Cow::Borrowed(value.as_bytes())
}

#[cfg(not(unix))]
fn os_bytes(value: &std::ffi::OsStr) -> std::borrow::Cow<'_, [u8]> {
    value.to_string_lossy().as_bytes().into()
}
