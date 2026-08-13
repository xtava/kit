use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    ffi::OsString,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context as _, Result};
use ignore::WalkBuilder;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot, Notify},
    task::JoinHandle,
    time::timeout,
};
use url::Url;
use uuid::Uuid;

use crate::cdp::SourceMap;
use crate::framework::process::{
    CommandSpec, ContainmentRequirement, EnvironmentBase, InputPolicy, OutputPolicy,
    ProcessByteEvent, ProcessByteStream, ProcessEnvironment, ProcessInputHandle,
    ProcessInputWriter, ProcessLabel, ProcessOutputHandle, ProcessSession, ProcessSpec,
    ProcessSupervisor, StreamPolicy, TerminationPolicy,
};

use super::diagnostics;
use super::graph;
use super::locus;
use super::protocol::{
    ChangedDiagnosticDocument, ChildIdentity, DiagnoseCompleteness, DiagnoseIncompleteReason,
    DiagnoseProject, DiagnoseRequest, DiagnoseResult, DiagnoseTiming, DiagnoseVerdict,
    DiagnosedDocument, DiagnosticAuthority, DiagnosticDependencyFreshness,
    DiagnosticProjectContexts, DiagnosticRecheckValue, LocusAcquisition, LocusAcquisitionResult,
    LocusAcquisitionState, LocusAnchor, LocusCapture, LocusCaptureCut, LocusCapturedCandidate,
    LocusCapturedFile, LocusChangedFile, LocusCutReason, LocusEvidence, LocusEvidenceCapture,
    LocusFreshness, LocusOmission, LocusOperation, LocusPrepareReceipt, LocusRecheckValue,
    LocusRequest, LocusSeedCandidate, LocusSeedResult, LocusSessionIntegrity, LocusTiming,
    RequestedDocumentFreshness, ServiceCommand, ServiceIdentity, ServiceInfo, ServiceReply,
    ServiceRequest, TraceAdvice, TraceAdviceReason, TraceBoundary, TraceBoundaryKind,
    TraceCallerGapReason, TraceCandidate, TraceCoverage, TraceCoveredDocument, TraceDirection,
    TraceDiscovery, TraceDocumentSync, TraceEdge, TraceGap, TraceIdentityGapReason, TraceLimits,
    TraceLocation, TraceNode, TracePackageScope, TraceProjectContext, TraceProjectOmissionReason,
    TraceResult, TraceScope, TraceScopeReceipt, TraceSelector, TraceStatus, TraceSummary,
    TraceTiming, TraceWorkspaceCoverage, DIAGNOSTIC_SCHEMA_VERSION,
    MAX_DIAGNOSE_TOTAL_SOURCE_BYTES, MAX_LOCUS_CANDIDATES, MAX_LOCUS_LABEL_BYTES,
    MAX_LOCUS_OBSERVED_FILES, MAX_LOCUS_SOURCE_BYTES, MAX_LOCUS_TEXT_BYTES,
    MAX_LOCUS_TOTAL_AMBIGUITY_CANDIDATES, MAX_LOCUS_TOTAL_CALL_SITES, MAX_LOCUS_TOTAL_EVIDENCE,
    MAX_LOCUS_TOTAL_SOURCE_BYTES, MAX_TRACE_DEPTH, MAX_TRACE_NATIVE_VARIANTS, MAX_TRACE_NODES,
    MAX_TRACE_PROJECT_CONTEXTS, SERVICE_PROTOCOL_VERSION,
};

const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const PROCESS_GRACE: Duration = Duration::from_secs(3);
const PROCESS_KILL_WAIT: Duration = Duration::from_secs(3);
const SOCKET_REQUEST_LIMIT: u64 = 1024 * 1024;
const LSP_MESSAGE_LIMIT: usize = 16 * 1024 * 1024;
const SOCKET_REPLY_LIMIT: usize = 16 * 1024 * 1024;
const STREAM_BUDGET: NonZeroUsize = NonZeroUsize::new(4 * 1024 * 1024).unwrap();
const DISCOVERY_MATCH_LIMIT: usize = 256;
const TRACE_ADVICE_NODE_THRESHOLD: usize = 64;
const TRACE_ADVICE_EDGE_THRESHOLD: usize = 96;
const TRACE_ADVICE_ELAPSED_THRESHOLD: Duration = Duration::from_secs(2);
const TRACE_SOURCE_MAP_LIMIT: u64 = 64 * 1024 * 1024;
const NATIVE_REQUEST_DEADLINE: Duration = Duration::from_secs(50);
const NATIVE_CANCEL_DRAIN: Duration = Duration::from_secs(3);

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

struct ActorMessage {
    command: ServiceCommand,
    response: oneshot::Sender<ActorReply>,
}

enum ActorReply {
    Success { service: ServiceInfo, result: Value, stop: bool },
    Failure { message: String, fatal: bool },
}

struct LocusExecution {
    result: Value,
    session_integrity_lost: bool,
}

#[derive(Clone)]
struct ResolvedLocusSeed {
    file: PathBuf,
    line: u32,
    character: u32,
    anchor: LocusAnchor,
}

struct AcquiredLocusEvidence {
    state: LocusAcquisitionState,
    evidence: Vec<LocusEvidence>,
    prepare: Option<LocusPrepareReceipt>,
}

struct ObservedDocument {
    absolute: PathBuf,
    public: PathBuf,
    sha256: String,
}

struct LocusObservation {
    first_sha256: String,
    source_bytes: u64,
}

struct TraceCapture {
    documents: BTreeMap<PathBuf, TraceDocumentSync>,
}

struct EffectiveTraceScope {
    source_roots: Vec<PathBuf>,
    package_root: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum TraceNormalizationMode {
    Native,
    CanonicalSource,
}

#[derive(Clone)]
struct NormalizedTraceItem {
    native_id: String,
    id: String,
    node: TraceNode,
    gap: Option<TraceGap>,
}

struct NativeLocusCapture {
    seeds: Vec<LocusSeedResult>,
    acquisitions: Vec<LocusAcquisitionResult>,
    evidence: Vec<LocusEvidence>,
    supplied_candidates: Vec<LocusCapturedCandidate>,
    session_integrity_lost: bool,
}

pub async fn serve(
    processes: &ProcessSupervisor,
    identity: ServiceIdentity,
    socket_path: PathBuf,
    token: String,
) -> Result<()> {
    super::ensure_runtime_dir()?;
    validate_socket_path(&identity, &socket_path)?;
    remove_owned_socket(&socket_path)?;

    let lsp = LspSession::start(processes, &identity).await?;
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind tsgo service socket {}", socket_path.display()))?;
    #[cfg(unix)]
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restrict tsgo service socket {}", socket_path.display()))?;

    let shutdown = Arc::new(Notify::new());
    let (actor_tx, actor_rx) = mpsc::channel(64);
    tokio::spawn(run_actor(lsp, identity, actor_rx, shutdown.clone()));

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accept tsgo service client")?;
                let token = token.clone();
                let actor_tx = actor_tx.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let _ = handle_client(stream, &token, actor_tx, shutdown).await;
                });
            }
            _ = shutdown.notified() => break,
        }
    }

    drop(listener);
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

async fn run_actor(
    mut lsp: LspSession,
    identity: ServiceIdentity,
    mut receiver: mpsc::Receiver<ActorMessage>,
    shutdown: Arc<Notify>,
) {
    let instance_id = Uuid::new_v4().to_string();
    let started_at_ms = super::now_unix_ms();
    let child = lsp.child_identity.clone();
    let mut request_count = 0u64;

    loop {
        let message = match timeout(IDLE_TIMEOUT, receiver.recv()).await {
            Ok(Some(message)) => message,
            Ok(None) | Err(_) => {
                let _ = lsp.finish(true).await;
                shutdown.notify_waiters();
                break;
            }
        };

        match message.command {
            ServiceCommand::Ping | ServiceCommand::Inspect => {
                let service = service_info(
                    &identity,
                    &instance_id,
                    started_at_ms,
                    request_count,
                    "running",
                    &child,
                );
                let _ = message.response.send(ActorReply::Success {
                    service,
                    result: Value::Null,
                    stop: false,
                });
            }
            ServiceCommand::Trace { selector, direction, limits, scope } => {
                match lsp.trace(selector, direction, limits, scope).await {
                    Ok(result) => {
                        request_count += 1;
                        let service = service_info(
                            &identity,
                            &instance_id,
                            started_at_ms,
                            request_count,
                            "running",
                            &child,
                        );
                        let _ = message.response.send(ActorReply::Success {
                            service,
                            result,
                            stop: false,
                        });
                    }
                    Err(error) => {
                        let detail = format!("native tsgo request failed: {error:#}");
                        let _ = lsp.finish(false).await;
                        let _ = message
                            .response
                            .send(ActorReply::Failure { message: detail, fatal: true });
                        shutdown.notify_waiters();
                        break;
                    }
                }
            }
            ServiceCommand::Locus { request } => match lsp.locus(request).await {
                Ok(execution) => {
                    request_count += 1;
                    if execution.session_integrity_lost {
                        let _ = lsp.finish(false).await;
                    }
                    let service = service_info(
                        &identity,
                        &instance_id,
                        started_at_ms,
                        request_count,
                        if execution.session_integrity_lost { "lost" } else { "running" },
                        &child,
                    );
                    let _ = message.response.send(ActorReply::Success {
                        service,
                        result: execution.result,
                        stop: execution.session_integrity_lost,
                    });
                    if execution.session_integrity_lost {
                        shutdown.notify_waiters();
                        break;
                    }
                }
                Err(error) => {
                    let fatal = lsp.session.is_none();
                    let _ = message.response.send(ActorReply::Failure {
                        message: format!("tsgo locus case failed: {error:#}"),
                        fatal,
                    });
                    if fatal {
                        shutdown.notify_waiters();
                        break;
                    }
                }
            },
            ServiceCommand::Diagnose { request } => match lsp.diagnose(request).await {
                Ok(result) => {
                    request_count += 1;
                    let service = service_info(
                        &identity,
                        &instance_id,
                        started_at_ms,
                        request_count,
                        "running",
                        &child,
                    );
                    let _ =
                        message.response.send(ActorReply::Success { service, result, stop: false });
                }
                Err(error) => {
                    let fatal = matches!(error, NativeFailure::Lost { .. });
                    if fatal {
                        let _ = lsp.finish(false).await;
                    }
                    let _ = message.response.send(ActorReply::Failure {
                        message: format!("tsgo diagnose failed: {error}"),
                        fatal,
                    });
                    if fatal {
                        shutdown.notify_waiters();
                        break;
                    }
                }
            },
            ServiceCommand::Stop => {
                let outcome = lsp.finish(true).await;
                let service = service_info(
                    &identity,
                    &instance_id,
                    started_at_ms,
                    request_count,
                    "stopped",
                    &child,
                );
                let result = match outcome {
                    Ok(value) => value,
                    Err(error) => json!({ "graceful": false, "detail": format!("{error:#}") }),
                };
                let _ = message.response.send(ActorReply::Success { service, result, stop: true });
                break;
            }
        }
    }
}

fn service_info(
    identity: &ServiceIdentity,
    instance_id: &str,
    started_at_ms: u64,
    request_count: u64,
    state: &str,
    child: &ChildIdentity,
) -> ServiceInfo {
    ServiceInfo {
        key: identity.key.clone(),
        protocol_version: SERVICE_PROTOCOL_VERSION,
        instance_id: instance_id.to_owned(),
        started_at_ms,
        request_count,
        state: state.to_owned(),
        workspace: identity.workspace.clone(),
        child: child.clone(),
    }
}

async fn handle_client(
    stream: UnixStream,
    token: &str,
    actor: mpsc::Sender<ActorMessage>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let reader = BufReader::new(read_half);
    let mut limited = reader.take(SOCKET_REQUEST_LIMIT + 1);
    let mut line = String::new();
    timeout(Duration::from_secs(10), limited.read_line(&mut line))
        .await
        .context("time out reading tsgo service request")??;

    let parsed = if line.len() as u64 > SOCKET_REQUEST_LIMIT {
        Err(anyhow!("tsgo service request exceeds one MiB"))
    } else {
        serde_json::from_str::<ServiceRequest>(line.trim()).context("decode tsgo service request")
    };
    let reply = match parsed {
        Ok(request) if request.token != token => {
            ServiceReply::error(request.request_id, "tsgo service authentication failed", false)
        }
        Ok(request) => {
            let request_id = request.request_id;
            let (response_tx, response_rx) = oneshot::channel();
            let sent =
                actor.send(ActorMessage { command: request.command, response: response_tx }).await;
            match sent {
                Ok(()) => match response_rx.await {
                    Ok(ActorReply::Success { service, result, stop }) => {
                        if stop {
                            shutdown.notify_waiters();
                        }
                        ServiceReply::success(request_id, service, result)
                    }
                    Ok(ActorReply::Failure { message, fatal }) => {
                        ServiceReply::error(request_id, message, fatal)
                    }
                    Err(_) => {
                        ServiceReply::error(request_id, "tsgo service request owner stopped", true)
                    }
                },
                Err(_) => ServiceReply::error(
                    request_id,
                    "tsgo service request owner is unavailable",
                    true,
                ),
            }
        }
        Err(error) => ServiceReply::error(String::new(), format!("{error:#}"), false),
    };

    let mut encoded = serde_json::to_vec(&reply)?;
    if encoded.len() > SOCKET_REPLY_LIMIT {
        encoded = serde_json::to_vec(&ServiceReply::error(
            reply.request_id,
            "tsgo service result exceeds the 16 MiB reply limit",
            false,
        ))?;
    }
    encoded.push(b'\n');
    write_half.write_all(&encoded).await?;
    write_half.shutdown().await?;
    Ok(())
}

fn validate_socket_path(identity: &ServiceIdentity, socket_path: &Path) -> Result<()> {
    let runtime = super::runtime_dir()?;
    let expected = runtime.join(format!("{}.sock", identity.key));
    if socket_path != expected {
        bail!("refusing non-canonical tsgo socket path {}", socket_path.display());
    }
    Ok(())
}

