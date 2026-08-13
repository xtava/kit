use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};

use crate::framework::{ConfigStore, RepositoryLocator};

use super::{
    catalog::{create_skill, load_catalog},
    config::{Config, UiConfig},
    model::{
        DoctorIssue, DoctorReport, LibraryReport, LibrarySetReport, OperationKind, OperationReport,
        ProjectionChange, ProjectionId, ProjectionOutcome, ProjectionReport, ProjectionScope,
        ProjectionState, RepositoryReport, Skill, SkillName, SkillStatus, SkillSummary,
        SkillsSnapshot,
    },
    projections::{disable_projection, enable_projection, inspect_projection, ProjectionRoots},
};

pub(super) struct SkillsController {
    config: Config,
    repositories: RepositoryLocator,
    working_directory: PathBuf,
    home: PathBuf,
}

impl SkillsController {
    pub(super) fn new(
        store: ConfigStore,
        repositories: RepositoryLocator,
        working_directory: PathBuf,
        home: PathBuf,
    ) -> Result<Self> {
        let working_directory = working_directory.canonicalize().with_context(|| {
            format!("resolve working directory {}", working_directory.display())
        })?;
        let home = home
            .canonicalize()
            .with_context(|| format!("resolve home directory {}", home.display()))?;
        Ok(Self { config: Config::load(store)?, repositories, working_directory, home })
    }

    pub(super) fn library_report(&self) -> LibraryReport {
        match self.config.library() {
            Some(path) => LibraryReport::Configured { path: path.to_path_buf() },
            None => LibraryReport::Unconfigured,
        }
    }

