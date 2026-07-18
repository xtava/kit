use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io::Write,
    marker::PhantomData,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use schemars::JsonSchema;
use serde::{de::DeserializeOwned, Deserialize};
use thiserror::Error;

use crate::framework::process::{
    CaptureOverflow, CapturePolicy, CommandSpec, CommandSpecError, CompletionCause,
    ContainmentRequirement, EnvironmentBase, InputPolicy, LeaderExit, LeaderExitObservation,
    OutputPolicy, OutputReport, PrivateBytes, ProcessByteEvent, ProcessByteStream, ProcessControl,
    ProcessControlError, ProcessDeadline, ProcessEnvironment, ProcessEnvironmentError,
    ProcessFailureReport, ProcessInputCompletion, ProcessInputError, ProcessInputHandle,
    ProcessOutputError, ProcessOutputHandle, ProcessSession, ProcessSpec, ProcessStartError,
    ProcessSupervisor, StreamPolicy, TerminationPolicy,
};

use super::model::{
    validate_planner_output, AgentOutput, CodexItem, CodexItemKind, CommandExecutionStatus,
    DevilOutput, ExpertOutput, FileChangeStatus, FileUpdate, ItemLifecycle, McpToolCallStatus,
    PlannerOutput, ReasoningEffort, RebuttalOutput, SynthesisOutput, TodoEntry, Usage,
};

const STREAM_BYTE_BUDGET: usize = 1024 * 1024;
const JSONL_RECORD_BYTE_LIMIT: usize = 8 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const EXIT_GRACE: Duration = Duration::from_secs(2);
static SCHEMA_NONCE: AtomicU64 = AtomicU64::new(1);

mod sealed {
    pub trait Sealed {}
}

pub trait StructuredOutput:
    sealed::Sealed + Clone + DeserializeOwned + JsonSchema + Send + Sync + 'static
{
    const NAME: &'static str;

    fn validate(&self) -> Result<(), String>;
    fn agent_output(&self) -> AgentOutput;
}

macro_rules! structured_output {
    ($type:ty, $name:literal, $variant:ident, $validate:expr) => {
        impl sealed::Sealed for $type {}
        impl StructuredOutput for $type {
            const NAME: &'static str = $name;

            fn validate(&self) -> Result<(), String> {
                ($validate)(self)
            }

            fn agent_output(&self) -> AgentOutput {
                AgentOutput::$variant(self.clone())
            }
        }
    };
}

structured_output!(PlannerOutput, "planner", Planner, validate_planner_output);
structured_output!(ExpertOutput, "expert", Expert, |output: &ExpertOutput| {
    validate_text_fields(&[&output.analysis, &output.recommendation])
});
structured_output!(RebuttalOutput, "rebuttal", Rebuttal, |output: &RebuttalOutput| {
    validate_text_fields(&[&output.revised_analysis, &output.recommendation])
});
structured_output!(DevilOutput, "devil", Devil, |output: &DevilOutput| {
    validate_nonempty_lists(&[
        &output.strongest_objections,
        &output.failure_modes,
        &output.required_corrections,
    ])
});
structured_output!(SynthesisOutput, "synthesis", Synthesis, |output: &SynthesisOutput| {
    validate_text_fields(&[&output.answer, &output.confidence])
});

fn validate_text_fields(fields: &[&String]) -> Result<(), String> {
    if fields.iter().any(|field| field.trim().is_empty()) {
        return Err("structured output contains an empty required text field".to_owned());
    }
    Ok(())
}

