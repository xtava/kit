use std::{
    fs::{DirBuilder, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context as _, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use crate::framework::{
    process::{
        CompletionCause, DescendantDisposition, LeaderExitObservation, OutputReport, ProcessReport,
        RecordAvailability, RecordDisposition, TerminationDisposition,
    },
    AtomicFileWriter, Context,
};

use super::{manifest::Workflow, ProtocolArtifacts};

const EVIDENCE_SCHEMA_VERSION: u32 = 2;
const MAX_EVIDENCE_RECORDS: usize = 8;
const MAX_EVIDENCE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_EVIDENCE_MARKER_BYTES: u64 = 256 * 1024;
const MAX_FAILURE_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_REQUEST_EVIDENCE_BYTES: u64 = 256 * 1024;
const MARKER_FILE: &str = "evidence.json";

const STDOUT_TRANSCRIPT: &str = "output/stdout.log";
const STDERR_TRANSCRIPT: &str = "output/stderr.log";
const STDOUT_TAIL: &str = "output/stdout.tail";
const STDERR_TAIL: &str = "output/stderr.tail";
const REQUEST_ARTIFACT: &str = "protocol/request.json";
const EVENTS_ARTIFACT: &str = "protocol/events.jsonl";
const RESULT_ARTIFACT: &str = "protocol/result.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildEvidenceRecord {
    schema_version: u32,
    run_id: String,
    workflow_id: String,
    workflow_label: String,
    created_unix_seconds: u64,
    failure: String,
    failure_truncated: bool,
    process: CompletedProcessEvidence,
    stdout: OutputEvidence,
    stderr: OutputEvidence,
    protocol: ProtocolEvidence,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ContainmentEvidence {
    CompleteTree,
    ProcessGroup,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletedProcessEvidence {
    completion: CompletionCause,
    leader_exit: LeaderExitObservation,
    containment: ContainmentEvidence,
    descendants: DescendantDisposition,
    termination: TerminationDisposition,
    elapsed_milliseconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputEvidence {
    observed_bytes: u64,
    retained_bytes: u64,
    disposition: RecordDisposition,
    availability: RecordAvailability,
    transcript: SnapshotFile,
    final_tail: SnapshotFile,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolEvidence {
    request: SnapshotFile,
    events: SnapshotFile,
    result: SnapshotFile,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum SnapshotFile {
    Stored { path: String, bytes: u64 },
    Missing,
    Unavailable,
    NotRegular,
    ChangedBeforeSnapshot,
    ChangedDuringSnapshot,
    ExceededLimit { limit_bytes: u64 },
}

#[derive(Debug)]
pub(super) enum CaptureOutcome {
    Stored(StoredEvidence),
    AtCapacity(CapacityEvidence),
}

#[derive(Debug)]
pub(super) struct StoredEvidence {
    pub(super) run_id: String,
    pub(super) directory: PathBuf,
    pub(super) bytes: u64,
}

#[derive(Debug)]
pub(super) struct CapacityEvidence {
    pub(super) records: usize,
    pub(super) bytes: u64,
    pub(super) candidate_reserved_bytes: u64,
}

impl CapacityEvidence {
    pub(super) fn render(&self) -> String {
        format!(
            "the Build evidence store currently uses {} of {} records and {} of {} bytes; this run requires capacity for up to {} bytes",
            self.records,
            MAX_EVIDENCE_RECORDS,
            self.bytes,
            MAX_EVIDENCE_BYTES,
            self.candidate_reserved_bytes
        )
    }
}

pub(super) fn capture(
    workflow: &Workflow,
    failure: &str,
    artifacts: &ProtocolArtifacts,
    report: &ProcessReport,
) -> Result<CaptureOutcome> {
    let store = EvidenceStore::bootstrap()?;
    store.capture(workflow, failure, artifacts, report)
}

pub(super) fn list(cx: &Context) -> Result<()> {
    let store = EvidenceStore::bootstrap()?;
    let inventory = store.inventory()?;
    let output = EvidenceListOutput::from_inventory(&inventory);
    if cx.out.is_json() {
        return cx.out.json(&output);
    }

    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    if output.records.is_empty() {
        writeln!(writer, "no retained build evidence")?;
    } else {
        for record in &output.records {
            let workflow = match (&record.workflow_id, &record.workflow_label) {
                (Some(id), Some(label)) => format!("  {id} ({label})"),
                _ => String::new(),
            };
            writeln!(
                writer,
                "{}  {}  {} bytes{}",
                record.run_id,
                record.state.as_str(),
                record.bytes,
                workflow
            )?;
        }
    }
    writeln!(
        writer,
        "usage: {} of {} records, {} of {} bytes",
        output.record_count, output.max_records, output.bytes, output.max_bytes
    )?;
    writer.flush().context("flush Build evidence list")
}

pub(super) fn inspect(cx: &Context, run_id: &str) -> Result<()> {
    let output = inspect_record(run_id)?;
    if cx.out.is_json() {
        return cx.out.json(&output);
    }

    let directory = PathBuf::from(&output.directory);
    let record = &output.record;
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();
    writeln!(writer, "build evidence {}", record.run_id)?;
    writeln!(writer, "directory: {}", escape_path(&directory))?;
    writeln!(writer, "stored bytes: {}", output.bytes)?;
    writeln!(writer, "workflow: {} ({})", record.workflow_id, record.workflow_label)?;
    writeln!(writer, "created unix seconds: {}", record.created_unix_seconds)?;
    writeln!(
        writer,
        "process: {:?}, {:?}, {:?}, {:?}, {:?}, {} ms",
        record.process.completion,
        record.process.leader_exit,
        record.process.containment,
        record.process.descendants,
        record.process.termination,
        record.process.elapsed_milliseconds
    )?;
    writeln!(
        writer,
        "failure{}: {}",
        if record.failure_truncated { " (truncated)" } else { "" },
        super::escape_terminal_controls(&record.failure)
    )?;
    render_output(&mut writer, "stdout", &record.stdout, &directory)?;
    render_output(&mut writer, "stderr", &record.stderr, &directory)?;
    render_snapshot(&mut writer, "request", &record.protocol.request, &directory)?;
    render_snapshot(&mut writer, "events", &record.protocol.events, &directory)?;
    render_snapshot(&mut writer, "result", &record.protocol.result, &directory)?;
    writeln!(writer, "forget: kit build evidence forget {run_id}")?;
    writer.flush().context("flush Build evidence inspection")
}

fn inspect_record(run_id: &str) -> Result<EvidenceInspectOutput> {
    super::protocol::validate_canonical_run_id(run_id)?;
    let store = EvidenceStore::bootstrap()?;
    let _lock = store.lock()?;
    let directory = store.runs.join(run_id);
    if !path_entry_exists(&directory)? {
        let pending = store.pending.join(run_id);
        if path_entry_exists(&pending)? {
            bail!(
                "build evidence capture for run {run_id} is incomplete; remove it deliberately with `kit build evidence forget {run_id}`"
            );
        }
        bail!("no retained build evidence exists for run {run_id}");
    }
    let bytes = directory_size(&directory)?;
    let record = read_record(&directory, run_id).with_context(|| {
        format!(
            "read retained build evidence for run {run_id}; remove the corrupt record with `kit build evidence forget {run_id}`"
        )
    })?;
    let output =
        EvidenceInspectOutput { directory: directory.display().to_string(), bytes, record };
    Ok(output)
}

pub(super) fn forget(cx: &Context, run_id: &str) -> Result<()> {
    let output = forget_record(run_id)?;
    if cx.out.is_json() {
        cx.out.json(&output)
    } else {
        let stdout = std::io::stdout();
        let mut writer = stdout.lock();
        writeln!(writer, "forgot build evidence {run_id} ({} bytes)", output.freed_bytes)?;
        writer.flush().context("flush Build evidence removal result")
    }
}

fn forget_record(run_id: &str) -> Result<EvidenceForgetOutput> {
    super::protocol::validate_canonical_run_id(run_id)?;
    let store = EvidenceStore::bootstrap()?;
    let _lock = store.lock()?;
    let published = store.runs.join(run_id);
    let pending = store.pending.join(run_id);
    let mut removed = Vec::new();
    let mut bytes = 0u64;
    for (state, path) in [(EvidenceState::Stored, published), (EvidenceState::Incomplete, pending)]
    {
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                bytes = bytes
                    .checked_add(directory_size(&path)?)
                    .context("count forgotten build evidence bytes")?;
                remove_exact_path(&path)?;
                sync_directory(path.parent().context("build evidence path has no parent")?)?;
                removed.push(state);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect build evidence path {}", path.display()));
            }
        }
    }
    if removed.is_empty() {
        bail!("no retained or incomplete build evidence exists for run {run_id}");
    }

    Ok(EvidenceForgetOutput {
        run_id: run_id.to_owned(),
        removed_states: removed,
        freed_bytes: bytes,
    })
}

#[derive(Clone, Debug)]
pub(super) struct TuiEvidenceRecord {
    pub(super) run_id: String,
    pub(super) state: String,
    pub(super) bytes: u64,
    pub(super) workflow: String,
}

pub(super) fn tui_records() -> Result<Vec<TuiEvidenceRecord>> {
    let store = EvidenceStore::bootstrap()?;
    let inventory = store.inventory()?;
    Ok(inventory
        .entries
        .into_iter()
        .map(|entry| TuiEvidenceRecord {
            run_id: entry.run_id,
            state: entry.state.as_str().to_owned(),
            bytes: entry.bytes,
            workflow: entry
                .record
                .map(|record| format!("{} ({})", record.workflow_id, record.workflow_label))
                .unwrap_or_else(|| "unavailable".to_owned()),
        })
        .collect())
}

pub(super) fn tui_inspect(run_id: &str) -> Result<String> {
    let output = inspect_record(run_id)?;
    serde_json::to_string_pretty(&output).context("serialize Build evidence inspection")
}

pub(super) fn tui_forget(run_id: &str) -> Result<String> {
    let output = forget_record(run_id)?;
    Ok(format!("forgot build evidence {} ({} bytes)", output.run_id, output.freed_bytes))
}

struct EvidenceStore {
    root: PathBuf,
    runs: PathBuf,
    pending: PathBuf,
    writer: AtomicFileWriter,
}

impl EvidenceStore {
    fn bootstrap() -> Result<Self> {
        #[cfg(not(unix))]
        bail!(
            "private Build evidence storage is unavailable on this platform; Kit does not yet have an owner-only ACL and durable-directory implementation here"
        );

        let project = ProjectDirs::from("", "", "kit")
            .context("resolve Kit state directory for Build evidence")?;
        let base = project.state_dir().unwrap_or_else(|| project.data_local_dir());
        let root = base.join("build-evidence");
        let runs = root.join("runs");
        let pending = root.join("pending");
        if !base.exists() {
            std::fs::create_dir_all(base).with_context(|| {
                format!("create Kit state directory for Build evidence {}", base.display())
            })?;
        }
        ensure_private_directory(&root)?;
        ensure_private_directory(&runs)?;
        ensure_private_directory(&pending)?;
        let writer = AtomicFileWriter::new(&root, "control.lock", ".control");
        Ok(Self { root, runs, pending, writer })
    }

    fn lock(&self) -> Result<File> {
        self.writer.lock().context("lock Build evidence store")
    }

    fn inventory(&self) -> Result<Inventory> {
        let _lock = self.lock()?;
        self.inventory_locked()
    }

    fn inventory_locked(&self) -> Result<Inventory> {
        let mut entries = Vec::new();
        scan_evidence_directory(&self.runs, EvidenceState::Stored, &mut entries)?;
        scan_evidence_directory(&self.pending, EvidenceState::Incomplete, &mut entries)?;
        entries.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        let bytes = entries.iter().try_fold(0u64, |total, entry| {
            total.checked_add(entry.bytes).context("count retained Build evidence bytes")
        })?;
        Ok(Inventory { entries, bytes })
    }

    fn capture(
        &self,
        workflow: &Workflow,
        failure: &str,
        artifacts: &ProtocolArtifacts,
        report: &ProcessReport,
    ) -> Result<CaptureOutcome> {
        let _lock = self.lock()?;
        let run_id = report.run_id.to_string();
        super::protocol::validate_canonical_run_id(&run_id)?;
        if path_entry_exists(&self.runs.join(&run_id))?
            || path_entry_exists(&self.pending.join(&run_id))?
        {
            bail!("Build evidence already exists for run {run_id}");
        }

        let inventory = self.inventory_locked()?;
        let stdout = PlannedOutput::new(&report.stdout, STDOUT_TRANSCRIPT, STDOUT_TAIL)?;
        let stderr = PlannedOutput::new(&report.stderr, STDERR_TRANSCRIPT, STDERR_TAIL)?;
        let request = SnapshotPlan::new(
            &artifacts.request,
            REQUEST_ARTIFACT,
            MAX_REQUEST_EVIDENCE_BYTES,
            None,
        );
        let events = SnapshotPlan::new(
            &artifacts.events,
            EVENTS_ARTIFACT,
            super::protocol::MAX_EVENT_STREAM_BYTES
                .checked_add(1)
                .context("calculate Build event evidence limit")?,
            None,
        );
        let result = SnapshotPlan::new(
            &artifacts.result,
            RESULT_ARTIFACT,
            super::protocol::MAX_FINAL_RESULT_BYTES
                .checked_add(1)
                .context("calculate Build result evidence limit")?,
            None,
        );
        let candidate_reserved_bytes = [
            stdout.reserved_bytes()?,
            stderr.reserved_bytes()?,
            request.reserved_bytes(),
            events.reserved_bytes(),
            result.reserved_bytes(),
            MAX_EVIDENCE_MARKER_BYTES,
        ]
        .into_iter()
        .try_fold(0u64, |total, bytes| {
            total.checked_add(bytes).context("calculate Build evidence reservation")
        })?;
        let projected_bytes = inventory
            .bytes
            .checked_add(candidate_reserved_bytes)
            .context("calculate projected Build evidence usage")?;
        if inventory.entries.len() >= MAX_EVIDENCE_RECORDS || projected_bytes > MAX_EVIDENCE_BYTES {
            return Ok(CaptureOutcome::AtCapacity(CapacityEvidence {
                records: inventory.entries.len(),
                bytes: inventory.bytes,
                candidate_reserved_bytes,
            }));
        }

        let staging = self.pending.join(&run_id);
        let destination = self.runs.join(&run_id);
        create_private_directory_new(&staging)?;
        let mut destination_published = false;
        let publication = (|| {
            create_private_directory_new(&staging.join("output"))?;
            create_private_directory_new(&staging.join("protocol"))?;
            let stdout = stdout.materialize(&staging)?;
            let stderr = stderr.materialize(&staging)?;
            let protocol = ProtocolEvidence {
                request: request.materialize(&staging)?,
                events: events.materialize(&staging)?,
                result: result.materialize(&staging)?,
            };
            let (failure, failure_truncated) = bounded_failure(failure);
            let elapsed_milliseconds = u64::try_from(report.elapsed.as_millis())
                .context("Build process elapsed time exceeds the evidence format")?;
            let containment = match report.containment {
                crate::framework::process::ContainmentStrength::CompleteTree => {
                    ContainmentEvidence::CompleteTree
                }
                crate::framework::process::ContainmentStrength::ProcessGroup => {
                    ContainmentEvidence::ProcessGroup
                }
            };
            let record = BuildEvidenceRecord {
                schema_version: EVIDENCE_SCHEMA_VERSION,
                run_id: run_id.clone(),
                workflow_id: workflow.id.clone(),
                workflow_label: workflow.label.as_str().to_owned(),
                created_unix_seconds: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .context("system clock precedes the Unix epoch")?
                    .as_secs(),
                failure,
                failure_truncated,
                process: CompletedProcessEvidence {
                    completion: report.completion,
                    leader_exit: report.leader_exit,
                    containment,
                    descendants: report.descendants,
                    termination: report.termination,
                    elapsed_milliseconds,
                },
                stdout,
                stderr,
                protocol,
            };
            write_record(&staging, &record)?;
            sync_directory(&staging.join("output"))?;
            sync_directory(&staging.join("protocol"))?;
            sync_directory(&staging)?;

            let bytes = directory_size(&staging)?;
            let final_usage = inventory
                .bytes
                .checked_add(bytes)
                .context("calculate final Build evidence usage")?;
            if final_usage > MAX_EVIDENCE_BYTES {
                bail!("Build evidence exceeded its admitted byte reservation");
            }
            std::fs::rename(&staging, &destination).with_context(|| {
                format!("atomically publish Build evidence {}", destination.display())
            })?;
            destination_published = true;
            sync_directory(&self.pending)?;
            sync_directory(&self.runs)?;
            sync_directory(&self.root)?;
            Ok(StoredEvidence { run_id: run_id.clone(), directory: destination.clone(), bytes })
        })();

        match publication {
            Ok(stored) => Ok(CaptureOutcome::Stored(stored)),
            Err(error) => {
                let cleanup_path = if destination_published { &destination } else { &staging };
                let cleanup = match std::fs::symlink_metadata(cleanup_path) {
                    Ok(_) => remove_exact_path(cleanup_path),
                    Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(source) => Err(source).with_context(|| {
                        format!("inspect incomplete Build evidence {}", cleanup_path.display())
                    }),
                };
                match cleanup {
                    Ok(()) => {
                        let cleanup_sync = cleanup_path
                            .parent()
                            .context("Build evidence cleanup path has no parent")
                            .and_then(sync_directory);
                        match cleanup_sync {
                            Ok(()) => Err(error),
                            Err(cleanup_error) => Err(anyhow::anyhow!(
                                "{error:#}; cleanup incomplete Build evidence: {cleanup_error:#}; a record for run {run_id} may remain visible to `kit build evidence list`"
                            )),
                        }
                    }
                    Err(cleanup_error) => Err(anyhow::anyhow!(
                        "{error:#}; cleanup incomplete Build evidence: {cleanup_error:#}; a record for run {run_id} may remain visible to `kit build evidence list`"
                    )),
                }
            }
        }
    }
}

struct PlannedOutput {
    observed_bytes: u64,
    retained_bytes: u64,
    disposition: RecordDisposition,
    availability: RecordAvailability,
    transcript: SnapshotPlan,
    final_tail: Vec<u8>,
    tail_path: &'static str,
}

impl PlannedOutput {
    fn new(
        report: &OutputReport,
        transcript_path: &'static str,
        tail_path: &'static str,
    ) -> Result<Self> {
        let OutputReport::Recorded(report) = report else {
            bail!("completed Build process report did not contain recorded output evidence");
        };
        if report.final_tail.len() > super::FAILURE_TRANSCRIPT_TAIL_BYTES {
            bail!("completed Build process report exceeded its final-tail invariant");
        }
        let expected =
            (report.availability == RecordAvailability::Available).then_some(report.retained_bytes);
        let transcript = if report.availability == RecordAvailability::Available {
            SnapshotPlan::new(
                report.path.as_path(),
                transcript_path,
                super::MAX_DURABLE_TRANSCRIPT_BYTES,
                expected,
            )
        } else {
            SnapshotPlan::Status(SnapshotFile::Unavailable)
        };
        Ok(Self {
            observed_bytes: report.observed_bytes,
            retained_bytes: report.retained_bytes,
            disposition: report.disposition,
            availability: report.availability,
            transcript,
            final_tail: report.final_tail.to_vec(),
            tail_path,
        })
    }

    fn reserved_bytes(&self) -> Result<u64> {
        self.transcript
            .reserved_bytes()
            .checked_add(
                u64::try_from(self.final_tail.len()).context("count Build final-tail bytes")?,
            )
            .context("calculate Build output evidence reservation")
    }

    fn materialize(self, staging: &Path) -> Result<OutputEvidence> {
        let transcript = self.transcript.materialize(staging)?;
        let tail = staging.join(self.tail_path);
        write_private_file(&tail, &self.final_tail)?;
        Ok(OutputEvidence {
            observed_bytes: self.observed_bytes,
            retained_bytes: self.retained_bytes,
            disposition: self.disposition,
            availability: self.availability,
            transcript,
            final_tail: SnapshotFile::Stored {
                path: self.tail_path.to_owned(),
                bytes: u64::try_from(self.final_tail.len())
                    .context("count Build final-tail bytes")?,
            },
        })
    }
}

enum SnapshotPlan {
    Copy { file: File, bytes: u64, destination: &'static str },
    Status(SnapshotFile),
}

impl SnapshotPlan {
    fn new(
        source: &Path,
        destination: &'static str,
        limit: u64,
        expected_bytes: Option<u64>,
    ) -> Self {
        let file = match open_snapshot_source(source) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Self::Status(SnapshotFile::Missing);
            }
            Err(_) => return Self::Status(SnapshotFile::Unavailable),
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(_) => return Self::Status(SnapshotFile::Unavailable),
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Self::Status(SnapshotFile::NotRegular);
        }
        if metadata.len() > limit {
            return Self::Status(SnapshotFile::ExceededLimit { limit_bytes: limit });
        }
        if expected_bytes.is_some_and(|expected| expected != metadata.len()) {
            return Self::Status(SnapshotFile::ChangedBeforeSnapshot);
        }
        Self::Copy { file, bytes: metadata.len(), destination }
    }

    fn reserved_bytes(&self) -> u64 {
        match self {
            Self::Copy { bytes, .. } => *bytes,
            Self::Status(_) => 0,
        }
    }

    fn materialize(self, staging: &Path) -> Result<SnapshotFile> {
        match self {
            Self::Status(status) => Ok(status),
            Self::Copy { file, bytes, destination } => {
                snapshot_open_file(file, bytes, staging, destination)
            }
        }
    }
}

fn snapshot_open_file(
    mut source: File,
    expected_bytes: u64,
    staging: &Path,
    relative_destination: &'static str,
) -> Result<SnapshotFile> {
    let destination = staging.join(relative_destination);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut output = options
        .open(&destination)
        .with_context(|| format!("create Build evidence snapshot {}", destination.display()))?;
    let mut copied = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    while copied < expected_bytes {
        let remaining = expected_bytes - copied;
        let buffer_bytes =
            u64::try_from(buffer.len()).context("count Build evidence snapshot buffer bytes")?;
        let read_capacity = usize::try_from(remaining.min(buffer_bytes))
            .context("calculate Build evidence snapshot buffer")?;
        let read = match source.read(&mut buffer[..read_capacity]) {
            Ok(0) => {
                drop(output);
                remove_partial_snapshot(&destination)?;
                return Ok(SnapshotFile::ChangedDuringSnapshot);
            }
            Ok(read) => read,
            Err(_) => {
                drop(output);
                remove_partial_snapshot(&destination)?;
                return Ok(SnapshotFile::Unavailable);
            }
        };
        output
            .write_all(&buffer[..read])
            .with_context(|| format!("write Build evidence snapshot {}", destination.display()))?;
        copied = copied
            .checked_add(u64::try_from(read).context("count Build evidence snapshot bytes")?)
            .context("count Build evidence snapshot bytes")?;
    }
    let mut extra = [0u8; 1];
    match source.read(&mut extra) {
        Ok(0) => {}
        Ok(_) => {
            drop(output);
            remove_partial_snapshot(&destination)?;
            return Ok(SnapshotFile::ChangedDuringSnapshot);
        }
        Err(_) => {
            drop(output);
            remove_partial_snapshot(&destination)?;
            return Ok(SnapshotFile::Unavailable);
        }
    }
    match source.metadata() {
        Ok(metadata) if metadata.len() == expected_bytes => {}
        Ok(_) => {
            drop(output);
            remove_partial_snapshot(&destination)?;
            return Ok(SnapshotFile::ChangedDuringSnapshot);
        }
        Err(_) => {
            drop(output);
            remove_partial_snapshot(&destination)?;
            return Ok(SnapshotFile::Unavailable);
        }
    }
    output
        .sync_all()
        .with_context(|| format!("sync Build evidence snapshot {}", destination.display()))?;
    Ok(SnapshotFile::Stored { path: relative_destination.to_owned(), bytes: copied })
}

fn open_snapshot_source(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn remove_partial_snapshot(path: &Path) -> Result<()> {
    std::fs::remove_file(path)
        .with_context(|| format!("remove incomplete Build evidence snapshot {}", path.display()))
}

fn write_record(directory: &Path, record: &BuildEvidenceRecord) -> Result<()> {
    let serialized =
        serde_json::to_vec_pretty(record).context("serialize Build evidence marker")?;
    if u64::try_from(serialized.len()).context("count Build evidence marker bytes")?
        > MAX_EVIDENCE_MARKER_BYTES
    {
        bail!("Build evidence marker exceeds its {MAX_EVIDENCE_MARKER_BYTES}-byte limit");
    }
    write_private_file(&directory.join(MARKER_FILE), &serialized)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("create Build evidence file {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write Build evidence file {}", path.display()))?;
    file.sync_all().with_context(|| format!("sync Build evidence file {}", path.display()))
}

fn read_record(directory: &Path, expected_run_id: &str) -> Result<BuildEvidenceRecord> {
    let directory_metadata = std::fs::symlink_metadata(directory)
        .with_context(|| format!("inspect Build evidence directory {}", directory.display()))?;
    validate_private_directory(directory, &directory_metadata)?;
    let path = directory.join(MARKER_FILE);
    let file = open_snapshot_source(&path)
        .with_context(|| format!("open Build evidence marker {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect Build evidence marker {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("Build evidence marker must be a regular file");
    }
    let limit = MAX_EVIDENCE_MARKER_BYTES
        .checked_add(1)
        .context("calculate Build evidence marker read limit")?;
    let mut bytes = Vec::new();
    file.take(limit)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read Build evidence marker {}", path.display()))?;
    if u64::try_from(bytes.len()).context("count Build evidence marker bytes")?
        > MAX_EVIDENCE_MARKER_BYTES
    {
        bail!("Build evidence marker exceeds its {MAX_EVIDENCE_MARKER_BYTES}-byte limit");
    }
    let record = serde_json::from_slice::<BuildEvidenceRecord>(&bytes)
        .with_context(|| format!("parse Build evidence marker {}", path.display()))?;
    validate_record(directory, &record, expected_run_id)?;
    Ok(record)
}

fn validate_record(
    directory: &Path,
    record: &BuildEvidenceRecord,
    expected_run_id: &str,
) -> Result<()> {
    if record.schema_version != EVIDENCE_SCHEMA_VERSION {
        bail!(
            "Build evidence schema version {} is unsupported; expected {EVIDENCE_SCHEMA_VERSION}",
            record.schema_version
        );
    }
    super::protocol::validate_canonical_run_id(&record.run_id)?;
    if record.run_id != expected_run_id {
        bail!("Build evidence marker belongs to a different run");
    }
    super::manifest::validate_workflow_id(&record.workflow_id)?;
    super::manifest::validate_workflow_label(&record.workflow_label)
        .context("validate Build evidence label")?;
    for (label, output, transcript_path, tail_path) in [
        ("stdout", &record.stdout, STDOUT_TRANSCRIPT, STDOUT_TAIL),
        ("stderr", &record.stderr, STDERR_TRANSCRIPT, STDERR_TAIL),
    ] {
        if output.retained_bytes > output.observed_bytes
            || output.retained_bytes > super::MAX_DURABLE_TRANSCRIPT_BYTES
        {
            bail!("Build evidence {label} byte accounting is invalid");
        }
        validate_snapshot(directory, &output.transcript, transcript_path, false)?;
        if let SnapshotFile::Stored { bytes, .. } = &output.transcript {
            if *bytes != output.retained_bytes {
                bail!("Build evidence {label} transcript length does not match its process report");
            }
        }
        validate_snapshot(directory, &output.final_tail, tail_path, true)?;
        if let SnapshotFile::Stored { bytes, .. } = &output.final_tail {
            if *bytes
                > u64::try_from(super::FAILURE_TRANSCRIPT_TAIL_BYTES)
                    .context("calculate Build final-tail evidence limit")?
            {
                bail!("Build evidence {label} final tail exceeds its limit");
            }
        }
    }
    validate_snapshot(directory, &record.protocol.request, REQUEST_ARTIFACT, false)?;
    validate_snapshot(directory, &record.protocol.events, EVENTS_ARTIFACT, false)?;
    validate_snapshot(directory, &record.protocol.result, RESULT_ARTIFACT, false)?;
    Ok(())
}

fn validate_snapshot(
    directory: &Path,
    snapshot: &SnapshotFile,
    expected_path: &str,
    required: bool,
) -> Result<()> {
    let SnapshotFile::Stored { path, bytes } = snapshot else {
        if required {
            bail!("Build evidence marker is missing a required stored payload");
        }
        return Ok(());
    };
    validate_relative_evidence_path(path)?;
    if path != expected_path {
        bail!("Build evidence marker names an unexpected payload path");
    }
    let payload = directory.join(path);
    let payload_directory = payload.parent().context("Build evidence payload has no parent")?;
    let payload_directory_metadata =
        std::fs::symlink_metadata(payload_directory).with_context(|| {
            format!("inspect Build evidence payload directory {}", payload_directory.display())
        })?;
    validate_private_directory(payload_directory, &payload_directory_metadata)?;
    let file = open_snapshot_source(&payload)
        .with_context(|| format!("open Build evidence payload {}", payload.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect Build evidence payload {}", payload.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("Build evidence payload is not a regular file: {}", payload.display());
    }
    if metadata.len() != *bytes {
        bail!(
            "Build evidence payload length changed for {}: expected {}, found {}",
            payload.display(),
            bytes,
            metadata.len()
        );
    }
    Ok(())
}

fn validate_relative_evidence_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("Build evidence marker contains an invalid relative path");
    }
    Ok(())
}

fn bounded_failure(value: &str) -> (String, bool) {
    if value.len() <= MAX_FAILURE_MESSAGE_BYTES {
        return (value.to_owned(), false);
    }
    let mut end = MAX_FAILURE_MESSAGE_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), true)
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceState {
    Stored,
    Corrupt,
    Incomplete,
}

impl EvidenceState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stored => "stored",
            Self::Corrupt => "corrupt",
            Self::Incomplete => "incomplete",
        }
    }
}

