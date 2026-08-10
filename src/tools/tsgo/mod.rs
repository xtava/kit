//! Warm, workspace-scoped native TypeScript language-service queries.

mod protocol;
mod service;

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{anyhow, bail, Context as _, Result};
use async_trait::async_trait;
use clap::{ArgMatches, Args, Command, CommandFactory, FromArgMatches, Parser, Subcommand};
use directories::ProjectDirs;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::{sleep, timeout},
};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};

use crate::framework::{AtomicFileWriter, Context, RepositoryLocator, Tool, ToolMeta};
use crate::framework::process::{
    CaptureOverflow, CapturePolicy, CommandSpec, ContainmentRequirement,
    DetachedLifetimeRequirement, DetachedOutputPolicy, DetachedProcessReceipt,
    DetachedProcessSpec, DetachedProcessStatus, EnvironmentBase, InputPolicy, LeaderExit,
    LeaderExitObservation, OutputPolicy, OutputReport, ProcessDeadline, ProcessEnvironment,
    ProcessLabel, ProcessSpec, ProcessSupervisor, TerminationPolicy,
};

use protocol::{
    CallKind, InspectEntry, ManagementOutput, QueryOutput, RegistryRecord, ServiceCommand,
    ServiceIdentity, ServiceInfo, ServiceReply, ServiceRequest, REGISTRY_SCHEMA,
};

const VERSION_CAPTURE: NonZeroUsize = NonZeroUsize::new(64 * 1024).unwrap();
const DETACHED_GRACE: Duration = Duration::from_secs(5);
const READY_ATTEMPTS: usize = 200;
const READY_INTERVAL: Duration = Duration::from_millis(50);
const REGISTRY_FILE_LIMIT: u64 = 64 * 1024;

pub fn tool() -> TsgoTool {
    TsgoTool
}

pub struct TsgoTool;

#[derive(Parser)]
#[command(
    name = "tsgo",
    about = "Warm native TypeScript call-hierarchy service",
    long_about = "Runs one reusable native tsgo language server per canonical workspace and exact server version. The first call starts it lazily; inspect, stop, and prune own the complete lifecycle."
)]
struct TsgoArgs {
    #[command(subcommand)]
    command: TsgoCommand,
}

#[derive(Subcommand)]
enum TsgoCommand {
    /// Query native TypeScript call hierarchy; starts the workspace service lazily.
    Call {
        #[command(subcommand)]
        command: CallCommand,
        /// Resolve the canonical Git worktree from this path instead of the queried file.
        #[arg(long, global = true)]
        workspace: Option<PathBuf>,
        /// Exact tsgo launcher. Defaults to <workspace>/node_modules/.bin/tsgo.
        #[arg(long, global = true)]
        tsgo: Option<PathBuf>,
    },
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

#[derive(Subcommand)]
enum CallCommand {
    /// Prepare call-hierarchy items at a UTF-16 LSP position.
    Prepare(LocationArgs),
    /// Return callers for one prepared item.
    Incoming(HierarchyArgs),
    /// Return callees for one prepared item.
    Outgoing(HierarchyArgs),
}

#[derive(Args)]
struct LocationArgs {
    file: PathBuf,
    #[arg(long)]
    line: u32,
    #[arg(long)]
    character: u32,
}

#[derive(Args)]
struct HierarchyArgs {
    #[command(flatten)]
    location: LocationArgs,
    /// Prepared item index when tsgo returns more than one item.
    #[arg(long, default_value_t = 0)]
    item: usize,
}

#[async_trait]
impl Tool for TsgoTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: "tsgo",
            about: "Warm native TypeScript call-hierarchy service",
            version: env!("CARGO_PKG_VERSION"),
        }
    }

    fn command(&self) -> Command {
        TsgoArgs::command()
    }

    async fn run(&self, cx: &Context, matches: &ArgMatches) -> Result<()> {
        let args = TsgoArgs::from_arg_matches(matches)?;
        match args.command {
            TsgoCommand::Call { command, workspace, tsgo } => {
                let (kind, location, item, action) = match command {
                    CallCommand::Prepare(location) => {
                        (CallKind::Prepare, location, 0, "call-prepare")
                    }
                    CallCommand::Incoming(args) => {
                        (CallKind::Incoming, args.location, args.item, "call-incoming")
                    }
                    CallCommand::Outgoing(args) => {
                        (CallKind::Outgoing, args.location, args.item, "call-outgoing")
                    }
                };
                let (identity, file) = resolve_query_identity(
                    &cx.repositories,
                    &cx.processes,
                    workspace.as_deref(),
                    tsgo.as_deref(),
                    &location.file,
                )
                .await?;
                let (service, result) = execute_call(
                    &cx.processes,
                    &identity,
                    ServiceCommand::Call {
                        kind,
                        file,
                        line: location.line,
                        character: location.character,
                        item,
                    },
                )
                .await?;
                render_query(cx, QueryOutput { action, service, result })
            }
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
            TsgoCommand::Serve {
                key,
                workspace,
                launcher,
                server_version,
                socket,
                token,
            } => {
                let identity = ServiceIdentity { key, workspace, launcher, server_version };
                validate_identity(&identity)?;
                service::serve(&cx.processes, identity, socket, token).await
            }
        }
    }
}

