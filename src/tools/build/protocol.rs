use std::{
    collections::BTreeMap,
    fs::{File, Metadata, OpenOptions},
    io::Read,
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, Context as _, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const BUILD_PROTOCOL_VERSION: u32 = 1;
pub(super) const MAX_EVENT_STREAM_BYTES: u64 = 16 * 1024 * 1024;
const MAX_EVENT_RECORDS: u32 = 100_000;
const MAX_EVENT_RECORD_BYTES: usize = 64 * 1024;
pub(super) const MAX_FINAL_RESULT_BYTES: u64 = 64 * 1024;
const JS_SAFE_INTEGER_MAX: u64 = 9_007_199_254_740_991;
const CANONICAL_UUID_PATTERN: &str =
    r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$";
const WORKFLOW_ID_PATTERN: &str = super::manifest::WORKFLOW_ID_PATTERN;
const WORKFLOW_ID_MAX_LENGTH: usize = super::manifest::WORKFLOW_ID_MAX_LENGTH;
const NON_WHITESPACE_PATTERN: &str = r"\S";

/// The only input Kit gives a repository-owned build provider.
///
/// The provider reads this document from `KIT_BUILD_REQUEST`; it does not receive Kit's process
/// transcript locations or any ambient build policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildRequest {
    #[schemars(extend("const" = BUILD_PROTOCOL_VERSION))]
    pub protocol_version: u32,
    #[schemars(length(equal = 36), pattern(CANONICAL_UUID_PATTERN))]
    pub run_id: String,
    #[schemars(length(min = 1, max = WORKFLOW_ID_MAX_LENGTH), pattern(WORKFLOW_ID_PATTERN))]
    pub workflow_id: String,
    #[schemars(length(min = 1), pattern(NON_WHITESPACE_PATTERN))]
    pub repository_root: String,
    #[schemars(length(min = 1), pattern(NON_WHITESPACE_PATTERN))]
    pub events_path: String,
    #[schemars(length(min = 1), pattern(NON_WHITESPACE_PATTERN))]
    pub result_path: String,
}

impl BuildRequest {
    pub fn validate(&self) -> Result<()> {
        if self.protocol_version != BUILD_PROTOCOL_VERSION {
            bail!(
                "build request protocol version {} is unsupported; expected {BUILD_PROTOCOL_VERSION}",
                self.protocol_version
            );
        }
        validate_canonical_run_id(&self.run_id)?;
        super::manifest::validate_workflow_id(&self.workflow_id)
            .context("validate build request workflow_id")?;
        for (name, value) in [
            ("build request repository_root", &self.repository_root),
            ("build request events_path", &self.events_path),
            ("build request result_path", &self.result_path),
        ] {
            validate_nonempty(name, value)?;
            if !Path::new(value).is_absolute() {
                bail!("{name} must be absolute");
            }
        }
        if self.events_path == self.result_path {
            bail!("build event and result artifacts must be distinct");
        }
        if Path::new(&self.events_path).parent() != Path::new(&self.result_path).parent() {
            bail!("build event and result artifacts must share one owner directory");
        }
        Ok(())
    }
}