fn remove_owned_socket(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            #[cfg(unix)]
            {
                if metadata.uid() != rustix::process::getuid().as_raw()
                    || (!metadata.file_type().is_socket() && !metadata.file_type().is_file())
                {
                    bail!("refusing unowned tsgo socket path {}", path.display());
                }
            }
            std::fs::remove_file(path)
                .with_context(|| format!("remove stale tsgo socket {}", path.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect tsgo service socket"),
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct DiagnosticCapabilities {
    supported: bool,
    inter_file_dependencies: bool,
    workspace_diagnostics: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct NativeCapabilities {
    definition: bool,
    source_definition: bool,
    references: bool,
    implementations: bool,
    call_hierarchy: bool,
    diagnostics: DiagnosticCapabilities,
}

#[derive(Debug)]
enum NativeFailure {
    Preserved { method: String, code: Option<i64>, detail: String },
    Lost { method: String, detail: String },
}

impl NativeFailure {
    fn preserved(method: impl Into<String>, error: impl std::fmt::Display) -> Self {
        Self::Preserved { method: method.into(), code: None, detail: bounded_failure_detail(error) }
    }

    fn response(method: &str, error: &Value) -> Self {
        Self::Preserved {
            method: method.to_owned(),
            code: error.get("code").and_then(Value::as_i64),
            detail: bounded_failure_detail(error),
        }
    }

    fn lost(method: impl Into<String>, error: impl std::fmt::Display) -> Self {
        Self::Lost { method: method.into(), detail: bounded_failure_detail(error) }
    }

    fn integrity(&self) -> LocusSessionIntegrity {
        match self {
            Self::Preserved { .. } => LocusSessionIntegrity::Preserved,
            Self::Lost { .. } => LocusSessionIntegrity::Lost,
        }
    }

    fn method_not_found(&self) -> bool {
        matches!(self, Self::Preserved { code: Some(-32601), .. })
    }
}

impl std::fmt::Display for NativeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preserved { method, code, detail } => match code {
                Some(code) => write!(formatter, "native tsgo {method} error {code}: {detail}"),
                None => write!(formatter, "native tsgo {method} failed: {detail}"),
            },
            Self::Lost { method, detail } => {
                write!(formatter, "native tsgo session lost during {method}: {detail}")
            }
        }
    }
}

impl std::error::Error for NativeFailure {}

impl From<anyhow::Error> for NativeFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::preserved("evidence-normalization", format!("{error:#}"))
    }
}

struct DocumentState {
    version: i64,
    text: String,
}

enum OpenDocumentMode {
    Semantic,
    Diagnostics { files: BTreeSet<PathBuf> },
}

struct LspSession {
    workspace: PathBuf,
    child_identity: ChildIdentity,
    session: Option<ProcessSession>,
    input: Option<ProcessInputWriter>,
    stdout: Option<ProcessByteStream>,
    stderr_task: Option<JoinHandle<()>>,
    buffer: Vec<u8>,
    next_request_id: u64,
    documents: HashMap<PathBuf, DocumentState>,
    document_mode: OpenDocumentMode,
    capabilities: NativeCapabilities,
    active_trace_capture: Option<TraceCapture>,
    active_locus_capture: Option<u64>,
    locus_observations: BTreeMap<PathBuf, LocusObservation>,
    next_locus_capture: u64,
}

impl LspSession {
    async fn start(processes: &ProcessSupervisor, identity: &ServiceIdentity) -> Result<Self> {
        let environment =
            ProcessEnvironment::new(EnvironmentBase::Inherit, BTreeMap::new(), BTreeSet::new())?;
        let command = CommandSpec::new(
            identity.launcher.clone().into_os_string(),
            vec![OsString::from("--lsp"), OsString::from("--stdio")],
            identity.workspace.clone(),
            environment,
            ProcessLabel::new("native tsgo language server".to_owned())?,
        )?;
        let spec = ProcessSpec::new(
            command,
            InputPolicy::Writable,
            OutputPolicy::Stream(StreamPolicy::new(STREAM_BUDGET)),
            OutputPolicy::Stream(StreamPolicy::new(STREAM_BUDGET)),
            ContainmentRequirement::ExplicitProcessGroup,
            crate::framework::process::ProcessDeadline::Unlimited,
            TerminationPolicy::new(PROCESS_GRACE),
        );
        let started = processes.spawn(spec).await.context("start native tsgo language server")?;
        let run_id = started.session.run_id().to_string();
        let input = match started.input {
            ProcessInputHandle::Writable(input) => input,
            other => bail!("native tsgo stdin policy mismatch: {other:?}"),
        };
        let stdout = match started.stdout {
            ProcessOutputHandle::Stream(stdout) => stdout,
            other => bail!("native tsgo stdout policy mismatch: {other:?}"),
        };
        let mut stderr = match started.stderr {
            ProcessOutputHandle::Stream(stderr) => stderr,
            other => bail!("native tsgo stderr policy mismatch: {other:?}"),
        };
        let stderr_task = tokio::spawn(async move {
            loop {
                match stderr.next().await {
                    Ok(ProcessByteEvent::Chunk { .. }) => {}
                    Ok(ProcessByteEvent::End) | Err(_) => break,
                }
            }
        });
        let child_identity = ChildIdentity {
            run_id,
            generation: 1,
            started_at_ms: super::now_unix_ms(),
            launcher: identity.launcher.clone(),
            server_version: identity.server_version.clone(),
        };
        let mut lsp = Self {
            workspace: identity.workspace.clone(),
            child_identity,
            session: Some(started.session),
            input: Some(input),
            stdout: Some(stdout),
            stderr_task: Some(stderr_task),
            buffer: Vec::new(),
            next_request_id: 1,
            documents: HashMap::new(),
            document_mode: OpenDocumentMode::Semantic,
            capabilities: NativeCapabilities::default(),
            active_trace_capture: None,
            active_locus_capture: None,
            locus_observations: BTreeMap::new(),
            next_locus_capture: 1,
        };
        if let Err(error) = lsp.initialize().await {
            let _ = lsp.finish(false).await;
            return Err(error.context("initialize native tsgo language server"));
        }
        Ok(lsp)
    }

    async fn initialize(&mut self) -> Result<()> {
        let root_uri = directory_uri(&self.workspace)?;
        let result = self
            .request(
                "initialize",
                Some(json!({
                    "processId": std::process::id(),
                    "clientInfo": { "name": "kit", "version": env!("CARGO_PKG_VERSION") },
                    "rootUri": root_uri,
                    "workspaceFolders": [{ "uri": root_uri, "name": workspace_name(&self.workspace) }],
                    "initializationOptions": { "disablePushDiagnostics": true },
                    "capabilities": {
                        "workspace": {
                            "configuration": true,
                            "workspaceFolders": true,
                            "symbol": { "dynamicRegistration": false }
                        },
                        "textDocument": {
                            "synchronization": { "dynamicRegistration": false, "didSave": false },
                            "definition": { "dynamicRegistration": false, "linkSupport": false },
                            "references": { "dynamicRegistration": false },
                            "implementation": { "dynamicRegistration": false, "linkSupport": false },
                            "callHierarchy": { "dynamicRegistration": false },
                            "diagnostic": {
                                "dynamicRegistration": false,
                                "relatedDocumentSupport": false
                            }
                        }
                    }
                })),
            )
            .await?;
        self.capabilities = NativeCapabilities {
            definition: capability_enabled(&result, "/capabilities/definitionProvider"),
            source_definition: capability_enabled(
                &result,
                "/capabilities/customSourceDefinitionProvider",
            ),
            references: capability_enabled(&result, "/capabilities/referencesProvider"),
            implementations: capability_enabled(&result, "/capabilities/implementationProvider"),
            call_hierarchy: capability_enabled(&result, "/capabilities/callHierarchyProvider"),
            diagnostics: diagnostic_capabilities(&result),
        };
        self.notify("initialized", Some(json!({}))).await
    }

    async fn trace(
        &mut self,
        selector: TraceSelector,
        direction: TraceDirection,
        limits: TraceLimits,
        scope: TraceScope,
    ) -> Result<Value> {
        validate_trace_limits(limits)?;
        if !self.capabilities.call_hierarchy {
            bail!("native tsgo did not advertise call hierarchy support");
        }
        if self.active_trace_capture.is_some() {
            bail!("native tsgo already has an active trace capture");
        }
        self.enter_semantic_document_mode().await?;
        let started = Instant::now();
        let request_start = self.next_request_id;
        self.active_trace_capture = Some(TraceCapture { documents: BTreeMap::new() });
        let captured = self.trace_captured(selector, direction, limits, scope).await;
        let capture = self
            .active_trace_capture
            .take()
            .context("trace capture state disappeared before completion")?;
        let mut result = captured?;
        result.coverage = self.trace_coverage(capture).await?;
        let elapsed = started.elapsed();
        result.timing = TraceTiming {
            elapsed_ms: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
            native_requests: self.next_request_id.saturating_sub(request_start),
        };
        if limits.max_depth > 1
            && (result.summary.nodes >= TRACE_ADVICE_NODE_THRESHOLD
                || result.summary.edges >= TRACE_ADVICE_EDGE_THRESHOLD
                || elapsed >= TRACE_ADVICE_ELAPSED_THRESHOLD)
        {
            result.advice.push(TraceAdvice {
                suggested_max_depth: 1,
                reason: TraceAdviceReason::BroadExpansion,
            });
        }
        serde_json::to_value(result).context("encode typed tsgo trace result")
    }

    async fn trace_captured(
        &mut self,
        selector: TraceSelector,
        direction: TraceDirection,
        limits: TraceLimits,
        scope: TraceScope,
    ) -> Result<TraceResult> {
        let selector_name = selector.display_name();
        let (prepared, candidates, discovery) = self.resolve_selector(&selector).await?;
        let prepared_items = prepared
            .as_array()
            .context("native tsgo returned a non-array call hierarchy preparation result")?;

        let mut result = empty_trace_result(
            selector_name,
            direction,
            candidates,
            discovery,
            trace_scope_receipt(&self.workspace, &scope, None),
        );
        match prepared_items.len() {
            0 => result.status = TraceStatus::NotFound,
            1 => {
                result = self
                    .traverse(
                        prepared_items[0].clone(),
                        result,
                        direction,
                        limits,
                        scope,
                        TraceNormalizationMode::CanonicalSource,
                    )
                    .await?;
            }
            _ => {
                result.status = TraceStatus::Ambiguous;
                if result.candidates.is_empty() {
                    result.candidates = prepared_items
                        .iter()
                        .map(|item| trace_candidate(item, &self.workspace))
                        .collect::<Result<Vec<_>>>()?;
                }
            }
        }
        if result.target.is_none() {
            if result.discovery.truncated {
                result.status = TraceStatus::Cut;
                result
                    .truncation_reasons
                    .push("symbol discovery candidate limit reached".to_owned());
            }
            result.summary.truncated = result.discovery.truncated;
        }
        Ok(result)
    }

    async fn trace_coverage(&mut self, capture: TraceCapture) -> Result<TraceCoverage> {
        let omitted_project_contexts =
            capture.documents.len().saturating_sub(MAX_TRACE_PROJECT_CONTEXTS);
        let mut documents = Vec::with_capacity(capture.documents.len());
        for (index, (file, sync)) in capture.documents.into_iter().enumerate() {
            let project = if index >= MAX_TRACE_PROJECT_CONTEXTS {
                TraceProjectContext::NotQueried {
                    reason: TraceProjectOmissionReason::ProjectContextLimit,
                }
            } else {
                match self.trace_project_context(&file).await {
                    Ok(project) => project,
                    Err(failure @ NativeFailure::Lost { .. }) => {
                        return Err(anyhow::Error::new(failure));
                    }
                    Err(failure) => {
                        TraceProjectContext::Unavailable { detail: bounded_failure_detail(failure) }
                    }
                }
            };
            documents.push(TraceCoveredDocument {
                file: public_file(&self.workspace, &file),
                sync,
                project,
            });
        }
        Ok(TraceCoverage {
            documents,
            omitted_project_contexts,
            workspace: TraceWorkspaceCoverage::ProjectFilesNotEnumerated,
        })
    }

    async fn trace_project_context(
        &mut self,
        file: &Path,
    ) -> std::result::Result<TraceProjectContext, NativeFailure> {
        let result = self
            .request_typed(
                "custom/projectInfo",
                Some(json!({ "textDocument": { "uri": file_uri(file)? } })),
            )
            .await?;
        let config = result.get("configFilePath").and_then(Value::as_str).ok_or_else(|| {
            NativeFailure::preserved(
                "custom/projectInfo",
                "native tsgo project info omitted configFilePath",
            )
        })?;
        if config.is_empty() {
            return Ok(TraceProjectContext::Inferred);
        }
        let path = PathBuf::from(config);
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalize native tsgo project config {}", path.display()))
            .map_err(NativeFailure::from)?;
        if !canonical.starts_with(&self.workspace) {
            return Err(NativeFailure::preserved(
                "custom/projectInfo",
                format!(
                    "native tsgo selected config {} outside workspace {}",
                    canonical.display(),
                    self.workspace.display()
                ),
            ));
        }
        Ok(TraceProjectContext::Configured { config: public_file(&self.workspace, &canonical) })
    }

    async fn normalize_trace_item(
        &mut self,
        item: &Value,
        mode: TraceNormalizationMode,
        cache: &mut HashMap<String, NormalizedTraceItem>,
        source_maps: &mut HashMap<PathBuf, std::result::Result<Arc<SourceMap>, String>>,
    ) -> std::result::Result<NormalizedTraceItem, NativeFailure> {
        let (native_id, native_node) =
            trace_node(item, &self.workspace).map_err(NativeFailure::from)?;
        if let Some(cached) = cache.get(&native_id) {
            return Ok(cached.clone());
        }
        let native = NormalizedTraceItem {
            native_id: native_id.clone(),
            id: native_id.clone(),
            node: native_node,
            gap: None,
        };
        if matches!(mode, TraceNormalizationMode::Native)
            || native.node.external
            || !is_declaration_source(&native.node.definition.file)
        {
            cache.insert(native_id, native.clone());
            return Ok(native);
        }

        let unresolved = |reason| unresolved_trace_identity(native.clone(), reason);
        if !self.capabilities.source_definition {
            let normalized = unresolved(TraceIdentityGapReason::SourceDefinitionUnsupported);
            cache.insert(native_id, normalized.clone());
            return Ok(normalized);
        }

        let (declaration_file, line, character) =
            item_location(item).map_err(NativeFailure::from)?;
        let declaration_file = declaration_file.canonicalize().unwrap_or(declaration_file);
        let response = match self
            .request_typed(
                "custom/textDocument/sourceDefinition",
                Some(json!({
                    "textDocument": { "uri": file_uri(&declaration_file)? },
                    "position": { "line": line, "character": character }
                })),
            )
            .await
        {
            Ok(response) => response,
            Err(failure @ NativeFailure::Lost { .. }) => return Err(failure),
            Err(failure) => {
                let normalized = unresolved(TraceIdentityGapReason::NativeRequestFailed {
                    detail: bounded_failure_detail(failure),
                });
                cache.insert(native_id, normalized.clone());
                return Ok(normalized);
            }
        };
        let source_definitions = match decode_lsp_locations(
            &response,
            &self.workspace,
            true,
            "custom/textDocument/sourceDefinition",
        ) {
            Ok(locations) => locations,
            Err(failure @ NativeFailure::Lost { .. }) => return Err(failure),
            Err(failure) => {
                let normalized = unresolved(TraceIdentityGapReason::NativeRequestFailed {
                    detail: bounded_failure_detail(failure),
                });
                cache.insert(native_id, normalized.clone());
                return Ok(normalized);
            }
        };
        let generated = match source_definitions.as_slice() {
            [] => {
                let normalized = unresolved(TraceIdentityGapReason::NoSourceDefinition);
                cache.insert(native_id, normalized.clone());
                return Ok(normalized);
            }
            [location] => location.clone(),
            locations => {
                let normalized = unresolved(TraceIdentityGapReason::AmbiguousSourceDefinition {
                    observed: locations.len(),
                });
                cache.insert(native_id, normalized.clone());
                return Ok(normalized);
            }
        };
        let (source_file, source_line, source_character) =
            match self.canonical_source_location(&generated, source_maps).await {
                Ok(location) => location,
                Err(reason) => {
                    let normalized = unresolved(reason);
                    cache.insert(native_id, normalized.clone());
                    return Ok(normalized);
                }
            };

        let prepared =
            match self.prepare_at_typed(&source_file, source_line, source_character).await {
                Ok(prepared) => prepared,
                Err(failure @ NativeFailure::Lost { .. }) => return Err(failure),
                Err(failure) => {
                    let normalized = unresolved(TraceIdentityGapReason::NativeRequestFailed {
                        detail: bounded_failure_detail(failure),
                    });
                    cache.insert(native_id, normalized.clone());
                    return Ok(normalized);
                }
            };
        let prepared_items = prepared.as_array().ok_or_else(|| {
            NativeFailure::preserved(
                "textDocument/prepareCallHierarchy",
                "native tsgo returned a non-array result while normalizing generated identity",
            )
        })?;
        let expected_name = item.get("name").and_then(Value::as_str);
        let matching = prepared_items
            .iter()
            .filter(|prepared| {
                prepared.get("name").and_then(Value::as_str) == expected_name
                    && item_file(prepared)
                        .and_then(|file| file.canonicalize().ok())
                        .is_some_and(|file| file == source_file)
            })
            .collect::<Vec<_>>();
        let canonical_item = match matching.as_slice() {
            [item] => *item,
            items => {
                let normalized = unresolved(TraceIdentityGapReason::SourcePreparationNotUnique {
                    observed: items.len(),
                });
                cache.insert(native_id, normalized.clone());
                return Ok(normalized);
            }
        };
        let (id, mut node) =
            trace_node(canonical_item, &self.workspace).map_err(NativeFailure::from)?;
        node.generated_aliases.push(native.node.definition.clone());
        if generated != node.definition {
            node.generated_aliases.push(generated);
        }
        node.generated_aliases.sort();
        node.generated_aliases.dedup();
        let normalized = NormalizedTraceItem { native_id: native_id.clone(), id, node, gap: None };
        cache.insert(native_id, normalized.clone());
        Ok(normalized)
    }

