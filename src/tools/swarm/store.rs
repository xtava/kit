use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::framework::process::{DetachedControlError, DetachedProcessStatus, ProcessSupervisor};
use crate::framework::{AtomicFileError, AtomicFileWriter};

use super::model::{
    DebatePolicy, ProjectionError, ReasoningEffort, SwarmEvent, SwarmEventRecord, SwarmId,
    SwarmProjection, SwarmSpec, SWARM_SCHEMA_VERSION,
};

const RUNS_DIRECTORY: &str = "runs";
const COUNTER_FILE: &str = "next-run";
const SPEC_FILE: &str = "spec.json";
const JOURNAL_FILE: &str = "events.jsonl";
const WRITER_LOCK_FILE: &str = "writer.lock";
const CANCEL_REQUEST_FILE: &str = "cancel.request";
const RESULT_FILE: &str = "result.json";
const JOURNAL_CAPACITY: usize = 256;
const OWNER_RELEASE_CONFIRMATION: Duration = Duration::from_secs(30);
const OWNER_RELEASE_RETRY: Duration = Duration::from_millis(25);

#[derive(Clone, Debug)]
pub struct NewSwarmSpec {
    pub prompt: String,
    pub working_directory: PathBuf,
    pub model: Option<String>,
    pub reasoning: ReasoningEffort,
    pub debate: DebatePolicy,
    pub retry_limit: u8,
}

#[derive(Clone, Debug)]
pub struct SwarmStore {
    root: PathBuf,
}

#[derive(Clone, Debug)]
pub enum DiscoveredRun {
    Valid(SwarmSpec),
    Corrupt { id: SwarmId, error: String },
}

