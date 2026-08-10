use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsString,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, bail, Context as _, Result};
use serde_json::{json, Map, Value};
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
    CallKind, ChildIdentity, ServiceCommand, ServiceIdentity, ServiceInfo, ServiceReply,
    ServiceRequest,
};

const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const PROCESS_GRACE: Duration = Duration::from_secs(3);
const PROCESS_KILL_WAIT: Duration = Duration::from_secs(3);
const SOCKET_REQUEST_LIMIT: u64 = 1024 * 1024;
const LSP_MESSAGE_LIMIT: usize = 16 * 1024 * 1024;
const STREAM_BUDGET: NonZeroUsize = NonZeroUsize::new(4 * 1024 * 1024).unwrap();

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
            ServiceCommand::Call { kind, file, line, character, item } => {
                match lsp.call(kind, &file, line, character, item).await {
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
                        "workspace": { "configuration": true, "workspaceFolders": true },
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

    async fn call(
        &mut self,
        kind: CallKind,
        file: &Path,
        line: u32,
        character: u32,
        item: usize,
    ) -> Result<Value> {
        let file = file
            .canonicalize()
            .with_context(|| format!("canonicalize TypeScript file {}", file.display()))?;
        if !file.starts_with(&self.workspace) {
            bail!("{} is outside workspace {}", file.display(), self.workspace.display());
        }
        self.synchronize_document(&file).await?;
        let uri = file_uri(&file)?;
        let prepared = self
            .request(
                "textDocument/prepareCallHierarchy",
                Some(json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": line, "character": character }
                })),
            )
            .await?;
        if matches!(kind, CallKind::Prepare) {
            return Ok(prepared);
        }
        let items = prepared
            .as_array()
            .context("native tsgo returned a non-array call hierarchy preparation result")?;
        let selected = items.get(item).cloned().with_context(|| {
            format!("call hierarchy item {item} is unavailable ({} prepared)", items.len())
        })?;
        let method = match kind {
            CallKind::Incoming => "callHierarchy/incomingCalls",
            CallKind::Outgoing => "callHierarchy/outgoingCalls",
            CallKind::Prepare => unreachable!(),
        };
        self.request(method, Some(json!({ "item": selected }))).await
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
                self.documents.insert(file.to_path_buf(), DocumentState { version: 1, text });
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