    async fn canonical_source_location(
        &self,
        location: &TraceLocation,
        source_maps: &mut HashMap<PathBuf, std::result::Result<Arc<SourceMap>, String>>,
    ) -> std::result::Result<(PathBuf, u32, u32), TraceIdentityGapReason> {
        let generated_file = absolute_trace_file(&self.workspace, &location.file)
            .canonicalize()
            .map_err(|_| TraceIdentityGapReason::SourceOutsideWorkspace)?;
        if !generated_file.starts_with(&self.workspace) {
            return Err(TraceIdentityGapReason::SourceOutsideWorkspace);
        }
        let line = location.line.saturating_sub(1);
        let character = location.character.saturating_sub(1);
        if is_canonical_trace_source(&generated_file) {
            return Ok((generated_file, line, character));
        }

        let file_name =
            generated_file.file_name().and_then(|name| name.to_str()).ok_or_else(|| {
                TraceIdentityGapReason::SourceMapInvalid {
                    detail: "generated source path has no UTF-8 file name".to_owned(),
                }
            })?;
        let source_map_file = generated_file.with_file_name(format!("{file_name}.map"));
        let parsed = if let Some(parsed) = source_maps.get(&source_map_file) {
            parsed.clone()
        } else {
            let parsed = read_source_map(&source_map_file).await;
            source_maps.insert(source_map_file.clone(), parsed.clone());
            parsed
        };
        let source_map = parsed.map_err(|detail| {
            if detail == "missing" {
                TraceIdentityGapReason::SourceMapMissing
            } else {
                TraceIdentityGapReason::SourceMapInvalid { detail }
            }
        })?;
        let (mapped_source, mapped_line, mapped_character) = source_map
            .original_for(line, character)
            .ok_or(TraceIdentityGapReason::SourcePositionUnmapped)?;
        let unresolved_source = if mapped_source.starts_with("file://") {
            uri_file_path(mapped_source).map_err(|error| {
                TraceIdentityGapReason::SourceMapInvalid { detail: bounded_failure_detail(error) }
            })?
        } else {
            let mapped = PathBuf::from(mapped_source);
            if mapped.is_absolute() {
                mapped
            } else {
                source_map_file.parent().unwrap_or(&self.workspace).join(mapped)
            }
        };
        let source_file = unresolved_source.canonicalize().map_err(|error| {
            TraceIdentityGapReason::SourceMapInvalid { detail: bounded_failure_detail(error) }
        })?;
        if !source_file.starts_with(&self.workspace) {
            return Err(TraceIdentityGapReason::SourceOutsideWorkspace);
        }
        if !is_canonical_trace_source(&source_file) {
            return Err(TraceIdentityGapReason::SourceMapInvalid {
                detail: format!(
                    "mapped source {} is not a TypeScript source file",
                    public_file(&self.workspace, &source_file).display()
                ),
            });
        }
        Ok((source_file, mapped_line, mapped_character))
    }

    async fn diagnose(
        &mut self,
        request: DiagnoseRequest,
    ) -> std::result::Result<Value, NativeFailure> {
        let files = request
            .files
            .iter()
            .map(|relative| super::canonical_locus_file(&self.workspace, relative))
            .collect::<Result<Vec<_>>>()?;
        let requested = files.iter().cloned().collect::<BTreeSet<_>>();
        let reusable = matches!(
            &self.document_mode,
            OpenDocumentMode::Diagnostics { files } if files == &requested
        );
        if !reusable {
            self.close_all_documents_typed().await?;
            self.document_mode = OpenDocumentMode::Diagnostics { files: requested };
        }
        self.diagnose_opened(files).await
    }

    async fn diagnose_opened(
        &mut self,
        files: Vec<PathBuf>,
    ) -> std::result::Result<Value, NativeFailure> {
        if !self.capabilities.diagnostics.supported {
            return Err(NativeFailure::preserved(
                "textDocument/diagnostic",
                "native tsgo did not advertise pull diagnostic support",
            ));
        }
        let started = Instant::now();
        let request_start = self.next_request_id;
        let mut captured = Vec::with_capacity(files.len());
        let mut total_source_bytes = 0u64;
        for file in files {
            self.synchronize_document_typed(&file).await?;
            let document = self.documents.get(&file).ok_or_else(|| {
                NativeFailure::preserved(
                    "textDocument synchronization",
                    format!("document state is missing for {}", file.display()),
                )
            })?;
            total_source_bytes = total_source_bytes.saturating_add(document.text.len() as u64);
            if total_source_bytes > MAX_DIAGNOSE_TOTAL_SOURCE_BYTES {
                return Err(NativeFailure::preserved(
                    "textDocument synchronization",
                    format!(
                        "diagnose source set exceeds {} MiB",
                        MAX_DIAGNOSE_TOTAL_SOURCE_BYTES / (1024 * 1024)
                    ),
                ));
            }
            captured.push((file, sha256_hex(document.text.as_bytes()), document.version));
        }

        let mut documents = Vec::with_capacity(captured.len());
        let mut parts = Vec::with_capacity(captured.len());
        for (file, sha256, version) in &captured {
            let project = self.diagnose_project(file).await?;
            let report = self
                .request_typed(
                    "textDocument/diagnostic",
                    Some(json!({ "textDocument": { "uri": file_uri(file)? } })),
                )
                .await?;
            let normalized =
                diagnostics::normalize_document_report(&report, file, &self.workspace)?;
            documents.push(DiagnosedDocument {
                file: public_file(&self.workspace, file),
                sha256: sha256.clone(),
                server_document_version: *version,
                selected_project: project,
                diagnostics: normalized.summary.total,
            });
            parts.push(normalized);
        }
        let normalized = diagnostics::merge(parts);
        let changed = recheck_diagnostic_documents(&self.workspace, &captured).await;
        let mut incomplete = Vec::new();
        if !changed.is_empty() {
            incomplete.push(DiagnoseIncompleteReason::ChangedInput);
        }
        if normalized.summary.omitted > 0 {
            incomplete.push(DiagnoseIncompleteReason::DiagnosticLimit {
                observed: normalized.summary.total,
                retained: normalized.summary.returned,
            });
        }
        if normalized.summary.truncated_details > 0 {
            incomplete.push(DiagnoseIncompleteReason::DiagnosticDetailLimit {
                omitted: normalized.summary.truncated_details,
            });
        }
        if normalized.summary.unspecified > 0 {
            incomplete.push(DiagnoseIncompleteReason::UnspecifiedSeverity {
                diagnostics: normalized.summary.unspecified,
            });
        }
        if normalized.summary.unknown > 0 {
            incomplete.push(DiagnoseIncompleteReason::UnknownSeverity {
                diagnostics: normalized.summary.unknown,
            });
        }
        let verdict = if !incomplete.is_empty() {
            DiagnoseVerdict::Incomplete { reasons: incomplete }
        } else if normalized.summary.total == 0 {
            DiagnoseVerdict::NoLocalDiagnostics
        } else {
            DiagnoseVerdict::LocalDiagnostics
        };
        let requested_document_freshness = if changed.is_empty() {
            RequestedDocumentFreshness::Verified
        } else {
            RequestedDocumentFreshness::Changed { files: changed }
        };
        let result = DiagnoseResult {
            schema: DIAGNOSTIC_SCHEMA_VERSION,
            authority: DiagnosticAuthority::LanguageService,
            verdict,
            documents,
            diagnostics: normalized.items,
            summary: normalized.summary,
            completeness: DiagnoseCompleteness {
                requested_documents: captured.len(),
                completed_documents: captured.len(),
                inter_file_dependencies: self.capabilities.diagnostics.inter_file_dependencies,
                workspace_diagnostics: self.capabilities.diagnostics.workspace_diagnostics,
                project_contexts: DiagnosticProjectContexts::SelectedOnly,
                dependency_freshness: DiagnosticDependencyFreshness::Unchecked,
            },
            requested_document_freshness,
            timing: DiagnoseTiming {
                elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                native_requests: self.next_request_id.saturating_sub(request_start),
            },
        };
        serde_json::to_value(result)
            .context("encode typed tsgo diagnose result")
            .map_err(NativeFailure::from)
    }

    async fn diagnose_project(
        &mut self,
        file: &Path,
    ) -> std::result::Result<DiagnoseProject, NativeFailure> {
        let result = self
            .request_typed(
                "custom/projectInfo",
                Some(json!({ "textDocument": { "uri": file_uri(file)? } })),
            )
            .await?;
        let config = result.get("configFilePath").and_then(Value::as_str).ok_or_else(|| {
            NativeFailure::preserved(
                "custom/projectInfo",
                "native tsgo project info omitted configFilePath",
            )
        })?;
        if config.is_empty() {
            return Ok(DiagnoseProject::Inferred);
        }
        let path = PathBuf::from(config);
        let unresolved = if path.is_absolute() { path } else { self.workspace.join(path) };
        let canonical = unresolved.canonicalize().with_context(|| {
            format!("canonicalize native tsgo project config {}", unresolved.display())
        })?;
        if !canonical.starts_with(&self.workspace) {
            return Err(NativeFailure::preserved(
                "custom/projectInfo",
                format!(
                    "native tsgo selected config {} outside workspace {}",
                    canonical.display(),
                    self.workspace.display()
                ),
            ));
        }
        Ok(DiagnoseProject::Configured { config: public_file(&self.workspace, &canonical) })
    }

    async fn locus(&mut self, request: LocusRequest) -> Result<LocusExecution> {
        locus::validate_request(&request)?;
        if self.active_locus_capture.is_some() {
            bail!("native tsgo already has an active locus capture");
        }
        self.enter_semantic_document_mode().await?;
        let capture_id = self.next_locus_capture;
        self.next_locus_capture =
            self.next_locus_capture.checked_add(1).context("locus capture generation overflow")?;
        self.active_locus_capture = Some(capture_id);
        self.locus_observations.clear();

        let started = Instant::now();
        let request_start = self.next_request_id;
        let native_capture = self.acquire_locus_case(&request).await;
        let observed = self.observed_documents();
        self.active_locus_capture = None;
        let native_capture = match native_capture {
            Ok(capture) => capture,
            Err(failure) if failure.integrity() == LocusSessionIntegrity::Lost => {
                let _ = self.finish(false).await;
                return Err(anyhow::Error::new(failure));
            }
            Err(failure) => return Err(anyhow::Error::new(failure)),
        };
        let freshness = recheck_observed_documents(&observed).await;
        let fingerprint =
            locus_fingerprint(&request, &self.child_identity.server_version, &observed)?;
        let capture = LocusCapture {
            seeds: native_capture.seeds,
            acquisitions: native_capture.acquisitions,
            evidence: native_capture.evidence,
            supplied_candidates: native_capture.supplied_candidates,
            freshness,
            fingerprint,
            timing: LocusTiming {
                elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                native_requests: self.next_request_id.saturating_sub(request_start),
            },
        };
        let result = match locus::evaluate(request, capture) {
            Ok(result) => result,
            Err(error) if native_capture.session_integrity_lost => {
                let _ = self.finish(false).await;
                return Err(
                    error.context("validate locus capture after native session integrity was lost")
                );
            }
            Err(error) => return Err(error),
        };
        Ok(LocusExecution {
            result: serde_json::to_value(result).context("encode typed tsgo locus result")?,
            session_integrity_lost: native_capture.session_integrity_lost,
        })
    }

