use std::{collections::HashSet, path::PathBuf};

use schemars::JsonSchema;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::framework::process::{DetachedProcessReceipt, DetachedReceiptDecodeError, ProcessRunId};

pub const SWARM_SCHEMA_VERSION: u32 = 1;

macro_rules! validated_id {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                if value.is_empty()
                    || !value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                {
                    return Err(IdError { kind: $label, value });
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

validated_id!(SwarmId, "swarm id");
validated_id!(AgentId, "agent id");

#[derive(Debug, Error)]
#[error("invalid {kind} '{value}'; expected letters, numbers, '-' or '_'")]
pub struct IdError {
    kind: &'static str,
    value: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    #[default]
    High,
    Xhigh,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DebatePolicy {
    Disabled,
    #[default]
    Enabled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmSpec {
    pub schema_version: u32,
    pub id: SwarmId,
    pub prompt: String,
    pub working_directory: PathBuf,
    pub model: Option<String>,
    pub reasoning: ReasoningEffort,
    pub debate: DebatePolicy,
    pub created_at_ms: u64,
    pub retry_limit: u8,
}

impl SwarmSpec {
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.schema_version != SWARM_SCHEMA_VERSION {
            return Err(SpecError::Schema {
                actual: self.schema_version,
                expected: SWARM_SCHEMA_VERSION,
            });
        }
        if self.prompt.trim().is_empty() {
            return Err(SpecError::EmptyPrompt);
        }
        if !self.working_directory.is_absolute() {
            return Err(SpecError::RelativeWorkingDirectory(self.working_directory.clone()));
        }
        if self.model.as_ref().is_some_and(|model| model.trim().is_empty()) {
            return Err(SpecError::EmptyModel);
        }
        if self.retry_limit > 5 {
            return Err(SpecError::RetryLimit(self.retry_limit));
        }
        Ok(())
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SpecError {
    #[error("swarm schema version {actual} is unsupported; expected {expected}")]
    Schema { actual: u32, expected: u32 },
    #[error("swarm prompt must not be empty")]
    EmptyPrompt,
    #[error("swarm working directory must be absolute: {}", .0.display())]
    RelativeWorkingDirectory(PathBuf),
    #[error("swarm model must not be empty when supplied")]
    EmptyModel,
    #[error("swarm retry limit {0} exceeds maximum 5")]
    RetryLimit(u8),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Planning,
    Experts,
    Debate,
    Devil,
    Synthesis,
}

impl Stage {
    fn ordinal(self) -> u8 {
        match self {
            Self::Planning => 0,
            Self::Experts => 1,
            Self::Debate => 2,
            Self::Devil => 3,
            Self::Synthesis => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Queued,
    Waiting,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    Orphaned,
    Unavailable,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// Durable authority for a detached Swarm owner.
///
/// The receipt is journal-only control material. It is intentionally excluded from normal
/// projection serialization so `swarm show` and the TUI never disclose it.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmOwner {
    process_run_id: ProcessRunId,
    encoded_receipt: String,
}

impl SwarmOwner {
    pub fn new(receipt: DetachedProcessReceipt) -> Self {
        Self { process_run_id: receipt.run_id(), encoded_receipt: receipt.encode() }
    }

    pub fn process_run_id(&self) -> ProcessRunId {
        self.process_run_id
    }

    pub(crate) fn receipt(&self) -> Result<DetachedProcessReceipt, DetachedReceiptDecodeError> {
        DetachedProcessReceipt::decode(&self.encoded_receipt)
    }

    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        let receipt = DetachedProcessReceipt::linux_systemd(ProcessRunId::new(), "0".repeat(32))
            .expect("static test receipt is valid");
        Self::new(receipt)
    }
}

impl std::fmt::Debug for SwarmOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SwarmOwner")
            .field("process_run_id", &self.process_run_id)
            .field("encoded_receipt", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExpertRole {
    pub title: String,
    pub mandate: String,
    pub perspective: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlannerOutput {
    pub roles: Vec<ExpertRole>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExpertOutput {
    pub analysis: String,
    pub findings: Vec<String>,
    pub recommendation: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RebuttalOutput {
    pub revised_analysis: String,
    pub accepted_challenges: Vec<String>,
    pub rejected_challenges: Vec<String>,
    pub recommendation: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DevilOutput {
    pub strongest_objections: Vec<String>,
    pub failure_modes: Vec<String>,
    pub required_corrections: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SynthesisOutput {
    pub answer: String,
    pub consensus: Vec<String>,
    pub dissent: Vec<String>,
    pub confidence: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentOutput {
    Planner(PlannerOutput),
    Expert(ExpertOutput),
    Rebuttal(RebuttalOutput),
    Devil(DevilOutput),
    Synthesis(SynthesisOutput),
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemLifecycle {
    Started,
    Updated,
    Completed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexItem {
    pub id: String,
    pub kind: CodexItemKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CodexItemKind {
    AgentMessage {
        text: String,
    },
    Reasoning {
        text: String,
    },
    CommandExecution {
        command: String,
        output: String,
        exit_code: Option<i32>,
        status: CommandExecutionStatus,
    },
    FileChange {
        changes: Vec<FileUpdate>,
        status: FileChangeStatus,
    },
    McpToolCall {
        server: String,
        tool: String,
        arguments: String,
        result: Option<String>,
        error: Option<String>,
        status: McpToolCallStatus,
    },
    WebSearch {
        query: String,
    },
    TodoList {
        items: Vec<TodoEntry>,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandExecutionStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Add,
    Delete,
    Update,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileUpdate {
    pub path: String,
    pub kind: FileChangeKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeStatus {
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpToolCallStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodoEntry {
    pub text: String,
    pub completed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    TurnPermit,
    RetryBackoff,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SwarmEvent {
    RunStarted { owner: SwarmOwner },
    StageStarted { stage: Stage },
    RolePlanned { agent: AgentId, role: ExpertRole },
    AgentPrompted { agent: AgentId, stage: Stage, prompt: String },
    AgentWaiting { agent: AgentId, stage: Stage, reason: WaitReason },
    AgentStarted { agent: AgentId, stage: Stage, attempt: u8 },
    ThreadStarted { agent: AgentId, thread_id: String },
    Item { agent: AgentId, lifecycle: ItemLifecycle, item: CodexItem },
    AgentSucceeded { agent: AgentId, output: AgentOutput, usage: Usage },
    AgentFailed { agent: AgentId, attempt: u8, error: String },
    StageSucceeded { stage: Stage },
    CancellationAccepted {},
    RunSucceeded { result: SynthesisOutput },
    RunFailed { error: String },
    RunCancelled {},
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmEventRecord {
    pub sequence: u64,
    pub at_ms: u64,
    pub event: SwarmEvent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamItem {
    pub stage: Stage,
    pub lifecycle: ItemLifecycle,
    pub item: CodexItem,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmNode {
    pub agent: AgentId,
    pub role: Option<ExpertRole>,
    pub stage: Stage,
    pub status: NodeStatus,
    pub attempt: u8,
    pub threads: Vec<AgentThread>,
    pub timings: Vec<AgentStageTiming>,
    pub prompts: Vec<StagePrompt>,
    pub items: Vec<StreamItem>,
    pub outputs: Vec<AgentOutput>,
    pub usage: Usage,
    pub error: Option<String>,
    pub wait_reason: Option<WaitReason>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStageTiming {
    pub stage: Stage,
    pub started_at_ms: u64,
    pub last_event_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagePrompt {
    pub stage: Stage,
    pub prompt: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentThread {
    pub stage: Stage,
    pub attempt: u8,
    pub thread_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmProjection {
    pub spec: SwarmSpec,
    pub status: RunStatus,
    #[serde(skip_serializing, skip_deserializing, default)]
    pub owner: Option<SwarmOwner>,
    pub active_stage: Option<Stage>,
    pub completed_stages: Vec<Stage>,
    pub nodes: Vec<SwarmNode>,
    pub result: Option<SynthesisOutput>,
    pub failure: Option<String>,
    pub last_sequence: u64,
    pub last_event_at_ms: Option<u64>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ProjectionError {
    #[error(transparent)]
    Spec(#[from] SpecError),
    #[error("expected event sequence {expected}, received {actual}")]
    Sequence { expected: u64, actual: u64 },
    #[error("event at sequence {sequence} is invalid: {reason}")]
    Transition { sequence: u64, reason: String },
}

impl SwarmProjection {
    pub fn new(spec: SwarmSpec) -> Result<Self, ProjectionError> {
        spec.validate()?;
        Ok(Self {
            spec,
            status: RunStatus::Queued,
            owner: None,
            active_stage: None,
            completed_stages: Vec::new(),
            nodes: Vec::new(),
            result: None,
            failure: None,
            last_sequence: 0,
            last_event_at_ms: None,
        })
    }

    pub fn replay(
        spec: SwarmSpec,
        events: impl IntoIterator<Item = SwarmEventRecord>,
    ) -> Result<Self, ProjectionError> {
        let mut projection = Self::new(spec)?;
        for event in events {
            projection.apply(event)?;
        }
        Ok(projection)
    }

    pub fn apply(&mut self, record: SwarmEventRecord) -> Result<(), ProjectionError> {
        let expected = self.last_sequence + 1;
        if record.sequence != expected {
            return Err(ProjectionError::Sequence { expected, actual: record.sequence });
        }
        if self.status.is_terminal() {
            return self.invalid(record.sequence, "event follows terminal run event");
        }

        let sequence = record.sequence;
        let at_ms = record.at_ms;
        match record.event {
            SwarmEvent::RunStarted { owner } => {
                if self.status != RunStatus::Queued || self.owner.is_some() {
                    return self.invalid(sequence, "run_started must be the first event");
                }
                self.owner = Some(owner);
                self.status = RunStatus::Running;
            }
            SwarmEvent::StageStarted { stage } => {
                self.require_running(sequence)?;
                if self.active_stage.is_some() {
                    return self.invalid(sequence, "another stage is active");
                }
                let expected_ordinal = self.completed_stages.len() as u8;
                let ordinal = if self.spec.debate == DebatePolicy::Disabled
                    && stage == Stage::Devil
                    && expected_ordinal == Stage::Debate.ordinal()
                {
                    expected_ordinal + 1
                } else {
                    expected_ordinal
                };
                if self.spec.debate == DebatePolicy::Disabled && stage == Stage::Debate {
                    return self
                        .invalid(sequence, "debate stage is disabled by the immutable spec");
                }
                if stage.ordinal() != ordinal {
                    return self.invalid(sequence, "stage order is not deterministic");
                }
                self.active_stage = Some(stage);
            }
            SwarmEvent::RolePlanned { agent, role } => {
                self.require_stage(sequence, Stage::Planning)?;
                if self.nodes.iter().any(|node| node.agent == agent) {
                    return self.invalid(sequence, "expert agent id was planned twice");
                }
                if self
                    .nodes
                    .iter()
                    .filter_map(|node| node.role.as_ref())
                    .any(|existing| existing.title.eq_ignore_ascii_case(role.title.trim()))
                {
                    return self.invalid(sequence, "expert role title was planned twice");
                }
                if role.title.trim().is_empty()
                    || role.mandate.trim().is_empty()
                    || role.perspective.trim().is_empty()
                {
                    return self.invalid(sequence, "expert role fields must not be empty");
                }
                self.nodes.push(SwarmNode {
                    agent,
                    role: Some(role),
                    stage: Stage::Experts,
                    status: NodeStatus::Queued,
                    attempt: 0,
                    threads: Vec::new(),
                    timings: Vec::new(),
                    prompts: Vec::new(),
                    items: Vec::new(),
                    outputs: Vec::new(),
                    usage: Usage::default(),
                    error: None,
                    wait_reason: None,
                });
            }
            SwarmEvent::AgentPrompted { agent, stage, prompt } => {
                self.require_stage(sequence, stage)?;
                if prompt.trim().is_empty() {
                    return self.invalid(sequence, "agent prompt must not be empty");
                }
                let node = self.node_or_insert_system(agent, stage);
                let changing_stage = node.stage != stage;
                if !(matches!(
                    node.status,
                    NodeStatus::Queued | NodeStatus::Waiting | NodeStatus::Failed
                ) || changing_stage && node.status == NodeStatus::Succeeded)
                {
                    return self
                        .invalid(sequence, "agent cannot receive a prompt in its current state");
                }
                if node.prompts.iter().any(|existing| existing.stage == stage) {
                    return self.invalid(sequence, "agent stage prompt was recorded twice");
                }
                node.stage = stage;
                node.status = NodeStatus::Queued;
                node.attempt = 0;
                node.error = None;
                node.wait_reason = None;
                node.prompts.push(StagePrompt { stage, prompt });
                touch_timing(node, stage, at_ms);
            }
            SwarmEvent::AgentWaiting { agent, stage, reason } => {
                self.require_stage(sequence, stage)?;
                let node = self.node_or_insert_system(agent, stage);
                let changing_stage = node.stage != stage;
                if !(matches!(
                    node.status,
                    NodeStatus::Queued | NodeStatus::Waiting | NodeStatus::Failed
                ) || changing_stage && node.status == NodeStatus::Succeeded)
                {
                    return self.invalid(sequence, "only queued or retrying agents may wait");
                }
                if changing_stage {
                    node.stage = stage;
                    node.attempt = 0;
                }
                node.status = NodeStatus::Waiting;
                node.wait_reason = Some(reason);
                touch_timing(node, stage, at_ms);
            }
            SwarmEvent::AgentStarted { agent, stage, attempt } => {
                self.require_stage(sequence, stage)?;
                let retry_limit = self.spec.retry_limit;
                let node = self.node_or_insert_system(agent, stage);
                let changing_stage = node.stage != stage;
                if !node.prompts.iter().any(|prompt| prompt.stage == stage) {
                    return self
                        .invalid(sequence, "agent started before its stage prompt was persisted");
                }
                if !(matches!(
                    node.status,
                    NodeStatus::Queued | NodeStatus::Waiting | NodeStatus::Failed
                ) || changing_stage && node.status == NodeStatus::Succeeded)
                {
                    return self.invalid(sequence, "agent is already running or cancelled");
                }
                if attempt == 0 || attempt > retry_limit.saturating_add(1) {
                    return self.invalid(sequence, "agent attempt is outside retry policy");
                }
                if !changing_stage && node.attempt >= attempt {
                    return self.invalid(sequence, "agent attempt did not increase");
                }
                node.stage = stage;
                node.status = NodeStatus::Running;
                node.attempt = attempt;
                node.error = None;
                node.wait_reason = None;
                touch_timing(node, stage, at_ms);
            }
            SwarmEvent::ThreadStarted { agent, thread_id } => {
                if thread_id.trim().is_empty() {
                    return self.invalid(sequence, "thread id must not be empty");
                }
                let node = self.running_node(sequence, &agent)?;
                if node
                    .threads
                    .iter()
                    .any(|thread| thread.stage == node.stage && thread.attempt == node.attempt)
                {
                    return self.invalid(sequence, "agent attempt recorded more than one thread");
                }
                node.threads.push(AgentThread {
                    stage: node.stage,
                    attempt: node.attempt,
                    thread_id,
                });
                let stage = node.stage;
                touch_timing(node, stage, at_ms);
            }
            SwarmEvent::Item { agent, lifecycle, item } => {
                let node = self.running_node(sequence, &agent)?;
                node.items.push(StreamItem { stage: node.stage, lifecycle, item });
                let stage = node.stage;
                touch_timing(node, stage, at_ms);
            }
            SwarmEvent::AgentSucceeded { agent, output, usage } => {
                let active_stage = self.active_stage;
                let node = self.running_node(sequence, &agent)?;
                if output_stage(&output) != active_stage {
                    return self.invalid(sequence, "agent output type does not match active stage");
                }
                node.status = NodeStatus::Succeeded;
                node.outputs.push(output);
                node.usage.input_tokens =
                    node.usage.input_tokens.saturating_add(usage.input_tokens);
                node.usage.cached_input_tokens =
                    node.usage.cached_input_tokens.saturating_add(usage.cached_input_tokens);
                node.usage.output_tokens =
                    node.usage.output_tokens.saturating_add(usage.output_tokens);
                node.usage.reasoning_output_tokens = node
                    .usage
                    .reasoning_output_tokens
                    .saturating_add(usage.reasoning_output_tokens);
                let stage = node.stage;
                touch_timing(node, stage, at_ms);
            }
            SwarmEvent::AgentFailed { agent, attempt, error } => {
                if error.trim().is_empty() {
                    return self.invalid(sequence, "agent failure must include an error");
                }
                let node = self.running_node(sequence, &agent)?;
                if node.attempt != attempt {
                    return self
                        .invalid(sequence, "agent failure attempt does not match active attempt");
                }
                node.status = NodeStatus::Failed;
                node.error = Some(error);
                let stage = node.stage;
                touch_timing(node, stage, at_ms);
            }
            SwarmEvent::StageSucceeded { stage } => {
                self.require_stage(sequence, stage)?;
                self.validate_stage_completion(sequence, stage)?;
                self.active_stage = None;
                if self.spec.debate == DebatePolicy::Disabled && stage == Stage::Experts {
                    self.completed_stages.push(Stage::Debate);
                }
                self.completed_stages.push(stage);
                self.completed_stages.sort_by_key(|completed| completed.ordinal());
            }
            SwarmEvent::CancellationAccepted {} => {
                self.require_running(sequence)?;
                self.status = RunStatus::Cancelling;
                for node in &mut self.nodes {
                    if matches!(
                        node.status,
                        NodeStatus::Queued | NodeStatus::Waiting | NodeStatus::Running
                    ) {
                        node.status = NodeStatus::Cancelled;
                        let stage = node.stage;
                        touch_timing(node, stage, at_ms);
                    }
                }
            }
            SwarmEvent::RunSucceeded { result } => {
                self.require_running(sequence)?;
                if self.active_stage.is_some() || !self.completed_stages.contains(&Stage::Synthesis)
                {
                    return self.invalid(sequence, "run succeeded before synthesis completed");
                }
                self.status = RunStatus::Succeeded;
                self.result = Some(result);
            }
            SwarmEvent::RunFailed { error } => {
                self.require_running(sequence)?;
                if error.trim().is_empty() {
                    return self.invalid(sequence, "run failure must include an error");
                }
                self.status = RunStatus::Failed;
                self.failure = Some(error);
            }
            SwarmEvent::RunCancelled {} => {
                if self.status != RunStatus::Cancelling {
                    return self
                        .invalid(sequence, "run_cancelled requires cancellation acceptance");
                }
                self.status = RunStatus::Cancelled;
            }
        }
        self.last_sequence = sequence;
        self.last_event_at_ms = Some(record.at_ms);
        Ok(())
    }

    pub fn mark_orphaned(&mut self) {
        if matches!(self.status, RunStatus::Running | RunStatus::Cancelling) {
            self.status = RunStatus::Orphaned;
        }
    }

    pub fn mark_unavailable(&mut self) {
        if matches!(self.status, RunStatus::Running | RunStatus::Cancelling) {
            self.status = RunStatus::Unavailable;
        }
    }

    fn require_running(&self, sequence: u64) -> Result<(), ProjectionError> {
        if self.status != RunStatus::Running {
            return self.invalid(sequence, "run is not active");
        }
        Ok(())
    }

    fn require_stage(&self, sequence: u64, stage: Stage) -> Result<(), ProjectionError> {
        self.require_running(sequence)?;
        if self.active_stage != Some(stage) {
            return self.invalid(sequence, "event does not belong to the active stage");
        }
        Ok(())
    }

    fn node_or_insert_system(&mut self, agent: AgentId, stage: Stage) -> &mut SwarmNode {
        let index = match self.nodes.iter().position(|node| node.agent == agent) {
            Some(index) => index,
            None => {
                self.nodes.push(SwarmNode {
                    agent,
                    role: None,
                    stage,
                    status: NodeStatus::Queued,
                    attempt: 0,
                    threads: Vec::new(),
                    timings: Vec::new(),
                    prompts: Vec::new(),
                    items: Vec::new(),
                    outputs: Vec::new(),
                    usage: Usage::default(),
                    error: None,
                    wait_reason: None,
                });
                self.nodes.len() - 1
            }
        };
        &mut self.nodes[index]
    }

    fn running_node(
        &mut self,
        sequence: u64,
        agent: &AgentId,
    ) -> Result<&mut SwarmNode, ProjectionError> {
        let Some(index) = self.nodes.iter().position(|node| &node.agent == agent) else {
            return Err(ProjectionError::Transition {
                sequence,
                reason: "event names an unknown agent".to_owned(),
            });
        };
        if self.nodes[index].status != NodeStatus::Running {
            return Err(ProjectionError::Transition {
                sequence,
                reason: "agent is not running".to_owned(),
            });
        }
        Ok(&mut self.nodes[index])
    }

    fn validate_stage_completion(
        &self,
        sequence: u64,
        stage: Stage,
    ) -> Result<(), ProjectionError> {
        let stage_nodes: Vec<&SwarmNode> =
            self.nodes.iter().filter(|node| node.stage == stage).collect();
        if stage == Stage::Planning {
            let role_count = self.nodes.iter().filter(|node| node.role.is_some()).count();
            if !(3..=5).contains(&role_count) {
                return self.invalid(sequence, "planner must produce three to five expert roles");
            }
        }
        if stage_nodes.is_empty()
            || stage_nodes.iter().any(|node| node.status != NodeStatus::Succeeded)
        {
            return self.invalid(sequence, "stage has incomplete mandatory agents");
        }
        Ok(())
    }

    fn invalid<T>(&self, sequence: u64, reason: impl Into<String>) -> Result<T, ProjectionError> {
        Err(ProjectionError::Transition { sequence, reason: reason.into() })
    }
}

fn touch_timing(node: &mut SwarmNode, stage: Stage, at_ms: u64) {
    if let Some(timing) = node.timings.iter_mut().find(|timing| timing.stage == stage) {
        timing.last_event_at_ms = at_ms;
    } else {
        node.timings.push(AgentStageTiming {
            stage,
            started_at_ms: at_ms,
            last_event_at_ms: at_ms,
        });
    }
}

fn output_stage(output: &AgentOutput) -> Option<Stage> {
    Some(match output {
        AgentOutput::Planner(_) => Stage::Planning,
        AgentOutput::Expert(_) => Stage::Experts,
        AgentOutput::Rebuttal(_) => Stage::Debate,
        AgentOutput::Devil(_) => Stage::Devil,
        AgentOutput::Synthesis(_) => Stage::Synthesis,
    })
}

pub fn structured_output_schemas(
) -> Result<Vec<(&'static str, serde_json::Value)>, serde_json::Error> {
    Ok(vec![
        ("planner", serde_json::to_value(schemars::schema_for!(PlannerOutput))?),
        ("expert", serde_json::to_value(schemars::schema_for!(ExpertOutput))?),
        ("rebuttal", serde_json::to_value(schemars::schema_for!(RebuttalOutput))?),
        ("devil", serde_json::to_value(schemars::schema_for!(DevilOutput))?),
        ("synthesis", serde_json::to_value(schemars::schema_for!(SynthesisOutput))?),
    ])
}

pub fn validate_planner_output(output: &PlannerOutput) -> Result<(), String> {
    if !(3..=5).contains(&output.roles.len()) {
        return Err("planner must return three to five roles".to_owned());
    }
    let mut titles = HashSet::new();
    for role in &output.roles {
        if role.title.trim().is_empty()
            || role.mandate.trim().is_empty()
            || role.perspective.trim().is_empty()
        {
            return Err("planner role fields must not be empty".to_owned());
        }
        if !titles.insert(role.title.trim().to_ascii_lowercase()) {
            return Err("planner role titles must be unique".to_owned());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(debate: DebatePolicy) -> SwarmSpec {
        SwarmSpec {
            schema_version: SWARM_SCHEMA_VERSION,
            id: SwarmId::new("swarm-1").unwrap(),
            prompt: "Choose the strongest architecture".to_owned(),
            working_directory: std::env::current_dir().unwrap(),
            model: None,
            reasoning: ReasoningEffort::High,
            debate,
            created_at_ms: 1,
            retry_limit: 2,
        }
    }

    fn owner() -> SwarmOwner {
        SwarmOwner::fixture()
    }

    fn role(index: usize) -> ExpertRole {
        ExpertRole {
            title: format!("Role {index}"),
            mandate: format!("Test concern {index}"),
            perspective: format!("Perspective {index}"),
        }
    }

    fn apply(projection: &mut SwarmProjection, event: SwarmEvent) {
        projection
            .apply(SwarmEventRecord {
                sequence: projection.last_sequence + 1,
                at_ms: projection.last_sequence + 10,
                event,
            })
            .unwrap();
    }

    #[test]
    fn strict_projection_accepts_complete_no_debate_lifecycle() {
        let mut projection = SwarmProjection::new(spec(DebatePolicy::Disabled)).unwrap();
        let planner = AgentId::new("planner").unwrap();
        apply(&mut projection, SwarmEvent::RunStarted { owner: owner() });
        apply(&mut projection, SwarmEvent::StageStarted { stage: Stage::Planning });
        apply(
            &mut projection,
            SwarmEvent::AgentPrompted {
                agent: planner.clone(),
                stage: Stage::Planning,
                prompt: "planner prompt".to_owned(),
            },
        );
        apply(
            &mut projection,
            SwarmEvent::AgentStarted { agent: planner.clone(), stage: Stage::Planning, attempt: 1 },
        );
        let roles: Vec<ExpertRole> = (1..=3).map(role).collect();
        apply(
            &mut projection,
            SwarmEvent::AgentSucceeded {
                agent: planner,
                output: AgentOutput::Planner(PlannerOutput { roles: roles.clone() }),
                usage: Usage::default(),
            },
        );
        for (index, role) in roles.into_iter().enumerate() {
            apply(
                &mut projection,
                SwarmEvent::RolePlanned {
                    agent: AgentId::new(format!("expert-{}", index + 1)).unwrap(),
                    role,
                },
            );
        }
        apply(&mut projection, SwarmEvent::StageSucceeded { stage: Stage::Planning });
        apply(&mut projection, SwarmEvent::StageStarted { stage: Stage::Experts });
        for index in 1..=3 {
            let agent = AgentId::new(format!("expert-{index}")).unwrap();
            apply(
                &mut projection,
                SwarmEvent::AgentPrompted {
                    agent: agent.clone(),
                    stage: Stage::Experts,
                    prompt: format!("expert prompt {index}"),
                },
            );
            apply(
                &mut projection,
                SwarmEvent::AgentStarted {
                    agent: agent.clone(),
                    stage: Stage::Experts,
                    attempt: 1,
                },
            );
            apply(
                &mut projection,
                SwarmEvent::AgentSucceeded {
                    agent,
                    output: AgentOutput::Expert(ExpertOutput {
                        analysis: "analysis".to_owned(),
                        findings: vec!["finding".to_owned()],
                        recommendation: "recommendation".to_owned(),
                    }),
                    usage: Usage::default(),
                },
            );
        }
        apply(&mut projection, SwarmEvent::StageSucceeded { stage: Stage::Experts });
        apply(&mut projection, SwarmEvent::StageStarted { stage: Stage::Devil });
        let devil = AgentId::new("devil").unwrap();
        apply(
            &mut projection,
            SwarmEvent::AgentPrompted {
                agent: devil.clone(),
                stage: Stage::Devil,
                prompt: "devil prompt".to_owned(),
            },
        );
        apply(
            &mut projection,
            SwarmEvent::AgentStarted { agent: devil.clone(), stage: Stage::Devil, attempt: 1 },
        );
        apply(
            &mut projection,
            SwarmEvent::AgentSucceeded {
                agent: devil,
                output: AgentOutput::Devil(DevilOutput {
                    strongest_objections: vec!["objection".to_owned()],
                    failure_modes: vec!["failure".to_owned()],
                    required_corrections: vec!["correction".to_owned()],
                }),
                usage: Usage::default(),
            },
        );
        apply(&mut projection, SwarmEvent::StageSucceeded { stage: Stage::Devil });
        apply(&mut projection, SwarmEvent::StageStarted { stage: Stage::Synthesis });
        let synthesis_agent = AgentId::new("synthesis").unwrap();
        let result = SynthesisOutput {
            answer: "answer".to_owned(),
            consensus: vec!["consensus".to_owned()],
            dissent: vec![],
            confidence: "high".to_owned(),
        };
        apply(
            &mut projection,
            SwarmEvent::AgentPrompted {
                agent: synthesis_agent.clone(),
                stage: Stage::Synthesis,
                prompt: "synthesis prompt".to_owned(),
            },
        );
        apply(
            &mut projection,
            SwarmEvent::AgentStarted {
                agent: synthesis_agent.clone(),
                stage: Stage::Synthesis,
                attempt: 1,
            },
        );
        apply(
            &mut projection,
            SwarmEvent::AgentSucceeded {
                agent: synthesis_agent,
                output: AgentOutput::Synthesis(result.clone()),
                usage: Usage::default(),
            },
        );
        apply(&mut projection, SwarmEvent::StageSucceeded { stage: Stage::Synthesis });
        apply(&mut projection, SwarmEvent::RunSucceeded { result: result.clone() });

        assert_eq!(projection.status, RunStatus::Succeeded);
        assert_eq!(projection.result, Some(result));
        assert_eq!(projection.last_sequence, 31);
    }

    #[test]
    fn projection_rejects_sequence_gaps_and_events_after_terminal() {
        let mut projection = SwarmProjection::new(spec(DebatePolicy::Enabled)).unwrap();
        let gap = projection
            .apply(SwarmEventRecord {
                sequence: 2,
                at_ms: 1,
                event: SwarmEvent::RunStarted { owner: owner() },
            })
            .unwrap_err();
        assert_eq!(gap, ProjectionError::Sequence { expected: 1, actual: 2 });

        apply(&mut projection, SwarmEvent::RunStarted { owner: owner() });
        apply(&mut projection, SwarmEvent::RunFailed { error: "failed".to_owned() });
        let terminal = projection
            .apply(SwarmEventRecord {
                sequence: 3,
                at_ms: 3,
                event: SwarmEvent::RunFailed { error: "again".to_owned() },
            })
            .unwrap_err();
        assert!(matches!(terminal, ProjectionError::Transition { sequence: 3, .. }));
    }

    #[test]
    fn generated_schemas_are_closed_and_require_every_property() {
        fn inspect(value: &serde_json::Value) {
            if value.get("type").and_then(serde_json::Value::as_str) == Some("object") {
                assert_eq!(
                    value.get("additionalProperties"),
                    Some(&serde_json::Value::Bool(false))
                );
                let properties =
                    value.get("properties").and_then(serde_json::Value::as_object).unwrap();
                let required = value.get("required").and_then(serde_json::Value::as_array).unwrap();
                assert_eq!(properties.len(), required.len());
            }
            match value {
                serde_json::Value::Array(values) => values.iter().for_each(inspect),
                serde_json::Value::Object(values) => values.values().for_each(inspect),
                _ => {}
            }
        }

        let schemas = structured_output_schemas().unwrap();
        assert_eq!(schemas.len(), 5);
        for (_, schema) in schemas {
            assert_eq!(schema.get("type").and_then(serde_json::Value::as_str), Some("object"));
            inspect(&schema);
        }
    }
}
