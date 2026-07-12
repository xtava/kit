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
    /// Linux process start ticks for `pid`. Paired with the pid so reconciliation never treats a
    /// reused pid as the original Attachment process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start_ticks: Option<u64>,
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
    pub browser_pid: u32,
    /// Controlled-launch ownership proof. Legacy records without this proof are retained but never
    /// signalled automatically: a pid alone is not a safe process identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership: Option<LaunchOwnership>,
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

/// Stable identity for one Linux process. Pids are reusable; `start_ticks` makes a match specific to
/// one process lifetime, while process-group/session ids establish the owned termination boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_ticks: u64,
    pub process_group_id: u32,
    pub session_id: u32,
}

/// Ownership recorded after the controlled process and its CDP endpoint are both live. The endpoint
/// may be a descendant of a launcher wrapper (notably `pnpm` -> Electron), but it must live in the
/// same newly-created session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchOwnership {
    pub leader: ProcessIdentity,
    pub endpoint: ProcessIdentity,
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

/// Drop records whose daemon is gone (and their stray sockets); return the survivors.
pub fn reconcile() -> Vec<Record> {
    all()
        .into_iter()
        .filter(|record| {
            if record_process_is_current(record) {
                true
            } else {
                remove(&record.name);
                false
            }
        })
        .collect()
}

/// Whether an Attachment record still names the exact process that wrote it. Old records without
/// start ticks keep the previous liveness check so `gc` can still recover them, but newly written
/// records cannot be kept alive by pid reuse.
pub fn record_process_is_current(record: &Record) -> bool {
    match record.process_start_ticks {
        Some(start_ticks) => {
            process_identity(record.pid).is_some_and(|identity| identity.start_ticks == start_ticks)
        }
        None => is_alive(record.pid),
    }
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

/// Read one process's identity from `/proc/<pid>/stat`. The command name is parenthesized and may
/// contain spaces or parentheses, so fields are parsed only after its final `)`.
pub fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_process_stat(pid, &stat)
}

/// Enumerate every live process currently in a Linux session. Callers must first verify a recorded
/// process identity in that session before treating this as an owned termination boundary.
pub fn processes_in_session(session_id: u32) -> Vec<ProcessIdentity> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
        .filter_map(process_identity)
        .filter(|identity| identity.session_id == session_id)
        .collect()
}

fn parse_process_stat(pid: u32, stat: &str) -> Option<ProcessIdentity> {
    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    Some(ProcessIdentity {
        pid,
        process_group_id: fields.get(2)?.parse().ok()?,
        session_id: fields.get(3)?.parse().ok()?,
        start_ticks: fields.get(19)?.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_stat_parser_handles_spaces_and_parentheses_in_command_name() {
        let mut fields = vec!["0"; 20];
        fields[0] = "S";
        fields[1] = "1";
        fields[2] = "42";
        fields[3] = "42";
        fields[19] = "987654";
        let stat = format!("42 (chrome (renderer)) {}", fields.join(" "));
        let identity = parse_process_stat(42, &stat).unwrap();
        assert_eq!(
            identity,
            ProcessIdentity { pid: 42, process_group_id: 42, session_id: 42, start_ticks: 987654 }
        );
    }
}