    async fn acquire_locus_case(
        &mut self,
        request: &LocusRequest,
    ) -> std::result::Result<NativeLocusCapture, NativeFailure> {
        let supplied_candidates = self.capture_supplied_candidates(request).await?;
        let mut seeds = Vec::with_capacity(request.seeds.len());
        let mut resolved = BTreeMap::<String, ResolvedLocusSeed>::new();
        let mut lost_reason = None::<String>;
        let mut remaining_ambiguity_candidates = MAX_LOCUS_TOTAL_AMBIGUITY_CANDIDATES;

        for seed in &request.seeds {
            if let Some(reason) = &lost_reason {
                seeds.push(LocusSeedResult::Failed {
                    seed_id: seed.id.clone(),
                    label: seed.label.clone(),
                    reason: format!("not attempted after session integrity was lost: {reason}"),
                    session_integrity: LocusSessionIntegrity::Lost,
                    discovery: TraceDiscovery::default(),
                });
                continue;
            }
            match self.resolve_locus_seed(seed, &mut remaining_ambiguity_candidates).await {
                Ok((result, resolved_seed)) => {
                    if let Some(resolved_seed) = resolved_seed {
                        resolved.insert(seed.id.clone(), resolved_seed);
                    }
                    seeds.push(result);
                }
                Err(failure) => {
                    let integrity = failure.integrity();
                    let reason = failure.to_string();
                    if integrity == LocusSessionIntegrity::Lost {
                        lost_reason = Some(reason.clone());
                    }
                    seeds.push(LocusSeedResult::Failed {
                        seed_id: seed.id.clone(),
                        label: seed.label.clone(),
                        reason,
                        session_integrity: integrity,
                        discovery: TraceDiscovery::default(),
                    });
                }
            }
        }

        let mut acquisitions = Vec::with_capacity(request.acquisitions.len());
        let mut evidence = Vec::new();
        let mut next_evidence_id = 1usize;
        let mut remaining_evidence = MAX_LOCUS_TOTAL_EVIDENCE;
        let mut remaining_call_sites = MAX_LOCUS_TOTAL_CALL_SITES;
        for acquisition in &request.acquisitions {
            let outcome = if let Some(reason) = &lost_reason {
                Err(NativeFailure::lost(
                    "locus acquisition",
                    format!("not attempted after session integrity was lost: {reason}"),
                ))
            } else if let Some(seed) = resolved.get(&acquisition.seed_id) {
                self.acquire_locus_evidence(
                    acquisition,
                    seed,
                    &mut next_evidence_id,
                    &mut remaining_evidence,
                    &mut remaining_call_sites,
                    &mut remaining_ambiguity_candidates,
                )
                .await
            } else {
                Err(NativeFailure::preserved(
                    "locus acquisition",
                    format!("seed {} did not resolve", acquisition.seed_id),
                ))
            };

            let acquired = match outcome {
                Ok(acquired) => acquired,
                Err(failure) if failure.method_not_found() => AcquiredLocusEvidence {
                    state: LocusAcquisitionState::Unsupported { reason: failure.to_string() },
                    evidence: Vec::new(),
                    prepare: None,
                },
                Err(failure) => {
                    let integrity = failure.integrity();
                    let reason = failure.to_string();
                    if integrity == LocusSessionIntegrity::Lost {
                        lost_reason = Some(reason.clone());
                    }
                    AcquiredLocusEvidence {
                        state: LocusAcquisitionState::Failed {
                            reason,
                            session_integrity: integrity,
                        },
                        evidence: Vec::new(),
                        prepare: None,
                    }
                }
            };
            let evidence_ids =
                acquired.evidence.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
            evidence.extend(acquired.evidence);
            acquisitions.push(LocusAcquisitionResult {
                id: acquisition.id.clone(),
                seed_id: acquisition.seed_id.clone(),
                required: acquisition.required,
                accept_no_call_item: acquisition.accept_no_call_item,
                operation: acquisition.operation.clone(),
                prepare: acquired.prepare,
                state: acquired.state,
                evidence_ids,
            });
        }

        for acquisition in &acquisitions {
            if let LocusAcquisitionState::AmbiguousCallItem { candidates, observed } =
                &acquisition.state
            {
                if let Some(seed) =
                    seeds.iter().find(|seed| seed.seed_id() == acquisition.seed_id).cloned()
                {
                    let (label, anchor, discovery) = match seed {
                        LocusSeedResult::Resolved { label, anchor, discovery, .. } => {
                            (label, anchor, discovery)
                        }
                        _ => continue,
                    };
                    if let Some(index) =
                        seeds.iter().position(|seed| seed.seed_id() == acquisition.seed_id)
                    {
                        seeds[index] = LocusSeedResult::AmbiguousCallItem {
                            seed_id: acquisition.seed_id.clone(),
                            label,
                            anchor,
                            acquisition_id: acquisition.id.clone(),
                            candidates: candidates.clone(),
                            observed: *observed,
                            discovery,
                        };
                    }
                }
            }
        }

        Ok(NativeLocusCapture {
            seeds,
            acquisitions,
            evidence,
            supplied_candidates,
            session_integrity_lost: lost_reason.is_some(),
        })
    }

    async fn capture_supplied_candidates(
        &mut self,
        request: &LocusRequest,
    ) -> std::result::Result<Vec<LocusCapturedCandidate>, NativeFailure> {
        let mut captured = Vec::with_capacity(request.supplied_candidates.len());
        for candidate in &request.supplied_candidates {
            let file = super::canonical_locus_file(&self.workspace, &candidate.position.file)?;
            self.synchronize_document_typed(&file).await?;
            self.validate_document_position(
                &file,
                candidate.position.line,
                candidate.position.character,
            )?;
            captured.push(LocusCapturedCandidate {
                request_id: candidate.id.clone(),
                label: candidate.label.clone(),
                anchor: LocusAnchor {
                    label: candidate.label.clone(),
                    location: public_location(
                        &self.workspace,
                        file,
                        candidate.position.line,
                        candidate.position.character,
                    ),
                    external: false,
                },
            });
        }
        Ok(captured)
    }

    async fn resolve_locus_seed(
        &mut self,
        seed: &super::protocol::LocusSeed,
        remaining_ambiguity_candidates: &mut usize,
    ) -> std::result::Result<(LocusSeedResult, Option<ResolvedLocusSeed>), NativeFailure> {
        match &seed.selector {
            TraceSelector::Position { file, line, character } => {
                let file = super::canonical_locus_file(&self.workspace, file)?;
                self.synchronize_document_typed(&file).await?;
                self.validate_document_position(&file, *line, *character)?;
                let anchor = LocusAnchor {
                    label: seed.label.clone(),
                    location: public_location(&self.workspace, file.clone(), *line, *character),
                    external: false,
                };
                Ok((
                    LocusSeedResult::Resolved {
                        seed_id: seed.id.clone(),
                        label: seed.label.clone(),
                        anchor: anchor.clone(),
                        discovery: TraceDiscovery::default(),
                    },
                    Some(ResolvedLocusSeed { file, line: *line, character: *character, anchor }),
                ))
            }
            TraceSelector::Symbol { query, scope } => {
                let leaf = symbol_leaf(query)?;
                let response =
                    self.request_typed("workspace/symbol", Some(json!({ "query": leaf }))).await?;
                let mut symbols = response
                    .as_array()
                    .ok_or_else(|| {
                        NativeFailure::preserved(
                            "workspace/symbol",
                            "native tsgo returned a non-array workspace symbol result",
                        )
                    })?
                    .iter()
                    .filter(|symbol| workspace_symbol_matches(symbol, query, scope.as_deref()))
                    .cloned()
                    .collect::<Vec<_>>();
                symbols.sort_by_key(workspace_symbol_sort_key);
                let mut discovery = TraceDiscovery::default();
                let observed = symbols.len();
                match symbols.len() {
                    0 => Ok((
                        LocusSeedResult::NotFound {
                            seed_id: seed.id.clone(),
                            label: seed.label.clone(),
                            discovery,
                        },
                        None,
                    )),
                    1 => {
                        let symbol = &symbols[0];
                        let file = workspace_symbol_file(symbol)?;
                        if !file.starts_with(&self.workspace) {
                            return Err(NativeFailure::preserved(
                                "workspace/symbol",
                                format!(
                                    "resolved file {} is outside workspace {}",
                                    file.display(),
                                    self.workspace.display()
                                ),
                            ));
                        }
                        self.synchronize_document_typed(&file).await?;
                        let (line, fallback_character) = workspace_symbol_position(symbol)?;
                        let name = symbol.get("name").and_then(Value::as_str).ok_or_else(|| {
                            NativeFailure::preserved(
                                "workspace/symbol",
                                "workspace symbol omitted its name",
                            )
                        })?;
                        let character = self
                            .declaration_character(&file, line, name)
                            .unwrap_or(fallback_character);
                        let anchor = LocusAnchor {
                            label: seed.label.clone(),
                            location: public_location(
                                &self.workspace,
                                file.clone(),
                                line,
                                character,
                            ),
                            external: false,
                        };
                        Ok((
                            LocusSeedResult::Resolved {
                                seed_id: seed.id.clone(),
                                label: seed.label.clone(),
                                anchor: anchor.clone(),
                                discovery,
                            },
                            Some(ResolvedLocusSeed { file, line, character, anchor }),
                        ))
                    }
                    _ => {
                        let retained_limit =
                            MAX_LOCUS_CANDIDATES.min(*remaining_ambiguity_candidates);
                        if symbols.len() > retained_limit {
                            symbols.truncate(retained_limit);
                            discovery.truncated = true;
                        }
                        let candidates = symbols
                            .iter()
                            .map(|symbol| locus_seed_candidate(symbol, &self.workspace))
                            .collect::<Result<Vec<_>>>()?;
                        *remaining_ambiguity_candidates =
                            remaining_ambiguity_candidates.saturating_sub(candidates.len());
                        Ok((
                            LocusSeedResult::Ambiguous {
                                seed_id: seed.id.clone(),
                                label: seed.label.clone(),
                                candidates,
                                observed,
                                discovery,
                            },
                            None,
                        ))
                    }
                }
            }
        }
    }

