use std::{
    fs::{File, OpenOptions, TryLockError},
    io::Write,
    path::{Path, PathBuf},
};

use thiserror::Error;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

/// Shared lock and atomic-replacement mechanics for small Kit-owned state files.
#[derive(Clone, Debug)]
pub struct AtomicFileWriter {
    dir: PathBuf,
    lock_name: String,
    temp_prefix: String,
}

#[derive(Debug)]
pub enum AtomicFileTryLock {
    Acquired(File),
    Busy,
}

#[derive(Debug, Error)]
pub enum AtomicFileError {
    #[error("create state directory {}: {source}", path.display())]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("open state lock {}: {source}", path.display())]
    OpenLock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("lock state file {}: {source}", path.display())]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("write state file {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl AtomicFileWriter {
    pub fn new(
        dir: impl Into<PathBuf>,
        lock_name: impl Into<String>,
        temp_prefix: impl Into<String>,
    ) -> Self {
        Self { dir: dir.into(), lock_name: lock_name.into(), temp_prefix: temp_prefix.into() }
    }

    /// Acquire the writer lock. Keep the returned file alive across any read-modify-write cycle.
    pub fn lock(&self) -> Result<File, AtomicFileError> {
        let (path, file) = self.open_lock()?;
        file.lock().map_err(|source| AtomicFileError::Lock { path, source })?;
        Ok(file)
    }

    /// Try to acquire the writer lock without waiting for another writer.
    pub fn try_lock(&self) -> Result<AtomicFileTryLock, AtomicFileError> {
        let (path, file) = self.open_lock()?;
        match file.try_lock() {
            Ok(()) => Ok(AtomicFileTryLock::Acquired(file)),
            Err(TryLockError::WouldBlock) => Ok(AtomicFileTryLock::Busy),
            Err(TryLockError::Error(source)) => Err(AtomicFileError::Lock { path, source }),
        }
    }

    /// Publish bytes by syncing a sibling temporary file and atomically replacing the destination.
    pub fn replace(&self, path: &Path, bytes: &[u8]) -> Result<(), AtomicFileError> {
        self.ensure_dir()?;
        let pending = self.dir.join(format!("{}.{}.tmp", self.temp_prefix, std::process::id()));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options
                .open(&pending)
                .map_err(|source| AtomicFileError::Write { path: pending.clone(), source })?;
            #[cfg(unix)]
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|source| AtomicFileError::Write { path: pending.clone(), source })?;
            file.write_all(bytes)
                .map_err(|source| AtomicFileError::Write { path: pending.clone(), source })?;
            file.sync_all()
                .map_err(|source| AtomicFileError::Write { path: pending.clone(), source })?;
            std::fs::rename(&pending, path)
                .map_err(|source| AtomicFileError::Write { path: path.to_path_buf(), source })?;
            #[cfg(unix)]
            {
                let parent = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let directory = File::open(parent).map_err(|source| AtomicFileError::Write {
                    path: parent.to_path_buf(),
                    source,
                })?;
                directory.sync_all().map_err(|source| AtomicFileError::Write {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(pending);
        }
        result
    }

    fn ensure_dir(&self) -> Result<(), AtomicFileError> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|source| AtomicFileError::CreateDirectory { path: self.dir.clone(), source })
    }

    fn open_lock(&self) -> Result<(PathBuf, File), AtomicFileError> {
        self.ensure_dir()?;
        let path = self.dir.join(&self.lock_name);
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let file = options
            .open(&path)
            .map_err(|source| AtomicFileError::OpenLock { path: path.clone(), source })?;
        let metadata = file
            .metadata()
            .map_err(|source| AtomicFileError::OpenLock { path: path.clone(), source })?;
        if !metadata.file_type().is_file() {
            return Err(AtomicFileError::OpenLock {
                path,
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "state lock is not a regular file",
                ),
            });
        }
        #[cfg(unix)]
        if metadata.nlink() != 1 {
            return Err(AtomicFileError::OpenLock {
                path,
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "state lock has multiple hard links",
                ),
            });
        }
        #[cfg(unix)]
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|source| AtomicFileError::OpenLock { path: path.clone(), source })?;
        Ok((path, file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_replacement_overwrites_without_leaving_a_temporary_file() {
        let dir = std::env::temp_dir().join(format!("kit-atomic-file-{}", std::process::id()));
        let path = dir.join("state.json");
        let writer = AtomicFileWriter::new(&dir, "state.lock", ".state");

        let lock = writer.lock().expect("lock state");
        writer.replace(&path, b"one").expect("write initial state");
        writer.replace(&path, b"two").expect("replace state");
        drop(lock);

        assert_eq!(std::fs::read(&path).expect("read state"), b"two");
        assert!(!dir.join(format!(".state.{}.tmp", std::process::id())).exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn try_lock_excludes_a_second_owner_and_releases_on_drop() {
        let dir = std::env::temp_dir().join(format!(
            "kit-atomic-file-lock-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let writer = AtomicFileWriter::new(&dir, "state.lock", ".state");

        let AtomicFileTryLock::Acquired(first) = writer.try_lock().expect("acquire first lock")
        else {
            panic!("first owner must acquire the lock");
        };
        assert!(matches!(writer.try_lock().expect("probe second lock"), AtomicFileTryLock::Busy));
        drop(first);
        assert!(matches!(
            writer.try_lock().expect("reacquire released lock"),
            AtomicFileTryLock::Acquired(_)
        ));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn lock_refuses_a_symlink_without_touching_its_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = std::env::temp_dir().join(format!(
            "kit-atomic-file-symlink-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("outside");
        std::fs::write(&target, b"unchanged").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&target, dir.join("state.lock")).unwrap();
        let writer = AtomicFileWriter::new(&dir, "state.lock", ".state");

        assert!(writer.try_lock().is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o644);

        let _ = std::fs::remove_dir_all(dir);
    }
}
