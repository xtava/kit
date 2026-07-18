use std::{
    collections::HashSet,
    ffi::OsString,
    fs::OpenOptions,
    io::Read,
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, Context as _, Result};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::framework::{process::ProcessLabel, WorktreeRoot};

pub const BUILD_MANIFEST_RELATIVE_PATH: &str = ".kit/build.toml";
const BUILD_MANIFEST_VERSION: u32 = 1;
const MAX_BUILD_MANIFEST_BYTES: u64 = 256 * 1024;
pub(super) const WORKFLOW_ID_PATTERN: &str = r"^[A-Za-z0-9_-]+$";
pub(super) const WORKFLOW_ID_MAX_LENGTH: usize = 64;
const NONEMPTY_NO_NUL_PATTERN: &str = r"^[^\u0000]*[^\s\u0000][^\u0000]*$";
// JavaScript's UTF-16 `string.length` does not implement JSON Schema's Unicode-scalar
// `maxLength` semantics for astral characters. A printable-ASCII provider label keeps every
// generated boundary and terminal acceptance-equivalent without narrowing general ProcessLabel.
const DISPLAY_LABEL_PATTERN: &str = r"^[ -~]*[!-~][ -~]*$";
const NO_NUL_PATTERN: &str = r"^[^\u0000]*$";

#[derive(Clone, Debug)]
pub struct LoadedBuildManifest {
    pub path: PathBuf,
    pub provider: ProviderCommand,
    workflows: Vec<Workflow>,
}

#[derive(Clone, Debug)]
pub struct ProviderCommand {
    pub program: OsString,
    pub arguments: Vec<OsString>,
}