    async fn acquire_locus_evidence(
        &mut self,
        acquisition: &LocusAcquisition,
        seed: &ResolvedLocusSeed,
        next_evidence_id: &mut usize,
        remaining_evidence: &mut usize,
        remaining_call_sites: &mut usize,
        remaining_ambiguity_candidates: &mut usize,
    ) -> std::result::Result<AcquiredLocusEvidence, NativeFailure> {
        match &acquisition.operation {
            LocusOperation::Definition { max_results } => {
                if !self.capabilities.definition {
                    return Ok(unsupported_acquisition(
                        "native tsgo did not advertise definition support",
                    ));
                }
                self.acquire_locations(
                    acquisition,
                    seed,
                    "textDocument/definition",
                    json!({
                        "textDocument": { "uri": file_uri(&seed.file)? },
                        "position": { "line": seed.line, "character": seed.character }
                    }),
                    *max_results,
                    true,
                    next_evidence_id,
                    remaining_evidence,
                )
                .await
            }
            LocusOperation::References { include_declaration, max_results } => {
                if !self.capabilities.references {
                    return Ok(unsupported_acquisition(
                        "native tsgo did not advertise references support",
                    ));
                }
                self.acquire_locations(
                    acquisition,
                    seed,
                    "textDocument/references",
                    json!({
                        "textDocument": { "uri": file_uri(&seed.file)? },
                        "position": { "line": seed.line, "character": seed.character },
                        "context": { "includeDeclaration": include_declaration }
                    }),
                    *max_results,
                    false,
                    next_evidence_id,
                    remaining_evidence,
                )
                .await
            }
            LocusOperation::Implementations { max_results } => {
                if !self.capabilities.implementations {
                    return Ok(unsupported_acquisition(
                        "native tsgo did not advertise implementation support",
                    ));
                }
                self.acquire_locations(
                    acquisition,
                    seed,
                    "textDocument/implementation",
                    json!({
                        "textDocument": { "uri": file_uri(&seed.file)? },
                        "position": { "line": seed.line, "character": seed.character }
                    }),
                    *max_results,
                    true,
                    next_evidence_id,
                    remaining_evidence,
                )
                .await
            }
            LocusOperation::IncomingCalls { limits } => {
                self.acquire_calls(
                    acquisition,
                    seed,
                    TraceDirection::Callers,
                    *limits,
                    next_evidence_id,
                    remaining_evidence,
                    remaining_call_sites,
                    remaining_ambiguity_candidates,
                )
                .await
            }
            LocusOperation::OutgoingCalls { limits } => {
                self.acquire_calls(
                    acquisition,
                    seed,
                    TraceDirection::Callees,
                    *limits,
                    next_evidence_id,
                    remaining_evidence,
                    remaining_call_sites,
                    remaining_ambiguity_candidates,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn acquire_locations(
        &mut self,
        acquisition: &LocusAcquisition,
        seed: &ResolvedLocusSeed,
        method: &str,
        params: Value,
        max_results: usize,
        allow_single: bool,
        next_evidence_id: &mut usize,
        remaining_evidence: &mut usize,
    ) -> std::result::Result<AcquiredLocusEvidence, NativeFailure> {
        let response = self.request_typed(method, Some(params)).await?;
        let mut anchors = decode_lsp_locations(&response, &self.workspace, allow_single, method)?
            .into_iter()
            .map(|location| LocusAnchor {
                label: bounded_utf8(
                    format!(
                        "{} from {}",
                        acquisition.operation.relation().label(),
                        seed.anchor.label
                    ),
                    MAX_LOCUS_LABEL_BYTES,
                ),
                external: location.file.is_absolute(),
                location,
            })
            .collect::<Vec<_>>();
        anchors.sort();
        let observed = anchors.len();
        anchors.truncate(max_results.min(*remaining_evidence));
        let cut = observed.saturating_sub(anchors.len());
        let capture = if cut == 0 {
            LocusEvidenceCapture::CompleteWithinCapture
        } else {
            LocusEvidenceCapture::RetainedBeforeCut
        };
        let mut evidence = Vec::with_capacity(anchors.len());
        for target in anchors {
            evidence.push(LocusEvidence {
                id: take_evidence_id(next_evidence_id)?,
                acquisition_id: acquisition.id.clone(),
                seed_id: acquisition.seed_id.clone(),
                relation: acquisition.operation.relation(),
                source: seed.anchor.clone(),
                target,
                call_sites: Vec::new(),
                capture,
            });
        }
        *remaining_evidence = remaining_evidence.saturating_sub(evidence.len());
        let state = if cut == 0 {
            LocusAcquisitionState::CompleteWithinCapture { retained: evidence.len() }
        } else {
            LocusAcquisitionState::Cut {
                retained: evidence.len(),
                cuts: vec![LocusCaptureCut {
                    reason: LocusCutReason::MaxResults,
                    omission: LocusOmission::Known { count: cut },
                }],
            }
        };
        Ok(AcquiredLocusEvidence { state, evidence, prepare: None })
    }

    async fn acquire_calls(
        &mut self,
        acquisition: &LocusAcquisition,
        seed: &ResolvedLocusSeed,
        direction: TraceDirection,
        limits: TraceLimits,
        next_evidence_id: &mut usize,
        remaining_evidence: &mut usize,
        remaining_call_sites: &mut usize,
        remaining_ambiguity_candidates: &mut usize,
    ) -> std::result::Result<AcquiredLocusEvidence, NativeFailure> {
        if !self.capabilities.call_hierarchy {
            return Ok(unsupported_acquisition(
                "native tsgo did not advertise call hierarchy support",
            ));
        }
        let prepared = self.prepare_at_typed(&seed.file, seed.line, seed.character).await?;
        let prepared_items = match prepared.as_array() {
            Some(items) => items,
            None if prepared.is_null() => {
                return Ok(AcquiredLocusEvidence {
                    state: LocusAcquisitionState::NoCallItem,
                    evidence: Vec::new(),
                    prepare: None,
                });
            }
            None => {
                return Err(NativeFailure::preserved(
                    "textDocument/prepareCallHierarchy",
                    "native tsgo returned a non-array result",
                ));
            }
        };
        if prepared_items.is_empty() {
            return Ok(AcquiredLocusEvidence {
                state: LocusAcquisitionState::NoCallItem,
                evidence: Vec::new(),
                prepare: None,
            });
        }
        if prepared_items.len() != 1 {
            let observed = prepared_items.len();
            let retained_limit = MAX_LOCUS_CANDIDATES.min(*remaining_ambiguity_candidates / 2);
            let candidates = prepared_items
                .iter()
                .take(retained_limit)
                .map(|item| locus_call_item_candidate(item, &self.workspace))
                .collect::<Result<Vec<_>>>()?;
            *remaining_ambiguity_candidates =
                remaining_ambiguity_candidates.saturating_sub(candidates.len().saturating_mul(2));
            return Ok(AcquiredLocusEvidence {
                state: LocusAcquisitionState::AmbiguousCallItem { candidates, observed },
                evidence: Vec::new(),
                prepare: None,
            });
        }

        let trace = empty_trace_result(
            seed.anchor.label.clone(),
            direction,
            Vec::new(),
            TraceDiscovery::default(),
            TraceScopeReceipt::default(),
        );
        let mut trace = self
            .traverse_typed(
                prepared_items[0].clone(),
                trace,
                direction,
                limits,
                TraceScope::default(),
                TraceNormalizationMode::Native,
            )
            .await?;
        let semantic_root = trace
            .target
            .as_ref()
            .and_then(|target| trace.nodes.get(target))
            .map(locus_anchor_from_trace_node)
            .ok_or_else(|| {
                NativeFailure::preserved(
                    "call hierarchy normalization",
                    "normalized trace omitted its prepared root",
                )
            })?;
        let mut cuts = trace
            .boundaries
            .iter()
            .map(|boundary| LocusCaptureCut {
                reason: match boundary.kind {
                    TraceBoundaryKind::External
                    | TraceBoundaryKind::SourceRoot
                    | TraceBoundaryKind::Package => LocusCutReason::ExternalBoundary,
                    TraceBoundaryKind::MaxDepth => LocusCutReason::MaxDepth,
                    TraceBoundaryKind::MaxNodes | TraceBoundaryKind::MaxNativeVariants => {
                        LocusCutReason::MaxNodes
                    }
                    TraceBoundaryKind::MaxRelations => LocusCutReason::MaxResults,
                    TraceBoundaryKind::MaxCallSites => LocusCutReason::MaxCallSites,
                },
                omission: if matches!(
                    boundary.kind,
                    TraceBoundaryKind::External
                        | TraceBoundaryKind::SourceRoot
                        | TraceBoundaryKind::Package
                ) {
                    LocusOmission::Unknown
                } else {
                    LocusOmission::Known { count: boundary.omitted_relations }
                },
            })
            .collect::<Vec<_>>();

        let observed_edge_count = trace.edges.len();
        let trace_edges = std::mem::take(&mut trace.edges);
        let mut edges = connected_edge_prefix(
            trace_edges,
            &trace.nodes,
            &semantic_root.location,
            direction,
            super::protocol::MAX_LOCUS_LOCATIONS.min(*remaining_evidence),
        );
        let edge_limit = super::protocol::MAX_LOCUS_LOCATIONS.min(*remaining_evidence);
        let omitted_edges = observed_edge_count.saturating_sub(edges.len());
        edges.truncate(edge_limit);
        if omitted_edges > 0 {
            cuts.push(LocusCaptureCut {
                reason: LocusCutReason::MaxResults,
                omission: LocusOmission::Known { count: omitted_edges },
            });
        }
        let mut omitted_sites = 0usize;
        let mut remaining_sites = *remaining_call_sites;
        for edge in &mut edges {
            let retained = edge.call_sites.len().min(remaining_sites);
            omitted_sites += edge.call_sites.len().saturating_sub(retained);
            edge.call_sites.truncate(retained);
            remaining_sites = remaining_sites.saturating_sub(retained);
        }
        if omitted_sites > 0 {
            cuts.push(LocusCaptureCut {
                reason: LocusCutReason::MaxCallSites,
                omission: LocusOmission::Known { count: omitted_sites },
            });
        }
        *remaining_call_sites = remaining_sites;
        let cuts = merge_locus_cuts(cuts);
        let capture = if cuts.is_empty() {
            LocusEvidenceCapture::CompleteWithinCapture
        } else {
            LocusEvidenceCapture::RetainedBeforeCut
        };
        let mut evidence = Vec::with_capacity(edges.len());
        for edge in edges {
            let caller = trace.nodes.get(&edge.caller).ok_or_else(|| {
                NativeFailure::preserved(
                    "call hierarchy normalization",
                    format!("call edge omitted caller node {}", edge.caller),
                )
            })?;
            let callee = trace.nodes.get(&edge.callee).ok_or_else(|| {
                NativeFailure::preserved(
                    "call hierarchy normalization",
                    format!("call edge omitted callee node {}", edge.callee),
                )
            })?;
            evidence.push(LocusEvidence {
                id: take_evidence_id(next_evidence_id)?,
                acquisition_id: acquisition.id.clone(),
                seed_id: acquisition.seed_id.clone(),
                relation: acquisition.operation.relation(),
                source: locus_anchor_from_trace_node(caller),
                target: locus_anchor_from_trace_node(callee),
                call_sites: edge.call_sites,
                capture,
            });
        }
        *remaining_evidence = remaining_evidence.saturating_sub(evidence.len());
        let state = if cuts.is_empty() {
            LocusAcquisitionState::CompleteWithinCapture { retained: evidence.len() }
        } else {
            LocusAcquisitionState::Cut { retained: evidence.len(), cuts }
        };
        Ok(AcquiredLocusEvidence {
            state,
            evidence,
            prepare: Some(LocusPrepareReceipt { query_anchor: seed.anchor.clone(), semantic_root }),
        })
    }

    fn observed_documents(&self) -> Vec<ObservedDocument> {
        let mut documents = self
            .locus_observations
            .iter()
            .map(|(file, observation)| ObservedDocument {
                absolute: file.clone(),
                public: file.strip_prefix(&self.workspace).unwrap_or(file).to_path_buf(),
                sha256: observation.first_sha256.clone(),
            })
            .collect::<Vec<_>>();
        documents.sort_by(|left, right| left.public.cmp(&right.public));
        documents
    }

    async fn resolve_selector(
        &mut self,
        selector: &TraceSelector,
    ) -> Result<(Value, Vec<TraceCandidate>, TraceDiscovery)> {
        match selector {
            TraceSelector::Position { file, line, character } => {
                let file = file
                    .canonicalize()
                    .with_context(|| format!("canonicalize TypeScript file {}", file.display()))?;
                if !file.starts_with(&self.workspace) {
                    bail!("{} is outside workspace {}", file.display(), self.workspace.display());
                }
                let prepared = self.prepare_at(&file, *line, *character).await?;
                let candidates = prepared
                    .as_array()
                    .context("native tsgo returned a non-array call hierarchy preparation result")?
                    .iter()
                    .map(|item| trace_candidate(item, &self.workspace))
                    .collect::<Result<Vec<_>>>()?;
                Ok((prepared, candidates, TraceDiscovery::default()))
            }
            TraceSelector::Symbol { query, scope } => {
                let mut discovery = TraceDiscovery::default();
                let mut symbols = self.semantic_symbols(query, scope.as_deref()).await?;
                if symbols.is_empty() {
                    let workspace = self.workspace.clone();
                    let scan_root = scope.clone().unwrap_or_else(|| workspace.clone());
                    let needle = symbol_leaf(query)?.to_owned();
                    let found = tokio::task::spawn_blocking(move || {
                        discover_candidate_files(&workspace, &scan_root, &needle)
                    })
                    .await
                    .context("join TypeScript symbol discovery")??;
                    discovery.scanned_files = found.scanned_files;
                    discovery.truncated = found.truncated;
                    for file in found.files {
                        self.synchronize_document(&file).await?;
                        discovery.activated_files += 1;
                    }
                    symbols = self.semantic_symbols(query, scope.as_deref()).await?;
                }

                let semantic_candidates = symbols
                    .iter()
                    .map(|symbol| workspace_symbol_candidate(symbol, &self.workspace))
                    .collect::<Result<Vec<_>>>()?;
                if symbols.len() != 1 {
                    return Ok((Value::Array(symbols), semantic_candidates, discovery));
                }

                let symbol = &symbols[0];
                let file = workspace_symbol_file(symbol)?;
                self.synchronize_document(&file).await?;
                let (line, fallback_character) = workspace_symbol_position(symbol)?;
                let name = symbol
                    .get("name")
                    .and_then(Value::as_str)
                    .context("workspace symbol omitted its name")?;
                let character =
                    self.declaration_character(&file, line, name).unwrap_or(fallback_character);
                let prepared = self.prepare_at(&file, line, character).await?;
                let prepared_items = prepared.as_array().context(
                    "native tsgo returned a non-array call hierarchy preparation result",
                )?;
                let candidates = if prepared_items.len() > 1 {
                    prepared_items
                        .iter()
                        .map(|item| trace_candidate(item, &self.workspace))
                        .collect::<Result<Vec<_>>>()?
                } else {
                    semantic_candidates
                };
                Ok((prepared, candidates, discovery))
            }
        }
    }

    async fn semantic_symbols(&mut self, query: &str, scope: Option<&Path>) -> Result<Vec<Value>> {
        let leaf = symbol_leaf(query)?;
        let response = self.request("workspace/symbol", Some(json!({ "query": leaf }))).await?;
        let mut matches = response
            .as_array()
            .context("native tsgo returned a non-array workspace symbol result")?
            .iter()
            .filter(|symbol| workspace_symbol_matches(symbol, query, scope))
            .cloned()
            .collect::<Vec<_>>();
        matches.sort_by_key(workspace_symbol_sort_key);
        Ok(matches)
    }

    async fn prepare_at(&mut self, file: &Path, line: u32, character: u32) -> Result<Value> {
        self.prepare_at_typed(file, line, character).await.map_err(anyhow::Error::new)
    }

    async fn prepare_at_typed(
        &mut self,
        file: &Path,
        line: u32,
        character: u32,
    ) -> std::result::Result<Value, NativeFailure> {
        self.synchronize_document_typed(file).await?;
        self.request_typed(
            "textDocument/prepareCallHierarchy",
            Some(json!({
                "textDocument": { "uri": file_uri(file)? },
                "position": { "line": line, "character": character }
            })),
        )
        .await
    }

    fn declaration_character(&self, file: &Path, line: u32, name: &str) -> Option<u32> {
        let text = &self.documents.get(file)?.text;
        let source_line = text.lines().nth(usize::try_from(line).ok()?)?;
        identifier_offsets(source_line, name)
            .next()
            .and_then(|offset| source_line[..offset].encode_utf16().count().try_into().ok())
    }

    fn validate_document_position(
        &self,
        file: &Path,
        line: u32,
        character: u32,
    ) -> std::result::Result<(), NativeFailure> {
        let document = self.documents.get(file).ok_or_else(|| {
            NativeFailure::preserved(
                "position validation",
                format!("synchronized document {} was not retained", file.display()),
            )
        })?;
        let source_line = document.text.split('\n').nth(line as usize).ok_or_else(|| {
            NativeFailure::preserved(
                "position validation",
                format!("line {line} is outside {}", file.display()),
            )
        })?;
        let source_line = source_line.strip_suffix('\r').unwrap_or(source_line);
        let maximum = source_line.encode_utf16().count();
        if character as usize > maximum {
            return Err(NativeFailure::preserved(
                "position validation",
                format!(
                    "UTF-16 character {character} exceeds line {line} length {maximum} in {}",
                    file.display()
                ),
            ));
        }
        Ok(())
    }

    async fn traverse(
        &mut self,
        root_item: Value,
        result: TraceResult,
        direction: TraceDirection,
        limits: TraceLimits,
        scope: TraceScope,
        normalization: TraceNormalizationMode,
    ) -> Result<TraceResult> {
        self.traverse_typed(root_item, result, direction, limits, scope, normalization)
            .await
            .map_err(anyhow::Error::new)
    }

    async fn traverse_typed(
        &mut self,
        root_item: Value,
        mut result: TraceResult,
        direction: TraceDirection,
        limits: TraceLimits,
        scope: TraceScope,
        normalization: TraceNormalizationMode,
    ) -> std::result::Result<TraceResult, NativeFailure> {
        let mut normalization_cache = HashMap::new();
        let mut source_maps = HashMap::new();
        let root = self
            .normalize_trace_item(
                &root_item,
                normalization,
                &mut normalization_cache,
                &mut source_maps,
            )
            .await?;
        let target_id = root.id.clone();
        let effective_scope = effective_trace_scope(&self.workspace, &scope, &root.node)?;
        result.scope =
            trace_scope_receipt(&self.workspace, &scope, effective_scope.package_root.as_deref());
        result.target = Some(target_id.clone());
        result.nodes.insert(target_id.clone(), root.node);
        if let Some(gap) = root.gap {
            result.gaps.push(gap);
        }

        let mut queue =
            VecDeque::from([(target_id.clone(), root.native_id.clone(), root_item, 0u32)]);
        let mut retained_variants = BTreeSet::from([root.native_id]);
        let mut expanded_variants = BTreeSet::new();
        let mut queried_nodes = BTreeSet::new();
        let mut nodes_with_relations = BTreeSet::new();
        let mut edges = BTreeMap::<(String, String), TraceEdge>::new();
        let mut retained_call_sites = 0usize;
        let mut boundaries = BTreeMap::<(String, TraceBoundaryKind), usize>::new();
        let mut omitted_relations = BTreeSet::new();
        let mut truncation_reasons = BTreeSet::new();

        while let Some((current_id, native_id, current, depth)) = queue.pop_front() {
            if !expanded_variants.insert(native_id.clone()) {
                continue;
            }
            let current_node =
                result.nodes.get(&current_id).context("trace graph lost its normalized node")?;
            if current_node.external {
                boundaries.entry((current_id, TraceBoundaryKind::External)).or_insert(0);
                continue;
            }
            if let Some(kind) = trace_expansion_boundary(
                &self.workspace,
                &effective_scope,
                &current_node.definition,
            ) {
                boundaries.entry((current_id, kind)).or_insert(0);
                continue;
            }
            self.synchronize_item_document_typed(&current).await?;
            let method = match direction {
                TraceDirection::Callers => "callHierarchy/incomingCalls",
                TraceDirection::Callees => "callHierarchy/outgoingCalls",
            };
            let response = self.request_typed(method, Some(json!({ "item": current }))).await?;
            queried_nodes.insert(current_id.clone());
            let calls = match response.as_array() {
                Some(calls) => calls,
                None if response.is_null() => continue,
                None => {
                    return Err(NativeFailure::preserved(
                        method,
                        "native tsgo returned a non-array result",
                    ));
                }
            };
            if calls.is_empty() {
                continue;
            }
            nodes_with_relations.insert(current_id.clone());
            if depth >= limits.max_depth {
                let mut omitted = BTreeSet::new();
                for call in calls {
                    let other = match direction {
                        TraceDirection::Callers => call.get("from"),
                        TraceDirection::Callees => call.get("to"),
                    }
                    .context("native tsgo call omitted its related item")?;
                    omitted.insert(trace_node(other, &self.workspace)?.0);
                }
                boundaries.insert((current_id, TraceBoundaryKind::MaxDepth), omitted.len());
                truncation_reasons.insert(format!("maximum depth {} reached", limits.max_depth));
                continue;
            }

            for call in calls {
                let other = match direction {
                    TraceDirection::Callers => call.get("from"),
                    TraceDirection::Callees => call.get("to"),
                }
                .context("native tsgo call omitted its related item")?;
                let normalized = self
                    .normalize_trace_item(
                        other,
                        normalization,
                        &mut normalization_cache,
                        &mut source_maps,
                    )
                    .await?;
                let other_id = normalized.id.clone();
                let other_native_id = normalized.native_id.clone();
                let is_new = !result.nodes.contains_key(&other_id);
                if is_new && result.nodes.len() >= limits.max_nodes {
                    *boundaries
                        .entry((current_id.clone(), TraceBoundaryKind::MaxNodes))
                        .or_insert(0) += 1;
                    truncation_reasons
                        .insert(format!("maximum node count {} reached", limits.max_nodes));
                    continue;
                }
                if is_new {
                    result.nodes.insert(other_id.clone(), normalized.node.clone());
                } else if let Some(existing) = result.nodes.get_mut(&other_id) {
                    merge_trace_node(existing, &normalized.node);
                }
                if let Some(gap) = normalized.gap.clone() {
                    if !result.gaps.contains(&gap) {
                        result.gaps.push(gap);
                    }
                }

                let other_node = result
                    .nodes
                    .get(&other_id)
                    .context("trace graph lost a retained normalized relation node")?;
                let expansion_boundary = if other_node.external {
                    Some(TraceBoundaryKind::External)
                } else {
                    trace_expansion_boundary(
                        &self.workspace,
                        &effective_scope,
                        &other_node.definition,
                    )
                };
                if let Some(kind) = expansion_boundary {
                    boundaries.entry((other_id.clone(), kind)).or_insert(0);
                } else if !retained_variants.contains(&other_native_id) {
                    if retained_variants.len() >= MAX_TRACE_NATIVE_VARIANTS {
                        boundaries
                            .entry((other_id.clone(), TraceBoundaryKind::MaxNativeVariants))
                            .or_insert(0);
                        truncation_reasons.insert(format!(
                            "maximum native call-item variant count {MAX_TRACE_NATIVE_VARIANTS} reached"
                        ));
                    } else {
                        retained_variants.insert(other_native_id.clone());
                        queue.push_back((
                            other_id.clone(),
                            other_native_id.clone(),
                            other.clone(),
                            depth + 1,
                        ));
                    }
                }

                let relation_key = match direction {
                    TraceDirection::Callers => (other_id.clone(), current_id.clone()),
                    TraceDirection::Callees => (current_id.clone(), other_id.clone()),
                };
                let new_relation = !edges.contains_key(&relation_key);
                if new_relation && edges.len() >= MAX_TRACE_NODES {
                    omitted_relations.insert(relation_key);
                    truncation_reasons
                        .insert(format!("maximum relation count {MAX_TRACE_NODES} reached"));
                    continue;
                }

                let (caller, callee, caller_item) = match direction {
                    TraceDirection::Callers => (other_id.clone(), current_id.clone(), other),
                    TraceDirection::Callees => (current_id.clone(), other_id.clone(), &current),
                };
                if caller == callee && other_native_id != native_id {
                    continue;
                }
                let available_sites = MAX_TRACE_NODES.saturating_sub(retained_call_sites);
                let (call_sites, omitted_sites) =
                    trace_call_sites(call, caller_item, &self.workspace, available_sites)?;
                if omitted_sites > 0 {
                    truncation_reasons
                        .insert(format!("maximum call-site count {MAX_TRACE_NODES} reached"));
                    boundaries
                        .entry((current_id.clone(), TraceBoundaryKind::MaxCallSites))
                        .and_modify(|count| *count += omitted_sites)
                        .or_insert(omitted_sites);
                }
                retained_call_sites += call_sites.len();
                let edge =
                    edges.entry((caller.clone(), callee.clone())).or_insert_with(|| TraceEdge {
                        caller: caller.clone(),
                        callee: callee.clone(),
                        call_sites: Vec::new(),
                        cycle: false,
                    });
                for site in call_sites {
                    if !edge.call_sites.contains(&site) {
                        edge.call_sites.push(site);
                    }
                }
            }
        }

        for (node, _) in omitted_relations {
            *boundaries.entry((node, TraceBoundaryKind::MaxRelations)).or_insert(0) += 1;
        }
        result.edges = edges.into_values().collect();
        graph::normalize_call_sites(&mut result.edges);
        result.cycle_components =
            graph::classify_cycles(&result.nodes, &mut result.edges).map_err(anyhow::Error::msg)?;
        result.observed_leaves = queried_nodes.difference(&nodes_with_relations).cloned().collect();
        if direction == TraceDirection::Callers {
            for node in &result.observed_leaves {
                result.gaps.push(TraceGap::CallerAbsenceUnproven {
                    node: node.clone(),
                    reason: TraceCallerGapReason::NativeCallHierarchyIsNotAbsenceProof,
                });
            }
        }
        result.boundaries = boundaries
            .into_iter()
            .map(|((node, kind), omitted_relations)| TraceBoundary {
                node,
                kind,
                omitted_relations,
            })
            .collect();
        result.truncation_reasons = truncation_reasons.into_iter().collect();
        let truncated = !result.truncation_reasons.is_empty() || result.discovery.truncated;
        result.status =
            if truncated { TraceStatus::Cut } else { TraceStatus::CompleteWithinCapture };
        result.summary = TraceSummary {
            observed_leaves: result.observed_leaves.len(),
            nodes: result.nodes.len(),
            edges: result.edges.len(),
            cycle_components: result.cycle_components.len(),
            boundaries: result.boundaries.len(),
            truncated,
        };
        Ok(result)
    }

    async fn synchronize_item_document_typed(
        &mut self,
        item: &Value,
    ) -> std::result::Result<(), NativeFailure> {
        let Some(file) = item_file(item) else {
            return Ok(());
        };
        let canonical = match file.canonicalize() {
            Ok(file) => file,
            Err(_) => return Ok(()),
        };
        if canonical.starts_with(&self.workspace) {
            self.synchronize_document_typed(&canonical).await?;
        }
        Ok(())
    }

    async fn synchronize_document(&mut self, file: &Path) -> Result<()> {
        self.synchronize_document_typed(file).await.map_err(anyhow::Error::new)
    }

    async fn synchronize_document_typed(
        &mut self,
        file: &Path,
    ) -> std::result::Result<(), NativeFailure> {
        if self.active_locus_capture.is_some() && self.locus_observations.contains_key(file) {
            return Ok(());
        }
        if self.active_locus_capture.is_some()
            && self.locus_observations.len() >= MAX_LOCUS_OBSERVED_FILES
        {
            return Err(NativeFailure::preserved(
                "textDocument synchronization",
                format!(
                    "locus capture reached the {MAX_LOCUS_OBSERVED_FILES}-file observation limit"
                ),
            ));
        }
        let capture_byte_limit = if self.active_locus_capture.is_some() {
            let captured_source_bytes = self
                .locus_observations
                .values()
                .fold(0u64, |total, observation| total.saturating_add(observation.source_bytes));
            let remaining = MAX_LOCUS_TOTAL_SOURCE_BYTES.saturating_sub(captured_source_bytes);
            if remaining == 0 {
                return Err(NativeFailure::preserved(
                    "textDocument synchronization",
                    format!(
                        "locus capture reached the {} MiB total source limit",
                        MAX_LOCUS_TOTAL_SOURCE_BYTES / (1024 * 1024)
                    ),
                ));
            }
            Some(remaining)
        } else {
            None
        };
        let read_limit =
            capture_byte_limit.unwrap_or(MAX_LOCUS_SOURCE_BYTES).min(MAX_LOCUS_SOURCE_BYTES);
        let source = tokio::fs::File::open(file)
            .await
            .with_context(|| format!("open TypeScript file {}", file.display()))
            .map_err(NativeFailure::from)?;
        let mut bytes = Vec::new();
        source
            .take(read_limit + 1)
            .read_to_end(&mut bytes)
            .await
            .with_context(|| format!("read TypeScript file {}", file.display()))
            .map_err(NativeFailure::from)?;
        let source_bytes = bytes.len() as u64;
        if source_bytes > MAX_LOCUS_SOURCE_BYTES {
            return Err(NativeFailure::preserved(
                "textDocument synchronization",
                format!(
                    "TypeScript file {} exceeds the 16 MiB source capture limit",
                    file.display()
                ),
            ));
        }
        if capture_byte_limit.is_some_and(|remaining| source_bytes > remaining) {
            return Err(NativeFailure::preserved(
                "textDocument synchronization",
                format!(
                    "TypeScript file {} would exceed the {} MiB total locus source limit",
                    file.display(),
                    MAX_LOCUS_TOTAL_SOURCE_BYTES / (1024 * 1024)
                ),
            ));
        }
        let text = String::from_utf8(bytes)
            .with_context(|| format!("decode TypeScript file {} as UTF-8", file.display()))
            .map_err(NativeFailure::from)?;
        if self.active_locus_capture.is_some() {
            let sha256 = sha256_hex(text.as_bytes());
            self.locus_observations.insert(
                file.to_path_buf(),
                LocusObservation { first_sha256: sha256, source_bytes },
            );
        }
        let uri = file_uri(file)?;
        let existing =
            self.documents.get(file).map(|document| (document.version, document.text == text));
        let sync = match existing {
            None => {
                self.notify_typed(
                    "textDocument/didOpen",
                    Some(json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id(file),
                            "version": 1,
                            "text": text
                        }
                    })),
                )
                .await?;
                self.documents.insert(file.to_path_buf(), DocumentState { version: 1, text });
                TraceDocumentSync::Opened
            }
            Some((previous_version, false)) => {
                self.notify_typed(
                    "textDocument/didClose",
                    Some(json!({ "textDocument": { "uri": uri.clone() } })),
                )
                .await?;
                let version = previous_version.checked_add(1).ok_or_else(|| {
                    NativeFailure::preserved(
                        "textDocument synchronization",
                        "document version overflow",
                    )
                })?;
                self.notify_typed(
                    "textDocument/didOpen",
                    Some(json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id(file),
                            "version": version,
                            "text": text
                        }
                    })),
                )
                .await?;
                self.documents.insert(file.to_path_buf(), DocumentState { version, text });
                TraceDocumentSync::Refreshed
            }
            Some((_, true)) => TraceDocumentSync::Reused,
        };
        if let Some(capture) = self.active_trace_capture.as_mut() {
            capture.documents.entry(file.to_path_buf()).or_insert(sync);
        }
        Ok(())
    }

    async fn enter_semantic_document_mode(&mut self) -> std::result::Result<(), NativeFailure> {
        if matches!(&self.document_mode, OpenDocumentMode::Diagnostics { .. }) {
            self.close_all_documents_typed().await?;
            self.document_mode = OpenDocumentMode::Semantic;
        }
        Ok(())
    }

    async fn close_all_documents_typed(&mut self) -> std::result::Result<(), NativeFailure> {
        let files = self.documents.keys().cloned().collect::<Vec<_>>();
        for file in files {
            self.notify_typed(
                "textDocument/didClose",
                Some(json!({ "textDocument": { "uri": file_uri(&file)? } })),
            )
            .await?;
        }
        self.documents.clear();
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        self.request_typed(method, params).await.map_err(anyhow::Error::new)
    }

    async fn request_typed(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> std::result::Result<Value, NativeFailure> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        let mut message = Map::new();
        message.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
        message.insert("id".to_owned(), json!(id));
        message.insert("method".to_owned(), Value::String(method.to_owned()));
        if let Some(params) = params {
            message.insert("params".to_owned(), params);
        }
        self.send(Value::Object(message))
            .await
            .map_err(|error| NativeFailure::lost(method, format!("{error:#}")))?;
        match timeout(NATIVE_REQUEST_DEADLINE, self.await_response(id, method)).await {
            Ok(result) => result,
            Err(_) => {
                self.notify_typed("$/cancelRequest", Some(json!({ "id": id }))).await?;
                match timeout(NATIVE_CANCEL_DRAIN, self.await_response(id, method)).await {
                    Ok(Ok(_)) | Ok(Err(NativeFailure::Preserved { .. })) => {
                        Err(NativeFailure::preserved(method, "request deadline exceeded"))
                    }
                    Ok(Err(failure @ NativeFailure::Lost { .. })) => Err(failure),
                    Err(_) => Err(NativeFailure::lost(
                        method,
                        "cancelled request did not produce a terminal response",
                    )),
                }
            }
        }
    }

    async fn await_response(
        &mut self,
        id: u64,
        method: &str,
    ) -> std::result::Result<Value, NativeFailure> {
        loop {
            let message = self
                .read_message()
                .await
                .map_err(|error| NativeFailure::lost(method, format!("{error:#}")))?;
            if let Some(server_method) = message.get("method").and_then(Value::as_str) {
                if let Some(server_id) = message.get("id").cloned() {
                    self.reply_to_server(server_id, server_method, message.get("params"))
                        .await
                        .map_err(|error| NativeFailure::lost(method, format!("{error:#}")))?;
                }
                continue;
            }
            if message.get("id") != Some(&json!(id)) {
                return Err(NativeFailure::lost(method, "unexpected response id"));
            }
            if let Some(error) = message.get("error") {
                return Err(NativeFailure::response(method, error));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        self.notify_typed(method, params).await.map_err(anyhow::Error::new)
    }

    async fn notify_typed(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> std::result::Result<(), NativeFailure> {
        let mut message = Map::new();
        message.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
        message.insert("method".to_owned(), Value::String(method.to_owned()));
        if let Some(params) = params {
            message.insert("params".to_owned(), params);
        }
        self.send(Value::Object(message))
            .await
            .map_err(|error| NativeFailure::lost(method, format!("{error:#}")))
    }

    async fn reply_to_server(
        &mut self,
        id: Value,
        method: &str,
        params: Option<&Value>,
    ) -> Result<()> {
        let result = match method {
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create" => Some(Value::Null),
            "workspace/configuration" => {
                let count = params
                    .and_then(|value| value.get("items"))
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                Some(Value::Array(vec![Value::Null; count]))
            }
            "workspace/workspaceFolders" => Some(json!([{
                "uri": directory_uri(&self.workspace)?,
                "name": workspace_name(&self.workspace)
            }])),
            "workspace/applyEdit" => Some(json!({
                "applied": false,
                "failureReason": "Kit's query service does not apply server edits"
            })),
            _ => None,
        };
        let response = match result {
            Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            None => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("unsupported server request: {method}") }
            }),
        };
        self.send(response).await
    }

    async fn send(&mut self, value: Value) -> Result<()> {
        let body = serde_json::to_vec(&value)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let input = self.input.as_mut().context("native tsgo stdin is closed")?;
        input.write(header.as_bytes()).await.context("write native tsgo LSP header")?;
        input.write(&body).await.context("write native tsgo LSP body")?;
        input.flush().await.context("flush native tsgo LSP message")?;
        Ok(())
    }

    async fn read_message(&mut self) -> Result<Value> {
        loop {
            if let Some((header_end, delimiter_len)) = header_end(&self.buffer) {
                let header = std::str::from_utf8(&self.buffer[..header_end])
                    .context("native tsgo emitted non-UTF8 LSP headers")?;
                let length = content_length(header)?;
                if length > LSP_MESSAGE_LIMIT {
                    bail!("native tsgo LSP message exceeds 16 MiB");
                }
                let body_start = header_end + delimiter_len;
                if self.buffer.len() >= body_start + length {
                    let body = self.buffer[body_start..body_start + length].to_vec();
                    self.buffer.drain(..body_start + length);
                    return serde_json::from_slice(&body)
                        .context("decode native tsgo LSP message body");
                }
            }
            let stdout = self.stdout.as_mut().context("native tsgo stdout is closed")?;
            match stdout.next().await.context("read native tsgo stdout")? {
                ProcessByteEvent::Chunk { bytes, .. } => self.buffer.extend_from_slice(&bytes),
                ProcessByteEvent::End => bail!("native tsgo stdout ended"),
            }
            if self.buffer.len() > LSP_MESSAGE_LIMIT + 64 * 1024 {
                bail!("native tsgo LSP framing buffer exceeded its limit");
            }
        }
    }

    async fn finish(&mut self, graceful: bool) -> Result<Value> {
        let protocol_shutdown = if graceful && self.session.is_some() {
            self.request("shutdown", None).await.is_ok()
        } else {
            false
        };
        if graceful && protocol_shutdown {
            let _ = self.notify("exit", None).await;
        }
        if let Some(input) = self.input.take() {
            let _ = input.close().await;
        }
        let stdout_task = self.stdout.take().map(|mut stdout| {
            tokio::spawn(async move {
                loop {
                    match stdout.next().await {
                        Ok(ProcessByteEvent::Chunk { .. }) => {}
                        Ok(ProcessByteEvent::End) | Err(_) => break,
                    }
                }
            })
        });
        let Some(session) = self.session.take() else {
            return Ok(json!({ "graceful": protocol_shutdown, "reaped": true }));
        };
        let control = session.control();
        let mut wait_task = tokio::spawn(async move { session.wait().await });
        let report = match timeout(PROCESS_GRACE, &mut wait_task).await {
            Ok(joined) => joined.context("join native tsgo process owner")?,
            Err(_) => {
                let _ = control.cancel().await;
                match timeout(PROCESS_KILL_WAIT, &mut wait_task).await {
                    Ok(joined) => joined.context("join cancelled native tsgo process owner")?,
                    Err(_) => {
                        let _ = control.force_kill().await;
                        wait_task.await.context("join killed native tsgo process owner")?
                    }
                }
            }
        };
        if let Some(task) = stdout_task {
            let _ = task.await;
        }
        if let Some(task) = self.stderr_task.take() {
            let _ = task.await;
        }
        match report {
            Ok(report) => Ok(json!({
                "graceful": protocol_shutdown,
                "reaped": true,
                "completion": format!("{:?}", report.completion),
                "leader_exit": format!("{:?}", report.leader_exit)
            })),
            Err(report) => Err(anyhow!(
                "native tsgo process infrastructure failure {:?} (run {})",
                report.failure,
                report.run_id
            )),
        }
    }
}