struct InventoryEntry {
    run_id: String,
    state: EvidenceState,
    bytes: u64,
    record: Option<BuildEvidenceRecord>,
}

struct Inventory {
    entries: Vec<InventoryEntry>,
    bytes: u64,
}

#[derive(Serialize)]
struct EvidenceListOutput {
    record_count: usize,
    bytes: u64,
    max_records: usize,
    max_bytes: u64,
    records: Vec<EvidenceListEntry>,
}

impl EvidenceListOutput {
    fn from_inventory(inventory: &Inventory) -> Self {
        let records = inventory
            .entries
            .iter()
            .map(|entry| EvidenceListEntry {
                run_id: entry.run_id.clone(),
                state: entry.state,
                bytes: entry.bytes,
                workflow_id: entry.record.as_ref().map(|record| record.workflow_id.clone()),
                workflow_label: entry.record.as_ref().map(|record| record.workflow_label.clone()),
                created_unix_seconds: entry
                    .record
                    .as_ref()
                    .map(|record| record.created_unix_seconds),
            })
            .collect();
        Self {
            record_count: inventory.entries.len(),
            bytes: inventory.bytes,
            max_records: MAX_EVIDENCE_RECORDS,
            max_bytes: MAX_EVIDENCE_BYTES,
            records,
        }
    }
}