impl DiscoveredRun {
    pub fn id(&self) -> &SwarmId {
        match self {
            Self::Valid(spec) => &spec.id,
            Self::Corrupt { id, .. } => id,
        }
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("resolve Kit state directory")]
    StateDirectory,
    #[error(transparent)]
    Atomic(#[from] AtomicFileError),
    #[error("create swarm state directory {}: {source}", path.display())]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("read swarm state {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("write swarm state {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse swarm state {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid swarm spec {}: {source}", path.display())]
    Spec {
        path: PathBuf,
        #[source]
        source: super::model::SpecError,
    },
    #[error("parse swarm run counter {}: {source}", path.display())]
    Counter {
        path: PathBuf,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("swarm run counter overflow")]
    CounterOverflow,
    #[error("swarm run {0} does not exist")]
    MissingRun(SwarmId),
    #[error("swarm spec {} names run {actual}; expected {expected}", path.display())]
    SpecId { path: PathBuf, expected: SwarmId, actual: SwarmId },
    #[error("swarm run {0} already has a Journal writer")]
    WriterBusy(SwarmId),
    #[error("swarm Journal {} has an incomplete terminal line", path.display())]
    IncompleteTerminalLine { path: PathBuf },
    #[error("swarm Journal {} contains an empty line", path.display())]
    EmptyJournalLine { path: PathBuf },
    #[error("swarm Journal {} was truncated while being followed", path.display())]
    JournalTruncated { path: PathBuf },
    #[error("swarm Journal projection failed: {0}")]
    Projection(#[from] ProjectionError),
    #[error("swarm Journal writer stopped before acknowledging the event")]
    WriterStopped,
    #[error("swarm Journal writer task failed: {0}")]
    WriterTask(String),
    #[error("cannot derive a result for non-terminal swarm run {0}")]
    NonTerminalResult(SwarmId),
    #[error("cannot remove swarm run {0} because its Journal has started")]
    StartedRun(SwarmId),
    #[error("cannot delete swarm run {0} before terminal or orphaned state")]
    NonDeletableRun(SwarmId),
    #[error("cannot delete terminal swarm run {0} before its matching result is durable")]
    TerminalResultUnavailable(SwarmId),
    #[error("swarm run {id} contains an invalid detached-owner receipt")]
    OwnerReceipt { id: SwarmId },
    #[error("inspect detached swarm owner for {id}: {source}")]
    OwnerInspection {
        id: SwarmId,
        #[source]
        source: DetachedControlError,
    },
    #[error("release detached swarm owner for {id}: {source}")]
    OwnerRelease {
        id: SwarmId,
        #[source]
        source: DetachedControlError,
    },
}

impl SwarmStore {
    pub fn bootstrap() -> Result<Self, StoreError> {
        let project =
            directories::ProjectDirs::from("", "", "kit").ok_or(StoreError::StateDirectory)?;
        let base = project.state_dir().unwrap_or_else(|| project.data_local_dir());
        Self::at(base.join("swarm"))
    }

    pub fn at(root: PathBuf) -> Result<Self, StoreError> {
        create_private_directory(&root)?;
        create_private_directory(&root.join(RUNS_DIRECTORY))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn run_dir(&self, id: &SwarmId) -> PathBuf {
        self.root.join(RUNS_DIRECTORY).join(id.as_str())
    }

    pub fn create(&self, input: NewSwarmSpec) -> Result<SwarmSpec, StoreError> {
        let id = self.reserve_id()?;
        let spec = SwarmSpec {
            schema_version: SWARM_SCHEMA_VERSION,
            id: id.clone(),
            prompt: input.prompt,
            working_directory: input.working_directory,
            model: input.model,
            reasoning: input.reasoning,
            debate: input.debate,
            created_at_ms: now_ms(),
            retry_limit: input.retry_limit,
        };
        spec.validate().map_err(|source| StoreError::Spec {
            path: self.run_dir(&id).join(SPEC_FILE),
            source,
        })?;

        let run_dir = self.run_dir(&id);
        create_private_directory(&run_dir)?;
        let result = self.publish_spec(&spec);
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&run_dir);
        }
        result.map(|()| spec)
    }

    pub fn load_spec(&self, id: &SwarmId) -> Result<SwarmSpec, StoreError> {
        let path = self.run_dir(id).join(SPEC_FILE);
        let raw = std::fs::read(&path).map_err(|source| match source.kind() {
            std::io::ErrorKind::NotFound => StoreError::MissingRun(id.clone()),
            _ => StoreError::Read { path: path.clone(), source },
        })?;
        let spec: SwarmSpec = serde_json::from_slice(&raw)
            .map_err(|source| StoreError::Parse { path: path.clone(), source })?;
        spec.validate().map_err(|source| StoreError::Spec { path, source })?;
        if &spec.id != id {
            return Err(StoreError::SpecId {
                path: self.run_dir(id).join(SPEC_FILE),
                expected: id.clone(),
                actual: spec.id,
            });
        }
        Ok(spec)
    }

    pub fn discover(&self) -> Result<Vec<DiscoveredRun>, StoreError> {
        let path = self.root.join(RUNS_DIRECTORY);
        let entries = std::fs::read_dir(&path)
            .map_err(|source| StoreError::Read { path: path.clone(), source })?;
        let mut runs = Vec::new();
        for entry in entries.flatten() {
            let Ok(id) = SwarmId::new(entry.file_name().to_string_lossy().into_owned()) else {
                continue;
            };
            match self.load_spec(&id) {
                Ok(spec) => runs.push(DiscoveredRun::Valid(spec)),
                Err(StoreError::MissingRun(_)) => {}
                Err(error) => runs.push(DiscoveredRun::Corrupt { id, error: error.to_string() }),
            }
        }
        runs.sort_by_key(|run| run_number(run.id()).unwrap_or(u64::MAX));
        Ok(runs)
    }

    pub fn read_journal(&self, id: &SwarmId) -> Result<JournalRead, StoreError> {
        let spec = self.load_spec(id)?;
        let path = self.run_dir(id).join(JOURNAL_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(source) => return Err(StoreError::Read { path, source }),
        };
        decode_journal(spec, &path, &bytes)
    }

    pub fn tail(&self, id: &SwarmId) -> Result<JournalTail, StoreError> {
        let spec = self.load_spec(id)?;
        let mut tail = JournalTail {
            path: self.run_dir(id).join(JOURNAL_FILE),
            offset: 0,
            buffer: Vec::new(),
            projection: SwarmProjection::new(spec)?,
        };
        tail.refresh()?;
        Ok(tail)
    }

    pub async fn inspect(
        &self,
        processes: &ProcessSupervisor,
        id: &SwarmId,
    ) -> Result<SwarmProjection, StoreError> {
        let mut projection = self.read_journal(id)?.projection;
        if matches!(
            projection.status,
            super::model::RunStatus::Running | super::model::RunStatus::Cancelling
        ) {
            let owner = projection
                .owner
                .as_ref()
                .ok_or_else(|| StoreError::OwnerReceipt { id: id.clone() })?;
            let receipt =
                owner.receipt().map_err(|_| StoreError::OwnerReceipt { id: id.clone() })?;
            match processes.inspect_detached(&receipt).await {
                Ok(DetachedProcessStatus::Running | DetachedProcessStatus::Stopping) => {}
                Ok(DetachedProcessStatus::Completed(_) | DetachedProcessStatus::Failed(_))
                | Err(DetachedControlError::AuthorityLost) => projection.mark_orphaned(),
                Err(DetachedControlError::Unavailable(_)) => projection.mark_unavailable(),
                Err(source) => return Err(StoreError::OwnerInspection { id: id.clone(), source }),
            }
        } else if projection.status.is_terminal() && self.valid_result(id)?.is_some() {
            let owner = projection
                .owner
                .as_ref()
                .ok_or_else(|| StoreError::OwnerReceipt { id: id.clone() })?;
            let receipt =
                owner.receipt().map_err(|_| StoreError::OwnerReceipt { id: id.clone() })?;
            let deadline = tokio::time::Instant::now() + OWNER_RELEASE_CONFIRMATION;
            loop {
                match processes.forget_detached(&receipt).await {
                    Ok(()) => break,
                    Err(DetachedControlError::NotCompleted) => {
                        match processes.inspect_detached(&receipt).await {
                            Ok(
                                DetachedProcessStatus::Completed(_)
                                | DetachedProcessStatus::Failed(_),
                            ) => continue,
                            Ok(
                                DetachedProcessStatus::Running | DetachedProcessStatus::Stopping,
                            ) if tokio::time::Instant::now() < deadline => {
                                tokio::time::sleep(OWNER_RELEASE_RETRY).await;
                            }
                            Ok(
                                DetachedProcessStatus::Running | DetachedProcessStatus::Stopping,
                            ) => {
                                return Err(StoreError::OwnerRelease {
                                    id: id.clone(),
                                    source: DetachedControlError::NotCompleted,
                                });
                            }
                            Err(source) => {
                                return Err(StoreError::OwnerRelease { id: id.clone(), source });
                            }
                        }
                    }
                    Err(source) => {
                        return Err(StoreError::OwnerRelease { id: id.clone(), source });
                    }
                }
            }
        }
        Ok(projection)
    }

    pub fn start_journal(&self, id: &SwarmId) -> Result<JournalHandle, StoreError> {
        self.load_spec(id)?;
        let run_dir = self.run_dir(id);
        let lock_path = run_dir.join(WRITER_LOCK_FILE);
        let lock = open_private(&lock_path, false)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(StoreError::WriterBusy(id.clone()));
            }
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(StoreError::Write { path: lock_path, source });
            }
        }
        let existing = self.read_journal(id)?;
        if existing.partial_tail.is_some() {
            return Err(StoreError::IncompleteTerminalLine { path: run_dir.join(JOURNAL_FILE) });
        }
        let journal_path = run_dir.join(JOURNAL_FILE);
        let journal = open_private(&journal_path, true)?;
        let (sender, receiver) = mpsc::channel(JOURNAL_CAPACITY);
        let join =
            tokio::spawn(journal_task(receiver, journal, lock, journal_path, existing.projection));
        Ok(JournalHandle { sender, join })
    }

    pub fn request_cancellation(&self, id: &SwarmId) -> Result<(), StoreError> {
        self.load_spec(id)?;
        let run_dir = self.run_dir(id);
        let writer = AtomicFileWriter::new(&run_dir, ".control.lock", ".cancel-request");
        let _lock = writer.lock()?;
        let request =
            CancellationRequest { schema_version: SWARM_SCHEMA_VERSION, requested_at_ms: now_ms() };
        let bytes = serde_json::to_vec_pretty(&request).map_err(|source| StoreError::Parse {
            path: run_dir.join(CANCEL_REQUEST_FILE),
            source,
        })?;
        let path = run_dir.join(CANCEL_REQUEST_FILE);
        writer.replace(&path, &bytes)?;
        set_private_file(&path)?;
        Ok(())
    }

    pub fn cancellation_requested(&self, id: &SwarmId) -> Result<bool, StoreError> {
        let path = self.run_dir(id).join(CANCEL_REQUEST_FILE);
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(source) => return Err(StoreError::Read { path, source }),
        };
        let request: CancellationRequest = serde_json::from_slice(&raw)
            .map_err(|source| StoreError::Parse { path: path.clone(), source })?;
        if request.schema_version != SWARM_SCHEMA_VERSION {
            return Err(StoreError::Spec {
                path,
                source: super::model::SpecError::Schema {
                    actual: request.schema_version,
                    expected: SWARM_SCHEMA_VERSION,
                },
            });
        }
        Ok(true)
    }

    pub fn clear_cancellation(&self, id: &SwarmId) -> Result<(), StoreError> {
        let path = self.run_dir(id).join(CANCEL_REQUEST_FILE);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Write { path, source }),
        }
    }

    pub fn write_result(&self, projection: &SwarmProjection) -> Result<RunResult, StoreError> {
        if !projection.status.is_terminal() {
            return Err(StoreError::NonTerminalResult(projection.spec.id.clone()));
        }
        let result = RunResult {
            schema_version: SWARM_SCHEMA_VERSION,
            id: projection.spec.id.clone(),
            terminal_sequence: projection.last_sequence,
            status: projection.status,
            result: projection.result.clone(),
            failure: projection.failure.clone(),
        };
        let run_dir = self.run_dir(&projection.spec.id);
        let writer = AtomicFileWriter::new(&run_dir, ".result.lock", ".result");
        let _lock = writer.lock()?;
        let path = run_dir.join(RESULT_FILE);
        let bytes = serde_json::to_vec_pretty(&result)
            .map_err(|source| StoreError::Parse { path: path.clone(), source })?;
        writer.replace(&path, &bytes)?;
        set_private_file(&path)?;
        Ok(result)
    }

    /// Return the derived result only when it exactly matches canonical terminal Journal state.
    pub fn valid_result(&self, id: &SwarmId) -> Result<Option<RunResult>, StoreError> {
        let path = self.run_dir(id).join(RESULT_FILE);
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StoreError::Read { path, source }),
        };
        let Ok(result) = serde_json::from_slice::<RunResult>(&raw) else {
            return Ok(None);
        };
        let journal_path = self.run_dir(id).join(JOURNAL_FILE);
        let Some(terminal) = read_last_complete_record(&journal_path)? else {
            return Ok(None);
        };
        let (status, output, failure) = match &terminal.event {
            SwarmEvent::RunSucceeded { result } => {
                (super::model::RunStatus::Succeeded, Some(result), None)
            }
            SwarmEvent::RunFailed { error } => (super::model::RunStatus::Failed, None, Some(error)),
            SwarmEvent::RunCancelled {} => (super::model::RunStatus::Cancelled, None, None),
            _ => return Ok(None),
        };
        if result.schema_version != SWARM_SCHEMA_VERSION
            || &result.id != id
            || result.terminal_sequence != terminal.sequence
            || result.status != status
            || result.result.as_ref() != output
            || result.failure.as_ref() != failure
        {
            return Ok(None);
        }
        Ok(Some(result))
    }

    pub fn remove_never_started(&self, id: &SwarmId) -> Result<(), StoreError> {
        let journal = self.read_journal(id)?;
        if !journal.records.is_empty() || journal.partial_tail.is_some() {
            return Err(StoreError::StartedRun(id.clone()));
        }
        let path = self.run_dir(id);
        std::fs::remove_dir_all(&path).map_err(|source| StoreError::Write { path, source })
    }

    pub fn delete(&self, id: &SwarmId) -> Result<(), StoreError> {
        let projection = self.read_journal(id)?.projection;
        if !projection.status.is_terminal()
            && projection.status != super::model::RunStatus::Orphaned
        {
            return Err(StoreError::NonDeletableRun(id.clone()));
        }
        if projection.status.is_terminal() && self.valid_result(id)?.is_none() {
            return Err(StoreError::TerminalResultUnavailable(id.clone()));
        }
        let path = self.run_dir(id);
        std::fs::remove_dir_all(&path).map_err(|source| StoreError::Write { path, source })
    }

    fn reserve_id(&self) -> Result<SwarmId, StoreError> {
        let writer = AtomicFileWriter::new(&self.root, ".runs.lock", ".next-run");
        let _lock = writer.lock()?;
        let path = self.root.join(COUNTER_FILE);
        let current = match std::fs::read_to_string(&path) {
            Ok(raw) => raw
                .trim()
                .parse::<u64>()
                .map_err(|source| StoreError::Counter { path: path.clone(), source })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(source) => return Err(StoreError::Read { path, source }),
        };
        let next = current.checked_add(1).ok_or(StoreError::CounterOverflow)?;
        writer.replace(&path, format!("{next}\n").as_bytes())?;
        set_private_file(&path)?;
        SwarmId::new(format!("swarm-{next}")).map_err(|_| StoreError::CounterOverflow)
    }

    fn publish_spec(&self, spec: &SwarmSpec) -> Result<(), StoreError> {
        let run_dir = self.run_dir(&spec.id);
        let writer = AtomicFileWriter::new(&run_dir, ".spec.lock", ".spec");
        let _lock = writer.lock()?;
        let path = run_dir.join(SPEC_FILE);
        if path.exists() {
            return Err(StoreError::Write {
                path,
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "immutable swarm spec already exists",
                ),
            });
        }
        let bytes = serde_json::to_vec_pretty(spec)
            .map_err(|source| StoreError::Parse { path: path.clone(), source })?;
        writer.replace(&path, &bytes)?;
        set_private_file(&path)
    }
}