fn connected_edge_prefix(
    mut edges: Vec<TraceEdge>,
    nodes: &BTreeMap<String, TraceNode>,
    root: &TraceLocation,
    direction: TraceDirection,
    maximum: usize,
) -> Vec<TraceEdge> {
    let mut reachable = BTreeSet::from([root.clone()]);
    let mut selected = Vec::new();
    while selected.len() < maximum && !edges.is_empty() {
        let position = edges.iter().position(|edge| {
            let caller = nodes.get(&edge.caller).map(|node| &node.definition);
            let callee = nodes.get(&edge.callee).map(|node| &node.definition);
            match (direction, caller, callee) {
                (TraceDirection::Callers, Some(_), Some(callee)) => reachable.contains(callee),
                (TraceDirection::Callees, Some(caller), Some(_)) => reachable.contains(caller),
                _ => false,
            }
        });
        let Some(position) = position else {
            break;
        };
        let edge = edges.remove(position);
        if let Some(caller) = nodes.get(&edge.caller) {
            reachable.insert(caller.definition.clone());
        }
        if let Some(callee) = nodes.get(&edge.callee) {
            reachable.insert(callee.definition.clone());
        }
        selected.push(edge);
    }
    selected
}

fn merge_locus_cuts(cuts: Vec<LocusCaptureCut>) -> Vec<LocusCaptureCut> {
    let mut merged = BTreeMap::<LocusCutReason, LocusOmission>::new();
    for cut in cuts {
        merged
            .entry(cut.reason)
            .and_modify(|current| {
                *current = match (&*current, &cut.omission) {
                    (
                        LocusOmission::Known { count: left },
                        LocusOmission::Known { count: right },
                    ) => LocusOmission::Known { count: left.saturating_add(*right) },
                    _ => LocusOmission::Unknown,
                };
            })
            .or_insert(cut.omission);
    }
    merged.into_iter().map(|(reason, omission)| LocusCaptureCut { reason, omission }).collect()
}