#[derive(Serialize)]
struct EvidenceListEntry {
    run_id: String,
    state: EvidenceState,
    bytes: u64,
    workflow_id: Option<String>,
    workflow_label: Option<String>,
    created_unix_seconds: Option<u64>,
}

#[derive(Serialize)]
struct EvidenceInspectOutput {
    directory: String,
    bytes: u64,
    record: BuildEvidenceRecord,
}

#[derive(Serialize)]
struct EvidenceForgetOutput {
    run_id: String,
    removed_states: Vec<EvidenceState>,
    freed_bytes: u64,
}

fn scan_evidence_directory(
    directory: &Path,
    expected_state: EvidenceState,
    entries: &mut Vec<InventoryEntry>,
) -> Result<()> {
    for entry in std::fs::read_dir(directory)
        .with_context(|| format!("list Build evidence directory {}", directory.display()))?
    {
        let entry = entry
            .with_context(|| format!("read Build evidence entry in {}", directory.display()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Build evidence directory contains a non-UTF-8 name"))?;
        super::protocol::validate_canonical_run_id(&name).with_context(|| {
            format!("Build evidence directory contains unexpected entry '{name}'")
        })?;
        let path = entry.path();
        let bytes = directory_size(&path)?;
        let (state, record) = match expected_state {
            EvidenceState::Stored => match read_record(&path, &name) {
                Ok(record) => (EvidenceState::Stored, Some(record)),
                Err(_) => (EvidenceState::Corrupt, None),
            },
            EvidenceState::Incomplete => (EvidenceState::Incomplete, None),
            EvidenceState::Corrupt => unreachable!("corrupt is a derived evidence state"),
        };
        entries.push(InventoryEntry { run_id: name, state, bytes, record });
    }
    Ok(())
}

fn directory_size(path: &Path) -> Result<u64> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect Build evidence path {}", path.display()))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Ok(metadata.len());
    }
    let mut total = 0u64;
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("measure Build evidence directory {}", directory.display()))?
        {
            let entry = entry.with_context(|| {
                format!("measure Build evidence entry in {}", directory.display())
            })?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)
                .with_context(|| format!("inspect Build evidence path {}", path.display()))?;
            if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                pending.push(path);
            } else {
                total = total
                    .checked_add(metadata.len())
                    .context("count Build evidence directory bytes")?;
            }
        }
    }
    Ok(total)
}

