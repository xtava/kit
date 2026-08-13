use std::{
    fs,
    path::{Path, PathBuf},
};

use thiserror::Error;

use super::model::{
    OccupiedKind, ProjectionId, ProjectionOutcome, ProjectionScope, ProjectionState,
    ProjectionTarget, SkillName,
};

#[derive(Clone, Debug)]
pub(super) struct ProjectionRoots {
    project: Option<PathBuf>,
    home: PathBuf,
}

impl ProjectionRoots {
    pub(super) fn new(project: Option<PathBuf>, home: PathBuf) -> Self {
        Self { project, home }
    }

    pub(super) fn destination(
        &self,
        id: ProjectionId,
        name: &SkillName,
    ) -> Result<PathBuf, ProjectionUnavailable> {
        let root = match id.scope {
            ProjectionScope::ThisProject => {
                self.project.as_deref().ok_or(ProjectionUnavailable::ProjectRoot)?
            }
            ProjectionScope::AllProjects => &self.home,
        };
        let platform = match id.target {
            ProjectionTarget::ClaudeCode => ".claude",
            ProjectionTarget::Codex => ".agents",
        };
        Ok(root.join(platform).join("skills").join(name.as_str()))
    }
}

#[derive(Clone, Copy, Debug, Error)]
pub(super) enum ProjectionUnavailable {
    #[error("This project is unavailable because no Git worktree could be resolved")]
    ProjectRoot,
}

#[derive(Debug, Error)]
pub(super) enum ProjectionError {
    #[error("inspect availability path {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("read availability link {path}: {source}")]
    ReadLink {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("resolve availability link {path} -> {target}: {source}")]
    ResolveLink {
        path: PathBuf,
        target: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("prepare availability directory {path}: {source}")]
    PrepareParent {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("availability parent is not a directory: {path}")]
    ParentNotDirectory { path: PathBuf },
    #[error("create availability link {path} -> {target}: {source}")]
    Create {
        path: PathBuf,
        target: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("remove availability link {path}: {source}")]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("refusing to change {path}: availability path is {state}")]
    UnsafeState { path: PathBuf, state: &'static str },
}

pub(super) fn inspect_projection(
    path: &Path,
    canonical_skill: &Path,
) -> Result<ProjectionState, ProjectionError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProjectionState::Disabled);
        }
        Err(source) => {
            return Err(ProjectionError::Inspect { path: path.to_path_buf(), source });
        }
    };

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)
            .map_err(|source| ProjectionError::ReadLink { path: path.to_path_buf(), source })?;
        let resolution_path = if target.is_absolute() {
            target.clone()
        } else {
            path.parent().unwrap_or_else(|| Path::new(".")).join(&target)
        };
        let resolved_target = match resolution_path.canonicalize() {
            Ok(resolved) => resolved,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProjectionState::BrokenLink { target });
            }
            Err(source) => {
                return Err(ProjectionError::ResolveLink {
                    path: path.to_path_buf(),
                    target,
                    source,
                });
            }
        };
        return if resolved_target == canonical_skill {
            Ok(ProjectionState::Enabled { target })
        } else {
            Ok(ProjectionState::ForeignLink { target, resolved_target })
        };
    }

    let kind = if metadata.is_file() {
        OccupiedKind::File
    } else if metadata.is_dir() {
        OccupiedKind::Directory
    } else {
        OccupiedKind::Other
    };
    Ok(ProjectionState::Occupied { kind })
}

pub(super) fn enable_projection(
    path: &Path,
    canonical_skill: &Path,
) -> Result<ProjectionOutcome, ProjectionError> {
    match inspect_projection(path, canonical_skill)? {
        ProjectionState::Enabled { .. } => return Ok(ProjectionOutcome::AlreadyEnabled),
        ProjectionState::Disabled => {}
        state => return Err(unsafe_state(path, &state)),
    }

    ensure_parent(path)?;
    match inspect_projection(path, canonical_skill)? {
        ProjectionState::Enabled { .. } => return Ok(ProjectionOutcome::AlreadyEnabled),
        ProjectionState::Disabled => {}
        state => return Err(unsafe_state(path, &state)),
    }

    match std::os::unix::fs::symlink(canonical_skill, path) {
        Ok(()) => Ok(ProjectionOutcome::Enabled),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match inspect_projection(path, canonical_skill)? {
                ProjectionState::Enabled { .. } => Ok(ProjectionOutcome::AlreadyEnabled),
                state => Err(unsafe_state(path, &state)),
            }
        }
        Err(source) => Err(ProjectionError::Create {
            path: path.to_path_buf(),
            target: canonical_skill.to_path_buf(),
            source,
        }),
    }
}

pub(super) fn disable_projection(
    path: &Path,
    canonical_skill: &Path,
) -> Result<ProjectionOutcome, ProjectionError> {
    match inspect_projection(path, canonical_skill)? {
        ProjectionState::Disabled => Ok(ProjectionOutcome::AlreadyDisabled),
        ProjectionState::Enabled { .. } => {
            match inspect_projection(path, canonical_skill)? {
                ProjectionState::Enabled { .. } => {}
                ProjectionState::Disabled => return Ok(ProjectionOutcome::AlreadyDisabled),
                state => return Err(unsafe_state(path, &state)),
            }
            fs::remove_file(path)
                .map_err(|source| ProjectionError::Remove { path: path.to_path_buf(), source })?;
            Ok(ProjectionOutcome::Disabled)
        }
        state => Err(unsafe_state(path, &state)),
    }
}

fn ensure_parent(path: &Path) -> Result<(), ProjectionError> {
    let parent = path
        .parent()
        .ok_or_else(|| ProjectionError::ParentNotDirectory { path: path.to_path_buf() })?;
    let platform = parent
        .parent()
        .ok_or_else(|| ProjectionError::ParentNotDirectory { path: parent.to_path_buf() })?;
    for candidate in [platform, parent] {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) => ensure_existing_directory(candidate, &metadata)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(candidate) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        let metadata = fs::symlink_metadata(candidate).map_err(|source| {
                            ProjectionError::PrepareParent { path: candidate.to_path_buf(), source }
                        })?;
                        ensure_existing_directory(candidate, &metadata)?;
                    }
                    Err(source) => {
                        return Err(ProjectionError::PrepareParent {
                            path: candidate.to_path_buf(),
                            source,
                        });
                    }
                }
            }
            Err(source) => {
                return Err(ProjectionError::PrepareParent {
                    path: candidate.to_path_buf(),
                    source,
                });
            }
        }
    }
    Ok(())
}

fn ensure_existing_directory(path: &Path, metadata: &fs::Metadata) -> Result<(), ProjectionError> {
    if metadata.is_dir() {
        return Ok(());
    }
    if metadata.file_type().is_symlink() {
        return match fs::metadata(path) {
            Ok(target) if target.is_dir() => Ok(()),
            Ok(_) => Err(ProjectionError::ParentNotDirectory { path: path.to_path_buf() }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(ProjectionError::ParentNotDirectory { path: path.to_path_buf() })
            }
            Err(source) => Err(ProjectionError::PrepareParent { path: path.to_path_buf(), source }),
        };
    }
    Err(ProjectionError::ParentNotDirectory { path: path.to_path_buf() })
}

fn unsafe_state(path: &Path, state: &ProjectionState) -> ProjectionError {
    ProjectionError::UnsafeState { path: path.to_path_buf(), state: state.short_label() }
}
