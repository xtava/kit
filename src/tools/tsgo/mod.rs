//! Warm, workspace-scoped native TypeScript language-service queries.

mod diagnostics;
mod graph;
mod locus;
mod protocol;
mod render;
mod service;

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io::Read as _,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{anyhow, bail, Context as _, Result};
use async_trait::async_trait;
use clap::{
    ArgMatches, Args, Command, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
};
use directories::ProjectDirs;
use serde::{de::DeserializeOwned, Deserialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::{sleep, timeout},
};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};

use crate::framework::process::{
    CaptureDisposition, CaptureOverflow, CapturePolicy, CommandSpec, CompletionCause,
    ContainmentRequirement, DetachedLifetimeRequirement, DetachedOutputPolicy,
    DetachedProcessReceipt, DetachedProcessSpec, DetachedProcessStatus, EnvironmentBase,
    InputPolicy, LeaderExit, LeaderExitObservation, OutputPolicy, OutputReport, ProcessDeadline,
    ProcessEnvironment, ProcessLabel, ProcessSpec, ProcessSupervisor, TerminationPolicy,
};
use crate::framework::{AtomicFileWriter, Context, RepositoryLocator, Tool, ToolMeta};

use protocol::{
    CheckCoverage, CheckEntryConfigFreshness, CheckIncompleteReason, CheckInputFreshness,
    CheckOutput, CheckProject, CheckResult, CheckTiming, CheckVerdict, CompilerExit,
    CompilerInvocation, CompilerOutputEvidence, DiagnoseOutput, DiagnoseRequest, DiagnoseResult,
    DiagnosticCommandFailure, InspectEntry, LocusOutput, LocusRequest, LocusResult,
    ManagementOutput, RegistryRecord, ServiceCommand, ServiceIdentity, ServiceInfo, ServiceReply,
    ServiceRequest, TraceDirection, TraceLimits, TraceOutput, TraceResult, TraceScope,
    TraceSelector, DIAGNOSTIC_SCHEMA_VERSION, REGISTRY_SCHEMA, SERVICE_PROTOCOL_VERSION,
};

const VERSION_CAPTURE: NonZeroUsize = NonZeroUsize::new(64 * 1024).unwrap();
const DETACHED_GRACE: Duration = Duration::from_secs(5);
const READY_ATTEMPTS: usize = 200;
const READY_INTERVAL: Duration = Duration::from_millis(50);
const REGISTRY_FILE_LIMIT: u64 = 64 * 1024;
const PACKAGE_MANIFEST_LIMIT: u64 = 256 * 1024;
const SOCKET_RESPONSE_LIMIT: u64 = 16 * 1024 * 1024;
const LOCUS_CASE_INPUT_LIMIT: u64 = 512 * 1024;
const CHECK_OUTPUT_CAPTURE: NonZeroUsize = NonZeroUsize::new(8 * 1024 * 1024).unwrap();
const CHECK_DEADLINE: Duration = Duration::from_secs(120);

pub fn tool() -> TsgoTool {
    TsgoTool
}

pub struct TsgoTool;

#[derive(Parser)]
#[command(
    name = "tsgo",
    about = "Warm native TypeScript semantic-query service",
    long_about = "Runs one reusable native tsgo language server per canonical workspace and exact server version. Diagnose returns fast document-scoped evidence; check runs an authoritative project compiler; trace and locus expose bounded semantic evidence."
)]
struct TsgoArgs {
    #[command(subcommand)]
    command: TsgoCommand,
}

#[derive(Subcommand)]
enum TsgoCommand {
    /// Trace callers or callees of one semantic TypeScript symbol.
    Trace(TraceArgs),
    /// Capture a replayable, bounded placement-evidence case.
    Locus(LocusArgs),
    /// Get fast, warm diagnostics for explicit documents without claiming project completeness.
    Diagnose(DiagnoseArgs),
    /// Run one authoritative, emission-free native TypeScript project check.
    Check(CheckArgs),
    /// Inspect live and stale service records without starting a service.
    Inspect {
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long, conflicts_with = "workspace")]
        all: bool,
    },
    /// Gracefully shut down and reap an owned workspace service.
    Stop {
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long, conflicts_with = "workspace")]
        all: bool,
    },
    /// Remove stale owned service state; live services are retained.
    Prune {
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long, conflicts_with = "workspace")]
        all: bool,
    },
    /// Internal detached service entry.
    #[command(name = "__serve", hide = true)]
    Serve {
        #[arg(long)]
        key: String,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        launcher: PathBuf,
        #[arg(long)]
        server_version: String,
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        token: String,
    },
}

#[derive(Args)]
struct TraceArgs {
    /// Exact semantic symbol name, optionally qualified by its container.
    #[arg(long, conflicts_with = "at")]
    symbol: Option<String>,
    /// Source file containing the target symbol.
    #[arg(long, conflicts_with = "symbol")]
    at: Option<PathBuf>,
    /// Zero-based UTF-16 line; required with --at.
    #[arg(long, requires = "at")]
    line: Option<u32>,
    /// Zero-based UTF-16 character; required with --at.
    #[arg(long, requires = "at")]
    character: Option<u32>,
    /// Restrict semantic name resolution to this workspace subpath.
    #[arg(long = "in", requires = "symbol")]
    in_path: Option<PathBuf>,
    /// Follow callers toward entry points, or callees away from the target.
    #[arg(long, value_enum, default_value = "callers")]
    direction: CliTraceDirection,
    /// Maximum number of call edges followed from the target.
    #[arg(long, default_value_t = 12)]
    max_depth: u32,
    /// Maximum number of unique semantic nodes returned.
    #[arg(long, default_value_t = 512)]
    max_nodes: usize,
    /// Expand only within these workspace source roots; repeat for multiple roots.
    #[arg(long = "within", value_name = "PATH")]
    source_roots: Vec<PathBuf>,
    /// Stop expansion when a relation first crosses the target's nearest package.json boundary.
    #[arg(long)]
    stop_at_package_boundary: bool,
    /// Resolve the canonical Git worktree from this path.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Exact tsgo launcher. Defaults to <workspace>/node_modules/.bin/tsgo.
    #[arg(long)]
    tsgo: Option<PathBuf>,
}

#[derive(Args)]
struct LocusArgs {
    /// Read one locus case JSON object from this path; use '-' for stdin.
    #[arg(long, value_name = "PATH")]
    case: PathBuf,
    /// Resolve the canonical Git worktree from this path.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Exact tsgo launcher. Defaults to <workspace>/node_modules/.bin/tsgo.
    #[arg(long)]
    tsgo: Option<PathBuf>,
}

#[derive(Args)]
struct DiagnoseArgs {
    /// JavaScript or TypeScript source files to diagnose as one synchronized set.
    #[arg(required = true, value_name = "FILE")]
    files: Vec<PathBuf>,
    /// Resolve the canonical Git worktree from this path.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Exact tsgo launcher. Defaults to <workspace>/node_modules/.bin/tsgo.
    #[arg(long)]
    tsgo: Option<PathBuf>,
}