fn render_output(
    writer: &mut impl Write,
    label: &str,
    output: &OutputEvidence,
    directory: &Path,
) -> Result<()> {
    writeln!(
        writer,
        "{label}: retained {} of {} bytes, {:?}, {:?}",
        output.retained_bytes, output.observed_bytes, output.disposition, output.availability
    )?;
    render_snapshot(writer, &format!("{label} transcript"), &output.transcript, directory)?;
    render_snapshot(writer, &format!("{label} final tail"), &output.final_tail, directory)
}

fn render_snapshot(
    writer: &mut impl Write,
    label: &str,
    snapshot: &SnapshotFile,
    directory: &Path,
) -> Result<()> {
    match snapshot {
        SnapshotFile::Stored { path, bytes } => {
            writeln!(writer, "{label}: {} ({bytes} bytes)", escape_path(&directory.join(path)))?
        }
        SnapshotFile::Missing => writeln!(writer, "{label}: missing when evidence was captured")?,
        SnapshotFile::Unavailable => {
            writeln!(writer, "{label}: unavailable when evidence was captured")?
        }
        SnapshotFile::NotRegular => writeln!(writer, "{label}: not a regular file")?,
        SnapshotFile::ChangedBeforeSnapshot => {
            writeln!(writer, "{label}: changed before evidence capture")?
        }
        SnapshotFile::ChangedDuringSnapshot => {
            writeln!(writer, "{label}: changed during evidence capture")?
        }
        SnapshotFile::ExceededLimit { limit_bytes } => {
            writeln!(writer, "{label}: exceeded the {limit_bytes}-byte evidence limit")?
        }
    }
    Ok(())
}

