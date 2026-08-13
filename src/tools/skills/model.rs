use std::{fmt, path::PathBuf};

use serde::Serialize;
use thiserror::Error;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(super) struct SkillName(String);

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(super) enum SkillNameError {
    #[error("skill name must contain between 1 and 64 characters")]
    Length,
    #[error("skill name must use only lowercase ASCII letters, digits, and hyphens")]
    Characters,
    #[error("skill name must not start or end with a hyphen")]
    EdgeHyphen,
    #[error("skill name must not contain consecutive hyphens")]
    ConsecutiveHyphens,
}

impl SkillName {
    pub(super) fn parse(value: impl Into<String>) -> Result<Self, SkillNameError> {
        let value = value.into();
        if value.is_empty() || value.len() > 64 {
            return Err(SkillNameError::Length);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(SkillNameError::Characters);
        }
        if value.starts_with('-') || value.ends_with('-') {
            return Err(SkillNameError::EdgeHyphen);
        }
        if value.contains("--") {
            return Err(SkillNameError::ConsecutiveHyphens);
        }
        Ok(Self(value))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SkillName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug)]
pub(super) struct Skill {
    name: SkillName,
    description: String,
    path: PathBuf,
    markdown: String,
}

impl Skill {
    pub(super) fn new(
        name: SkillName,
        description: String,
        path: PathBuf,
        markdown: String,
    ) -> Self {
        Self { name, description, path, markdown }
    }

    pub(super) fn name(&self) -> &SkillName {
        &self.name
    }

    pub(super) fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub(super) fn markdown(&self) -> &str {
        &self.markdown
    }

    pub(super) fn summary(&self) -> SkillSummary {
        SkillSummary {
            name: self.name.clone(),
            description: self.description.clone(),
            path: self.path.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct SkillSummary {
    pub name: SkillName,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct InvalidSkill {
    pub directory: String,
    pub path: PathBuf,
    pub error: String,
}

#[derive(Clone, Debug)]
pub(super) struct Catalog {
    library: PathBuf,
    skills: Vec<Skill>,
    invalid: Vec<InvalidSkill>,
}

impl Catalog {
    pub(super) fn new(library: PathBuf, skills: Vec<Skill>, invalid: Vec<InvalidSkill>) -> Self {
        Self { library, skills, invalid }
    }

    pub(super) fn library(&self) -> &std::path::Path {
        &self.library
    }

    pub(super) fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub(super) fn invalid(&self) -> &[InvalidSkill] {
        &self.invalid
    }

    pub(super) fn find(&self, name: &SkillName) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name() == name)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProjectionScope {
    ThisProject,
    AllProjects,
}

impl ProjectionScope {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::ThisProject => "This project",
            Self::AllProjects => "All projects",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProjectionTarget {
    ClaudeCode,
    Codex,
}

impl ProjectionTarget {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) struct ProjectionId {
    pub scope: ProjectionScope,
    pub target: ProjectionTarget,
}

impl ProjectionId {
    pub(super) const ALL: [Self; 4] = [
        Self { scope: ProjectionScope::ThisProject, target: ProjectionTarget::ClaudeCode },
        Self { scope: ProjectionScope::ThisProject, target: ProjectionTarget::Codex },
        Self { scope: ProjectionScope::AllProjects, target: ProjectionTarget::ClaudeCode },
        Self { scope: ProjectionScope::AllProjects, target: ProjectionTarget::Codex },
    ];

    pub(super) const fn new(scope: ProjectionScope, target: ProjectionTarget) -> Self {
        Self { scope, target }
    }

    pub(super) fn label(self) -> String {
        format!("{} / {}", self.scope.label(), self.target.label())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OccupiedKind {
    File,
    Directory,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum ProjectionState {
    Disabled,
    Enabled { target: PathBuf },
    BrokenLink { target: PathBuf },
    ForeignLink { target: PathBuf, resolved_target: PathBuf },
    Occupied { kind: OccupiedKind },
}

impl ProjectionState {
    pub(super) const fn short_label(&self) -> &'static str {
        match self {
            Self::Disabled => "off",
            Self::Enabled { .. } => "on",
            Self::BrokenLink { .. } => "broken",
            Self::ForeignLink { .. } => "foreign",
            Self::Occupied { .. } => "occupied",
        }
    }

    pub(super) const fn is_problem(&self) -> bool {
        matches!(self, Self::BrokenLink { .. } | Self::ForeignLink { .. } | Self::Occupied { .. })
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub(super) enum ProjectionReport {
    Observed { id: ProjectionId, path: PathBuf, projection: ProjectionState },
    Unavailable { id: ProjectionId, reason: String },
}

impl ProjectionReport {
    pub(super) const fn id(&self) -> ProjectionId {
        match self {
            Self::Observed { id, .. } | Self::Unavailable { id, .. } => *id,
        }
    }

    pub(super) fn state(&self) -> Option<&ProjectionState> {
        match self {
            Self::Observed { projection, .. } => Some(projection),
            Self::Unavailable { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct SkillStatus {
    pub skill: SkillSummary,
    pub projections: Vec<ProjectionReport>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub(super) enum RepositoryReport {
    Available { root: PathBuf },
    Unavailable { reason: String },
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct SkillsSnapshot {
    pub library: PathBuf,
    pub repository: RepositoryReport,
    pub skills: Vec<SkillStatus>,
    pub invalid: Vec<InvalidSkill>,
}

impl SkillsSnapshot {
    pub(super) fn problem_count(&self) -> usize {
        self.invalid.len()
            + self
                .skills
                .iter()
                .flat_map(|skill| &skill.projections)
                .filter(|report| match report {
                    ProjectionReport::Observed { projection, .. } => projection.is_problem(),
                    ProjectionReport::Unavailable { id, .. } => {
                        id.scope == ProjectionScope::ThisProject
                    }
                })
                .count()
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum LibraryReport {
    Unconfigured,
    Configured { path: PathBuf },
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct LibrarySetReport {
    pub path: PathBuf,
    pub created: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum OperationKind {
    Enable,
    Disable,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProjectionOutcome {
    Enabled,
    AlreadyEnabled,
    Disabled,
    AlreadyDisabled,
}

impl ProjectionOutcome {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::AlreadyEnabled => "already enabled",
            Self::Disabled => "disabled",
            Self::AlreadyDisabled => "already disabled",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ProjectionChange {
    pub skill: SkillName,
    pub projection: ProjectionId,
    pub path: PathBuf,
    pub outcome: ProjectionOutcome,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct OperationReport {
    pub operation: OperationKind,
    pub changes: Vec<ProjectionChange>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum DoctorIssue {
    LibraryUnconfigured,
    LibraryUnavailable {
        path: PathBuf,
        error: String,
    },
    RepositoryUnavailable {
        error: String,
    },
    InvalidSkill {
        directory: String,
        path: PathBuf,
        error: String,
    },
    ProjectionProblem {
        skill: SkillName,
        projection: ProjectionId,
        path: PathBuf,
        state: ProjectionState,
    },
    ProjectionUnavailable {
        skill: SkillName,
        projection: ProjectionId,
        error: String,
    },
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct DoctorReport {
    pub library: LibraryReport,
    pub issues: Vec<DoctorIssue>,
}

impl DoctorReport {
    pub(super) fn healthy(&self) -> bool {
        self.issues.is_empty()
    }
}