#[derive(Args)]
struct CheckArgs {
    /// TypeScript project configuration. Defaults to the nearest tsconfig.json.
    #[arg(short = 'p', long, value_name = "CONFIG")]
    project: Option<PathBuf>,
    /// Resolve the canonical Git worktree from this path.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Exact tsgo launcher. Defaults to <workspace>/node_modules/.bin/tsgo.
    #[arg(long)]
    tsgo: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum CliTraceDirection {
    Callers,
    Callees,
}

impl From<CliTraceDirection> for TraceDirection {
    fn from(value: CliTraceDirection) -> Self {
        match value {
            CliTraceDirection::Callers => Self::Callers,
            CliTraceDirection::Callees => Self::Callees,
        }
    }
}

#[async_trait]
impl Tool for TsgoTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "tsgo",
            about: "Warm native TypeScript semantic-query service",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        TsgoArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = TsgoArgs::from_arg_matches(matches)?;
        match args.command {
            TsgoCommand::Trace(args) => {
                let (identity, selector) = resolve_trace_identity(
                    &cx.repositories,
                    &cx.processes,
                    args.workspace.as_deref(),
                    args.tsgo.as_deref(),
                    args.symbol,
                    args.at.as_deref(),
                    args.line,
                    args.character,
                    args.in_path.as_deref(),
                )
                .await?;
                let direction = args.direction.into();
                let limits = TraceLimits { max_depth: args.max_depth, max_nodes: args.max_nodes };
                let scope = resolve_trace_scope(
                    &identity.workspace,
                    args.source_roots,
                    args.stop_at_package_boundary,
                )?;
                let (service, result) = execute_query::<TraceResult>(
                    &cx.processes,
                    &identity,
                    ServiceCommand::Trace { selector, direction, limits, scope },
                    "trace",
                )
                .await?;
                render_trace(cx, service, result)
            }
            TsgoCommand::Locus(args) => {
                let input = read_locus_case(&args.case)?;
                let mut request: LocusRequest =
                    serde_json::from_slice(&input).with_context(|| {
                        format!("decode locus case JSON from {}", args.case.display())
                    })?;
                locus::validate_request(&request)?;
                let workspace =
                    resolve_locus_workspace(&cx.repositories, args.workspace.as_deref())?;
                normalize_locus_request(&mut request, &workspace)?;
                locus::validate_request(&request)?;
                let identity = resolve_locus_identity(
                    &cx.repositories,
                    &cx.processes,
                    args.workspace.as_deref(),
                    args.tsgo.as_deref(),
                )
                .await?;
                if identity.workspace != workspace {
                    bail!("locus workspace resolution changed during service identity acquisition");
                }
                let (service, result) = execute_query::<LocusResult>(
                    &cx.processes,
                    &identity,
                    ServiceCommand::Locus { request },
                    "locus",
                )
                .await?;
                render_locus(cx, service, result)
            }
            TsgoCommand::Diagnose(args) => {
                let outcome: Result<(ServiceInfo, DiagnoseResult)> = async {
                    let current = std::env::current_dir().context("resolve current directory")?;
                    let inferred_start = args
                        .files
                        .first()
                        .and_then(|file| file.parent())
                        .map(|parent| {
                            if parent.is_absolute() {
                                parent.to_path_buf()
                            } else {
                                current.join(parent)
                            }
                        })
                        .unwrap_or_else(|| current.clone());
                    let identity = resolve_query_identity(
                        &cx.repositories,
                        &cx.processes,
                        args.workspace.as_deref(),
                        args.tsgo.as_deref(),
                        &inferred_start,
                        &current,
                    )
                    .await?;
                    let files =
                        normalize_diagnose_files(&identity.workspace, &current, args.files)?;
                    execute_query::<DiagnoseResult>(
                        &cx.processes,
                        &identity,
                        ServiceCommand::Diagnose { request: DiagnoseRequest { files } },
                        "diagnose",
                    )
                    .await
                }
                .await;
                match outcome {
                    Ok((service, result)) => render_diagnose(cx, service, result),
                    Err(error) => render_diagnose_failure(cx, error),
                }
            }
            TsgoCommand::Check(args) => match run_project_check(cx, args).await {
                Ok(result) => render_check(cx, result),
                Err(error) => render_check_failure(cx, error),
            },
            TsgoCommand::Inspect { workspace, all } => {
                let workspace = management_workspace(&cx.repositories, workspace.as_deref(), all)?;
                let output = inspect_services(&cx.processes, workspace.as_deref()).await?;
                render_management(cx, output)
            }
            TsgoCommand::Stop { workspace, all } => {
                let workspace = management_workspace(&cx.repositories, workspace.as_deref(), all)?;
                let output = stop_services(&cx.processes, workspace.as_deref()).await?;
                render_management(cx, output)
            }
            TsgoCommand::Prune { workspace, all } => {
                let workspace = management_workspace(&cx.repositories, workspace.as_deref(), all)?;
                let output = prune_services(&cx.processes, workspace.as_deref(), all).await?;
                render_management(cx, output)
            }
            TsgoCommand::Serve { key, workspace, launcher, server_version, socket, token } => {
                let identity = ServiceIdentity { key, workspace, launcher, server_version };
                validate_identity(&identity)?;
                service::serve(&cx.processes, identity, socket, token).await
            }
        }
    }
}

fn read_locus_case(path: &Path) -> Result<Vec<u8>> {
    let mut input = Vec::new();
    if path == Path::new("-") {
        std::io::stdin()
            .take(LOCUS_CASE_INPUT_LIMIT + 1)
            .read_to_end(&mut input)
            .context("read locus case from stdin")?;
    } else {
        std::fs::File::open(path)
            .with_context(|| format!("open locus case {}", path.display()))?
            .take(LOCUS_CASE_INPUT_LIMIT + 1)
            .read_to_end(&mut input)
            .with_context(|| format!("read locus case {}", path.display()))?;
    }
    if input.len() as u64 > LOCUS_CASE_INPUT_LIMIT {
        bail!("locus case exceeds 512 KiB");
    }
    Ok(input)
}

fn normalize_locus_request(request: &mut LocusRequest, workspace: &Path) -> Result<()> {
    for seed in &mut request.seeds {
        match &mut seed.selector {
            TraceSelector::Position { file, .. } => {
                let canonical = canonical_locus_file(workspace, file)?;
                *file = canonical
                    .strip_prefix(workspace)
                    .expect("canonical locus file is inside workspace")
                    .to_path_buf();
            }
            TraceSelector::Symbol { query, scope } => {
                *query = query.trim().to_owned();
                if let Some(path) = scope {
                    *path = canonical_locus_path(workspace, path, "symbol scope")?;
                }
            }
        }
    }
    for candidate in &mut request.supplied_candidates {
        let canonical = canonical_locus_file(workspace, &candidate.position.file)?;
        candidate.position.file = canonical
            .strip_prefix(workspace)
            .expect("canonical locus candidate is inside workspace")
            .to_path_buf();
    }
    Ok(())
}

fn canonical_locus_path(workspace: &Path, path: &Path, kind: &str) -> Result<PathBuf> {
    let unresolved = if path.is_absolute() { path.to_path_buf() } else { workspace.join(path) };
    let canonical = unresolved
        .canonicalize()
        .with_context(|| format!("canonicalize locus {kind} {}", unresolved.display()))?;
    if !canonical.starts_with(workspace) {
        bail!("locus {kind} {} is outside workspace {}", canonical.display(), workspace.display());
    }
    Ok(canonical)
}

pub(super) fn canonical_locus_file(workspace: &Path, file: &Path) -> Result<PathBuf> {
    let unresolved = if file.is_absolute() { file.to_path_buf() } else { workspace.join(file) };
    canonical_source_file(workspace, &unresolved, "locus")
}

fn normalize_diagnose_files(
    workspace: &Path,
    current: &Path,
    files: Vec<PathBuf>,
) -> Result<Vec<PathBuf>> {
    if files.len() > protocol::MAX_DIAGNOSE_FILES {
        bail!("diagnose accepts at most {} explicit files", protocol::MAX_DIAGNOSE_FILES);
    }
    let mut normalized = BTreeSet::new();
    for file in files {
        let unresolved = if file.is_absolute() { file } else { current.join(file) };
        let canonical = canonical_source_file(workspace, &unresolved, "diagnose")?;
        normalized.insert(
            canonical
                .strip_prefix(workspace)
                .expect("canonical diagnose source is inside workspace")
                .to_path_buf(),
        );
    }
    Ok(normalized.into_iter().collect())
}

fn canonical_source_file(workspace: &Path, file: &Path, operation: &str) -> Result<PathBuf> {
    let canonical = file
        .canonicalize()
        .with_context(|| format!("canonicalize {operation} source file {}", file.display()))?;
    if !canonical.starts_with(workspace) {
        bail!(
            "{operation} source {} is outside workspace {}",
            canonical.display(),
            workspace.display()
        );
    }
    let metadata = std::fs::metadata(&canonical)
        .with_context(|| format!("inspect {operation} source file {}", canonical.display()))?;
    if !metadata.is_file() {
        bail!("{operation} source is not a regular file: {}", canonical.display());
    }
    if !matches!(
        canonical.extension().and_then(|extension| extension.to_str()),
        Some("ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs")
    ) {
        bail!("{operation} source is not JavaScript or TypeScript: {}", canonical.display());
    }
    Ok(canonical)
}