    pub(super) fn set_library(
        &mut self,
        requested: &Path,
        create: bool,
    ) -> Result<LibrarySetReport> {
        let requested = self.resolve_input_path(requested);
        let parent =
            requested.parent().context("Skills library path must have a parent directory")?;
        let path = match parent.canonicalize() {
            Ok(canonical_parent) => {
                let leaf =
                    requested.file_name().context("Skills library path must name a directory")?;
                canonical_parent.join(leaf)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                fs::create_dir_all(parent).with_context(|| {
                    format!("create parent directories for Skills library {}", requested.display())
                })?;
                requested.clone()
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("resolve parent of requested Skills library {}", requested.display())
                });
            }
        };
        let existed = match fs::symlink_metadata(&path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect requested Skills library {}", path.display())
                });
            }
        };
        if !existed {
            if !create {
                bail!(
                    "Skills library does not exist: {}; pass --create to create it",
                    path.display()
                );
            }
            fs::create_dir(&path)
                .with_context(|| format!("create Skills library {}", path.display()))?;
        }
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolve Skills library {}", path.display()))?;
        if !fs::metadata(&canonical)
            .with_context(|| format!("inspect Skills library {}", canonical.display()))?
            .is_dir()
        {
            bail!("Skills library is not a directory: {}", canonical.display());
        }
        self.config.set_library(canonical.clone())?;
        Ok(LibrarySetReport { path: canonical, created: !existed })
    }

    pub(super) fn create(&self, name: &str, description: &str) -> Result<SkillSummary> {
        let name = SkillName::parse(name.to_owned()).context("validate skill name")?;
        let library = self.library_path()?;
        Ok(create_skill(library, name, description)?.summary())
    }

    pub(super) fn snapshot(&self, repository: Option<&Path>) -> Result<SkillsSnapshot> {
        let catalog = load_catalog(self.library_path()?)?;
        let (repository_report, roots) = self.resolve_roots(repository);
        let mut skills = Vec::with_capacity(catalog.skills().len());
        for skill in catalog.skills() {
            let canonical_skill = skill.path().canonicalize().with_context(|| {
                format!("resolve canonical skill directory {}", skill.path().display())
            })?;
            let mut projections = Vec::with_capacity(ProjectionId::ALL.len());
            for id in ProjectionId::ALL {
                let report = match roots.destination(id, skill.name()) {
                    Ok(path) => match inspect_projection(&path, &canonical_skill) {
                        Ok(projection) => ProjectionReport::Observed { id, path, projection },
                        Err(error) => {
                            ProjectionReport::Unavailable { id, reason: error.to_string() }
                        }
                    },
                    Err(error) => ProjectionReport::Unavailable { id, reason: error.to_string() },
                };
                projections.push(report);
            }
            skills.push(SkillStatus { skill: skill.summary(), projections });
        }
        Ok(SkillsSnapshot {
            library: catalog.library().to_path_buf(),
            repository: repository_report,
            skills,
            invalid: catalog.invalid().to_vec(),
        })
    }

    pub(super) fn skill(&self, name: &str) -> Result<Skill> {
        let name = SkillName::parse(name.to_owned()).context("validate skill name")?;
        let catalog = load_catalog(self.library_path()?)?;
        catalog
            .find(&name)
            .cloned()
            .with_context(|| format!("no valid canonical skill exactly matches {name:?}"))
    }

    pub(super) fn mutate(
        &self,
        operation: OperationKind,
        names: &[String],
        projections: &[ProjectionId],
        repository: Option<&Path>,
    ) -> Result<OperationReport> {
        if names.is_empty() {
            bail!("at least one skill name is required");
        }
        if projections.is_empty() {
            bail!("at least one availability destination is required");
        }

        let catalog = load_catalog(self.library_path()?)?;
        let (_, roots) = self.resolve_roots(repository);
        let mut seen = HashSet::new();
        let mut plan = Vec::new();
        for raw_name in names {
            let name = SkillName::parse(raw_name.clone())
                .with_context(|| format!("validate skill name {raw_name:?}"))?;
            if !seen.insert(name.clone()) {
                continue;
            }
            let skill = catalog.find(&name).with_context(|| {
                if let Some(invalid) =
                    catalog.invalid().iter().find(|item| item.directory == raw_name.as_str())
                {
                    format!("skill {raw_name:?} is invalid: {}", invalid.error)
                } else {
                    format!("no canonical skill exactly matches {raw_name:?}")
                }
            })?;
            let canonical_skill = skill.path().canonicalize().with_context(|| {
                format!("resolve canonical skill directory {}", skill.path().display())
            })?;
            for id in projections {
                let destination = roots
                    .destination(*id, skill.name())
                    .with_context(|| format!("resolve {} availability for {name}", id.label()))?;
                plan.push((name.clone(), *id, canonical_skill.clone(), destination));
            }
        }

        for (name, id, canonical_skill, destination) in &plan {
            let state = inspect_projection(destination, canonical_skill)
                .with_context(|| format!("preflight {} availability for {name}", id.label()))?;
            let safe = matches!(
                (operation, &state),
                (
                    OperationKind::Enable,
                    ProjectionState::Disabled | ProjectionState::Enabled { .. }
                ) | (
                    OperationKind::Disable,
                    ProjectionState::Disabled | ProjectionState::Enabled { .. }
                )
            );
            if !safe {
                bail!(
                    "refusing to {} {} availability for {} at {}: state is {}",
                    operation.label(),
                    id.label(),
                    name,
                    destination.display(),
                    state.short_label()
                );
            }
        }

        let mut changes = Vec::with_capacity(plan.len());
        let mut rollback = Vec::new();
        for (name, id, canonical_skill, destination) in &plan {
            let result = match operation {
                OperationKind::Enable => enable_projection(destination, canonical_skill),
                OperationKind::Disable => disable_projection(destination, canonical_skill),
            };
            let outcome = match result {
                Ok(outcome) => outcome,
                Err(error) => {
                    let rollback_errors = rollback_changes(&rollback);
                    if rollback_errors.is_empty() {
                        return Err(error).with_context(|| {
                            format!(
                                "{} {} availability for {}; earlier changes were rolled back",
                                operation.label(),
                                id.label(),
                                name
                            )
                        });
                    }
                    return Err(error).context(format!(
                        "{} {} availability for {}; rollback also failed: {}",
                        operation.label(),
                        id.label(),
                        name,
                        rollback_errors.join("; ")
                    ));
                }
            };
            match (operation, outcome) {
                (OperationKind::Enable, ProjectionOutcome::Enabled) => {
                    rollback.push(RollbackChange::Disable {
                        path: destination.clone(),
                        canonical_skill: canonical_skill.clone(),
                    })
                }
                (OperationKind::Disable, ProjectionOutcome::Disabled) => {
                    rollback.push(RollbackChange::Enable {
                        path: destination.clone(),
                        canonical_skill: canonical_skill.clone(),
                    })
                }
                _ => {}
            }
            changes.push(ProjectionChange {
                skill: name.clone(),
                projection: *id,
                path: destination.clone(),
                outcome,
            });
        }
        Ok(OperationReport { operation, changes })
    }

    pub(super) fn doctor(&self, repository: Option<&Path>) -> DoctorReport {
        let library = self.library_report();
        let library_path = match &library {
            LibraryReport::Configured { path } => path.clone(),
            LibraryReport::Unconfigured => {
                return DoctorReport { library, issues: vec![DoctorIssue::LibraryUnconfigured] };
            }
        };
        let snapshot = match self.snapshot(repository) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return DoctorReport {
                    library,
                    issues: vec![DoctorIssue::LibraryUnavailable {
                        path: library_path,
                        error: format!("{error:#}"),
                    }],
                };
            }
        };

        let repository_available =
            matches!(&snapshot.repository, RepositoryReport::Available { .. });
        let mut issues = Vec::new();
        if let RepositoryReport::Unavailable { reason } = &snapshot.repository {
            issues.push(DoctorIssue::RepositoryUnavailable { error: reason.clone() });
        }
        for invalid in &snapshot.invalid {
            issues.push(DoctorIssue::InvalidSkill {
                directory: invalid.directory.clone(),
                path: invalid.path.clone(),
                error: invalid.error.clone(),
            });
        }
        for skill in snapshot.skills {
            for projection in skill.projections {
                match projection {
                    ProjectionReport::Observed { id, path, projection }
                        if projection.is_problem() =>
                    {
                        issues.push(DoctorIssue::ProjectionProblem {
                            skill: skill.skill.name.clone(),
                            projection: id,
                            path,
                            state: projection,
                        });
                    }
                    ProjectionReport::Unavailable { id, reason }
                        if repository_available || id.scope == ProjectionScope::AllProjects =>
                    {
                        issues.push(DoctorIssue::ProjectionUnavailable {
                            skill: skill.skill.name.clone(),
                            projection: id,
                            error: reason,
                        });
                    }
                    _ => {}
                }
            }
        }
        DoctorReport { library, issues }
    }

    pub(super) fn ui(&self) -> &UiConfig {
        self.config.ui()
    }

    pub(super) fn config_store(&self) -> ConfigStore {
        self.config.store()
    }

    pub(super) fn reload_config(&mut self) -> Result<()> {
        self.config = Config::load(self.config.store())?;
        Ok(())
    }

    pub(super) fn set_panel_ratio(&mut self, ratio: crate::tui::SplitRatio) -> Result<()> {
        self.config.set_panel_ratio(ratio)
    }

    fn library_path(&self) -> Result<&Path> {
        self.config.library().context(
            "Skills library is not configured; run `kit skills library set <path> --create`",
        )
    }

    fn resolve_roots(&self, repository: Option<&Path>) -> (RepositoryReport, ProjectionRoots) {
        let start = repository.unwrap_or(&self.working_directory);
        match self.repositories.nearest_worktree_root(start) {
            Ok(root) => {
                let root = root.as_path().to_path_buf();
                (
                    RepositoryReport::Available { root: root.clone() },
                    ProjectionRoots::new(Some(root), self.home.clone()),
                )
            }
            Err(error) => (
                RepositoryReport::Unavailable { reason: error.to_string() },
                ProjectionRoots::new(None, self.home.clone()),
            ),
        }
    }

    fn resolve_input_path(&self, requested: &Path) -> PathBuf {
        let expanded = expand_home(requested, &self.home);
        if expanded.is_absolute() {
            expanded
        } else {
            self.working_directory.join(expanded)
        }
    }
}

impl OperationKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Enable => "enable",
            Self::Disable => "disable",
        }
    }
}

enum RollbackChange {
    Enable { path: PathBuf, canonical_skill: PathBuf },
    Disable { path: PathBuf, canonical_skill: PathBuf },
}

fn rollback_changes(changes: &[RollbackChange]) -> Vec<String> {
    let mut errors = Vec::new();
    for change in changes.iter().rev() {
        let result = match change {
            RollbackChange::Enable { path, canonical_skill } => {
                enable_projection(path, canonical_skill).map(|_| ())
            }
            RollbackChange::Disable { path, canonical_skill } => {
                disable_projection(path, canonical_skill).map(|_| ())
            }
        };
        if let Err(error) = result {
            errors.push(error.to_string());
        }
    }
    errors
}

fn expand_home(requested: &Path, home: &Path) -> PathBuf {
    let mut components = requested.components();
    let Some(first) = components.next() else {
        return requested.to_path_buf();
    };
    if first.as_os_str() != "~" {
        return requested.to_path_buf();
    }
    let mut expanded = home.to_path_buf();
    expanded.extend(components);
    expanded
}
