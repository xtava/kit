use std::{
    collections::BTreeMap,
    fmt,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    framework::{AtomicFileError, AtomicFileWriter},
    onepassword::SecretReference,
};

const JOURNAL_SCHEMA_VERSION: u32 = 1;
const JOURNAL_FILE: &str = "ops-journal.json";
const LOCK_FILE: &str = "ops-journal.lock";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalOutcome {
    Success,
    Failed,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEntry {
    pub operation_id: String,
    pub references: BTreeMap<String, SecretReference>,
    pub machine: String,
    pub timestamp_secs: u64,
    pub outcome: JournalOutcome,
    pub duration_ms: u64,
}

impl fmt::Debug for JournalEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JournalEntry")
            .field("operation_id", &self.operation_id)
            .field("reference_count", &self.references.len())
            .field("machine", &self.machine)
            .field("timestamp_secs", &self.timestamp_secs)
            .field("outcome", &self.outcome)
            .field("duration_ms", &self.duration_ms)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpsJournal {
    pub schema_version: u32,
    pub entries: Vec<JournalEntry>,
}

impl Default for OpsJournal {
    fn default() -> Self {
        Self { schema_version: JOURNAL_SCHEMA_VERSION, entries: Vec::new() }
    }
}

impl fmt::Debug for OpsJournal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpsJournal")
            .field("schema_version", &self.schema_version)
            .field("entry_count", &self.entries.len())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct JournalStore {
    dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("resolve Kit state directory")]
    StateDirectory,
    #[error("resolve machine name: {source}")]
    Machine {
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Storage(#[from] AtomicFileError),
    #[error("read ops journal {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse ops journal {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("ops journal {} uses schema version {actual}; expected {expected}", path.display())]
    Schema { path: PathBuf, actual: u32, expected: u32 },
    #[error("serialize ops journal: {0}")]
    Serialize(#[from] serde_json::Error),
}

impl JournalStore {
    pub fn bootstrap() -> Result<Self, JournalError> {
        let project = ProjectDirs::from("", "", "kit").ok_or(JournalError::StateDirectory)?;
        let dir = project.state_dir().unwrap_or_else(|| project.data_local_dir()).to_path_buf();
        Ok(Self { dir })
    }

    #[cfg(test)]
    pub fn rooted(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(JOURNAL_FILE)
    }

    pub fn load(&self) -> Result<OpsJournal, JournalError> {
        load_path(&self.path())
    }

    pub fn record(&self, entry: JournalEntry) -> Result<(), JournalError> {
        let writer = self.writer();
        let _lock = writer.lock()?;
        let mut journal = self.load()?;
        journal.entries.push(entry);
        let mut bytes = serde_json::to_vec_pretty(&journal)?;
        bytes.push(b'\n');
        writer.replace(&self.path(), &bytes)?;
        Ok(())
    }

    fn writer(&self) -> AtomicFileWriter {
        AtomicFileWriter::new(&self.dir, LOCK_FILE, ".ops-state")
    }
}

pub fn machine_name() -> Result<String, JournalError> {
    resolve_machine_name().map_err(|source| JournalError::Machine { source })
}

#[cfg(unix)]
fn resolve_machine_name() -> Result<String, std::io::Error> {
    let mut bytes = [0_u8; 256];
    let result = unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let length = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    let name = std::str::from_utf8(&bytes[..length])
        .map_err(|source| std::io::Error::new(std::io::ErrorKind::InvalidData, source))?
        .trim();
    if name.is_empty() {
        return Err(std::io::Error::other("machine name was empty"));
    }
    Ok(name.to_owned())
}

#[cfg(not(unix))]
fn resolve_machine_name() -> Result<String, std::io::Error> {
    ["COMPUTERNAME", "HOSTNAME"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.trim().is_empty()))
        .ok_or_else(|| std::io::Error::other("machine name environment variable was unavailable"))
}

fn load_path(path: &Path) -> Result<OpsJournal, JournalError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(OpsJournal::default());
        }
        Err(source) => return Err(JournalError::Read { path: path.to_path_buf(), source }),
    };
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|source| JournalError::Read { path: path.to_path_buf(), source })?;
    let journal = serde_json::from_str::<OpsJournal>(&raw)
        .map_err(|source| JournalError::Parse { path: path.to_path_buf(), source })?;
    if journal.schema_version != JOURNAL_SCHEMA_VERSION {
        return Err(JournalError::Schema {
            path: path.to_path_buf(),
            actual: journal.schema_version,
            expected: JOURNAL_SCHEMA_VERSION,
        });
    }
    Ok(journal)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn store() -> JournalStore {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        JournalStore::rooted(
            std::env::temp_dir().join(format!("kit-ops-journal-{}-{id}", std::process::id())),
        )
    }

    fn entry() -> JournalEntry {
        JournalEntry {
            operation_id: "deploy-marketing".to_owned(),
            references: BTreeMap::from([(
                "TOKEN".to_owned(),
                SecretReference::new("op://Deploy/cloudflare/api_token".to_owned()).unwrap(),
            )]),
            machine: "workstation".to_owned(),
            timestamp_secs: 42,
            outcome: JournalOutcome::Success,
            duration_ms: 1500,
        }
    }

    #[test]
    fn journal_round_trip_contains_references_but_no_resolved_value() -> Result<(), JournalError> {
        let store = store();
        let entry = entry();

        store.record(entry.clone())?;
        let raw = std::fs::read_to_string(store.path())
            .map_err(|source| JournalError::Read { path: store.path(), source })?;
        let loaded = store.load()?;

        assert_eq!(loaded.entries, [entry]);
        assert!(raw.contains("op://Deploy/cloudflare/api_token"));
        assert!(!raw.contains("resolved-secret-sentinel"));
        let _ = std::fs::remove_dir_all(&store.dir);
        Ok(())
    }

    #[test]
    fn journal_rejects_non_reference_values() -> Result<(), Box<dyn std::error::Error>> {
        let store = store();
        std::fs::create_dir_all(&store.dir)?;
        std::fs::write(
            store.path(),
            r#"{"schema_version":1,"entries":[{"operation_id":"bad","references":{"TOKEN":"resolved-secret-sentinel"},"machine":"workstation","timestamp_secs":1,"outcome":"failed","duration_ms":1}]}"#,
        )?;

        let error = store.load().expect_err("literal journal value must fail");

        assert!(matches!(error, JournalError::Parse { .. }));
        assert!(!error.to_string().contains("resolved-secret-sentinel"));
        let _ = std::fs::remove_dir_all(&store.dir);
        Ok(())
    }

    #[test]
    fn debug_never_prints_reference_strings() {
        let entry = entry();
        assert!(!format!("{entry:?}").contains("api_token"));
        assert!(!format!(
            "{:?}",
            OpsJournal { schema_version: JOURNAL_SCHEMA_VERSION, entries: vec![entry] }
        )
        .contains("api_token"));
    }
}
