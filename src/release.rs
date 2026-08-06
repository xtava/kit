//! Kit release discovery, cache, and verified executable replacement.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};

use crate::framework::{
    process::{
        CaptureOverflow, CapturePolicy, CommandSpec, ContainmentRequirement, EnvironmentBase,
        InputPolicy, LeaderExit, LeaderExitObservation, OutputPolicy, OutputReport,
        ProcessDeadline, ProcessEnvironment, ProcessLabel, ProcessSpec, ProcessSupervisor,
        TerminationPolicy,
    },
    AtomicFileWriter,
};

const REPOSITORY_OWNER: &str = "xtava";
const REPOSITORY_NAME: &str = "kit";
const CHECK_INTERVAL: Duration = Duration::from_secs(20 * 60 * 60);
const CACHE_FILE: &str = "version.json";
const COMMAND_OUTPUT_BYTES: NonZeroUsize = NonZeroUsize::new(1024 * 1024).unwrap();
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const COMMAND_TERMINATION_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum UpdateAvailability {
    Current { version: String },
    Available { current: String, latest: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum UpdateOutcome {
    Updated { from: String, to: String },
    AlreadyCurrent { version: String },
}

pub struct ManagedUpdate {
    pub outcome: UpdateOutcome,
    pub console_status: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedRelease {
    pub availability: UpdateAvailability,
    pub stale: bool,
    pub dismissed: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ReleaseUpdater;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ReleaseCache {
    latest_version: String,
    checked_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dismissed_version: Option<String>,
}

impl ReleaseUpdater {
    pub const fn new() -> Self {
        Self
    }

    pub fn cached(self) -> Option<CachedRelease> {
        let cache = read_cache(&cache_path()?)?;
        Some(CachedRelease {
            availability: availability(&cache.latest_version),
            stale: cache.is_stale(),
            dismissed: cache.dismissed_version.as_deref() == Some(&cache.latest_version),
        })
    }

    pub async fn check(self) -> Result<UpdateAvailability> {
        let latest = configured_updater(false)?
            .is_update_available_async()
            .await
            .context("check the latest Kit release")?
            .map(|release| release.version().to_owned())
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());
        write_cache(|current| ReleaseCache {
            latest_version: latest.clone(),
            checked_at: now(),
            dismissed_version: current.and_then(|cache| cache.dismissed_version),
        })?;
        Ok(availability(&latest))
    }

    pub async fn install(self, show_progress: bool) -> Result<UpdateOutcome> {
        let current = env!("CARGO_PKG_VERSION").to_owned();
        let status = configured_updater(show_progress)?
            .update_async()
            .await
            .context("update Kit from GitHub Releases")?;
        if status.is_updated() {
            Ok(UpdateOutcome::Updated { from: current, to: status.version().to_owned() })
        } else {
            Ok(UpdateOutcome::AlreadyCurrent { version: status.version().to_owned() })
        }
    }

    pub async fn install_managed(
        self,
        processes: &ProcessSupervisor,
        show_progress: bool,
    ) -> Result<ManagedUpdate> {
        let executable =
            std::env::current_exe().context("resolve the installed Kit executable path")?;
        let managed = managed_executable()?;
        if executable != managed {
            bail!(
                "Kit is running from {}, but the managed executable is {}; reinstall with \
                 install.sh, ensure ~/.local/bin precedes legacy paths, and retry",
                executable.display(),
                managed.display()
            );
        }
        let outcome = self.install(show_progress).await?;
        let console_status = run_checked(
            processes,
            "reconcile Console after updating Kit",
            executable.as_os_str(),
            os_args(["--json", "console", "setup"]),
            &std::env::current_dir().context("resolve the Kit update working directory")?,
            BTreeMap::new(),
        )
        .await?
        .into_bytes();
        Ok(ManagedUpdate { outcome, console_status })
    }

    pub async fn download_verified_binary(
        self,
        version: &str,
        target: &str,
        destination: &Path,
    ) -> Result<String> {
        let status = configured_updater_for_target(version, target, destination)?
            .update_async()
            .await
            .with_context(|| format!("download verified Kit release for {target}"))?;
        if status.is_updated() {
            Ok(status.version().to_owned())
        } else {
            anyhow::bail!("the Kit release channel returned no binary for {target}")
        }
    }

    pub fn dismiss(self, version: &str) -> Result<()> {
        write_cache(|current| ReleaseCache {
            latest_version: version.to_owned(),
            checked_at: current.as_ref().map_or_else(now, |cache| cache.checked_at),
            dismissed_version: Some(version.to_owned()),
        })
    }
}

fn managed_executable() -> Result<PathBuf> {
    let base = BaseDirs::new().context("resolve the local home directory")?;
    Ok(base.home_dir().join(".local/bin/kit"))
}

fn os_args<const N: usize>(values: [&str; N]) -> Vec<OsString> {
    values.into_iter().map(OsString::from).collect()
}

async fn run_checked(
    processes: &ProcessSupervisor,
    label: &str,
    program: impl AsRef<std::ffi::OsStr>,
    arguments: Vec<OsString>,
    working_directory: &Path,
    environment_overrides: BTreeMap<OsString, OsString>,
) -> Result<String> {
    let environment =
        ProcessEnvironment::new(EnvironmentBase::Inherit, environment_overrides, BTreeSet::new())?;
    let command = CommandSpec::new(
        program.as_ref().to_owned(),
        arguments,
        working_directory.to_path_buf(),
        environment,
        ProcessLabel::new(label.to_owned())?,
    )?;
    let capture = OutputPolicy::Capture(CapturePolicy::new(
        COMMAND_OUTPUT_BYTES,
        CaptureOverflow::FailAndTerminate,
    ));
    let spec = ProcessSpec::new(
        command,
        InputPolicy::Closed,
        capture,
        capture,
        ContainmentRequirement::ExplicitProcessGroup,
        ProcessDeadline::After(COMMAND_TIMEOUT),
        TerminationPolicy::new(COMMAND_TERMINATION_GRACE),
    );
    let report = processes
        .spawn(spec)
        .await
        .with_context(|| format!("start {label}"))?
        .session
        .wait()
        .await
        .map_err(|failure| anyhow!("{label} supervision failed: {:?}", failure.failure))?;
    let stdout = captured_output(report.stdout, label, "stdout")?;
    let stderr = captured_output(report.stderr, label, "stderr")?;
    if report.leader_exit != LeaderExitObservation::Observed(LeaderExit::Code(0)) {
        bail!(
            "{label} failed{}",
            if stderr.is_empty() { String::new() } else { format!(": {stderr}") }
        );
    }
    Ok(stdout)
}

fn captured_output(output: OutputReport, label: &str, stream: &str) -> Result<String> {
    match output {
        OutputReport::Captured(capture) => {
            Ok(String::from_utf8_lossy(capture.bytes.as_ref()).trim().to_owned())
        }
        _ => bail!("{label} {stream} was not captured"),
    }
}

impl ReleaseCache {
    fn is_stale(&self) -> bool {
        now().saturating_sub(self.checked_at) >= CHECK_INTERVAL.as_secs()
    }
}

fn availability(latest: &str) -> UpdateAvailability {
    let current = env!("CARGO_PKG_VERSION");
    if self_update::version::bump_is_greater(current, latest).unwrap_or(false) {
        UpdateAvailability::Available { current: current.to_owned(), latest: latest.to_owned() }
    } else {
        UpdateAvailability::Current { version: current.to_owned() }
    }
}

fn configured_updater(show_progress: bool) -> Result<self_update::backends::github::AsyncUpdate> {
    self_update::backends::github::Update::configure()
        .repo_owner(REPOSITORY_OWNER)
        .repo_name(REPOSITORY_NAME)
        .bin_name("kit")
        .current_version(env!("CARGO_PKG_VERSION"))
        .show_download_progress(show_progress)
        .show_output(false)
        .no_confirm(true)
        .verify_release_digest(true)
        .build_async()
        .context("configure the Kit release updater")
}

fn configured_updater_for_target(
    version: &str,
    target: &str,
    destination: &Path,
) -> Result<self_update::backends::github::AsyncUpdate> {
    let tag = format!("v{version}");
    self_update::backends::github::Update::configure()
        .repo_owner(REPOSITORY_OWNER)
        .repo_name(REPOSITORY_NAME)
        .bin_name("kit")
        .bin_install_path(destination)
        .current_version("0.0.0")
        .release_tag(tag)
        .target(target)
        .show_download_progress(false)
        .show_output(false)
        .no_confirm(true)
        .verify_release_digest(true)
        .build_async()
        .context("configure the Kit release downloader")
}

fn cache_path() -> Option<PathBuf> {
    let project = ProjectDirs::from("", "", "kit")?;
    let base = project.state_dir().unwrap_or_else(|| project.data_local_dir());
    Some(base.join("updates").join(CACHE_FILE))
}

fn read_cache(path: &Path) -> Option<ReleaseCache> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_cache(update: impl FnOnce(Option<ReleaseCache>) -> ReleaseCache) -> Result<()> {
    let path = cache_path().context("resolve the Kit update cache path")?;
    let directory = path.parent().context("resolve the Kit update cache directory")?;
    let writer = AtomicFileWriter::new(directory, ".version.lock", ".version");
    let _lock = writer.lock().context("lock the Kit update cache")?;
    let cache = update(read_cache(&path));
    let bytes = serde_json::to_vec_pretty(&cache).context("serialize the Kit update cache")?;
    writer.replace(&path, &bytes).context("write the Kit update cache")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_only_reports_strictly_newer_versions() {
        assert!(matches!(
            availability("99.0.0"),
            UpdateAvailability::Available { latest, .. } if latest == "99.0.0"
        ));
        assert_eq!(
            availability(env!("CARGO_PKG_VERSION")),
            UpdateAvailability::Current { version: env!("CARGO_PKG_VERSION").to_owned() }
        );
    }
}
