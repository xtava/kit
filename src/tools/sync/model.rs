use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

const MAX_NAME_BYTES: usize = 64;
const MAX_NODE_ID_BYTES: usize = 512;
const MAX_UNIX_USER_BYTES: usize = 64;
const MAX_PATTERN_BYTES: usize = 1024;
const DEFAULT_EXCLUDES: [&str; 18] = [
    "node_modules",
    "target",
    ".venv",
    "venv",
    "dist",
    "build",
    ".cache",
    ".next",
    ".turbo",
    "coverage",
    "__pycache__",
    "*.pyc",
    ".DS_Store",
    ".env",
    ".env.local",
    ".env.*.local",
    ".direnv",
    ".pytest_cache",
];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub(super) struct ProjectId(Uuid);

impl ProjectId {
    pub(super) fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ProjectId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ProjectId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LocalEndpoint {
    root: PathBuf,
}

impl LocalEndpoint {
    pub(super) fn new(root: PathBuf) -> Result<Self, ProjectValidationError> {
        validate_root(&root, EndpointSide::Local)?;
        Ok(Self { root })
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    fn validate(&self) -> Result<(), ProjectValidationError> {
        validate_root(&self.root, EndpointSide::Local)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
struct StableNodeId(String);

impl StableNodeId {
    fn new(value: String) -> Result<Self, ProjectValidationError> {
        if value.is_empty()
            || value.len() > MAX_NODE_ID_BYTES
            || value.chars().any(char::is_control)
        {
            Err(ProjectValidationError::StableNodeId)
        } else {
            Ok(Self(value))
        }
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<(), ProjectValidationError> {
        Self::new(self.0.clone()).map(|_| ())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
struct UnixUser(String);

impl UnixUser {
    fn new(value: String) -> Result<Self, ProjectValidationError> {
        if valid_unix_user(&value) {
            Ok(Self(value))
        } else {
            Err(ProjectValidationError::UnixUser)
        }
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<(), ProjectValidationError> {
        Self::new(self.0.clone()).map(|_| ())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RemoteEndpoint {
    stable_node_id: StableNodeId,
    unix_user: UnixUser,
    root: PathBuf,
}

impl RemoteEndpoint {
    pub(super) fn new(
        stable_node_id: impl Into<String>,
        unix_user: impl Into<String>,
        root: PathBuf,
    ) -> Result<Self, ProjectValidationError> {
        let endpoint = Self {
            stable_node_id: StableNodeId::new(stable_node_id.into())?,
            unix_user: UnixUser::new(unix_user.into())?,
            root,
        };
        endpoint.validate()?;
        Ok(endpoint)
    }

    pub(super) fn stable_node_id(&self) -> &str {
        self.stable_node_id.as_str()
    }

    pub(super) fn unix_user(&self) -> &str {
        self.unix_user.as_str()
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    fn validate(&self) -> Result<(), ProjectValidationError> {
        self.stable_node_id.validate()?;
        self.unix_user.validate()?;
        validate_root(&self.root, EndpointSide::Remote)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SourcePolicy {
    #[serde(default)]
    additional_excludes: Vec<String>,
    #[serde(default)]
    additional_includes: Vec<String>,
}

impl SourcePolicy {
    pub(super) fn new(
        excludes: Vec<String>,
        includes: Vec<String>,
    ) -> Result<Self, ProjectValidationError> {
        let policy = Self { additional_excludes: excludes, additional_includes: includes };
        policy.validate()?;
        Ok(policy)
    }

    pub(super) fn excludes(&self) -> Vec<&str> {
        let mut seen = HashSet::new();
        DEFAULT_EXCLUDES
            .iter()
            .copied()
            .chain(self.additional_excludes.iter().map(String::as_str))
            .filter(|pattern| seen.insert(*pattern))
            .collect()
    }

    pub(super) fn includes(&self) -> impl Iterator<Item = &str> {
        self.additional_includes.iter().map(String::as_str)
    }

    fn validate(&self) -> Result<(), ProjectValidationError> {
        let mut patterns = HashSet::new();
        for pattern in self.additional_excludes.iter().chain(&self.additional_includes) {
            if !valid_pattern(pattern) {
                return Err(ProjectValidationError::SourcePattern(pattern.clone()));
            }
            if !patterns.insert(pattern.as_str()) {
                return Err(ProjectValidationError::DuplicateSourcePattern(pattern.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum ProjectLifecycle {
    Creating,
    Active,
    Paused,
    Removing,
}

impl ProjectLifecycle {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Removing => "removing",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SyncedProject {
    id: ProjectId,
    name: String,
    local: LocalEndpoint,
    remote: RemoteEndpoint,
    source: SourcePolicy,
    lifecycle: ProjectLifecycle,
}

impl SyncedProject {
    pub(super) fn new(
        name: impl Into<String>,
        local: LocalEndpoint,
        remote: RemoteEndpoint,
        source: SourcePolicy,
    ) -> Result<Self, ProjectValidationError> {
        let project = Self {
            id: ProjectId::new(),
            name: name.into(),
            local,
            remote,
            source,
            lifecycle: ProjectLifecycle::Creating,
        };
        project.validate()?;
        Ok(project)
    }

    pub(super) fn id(&self) -> ProjectId {
        self.id
    }

    pub(super) fn name(&self) -> &str {
        &self.name
    }

    pub(super) fn local(&self) -> &LocalEndpoint {
        &self.local
    }

    pub(super) fn remote(&self) -> &RemoteEndpoint {
        &self.remote
    }

    pub(super) fn source(&self) -> &SourcePolicy {
        &self.source
    }

    pub(super) fn lifecycle(&self) -> ProjectLifecycle {
        self.lifecycle
    }

    pub(super) fn set_lifecycle(&mut self, lifecycle: ProjectLifecycle) {
        self.lifecycle = lifecycle;
    }

    pub(super) fn validate(&self) -> Result<(), ProjectValidationError> {
        if !valid_name(&self.name) {
            return Err(ProjectValidationError::Name(self.name.clone()));
        }
        self.local.validate()?;
        self.remote.validate()?;
        self.source.validate()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EndpointSide {
    Local,
    Remote,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(super) enum ProjectValidationError {
    #[error("Synced Project name {0:?} must contain 1-64 ASCII letters, numbers, '.', '_' or '-'")]
    Name(String),
    #[error("local synchronization root must be an absolute, normalized non-root path")]
    LocalRoot,
    #[error("remote synchronization root must be an absolute, normalized non-root path")]
    RemoteRoot,
    #[error("Tailscale stable node identity is empty, too long, or contains control characters")]
    StableNodeId,
    #[error("remote Unix user must contain 1-64 ASCII letters, numbers, '.', '_' or '-'")]
    UnixUser,
    #[error("source pattern {0:?} is empty, negated, too long, or contains control characters")]
    SourcePattern(String),
    #[error("source pattern {0:?} is configured more than once")]
    DuplicateSourcePattern(String),
}

fn validate_root(path: &Path, side: EndpointSide) -> Result<(), ProjectValidationError> {
    let invalid = !path.is_absolute()
        || path.parent().is_none()
        || path.to_str().is_none()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir));
    if invalid {
        Err(match side {
            EndpointSide::Local => ProjectValidationError::LocalRoot,
            EndpointSide::Remote => ProjectValidationError::RemoteRoot,
        })
    } else {
        Ok(())
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_unix_user(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= MAX_UNIX_USER_BYTES
        && user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_pattern(pattern: &str) -> bool {
    !pattern.is_empty()
        && pattern.len() <= MAX_PATTERN_BYTES
        && !pattern.starts_with('!')
        && !pattern.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> SyncedProject {
        SyncedProject::new(
            "project",
            LocalEndpoint::new(PathBuf::from("/work/project")).unwrap(),
            RemoteEndpoint::new("node-remote", "remote-user", PathBuf::from("/workspace/project"))
                .unwrap(),
            SourcePolicy::default(),
        )
        .unwrap()
    }

    #[test]
    fn valid_project_has_one_stable_identity_and_endpoint_contract() {
        let project = project();

        assert_eq!(project.name(), "project");
        assert_eq!(project.local().root(), Path::new("/work/project"));
        assert_eq!(project.lifecycle(), ProjectLifecycle::Creating);
        assert_eq!(project.remote().stable_node_id(), "node-remote");
        assert_eq!(project.remote().unix_user(), "remote-user");
        assert_eq!(project.remote().root(), Path::new("/workspace/project"));
    }

    #[test]
    fn roots_names_users_and_patterns_reject_ambiguous_command_shapes() {
        let endpoint =
            RemoteEndpoint::new("node-remote", "remote-user", PathBuf::from("/workspace/project"))
                .unwrap();
        assert!(matches!(
            SyncedProject::new(
                "bad name",
                LocalEndpoint::new(PathBuf::from("/work/project")).unwrap(),
                endpoint.clone(),
                SourcePolicy::default()
            ),
            Err(ProjectValidationError::Name(_))
        ));
        assert!(matches!(
            LocalEndpoint::new(PathBuf::from("relative")),
            Err(ProjectValidationError::LocalRoot)
        ));
        assert!(matches!(
            RemoteEndpoint::new(
                "node-remote",
                "remote-user@host",
                PathBuf::from("/workspace/project"),
            ),
            Err(ProjectValidationError::UnixUser)
        ));
        assert!(matches!(
            SourcePolicy::new(vec!["target".to_owned()], vec!["target".to_owned()]),
            Err(ProjectValidationError::DuplicateSourcePattern(_))
        ));
        assert!(matches!(
            SourcePolicy::new(vec!["!target".to_owned()], Vec::new()),
            Err(ProjectValidationError::SourcePattern(_))
        ));
    }

    #[test]
    fn explicit_source_policy_extends_stable_defaults_without_engine_syntax() {
        let policy = SourcePolicy::new(
            vec!["build".to_owned(), "*.generated".to_owned()],
            vec!["build/schema.generated".to_owned()],
        )
        .unwrap();

        assert!(policy.excludes().into_iter().any(|pattern| pattern == "*.generated"));
        assert!(policy.includes().any(|pattern| pattern == "build/schema.generated"));
    }
}