fn resolve_trace_scope(
    workspace: &Path,
    source_roots: Vec<PathBuf>,
    stop_at_package_boundary: bool,
) -> Result<TraceScope> {
    let mut canonical_roots = source_roots
        .into_iter()
        .map(|root| {
            let unresolved = if root.is_absolute() { root } else { workspace.join(root) };
            let canonical = unresolved.canonicalize().with_context(|| {
                format!("canonicalize trace source root {}", unresolved.display())
            })?;
            if !canonical.starts_with(workspace) {
                bail!(
                    "trace source root {} is outside workspace {}",
                    canonical.display(),
                    workspace.display()
                );
            }
            if !canonical.is_dir() {
                bail!("trace source root is not a directory: {}", canonical.display());
            }
            Ok(canonical)
        })
        .collect::<Result<Vec<_>>>()?;
    canonical_roots.sort();
    canonical_roots.dedup();
    Ok(TraceScope { source_roots: canonical_roots, stop_at_package_boundary })
}

#[allow(clippy::too_many_arguments)]
async fn resolve_trace_identity(
    repositories: &RepositoryLocator,
    processes: &ProcessSupervisor,
    workspace: Option<&Path>,
    launcher: Option<&Path>,
    symbol: Option<String>,
    at: Option<&Path>,
    line: Option<u32>,
    character: Option<u32>,
    in_path: Option<&Path>,
) -> Result<(ServiceIdentity, TraceSelector)> {
    let current = std::env::current_dir().context("resolve current directory")?;
    let unresolved_file =
        at.map(|file| if file.is_absolute() { file.to_path_buf() } else { current.join(file) });
    let file = unresolved_file
        .as_ref()
        .map(|file| {
            file.canonicalize()
                .with_context(|| format!("canonicalize TypeScript file {}", file.display()))
        })
        .transpose()?;
    let inferred_start = file
        .as_ref()
        .and_then(|file| file.parent())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| current.clone());
    let identity = resolve_query_identity(
        repositories,
        processes,
        workspace,
        launcher,
        &inferred_start,
        &current,
    )
    .await?;
    let workspace = &identity.workspace;
    let selector = match (symbol, file) {
        (Some(query), None) => {
            let query = query.trim().to_owned();
            if query.is_empty() {
                bail!("--symbol must not be empty");
            }
            if line.is_some() || character.is_some() {
                bail!("--line and --character apply only to --at");
            }
            let scope = in_path
                .map(|scope| {
                    let unresolved = if scope.is_absolute() {
                        scope.to_path_buf()
                    } else {
                        workspace.join(scope)
                    };
                    unresolved.canonicalize().with_context(|| {
                        format!("canonicalize trace scope {}", unresolved.display())
                    })
                })
                .transpose()?;
            if let Some(scope) = &scope {
                if !scope.starts_with(workspace) {
                    bail!(
                        "trace scope {} is outside workspace {}",
                        scope.display(),
                        workspace.display()
                    );
                }
            }
            TraceSelector::Symbol { query, scope }
        }
        (None, Some(file)) => {
            if !file.starts_with(workspace) {
                bail!("{} is outside workspace {}", file.display(), workspace.display());
            }
            let line = line.context("--line is required with --at")?;
            let character = character.context("--character is required with --at")?;
            if in_path.is_some() {
                bail!("--in applies only to --symbol");
            }
            TraceSelector::Position { file, line, character }
        }
        (Some(_), Some(_)) => bail!("use exactly one of --symbol or --at"),
        (None, None) => bail!("use exactly one of --symbol or --at"),
    };
    Ok((identity, selector))
}

async fn resolve_locus_identity(
    repositories: &RepositoryLocator,
    processes: &ProcessSupervisor,
    workspace: Option<&Path>,
    launcher: Option<&Path>,
) -> Result<ServiceIdentity> {
    let current = std::env::current_dir().context("resolve current directory")?;
    resolve_query_identity(repositories, processes, workspace, launcher, &current, &current).await
}

fn resolve_locus_workspace(
    repositories: &RepositoryLocator,
    workspace: Option<&Path>,
) -> Result<PathBuf> {
    let current = std::env::current_dir().context("resolve current directory")?;
    let unresolved = workspace.unwrap_or(&current);
    let start =
        if unresolved.is_absolute() { unresolved.to_path_buf() } else { current.join(unresolved) };
    Ok(repositories.nearest_worktree_root(&start)?.as_path().to_path_buf())
}

async fn resolve_service_identity(
    repositories: &RepositoryLocator,
    processes: &ProcessSupervisor,
    workspace_override: Option<&Path>,
    launcher: Option<&Path>,
    inferred_start: &Path,
    current: &Path,
) -> Result<ServiceIdentity> {
    let locator = resolve_service_locator(
        repositories,
        workspace_override,
        launcher,
        inferred_start,
        current,
    )?;
    resolve_current_service_identity(processes, locator).await
}

async fn resolve_query_identity(
    repositories: &RepositoryLocator,
    processes: &ProcessSupervisor,
    workspace_override: Option<&Path>,
    launcher: Option<&Path>,
    inferred_start: &Path,
    current: &Path,
) -> Result<ServiceIdentity> {
    let locator = resolve_service_locator(
        repositories,
        workspace_override,
        launcher,
        inferred_start,
        current,
    )?;
    if let Some(identity) = resolve_unique_live_identity(&locator).await? {
        return Ok(identity);
    }
    resolve_current_service_identity(processes, locator).await
}

struct ServiceLocator {
    workspace: PathBuf,
    launcher: PathBuf,
}