fn capability_enabled(initialize: &Value, pointer: &str) -> bool {
    match initialize.pointer(pointer) {
        Some(Value::Bool(enabled)) => *enabled,
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

fn diagnostic_capabilities(initialize: &Value) -> DiagnosticCapabilities {
    match initialize.pointer("/capabilities/diagnosticProvider") {
        Some(Value::Bool(supported)) => DiagnosticCapabilities {
            supported: *supported,
            inter_file_dependencies: false,
            workspace_diagnostics: false,
        },
        Some(Value::Object(options)) => DiagnosticCapabilities {
            supported: true,
            inter_file_dependencies: options
                .get("interFileDependencies")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            workspace_diagnostics: options
                .get("workspaceDiagnostics")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        Some(Value::Null) | None => DiagnosticCapabilities::default(),
        Some(_) => DiagnosticCapabilities::default(),
    }
}

fn bounded_failure_detail(value: impl std::fmt::Display) -> String {
    bounded_utf8(value.to_string(), MAX_LOCUS_TEXT_BYTES)
}

fn bounded_utf8(value: String, maximum: usize) -> String {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum.saturating_sub('…'.len_utf8());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn unsupported_acquisition(reason: impl Into<String>) -> AcquiredLocusEvidence {
    AcquiredLocusEvidence {
        state: LocusAcquisitionState::Unsupported { reason: reason.into() },
        evidence: Vec::new(),
        prepare: None,
    }
}

fn take_evidence_id(next_evidence_id: &mut usize) -> std::result::Result<String, NativeFailure> {
    let current = *next_evidence_id;
    *next_evidence_id = current.checked_add(1).ok_or_else(|| {
        NativeFailure::preserved("evidence identity", "evidence identity counter overflow")
    })?;
    Ok(format!("evidence-{current:06}"))
}

fn locus_seed_candidate(symbol: &Value, workspace: &Path) -> Result<LocusSeedCandidate> {
    let candidate = workspace_symbol_candidate(symbol, workspace)?;
    let label = bounded_utf8(
        candidate
            .detail
            .as_ref()
            .map(|detail| format!("{detail}.{}", candidate.name))
            .unwrap_or_else(|| candidate.name.clone()),
        MAX_LOCUS_LABEL_BYTES,
    );
    Ok(LocusSeedCandidate {
        label: label.clone(),
        anchor: LocusAnchor {
            label,
            external: candidate.location.file.is_absolute(),
            location: candidate.location,
        },
    })
}

fn locus_call_item_candidate(item: &Value, workspace: &Path) -> Result<LocusSeedCandidate> {
    let candidate = trace_candidate(item, workspace)?;
    let label = bounded_utf8(
        candidate
            .detail
            .as_ref()
            .map(|detail| format!("{detail}.{}", candidate.name))
            .unwrap_or_else(|| candidate.name.clone()),
        MAX_LOCUS_LABEL_BYTES,
    );
    Ok(LocusSeedCandidate {
        label: label.clone(),
        anchor: LocusAnchor {
            label,
            external: candidate.location.file.is_absolute(),
            location: candidate.location,
        },
    })
}

fn decode_lsp_locations(
    response: &Value,
    workspace: &Path,
    allow_single: bool,
    method: &str,
) -> std::result::Result<Vec<TraceLocation>, NativeFailure> {
    if response.is_null() {
        return Ok(Vec::new());
    }
    let values = match response.as_array() {
        Some(values) => values.iter().collect::<Vec<_>>(),
        None if allow_single && response.is_object() => vec![response],
        None => {
            return Err(NativeFailure::preserved(
                method,
                "native tsgo returned an invalid location collection",
            ));
        }
    };
    values
        .into_iter()
        .map(|value| {
            native_location(value, workspace)
                .map_err(|error| NativeFailure::preserved(method, format!("{error:#}")))
        })
        .collect()
}

fn native_location(value: &Value, workspace: &Path) -> Result<TraceLocation> {
    let (uri, start) = if let Some(uri) = value.get("uri").and_then(Value::as_str) {
        let start =
            value.pointer("/range/start").context("native location omitted its range start")?;
        (uri, start)
    } else {
        let uri = value
            .get("targetUri")
            .and_then(Value::as_str)
            .context("native location link omitted targetUri")?;
        let start = value
            .pointer("/targetSelectionRange/start")
            .or_else(|| value.pointer("/targetRange/start"))
            .context("native location link omitted its target start")?;
        (uri, start)
    };
    let (line, character) = value_position(start)?;
    let file = uri_file_path(uri)?;
    let file = file.canonicalize().unwrap_or(file);
    Ok(public_location(workspace, file, line, character))
}

fn locus_anchor_from_trace_node(node: &TraceNode) -> LocusAnchor {
    LocusAnchor {
        label: bounded_utf8(node.name.clone(), MAX_LOCUS_LABEL_BYTES),
        location: node.definition.clone(),
        external: node.external,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect()
}

fn public_file(workspace: &Path, file: &Path) -> PathBuf {
    file.strip_prefix(workspace).map(Path::to_path_buf).unwrap_or_else(|_| file.to_path_buf())
}

async fn recheck_diagnostic_documents(
    workspace: &Path,
    captured: &[(PathBuf, String, i64)],
) -> Vec<ChangedDiagnosticDocument> {
    let mut changed = Vec::new();
    for (file, before_sha256, _) in captured {
        let recheck = async {
            let source = tokio::fs::File::open(file).await?;
            let mut bytes = Vec::new();
            source.take(MAX_LOCUS_SOURCE_BYTES + 1).read_to_end(&mut bytes).await?;
            if bytes.len() as u64 > MAX_LOCUS_SOURCE_BYTES {
                bail!("source exceeds the 16 MiB recheck limit");
            }
            Ok::<_, anyhow::Error>(bytes)
        }
        .await;
        let after = match recheck {
            Ok(bytes) => {
                let sha256 = sha256_hex(&bytes);
                if sha256 == *before_sha256 {
                    continue;
                }
                DiagnosticRecheckValue::Present { sha256 }
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                DiagnosticRecheckValue::Missing
            }
            Err(error) => {
                DiagnosticRecheckValue::Unreadable { detail: bounded_failure_detail(error) }
            }
        };
        changed.push(ChangedDiagnosticDocument {
            file: public_file(workspace, file),
            before_sha256: before_sha256.clone(),
            after,
        });
    }
    changed
}

async fn recheck_observed_documents(observed: &[ObservedDocument]) -> LocusFreshness {
    let mut unchanged_files = Vec::new();
    let mut changed_files = Vec::new();
    for document in observed {
        let recheck = async {
            let file = tokio::fs::File::open(&document.absolute).await?;
            let mut bytes = Vec::new();
            file.take(MAX_LOCUS_SOURCE_BYTES + 1).read_to_end(&mut bytes).await?;
            if bytes.len() as u64 > MAX_LOCUS_SOURCE_BYTES {
                bail!("source exceeds the 16 MiB recheck limit");
            }
            Ok::<_, anyhow::Error>(bytes)
        }
        .await;
        match recheck {
            Ok(bytes) => {
                let after = sha256_hex(&bytes);
                if after == document.sha256 {
                    unchanged_files.push(LocusCapturedFile {
                        file: document.public.clone(),
                        sha256: document.sha256.clone(),
                    });
                } else {
                    changed_files.push(LocusChangedFile {
                        file: document.public.clone(),
                        before_sha256: document.sha256.clone(),
                        after: LocusRecheckValue::Sha256 { sha256: after },
                    });
                }
            }
            Err(error) => changed_files.push(LocusChangedFile {
                file: document.public.clone(),
                before_sha256: document.sha256.clone(),
                after: LocusRecheckValue::Unavailable { reason: error.to_string() },
            }),
        }
    }
    if changed_files.is_empty() {
        LocusFreshness::Checked { files: unchanged_files }
    } else {
        LocusFreshness::ChangedObservedInput { unchanged_files, changed_files }
    }
}

fn locus_fingerprint(
    request: &LocusRequest,
    server_version: &str,
    observed: &[ObservedDocument],
) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(b"kit-tsgo-locus-v3\0");
    hash.update(server_version.as_bytes());
    hash.update([0]);
    hash.update(serde_json::to_vec(request).context("encode locus request fingerprint")?);
    for document in observed {
        hash.update([0]);
        hash.update(document.public.to_string_lossy().as_bytes());
        hash.update([0]);
        hash.update(document.sha256.as_bytes());
    }
    Ok(hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

struct DiscoveryFiles {
    files: Vec<PathBuf>,
    scanned_files: usize,
    truncated: bool,
}

fn effective_trace_scope(
    workspace: &Path,
    requested: &TraceScope,
    target: &TraceNode,
) -> std::result::Result<EffectiveTraceScope, NativeFailure> {
    let mut source_roots = requested
        .source_roots
        .iter()
        .map(|root| {
            let unresolved = if root.is_absolute() { root.clone() } else { workspace.join(root) };
            unresolved
                .canonicalize()
                .with_context(|| format!("canonicalize trace source root {}", unresolved.display()))
                .map_err(NativeFailure::from)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    source_roots.sort();
    source_roots.dedup();
    if source_roots.iter().any(|root| !root.starts_with(workspace)) {
        return Err(NativeFailure::preserved(
            "trace scope",
            "trace source roots must remain inside the workspace",
        ));
    }

    let target_file = absolute_trace_file(workspace, &target.definition.file);
    if !source_roots.is_empty() && !source_roots.iter().any(|root| target_file.starts_with(root)) {
        return Err(NativeFailure::preserved(
            "trace scope",
            format!(
                "trace target {} is outside every --within source root",
                target.definition.file.display()
            ),
        ));
    }
    let package_root = requested
        .stop_at_package_boundary
        .then(|| nearest_package_root(workspace, &target_file))
        .flatten();
    Ok(EffectiveTraceScope { source_roots, package_root })
}

fn nearest_package_root(workspace: &Path, file: &Path) -> Option<PathBuf> {
    let mut directory = file.parent()?;
    loop {
        if directory.join("package.json").is_file() {
            return Some(directory.to_path_buf());
        }
        if directory == workspace {
            return None;
        }
        directory = directory.parent()?;
        if !directory.starts_with(workspace) {
            return None;
        }
    }
}

fn trace_scope_receipt(
    workspace: &Path,
    requested: &TraceScope,
    package_root: Option<&Path>,
) -> TraceScopeReceipt {
    let mut source_roots =
        requested.source_roots.iter().map(|root| public_file(workspace, root)).collect::<Vec<_>>();
    source_roots.sort();
    source_roots.dedup();
    let package = if !requested.stop_at_package_boundary {
        TracePackageScope::Disabled
    } else if let Some(root) = package_root {
        TracePackageScope::Enabled { root: public_file(workspace, root) }
    } else {
        TracePackageScope::Unresolved
    };
    TraceScopeReceipt { source_roots, package }
}

fn trace_expansion_boundary(
    workspace: &Path,
    scope: &EffectiveTraceScope,
    definition: &TraceLocation,
) -> Option<TraceBoundaryKind> {
    let file = absolute_trace_file(workspace, &definition.file);
    if !scope.source_roots.is_empty()
        && !scope.source_roots.iter().any(|root| file.starts_with(root))
    {
        return Some(TraceBoundaryKind::SourceRoot);
    }
    if scope.package_root.as_ref().is_some_and(|root| !file.starts_with(root)) {
        return Some(TraceBoundaryKind::Package);
    }
    None
}

fn absolute_trace_file(workspace: &Path, file: &Path) -> PathBuf {
    if file.is_absolute() {
        file.to_path_buf()
    } else {
        workspace.join(file)
    }
}

fn merge_trace_node(existing: &mut TraceNode, incoming: &TraceNode) {
    for alias in &incoming.generated_aliases {
        if !existing.generated_aliases.contains(alias) {
            existing.generated_aliases.push(alias.clone());
        }
    }
    existing.generated_aliases.sort();
    existing.generated_aliases.dedup();
}

fn unresolved_trace_identity(
    mut native: NormalizedTraceItem,
    reason: TraceIdentityGapReason,
) -> NormalizedTraceItem {
    native.gap = Some(TraceGap::GeneratedIdentityUnresolved {
        node: native.id.clone(),
        declaration: native.node.definition.clone(),
        reason,
    });
    native
}

async fn read_source_map(file: &Path) -> std::result::Result<Arc<SourceMap>, String> {
    let source = match tokio::fs::File::open(file).await {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("missing".to_owned());
        }
        Err(error) => return Err(bounded_failure_detail(error)),
    };
    let mut bytes = Vec::new();
    source
        .take(TRACE_SOURCE_MAP_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(bounded_failure_detail)?;
    if bytes.len() as u64 > TRACE_SOURCE_MAP_LIMIT {
        return Err(format!(
            "source map exceeds the {} MiB read limit",
            TRACE_SOURCE_MAP_LIMIT / (1024 * 1024)
        ));
    }
    let text = String::from_utf8(bytes).map_err(bounded_failure_detail)?;
    SourceMap::parse(&text).map(Arc::new)
}

fn is_declaration_source(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
        name.ends_with(".d.ts") || name.ends_with(".d.mts") || name.ends_with(".d.cts")
    })
}

fn is_canonical_trace_source(path: &Path) -> bool {
    !is_declaration_source(path)
        && matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("ts" | "tsx" | "mts" | "cts")
        )
}

fn validate_trace_limits(limits: TraceLimits) -> Result<()> {
    if limits.max_depth > MAX_TRACE_DEPTH {
        bail!("--max-depth may not exceed {MAX_TRACE_DEPTH}");
    }
    if limits.max_nodes == 0 || limits.max_nodes > MAX_TRACE_NODES {
        bail!("--max-nodes must be between 1 and {MAX_TRACE_NODES}");
    }
    Ok(())
}

fn empty_trace_result(
    selector: String,
    direction: TraceDirection,
    candidates: Vec<TraceCandidate>,
    discovery: TraceDiscovery,
    scope: TraceScopeReceipt,
) -> TraceResult {
    TraceResult {
        status: TraceStatus::NotFound,
        selector,
        direction,
        target: None,
        candidates,
        nodes: BTreeMap::new(),
        edges: Vec::new(),
        observed_leaves: Vec::new(),
        cycle_components: Vec::new(),
        boundaries: Vec::new(),
        summary: TraceSummary::default(),
        timing: TraceTiming::default(),
        discovery,
        coverage: TraceCoverage::default(),
        scope,
        gaps: Vec::new(),
        advice: Vec::new(),
        truncation_reasons: Vec::new(),
    }
}

fn discover_candidate_files(
    workspace: &Path,
    scan_root: &Path,
    needle: &str,
) -> Result<DiscoveryFiles> {
    if !scan_root.starts_with(workspace) {
        bail!("symbol discovery root is outside its workspace");
    }
    let mut files = Vec::new();
    let mut scanned_files = 0usize;
    let mut truncated = false;
    let walker = WalkBuilder::new(scan_root).standard_filters(true).follow_links(false).build();
    for entry in walker {
        let entry = entry.context("walk TypeScript symbol discovery scope")?;
        if !entry.file_type().is_some_and(|kind| kind.is_file())
            || !is_typescript_source(entry.path())
        {
            continue;
        }
        scanned_files += 1;
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if identifier_offsets(&text, needle).next().is_none() {
            continue;
        }
        files.push(
            entry
                .path()
                .canonicalize()
                .with_context(|| format!("canonicalize candidate {}", entry.path().display()))?,
        );
        if files.len() >= DISCOVERY_MATCH_LIMIT {
            truncated = true;
            break;
        }
    }
    files.sort();
    files.dedup();
    Ok(DiscoveryFiles { files, scanned_files, truncated })
}

fn is_typescript_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs")
    )
}

fn symbol_leaf(query: &str) -> Result<&str> {
    let leaf = query.rsplit('.').next().unwrap_or(query).trim();
    if leaf.is_empty() {
        bail!("semantic symbol name must end in an identifier");
    }
    Ok(leaf)
}

fn identifier_offsets<'a>(text: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    text.match_indices(needle).filter_map(move |(offset, _)| {
        let before = text[..offset].chars().next_back();
        let after = text[offset + needle.len()..].chars().next();
        (!before.is_some_and(is_identifier_character)
            && !after.is_some_and(is_identifier_character))
        .then_some(offset)
    })
}