fn read_last_complete_record(path: &Path) -> Result<Option<SwarmEventRecord>, StoreError> {
    const SCAN_CHUNK: u64 = 64 * 1024;

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(StoreError::Read { path: path.to_owned(), source }),
    };
    let length =
        file.metadata().map_err(|source| StoreError::Read { path: path.to_owned(), source })?.len();
    if length == 0 {
        return Ok(None);
    }

    file.seek(SeekFrom::End(-1))
        .map_err(|source| StoreError::Read { path: path.to_owned(), source })?;
    let mut final_byte = [0_u8; 1];
    file.read_exact(&mut final_byte)
        .map_err(|source| StoreError::Read { path: path.to_owned(), source })?;
    if final_byte[0] != b'\n' || length == 1 {
        return Ok(None);
    }

    let record_end = length - 1;
    let mut cursor = record_end;
    let mut record_start = 0;
    while cursor > 0 {
        let chunk_start = cursor.saturating_sub(SCAN_CHUNK);
        let chunk_length =
            usize::try_from(cursor - chunk_start).expect("journal scan chunk always fits usize");
        let mut chunk = vec![0_u8; chunk_length];
        file.seek(SeekFrom::Start(chunk_start))
            .and_then(|_| file.read_exact(&mut chunk))
            .map_err(|source| StoreError::Read { path: path.to_owned(), source })?;
        if let Some(index) = chunk.iter().rposition(|byte| *byte == b'\n') {
            record_start = chunk_start + index as u64 + 1;
            break;
        }
        cursor = chunk_start;
    }
    if record_start == record_end {
        return Ok(None);
    }

    let record_length =
        usize::try_from(record_end - record_start).map_err(|_| StoreError::Read {
            path: path.to_owned(),
            source: std::io::Error::other("terminal Journal record exceeds addressable memory"),
        })?;
    let mut raw = vec![0_u8; record_length];
    file.seek(SeekFrom::Start(record_start))
        .and_then(|_| file.read_exact(&mut raw))
        .map_err(|source| StoreError::Read { path: path.to_owned(), source })?;
    Ok(serde_json::from_slice(&raw).ok())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalRead {
    pub records: Vec<SwarmEventRecord>,
    pub projection: SwarmProjection,
    pub partial_tail: Option<Vec<u8>>,
}

