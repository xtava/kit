//! The Attachment registry — the single source of truth for which Attachments are live. One JSON
//! record + one socket per Attachment under the user's runtime dir, keyed by Instance name.
//!
//! Reconciliation is the disposal backstop (`docs/adr/0003`): a record whose daemon pid is dead is
//! swept, so a crashed daemon never lingers as a phantom entry.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A live Attachment, as recorded on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub name: String,
    pub app: String,
    /// The Instance selector the Attachment was created with — used to re-discover on reconnect.
    pub selector: String,
    pub port: u16,
    /// The daemon process pid (the Attachment itself).
    pub pid: u32,
    /// The Instance's browser pid at attach time.
    pub root_pid: u32,
    pub started_at_ms: u64,
    pub tracks: Vec<String>,
}

/// A browser session launched by `kit cdp launch`. This is separate from an Attachment record:
/// a launched browser can be closed, profiled, and bundled, while an Attachment is only the warm CDP
/// daemon that observes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRecord {
    pub name: String,
    pub url: String,
    pub browser: String,
    pub browser_pid: u32,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devtools_ws_url: Option<String>,
    pub profile_dir: PathBuf,
    pub profile_name: Option<String>,
    pub temp_profile: bool,
    pub keep_profile: bool,
    pub artifact_dir: PathBuf,
    pub started_at_ms: u64,
    pub startup_capture: bool,
    pub headless: bool,
    pub viewport: Option<String>,
    pub timezone: Option<String>,
    pub locale: Option<String>,
    pub dark: bool,
    pub offline: bool,
    pub throttle: Option<String>,
}

pub fn dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "kit")
        .and_then(|dirs| dirs.runtime_dir().map(|dir| dir.join("cdp")))
        .unwrap_or_else(|| {
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("kit-cdp-{uid}"))
        })
}

pub fn socket_path(name: &str) -> PathBuf {
    dir().join(format!("{name}.sock"))
}

pub fn log_path(name: &str) -> PathBuf {
    dir().join(format!("{name}.log"))
}

pub fn profiles_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "kit")
        .map(|dirs| dirs.config_dir().join("cdp/profiles"))
        .unwrap_or_else(|| dir().join("profiles"))
}

pub fn temp_profiles_dir() -> PathBuf {
    dir().join("profiles")
}

pub fn artifacts_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "kit")
        .map(|dirs| dirs.data_local_dir().join("cdp/artifacts"))
        .unwrap_or_else(|| dir().join("artifacts"))
}

pub fn artifact_dir(name: &str) -> PathBuf {
    artifacts_dir().join(name)
}

fn record_path(name: &str) -> PathBuf {
    dir().join(format!("{name}.json"))
}

fn launch_path(name: &str) -> PathBuf {
    dir().join(format!("{name}.launch.json"))
}

pub fn write(record: &Record) -> Result<()> {
    let dir = dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let json = serde_json::to_string_pretty(record)?;
    std::fs::write(record_path(&record.name), json).context("write attachment record")
}

pub fn read(name: &str) -> Option<Record> {
    let raw = std::fs::read_to_string(record_path(name)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn write_launch(record: &LaunchRecord) -> Result<()> {
    let dir = dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::create_dir_all(&record.artifact_dir)
        .with_context(|| format!("create {}", record.artifact_dir.display()))?;
    let json = serde_json::to_string_pretty(record)?;
    std::fs::write(launch_path(&record.name), json).context("write launch record")
}

pub fn read_launch(name: &str) -> Option<LaunchRecord> {
    let raw = std::fs::read_to_string(launch_path(name)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Every recorded Attachment, live or not.
pub fn all() -> Vec<Record> {
    let Ok(entries) = std::fs::read_dir(dir()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|raw| serde_json::from_str(&raw).ok())
        .collect()
}

pub fn all_launches() -> Vec<LaunchRecord> {
    let Ok(entries) = std::fs::read_dir(dir()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".launch.json"))
        })
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|raw| serde_json::from_str(&raw).ok())
        .collect()
}

/// Drop records whose daemon is gone (and their stray sockets); return the survivors.
pub fn reconcile() -> Vec<Record> {
    all()
        .into_iter()
        .filter(|record| {
            if is_alive(record.pid) {
                true
            } else {
                remove(&record.name);
                false
            }
        })
        .collect()
}

pub fn remove(name: &str) {
    let _ = std::fs::remove_file(record_path(name));
    let _ = std::fs::remove_file(socket_path(name));
}

pub fn remove_launch(name: &str) {
    let _ = std::fs::remove_file(launch_path(name));
}

pub fn remove_launch_profile(record: &LaunchRecord) {
    if record.temp_profile
        && !record.keep_profile
        && is_under(&record.profile_dir, &temp_profiles_dir())
    {
        let _ = std::fs::remove_dir_all(&record.profile_dir);
    }
}

fn is_under(path: &Path, root: &Path) -> bool {
    match (path.canonicalize(), root.canonicalize()) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => path.starts_with(root),
    }
}

/// Whether a pid is a live process.
pub fn is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if unsafe { libc::kill(pid as i32, 0) == 0 } {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
