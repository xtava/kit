//! The Attachment registry — the single source of truth for which Attachments are live. One JSON
//! record + one socket per Attachment under the user's runtime dir, keyed by Instance name.
//!
//! Reconciliation is the disposal backstop (`docs/adr/0003`): the caller asks the framework
//! supervisor to inspect each durable receipt, so a completed daemon never lingers as a phantom
//! entry and a lost authority is never guessed from a PID.

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
    /// Git worktree root for attachments discovered from a running process. Controlled launches
    /// use their explicit session identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_root: Option<PathBuf>,
    pub port: u16,
    /// Opaque receipt for the detached daemon authority. It must be decoded and controlled only
    /// by `ProcessSupervisor`.
    pub daemon_receipt: String,
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
    #[serde(default)]
    pub phase: LaunchPhase,
    pub url: String,
    pub browser: String,
    /// Opaque receipt for the detached browser/Electron authority. It must be decoded and
    /// controlled only by `ProcessSupervisor`.
    pub process_receipt: String,
    /// The process currently serving the browser-level CDP endpoint. This is CDP routing metadata,
    /// never a lifecycle authority.
    pub root_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_kind: Option<LaunchKind>,
    #[serde(default)]
    pub render_mode: RenderMode,
    #[serde(default)]
    pub gpu_mode: GpuMode,
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

/// Which controlled launcher created a session. Ambient attachments have no launch record and
/// therefore no kind; the daemon probes an Electron main inspector only when this is Electron or
/// unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchKind {
    Chrome,
    Electron,
}

/// Durable controlled-launch lifecycle. `Starting` is written immediately after process/endpoint
/// ownership is proven; only a fully configured, attached session is promoted to `Ready`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchPhase {
    Starting,
    Ready,
    #[default]
    Unknown,
}

/// Rendering evidence recorded from the exact launch command. This is intentionally declarative:
/// Kit reports the chosen mode without guessing at a GPU-disable policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderMode {
    HeadlessNew,
    Windowed,
    ApplicationManaged,
    #[default]
    Unknown,
}

impl RenderMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HeadlessNew => "headless=new",
            Self::Windowed => "windowed",
            Self::ApplicationManaged => "application-managed",
            Self::Unknown => "unknown",
        }
    }
}

/// GPU evidence recorded from the launch boundary. `BrowserDefault` means Kit passed no GPU flag;
/// it does not claim that Chromium necessarily selected hardware acceleration at runtime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuMode {
    BrowserDefault,
    ApplicationManaged,
    #[default]
    Unknown,
}

impl GpuMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BrowserDefault => "browser-default (no Kit GPU flag)",
            Self::ApplicationManaged => "application-managed",
            Self::Unknown => "unknown",
        }
    }
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
    let path = launch_path(&record.name);
    let pending = dir.join(format!(".{}.launch.json.{}.tmp", record.name, std::process::id()));
    let result = std::fs::write(&pending, json)
        .context("write pending launch record")
        .and_then(|()| std::fs::rename(&pending, &path).context("publish launch record"));
    if result.is_err() {
        let _ = std::fs::remove_file(pending);
    }
    result
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
