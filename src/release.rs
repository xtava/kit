//! Kit release discovery, cache, and verified executable replacement.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::framework::AtomicFileWriter;

const REPOSITORY_OWNER: &str = "xtava";
const REPOSITORY_NAME: &str = "kit";
const CHECK_INTERVAL: Duration = Duration::from_secs(20 * 60 * 60);
const CACHE_FILE: &str = "version.json";

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

    pub fn dismiss(self, version: &str) -> Result<()> {
        write_cache(|current| ReleaseCache {
            latest_version: version.to_owned(),
            checked_at: current.as_ref().map_or_else(now, |cache| cache.checked_at),
            dismissed_version: Some(version.to_owned()),
        })
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
