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

use crate::framework::process::{
    CommandSpec, ContainmentRequirement, EnvironmentBase, InputPolicy, OutputPolicy,
    ProcessByteEvent, ProcessByteStream, ProcessEnvironment, ProcessInputHandle,
    ProcessInputWriter, ProcessLabel, ProcessOutputHandle, ProcessSession, ProcessSpec,
    ProcessSupervisor, StreamPolicy, TerminationPolicy,
};

use super::protocol::{
    ChildIdentity, ServiceCommand, ServiceIdentity, ServiceInfo, ServiceReply, ServiceRequest,
    TraceCandidate, TraceDirection, TraceDiscovery, TraceEdge, TraceLimits, TraceLocation,
    TraceNode, TracePath, TraceResult, TraceSelector, TraceSummary, TraceTiming,
};

const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const PROCESS_GRACE: Duration = Duration::from_secs(3);
const PROCESS_KILL_WAIT: Duration = Duration::from_secs(3);
const SOCKET_REQUEST_LIMIT: u64 = 1024 * 1024;
const LSP_MESSAGE_LIMIT: usize = 16 * 1024 * 1024;
const STREAM_BUDGET: NonZeroUsize = NonZeroUsize::new(4 * 1024 * 1024).unwrap();
const DISCOVERY_MATCH_LIMIT: usize = 256;
const MAX_TRACE_DEPTH: u32 = 64;
const MAX_TRACE_NODES: usize = 4_096;
const MAX_TRACE_PATHS: usize = 1_024;

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
            ServiceCommand::Trace { selector, direction, limits } => {
                match lsp.trace(selector, direction, limits).await {
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
                        let _ = message.response.send(ActorReply::Failure {
                            message: detail,
                            fatal: true,
                        });
                        shutdown.notify_waiters();
                        break;
                    }
                }
            }
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
                let _ = message.response.send(ActorReply::Success {
                    service,
                    result,
                    stop: true,
                });
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
            let sent = actor
                .send(ActorMessage { command: request.command, response: response_tx })
                .await;
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
                    Err(_) => ServiceReply::error(
                        request_id,
                        "tsgo service request owner stopped",
                        true,
                    ),
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

struct DocumentState {
    version: i64,
    text: String,
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
}