fn escape_path(path: &Path) -> String {
    super::escape_terminal_controls(&path.display().to_string())
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_directory(path, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match create_private_directory_new(path) {
                Ok(()) => {}
                Err(error)
                    if error.root_cause().downcast_ref::<std::io::Error>().is_some_and(
                        |source| source.kind() == std::io::ErrorKind::AlreadyExists,
                    ) => {}
                Err(error) => return Err(error),
            }
            let metadata = std::fs::symlink_metadata(path).with_context(|| {
                format!("inspect Build evidence directory {} after creation", path.display())
            })?;
            validate_private_directory(path, &metadata)
        }
        Err(error) => Err(error)
            .with_context(|| format!("inspect Build evidence directory {}", path.display())),
    }
}

fn create_private_directory_new(path: &Path) -> Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("create Build evidence directory {}", path.display()))?;
    set_private_directory_permissions(path)
}

fn validate_private_directory(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!("Build evidence path is not an owner-only directory: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != rustix::process::getuid().as_raw() || metadata.mode() & 0o077 != 0 {
            bail!("Build evidence path is not an owner-only directory: {}", path.display());
        }
    }
    Ok(())
}

fn set_private_directory_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protect Build evidence directory {}", path.display()))?;
    }
    Ok(())
}

fn remove_exact_path(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect Build evidence path {}", path.display()))?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("remove Build evidence directory {}", path.display()))
    } else {
        std::fs::remove_file(path)
            .with_context(|| format!("remove Build evidence path {}", path.display()))
    }
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("inspect Build evidence path {}", path.display()))
        }
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open Build evidence directory {}", path.display()))?
        .sync_all()
        .with_context(|| format!("sync Build evidence directory {}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}
