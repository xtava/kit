use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use directories::ProjectDirs;

use crate::framework::{AtomicFileTryLock, AtomicFileWriter};

use super::model::{SlotState, SLOT_SCHEMA_VERSION};

pub(super) struct SlotStore {
    directory: PathBuf,
    path: PathBuf,
    writer: AtomicFileWriter,
}

impl SlotStore {
    pub(super) fn bootstrap() -> Result<Self> {
        let project = ProjectDirs::from("", "", "kit").context("resolve Kit state directory")?;
        let directory =
            project.state_dir().unwrap_or_else(|| project.data_local_dir()).join("stream");
        let path = directory.join("slot.json");
        let writer = AtomicFileWriter::new(&directory, "slot.lock", ".slot");
        Ok(Self { directory, path, writer })
    }

    pub(super) fn lock(&self) -> Result<std::fs::File> {
        match self.writer.try_lock()? {
            AtomicFileTryLock::Acquired(lock) => Ok(lock),
            AtomicFileTryLock::Busy => {
                bail!("another Stream Slot operation is already running; try again in a moment")
            }
        }
    }

    pub(super) fn load(&self) -> Result<Option<SlotState>> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", self.path.display()))
            }
        };
        let state: SlotState =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", self.path.display()))?;
        if state.schema_version != SLOT_SCHEMA_VERSION {
            bail!(
                "Stream Slot state uses schema {}; expected {}",
                state.schema_version,
                SLOT_SCHEMA_VERSION
            );
        }
        Ok(Some(state))
    }

    pub(super) fn save(&self, state: &SlotState) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(state).context("serialize Stream Slot state")?;
        bytes.push(b'\n');
        self.writer.replace(&self.path, &bytes)?;
        Ok(())
    }

    pub(super) fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => sync_directory(&self.directory),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove {}", self.path.display())),
        }
    }
}

fn sync_directory(directory: &std::path::Path) -> Result<()> {
    std::fs::File::open(directory)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync {}", directory.display()))
}