impl LspSession {
    async fn start(processes: &ProcessSupervisor, identity: &ServiceIdentity) -> Result<Self> {
        let environment = ProcessEnvironment::new(
            EnvironmentBase::Inherit,
            BTreeMap::new(),
            BTreeSet::new(),
        )?;
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
                    "capabilities": {
                        "workspace": {
                            "configuration": true,
                            "workspaceFolders": true,
                            "symbol": { "dynamicRegistration": false }
                        },
                        "textDocument": {
                            "synchronization": { "dynamicRegistration": false, "didSave": false },
                            "callHierarchy": { "dynamicRegistration": false }
                        }
                    }
                })),
            )
            .await?;
        if result.pointer("/capabilities/callHierarchyProvider").is_none() {
            bail!("native tsgo did not advertise call hierarchy support");
        }
        self.notify("initialized", Some(json!({}))).await
    }

    async fn trace(
        &mut self,
        selector: TraceSelector,
        direction: TraceDirection,
        limits: TraceLimits,
    ) -> Result<Value> {
        validate_trace_limits(limits)?;
        let started = Instant::now();
        let request_start = self.next_request_id;
        let selector_name = selector.display_name();
        let (prepared, candidates, discovery) = self.resolve_selector(&selector).await?;
        let prepared_items = prepared
            .as_array()
            .context("native tsgo returned a non-array call hierarchy preparation result")?;

        let mut result = empty_trace_result(selector_name, direction, candidates, discovery);
        match prepared_items.len() {
            0 => result.status = "not-found".to_owned(),
            1 => {
                result = self
                    .traverse(
                        prepared_items[0].clone(),
                        result,
                        direction,
                        limits,
                    )
                    .await?;
            }
            _ => {
                result.status = "ambiguous".to_owned();
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
                result.status = "truncated".to_owned();
                result
                    .truncation_reasons
                    .push("symbol discovery candidate limit reached".to_owned());
            }
            result.summary.truncated = result.discovery.truncated;
        }
        result.timing = TraceTiming {
            elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            native_requests: self.next_request_id.saturating_sub(request_start),
        };
        serde_json::to_value(result).context("encode typed tsgo trace result")
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
                let character = self
                    .declaration_character(&file, line, name)
                    .unwrap_or(fallback_character);
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
        let response = self
            .request("workspace/symbol", Some(json!({ "query": leaf })))
            .await?;
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
        self.synchronize_document(file).await?;
        self.request(
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
        identifier_offsets(source_line, name).next().and_then(|offset| {
            source_line[..offset].encode_utf16().count().try_into().ok()
        })
    }

    async fn traverse(
        &mut self,
        root_item: Value,
        mut result: TraceResult,
        direction: TraceDirection,
        limits: TraceLimits,
    ) -> Result<TraceResult> {
        let (target_id, target_node) = trace_node(&root_item, &self.workspace)?;
        result.target = Some(target_id.clone());
        result.nodes.insert(target_id.clone(), target_node);

        let mut items = HashMap::from([(target_id.clone(), root_item)]);
        let mut queue = VecDeque::from([(target_id.clone(), 0u32)]);
        let mut expanded = BTreeSet::new();
        let mut edges = BTreeMap::<(String, String), TraceEdge>::new();
        let mut adjacency = BTreeMap::<String, BTreeSet<String>>::new();
        let mut boundaries = BTreeSet::new();
        let mut truncation_reasons = BTreeSet::new();

        while let Some((current_id, depth)) = queue.pop_front() {
            if !expanded.insert(current_id.clone()) {
                continue;
            }
            let current = items
                .get(&current_id)
                .cloned()
                .context("trace graph lost its native call hierarchy item")?;
            let current_node = result
                .nodes
                .get(&current_id)
                .context("trace graph lost its normalized node")?;
            if current_node.external {
                boundaries.insert(current_id);
                continue;
            }
            self.synchronize_item_document(&current).await?;
            let method = match direction {
                TraceDirection::Callers => "callHierarchy/incomingCalls",
                TraceDirection::Callees => "callHierarchy/outgoingCalls",
            };
            let response = self.request(method, Some(json!({ "item": current }))).await?;
            let calls = match response.as_array() {
                Some(calls) => calls,
                None if response.is_null() => continue,
                None => bail!("native tsgo returned a non-array {method} result"),
            };
            if depth >= limits.max_depth && !calls.is_empty() {
                truncation_reasons.insert(format!(
                    "maximum depth {} reached",
                    limits.max_depth
                ));
                continue;
            }

            for call in calls {
                let other = match direction {
                    TraceDirection::Callers => call.get("from"),
                    TraceDirection::Callees => call.get("to"),
                }
                .context("native tsgo call omitted its related item")?;
                let (other_id, other_node) = trace_node(other, &self.workspace)?;
                let is_new = !result.nodes.contains_key(&other_id);
                if is_new && result.nodes.len() >= limits.max_nodes {
                    truncation_reasons.insert(format!(
                        "maximum node count {} reached",
                        limits.max_nodes
                    ));
                    continue;
                }
                if is_new {
                    if other_node.external {
                        boundaries.insert(other_id.clone());
                    }
                    result.nodes.insert(other_id.clone(), other_node);
                    items.insert(other_id.clone(), other.clone());
                    queue.push_back((other_id.clone(), depth + 1));
                }

                let (caller, callee, caller_item) = match direction {
                    TraceDirection::Callers => {
                        (other_id.clone(), current_id.clone(), other)
                    }
                    TraceDirection::Callees => {
                        (current_id.clone(), other_id.clone(), &current)
                    }
                };
                let call_sites = trace_call_sites(call, caller_item, &self.workspace)?;
                let cycle = caller == callee || path_exists(&adjacency, &callee, &caller);
                let edge = edges
                    .entry((caller.clone(), callee.clone()))
                    .or_insert_with(|| TraceEdge {
                        caller: caller.clone(),
                        callee: callee.clone(),
                        call_sites: Vec::new(),
                        cycle,
                    });
                edge.cycle |= cycle;
                for site in call_sites {
                    if !edge.call_sites.contains(&site) {
                        edge.call_sites.push(site);
                    }
                }
                adjacency.entry(caller).or_default().insert(callee);
            }
        }

        result.edges = edges.into_values().collect();
        let (paths, path_truncated) = enumerate_paths(
            direction,
            &target_id,
            &result.edges,
            limits.max_paths,
        );
        if path_truncated {
            truncation_reasons.insert(format!(
                "maximum path count {} reached",
                limits.max_paths
            ));
        }
        result.paths = paths;
        result.roots = result
            .paths
            .iter()
            .filter_map(|path| path.nodes.first().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let cycle_count = result.edges.iter().filter(|edge| edge.cycle).count();
        result.truncation_reasons = truncation_reasons.into_iter().collect();
        let truncated = !result.truncation_reasons.is_empty() || result.discovery.truncated;
        result.status = if truncated { "truncated" } else { "complete" }.to_owned();
        result.summary = TraceSummary {
            roots: result.roots.len(),
            paths: result.paths.len(),
            nodes: result.nodes.len(),
            edges: result.edges.len(),
            cycles: cycle_count,
            boundaries: boundaries.len(),
            truncated,
        };
        Ok(result)
    }

    async fn synchronize_item_document(&mut self, item: &Value) -> Result<()> {
        let Some(file) = item_file(item) else {
            return Ok(());
        };
        let canonical = match file.canonicalize() {
            Ok(file) => file,
            Err(_) => return Ok(()),
        };
        if canonical.starts_with(&self.workspace) {
            self.synchronize_document(&canonical).await?;
        }
        Ok(())
    }

    async fn synchronize_document(&mut self, file: &Path) -> Result<()> {
        let text = tokio::fs::read_to_string(file)
            .await
            .with_context(|| format!("read TypeScript file {}", file.display()))?;
        let uri = file_uri(file)?;
        match self.documents.get_mut(file) {
            None => {
                self.notify(
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
                self.notify(
                    "textDocument/didChange",
                    Some(json!({
                        "textDocument": { "uri": uri, "version": 2 },
                        "contentChanges": [{ "text": text }]
                    })),
                )
                .await?;
                self.documents.insert(file.to_path_buf(), DocumentState { version: 2, text });
            }
            Some(document) if document.text != text => {
                document.version += 1;
                document.text.clone_from(&text);
                let version = document.version;
                self.notify(
                    "textDocument/didChange",
                    Some(json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }]
                    })),
                )
                .await?;
            }
            Some(_) => {}
        }
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_request_id;
        self.next_request_id += 1;
        let mut message = Map::new();
        message.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
        message.insert("id".to_owned(), json!(id));
        message.insert("method".to_owned(), Value::String(method.to_owned()));
        if let Some(params) = params {
            message.insert("params".to_owned(), params);
        }
        self.send(Value::Object(message)).await?;

        loop {
            let message = self.read_message().await?;
            if let Some(server_method) = message.get("method").and_then(Value::as_str) {
                if let Some(server_id) = message.get("id").cloned() {
                    self.reply_to_server(server_id, server_method, message.get("params"))
                        .await?;
                }
                continue;
            }
            if message.get("id") != Some(&json!(id)) {
                bail!("native tsgo returned an unexpected response id");
            }
            if let Some(error) = message.get("error") {
                bail!("native tsgo {method} error: {error}");
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<()> {
        let mut message = Map::new();
        message.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
        message.insert("method".to_owned(), Value::String(method.to_owned()));
        if let Some(params) = params {
            message.insert("params".to_owned(), params);
        }
        self.send(Value::Object(message)).await
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

struct DiscoveryFiles {
    files: Vec<PathBuf>,
    scanned_files: usize,
    truncated: bool,
}

fn validate_trace_limits(limits: TraceLimits) -> Result<()> {
    if limits.max_depth > MAX_TRACE_DEPTH {
        bail!("--max-depth may not exceed {MAX_TRACE_DEPTH}");
    }
    if limits.max_nodes == 0 || limits.max_nodes > MAX_TRACE_NODES {
        bail!("--max-nodes must be between 1 and {MAX_TRACE_NODES}");
    }
    if limits.max_paths == 0 || limits.max_paths > MAX_TRACE_PATHS {
        bail!("--max-paths must be between 1 and {MAX_TRACE_PATHS}");
    }
    Ok(())
}

fn empty_trace_result(
    selector: String,
    direction: TraceDirection,
    candidates: Vec<TraceCandidate>,
    discovery: TraceDiscovery,
) -> TraceResult {
    TraceResult {
        status: String::new(),
        selector,
        direction,
        target: None,
        candidates,
        nodes: BTreeMap::new(),
        edges: Vec::new(),
        roots: Vec::new(),
        paths: Vec::new(),
        summary: TraceSummary::default(),
        timing: TraceTiming::default(),
        discovery,
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
    let walker = WalkBuilder::new(scan_root)
        .standard_filters(true)
        .follow_links(false)
        .build();
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
        symbol
            .pointer("/location/range/start/line")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
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
    let uri = item
        .get("uri")
        .and_then(Value::as_str)
        .context("call hierarchy item omitted its URI")?;
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
        hash.finalize()[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let node = TraceNode {
        id: id.clone(),
        name,
        detail,
        kind,
        definition: public_location(workspace, canonical, line, character),
        external,
    };
    Ok((id, node))
}

fn item_location(item: &Value) -> Result<(PathBuf, u32, u32)> {
    let uri = item
        .get("uri")
        .and_then(Value::as_str)
        .context("call hierarchy item omitted its URI")?;
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

fn public_location(
    workspace: &Path,
    file: PathBuf,
    line: u32,
    character: u32,
) -> TraceLocation {
    let file = file.strip_prefix(workspace).map(Path::to_path_buf).unwrap_or(file);
    TraceLocation { file, line: line + 1, character: character + 1 }
}

fn trace_call_sites(
    call: &Value,
    caller_item: &Value,
    workspace: &Path,
) -> Result<Vec<TraceLocation>> {
    let unresolved_file =
        item_file(caller_item).context("call hierarchy caller omitted a usable file URI")?;
    let file = unresolved_file.canonicalize().unwrap_or(unresolved_file);
    let ranges = call
        .get("fromRanges")
        .and_then(Value::as_array)
        .context("native tsgo call omitted fromRanges")?;
    ranges
        .iter()
        .map(|range| {
            let start = range.get("start").context("call range omitted its start")?;
            let (line, character) = value_position(start)?;
            Ok(public_location(workspace, file.clone(), line, character))
        })
        .collect()
}

fn uri_file_path(uri: &str) -> Result<PathBuf> {
    Url::parse(uri)
        .with_context(|| format!("parse native tsgo URI {uri}"))?
        .to_file_path()
        .map_err(|()| anyhow!("native tsgo returned a non-file URI: {uri}"))
}

fn path_exists(
    adjacency: &BTreeMap<String, BTreeSet<String>>,
    start: &str,
    target: &str,
) -> bool {
    let mut pending = vec![start.to_owned()];
    let mut seen = BTreeSet::new();
    while let Some(node) = pending.pop() {
        if node == target {
            return true;
        }
        if !seen.insert(node.clone()) {
            continue;
        }
        if let Some(next) = adjacency.get(&node) {
            pending.extend(next.iter().cloned());
        }
    }
    false
}

fn enumerate_paths(
    direction: TraceDirection,
    target: &str,
    edges: &[TraceEdge],
    max_paths: usize,
) -> (Vec<TracePath>, bool) {
    let mut forward = BTreeMap::<String, BTreeSet<String>>::new();
    let mut reverse = BTreeMap::<String, BTreeSet<String>>::new();
    for edge in edges {
        forward.entry(edge.caller.clone()).or_default().insert(edge.callee.clone());
        reverse.entry(edge.callee.clone()).or_default().insert(edge.caller.clone());
    }
    let adjacency = match direction {
        TraceDirection::Callers => &reverse,
        TraceDirection::Callees => &forward,
    };
    let mut paths = Vec::new();
    let mut path = vec![target.to_owned()];
    let mut visited = BTreeSet::from([target.to_owned()]);
    let mut truncated = false;
    enumerate_path_branch(
        target,
        direction,
        adjacency,
        &mut path,
        &mut visited,
        &mut paths,
        max_paths,
        &mut truncated,
    );
    (paths, truncated)
}

#[allow(clippy::too_many_arguments)]
fn enumerate_path_branch(
    current: &str,
    direction: TraceDirection,
    adjacency: &BTreeMap<String, BTreeSet<String>>,
    path: &mut Vec<String>,
    visited: &mut BTreeSet<String>,
    output: &mut Vec<TracePath>,
    max_paths: usize,
    truncated: &mut bool,
) {
    if output.len() >= max_paths {
        *truncated = true;
        return;
    }
    let next = adjacency.get(current).cloned().unwrap_or_default();
    if next.is_empty() {
        let mut nodes = path.clone();
        if matches!(direction, TraceDirection::Callers) {
            nodes.reverse();
        }
        output.push(TracePath { nodes, cycle: false });
        return;
    }
    for node in next {
        if output.len() >= max_paths {
            *truncated = true;
            break;
        }
        path.push(node.clone());
        if visited.contains(&node) {
            let mut nodes = path.clone();
            if matches!(direction, TraceDirection::Callers) {
                nodes.reverse();
            }
            output.push(TracePath { nodes, cycle: true });
        } else {
            visited.insert(node.clone());
            enumerate_path_branch(
                &node,
                direction,
                adjacency,
                path,
                visited,
                output,
                max_paths,
                truncated,
            );
            visited.remove(&node);
        }
        path.pop();
    }
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
        _ => "typescript",
    }
}