fn validate_nonempty_lists(lists: &[&Vec<String>]) -> Result<(), String> {
    if lists.iter().any(|list| list.is_empty() || list.iter().any(|item| item.trim().is_empty())) {
        return Err("structured output contains an empty required list or list item".to_owned());
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct CodexClient {
    executable: PathBuf,
    working_directory: PathBuf,
    processes: ProcessSupervisor,
}

#[derive(Clone, Debug)]
pub struct TurnRequest {
    pub prompt: String,
    pub model: Option<String>,
    pub reasoning: ReasoningEffort,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransportEvent {
    ThreadStarted { thread_id: String },
    TurnStarted,
    Item { lifecycle: ItemLifecycle, item: CodexItem },
    TurnCompleted { usage: Usage },
    TurnFailed { message: String },
    Error { message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnResult<T> {
    pub thread_id: String,
    pub output: T,
    pub usage: Usage,
}

#[derive(Debug, Error)]
pub enum CodexError {
    #[error("Codex prompt must not be empty")]
    EmptyPrompt,
    #[error("Codex working directory does not exist: {}", .0.display())]
    MissingWorkingDirectory(PathBuf),
    #[error("create structured-output schema: {0}")]
    Schema(#[from] SchemaError),
    #[error("construct Codex command: {0}")]
    Command(#[from] CommandSpecError),
    #[error("construct Codex environment: {0}")]
    Environment(#[from] ProcessEnvironmentError),
    #[error("start Codex process: {0}")]
    Start(#[from] ProcessStartError),
    #[error("write Codex prompt: {0}")]
    Input(#[from] ProcessInputError),
    #[error("read Codex JSONL stream: {0}")]
    Output(#[from] ProcessOutputError),
    #[error("decode Codex JSONL event: {0}")]
    Protocol(#[from] serde_json::Error),
    #[error("Codex JSONL record exceeded the {limit}-byte protocol limit")]
    ProtocolRecordTooLarge { limit: usize },
    #[error("Codex protocol violation: {0}")]
    ProtocolState(String),
    #[error("Codex turn failed: {0}")]
    TurnFailed(String),
    #[error("Codex exited with {leader_exit:?}: {stderr}")]
    Exit { leader_exit: LeaderExit, stderr: String },
    #[error("Codex process supervision failed: {0:?}")]
    ProcessFailure(ProcessFailureReport),
    #[error("Codex completed with unexpected cause {0:?}")]
    Completion(CompletionCause),
    #[error("control Codex process: {0}")]
    Control(#[from] ProcessControlError),
    #[error("finalize Codex process: {0}")]
    Finalization(String),
    #[error("Codex structured response was missing")]
    MissingResponse,
    #[error("decode {kind} structured response: {source}")]
    StructuredDecode {
        kind: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid {kind} structured response: {message}")]
    StructuredValidation { kind: &'static str, message: String },
}

impl CodexClient {
    pub fn installed(working_directory: PathBuf, processes: ProcessSupervisor) -> Self {
        Self { executable: PathBuf::from("codex"), working_directory, processes }
    }

    pub fn new(
        executable: PathBuf,
        working_directory: PathBuf,
        processes: ProcessSupervisor,
    ) -> Self {
        Self { executable, working_directory, processes }
    }

    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    pub async fn start<T: StructuredOutput>(
        &self,
        request: TurnRequest,
    ) -> Result<CodexTurn<T>, CodexError> {
        self.spawn(None, request).await
    }

    pub async fn resume<T: StructuredOutput>(
        &self,
        thread_id: &str,
        request: TurnRequest,
    ) -> Result<CodexTurn<T>, CodexError> {
        if thread_id.trim().is_empty() {
            return Err(CodexError::ProtocolState("resume thread id must not be empty".to_owned()));
        }
        self.spawn(Some(thread_id), request).await
    }

    async fn spawn<T: StructuredOutput>(
        &self,
        resume_thread_id: Option<&str>,
        request: TurnRequest,
    ) -> Result<CodexTurn<T>, CodexError> {
        if request.prompt.trim().is_empty() {
            return Err(CodexError::EmptyPrompt);
        }
        if !self.working_directory.is_dir() {
            return Err(CodexError::MissingWorkingDirectory(self.working_directory.clone()));
        }
        let schema = SchemaFile::create::<T>()?;
        let mut arguments = vec![
            "exec".into(),
            "--json".into(),
            "--color".into(),
            "never".into(),
            "--sandbox".into(),
            "read-only".into(),
            "--skip-git-repo-check".into(),
            "--cd".into(),
            self.working_directory.as_os_str().to_owned(),
            "--output-schema".into(),
            schema.path().as_os_str().to_owned(),
            "--config".into(),
            "approval_policy=\"never\"".into(),
            "--config".into(),
            "web_search=\"disabled\"".into(),
            "--config".into(),
            "sandbox_workspace_write.network_access=false".into(),
            "--config".into(),
            format!("model_reasoning_effort=\"{}\"", reasoning_name(request.reasoning)).into(),
        ];
        if let Some(model) = request.model.as_ref() {
            if model.trim().is_empty() {
                return Err(CodexError::ProtocolState("model must not be empty".to_owned()));
            }
            arguments.push("--model".into());
            arguments.push(model.as_str().into());
        }
        if let Some(thread_id) = resume_thread_id {
            arguments.push("resume".into());
            arguments.push(thread_id.into());
        }
        arguments.push("-".into());

        let environment = ProcessEnvironment::new(
            EnvironmentBase::Inherit,
            BTreeMap::from([(
                OsString::from("CODEX_INTERNAL_ORIGINATOR_OVERRIDE"),
                "kit_swarm".into(),
            )]),
            BTreeSet::new(),
        )?;
        let command = CommandSpec::new(
            self.executable.as_os_str().to_owned(),
            arguments,
            self.working_directory.clone(),
            environment,
            crate::framework::process::ProcessLabel::new("Codex swarm turn".to_owned())
                .expect("static process label"),
        )?;
        let spec = ProcessSpec::new(
            command,
            InputPolicy::Once(PrivateBytes::new(request.prompt.into_bytes())),
            OutputPolicy::Stream(StreamPolicy::new(
                NonZeroUsize::new(STREAM_BYTE_BUDGET).expect("stream budget is nonzero"),
            )),
            OutputPolicy::Capture(CapturePolicy::new(
                NonZeroUsize::new(STDERR_LIMIT).expect("stderr limit is nonzero"),
                CaptureOverflow::TruncateWithEvidence,
            )),
            ContainmentRequirement::CompleteTree,
            ProcessDeadline::Unlimited,
            TerminationPolicy::new(EXIT_GRACE),
        );
        let started = self.processes.spawn(spec).await?;
        let input = match started.input {
            ProcessInputHandle::Once(input) => input,
            _ => {
                return Err(CodexError::ProtocolState("Codex input policy was not honored".into()))
            }
        };
        let stdout = match started.stdout {
            ProcessOutputHandle::Stream(stdout) => stdout,
            _ => {
                return Err(CodexError::ProtocolState("Codex output policy was not honored".into()))
            }
        };
        if !matches!(started.stderr, ProcessOutputHandle::CapturedAtCompletion) {
            return Err(CodexError::ProtocolState("Codex stderr policy was not honored".into()));
        }
        let control = started.session.control();

        Ok(CodexTurn {
            session: Some(started.session),
            control,
            input: Some(input),
            stdout: Some(stdout),
            stdout_buffer: Vec::new(),
            stdout_ended: false,
            schema: Some(schema),
            expected_thread_id: resume_thread_id.map(str::to_owned),
            thread_id: None,
            final_response: None,
            usage: None,
            failure: None,
            seen_event: false,
            turn_completed: false,
            stream_ended: false,
            output: PhantomData,
        })
    }
}

pub struct CodexTurn<T> {
    session: Option<ProcessSession>,
    control: ProcessControl,
    input: Option<ProcessInputCompletion>,
    stdout: Option<ProcessByteStream>,
    stdout_buffer: Vec<u8>,
    stdout_ended: bool,
    schema: Option<SchemaFile>,
    expected_thread_id: Option<String>,
    thread_id: Option<String>,
    final_response: Option<String>,
    usage: Option<Usage>,
    failure: Option<String>,
    seen_event: bool,
    turn_completed: bool,
    stream_ended: bool,
    output: PhantomData<T>,
}

impl<T: StructuredOutput> CodexTurn<T> {
    pub async fn next_event(&mut self) -> Result<Option<TransportEvent>, CodexError> {
        let raw = loop {
            if let Some(end) = self.stdout_buffer.iter().position(|byte| *byte == b'\n') {
                let line = self.stdout_buffer.drain(..=end).collect::<Vec<_>>();
                break serde_json::from_slice::<RawThreadEvent>(&line)?;
            }
            if self.stdout_ended {
                if self.stdout_buffer.is_empty() {
                    self.stream_ended = true;
                    return Ok(None);
                }
                let line = std::mem::take(&mut self.stdout_buffer);
                break serde_json::from_slice::<RawThreadEvent>(&line)?;
            }
            let stdout =
                self.stdout.as_mut().expect("a live Codex turn owns its stdout byte stream");
            match stdout.next().await? {
                ProcessByteEvent::Chunk { bytes, .. } => {
                    let bytes_before_delimiter =
                        bytes.iter().position(|byte| *byte == b'\n').unwrap_or(bytes.len());
                    if self.stdout_buffer.len().saturating_add(bytes_before_delimiter)
                        > JSONL_RECORD_BYTE_LIMIT
                    {
                        return Err(CodexError::ProtocolRecordTooLarge {
                            limit: JSONL_RECORD_BYTE_LIMIT,
                        });
                    }
                    self.stdout_buffer.extend_from_slice(&bytes);
                }
                ProcessByteEvent::End => self.stdout_ended = true,
            }
        };
        if !self.seen_event && !matches!(raw, RawThreadEvent::ThreadStarted { .. }) {
            return Err(CodexError::ProtocolState(
                "thread.started was not the first JSONL event".to_owned(),
            ));
        }
        if self.turn_completed {
            return Err(CodexError::ProtocolState(
                "event followed turn.completed in the JSONL stream".to_owned(),
            ));
        }
        self.seen_event = true;
        let event = match raw {
            RawThreadEvent::ThreadStarted { thread_id } => {
                if thread_id.trim().is_empty() || self.thread_id.is_some() {
                    return Err(CodexError::ProtocolState(
                        "thread.started contained an empty or duplicate id".to_owned(),
                    ));
                }
                if self.expected_thread_id.as_ref().is_some_and(|expected| expected != &thread_id) {
                    return Err(CodexError::ProtocolState(
                        "resumed thread id does not match requested thread".to_owned(),
                    ));
                }
                self.thread_id = Some(thread_id.clone());
                TransportEvent::ThreadStarted { thread_id }
            }
            RawThreadEvent::TurnStarted {} => TransportEvent::TurnStarted,
            RawThreadEvent::ItemStarted { item } => TransportEvent::Item {
                lifecycle: ItemLifecycle::Started,
                item: normalize_item(item),
            },
            RawThreadEvent::ItemUpdated { item } => TransportEvent::Item {
                lifecycle: ItemLifecycle::Updated,
                item: normalize_item(item),
            },
            RawThreadEvent::ItemCompleted { item } => {
                let item = normalize_item(item);
                if let CodexItemKind::AgentMessage { text } = &item.kind {
                    self.final_response = Some(text.clone());
                }
                TransportEvent::Item { lifecycle: ItemLifecycle::Completed, item }
            }
            RawThreadEvent::TurnCompleted { usage } => {
                self.turn_completed = true;
                self.usage = Some(usage.clone());
                TransportEvent::TurnCompleted { usage }
            }
            RawThreadEvent::TurnFailed { error } => {
                self.failure = Some(error.message.clone());
                TransportEvent::TurnFailed { message: error.message }
            }
            RawThreadEvent::Error { message } => {
                self.failure = Some(message.clone());
                TransportEvent::Error { message }
            }
        };
        Ok(Some(event))
    }

    pub async fn finish(mut self) -> Result<TurnResult<T>, CodexError> {
        if !self.stream_ended {
            return Err(CodexError::ProtocolState(
                "finish requires draining the JSONL stream".to_owned(),
            ));
        }
        let input = self.input.take();
        let session = self.session.take().expect("Codex turn owns one process session");
        let (input, report) = tokio::join!(wait_input_completion(input), session.wait());
        let report = report.map_err(CodexError::ProcessFailure)?;
        input?;
        let stderr = stderr_text(&report.stderr);
        if report.completion != CompletionCause::Natural {
            return Err(CodexError::Completion(report.completion));
        }
        let LeaderExitObservation::Observed(leader_exit) = report.leader_exit else {
            return Err(CodexError::ProtocolState(
                "Codex completed without an observed process leader".to_owned(),
            ));
        };
        if leader_exit != LeaderExit::Code(0) {
            return Err(CodexError::Exit { leader_exit, stderr });
        }
        if let Some(message) = self.failure.take() {
            return Err(CodexError::TurnFailed(message));
        }
        if !self.turn_completed {
            return Err(CodexError::ProtocolState("stream ended before turn.completed".to_owned()));
        }
        let thread_id = self.thread_id.take().ok_or_else(|| {
            CodexError::ProtocolState("stream ended without thread.started".to_owned())
        })?;
        let response = self.final_response.take().ok_or(CodexError::MissingResponse)?;
        let output: T = serde_json::from_str(&response)
            .map_err(|source| CodexError::StructuredDecode { kind: T::NAME, source })?;
        output
            .validate()
            .map_err(|message| CodexError::StructuredValidation { kind: T::NAME, message })?;
        Ok(TurnResult {
            thread_id,
            output,
            usage: self.usage.take().expect("turn_completed always records usage"),
        })
    }

    pub async fn terminate(mut self) -> Result<(), CodexError> {
        let cancelled = self.control.cancel().await;
        let mut stdout = self.stdout.take().expect("Codex turn owns stdout until completion");
        let session = self.session.take().expect("Codex turn owns one process session");
        let input = self.input.take();
        let (drained, completed, input) =
            tokio::join!(drain_stdout(&mut stdout), session.wait(), wait_input_completion(input),);
        let mut failures = Vec::new();
        if let Err(error) = cancelled {
            failures.push(format!("cancellation request failed ({error})"));
        }
        if let Err(error) = drained {
            failures.push(format!("stdout drain failed ({error})"));
        }
        if let Err(error) = input {
            failures.push(format!("prompt input completion failed ({error})"));
        }
        if let Err(error) = completed {
            failures.push(format!("terminal process proof failed ({error:?})"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(CodexError::Finalization(failures.join("; ")))
        }
    }
}

impl<T> Drop for CodexTurn<T> {
    fn drop(&mut self) {
        self.schema.take();
    }
}

async fn drain_stdout(stdout: &mut ProcessByteStream) -> Result<(), ProcessOutputError> {
    loop {
        match stdout.next().await? {
            ProcessByteEvent::Chunk { .. } => {}
            ProcessByteEvent::End => return Ok(()),
        }
    }
}

async fn wait_input_completion(
    input: Option<ProcessInputCompletion>,
) -> Result<(), ProcessInputError> {
    match input {
        Some(input) => input.wait().await,
        None => Ok(()),
    }
}

fn stderr_text(report: &OutputReport) -> String {
    match report {
        OutputReport::Captured(report) => String::from_utf8_lossy(&report.bytes).into_owned(),
        _ => String::new(),
    }
}

fn reasoning_name(reasoning: ReasoningEffort) -> &'static str {
    match reasoning {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Xhigh => "xhigh",
    }
}

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("create schema directory {}: {source}", path.display())]
    Directory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("serialize schema: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("write schema {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

struct SchemaFile {
    directory: PathBuf,
    path: PathBuf,
}

impl SchemaFile {
    fn create<T: StructuredOutput>() -> Result<Self, SchemaError> {
        let nonce = SCHEMA_NONCE.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("kit-swarm-schema-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory)
            .map_err(|source| SchemaError::Directory { path: directory.clone(), source })?;
        set_directory_private(&directory)?;
        let path = directory.join("schema.json");
        let schema = serde_json::to_vec(&schemars::schema_for!(T))?;
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|source| SchemaError::Write { path: path.clone(), source })?;
        file.write_all(&schema)
            .and_then(|()| file.sync_all())
            .map_err(|source| SchemaError::Write { path: path.clone(), source })?;
        Ok(Self { directory, path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SchemaFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn set_directory_private(path: &Path) -> Result<(), SchemaError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|source| SchemaError::Directory { path: path.to_path_buf(), source })?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum RawThreadEvent {
    #[serde(rename = "thread.started")]
    ThreadStarted { thread_id: String },
    #[serde(rename = "turn.started")]
    TurnStarted {},
    #[serde(rename = "item.started")]
    ItemStarted { item: RawItem },
    #[serde(rename = "item.updated")]
    ItemUpdated { item: RawItem },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: RawItem },
    #[serde(rename = "turn.completed")]
    TurnCompleted { usage: Usage },
    #[serde(rename = "turn.failed")]
    TurnFailed { error: RawError },
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum RawItem {
    CommandExecution {
        id: String,
        command: String,
        aggregated_output: String,
        exit_code: Option<i32>,
        status: CommandExecutionStatus,
    },
    FileChange {
        id: String,
        changes: Vec<FileUpdate>,
        status: FileChangeStatus,
    },
    McpToolCall {
        id: String,
        server: String,
        tool: String,
        arguments: serde_json::Value,
        result: Option<serde_json::Value>,
        error: Option<RawError>,
        status: McpToolCallStatus,
    },
    AgentMessage {
        id: String,
        text: String,
    },
    Reasoning {
        id: String,
        text: String,
    },
    WebSearch {
        id: String,
        query: String,
    },
    TodoList {
        id: String,
        items: Vec<TodoEntry>,
    },
    Error {
        id: String,
        message: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawError {
    message: String,
}

fn normalize_item(item: RawItem) -> CodexItem {
    match item {
        RawItem::CommandExecution { id, command, aggregated_output, exit_code, status } => {
            CodexItem {
                id,
                kind: CodexItemKind::CommandExecution {
                    command,
                    output: aggregated_output,
                    exit_code,
                    status,
                },
            }
        }
        RawItem::FileChange { id, changes, status } => {
            CodexItem { id, kind: CodexItemKind::FileChange { changes, status } }
        }
        RawItem::McpToolCall { id, server, tool, arguments, result, error, status } => CodexItem {
            id,
            kind: CodexItemKind::McpToolCall {
                server,
                tool,
                arguments: arguments.to_string(),
                result: result.map(|value| value.to_string()),
                error: error.map(|error| error.message),
                status,
            },
        },
        RawItem::AgentMessage { id, text } => {
            CodexItem { id, kind: CodexItemKind::AgentMessage { text } }
        }
        RawItem::Reasoning { id, text } => {
            CodexItem { id, kind: CodexItemKind::Reasoning { text } }
        }
        RawItem::WebSearch { id, query } => {
            CodexItem { id, kind: CodexItemKind::WebSearch { query } }
        }
        RawItem::TodoList { id, items } => {
            CodexItem { id, kind: CodexItemKind::TodoList { items } }
        }
        RawItem::Error { id, message } => CodexItem { id, kind: CodexItemKind::Error { message } },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kit-swarm-codex-{name}-{}", std::process::id()))
    }

    fn supervisor(directory: &Path) -> ProcessSupervisor {
        ProcessSupervisor::for_test(directory.join("processes")).unwrap()
    }

    fn planner_output() -> PlannerOutput {
        PlannerOutput {
            roles: (1..=3)
                .map(|index| super::super::model::ExpertRole {
                    title: format!("Role {index}"),
                    mandate: format!("Mandate {index}"),
                    perspective: format!("Perspective {index}"),
                })
                .collect(),
        }
    }

    fn write_fixture(directory: &Path, thread_id: &str, output: &PlannerOutput) {
        let response = serde_json::to_string(output).unwrap();
        let events = [
            json!({"type": "thread.started", "thread_id": thread_id}),
            json!({"type": "turn.started"}),
            json!({
                "type": "item.started",
                "item": {"id": "reason-1", "type": "reasoning", "text": "checking"}
            }),
            json!({
                "type": "item.completed",
                "item": {"id": "message-1", "type": "agent_message", "text": response}
            }),
            json!({
                "type": "turn.completed",
                "usage": {
                    "input_tokens": 10,
                    "cached_input_tokens": 2,
                    "output_tokens": 4,
                    "reasoning_output_tokens": 1
                }
            }),
        ];
        let jsonl =
            events.into_iter().map(|event| event.to_string()).collect::<Vec<_>>().join("\n") + "\n";
        std::fs::write(directory.join("events.jsonl"), jsonl).unwrap();
    }

    fn write_fake(directory: &Path) -> PathBuf {
        std::fs::create_dir_all(directory).unwrap();
        let executable = directory.join("fake-codex");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
dir=$(dirname "$0")
: > "$dir/args"
for arg in "$@"; do
  printf '%s\n' "$arg" >> "$dir/args"
done
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-schema" ]; then
    shift
    cp "$1" "$dir/schema.json"
  fi
  shift
done
cat > "$dir/prompt"
cat "$dir/events.jsonl"
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        executable
    }

    async fn drain<T: StructuredOutput>(turn: &mut CodexTurn<T>) -> Vec<TransportEvent> {
        let mut events = Vec::new();
        while let Some(event) = turn.next_event().await.unwrap() {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn fake_transport_proves_start_resume_stream_and_exact_generated_schema() {
        let directory = root("contract");
        let _ = std::fs::remove_dir_all(&directory);
        let executable = write_fake(&directory);
        let expected = planner_output();
        write_fixture(&directory, "thread-1", &expected);
        let client = CodexClient::new(executable, directory.clone(), supervisor(&directory));
        let prompt = "Plan this exactly".to_owned();
        let request = TurnRequest {
            prompt: prompt.clone(),
            model: Some("test-model".to_owned()),
            reasoning: ReasoningEffort::High,
        };

        let mut turn = client.start::<PlannerOutput>(request.clone()).await.unwrap();
        let events = drain(&mut turn).await;
        assert!(matches!(events.first(), Some(TransportEvent::ThreadStarted { .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            TransportEvent::Item { lifecycle: ItemLifecycle::Started, .. }
        )));
        let result = turn.finish().await.unwrap();
        assert_eq!(result.thread_id, "thread-1");
        assert_eq!(result.output, expected);
        assert_eq!(result.usage.output_tokens, 4);

        let args = std::fs::read_to_string(directory.join("args")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(args.first(), Some(&"exec"));
        assert!(args.windows(2).any(|pair| pair == ["--sandbox", "read-only"]));
        assert!(args.contains(&"--json"));
        assert!(args.contains(&"--output-schema"));
        assert!(args.windows(2).any(|pair| pair == ["--model", "test-model"]));
        assert_eq!(args.last(), Some(&"-"));
        assert!(!args.contains(&prompt.as_str()));
        assert_eq!(std::fs::read_to_string(directory.join("prompt")).unwrap(), prompt);
        let captured: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.join("schema.json")).unwrap()).unwrap();
        let generated = serde_json::to_value(schemars::schema_for!(PlannerOutput)).unwrap();
        assert_eq!(captured, generated);
        let schema_index = args.iter().position(|argument| *argument == "--output-schema").unwrap();
        assert!(!Path::new(args[schema_index + 1]).exists());

        write_fixture(&directory, "thread-1", &planner_output());
        let mut resumed = client.resume::<PlannerOutput>("thread-1", request).await.unwrap();
        drain(&mut resumed).await;
        resumed.finish().await.unwrap();
        let args = std::fs::read_to_string(directory.join("args")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert!(args.windows(2).any(|pair| pair == ["resume", "thread-1"]));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn unknown_protocol_event_fails_instead_of_disappearing() {
        let directory = root("unknown");
        let _ = std::fs::remove_dir_all(&directory);
        let executable = write_fake(&directory);
        std::fs::write(
            directory.join("events.jsonl"),
            "{\"type\":\"thread.started\",\"thread_id\":\"thread-1\"}\n{\"type\":\"future.event\"}\n",
        )
        .unwrap();
        let client = CodexClient::new(executable, directory.clone(), supervisor(&directory));
        let mut turn = client
            .start::<PlannerOutput>(TurnRequest {
                prompt: "prompt".to_owned(),
                model: None,
                reasoning: ReasoningEffort::Low,
            })
            .await
            .unwrap();
        assert!(turn.next_event().await.unwrap().is_some());
        assert!(matches!(turn.next_event().await, Err(CodexError::Protocol(_))));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn cancellation_waits_for_verified_containment_completion() {
        let directory = root("cancel");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let executable = directory.join("slow-codex");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
trap 'exit 0' TERM
cat > /dev/null
printf '%s\n' '{"type":"thread.started","thread_id":"slow-thread"}'
while :; do sleep 1; done
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let client = CodexClient::new(executable, directory.clone(), supervisor(&directory));
        let mut turn = client
            .start::<PlannerOutput>(TurnRequest {
                prompt: "prompt".to_owned(),
                model: None,
                reasoning: ReasoningEffort::Low,
            })
            .await
            .unwrap();
        assert!(turn.next_event().await.unwrap().is_some());
        turn.terminate().await.unwrap();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn decoder_covers_every_official_sdk_item_and_event_shape() {
        let events = vec![
            json!({"type": "thread.started", "thread_id": "thread-1"}),
            json!({"type": "turn.started"}),
            json!({
                "type": "item.started",
                "item": {
                    "id": "command", "type": "command_execution", "command": "pwd",
                    "aggregated_output": "", "status": "in_progress"
                }
            }),
            json!({
                "type": "item.updated",
                "item": {
                    "id": "file", "type": "file_change",
                    "changes": [{"path": "a.txt", "kind": "update"}], "status": "completed"
                }
            }),
            json!({
                "type": "item.completed",
                "item": {
                    "id": "mcp", "type": "mcp_tool_call", "server": "server", "tool": "tool",
                    "arguments": {"key": "value"},
                    "result": {"content": [], "structured_content": {}, "_meta": {}},
                    "status": "completed"
                }
            }),
            json!({
                "type": "item.completed",
                "item": {"id": "message", "type": "agent_message", "text": "hello"}
            }),
            json!({
                "type": "item.completed",
                "item": {"id": "reason", "type": "reasoning", "text": "summary"}
            }),
            json!({
                "type": "item.completed",
                "item": {"id": "search", "type": "web_search", "query": "query"}
            }),
            json!({
                "type": "item.updated",
                "item": {
                    "id": "todos", "type": "todo_list",
                    "items": [{"text": "step", "completed": false}]
                }
            }),
            json!({
                "type": "item.completed",
                "item": {"id": "item-error", "type": "error", "message": "non-fatal"}
            }),
            json!({
                "type": "turn.completed",
                "usage": {
                    "input_tokens": 1, "cached_input_tokens": 0, "output_tokens": 1,
                    "reasoning_output_tokens": 0
                }
            }),
            json!({"type": "turn.failed", "error": {"message": "failed"}}),
            json!({"type": "error", "message": "fatal"}),
        ];
        let mut normalized = Vec::new();
        for event in events {
            let event = serde_json::from_value::<RawThreadEvent>(event).unwrap();
            match event {
                RawThreadEvent::ItemStarted { item }
                | RawThreadEvent::ItemUpdated { item }
                | RawThreadEvent::ItemCompleted { item } => normalized.push(normalize_item(item)),
                _ => {}
            }
        }
        assert_eq!(normalized.len(), 8);
        assert!(matches!(&normalized[0].kind, CodexItemKind::CommandExecution { .. }));
        assert!(matches!(&normalized[1].kind, CodexItemKind::FileChange { .. }));
        assert!(matches!(&normalized[2].kind, CodexItemKind::McpToolCall { .. }));
        assert!(matches!(&normalized[3].kind, CodexItemKind::AgentMessage { .. }));
        assert!(matches!(&normalized[4].kind, CodexItemKind::Reasoning { .. }));
        assert!(matches!(&normalized[5].kind, CodexItemKind::WebSearch { .. }));
        assert!(matches!(&normalized[6].kind, CodexItemKind::TodoList { .. }));
        assert!(matches!(&normalized[7].kind, CodexItemKind::Error { .. }));
        assert!(serde_json::from_value::<RawThreadEvent>(
            json!({"type": "turn.started", "future_field": true})
        )
        .is_err());
    }

    #[tokio::test]
    async fn malformed_invalid_missing_and_nonzero_fail_explicitly() {
        let directory = root("negative");
        let _ = std::fs::remove_dir_all(&directory);
        let executable = write_fake(&directory);
        let processes = supervisor(&directory);
        let client = CodexClient::new(executable.clone(), directory.clone(), processes.clone());
        let request = TurnRequest {
            prompt: "prompt".to_owned(),
            model: None,
            reasoning: ReasoningEffort::Low,
        };

        std::fs::write(
            directory.join("events.jsonl"),
            "{\"type\":\"thread.started\",\"thread_id\":\"thread-1\"}\nnot-json\n",
        )
        .unwrap();
        let mut malformed = client.start::<PlannerOutput>(request.clone()).await.unwrap();
        malformed.next_event().await.unwrap();
        assert!(matches!(malformed.next_event().await, Err(CodexError::Protocol(_))));
        drop(malformed);

        let invalid = PlannerOutput { roles: Vec::new() };
        write_fixture(&directory, "thread-1", &invalid);
        let mut turn = client.start::<PlannerOutput>(request.clone()).await.unwrap();
        drain(&mut turn).await;
        assert!(matches!(
            turn.finish().await,
            Err(CodexError::StructuredValidation { kind: "planner", .. })
        ));

        let missing =
            CodexClient::new(directory.join("missing-codex"), directory.clone(), processes.clone());
        assert!(matches!(
            missing.start::<PlannerOutput>(request.clone()).await,
            Err(CodexError::Start(_))
        ));

        let failing = directory.join("failing-codex");
        std::fs::write(
            &failing,
            "#!/bin/sh\ncat >/dev/null\nprintf 'provider failed' >&2\nexit 7\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&failing, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let failing = CodexClient::new(failing, directory.clone(), processes);
        let mut turn = failing.start::<PlannerOutput>(request).await.unwrap();
        assert!(turn.next_event().await.unwrap().is_none());
        assert!(matches!(turn.finish().await, Err(CodexError::Exit { .. })));
        let _ = std::fs::remove_dir_all(directory);
    }
}
