#![cfg(feature = "simple_tempdir")]

use crate::{BlobReaderSource, BlobStorage, ContentId, Error, LeaseId};
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

pub struct SimpleTempDir {
    root: TempDir,
    refs: Mutex<HashMap<ContentId, usize>>,
}

impl SimpleTempDir {
    pub fn new() -> Result<Self, Error> {
        let root = tempfile::Builder::new()
            .prefix("wezterm-blob-lease-")
            .rand_bytes(8)
            .tempdir()?;
        Ok(Self {
            root,
            refs: Mutex::new(HashMap::new()),
        })
    }

    pub fn new_in<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        let root = tempfile::Builder::new()
            .prefix("wezterm-blob-lease-")
            .rand_bytes(8)
            .tempdir_in(path)?;
        Ok(Self {
            root,
            refs: Mutex::new(HashMap::new()),
        })
    }

    fn path_for_content(&self, content_id: ContentId) -> Result<PathBuf, Error> {
        let path = self.root.path().join(format!("{content_id}"));
        std::fs::create_dir_all(path.parent().unwrap())
            .map_err(|err| Error::StorageDirIoError(path.clone(), err))?;
        Ok(path)
    }

    fn del_ref(&self, content_id: ContentId) {
        let mut refs = self.refs.lock().unwrap();
        match refs.get_mut(&content_id) {
            Some(count) if *count == 1 => {
                if let Ok(path) = self.path_for_content(content_id) {
                    if let Err(err) = std::fs::remove_file(&path) {
                        eprintln!("Failed to remove {}: {err:#}", path.display());
                    }
                }
                *count = 0;
            }
            Some(count) => {
                *count -= 1;
            }
            None => {
                // Shouldn't really happen...
            }
        }
    }
}

impl BlobStorage for SimpleTempDir {
    fn store(&self, content_id: ContentId, data: &[u8], _lease_id: LeaseId) -> Result<(), Error> {
        let mut refs = self.refs.lock().unwrap();

        let path = self.path_for_content(content_id)?;
        let mut file = tempfile::Builder::new()
            .prefix("new-")
            .rand_bytes(5)
            .tempfile_in(&self.root.path())?;

        file.write_all(data)?;
        file.persist(&path)
            .map_err(|persist_err| persist_err.error)?;

        *refs.entry(content_id).or_insert(0) += 1;

        Ok(())
    }

    fn lease_by_content(&self, content_id: ContentId, _lease_id: LeaseId) -> Result<(), Error> {
        let mut refs = self.refs.lock().unwrap();

        let path = self.path_for_content(content_id)?;
        if path.exists() {
            *refs.entry(content_id).or_insert(0) += 1;
            Ok(())
        } else {
            Err(Error::ContentNotFound(content_id))
        }
    }

    fn open_reader(&self, content_id: ContentId) -> Result<BlobReaderSource, Error> {
        let path = self.path_for_content(content_id)?;
        let file = std::fs::File::open(&path)
            .map_err(|error| Error::StorageDirIoError(path.clone(), error))?;
        let declared_len = file
            .metadata()
            .map_err(|error| Error::StorageDirIoError(path, error))?
            .len();

        Ok(BlobReaderSource::new(BufReader::new(file), declared_len))
    }

    fn advise_lease_dropped(&self, _lease_id: LeaseId, content_id: ContentId) -> Result<(), Error> {
        self.del_ref(content_id);
        Ok(())
    }

    fn advise_of_pid(&self, _pid: u32) -> Result<(), Error> {
        Ok(())
    }

    fn advise_pid_terminated(&self, _pid: u32) -> Result<(), Error> {
        Ok(())
    }
}