pub struct JournalTail {
    path: PathBuf,
    offset: u64,
    buffer: Vec<u8>,
    projection: SwarmProjection,
}

impl JournalTail {
    pub fn projection(&self) -> &SwarmProjection {
        &self.projection
    }

    pub fn has_partial_tail(&self) -> bool {
        !self.buffer.is_empty()
    }

    pub fn refresh(&mut self) -> Result<Vec<SwarmEventRecord>, StoreError> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(StoreError::Read { path: self.path.clone(), source }),
        };
        let length = file
            .metadata()
            .map_err(|source| StoreError::Read { path: self.path.clone(), source })?
            .len();
        if length < self.offset {
            return Err(StoreError::JournalTruncated { path: self.path.clone() });
        }
        file.seek(SeekFrom::Start(self.offset))
            .map_err(|source| StoreError::Read { path: self.path.clone(), source })?;
        let mut appended = Vec::new();
        file.read_to_end(&mut appended)
            .map_err(|source| StoreError::Read { path: self.path.clone(), source })?;
        self.offset = length;
        self.buffer.extend(appended);

        let mut records = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=newline).collect();
            line.pop();
            if line.is_empty() {
                return Err(StoreError::EmptyJournalLine { path: self.path.clone() });
            }
            let record = serde_json::from_slice::<SwarmEventRecord>(&line)
                .map_err(|source| StoreError::Parse { path: self.path.clone(), source })?;
            self.projection.apply(record.clone())?;
            records.push(record);
        }
        if !self.buffer.is_empty() && self.projection.status.is_terminal() {
            return Err(StoreError::IncompleteTerminalLine { path: self.path.clone() });
        }
        Ok(records)
    }
}