#[derive(Deserialize)]
struct LauncherPackageManifest {
    version: String,
    bin: LauncherPackageBins,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LauncherPackageBins {
    Single(PathBuf),
    Named(BTreeMap<String, PathBuf>),
}

impl LauncherPackageBins {
    fn binds(&self, package: &Path, launcher: &Path) -> bool {
        let binds =
            |target: &Path| package.join(target).canonicalize().ok().as_deref() == Some(launcher);
        match self {
            Self::Single(target) => binds(target),
            Self::Named(targets) => targets.values().any(|target| binds(target)),
        }
    }
}

fn resolve_service_locator(
    repositories: &RepositoryLocator,
    workspace_override: Option<&Path>,
    launcher: Option<&Path>,
    inferred_start: &Path,
    current: &Path,
) -> Result<ServiceLocator> {
    let unresolved_start = workspace_override.unwrap_or(inferred_start);
    let start = if unresolved_start.is_absolute() {
        unresolved_start.to_path_buf()
    } else {
        current.join(unresolved_start)
    };
    let workspace = repositories.nearest_worktree_root(&start)?.as_path().to_path_buf();
    let unresolved_launcher = match launcher {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => current.join(path),
        None => workspace.join("node_modules/.bin/tsgo"),
    };
    let launcher = unresolved_launcher
        .canonicalize()
        .with_context(|| format!("canonicalize tsgo launcher {}", unresolved_launcher.display()))?;
    let metadata = launcher
        .metadata()
        .with_context(|| format!("inspect tsgo launcher {}", launcher.display()))?;
    if !metadata.is_file() {
        bail!("tsgo launcher is not a file: {}", launcher.display());
    }
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o111 == 0 {
        bail!("tsgo launcher is not executable: {}", launcher.display());
    }
    Ok(ServiceLocator { workspace, launcher })
}

async fn resolve_unique_live_identity(locator: &ServiceLocator) -> Result<Option<ServiceIdentity>> {
    let Some(server_version) = resolve_launcher_manifest_version(locator) else {
        return Ok(None);
    };
    let candidates = records()?.into_iter().filter(|record| {
        record.identity.workspace == locator.workspace
            && record.identity.launcher == locator.launcher
            && record.identity.server_version == server_version
    });
    let mut live = Vec::new();
    for record in candidates {
        if let Ok(reply) = send_service(&record, ServiceCommand::Ping).await {
            if service_reply_compatible(&record.identity, &reply) {
                live.push(record.identity);
            }
        }
    }
    Ok(if live.len() == 1 { live.pop() } else { None })
}

fn resolve_launcher_manifest_version(locator: &ServiceLocator) -> Option<String> {
    if !locator.launcher.starts_with(&locator.workspace) {
        return None;
    }
    let mut directory = locator.launcher.parent();
    while let Some(candidate) = directory.filter(|path| path.starts_with(&locator.workspace)) {
        let manifest_path = candidate.join("package.json");
        let manifest = std::fs::metadata(&manifest_path)
            .ok()
            .filter(|metadata| metadata.is_file() && metadata.len() <= PACKAGE_MANIFEST_LIMIT)
            .and_then(|_| std::fs::read(&manifest_path).ok())
            .and_then(|bytes| serde_json::from_slice::<LauncherPackageManifest>(&bytes).ok());
        if let Some(manifest) = manifest {
            let binds_launcher = manifest.bin.binds(candidate, &locator.launcher);
            let valid_version =
                !manifest.version.is_empty() && !manifest.version.chars().any(char::is_whitespace);
            if binds_launcher && valid_version {
                return Some(manifest.version);
            }
        }
        if candidate == locator.workspace {
            break;
        }
        directory = candidate.parent();
    }
    None
}

async fn resolve_current_service_identity(
    processes: &ProcessSupervisor,
    locator: ServiceLocator,
) -> Result<ServiceIdentity> {
    let server_version =
        resolve_server_version(processes, &locator.workspace, &locator.launcher).await?;
    let key = identity_key(&locator.workspace, &locator.launcher, &server_version);
    Ok(ServiceIdentity {
        key,
        workspace: locator.workspace,
        launcher: locator.launcher,
        server_version,
    })
}

async fn resolve_server_version(
    processes: &ProcessSupervisor,
    workspace: &Path,
    launcher: &Path,
) -> Result<String> {
    let environment =
        ProcessEnvironment::new(EnvironmentBase::Inherit, BTreeMap::new(), BTreeSet::new())?;
    let command = CommandSpec::new(
        launcher.as_os_str().to_owned(),
        vec![OsString::from("--version")],
        workspace.to_path_buf(),
        environment,
        ProcessLabel::new("resolve native tsgo version".to_owned())?,
    )?;
    let spec = ProcessSpec::new(
        command,
        InputPolicy::Closed,
        OutputPolicy::Capture(CapturePolicy::new(
            VERSION_CAPTURE,
            CaptureOverflow::FailAndTerminate,
        )),
        OutputPolicy::Capture(CapturePolicy::new(
            VERSION_CAPTURE,
            CaptureOverflow::FailAndTerminate,
        )),
        ContainmentRequirement::ExplicitProcessGroup,
        ProcessDeadline::After(Duration::from_secs(10)),
        TerminationPolicy::new(Duration::from_secs(2)),
    );
    let started = processes.spawn(spec).await.context("run tsgo --version")?;
    let report = started.session.wait().await.map_err(|failure| {
        anyhow!("tsgo --version process failure {:?} (run {})", failure.failure, failure.run_id)
    })?;
    if report.leader_exit != LeaderExitObservation::Observed(LeaderExit::Code(0)) {
        bail!("tsgo --version exited with {:?}", report.leader_exit);
    }
    let OutputReport::Captured(stdout) = report.stdout else {
        bail!("tsgo --version stdout was not captured");
    };
    let raw = std::str::from_utf8(&stdout.bytes).context("tsgo --version emitted non-UTF8")?;
    let version = raw.trim().strip_prefix("Version ").unwrap_or(raw.trim()).trim();
    if version.is_empty() || version.chars().any(char::is_whitespace) {
        bail!("tsgo --version returned an invalid version: {version:?}");
    }
    Ok(version.to_owned())
}

fn identity_key(workspace: &Path, launcher: &Path, server_version: &str) -> String {
    let mut hash = Sha256::new();
    #[cfg(unix)]
    {
        hash.update(workspace.as_os_str().as_bytes());
        hash.update([0]);
        hash.update(launcher.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    {
        hash.update(workspace.to_string_lossy().as_bytes());
        hash.update([0]);
        hash.update(launcher.to_string_lossy().as_bytes());
    }
    hash.update([0]);
    hash.update(server_version.as_bytes());
    hash.finalize()[..16].iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_identity(identity: &ServiceIdentity) -> Result<()> {
    if identity.workspace.canonicalize().ok().as_deref() != Some(&identity.workspace)
        || identity.launcher.canonicalize().ok().as_deref() != Some(&identity.launcher)
    {
        bail!("tsgo service identity paths are not canonical");
    }
    let expected = identity_key(&identity.workspace, &identity.launcher, &identity.server_version);
    if identity.key != expected {
        bail!("tsgo service identity key does not match its workspace and server");
    }
    Ok(())
}

async fn run_project_check(cx: &Context, args: CheckArgs) -> Result<CheckResult> {
    let current = std::env::current_dir().context("resolve current directory")?;
    let project_hint = args.project.as_ref().map(|project| {
        let unresolved =
            if project.is_absolute() { project.clone() } else { current.join(project) };
        if unresolved.is_dir() {
            unresolved.join("tsconfig.json")
        } else {
            unresolved
        }
    });
    let inferred_start =
        project_hint.as_ref().and_then(|project| project.parent()).unwrap_or(&current);
    let identity = resolve_service_identity(
        &cx.repositories,
        &cx.processes,
        args.workspace.as_deref(),
        args.tsgo.as_deref(),
        inferred_start,
        &current,
    )
    .await?;
    let config = resolve_check_config(&identity.workspace, &current, project_hint.as_deref())?;
    let before_bytes = std::fs::read(&config)
        .with_context(|| format!("read TypeScript project config {}", config.display()))?;
    let before_sha256 = sha256_hex(&before_bytes);
    let public_config = config
        .strip_prefix(&identity.workspace)
        .expect("canonical check config is inside workspace")
        .to_path_buf();
    let effective = load_effective_check_config(&cx.processes, &identity, &config).await?;
    let write_options = effective.compiler_options.write_producing_options();
    if !write_options.is_empty() {
        bail!(
            "project config enables write-producing compiler options that check will not execute: {}",
            write_options.join(", ")
        );
    }
    let coverage = CheckCoverage {
        root_files: effective.files.len(),
        project_references: effective.references.len(),
    };

    let prepared = cx.processes.prepare().context("prepare private project-check run")?;
    let private = prepared.create_workspace().context("create private project-check workspace")?;
    let build_info = private.as_path().join("check.tsbuildinfo");
    let arguments = vec![
        "--noEmit".to_owned(),
        "--pretty".to_owned(),
        "false".to_owned(),
        "--locale".to_owned(),
        "en".to_owned(),
        "--noCheck".to_owned(),
        "false".to_owned(),
        "--incremental".to_owned(),
        "true".to_owned(),
        "--tsBuildInfoFile".to_owned(),
        build_info.to_string_lossy().into_owned(),
        "-p".to_owned(),
        config.to_string_lossy().into_owned(),
    ];
    let environment =
        ProcessEnvironment::new(EnvironmentBase::Inherit, BTreeMap::new(), BTreeSet::new())?;
    let command = CommandSpec::new(
        identity.launcher.clone().into_os_string(),
        arguments.iter().map(OsString::from).collect(),
        identity.workspace.clone(),
        environment,
        ProcessLabel::new("native TypeScript project check".to_owned())?,
    )?;
    let capture = OutputPolicy::Capture(CapturePolicy::new(
        CHECK_OUTPUT_CAPTURE,
        CaptureOverflow::TruncateWithEvidence,
    ));
    let spec = ProcessSpec::new(
        command,
        InputPolicy::Closed,
        capture,
        capture,
        ContainmentRequirement::CompleteTree,
        ProcessDeadline::After(CHECK_DEADLINE),
        TerminationPolicy::new(Duration::from_secs(3)),
    );
    let started = cx
        .processes
        .spawn_prepared(prepared, spec)
        .await
        .context("start native TypeScript project check")?;
    let report = started.session.wait().await.map_err(|failure| {
        anyhow!(
            "native TypeScript project check process failure {:?} (run {})",
            failure.failure,
            failure.run_id
        )
    })?;
    let stdout = captured_check_output(report.stdout, "stdout")?;
    let stderr = captured_check_output(report.stderr, "stderr")?;
    let stdout_text = String::from_utf8_lossy(&stdout.bytes).into_owned();
    let stderr_text = String::from_utf8_lossy(&stderr.bytes).into_owned();
    let parsed = diagnostics::parse_compiler_output(
        &stdout_text,
        &stderr_text,
        &identity.workspace,
        &public_config,
    )?;
    let exit = compiler_exit(report.leader_exit);
    let entry_config_freshness = recheck_config(&config, &before_sha256);
    let output_truncated = stdout.disposition == CaptureDisposition::Truncated
        || stderr.disposition == CaptureDisposition::Truncated;
    let mut incomplete = Vec::new();
    match report.completion {
        CompletionCause::DeadlineExceeded => {
            incomplete.push(CheckIncompleteReason::DeadlineExceeded)
        }
        CompletionCause::Cancelled | CompletionCause::OwnerDropped => {
            incomplete.push(CheckIncompleteReason::Cancelled)
        }
        CompletionCause::ExternalTermination => {
            incomplete.push(CheckIncompleteReason::ExternalTermination)
        }
        CompletionCause::Natural => {}
    }
    if !matches!(&entry_config_freshness, CheckEntryConfigFreshness::Verified) {
        incomplete.push(match &entry_config_freshness {
            CheckEntryConfigFreshness::Changed { .. } => CheckIncompleteReason::EntryConfigChanged,
            CheckEntryConfigFreshness::Unreadable { .. } => {
                CheckIncompleteReason::EntryConfigUnreadable
            }
            CheckEntryConfigFreshness::Verified => unreachable!(),
        });
    }
    if coverage.root_files == 0 {
        incomplete.push(CheckIncompleteReason::NoRootFiles);
    }
    if coverage.project_references > 0 {
        incomplete.push(CheckIncompleteReason::ProjectReferencesNotChecked {
            references: coverage.project_references,
        });
    }
    if output_truncated {
        incomplete.push(CheckIncompleteReason::OutputTruncated);
    }
    if parsed.normalized.summary.omitted > 0 {
        incomplete.push(CheckIncompleteReason::DiagnosticLimit);
    }
    if parsed.normalized.summary.truncated_details > 0 {
        incomplete.push(CheckIncompleteReason::DiagnosticDetailLimit);
    }
    let project_diagnostics = parsed
        .normalized
        .items
        .iter()
        .filter(|diagnostic| {
            matches!(diagnostic.location, protocol::DiagnosticLocation::Project { .. })
        })
        .count();
    if project_diagnostics > 0 {
        incomplete
            .push(CheckIncompleteReason::ProjectDiagnostic { diagnostics: project_diagnostics });
    }
    let code = match exit {
        CompilerExit::Code { code } => Some(code),
        CompilerExit::Signal { .. } | CompilerExit::NotObserved => None,
    };
    if !parsed.classified || (code != Some(0) && parsed.normalized.summary.total == 0) {
        incomplete.push(CheckIncompleteReason::UnclassifiedOutput);
    }
    if code == Some(0) && parsed.normalized.summary.total > 0 {
        incomplete.push(CheckIncompleteReason::InconsistentCompilerResult);
    }
    if !matches!(code, Some(0 | 2)) {
        incomplete.push(CheckIncompleteReason::UnexpectedExit);
    }
    incomplete.sort_by_key(|reason| format!("{reason:?}"));
    incomplete.dedup();
    let verdict = if !incomplete.is_empty() {
        CheckVerdict::Incomplete { reasons: incomplete }
    } else if code == Some(0) && parsed.normalized.summary.total == 0 {
        CheckVerdict::CompilerReportedNoDiagnostics
    } else {
        CheckVerdict::DiagnosticsPresent
    };
    let output = if output_truncated {
        CompilerOutputEvidence::Truncated {
            stdout: bounded_check_output(&stdout_text),
            stderr: bounded_check_output(&stderr_text),
            stdout_observed_bytes: stdout.observed_bytes,
            stderr_observed_bytes: stderr.observed_bytes,
        }
    } else if parsed.classified {
        CompilerOutputEvidence::Classified
    } else {
        CompilerOutputEvidence::Unclassified {
            stdout: bounded_check_output(&stdout_text),
            stderr: bounded_check_output(&stderr_text),
        }
    };
    Ok(CheckResult {
        schema: DIAGNOSTIC_SCHEMA_VERSION,
        authority: protocol::DiagnosticAuthority::Compiler,
        workspace: identity.workspace,
        project: CheckProject { config: public_config, entry_config_sha256: before_sha256 },
        coverage,
        invocation: CompilerInvocation {
            launcher: identity.launcher,
            server_version: identity.server_version,
            arguments,
        },
        verdict,
        diagnostics: parsed.normalized.items,
        summary: parsed.normalized.summary,
        output,
        exit,
        entry_config_freshness,
        input_freshness: CheckInputFreshness::Unchecked,
        timing: CheckTiming {
            elapsed_ms: report.elapsed.as_millis().try_into().unwrap_or(u64::MAX),
        },
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EffectiveCheckConfig {
    #[serde(default)]
    compiler_options: EffectiveCheckCompilerOptions,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    references: Vec<serde_json::Value>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EffectiveCheckCompilerOptions {
    generate_trace: Option<serde_json::Value>,
    generate_cpu_profile: Option<serde_json::Value>,
}

impl EffectiveCheckCompilerOptions {
    fn write_producing_options(&self) -> Vec<&'static str> {
        let mut options = Vec::new();
        if self.generate_trace.is_some() {
            options.push("generateTrace");
        }
        if self.generate_cpu_profile.is_some() {
            options.push("generateCpuProfile");
        }
        options
    }
}

async fn load_effective_check_config(
    processes: &ProcessSupervisor,
    identity: &ServiceIdentity,
    config: &Path,
) -> Result<EffectiveCheckConfig> {
    let arguments = vec![
        OsString::from("--showConfig"),
        OsString::from("--pretty"),
        OsString::from("false"),
        OsString::from("-p"),
        config.as_os_str().to_owned(),
    ];
    let environment =
        ProcessEnvironment::new(EnvironmentBase::Inherit, BTreeMap::new(), BTreeSet::new())?;
    let command = CommandSpec::new(
        identity.launcher.clone().into_os_string(),
        arguments,
        identity.workspace.clone(),
        environment,
        ProcessLabel::new("native TypeScript project preflight".to_owned())?,
    )?;
    let capture = OutputPolicy::Capture(CapturePolicy::new(
        CHECK_OUTPUT_CAPTURE,
        CaptureOverflow::TruncateWithEvidence,
    ));
    let report = processes
        .spawn(ProcessSpec::new(
            command,
            InputPolicy::Closed,
            capture,
            capture,
            ContainmentRequirement::CompleteTree,
            ProcessDeadline::After(CHECK_DEADLINE),
            TerminationPolicy::new(Duration::from_secs(3)),
        ))
        .await
        .context("start native TypeScript project preflight")?
        .session
        .wait()
        .await
        .map_err(|failure| {
            anyhow!(
                "native TypeScript project preflight process failure {:?} (run {})",
                failure.failure,
                failure.run_id
            )
        })?;
    let stdout = captured_check_output(report.stdout, "preflight stdout")?;
    let stderr = captured_check_output(report.stderr, "preflight stderr")?;
    let stdout_text = String::from_utf8_lossy(&stdout.bytes);
    let stderr_text = String::from_utf8_lossy(&stderr.bytes);
    if stdout.disposition == CaptureDisposition::Truncated
        || stderr.disposition == CaptureDisposition::Truncated
    {
        bail!("native TypeScript project preflight output exceeded its bound");
    }
    if report.completion != CompletionCause::Natural
        || !matches!(report.leader_exit, LeaderExitObservation::Observed(LeaderExit::Code(0)))
    {
        bail!(
            "native TypeScript project preflight failed: stdout={} stderr={}",
            bounded_check_output(&stdout_text),
            bounded_check_output(&stderr_text)
        );
    }
    if !stderr_text.trim().is_empty() {
        bail!(
            "native TypeScript project preflight emitted unexpected stderr: {}",
            bounded_check_output(&stderr_text)
        );
    }
    serde_json::from_slice(&stdout.bytes).with_context(|| {
        format!("decode effective TypeScript project config from {}", config.display())
    })
}

struct CheckCapturedOutput {
    bytes: Box<[u8]>,
    observed_bytes: u64,
    disposition: CaptureDisposition,
}

fn captured_check_output(output: OutputReport, stream: &str) -> Result<CheckCapturedOutput> {
    let OutputReport::Captured(capture) = output else {
        bail!("native TypeScript project check {stream} was not captured");
    };
    Ok(CheckCapturedOutput {
        bytes: capture.bytes,
        observed_bytes: capture.observed_bytes,
        disposition: capture.disposition,
    })
}

fn resolve_check_config(
    workspace: &Path,
    current: &Path,
    explicit: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        let config = explicit.canonicalize().with_context(|| {
            format!("canonicalize TypeScript project config {}", explicit.display())
        })?;
        return validate_check_config(workspace, config);
    }
    let mut directory = current
        .canonicalize()
        .with_context(|| format!("canonicalize check start {}", current.display()))?;
    if !directory.starts_with(workspace) {
        directory = workspace.to_path_buf();
    }
    loop {
        let candidate = directory.join("tsconfig.json");
        if candidate.is_file() {
            return validate_check_config(
                workspace,
                candidate.canonicalize().context("canonicalize nearest tsconfig.json")?,
            );
        }
        if directory == workspace || !directory.pop() {
            break;
        }
    }
    bail!(
        "no tsconfig.json exists between {} and workspace {}",
        current.display(),
        workspace.display()
    )
}

fn validate_check_config(workspace: &Path, config: PathBuf) -> Result<PathBuf> {
    if !config.starts_with(workspace) {
        bail!(
            "TypeScript project config {} is outside workspace {}",
            config.display(),
            workspace.display()
        );
    }
    if !config.is_file() {
        bail!("TypeScript project config is not a regular file: {}", config.display());
    }
    Ok(config)
}

fn recheck_config(config: &Path, before_sha256: &str) -> CheckEntryConfigFreshness {
    match std::fs::read(config) {
        Ok(bytes) => {
            let after_sha256 = sha256_hex(&bytes);
            if after_sha256 == before_sha256 {
                CheckEntryConfigFreshness::Verified
            } else {
                CheckEntryConfigFreshness::Changed {
                    before_sha256: before_sha256.to_owned(),
                    after_sha256,
                }
            }
        }
        Err(error) => CheckEntryConfigFreshness::Unreadable { detail: error.to_string() },
    }
}

fn compiler_exit(exit: LeaderExitObservation) -> CompilerExit {
    match exit {
        LeaderExitObservation::Observed(LeaderExit::Code(code)) => CompilerExit::Code { code },
        LeaderExitObservation::Observed(LeaderExit::Signal(signal)) => {
            CompilerExit::Signal { signal: signal.get() }
        }
        LeaderExitObservation::NotObserved => CompilerExit::NotObserved,
    }
}

fn bounded_check_output(value: &str) -> String {
    const LIMIT: usize = 64 * 1024;
    if value.len() <= LIMIT {
        return value.to_owned();
    }
    let mut end = LIMIT.saturating_sub('…'.len_utf8());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}…", &value[..end])
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

fn management_workspace(
    repositories: &RepositoryLocator,
    workspace: Option<&Path>,
    all: bool,
) -> Result<Option<PathBuf>> {
    if all {
        return Ok(None);
    }
    let start = match workspace {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("resolve current directory")?,
    };
    Ok(Some(repositories.nearest_worktree_root(&start)?.as_path().to_path_buf()))
}

async fn execute_query<T: DeserializeOwned>(
    processes: &ProcessSupervisor,
    identity: &ServiceIdentity,
    command: ServiceCommand,
    result_name: &str,
) -> Result<(ServiceInfo, T)> {
    let mut record = ensure_service(processes, identity).await?;
    for attempt in 0..2 {
        match send_service(&record, command.clone()).await {
            Ok(reply) if service_reply_compatible(identity, &reply) => {
                let service = reply.service.context("tsgo service reply omitted identity")?;
                let result = serde_json::from_value(
                    reply
                        .result
                        .with_context(|| format!("tsgo {result_name} reply omitted its result"))?,
                )
                .with_context(|| format!("decode typed tsgo {result_name} result"))?;
                return Ok((service, result));
            }
            Ok(reply) if reply.ok && attempt == 0 => {
                record = recover_service(processes, identity, &record, true).await?;
            }
            Ok(reply) if !reply.fatal => {
                bail!(
                    "{}",
                    reply.error.unwrap_or_else(|| format!("tsgo {result_name} query failed"))
                );
            }
            outcome if attempt == 0 => {
                let force = matches!(&outcome, Ok(reply) if reply.fatal);
                record = recover_service(processes, identity, &record, force).await?;
            }
            Ok(reply) => bail!(
                "{}",
                reply.error.unwrap_or_else(|| "tsgo service stopped during query".to_owned())
            ),
            Err(error) => return Err(error.context("query replacement tsgo service")),
        }
    }
    unreachable!()
}

fn service_reply_compatible(identity: &ServiceIdentity, reply: &ServiceReply) -> bool {
    reply.ok
        && reply.service.as_ref().is_some_and(|service| {
            service.protocol_version == SERVICE_PROTOCOL_VERSION
                && service.key == identity.key
                && service.workspace == identity.workspace
                && service.child.launcher == identity.launcher
                && service.child.server_version == identity.server_version
        })
}

async fn ensure_service(
    processes: &ProcessSupervisor,
    identity: &ServiceIdentity,
) -> Result<RegistryRecord> {
    ensure_runtime_dir()?;
    let writer = registry_writer(&identity.key)?;
    let _lock = writer.lock().context("lock tsgo service registry")?;
    if let Some(record) = read_record(&identity.key)? {
        if record.identity == *identity {
            if let Ok(reply) = send_service(&record, ServiceCommand::Ping).await {
                if service_reply_compatible(identity, &reply) {
                    return Ok(record);
                }
            }
        }
        cleanup_record(processes, &record).await?;
    }
    launch_service_locked(processes, identity, &writer).await
}

async fn recover_service(
    processes: &ProcessSupervisor,
    identity: &ServiceIdentity,
    failed: &RegistryRecord,
    force: bool,
) -> Result<RegistryRecord> {
    let writer = registry_writer(&identity.key)?;
    let _lock = writer.lock().context("lock tsgo service recovery")?;
    if let Some(current) = read_record(&identity.key)? {
        if !force {
            if let Ok(reply) = send_service(&current, ServiceCommand::Ping).await {
                if service_reply_compatible(identity, &reply) {
                    return Ok(current);
                }
            }
        }
        cleanup_record(processes, &current).await?;
    } else {
        let _ = failed;
    }
    launch_service_locked(processes, identity, &writer).await
}

async fn launch_service_locked(
    processes: &ProcessSupervisor,
    identity: &ServiceIdentity,
    writer: &AtomicFileWriter,
) -> Result<RegistryRecord> {
    let executable = std::env::current_exe().context("resolve Kit executable")?;
    let socket_path = socket_path(&identity.key)?;
    let token = Uuid::new_v4().to_string();
    let arguments = vec![
        OsString::from("tsgo"),
        OsString::from("__serve"),
        OsString::from("--key"),
        OsString::from(&identity.key),
        OsString::from("--workspace"),
        identity.workspace.as_os_str().to_owned(),
        OsString::from("--launcher"),
        identity.launcher.as_os_str().to_owned(),
        OsString::from("--server-version"),
        OsString::from(&identity.server_version),
        OsString::from("--socket"),
        socket_path.as_os_str().to_owned(),
        OsString::from("--token"),
        OsString::from(&token),
    ];
    let environment =
        ProcessEnvironment::new(EnvironmentBase::Inherit, BTreeMap::new(), BTreeSet::new())?;
    let command = CommandSpec::new(
        executable.into_os_string(),
        arguments,
        identity.workspace.clone(),
        environment,
        ProcessLabel::new("tsgo language service".to_owned())?,
    )?;
    let spec = DetachedProcessSpec::new(
        command,
        DetachedOutputPolicy::Discard,
        DetachedOutputPolicy::Discard,
        DetachedLifetimeRequirement::InvocationIndependent,
        TerminationPolicy::new(DETACHED_GRACE),
    );
    let transaction =
        processes.launch_detached(spec).await.context("launch detached tsgo service")?;
    let record = RegistryRecord {
        schema: REGISTRY_SCHEMA,
        identity: identity.clone(),
        socket_path,
        daemon_receipt: transaction.receipt().encode(),
        token,
        published_at_ms: now_unix_ms(),
    };
    write_record(writer, &record)?;
    let receipt = match transaction.commit() {
        Ok(receipt) => receipt,
        Err(error) => {
            remove_state(&record)?;
            let detail = error.to_string();
            return match error.into_transaction().rollback(anyhow!(detail)).await {
                Ok(cause) => Err(cause),
                Err(rollback) => Err(anyhow!(rollback)),
            };
        }
    };
    for _ in 0..READY_ATTEMPTS {
        if let Ok(reply) = send_service(&record, ServiceCommand::Ping).await {
            if service_reply_compatible(identity, &reply) {
                return Ok(record);
            }
        }
        sleep(READY_INTERVAL).await;
    }
    let startup_error = anyhow!("tsgo service {} did not become ready", identity.key);
    let cleanup = release_receipt(processes, &receipt).await;
    if cleanup.is_ok() {
        remove_state(&record)?;
        Err(startup_error)
    } else {
        Err(startup_error.context(format!(
            "detached cleanup failed; recovery record retained at {}",
            record_path(&identity.key)?.display()
        )))
    }
}

async fn send_service(record: &RegistryRecord, command: ServiceCommand) -> Result<ServiceReply> {
    validate_record(record)?;
    let request_id = Uuid::new_v4().to_string();
    let request =
        ServiceRequest { token: record.token.clone(), request_id: request_id.clone(), command };
    let stream = timeout(Duration::from_secs(3), UnixStream::connect(&record.socket_path))
        .await
        .context("time out connecting to tsgo service")?
        .with_context(|| format!("connect tsgo service {}", record.identity.key))?;
    let mut reader = BufReader::new(stream);
    let mut encoded = serde_json::to_vec(&request)?;
    encoded.push(b'\n');
    reader.get_mut().write_all(&encoded).await?;
    let mut response = String::new();
    let mut limited = reader.take(SOCKET_RESPONSE_LIMIT + 1);
    timeout(Duration::from_secs(60), limited.read_line(&mut response))
        .await
        .context("time out waiting for tsgo service reply")??;
    if response.len() as u64 > SOCKET_RESPONSE_LIMIT {
        bail!("tsgo service reply exceeded 16 MiB");
    }
    let reply: ServiceReply = serde_json::from_str(response.trim())?;
    if reply.request_id != request_id {
        bail!("tsgo service reply correlation mismatch");
    }
    Ok(reply)
}

async fn inspect_services(
    processes: &ProcessSupervisor,
    workspace: Option<&Path>,
) -> Result<ManagementOutput> {
    let mut services = Vec::new();
    for record in records()? {
        if !matches_workspace(&record, workspace) {
            continue;
        }
        let receipt = DetachedProcessReceipt::decode(&record.daemon_receipt).ok();
        let daemon_run_id = receipt.as_ref().map(|receipt| receipt.run_id().to_string());
        match send_service(&record, ServiceCommand::Inspect).await {
            Ok(reply) if service_reply_compatible(&record.identity, &reply) => {
                services.push(InspectEntry {
                    identity: record.identity,
                    status: "running".to_owned(),
                    service: reply.service,
                    daemon_run_id,
                    result: reply.result,
                    detail: None,
                })
            }
            outcome => {
                let authority = match receipt {
                    Some(receipt) => match processes.inspect_detached(&receipt).await {
                        Ok(status) => format!("{:?}", status_kind(&status)),
                        Err(error) => format!("unavailable: {error}"),
                    },
                    None => "invalid receipt".to_owned(),
                };
                let detail = match outcome {
                    Ok(reply) => reply.error,
                    Err(error) => Some(format!("{error:#}")),
                };
                services.push(InspectEntry {
                    identity: record.identity,
                    status: "stale".to_owned(),
                    service: None,
                    daemon_run_id,
                    result: None,
                    detail: Some(format!("{authority}; {}", detail.unwrap_or_default())),
                });
            }
        }
    }
    Ok(ManagementOutput { action: "inspect", matched: services.len(), changed: 0, services })
}

async fn stop_services(
    processes: &ProcessSupervisor,
    workspace: Option<&Path>,
) -> Result<ManagementOutput> {
    let mut services = Vec::new();
    let mut changed = 0;
    for record in records()? {
        if !matches_workspace(&record, workspace) {
            continue;
        }
        let reply = send_service(&record, ServiceCommand::Stop).await.ok();
        let service = reply.as_ref().and_then(|reply| reply.service.clone());
        let result = reply.as_ref().and_then(|reply| reply.result.clone());
        let receipt = DetachedProcessReceipt::decode(&record.daemon_receipt)
            .context("decode tsgo detached receipt")?;
        release_receipt(processes, &receipt).await?;
        remove_state(&record)?;
        changed += 1;
        services.push(InspectEntry {
            identity: record.identity,
            status: "stopped".to_owned(),
            service,
            daemon_run_id: Some(receipt.run_id().to_string()),
            result,
            detail: reply.and_then(|reply| reply.error),
        });
    }
    Ok(ManagementOutput { action: "stop", matched: services.len(), changed, services })
}

async fn prune_services(
    processes: &ProcessSupervisor,
    workspace: Option<&Path>,
    all: bool,
) -> Result<ManagementOutput> {
    let mut services = Vec::new();
    let mut changed = 0;
    for record in records()? {
        if !matches_workspace(&record, workspace) {
            continue;
        }
        if let Ok(reply) = send_service(&record, ServiceCommand::Ping).await {
            if service_reply_compatible(&record.identity, &reply) {
                services.push(InspectEntry {
                    identity: record.identity,
                    status: "running".to_owned(),
                    service: reply.service,
                    daemon_run_id: DetachedProcessReceipt::decode(&record.daemon_receipt)
                        .ok()
                        .map(|receipt| receipt.run_id().to_string()),
                    result: reply.result,
                    detail: Some("retained".to_owned()),
                });
                continue;
            }
        }
        let receipt = DetachedProcessReceipt::decode(&record.daemon_receipt)
            .context("decode stale tsgo detached receipt")?;
        release_receipt(processes, &receipt).await?;
        remove_state(&record)?;
        changed += 1;
        services.push(InspectEntry {
            identity: record.identity,
            status: "pruned".to_owned(),
            service: None,
            daemon_run_id: Some(receipt.run_id().to_string()),
            result: None,
            detail: None,
        });
    }
    if all {
        changed += prune_invalid_records()?;
    }
    Ok(ManagementOutput { action: "prune", matched: services.len(), changed, services })
}

async fn cleanup_record(processes: &ProcessSupervisor, record: &RegistryRecord) -> Result<()> {
    validate_record(record)?;
    let receipt = DetachedProcessReceipt::decode(&record.daemon_receipt)
        .context("decode stale tsgo detached receipt")?;
    release_receipt(processes, &receipt).await?;
    remove_state(record)
}

async fn release_receipt(
    processes: &ProcessSupervisor,
    receipt: &DetachedProcessReceipt,
) -> Result<()> {
    match processes.inspect_detached(receipt).await {
        Ok(DetachedProcessStatus::Running | DetachedProcessStatus::Stopping) => {
            processes.stop_detached(receipt).await.context("stop detached tsgo service")?;
        }
        Ok(DetachedProcessStatus::Completed(_) | DetachedProcessStatus::Failed(_)) => {}
        Err(error) => {
            processes
                .forget_detached(receipt)
                .await
                .with_context(|| format!("reconcile detached tsgo service after {error}"))?;
            return Ok(());
        }
    }
    processes.forget_detached(receipt).await.context("forget detached tsgo service")
}

fn status_kind(status: &DetachedProcessStatus) -> &'static str {
    match status {
        DetachedProcessStatus::Running => "running",
        DetachedProcessStatus::Stopping => "stopping",
        DetachedProcessStatus::Completed(_) => "completed",
        DetachedProcessStatus::Failed(_) => "failed",
    }
}

fn records() -> Result<Vec<RegistryRecord>> {
    let dir = runtime_dir()?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("read tsgo service registry"),
    };
    let mut records = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        if let Ok(record) = read_record_path(&path) {
            records.push(record);
        }
    }
    records.sort_by(|left, right| left.identity.key.cmp(&right.identity.key));
    Ok(records)
}

fn read_record(key: &str) -> Result<Option<RegistryRecord>> {
    let path = record_path(key)?;
    match read_record_path(&path) {
        Ok(record) => Ok(Some(record)),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn read_record_path(path: &Path) -> Result<RegistryRecord> {
    let metadata = path.metadata()?;
    if !metadata.is_file() || metadata.len() > REGISTRY_FILE_LIMIT {
        bail!("invalid tsgo registry file {}", path.display());
    }
    let raw = std::fs::read(path)?;
    let record: RegistryRecord = serde_json::from_slice(&raw)
        .with_context(|| format!("decode tsgo registry file {}", path.display()))?;
    validate_record(&record)?;
    if path.file_stem().and_then(|name| name.to_str()) != Some(&record.identity.key) {
        bail!("tsgo registry filename does not match its service key");
    }
    Ok(record)
}

fn validate_record(record: &RegistryRecord) -> Result<()> {
    if record.schema != REGISTRY_SCHEMA {
        bail!("unsupported tsgo registry schema {}", record.schema);
    }
    validate_identity(&record.identity)?;
    if record.socket_path != socket_path(&record.identity.key)? {
        bail!("tsgo registry socket does not match its service key");
    }
    if record.token.is_empty() {
        bail!("tsgo registry token is empty");
    }
    Ok(())
}

fn write_record(writer: &AtomicFileWriter, record: &RegistryRecord) -> Result<()> {
    let path = record_path(&record.identity.key)?;
    let bytes = serde_json::to_vec_pretty(record)?;
    writer.replace(&path, &bytes).context("publish tsgo service registry")
}

fn remove_state(record: &RegistryRecord) -> Result<()> {
    validate_record(record)?;
    remove_owned_path(&record.socket_path)?;
    remove_owned_path(&record_path(&record.identity.key)?)
}

fn remove_owned_path(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!("refusing symlinked tsgo state path {}", path.display());
            }
            std::fs::remove_file(path)
                .with_context(|| format!("remove tsgo state {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect tsgo state {}", path.display())),
    }
}

fn prune_invalid_records() -> Result<usize> {
    let dir = runtime_dir()?;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).context("read tsgo service registry"),
    };
    let mut removed = 0;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json")
            || read_record_path(&path).is_ok()
        {
            continue;
        }
        let Some(key) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        if key.len() != 32 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        remove_owned_path(&path)?;
        remove_owned_path(&dir.join(format!("{key}.sock")))?;
        removed += 1;
    }
    Ok(removed)
}