fn is_identifier_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '$')
}

fn workspace_symbol_matches(symbol: &Value, query: &str, scope: Option<&Path>) -> bool {
    let Some(name) = symbol.get("name").and_then(Value::as_str) else {
        return false;
    };
    let semantic_name = symbol
        .get("containerName")
        .and_then(Value::as_str)
        .filter(|container| !container.is_empty())
        .map(|container| format!("{container}.{name}"));
    if query != name && semantic_name.as_deref() != Some(query) {
        return false;
    }
    match (scope, workspace_symbol_file(symbol).ok()) {
        (Some(scope), Some(file)) => file.starts_with(scope),
        (Some(_), None) => false,
        (None, _) => true,
    }
}

fn workspace_symbol_sort_key(symbol: &Value) -> String {
    format!(
        "{}\0{:010}\0{}",
        symbol.pointer("/location/uri").and_then(Value::as_str).unwrap_or_default(),
        symbol.pointer("/location/range/start/line").and_then(Value::as_u64).unwrap_or_default(),
        symbol.get("name").and_then(Value::as_str).unwrap_or_default()
    )
}

fn workspace_symbol_file(symbol: &Value) -> Result<PathBuf> {
    let uri = symbol
        .pointer("/location/uri")
        .and_then(Value::as_str)
        .context("workspace symbol omitted its file URI")?;
    uri_file_path(uri)?.canonicalize().with_context(|| {
        format!("canonicalize workspace symbol file returned by native tsgo: {uri}")
    })
}

fn workspace_symbol_position(symbol: &Value) -> Result<(u32, u32)> {
    let start = symbol
        .pointer("/location/range/start")
        .context("workspace symbol omitted its start position")?;
    value_position(start)
}

fn workspace_symbol_candidate(symbol: &Value, workspace: &Path) -> Result<TraceCandidate> {
    let file = workspace_symbol_file(symbol)?;
    let (line, character) = workspace_symbol_position(symbol)?;
    let name = symbol
        .get("name")
        .and_then(Value::as_str)
        .context("workspace symbol omitted its name")?
        .to_owned();
    let detail = symbol
        .get("containerName")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    Ok(TraceCandidate {
        name,
        detail,
        kind: symbol.get("kind").and_then(Value::as_u64).unwrap_or_default(),
        location: public_location(workspace, file, line, character),
    })
}

fn trace_candidate(item: &Value, workspace: &Path) -> Result<TraceCandidate> {
    let (file, line, character) = item_location(item)?;
    Ok(TraceCandidate {
        name: item
            .get("name")
            .and_then(Value::as_str)
            .context("call hierarchy item omitted its name")?
            .to_owned(),
        detail: item.get("detail").and_then(Value::as_str).map(str::to_owned),
        kind: item.get("kind").and_then(Value::as_u64).unwrap_or_default(),
        location: public_location(workspace, file, line, character),
    })
}

fn trace_node(item: &Value, workspace: &Path) -> Result<(String, TraceNode)> {
    let uri =
        item.get("uri").and_then(Value::as_str).context("call hierarchy item omitted its URI")?;
    let (file, line, character) = item_location(item)?;
    let canonical = file.canonicalize().unwrap_or(file);
    let external = !canonical.starts_with(workspace);
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .context("call hierarchy item omitted its name")?
        .to_owned();
    let detail = item.get("detail").and_then(Value::as_str).map(str::to_owned);
    let kind = item.get("kind").and_then(Value::as_u64).unwrap_or_default();
    let mut hash = Sha256::new();
    hash.update(uri.as_bytes());
    hash.update([0]);
    hash.update(line.to_le_bytes());
    hash.update(character.to_le_bytes());
    hash.update(kind.to_le_bytes());
    hash.update(name.as_bytes());
    if let Some(detail) = &detail {
        hash.update(detail.as_bytes());
    }
    let id = format!(
        "sym_{}",
        hash.finalize()[..16].iter().map(|byte| format!("{byte:02x}")).collect::<String>()
    );
    let node = TraceNode {
        id: id.clone(),
        name,
        detail,
        kind,
        definition: public_location(workspace, canonical, line, character),
        generated_aliases: Vec::new(),
        external,
    };
    Ok((id, node))
}

fn item_location(item: &Value) -> Result<(PathBuf, u32, u32)> {
    let uri =
        item.get("uri").and_then(Value::as_str).context("call hierarchy item omitted its URI")?;
    let start = item
        .pointer("/selectionRange/start")
        .or_else(|| item.pointer("/range/start"))
        .context("call hierarchy item omitted its selection range")?;
    let (line, character) = value_position(start)?;
    let file = uri_file_path(uri).unwrap_or_else(|_| PathBuf::from(uri));
    Ok((file, line, character))
}

fn item_file(item: &Value) -> Option<PathBuf> {
    item.get("uri").and_then(Value::as_str).and_then(|uri| uri_file_path(uri).ok())
}

fn value_position(value: &Value) -> Result<(u32, u32)> {
    let line = value
        .get("line")
        .and_then(Value::as_u64)
        .context("LSP position omitted its line")?
        .try_into()
        .context("LSP line exceeds u32")?;
    let character = value
        .get("character")
        .and_then(Value::as_u64)
        .context("LSP position omitted its character")?
        .try_into()
        .context("LSP character exceeds u32")?;
    Ok((line, character))
}

fn public_location(workspace: &Path, file: PathBuf, line: u32, character: u32) -> TraceLocation {
    let file = file.strip_prefix(workspace).map(Path::to_path_buf).unwrap_or(file);
    TraceLocation { file, line: line + 1, character: character + 1 }
}

fn trace_call_sites(
    call: &Value,
    caller_item: &Value,
    workspace: &Path,
    maximum: usize,
) -> Result<(Vec<TraceLocation>, usize)> {
    let unresolved_file =
        item_file(caller_item).context("call hierarchy caller omitted a usable file URI")?;
    let file = unresolved_file.canonicalize().unwrap_or(unresolved_file);
    let ranges = call
        .get("fromRanges")
        .and_then(Value::as_array)
        .context("native tsgo call omitted fromRanges")?;
    let omitted = ranges.len().saturating_sub(maximum);
    let locations = ranges
        .iter()
        .take(maximum)
        .map(|range| {
            let start = range.get("start").context("call range omitted its start")?;
            let (line, character) = value_position(start)?;
            Ok(public_location(workspace, file.clone(), line, character))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((locations, omitted))
}

fn uri_file_path(uri: &str) -> Result<PathBuf> {
    Url::parse(uri)
        .with_context(|| format!("parse native tsgo URI {uri}"))?
        .to_file_path()
        .map_err(|()| anyhow!("native tsgo returned a non-file URI: {uri}"))
}

fn header_end(buffer: &[u8]) -> Option<(usize, usize)> {
    if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
        return Some((index, 4));
    }
    buffer.windows(2).position(|window| window == b"\n\n").map(|index| (index, 2))
}

fn content_length(header: &str) -> Result<usize> {
    for line in header.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            return value.trim().parse().context("parse native tsgo LSP Content-Length");
        }
    }
    bail!("native tsgo LSP frame omitted Content-Length")
}

fn directory_uri(path: &Path) -> Result<String> {
    Url::from_directory_path(path)
        .map(|url| url.to_string())
        .map_err(|()| anyhow!("convert workspace {} to file URI", path.display()))
}

fn file_uri(path: &Path) -> Result<String> {
    Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|()| anyhow!("convert file {} to URI", path.display()))
}

fn workspace_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace")
        .to_owned()
}

fn language_id(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("tsx") => "typescriptreact",
        Some("js" | "mjs" | "cjs") => "javascript",
        Some("jsx") => "javascriptreact",
        _ => "typescript",
    }
}