#[derive(Clone, Debug)]
pub struct JournalReceipt {
    pub record: SwarmEventRecord,
    pub projection: SwarmProjection,
}

pub struct JournalHandle {
    sender: mpsc::Sender<JournalCommand>,
    join: tokio::task::JoinHandle<Result<(), StoreError>>,
}

impl JournalHandle {
    pub fn sink(&self) -> JournalSink {
        JournalSink { sender: self.sender.clone() }
    }

    pub async fn append(&self, event: SwarmEvent) -> Result<JournalReceipt, StoreError> {
        self.sink().append(event).await
    }

    pub async fn shutdown(self) -> Result<(), StoreError> {
        self.sender.send(JournalCommand::Shutdown).await.map_err(|_| StoreError::WriterStopped)?;
        self.join.await.map_err(|error| StoreError::WriterTask(error.to_string()))?
    }
}

#[derive(Clone)]
pub struct JournalSink {
    sender: mpsc::Sender<JournalCommand>,
}

impl JournalSink {
    pub async fn append(&self, event: SwarmEvent) -> Result<JournalReceipt, StoreError> {
        let (acknowledge, receipt) = oneshot::channel();
        self.sender
            .send(JournalCommand::Append { event, acknowledge })
            .await
            .map_err(|_| StoreError::WriterStopped)?;
        receipt.await.map_err(|_| StoreError::WriterStopped)?
    }
}

enum JournalCommand {
    Append { event: SwarmEvent, acknowledge: oneshot::Sender<Result<JournalReceipt, StoreError>> },
    Shutdown,
}

async fn journal_task(
    mut receiver: mpsc::Receiver<JournalCommand>,
    mut journal: File,
    _writer_lock: File,
    journal_path: PathBuf,
    mut projection: SwarmProjection,
) -> Result<(), StoreError> {
    while let Some(command) = receiver.recv().await {
        match command {
            JournalCommand::Append { event, acknowledge } => {
                let record = SwarmEventRecord {
                    sequence: projection.last_sequence + 1,
                    at_ms: now_ms(),
                    event,
                };
                let mut next = projection.clone();
                let outcome = match next.apply(record.clone()) {
                    Ok(()) => append_record(&mut journal, &journal_path, &record).map(|()| {
                        projection = next;
                        JournalReceipt { record, projection: projection.clone() }
                    }),
                    Err(error) => Err(StoreError::Projection(error)),
                };
                let _ = acknowledge.send(outcome);
            }
            JournalCommand::Shutdown => return Ok(()),
        }
    }
    Ok(())
}