fn matches_workspace(record: &RegistryRecord, workspace: Option<&Path>) -> bool {
    workspace.is_none_or(|workspace| record.identity.workspace == workspace)
}

fn registry_writer(key: &str) -> Result<AtomicFileWriter> {
    Ok(AtomicFileWriter::new(runtime_dir()?, format!("{key}.lock"), format!(".{key}")))
}

fn record_path(key: &str) -> Result<PathBuf> {
    Ok(runtime_dir()?.join(format!("{key}.json")))
}

fn socket_path(key: &str) -> Result<PathBuf> {
    Ok(runtime_dir()?.join(format!("{key}.sock")))
}

pub(super) fn runtime_dir() -> Result<PathBuf> {
    let project = ProjectDirs::from("", "", "kit").context("resolve Kit runtime directory")?;
    let base = project
        .state_dir()
        .context("Kit has no stable state directory for tsgo service ownership")?;
    Ok(base.join("tsgo"))
}

pub(super) fn ensure_runtime_dir() -> Result<PathBuf> {
    let dir = runtime_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create tsgo runtime directory {}", dir.display()))?;
    let metadata = std::fs::symlink_metadata(&dir)
        .with_context(|| format!("inspect tsgo runtime directory {}", dir.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("tsgo runtime path is not a private directory: {}", dir.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != rustix::process::getuid().as_raw() {
            bail!("tsgo runtime directory is owned by another user: {}", dir.display());
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restrict tsgo runtime directory {}", dir.display()))?;
    }
    Ok(dir)
}

pub(super) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn render_trace(cx: &Context, service: ServiceInfo, result: TraceResult) -> Result<()> {
    let ascii = render::trace_text(&service, &result)?;
    let output = TraceOutput { action: "trace", service, result, ascii };
    if cx.out.is_json() {
        return cx.out.json(&output);
    }
    println!("{}", output.ascii);
    Ok(())
}

fn render_locus(cx: &Context, service: ServiceInfo, result: LocusResult) -> Result<()> {
    let text = render::locus_text(&service, &result);
    let output = LocusOutput { action: "locus", service, result, text };
    if cx.out.is_json() {
        return cx.out.json(&output);
    }
    println!("{}", output.text);
    Ok(())
}

fn render_diagnose(cx: &Context, service: ServiceInfo, result: DiagnoseResult) -> Result<()> {
    let exit_code = result.exit_code();
    let text = render::diagnose_text(&service, &result);
    let output = DiagnoseOutput::Result { action: "diagnose", service, result, text: text.clone() };
    if cx.out.is_json() {
        cx.out.json(&output)?;
    } else {
        println!("{text}");
    }
    finish(exit_code)
}

fn render_diagnose_failure(cx: &Context, error: anyhow::Error) -> Result<()> {
    let detail = bounded_check_output(&format!("{error:#}"));
    let text = render::diagnostic_failure_text("Document Diagnostics", &detail);
    let output = DiagnoseOutput::OperationalFailure {
        action: "diagnose",
        failure: DiagnosticCommandFailure::Operational { detail },
        text: text.clone(),
    };
    if cx.out.is_json() {
        cx.out.json(&output)?;
    } else {
        println!("{text}");
    }
    finish(2)
}

fn render_check(cx: &Context, result: CheckResult) -> Result<()> {
    let exit_code = result.exit_code();
    let text = render::check_text(&result);
    let output = CheckOutput::Result { action: "check", result, text: text.clone() };
    if cx.out.is_json() {
        cx.out.json(&output)?;
    } else {
        println!("{text}");
    }
    finish(exit_code)
}

fn render_check_failure(cx: &Context, error: anyhow::Error) -> Result<()> {
    let detail = bounded_check_output(&format!("{error:#}"));
    let text = render::diagnostic_failure_text("TypeScript Project Check", &detail);
    let output = CheckOutput::OperationalFailure {
        action: "check",
        failure: DiagnosticCommandFailure::Operational { detail },
        text: text.clone(),
    };
    if cx.out.is_json() {
        cx.out.json(&output)?;
    } else {
        println!("{text}");
    }
    finish(2)
}

fn finish(exit_code: i32) -> Result<()> {
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn render_management(cx: &Context, output: ManagementOutput) -> Result<()> {
    if cx.out.is_json() {
        return cx.out.json(&output);
    }
    println!("{}: {} matched, {} changed", output.action, output.matched, output.changed);
    for service in output.services {
        println!(
            "  {}  {}  {}",
            service.status,
            service.identity.key,
            service.identity.workspace.display()
        );
        if let Some(live) = service.service {
            println!(
                "    instance {} · child {} · requests {} · tsgo {}",
                live.instance_id, live.child.run_id, live.request_count, live.child.server_version
            );
        }
        if let Some(detail) = service.detail {
            println!("    {detail}");
        }
        if let Some(result) = service.result {
            println!("    {}", serde_json::to_string(&result)?);
        }
    }
    Ok(())
}