/// One complete JSONL record emitted by a repository-owned build provider.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildEvent {
    #[schemars(extend("const" = BUILD_PROTOCOL_VERSION))]
    pub protocol_version: u32,
    #[schemars(length(equal = 36), pattern(CANONICAL_UUID_PATTERN))]
    pub run_id: String,
    #[schemars(length(min = 1, max = WORKFLOW_ID_MAX_LENGTH), pattern(WORKFLOW_ID_PATTERN))]
    pub workflow_id: String,
    #[schemars(range(min = 1, max = MAX_EVENT_RECORDS))]
    pub sequence: u32,
    #[schemars(range(max = JS_SAFE_INTEGER_MAX))]
    pub timestamp_ms: u64,
    pub event: BuildEventKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BuildEventKind {
    RunStarted {},
    StageStarted {
        #[schemars(length(min = 1), pattern(NON_WHITESPACE_PATTERN))]
        stage_id: String,
        #[schemars(length(min = 1), pattern(NON_WHITESPACE_PATTERN))]
        parent_stage_id: Option<String>,
    },
    StageProgress {
        #[schemars(length(min = 1), pattern(NON_WHITESPACE_PATTERN))]
        stage_id: String,
        #[schemars(length(min = 1), pattern(NON_WHITESPACE_PATTERN))]
        message: String,
    },
    StageFinished {
        #[schemars(length(min = 1), pattern(NON_WHITESPACE_PATTERN))]
        stage_id: String,
        outcome: StageOutcome,
    },
    ArtifactReported {
        #[schemars(length(min = 1), pattern(NON_WHITESPACE_PATTERN))]
        path: String,
        #[schemars(length(min = 1), pattern(NON_WHITESPACE_PATTERN))]
        label: String,
    },
    EvidenceReported {
        #[schemars(length(min = 1), pattern(NON_WHITESPACE_PATTERN))]
        path: String,
        #[schemars(length(min = 1), pattern(NON_WHITESPACE_PATTERN))]
        label: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StageOutcome {
    Succeeded,
    Failed,
}

pub(super) struct ValidatedBuildEvents {
    events: Vec<BuildEvent>,
    event_count: u32,
    has_failed_stage: bool,
}

impl ValidatedBuildEvents {
    pub(super) fn event_count(&self) -> u32 {
        self.event_count
    }

    pub(super) fn into_events(self) -> Vec<BuildEvent> {
        self.events
    }

    fn validate_final_outcome(&self, outcome: BuildOutcome) -> Result<()> {
        if outcome == BuildOutcome::Succeeded && self.has_failed_stage {
            bail!("build result reported success after at least one stage failed");
        }
        Ok(())
    }
}

struct StageLifecycle {
    parent_stage_id: Option<String>,
    active: bool,
    active_children: u32,
}

#[derive(Default)]
struct EventLifecycleValidator {
    stages: BTreeMap<String, StageLifecycle>,
    previous_timestamp: Option<(u32, u64)>,
    has_failed_stage: bool,
}

impl EventLifecycleValidator {
    fn observe(&mut self, event: &BuildEvent) -> Result<()> {
        if let Some((previous_sequence, previous_timestamp_ms)) = self.previous_timestamp {
            if event.timestamp_ms < previous_timestamp_ms {
                bail!(
                    "build event {} timestamp_ms {} precedes event {previous_sequence} timestamp_ms {previous_timestamp_ms}",
                    event.sequence,
                    event.timestamp_ms
                );
            }
        }
        self.previous_timestamp = Some((event.sequence, event.timestamp_ms));

        match &event.event {
            BuildEventKind::RunStarted {} => {}
            BuildEventKind::StageStarted { stage_id, parent_stage_id } => {
                if self.stages.contains_key(stage_id) {
                    bail!("build event {} starts duplicate stage_id '{stage_id}'", event.sequence);
                }
                if let Some(parent_stage_id) = parent_stage_id {
                    let parent = self.stages.get_mut(parent_stage_id).ok_or_else(|| {
                        anyhow::anyhow!(
                            "build event {} starts stage '{stage_id}' before parent stage '{parent_stage_id}'",
                            event.sequence
                        )
                    })?;
                    if !parent.active {
                        bail!(
                            "build event {} starts stage '{stage_id}' under inactive parent stage '{parent_stage_id}'",
                            event.sequence
                        );
                    }
                    parent.active_children = parent
                        .active_children
                        .checked_add(1)
                        .context("count active build-stage children")?;
                }
                self.stages.insert(
                    stage_id.clone(),
                    StageLifecycle {
                        parent_stage_id: parent_stage_id.clone(),
                        active: true,
                        active_children: 0,
                    },
                );
            }
            BuildEventKind::StageProgress { stage_id, .. } => {
                let stage = self.stages.get(stage_id).ok_or_else(|| {
                    anyhow::anyhow!(
                        "build event {} reports progress for stage '{stage_id}' before it starts",
                        event.sequence
                    )
                })?;
                if !stage.active {
                    bail!(
                        "build event {} reports progress for inactive stage '{stage_id}'",
                        event.sequence
                    );
                }
            }
            BuildEventKind::StageFinished { stage_id, outcome } => {
                let parent_stage_id = {
                    let stage = self.stages.get_mut(stage_id).ok_or_else(|| {
                        anyhow::anyhow!(
                            "build event {} finishes stage '{stage_id}' before it starts",
                            event.sequence
                        )
                    })?;
                    if !stage.active {
                        bail!(
                            "build event {} finishes inactive stage '{stage_id}'",
                            event.sequence
                        );
                    }
                    if stage.active_children != 0 {
                        bail!(
                            "build event {} finishes stage '{stage_id}' with {} active child stage(s)",
                            event.sequence,
                            stage.active_children
                        );
                    }
                    stage.active = false;
                    stage.parent_stage_id.clone()
                };
                if let Some(parent_stage_id) = parent_stage_id {
                    let parent = self.stages.get_mut(&parent_stage_id).with_context(|| {
                        format!(
                            "stage '{stage_id}' lost parent lifecycle state '{parent_stage_id}'"
                        )
                    })?;
                    parent.active_children = parent
                        .active_children
                        .checked_sub(1)
                        .context("release active build-stage child")?;
                }
                self.has_failed_stage |= *outcome == StageOutcome::Failed;
            }
            BuildEventKind::ArtifactReported { .. } | BuildEventKind::EvidenceReported { .. } => {}
        }
        Ok(())
    }

    fn finish(self, events: Vec<BuildEvent>) -> Result<ValidatedBuildEvents> {
        let active_stages = self
            .stages
            .iter()
            .filter_map(|(stage_id, stage)| stage.active.then_some(stage_id.as_str()))
            .collect::<Vec<_>>();
        if !active_stages.is_empty() {
            bail!("build event stream ended with active stage(s): {}", active_stages.join(", "));
        }
        let event_count = u32::try_from(events.len()).context("count validated build events")?;
        Ok(ValidatedBuildEvents { events, event_count, has_failed_stage: self.has_failed_stage })
    }
}

impl BuildEvent {
    pub fn validate_for(
        &self,
        run_id: &str,
        workflow_id: &str,
        expected_sequence: u32,
    ) -> Result<()> {
        if self.protocol_version != BUILD_PROTOCOL_VERSION {
            bail!(
                "build event {} uses unsupported protocol version {}; expected {BUILD_PROTOCOL_VERSION}",
                self.sequence,
                self.protocol_version
            );
        }
        if self.run_id != run_id {
            bail!("build event {} belongs to a different run", self.sequence);
        }
        super::manifest::validate_workflow_id(&self.workflow_id)
            .with_context(|| format!("validate build event {} workflow_id", self.sequence))?;
        if self.workflow_id != workflow_id {
            bail!("build event {} belongs to a different workflow", self.sequence);
        }
        if self.sequence != expected_sequence {
            bail!(
                "build event sequence is not contiguous: expected {expected_sequence}, found {}",
                self.sequence
            );
        }
        if self.timestamp_ms > JS_SAFE_INTEGER_MAX {
            bail!(
                "build event {} timestamp_ms {} exceeds the JavaScript safe-integer maximum {JS_SAFE_INTEGER_MAX}",
                self.sequence,
                self.timestamp_ms
            );
        }
        validate_event_payload(&self.event)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildFinalResult {
    #[schemars(extend("const" = BUILD_PROTOCOL_VERSION))]
    pub protocol_version: u32,
    #[schemars(length(equal = 36), pattern(CANONICAL_UUID_PATTERN))]
    pub run_id: String,
    #[schemars(length(min = 1, max = WORKFLOW_ID_MAX_LENGTH), pattern(WORKFLOW_ID_PATTERN))]
    pub workflow_id: String,
    pub outcome: BuildOutcome,
    #[schemars(range(min = 1, max = MAX_EVENT_RECORDS))]
    pub last_event_sequence: u32,
    #[schemars(range(min = 1, max = MAX_EVENT_RECORDS))]
    pub event_count: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BuildOutcome {
    Succeeded,
    Failed,
}

impl BuildFinalResult {
    pub fn validate_for(&self, run_id: &str, workflow_id: &str, event_count: u32) -> Result<()> {
        if self.protocol_version != BUILD_PROTOCOL_VERSION {
            bail!(
                "build result uses unsupported protocol version {}; expected {BUILD_PROTOCOL_VERSION}",
                self.protocol_version
            );
        }
        if self.run_id != run_id {
            bail!("build result belongs to a different run");
        }
        super::manifest::validate_workflow_id(&self.workflow_id)
            .context("validate build result workflow_id")?;
        if self.workflow_id != workflow_id {
            bail!("build result belongs to a different workflow");
        }
        if self.event_count != event_count {
            bail!(
                "build result event_count {} does not match the {} complete event records",
                self.event_count,
                event_count
            );
        }
        let expected_last = event_count;
        if self.last_event_sequence != expected_last {
            bail!(
                "build result last_event_sequence {} does not match expected {expected_last}",
                self.last_event_sequence
            );
        }
        Ok(())
    }
}

// This aggregate exists only for `schemars` to traverse the complete wire contract.
#[allow(dead_code)]
#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "KitBuildProviderProtocol")]
struct ProviderSchema {
    #[schemars(extend("const" = BUILD_PROTOCOL_VERSION))]
    protocol_version: u32,
    manifest: super::manifest::BuildManifestDocument,
    request: BuildRequest,
    event: BuildEvent,
    final_result: BuildFinalResult,
}

pub fn provider_schema() -> Result<serde_json::Value> {
    serde_json::to_value(schemars::schema_for!(ProviderSchema))
        .context("serialize build provider schema")
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EventFileIdentity {
    device: u64,
    inode: u64,
    links: u64,
}

#[cfg(unix)]
impl EventFileIdentity {
    fn from_metadata(metadata: &Metadata) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;

        if metadata.nlink() != 1 {
            bail!("build event stream must retain exactly one hard link");
        }
        Ok(Self { device: metadata.dev(), inode: metadata.ino(), links: metadata.nlink() })
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EventFileIdentity {
    creation_time: u64,
}

#[cfg(windows)]
impl EventFileIdentity {
    fn from_metadata(metadata: &Metadata) -> Result<Self> {
        use std::os::windows::fs::MetadataExt;

        Ok(Self { creation_time: metadata.creation_time() })
    }
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EventFileIdentity;

#[cfg(not(any(unix, windows)))]
impl EventFileIdentity {
    fn from_metadata(metadata: &Metadata) -> Result<Self> {
        if !metadata.is_file() {
            bail!("build event stream must be a regular file");
        }
        Ok(Self)
    }
}

fn open_event_stream_file(path: &Path) -> std::io::Result<File> {
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
    options.open(path)
}

pub(super) struct BuildEventStreamReader {
    path: PathBuf,
    run_id: String,
    workflow_id: String,
    file: Option<File>,
    identity: Option<EventFileIdentity>,
    total_bytes: u64,
    verified_bytes: Vec<u8>,
    pending: Vec<u8>,
    events: Vec<BuildEvent>,
    lifecycle: EventLifecycleValidator,
}

impl BuildEventStreamReader {
    pub(super) fn new(path: &Path, run_id: &str, workflow_id: &str) -> Self {
        Self {
            path: path.to_path_buf(),
            run_id: run_id.to_owned(),
            workflow_id: workflow_id.to_owned(),
            file: None,
            identity: None,
            total_bytes: 0,
            verified_bytes: Vec::new(),
            pending: Vec::new(),
            events: Vec::new(),
            lifecycle: EventLifecycleValidator::default(),
        }
    }

    pub(super) fn read_available(&mut self) -> Result<&[BuildEvent]> {
        let first_new_event = self.read_available_records(false)?;
        Ok(&self.events[first_new_event..])
    }

    pub(super) fn finish(mut self) -> Result<ValidatedBuildEvents> {
        self.read_available_records(true)?;
        self.verify_final_contents()?;
        if !self.pending.is_empty() {
            bail!(
                "build event stream ended with a partial JSONL record at line {}",
                self.events.len() + 1
            );
        }
        if self.events.is_empty() {
            bail!("build provider wrote no events");
        }
        self.lifecycle.finish(self.events)
    }

    fn read_available_records(&mut self, file_required: bool) -> Result<usize> {
        let first_new_event = self.events.len();
        if !self.ensure_file_open(file_required)? {
            return Ok(first_new_event);
        }
        self.revalidate_path_identity()?;

        {
            let file = self.file.as_mut().context("build event stream file was not opened")?;
            let metadata = file
                .metadata()
                .with_context(|| format!("inspect build event stream {}", self.path.display()))?;
            if metadata.len() < self.total_bytes {
                bail!("build event stream was truncated after Kit began reading it");
            }
            let read_limit = MAX_EVENT_STREAM_BYTES
                .saturating_sub(self.total_bytes)
                .checked_add(1)
                .context("build event stream read limit overflow")?;
            let pending_before = self.pending.len();
            file.take(read_limit)
                .read_to_end(&mut self.pending)
                .with_context(|| format!("read build event stream {}", self.path.display()))?;
            let bytes_read = self
                .pending
                .len()
                .checked_sub(pending_before)
                .context("build event buffer length regressed while reading")?;
            let bytes_read =
                u64::try_from(bytes_read).context("count newly available build event bytes")?;
            self.verified_bytes.extend_from_slice(&self.pending[pending_before..]);
            self.total_bytes = self
                .total_bytes
                .checked_add(bytes_read)
                .context("count build event stream bytes")?;
            if self.total_bytes > MAX_EVENT_STREAM_BYTES {
                bail!(
                    "build event stream exceeds the {MAX_EVENT_STREAM_BYTES}-byte protocol limit"
                );
            }
        }

        let mut record_start = 0;
        while let Some(relative_newline) =
            self.pending[record_start..].iter().position(|byte| *byte == b'\n')
        {
            let newline = record_start + relative_newline;
            let record_bytes = newline - record_start;
            let line_number = self.events.len() + 1;
            if record_bytes == 0 {
                bail!("build event stream contains an empty record at line {line_number}");
            }
            if record_bytes > MAX_EVENT_RECORD_BYTES {
                bail!(
                    "build event record at line {line_number} exceeds the {MAX_EVENT_RECORD_BYTES}-byte protocol limit"
                );
            }
            let line = self.pending[record_start..newline].to_vec();
            self.parse_record(&line, line_number)?;
            record_start = newline + 1;
        }
        if record_start != 0 {
            self.pending.drain(..record_start);
        }
        if self.pending.len() > MAX_EVENT_RECORD_BYTES {
            bail!(
                "partial build event record at line {} exceeds the {MAX_EVENT_RECORD_BYTES}-byte protocol limit",
                self.events.len() + 1
            );
        }
        Ok(first_new_event)
    }

    fn ensure_file_open(&mut self, required: bool) -> Result<bool> {
        if self.file.is_some() {
            return Ok(true);
        }
        let file = match open_event_stream_file(&self.path) {
            Ok(file) => file,
            Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(false);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("open build event stream {}", self.path.display()));
            }
        };
        require_regular_file(&file, "build event stream")?;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect build event stream {}", self.path.display()))?;
        self.identity = Some(EventFileIdentity::from_metadata(&metadata)?);
        self.file = Some(file);
        Ok(true)
    }

    fn revalidate_path_identity(&self) -> Result<File> {
        let file = self.file.as_ref().context("build event stream file was not opened")?;
        let expected =
            self.identity.as_ref().context("build event stream identity was not established")?;
        let opened_metadata = file.metadata().with_context(|| {
            format!("inspect opened build event stream {}", self.path.display())
        })?;
        let opened_identity = EventFileIdentity::from_metadata(&opened_metadata)?;
        if &opened_identity != expected {
            bail!("opened build event stream identity changed while Kit was reading it");
        }

        let path_file = open_event_stream_file(&self.path)
            .with_context(|| format!("reopen build event stream {}", self.path.display()))?;
        require_regular_file(&path_file, "build event stream")?;
        let path_metadata = path_file
            .metadata()
            .with_context(|| format!("inspect build event stream path {}", self.path.display()))?;
        let path_identity = EventFileIdentity::from_metadata(&path_metadata)?;
        if &path_identity != expected {
            bail!("build event stream path no longer names the file Kit began reading");
        }
        if opened_metadata.len() < self.total_bytes || path_metadata.len() < self.total_bytes {
            bail!("build event stream was truncated after Kit began reading it");
        }
        Ok(path_file)
    }

    fn verify_final_contents(&self) -> Result<()> {
        let file = self.revalidate_path_identity()?;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect final build event stream {}", self.path.display()))?;
        if metadata.len() != self.total_bytes {
            bail!(
                "build event stream length changed during finalization: read {}, found {}",
                self.total_bytes,
                metadata.len()
            );
        }
        let read_limit = MAX_EVENT_STREAM_BYTES
            .checked_add(1)
            .context("build event stream verification limit overflow")?;
        let mut final_bytes = Vec::new();
        file.take(read_limit)
            .read_to_end(&mut final_bytes)
            .with_context(|| format!("verify build event stream {}", self.path.display()))?;
        if final_bytes != self.verified_bytes {
            bail!("build event stream prefix changed after Kit consumed it");
        }
        self.revalidate_path_identity()?;
        Ok(())
    }

    fn parse_record(&mut self, line: &[u8], line_number: usize) -> Result<()> {
        let raw = std::str::from_utf8(line)
            .with_context(|| format!("build event at line {line_number} is not UTF-8 JSON"))?;
        let event = serde_json::from_str::<BuildEvent>(raw)
            .with_context(|| format!("parse build event at line {line_number}"))?;
        let expected_sequence =
            u32::try_from(self.events.len() + 1).context("count build event sequence")?;
        if expected_sequence > MAX_EVENT_RECORDS {
            bail!("build event stream exceeds the {MAX_EVENT_RECORDS}-record protocol limit");
        }
        event.validate_for(&self.run_id, &self.workflow_id, expected_sequence)?;
        if expected_sequence == 1 && !matches!(&event.event, BuildEventKind::RunStarted {}) {
            bail!("the first build event must be run_started");
        }
        if expected_sequence > 1 && matches!(&event.event, BuildEventKind::RunStarted {}) {
            bail!("run_started may appear only as the first build event");
        }
        self.lifecycle.observe(&event)?;
        self.events.push(event);
        Ok(())
    }
}

pub fn read_final_result(
    path: &Path,
    run_id: &str,
    workflow_id: &str,
    events: &ValidatedBuildEvents,
) -> Result<BuildFinalResult> {
    let raw = read_bounded_regular_file(path, MAX_FINAL_RESULT_BYTES, "build result")?;
    let result = serde_json::from_slice::<BuildFinalResult>(&raw)
        .with_context(|| format!("parse build result {}", path.display()))?;
    result.validate_for(run_id, workflow_id, events.event_count())?;
    events.validate_final_outcome(result.outcome)?;
    Ok(result)
}

fn read_bounded_regular_file(path: &Path, limit: u64, label: &str) -> Result<Vec<u8>> {
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

    let file = options.open(path).with_context(|| format!("open {label} {}", path.display()))?;
    require_regular_file(&file, label)?;

    let read_limit = limit.checked_add(1).context("build protocol byte limit overflow")?;
    let mut bytes = Vec::new();
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {label} {}", path.display()))?;
    if u64::try_from(bytes.len()).context("count build protocol bytes")? > limit {
        bail!("{label} exceeds the {limit}-byte protocol limit");
    }
    Ok(bytes)
}

fn require_regular_file(file: &File, label: &str) -> Result<()> {
    let metadata = file.metadata().with_context(|| format!("inspect opened {label}"))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("{label} must be a regular file");
    }
    Ok(())
}

fn validate_event_payload(event: &BuildEventKind) -> Result<()> {
    match event {
        BuildEventKind::RunStarted {} => Ok(()),
        BuildEventKind::StageStarted { stage_id, parent_stage_id } => {
            validate_opaque("stage_id", stage_id)?;
            if let Some(parent_stage_id) = parent_stage_id {
                validate_opaque("parent_stage_id", parent_stage_id)?;
            }
            Ok(())
        }
        BuildEventKind::StageProgress { stage_id, message } => {
            validate_opaque("stage_id", stage_id)?;
            validate_nonempty("stage progress message", message)
        }
        BuildEventKind::StageFinished { stage_id, .. } => validate_opaque("stage_id", stage_id),
        BuildEventKind::ArtifactReported { path, label }
        | BuildEventKind::EvidenceReported { path, label } => {
            validate_repository_relative_path(path)?;
            validate_nonempty("reported path label", label)
        }
    }
}

pub(super) fn validate_canonical_run_id(value: &str) -> Result<()> {
    let run_id = uuid::Uuid::parse_str(value).context("build request run_id must be a UUID")?;
    if run_id.hyphenated().to_string() != value {
        bail!("build request run_id must use canonical lowercase hyphenated UUID form");
    }
    if run_id.get_version_num() != 4 || run_id.get_variant() != uuid::Variant::RFC4122 {
        bail!("build request run_id must be an RFC-variant UUID v4");
    }
    Ok(())
}

fn validate_opaque(label: &str, value: &str) -> Result<()> {
    validate_nonempty(label, value)?;
    if value.contains('\0') {
        bail!("{label} must not contain a NUL byte");
    }
    Ok(())
}

fn validate_nonempty(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{label} must not be empty");
    }
    Ok(())
}

fn validate_repository_relative_path(value: &str) -> Result<()> {
    validate_nonempty("reported repository path", value)?;
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(component, Component::ParentDir | Component::RootDir | Component::Prefix(_))
        })
    {
        bail!("reported repository path must stay relative to the repository root");
    }
    Ok(())
}