#[derive(Clone, Debug)]
pub struct Workflow {
    pub id: String,
    pub label: ProcessLabel,
    platforms: Vec<HostPlatform>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
enum HostPlatform {
    Linux,
    Macos,
    Windows,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BuildManifestDocument {
    #[schemars(extend("const" = BUILD_MANIFEST_VERSION))]
    version: u32,
    provider: ProviderDocument,
    #[schemars(length(min = 1))]
    workflows: Vec<WorkflowDocument>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProviderDocument {
    #[schemars(length(min = 1), pattern(NONEMPTY_NO_NUL_PATTERN))]
    program: String,
    #[serde(default)]
    #[schemars(inner(pattern(NO_NUL_PATTERN)))]
    args: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WorkflowDocument {
    #[schemars(length(min = 1, max = WORKFLOW_ID_MAX_LENGTH), pattern(WORKFLOW_ID_PATTERN))]
    id: String,
    #[schemars(length(min = 1, max = 128), pattern(DISPLAY_LABEL_PATTERN))]
    label: String,
    #[schemars(length(min = 1))]
    platforms: Vec<HostPlatform>,
}

impl LoadedBuildManifest {
    pub fn load(root: &WorktreeRoot, start: &Path) -> Result<Self> {
        let start = start
            .canonicalize()
            .with_context(|| format!("canonicalize build start directory {}", start.display()))?;
        if !start.starts_with(root.as_path()) {
            bail!(
                "build start directory {} is outside worktree {}",
                start.display(),
                root.as_path().display()
            );
        }
        let path = nearest_manifest(root, &start)?;
        if !path.starts_with(root.as_path()) {
            bail!("build manifest resolves outside the canonical worktree root");
        }
        let raw = read_build_manifest(&path)?;
        let document = toml::from_str::<BuildManifestDocument>(&raw)
            .with_context(|| format!("parse build manifest {}", path.display()))?;
        if document.version != BUILD_MANIFEST_VERSION {
            bail!(
                "build manifest {} uses version {}; expected {BUILD_MANIFEST_VERSION}",
                path.display(),
                document.version
            );
        }
        let provider = resolve_provider(&document.provider, root.as_path())?;
        let workflows = validate_workflows(document.workflows, &path)?;

        Ok(Self { path, provider, workflows })
    }

    pub fn workflow(&self, id: &str) -> Result<&Workflow> {
        self.workflows.iter().find(|workflow| workflow.id == id).ok_or_else(|| {
            let available = self
                .workflows
                .iter()
                .map(|workflow| workflow.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!(
                "build workflow '{id}' is not declared in {}; available workflows: {available}",
                self.path.display()
            )
        })
    }

    pub(super) fn workflows(&self) -> &[Workflow] {
        &self.workflows
    }
}

fn read_build_manifest(path: &Path) -> Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file =
        options.open(path).with_context(|| format!("open build manifest {}", path.display()))?;
    let metadata =
        file.metadata().with_context(|| format!("inspect build manifest {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("build manifest must be a regular file");
    }
    let read_limit =
        MAX_BUILD_MANIFEST_BYTES.checked_add(1).context("build manifest byte limit overflow")?;
    let mut bytes = Vec::new();
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read build manifest {}", path.display()))?;
    if u64::try_from(bytes.len()).context("count build manifest bytes")? > MAX_BUILD_MANIFEST_BYTES
    {
        bail!("build manifest exceeds the {MAX_BUILD_MANIFEST_BYTES}-byte limit");
    }
    String::from_utf8(bytes)
        .with_context(|| format!("build manifest {} is not valid UTF-8", path.display()))
}

impl Workflow {
    pub(super) fn supports_current_platform(&self) -> bool {
        self.platforms.contains(&current_platform())
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn label(&self) -> &str {
        self.label.as_str()
    }
}

fn nearest_manifest(root: &WorktreeRoot, start: &Path) -> Result<PathBuf> {
    for directory in start.ancestors() {
        if !directory.starts_with(root.as_path()) {
            break;
        }
        let candidate = directory.join(BUILD_MANIFEST_RELATIVE_PATH);
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => {
                return candidate.canonicalize().with_context(|| {
                    format!("canonicalize nearest build manifest {}", candidate.display())
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspect nearest build manifest boundary {}", candidate.display())
                });
            }
        }
        if directory == root.as_path() {
            break;
        }
    }
    bail!(
        "no build manifest found between {} and {}; create {}",
        start.display(),
        root.as_path().display(),
        BUILD_MANIFEST_RELATIVE_PATH
    )
}

fn resolve_provider(provider: &ProviderDocument, root: &Path) -> Result<ProviderCommand> {
    if provider.program.trim().is_empty() {
        bail!("build manifest provider.program must not be empty");
    }
    if provider.program.contains('\0')
        || provider.args.iter().any(|argument| argument.contains('\0'))
    {
        bail!("build manifest provider command must not contain a NUL byte");
    }

    let program_path = Path::new(&provider.program);
    let program = if program_path.components().count() == 1
        && matches!(program_path.components().next(), Some(Component::Normal(_)))
    {
        OsString::from(&provider.program)
    } else {
        if program_path.is_absolute()
            || program_path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            bail!(
                "build manifest provider.program must be a bare executable name or a repository-relative path"
            );
        }
        let resolved = root.join(program_path).canonicalize().with_context(|| {
            format!("resolve repository-relative build provider {}", provider.program)
        })?;
        if !resolved.starts_with(root) {
            bail!("build manifest provider.program escapes the canonical worktree root");
        }
        if !resolved.is_file() {
            bail!("build manifest provider.program must resolve to a regular file");
        }
        resolved.into_os_string()
    };

    Ok(ProviderCommand { program, arguments: provider.args.iter().map(OsString::from).collect() })
}

fn validate_workflows(documents: Vec<WorkflowDocument>, path: &Path) -> Result<Vec<Workflow>> {
    if documents.is_empty() {
        bail!("build manifest {} must declare at least one workflow", path.display());
    }
    let mut ids = HashSet::new();
    let mut workflows = Vec::with_capacity(documents.len());
    for document in documents {
        validate_workflow_id(&document.id)?;
        if !ids.insert(document.id.clone()) {
            bail!(
                "build manifest {} declares workflow '{}' more than once",
                path.display(),
                document.id
            );
        }
        let label = validate_workflow_label(&document.label)
            .with_context(|| format!("validate build workflow '{}' label", document.id))?;
        if document.platforms.is_empty() {
            bail!("build workflow '{}' must declare at least one platform", document.id);
        }
        workflows.push(Workflow { id: document.id, label, platforms: document.platforms });
    }
    Ok(workflows)
}

pub(super) fn validate_workflow_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > WORKFLOW_ID_MAX_LENGTH
        || !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!(
            "build workflow id '{value}' must contain 1 to {WORKFLOW_ID_MAX_LENGTH} letters, numbers, '-' or '_'"
        );
    }
    Ok(())
}

pub(super) fn validate_workflow_label(value: &str) -> Result<ProcessLabel> {
    if value.trim().is_empty() {
        bail!("build workflow label must not be empty");
    }
    if !value.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
        bail!("build workflow label must use printable ASCII characters");
    }
    ProcessLabel::new(value.to_owned()).context("validate build workflow display label")
}

#[cfg(target_os = "linux")]
fn current_platform() -> HostPlatform {
    HostPlatform::Linux
}

#[cfg(target_os = "macos")]
fn current_platform() -> HostPlatform {
    HostPlatform::Macos
}

#[cfg(target_os = "windows")]
fn current_platform() -> HostPlatform {
    HostPlatform::Windows
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("kit build has no declared host-platform mapping for this target");