fn append_record(
    journal: &mut File,
    path: &Path,
    record: &SwarmEventRecord,
) -> Result<(), StoreError> {
    let mut bytes = serde_json::to_vec(record)
        .map_err(|source| StoreError::Parse { path: path.to_path_buf(), source })?;
    bytes.push(b'\n');
    journal
        .write_all(&bytes)
        .and_then(|()| journal.flush())
        .and_then(|()| journal.sync_data())
        .map_err(|source| StoreError::Write { path: path.to_path_buf(), source })
}

fn decode_journal(spec: SwarmSpec, path: &Path, bytes: &[u8]) -> Result<JournalRead, StoreError> {
    let mut records = Vec::new();
    let complete_length =
        bytes.iter().rposition(|byte| *byte == b'\n').map_or(0, |index| index + 1);
    let mut cursor = 0;
    while cursor < complete_length {
        let newline = bytes[cursor..complete_length]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| cursor + index)
            .expect("complete Journal prefix ends with a newline");
        let line = &bytes[cursor..newline];
        if line.is_empty() {
            return Err(StoreError::EmptyJournalLine { path: path.to_path_buf() });
        }
        records.push(
            serde_json::from_slice(line)
                .map_err(|source| StoreError::Parse { path: path.to_path_buf(), source })?,
        );
        cursor = newline + 1;
    }
    let partial_tail = (complete_length < bytes.len()).then(|| bytes[complete_length..].to_vec());
    let projection = SwarmProjection::replay(spec, records.iter().cloned())?;
    if partial_tail.is_some() && projection.status.is_terminal() {
        return Err(StoreError::IncompleteTerminalLine { path: path.to_path_buf() });
    }
    Ok(JournalRead { records, projection, partial_tail })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancellationRequest {
    schema_version: u32,
    requested_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunResult {
    pub schema_version: u32,
    pub id: SwarmId,
    pub terminal_sequence: u64,
    pub status: super::model::RunStatus,
    pub result: Option<super::model::SynthesisOutput>,
    pub failure: Option<String>,
}

fn run_number(id: &SwarmId) -> Option<u64> {
    id.as_str().strip_prefix("swarm-")?.parse().ok()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn create_private_directory(path: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(path)
        .map_err(|source| StoreError::CreateDirectory { path: path.to_path_buf(), source })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|source| StoreError::Write { path: path.to_path_buf(), source })?;
    }
    Ok(())
}

fn open_private(path: &Path, append: bool) -> Result<File, StoreError> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).append(append);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|source| StoreError::Write { path: path.to_path_buf(), source })?;
    set_private_file(path)?;
    Ok(file)
}

