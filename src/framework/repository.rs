use std::path::{Path, PathBuf};

use thiserror::Error;

/// Locates canonical repository boundaries without owning Git operations.
#[derive(Clone, Copy, Debug, Default)]
pub struct RepositoryLocator;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WorktreeRoot(PathBuf);

#[derive(Debug, Error)]
pub enum RepositoryRootError {
    #[error("{} is not inside a Git worktree", start.display())]
    NotInWorktree { start: PathBuf },
    #[error("canonicalize repository lookup path {}: {source}", path.display())]
    CanonicalizePath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("inspect Git worktree marker {}: {source}", path.display())]
    InspectMarker {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl RepositoryLocator {
    pub const fn new() -> Self {
        Self
    }

    pub fn nearest_worktree_root(&self, start: &Path) -> Result<WorktreeRoot, RepositoryRootError> {
        let canonical_start = start.canonicalize().map_err(|source| {
            RepositoryRootError::CanonicalizePath { path: start.to_path_buf(), source }
        })?;
        for ancestor in canonical_start.ancestors() {
            let marker = ancestor.join(".git");
            match std::fs::symlink_metadata(&marker) {
                Ok(_) => return Ok(WorktreeRoot(ancestor.to_path_buf())),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(RepositoryRootError::InspectMarker { path: marker, source });
                }
            }
        }
        Err(RepositoryRootError::NotInWorktree { start: canonical_start })
    }
}

impl WorktreeRoot {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}
