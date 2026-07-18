use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};

use thiserror::Error;
use tokio::sync::watch;

const RETRY_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub struct TurnPermitPool {
    directory: PathBuf,
    limit: usize,
}

pub struct TurnPermit {
    slot: usize,
    lock: File,
}

impl TurnPermit {
    pub fn slot(&self) -> usize {
        self.slot
    }
}

impl Drop for TurnPermit {
    fn drop(&mut self) {
        let _ = self.lock.unlock();
    }
}

#[derive(Debug, Error)]
pub enum PermitError {
    #[error("turn permit limit must be greater than zero")]
    ZeroLimit,
    #[error("create turn permit directory {}: {source}", path.display())]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("open turn permit slot {}: {source}", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("lock turn permit slot {}: {source}", path.display())]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("turn permit wait was cancelled")]
    Cancelled,
}

impl TurnPermitPool {
    pub fn new(root: &Path, limit: usize) -> Result<Self, PermitError> {
        if limit == 0 {
            return Err(PermitError::ZeroLimit);
        }
        let directory = root.join("turn-permits");
        std::fs::create_dir_all(&directory)
            .map_err(|source| PermitError::CreateDirectory { path: directory.clone(), source })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).map_err(
                |source| PermitError::CreateDirectory { path: directory.clone(), source },
            )?;
        }
        Ok(Self { directory, limit })
    }

    pub fn try_acquire(&self) -> Result<Option<TurnPermit>, PermitError> {
        for slot in 0..self.limit {
            let path = self.directory.join(format!("slot-{slot}.lock"));
            let lock = open_slot(&path)?;
            match lock.try_lock() {
                Ok(()) => return Ok(Some(TurnPermit { slot, lock })),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(source)) => {
                    return Err(PermitError::Lock { path, source });
                }
            }
        }
        Ok(None)
    }

    pub async fn acquire(
        &self,
        mut cancelled: watch::Receiver<bool>,
    ) -> Result<TurnPermit, PermitError> {
        loop {
            if *cancelled.borrow() {
                return Err(PermitError::Cancelled);
            }
            if let Some(permit) = self.try_acquire()? {
                return Ok(permit);
            }
            tokio::select! {
                _ = tokio::time::sleep(RETRY_INTERVAL) => {}
                changed = cancelled.changed() => {
                    if changed.is_err() || *cancelled.borrow() {
                        return Err(PermitError::Cancelled);
                    }
                }
            }
        }
    }
}

fn open_slot(path: &Path) -> Result<File, PermitError> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|source| PermitError::Open { path: path.to_path_buf(), source })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|source| PermitError::Open { path: path.to_path_buf(), source })?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kit-swarm-limiter-{name}-{}", std::process::id()))
    }

    #[tokio::test]
    async fn permits_bound_concurrency_and_waits_are_cancellable() {
        let root = temp_dir("bound");
        let _ = std::fs::remove_dir_all(&root);
        let pool = TurnPermitPool::new(&root, 2).unwrap();
        let first = pool.try_acquire().unwrap().unwrap();
        let second = pool.try_acquire().unwrap().unwrap();
        assert!(pool.try_acquire().unwrap().is_none());
        drop(first);
        let third = pool.try_acquire().unwrap().unwrap();
        drop(third);

        let single = TurnPermitPool::new(&root.join("single"), 1).unwrap();
        let held = single.try_acquire().unwrap().unwrap();
        let (cancel, cancelled) = watch::channel(false);
        let waiter = tokio::spawn(async move { single.acquire(cancelled).await });
        cancel.send(true).unwrap();
        assert!(matches!(waiter.await.unwrap(), Err(PermitError::Cancelled)));
        drop(held);
        drop(second);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn permit_limit_is_shared_across_processes() {
        let root = temp_dir("process-bound");
        let _ = std::fs::remove_dir_all(&root);
        TurnPermitPool::new(&root, 2).unwrap();
        let executable = std::env::current_exe().unwrap();
        let mut children: Vec<_> = (0..3)
            .map(|_| {
                std::process::Command::new(&executable)
                    .arg("--ignored")
                    .arg("--exact")
                    .arg("tools::swarm::limiter::tests::permit_process_child")
                    .env("KIT_SWARM_PERMIT_ROOT", &root)
                    .spawn()
                    .unwrap()
            })
            .collect();

        wait_until(Duration::from_secs(5), || acquired_count(&root) == 2);
        assert_eq!(acquired_count(&root), 2);
        let first = acquired_pids(&root).into_iter().next().unwrap();
        std::fs::write(root.join(format!("release-{first}")), b"").unwrap();
        wait_until(Duration::from_secs(5), || acquired_count(&root) == 3);
        assert_eq!(acquired_count(&root), 3);

        for child in &children {
            std::fs::write(root.join(format!("release-{}", child.id())), b"").unwrap();
        }
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "helper invoked by permit_limit_is_shared_across_processes"]
    fn permit_process_child() {
        let root = PathBuf::from(std::env::var_os("KIT_SWARM_PERMIT_ROOT").unwrap());
        let pool = TurnPermitPool::new(&root, 2).unwrap();
        let (_cancel, cancelled) = watch::channel(false);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        let _permit = runtime.block_on(pool.acquire(cancelled)).unwrap();
        let pid = std::process::id();
        std::fs::write(root.join(format!("acquired-{pid}")), b"").unwrap();
        wait_until(Duration::from_secs(10), || root.join(format!("release-{pid}")).exists());
    }

    fn acquired_count(root: &Path) -> usize {
        acquired_pids(root).len()
    }

    fn acquired_pids(root: &Path) -> Vec<u32> {
        std::fs::read_dir(root)
            .unwrap()
            .flatten()
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .strip_prefix("acquired-")
                    .and_then(|pid| pid.parse().ok())
            })
            .collect()
    }

    fn wait_until(timeout: Duration, condition: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + timeout;
        while !condition() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
