//! The Attachment registry — the single source of truth for which Attachments are live. One JSON
//! record + one socket per Attachment under the user's runtime dir, keyed by Instance name.
//!
//! Reconciliation is the disposal backstop (`docs/adr/0003`): a record whose daemon pid is dead is
//! swept, so a crashed daemon never lingers as a phantom entry.

use std::path::PathBuf;

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

fn record_path(name: &str) -> PathBuf {
    dir().join(format!("{name}.json"))
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

/// Whether a pid is a live process.
pub fn is_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}