fn set_private_file(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|source| StoreError::Write { path: path.to_path_buf(), source })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::tools::swarm::model::{
        AgentId, CodexItem, CodexItemKind, ItemLifecycle, Stage, SwarmEvent, SwarmOwner, WaitReason,
    };

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kit-swarm-store-{name}-{}", std::process::id()))
    }

    fn new_spec(index: usize) -> NewSwarmSpec {
        NewSwarmSpec {
            prompt: format!("Evaluate architecture {index}"),
            working_directory: std::env::current_dir().unwrap(),
            model: None,
            reasoning: ReasoningEffort::High,
            debate: DebatePolicy::Enabled,
            retry_limit: 2,
        }
    }

    fn owner() -> SwarmOwner {
        SwarmOwner::fixture()
    }

    fn valid_specs(store: &SwarmStore) -> Vec<SwarmSpec> {
        store
            .discover()
            .unwrap()
            .into_iter()
            .filter_map(|run| match run {
                DiscoveredRun::Valid(spec) => Some(spec),
                DiscoveredRun::Corrupt { .. } => None,
            })
            .collect()
    }

    #[test]
    fn concurrent_reservations_are_unique_and_private() {
        let root = test_root("reserve");
        let _ = std::fs::remove_dir_all(&root);
        let store = SwarmStore::at(root.clone()).unwrap();
        let threads: Vec<_> = (0..16)
            .map(|index| {
                let store = store.clone();
                std::thread::spawn(move || store.create(new_spec(index)).unwrap().id)
            })
            .collect();
        let ids: HashSet<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
        assert_eq!(ids.len(), 16);
        assert_eq!(valid_specs(&store).len(), 16);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&root).unwrap().permissions().mode() & 0o077, 0);
            for spec in valid_specs(&store) {
                let run_dir = store.run_dir(&spec.id);
                assert_eq!(std::fs::metadata(&run_dir).unwrap().permissions().mode() & 0o077, 0);
                assert_eq!(
                    std::fs::metadata(run_dir.join(SPEC_FILE)).unwrap().permissions().mode()
                        & 0o077,
                    0
                );
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn discovery_isolates_corrupt_specs_without_hiding_valid_runs() {
        let root = test_root("corrupt-discovery");
        let _ = std::fs::remove_dir_all(&root);
        let store = SwarmStore::at(root.clone()).unwrap();
        let corrupt = store.create(new_spec(1)).unwrap();
        let valid = store.create(new_spec(2)).unwrap();
        std::fs::write(store.run_dir(&corrupt.id).join(SPEC_FILE), b"{").unwrap();

        let discovered = store.discover().unwrap();
        assert_eq!(discovered.len(), 2);
        assert!(matches!(
            &discovered[0],
            DiscoveredRun::Corrupt { id, error }
                if id == &corrupt.id && error.contains("parse swarm state")
        ));
        assert!(matches!(
            &discovered[1],
            DiscoveredRun::Valid(spec) if spec.id == valid.id
        ));
        assert_eq!(
            valid_specs(&store).into_iter().map(|spec| spec.id).collect::<Vec<_>>(),
            vec![valid.id]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn concurrent_process_reservations_are_unique() {
        let root = test_root("process-reserve");
        let _ = std::fs::remove_dir_all(&root);
        SwarmStore::at(root.clone()).unwrap();
        let executable = std::env::current_exe().unwrap();
        let children: Vec<_> = (0..8)
            .map(|index| {
                std::process::Command::new(&executable)
                    .arg("--ignored")
                    .arg("--exact")
                    .arg("tools::swarm::store::tests::reservation_process_child")
                    .env("KIT_SWARM_RESERVATION_ROOT", &root)
                    .env("KIT_SWARM_RESERVATION_INDEX", index.to_string())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        let store = SwarmStore::at(root.clone()).unwrap();
        let ids: HashSet<_> = valid_specs(&store).into_iter().map(|spec| spec.id).collect();
        assert_eq!(ids.len(), 8);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "helper invoked by concurrent_process_reservations_are_unique"]
    fn reservation_process_child() {
        let root = PathBuf::from(std::env::var_os("KIT_SWARM_RESERVATION_ROOT").unwrap());
        let index = std::env::var("KIT_SWARM_RESERVATION_INDEX").unwrap().parse().unwrap();
        SwarmStore::at(root).unwrap().create(new_spec(index)).unwrap();
    }

    #[tokio::test]
    async fn serialized_writer_is_exclusive_contiguous_and_replay_equivalent() {
        let root = test_root("journal");
        let _ = std::fs::remove_dir_all(&root);
        let store = SwarmStore::at(root.clone()).unwrap();
        let spec = store.create(new_spec(1)).unwrap();
        let journal = store.start_journal(&spec.id).unwrap();
        assert!(matches!(store.start_journal(&spec.id), Err(StoreError::WriterBusy(_))));

        journal.append(SwarmEvent::RunStarted { owner: owner() }).await.unwrap();
        journal.append(SwarmEvent::StageStarted { stage: Stage::Planning }).await.unwrap();
        let planner = AgentId::new("planner").unwrap();
        journal
            .append(SwarmEvent::AgentPrompted {
                agent: planner.clone(),
                stage: Stage::Planning,
                prompt: "planner prompt".to_owned(),
            })
            .await
            .unwrap();
        journal
            .append(SwarmEvent::AgentWaiting {
                agent: planner.clone(),
                stage: Stage::Planning,
                reason: WaitReason::TurnPermit,
            })
            .await
            .unwrap();
        journal
            .append(SwarmEvent::AgentStarted {
                agent: planner.clone(),
                stage: Stage::Planning,
                attempt: 1,
            })
            .await
            .unwrap();
        journal
            .append(SwarmEvent::ThreadStarted {
                agent: planner.clone(),
                thread_id: "thread-planner".to_owned(),
            })
            .await
            .unwrap();

        let sink = journal.sink();
        let tasks: Vec<_> = (0..64)
            .map(|index| {
                let sink = sink.clone();
                let planner = planner.clone();
                tokio::spawn(async move {
                    sink.append(SwarmEvent::Item {
                        agent: planner,
                        lifecycle: ItemLifecycle::Updated,
                        item: CodexItem {
                            id: format!("item-{index}"),
                            kind: CodexItemKind::Reasoning { text: format!("item {index}") },
                        },
                    })
                    .await
                    .unwrap()
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        let invalid = journal.append(SwarmEvent::RunStarted { owner: owner() }).await.unwrap_err();
        assert!(matches!(invalid, StoreError::Projection(_)));
        journal
            .append(SwarmEvent::RunFailed { error: "fixture complete".to_owned() })
            .await
            .unwrap();
        journal.shutdown().await.unwrap();

        let replay = store.read_journal(&spec.id).unwrap();
        assert_eq!(replay.records.len(), 71);
        assert_eq!(replay.projection.last_sequence, 71);
        assert_eq!(replay.projection.status, super::super::model::RunStatus::Failed);
        assert!(replay.partial_tail.is_none());
        for (index, record) in replay.records.iter().enumerate() {
            assert_eq!(record.sequence, index as u64 + 1);
        }
        let bytes = std::fs::read(store.run_dir(&spec.id).join(JOURNAL_FILE)).unwrap();
        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 71);

        let result = store.write_result(&replay.projection).unwrap();
        assert_eq!(store.valid_result(&spec.id).unwrap(), Some(result.clone()));
        std::fs::remove_file(store.run_dir(&spec.id).join(RESULT_FILE)).unwrap();
        assert_eq!(store.valid_result(&spec.id).unwrap(), None);
        assert_eq!(store.read_journal(&spec.id).unwrap().projection, replay.projection);

        let mut stale = result;
        stale.terminal_sequence += 1;
        std::fs::write(
            store.run_dir(&spec.id).join(RESULT_FILE),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();
        assert_eq!(store.valid_result(&spec.id).unwrap(), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn live_partial_tail_is_buffered_but_terminal_partial_tail_is_corrupt() {
        let root = test_root("partial");
        let _ = std::fs::remove_dir_all(&root);
        let store = SwarmStore::at(root.clone()).unwrap();
        let spec = store.create(new_spec(1)).unwrap();
        let path = store.run_dir(&spec.id).join(JOURNAL_FILE);
        let started = SwarmEventRecord {
            sequence: 1,
            at_ms: 1,
            event: SwarmEvent::RunStarted { owner: owner() },
        };
        let mut bytes = serde_json::to_vec(&started).unwrap();
        bytes.push(b'\n');
        bytes.extend_from_slice(br#"{"sequence":2"#);
        std::fs::write(&path, &bytes).unwrap();
        let live = store.read_journal(&spec.id).unwrap();
        assert_eq!(live.records.len(), 1);
        assert!(live.partial_tail.is_some());

        let failed = SwarmEventRecord {
            sequence: 2,
            at_ms: 2,
            event: SwarmEvent::RunFailed { error: "done".to_owned() },
        };
        let mut terminal = serde_json::to_vec(&started).unwrap();
        terminal.push(b'\n');
        terminal.extend(serde_json::to_vec(&failed).unwrap());
        terminal.push(b'\n');
        terminal.extend_from_slice(b"partial");
        std::fs::write(&path, terminal).unwrap();
        assert!(matches!(
            store.read_journal(&spec.id),
            Err(StoreError::IncompleteTerminalLine { .. })
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn incremental_tail_only_applies_new_complete_records() {
        let root = test_root("incremental-tail");
        let _ = std::fs::remove_dir_all(&root);
        let store = SwarmStore::at(root.clone()).unwrap();
        let spec = store.create(new_spec(1)).unwrap();
        let path = store.run_dir(&spec.id).join(JOURNAL_FILE);
        let started = SwarmEventRecord {
            sequence: 1,
            at_ms: 1,
            event: SwarmEvent::RunStarted { owner: owner() },
        };
        let failed = SwarmEventRecord {
            sequence: 2,
            at_ms: 2,
            event: SwarmEvent::RunFailed { error: "done".to_owned() },
        };
        let mut first = serde_json::to_vec(&started).unwrap();
        first.push(b'\n');
        std::fs::write(&path, first).unwrap();
        let mut tail = store.tail(&spec.id).unwrap();
        assert_eq!(tail.projection().last_sequence, 1);

        let second = serde_json::to_vec(&failed).unwrap();
        let split = second.len() / 2;
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&second[..split]).unwrap();
        file.flush().unwrap();
        assert!(tail.refresh().unwrap().is_empty());
        assert!(tail.has_partial_tail());
        file.write_all(&second[split..]).unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();
        let records = tail.refresh().unwrap();
        assert_eq!(records, vec![failed]);
        assert_eq!(tail.projection().status, super::super::model::RunStatus::Failed);
        assert!(!tail.has_partial_tail());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn missing_detached_authority_projects_unavailable_without_pid_fallback() {
        let root = test_root("orphan");
        let _ = std::fs::remove_dir_all(&root);
        let store = SwarmStore::at(root.clone()).unwrap();
        let spec = store.create(new_spec(1)).unwrap();
        let record = SwarmEventRecord {
            sequence: 1,
            at_ms: 1,
            event: SwarmEvent::RunStarted { owner: owner() },
        };
        let mut bytes = serde_json::to_vec(&record).unwrap();
        bytes.push(b'\n');
        std::fs::write(store.run_dir(&spec.id).join(JOURNAL_FILE), bytes).unwrap();
        let processes = ProcessSupervisor::for_test(root.join("processes")).unwrap();

        assert_eq!(
            store.inspect(&processes, &spec.id).await.unwrap().status,
            super::super::model::RunStatus::Unavailable
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn delete_requires_terminal_or_orphaned_state() {
        let root = test_root("delete-safety");
        let _ = std::fs::remove_dir_all(&root);
        let store = SwarmStore::at(root.clone()).unwrap();
        let spec = store.create(new_spec(1)).unwrap();
        assert!(matches!(store.delete(&spec.id), Err(StoreError::NonDeletableRun(_))));

        let journal = store.start_journal(&spec.id).unwrap();
        journal.append(SwarmEvent::RunStarted { owner: owner() }).await.unwrap();
        assert!(matches!(store.delete(&spec.id), Err(StoreError::NonDeletableRun(_))));
        journal
            .append(SwarmEvent::RunFailed { error: "fixture complete".to_owned() })
            .await
            .unwrap();
        journal.shutdown().await.unwrap();
        let replay = store.read_journal(&spec.id).unwrap();
        store.write_result(&replay.projection).unwrap();

        store.delete(&spec.id).unwrap();
        assert!(!store.run_dir(&spec.id).exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