async fn resolve_query_identity(
    repositories: &RepositoryLocator,
    processes: &ProcessSupervisor,
    workspace: Option<&Path>,
    launcher: Option<&Path>,
    file: &Path,
) -> Result<(ServiceIdentity, PathBuf)> {
    let current = std::env::current_dir().context("resolve current directory")?;
    let unresolved_file = if file.is_absolute() { file.to_path_buf() } else { current.join(file) };
    let file = unresolved_file
        .canonicalize()
        .with_context(|| format!("canonicalize TypeScript file {}", unresolved_file.display()))?;
    let start = workspace
        .map(Path::to_path_buf)
        .unwrap_or_else(|| file.parent().unwrap_or(&file).to_path_buf());
    let workspace = repositories.nearest_worktree_root(&start)?.as_path().to_path_buf();
    if !file.starts_with(&workspace) {
        bail!("{} is outside workspace {}", file.display(), workspace.display());
    }
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
    let server_version = resolve_server_version(processes, &workspace, &launcher).await?;
    let key = identity_key(&workspace, &launcher, &server_version);
    Ok((ServiceIdentity { key, workspace, launcher, server_version }, file))
}

async fn resolve_server_version(
    processes: &ProcessSupervisor,
    workspace: &Path,
    launcher: &Path,
) -> Result<String> {
    let environment = ProcessEnvironment::new(
        EnvironmentBase::Inherit,
        BTreeMap::new(),
        BTreeSet::new(),
    )?;
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
    let expected = identity_key(
        &identity.workspace,
        &identity.launcher,
        &identity.server_version,
    );
    if identity.key != expected {
        bail!("tsgo service identity key does not match its workspace and server");
    }
    Ok(())
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

async fn execute_call(
    processes: &ProcessSupervisor,
    identity: &ServiceIdentity,
    command: ServiceCommand,
) -> Result<(ServiceInfo, Value)> {
    let mut record = ensure_service(processes, identity).await?;
    for attempt in 0..2 {
        match send_service(&record, command.clone()).await {
            Ok(reply) if reply.ok => {
                let service = reply.service.context("tsgo service reply omitted identity")?;
                return Ok((service, reply.result.unwrap_or(Value::Null)));
            }
            Ok(reply) if !reply.fatal => {
                bail!("{}", reply.error.unwrap_or_else(|| "tsgo query failed".to_owned()));
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
                if reply.ok {
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
                if reply.ok {
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
    let environment = ProcessEnvironment::new(
        EnvironmentBase::Inherit,
        BTreeMap::new(),
        BTreeSet::new(),
    )?;
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
    let transaction = processes
        .launch_detached(spec)
        .await
        .context("launch detached tsgo service")?;
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
            if reply.ok {
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
    let request = ServiceRequest {
        token: record.token.clone(),
        request_id: request_id.clone(),
        command,
    };
    let stream = timeout(Duration::from_secs(3), UnixStream::connect(&record.socket_path))
        .await
        .context("time out connecting to tsgo service")?
        .with_context(|| format!("connect tsgo service {}", record.identity.key))?;
    let mut reader = BufReader::new(stream);
    let mut encoded = serde_json::to_vec(&request)?;
    encoded.push(b'\n');
    reader.get_mut().write_all(&encoded).await?;
    let mut response = String::new();
    timeout(Duration::from_secs(60), reader.read_line(&mut response))
        .await
        .context("time out waiting for tsgo service reply")??;
    if response.len() as u64 > REGISTRY_FILE_LIMIT {
        bail!("tsgo service reply exceeded 64 KiB");
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
            Ok(reply) if reply.ok => services.push(InspectEntry {
                identity: record.identity,
                status: "running".to_owned(),
                service: reply.service,
                daemon_run_id,
                result: reply.result,
                detail: None,
            }),
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
    Ok(ManagementOutput {
        action: "inspect",
        matched: services.len(),
        changed: 0,
        services,
    })
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
    Ok(ManagementOutput {
        action: "stop",
        matched: services.len(),
        changed,
        services,
    })
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
            if reply.ok {
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
    Ok(ManagementOutput {
        action: "prune",
        matched: services.len(),
        changed,
        services,
    })
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
        Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|io| {
            io.kind() == std::io::ErrorKind::NotFound
        }) => Ok(None),
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
    Ok(AtomicFileWriter::new(
        runtime_dir()?,
        format!("{key}.lock"),
        format!(".{key}"),
    ))
}

fn record_path(key: &str) -> Result<PathBuf> {
    Ok(runtime_dir()?.join(format!("{key}.json")))
}

fn socket_path(key: &str) -> Result<PathBuf> {
    Ok(runtime_dir()?.join(format!("{key}.sock")))
}

pub(super) fn runtime_dir() -> Result<PathBuf> {
    let project = ProjectDirs::from("", "", "kit").context("resolve Kit runtime directory")?;
    let base = project.runtime_dir().or_else(|| project.state_dir()).with_context(|| {
        "Kit has neither a runtime directory nor a state directory for tsgo service ownership"
    })?;
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

fn render_query(cx: &Context, output: QueryOutput) -> Result<()> {
    if cx.out.is_json() {
        return cx.out.json(&output);
    }
    println!(
        "{} · instance {} · child {} · request {}",
        output.action,
        output.service.instance_id,
        output.service.child.run_id,
        output.service.request_count
    );
    println!("{}", serde_json::to_string_pretty(&output.result)?);
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
                live.instance_id,
                live.child.run_id,
                live.request_count,
                live.child.server_version
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
