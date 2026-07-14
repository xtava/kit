use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use thiserror::Error;
use tokio::{process::Command, time};

use crate::framework::{AtomicFileError, AtomicFileWriter};

const JOURNAL_SCHEMA_VERSION: u32 = 1;
const JOURNAL_FILE: &str = "deploy-journal.json";
const LOCK_FILE: &str = "deploy-journal.lock";
const COUNTER_FILE: &str = "deploy-version-counter";

/// A deploy or rollback Version recorded in the Journal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct VersionId(pub String);

impl<'de> Deserialize<'de> for VersionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(D::Error::custom(
                "Version must use only letters, numbers, '.', '_' or '-'",
            ));
        }
        Ok(Self(value))
    }
}

/// Whether a Journal entry was a normal deploy or a rollback.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum JournalOperation {
    Deploy,
    Rollback { selected_version: VersionId },
}

/// The terminal status persisted for one Target Run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalStatus {
    Success,
    Failed,
    Cancelled,
    RolledBack,
}

/// The persisted outcome of one Step that started.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalStepStatus {
    Success,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalStep {
    pub name: String,
    pub status: JournalStepStatus,
    pub duration_ms: u64,
}

/// One persisted Target Run, intentionally excluding commands, output, paths, and machine identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEntry {
    pub version: VersionId,
    pub timestamp_secs: u64,
    pub operation: JournalOperation,
    pub status: JournalStatus,
    pub duration_ms: u64,
    pub steps: Vec<JournalStep>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetJournal {
    pub target_id: String,
    pub entries: Vec<JournalEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployJournal {
    pub schema_version: u32,
    pub targets: Vec<TargetJournal>,
}

impl Default for DeployJournal {
    fn default() -> Self {
        Self { schema_version: JOURNAL_SCHEMA_VERSION, targets: Vec::new() }
    }
}

impl DeployJournal {
    pub fn entries(&self, target_id: &str) -> &[JournalEntry] {
        self.targets
            .iter()
            .find(|journal| journal.target_id == target_id)
            .map(|journal| journal.entries.as_slice())
            .unwrap_or_default()
    }

    fn append(&mut self, target_id: &str, entry: JournalEntry) {
        match self.targets.iter_mut().find(|journal| journal.target_id == target_id) {
            Some(journal) => journal.entries.push(entry),
            None => self
                .targets
                .push(TargetJournal { target_id: target_id.to_owned(), entries: vec![entry] }),
        }
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
    #[error(transparent)]
    Storage(#[from] AtomicFileError),
    #[error("read deploy journal {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse deploy journal {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("deploy journal {} uses schema version {actual}; expected {expected}", path.display())]
    Schema { path: PathBuf, actual: u32, expected: u32 },
    #[error("serialize deploy journal: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("parse deploy version counter {}: {source}", path.display())]
    Counter {
        path: PathBuf,
        #[source]
        source: std::num::ParseIntError,
    },
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

    pub fn load(&self) -> Result<DeployJournal, JournalError> {
        load_path(&self.path())
    }

    pub fn record_many(
        &self,
        entries: impl IntoIterator<Item = (String, JournalEntry)>,
    ) -> Result<(), JournalError> {
        let writer = self.writer();
        let _lock = writer.lock()?;
        let mut journal = self.load()?;
        for (target_id, entry) in entries {
            journal.append(&target_id, entry);
        }
        self.write_json(&writer, &self.path(), &journal)
    }

    pub fn reserve_monotonic_version(&self, target_id: &str) -> Result<VersionId, JournalError> {
        let writer = self.writer();
        let _lock = writer.lock()?;
        let path = self.dir.join(format!("{COUNTER_FILE}-{target_id}"));
        let current = match std::fs::read_to_string(&path) {
            Ok(raw) => raw
                .trim()
                .parse::<u64>()
                .map_err(|source| JournalError::Counter { path: path.clone(), source })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(source) => return Err(JournalError::Read { path, source }),
        };
        let next = current.saturating_add(1);
        writer.replace(&path, format!("{next}\n").as_bytes())?;
        Ok(VersionId(format!("run-{next}")))
    }

    pub async fn current_version(
        &self,
        target_id: &str,
        working_dir: &Path,
    ) -> Result<VersionId, JournalError> {
        let git = time::timeout(
            std::time::Duration::from_secs(2),
            Command::new("git")
                .args(["rev-parse", "--verify", "HEAD"])
                .env("GIT_OPTIONAL_LOCKS", "0")
                .current_dir(working_dir)
                .output(),
        )
        .await;
        if let Ok(Ok(output)) = git {
            let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if output.status.success()
                && (7..=64).contains(&sha.len())
                && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Ok(VersionId(sha));
            }
        }
        self.reserve_monotonic_version(target_id)
    }

    fn write_json(
        &self,
        writer: &AtomicFileWriter,
        path: &Path,
        journal: &DeployJournal,
    ) -> Result<(), JournalError> {
        let mut bytes = serde_json::to_vec_pretty(journal)?;
        bytes.push(b'\n');
        writer.replace(path, &bytes)?;
        Ok(())
    }

    fn writer(&self) -> AtomicFileWriter {
        AtomicFileWriter::new(&self.dir, LOCK_FILE, ".deploy-state")
    }
}

fn load_path(path: &Path) -> Result<DeployJournal, JournalError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DeployJournal::default());
        }
        Err(source) => return Err(JournalError::Read { path: path.to_path_buf(), source }),
    };
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|source| JournalError::Read { path: path.to_path_buf(), source })?;
    let journal = serde_json::from_str::<DeployJournal>(&raw)
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

    fn store() -> JournalStore {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        JournalStore::rooted(
            std::env::temp_dir().join(format!("kit-deploy-journal-{}-{id}", std::process::id())),
        )
    }

    #[test]
    fn journal_round_trip_preserves_typed_history() -> Result<(), JournalError> {
        let store = store();
        let entry = JournalEntry {
            version: VersionId("abc123".to_owned()),
            timestamp_secs: 42,
            operation: JournalOperation::Deploy,
            status: JournalStatus::Success,
            duration_ms: 1500,
            steps: vec![JournalStep {
                name: "Publish".to_owned(),
                status: JournalStepStatus::Success,
                duration_ms: 1400,
            }],
        };

        store.record_many([("preview".to_owned(), entry.clone())])?;
        let loaded = store.load()?;
        let _ = std::fs::remove_dir_all(&store.dir);

        assert_eq!(loaded.entries("preview"), [entry]);
        Ok(())
    }

    #[test]
    fn monotonic_versions_are_reserved_atomically() -> Result<(), JournalError> {
        let store = store();
        assert_eq!(store.reserve_monotonic_version("preview")?, VersionId("run-1".to_owned()));
        assert_eq!(store.reserve_monotonic_version("preview")?, VersionId("run-2".to_owned()));
        assert_eq!(store.reserve_monotonic_version("production")?, VersionId("run-1".to_owned()));
        let _ = std::fs::remove_dir_all(&store.dir);
        Ok(())
    }
}
