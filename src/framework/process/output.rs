use std::{
    collections::VecDeque,
    fmt, io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::ChildStdin,
    sync::{mpsc, oneshot, watch, Notify},
    task::JoinHandle,
};

use super::report::OutputReport;
use super::supervisor::RunDirectoryLease;
use super::{
    CaptureOverflow, OutputPolicy, PartialOutputReport, PrivateBytes, ProcessFailureKind,
    ProcessStream, RecordLimit, RecordOverflow, RecordPolicy,
};

const READ_BUFFER_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureDisposition {
    Complete,
    Truncated,
}

#[derive(Clone, Eq, PartialEq)]
pub struct CaptureReport {
    pub bytes: Box<[u8]>,
    pub observed_bytes: u64,
    pub retained_bytes: u64,
    pub disposition: CaptureDisposition,
}

impl fmt::Debug for CaptureReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CaptureReport")
            .field("observed_bytes", &self.observed_bytes)
            .field("retained_bytes", &self.retained_bytes)
            .field("disposition", &self.disposition)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct PartialCaptureReport {
    pub bytes: Box<[u8]>,
    pub observed_bytes: u64,
    pub retained_bytes: u64,
    pub disposition: CaptureDisposition,
}

impl fmt::Debug for PartialCaptureReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PartialCaptureReport")
            .field("observed_bytes", &self.observed_bytes)
            .field("retained_bytes", &self.retained_bytes)
            .field("disposition", &self.disposition)
            .finish_non_exhaustive()
    }
}

impl From<&CaptureReport> for PartialCaptureReport {
    fn from(report: &CaptureReport) -> Self {
        Self {
            bytes: report.bytes.clone(),
            observed_bytes: report.observed_bytes,
            retained_bytes: report.retained_bytes,
            disposition: report.disposition,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamReport {
    pub observed_bytes: u64,
    pub streamed_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartialStreamReport {
    pub observed_bytes: u64,
    pub streamed_bytes: u64,
}

impl From<&StreamReport> for PartialStreamReport {
    fn from(report: &StreamReport) -> Self {
        Self { observed_bytes: report.observed_bytes, streamed_bytes: report.streamed_bytes }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordDisposition {
    Complete,
    Truncated,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordAvailability {
    Available,
    Unavailable,
}

#[derive(Clone)]
pub struct RecordedOutputPath {
    path: PathBuf,
    _retention: Option<Arc<RunDirectoryLease>>,
}

impl RecordedOutputPath {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path, _retention: None }
    }

    pub(crate) fn retained(path: PathBuf, retention: Arc<RunDirectoryLease>) -> Self {
        Self { path, _retention: Some(retention) }
    }

    pub fn as_path(&self) -> &Path {
        &self.path
    }
}

impl PartialEq for RecordedOutputPath {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Eq for RecordedOutputPath {}

impl std::hash::Hash for RecordedOutputPath {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.path, state);
    }
}

impl fmt::Debug for RecordedOutputPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("RecordedOutputPath").field(&self.path).finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RecordReport {
    pub path: RecordedOutputPath,
    pub observed_bytes: u64,
    pub retained_bytes: u64,
    pub disposition: RecordDisposition,
    pub availability: RecordAvailability,
    pub final_tail: Box<[u8]>,
}

impl fmt::Debug for RecordReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordReport")
            .field("path", &self.path)
            .field("observed_bytes", &self.observed_bytes)
            .field("retained_bytes", &self.retained_bytes)
            .field("disposition", &self.disposition)
            .field("availability", &self.availability)
            .field("final_tail_bytes", &self.final_tail.len())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct PartialRecordReport {
    pub path: RecordedOutputPath,
    pub observed_bytes: u64,
    pub retained_bytes: u64,
    pub disposition: RecordDisposition,
    pub availability: RecordAvailability,
    pub final_tail: Box<[u8]>,
}

impl fmt::Debug for PartialRecordReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PartialRecordReport")
            .field("path", &self.path)
            .field("observed_bytes", &self.observed_bytes)
            .field("retained_bytes", &self.retained_bytes)
            .field("disposition", &self.disposition)
            .field("availability", &self.availability)
            .field("final_tail_bytes", &self.final_tail.len())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnavailableOutput {
    Capture,
    Stream,
    Record { path: Option<RecordedOutputPath> },
}

impl From<&RecordReport> for PartialRecordReport {
    fn from(report: &RecordReport) -> Self {
        Self {
            path: report.path.clone(),
            observed_bytes: report.observed_bytes,
            retained_bytes: report.retained_bytes,
            disposition: report.disposition,
            availability: report.availability,
            final_tail: report.final_tail.clone(),
        }
    }
}

pub enum ProcessInputHandle {
    Closed,
    Once(ProcessInputCompletion),
    Writable(ProcessInputWriter),
}

impl fmt::Debug for ProcessInputHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Closed => "ProcessInputHandle::Closed",
            Self::Once(_) => "ProcessInputHandle::Once(..)",
            Self::Writable(_) => "ProcessInputHandle::Writable(..)",
        })
    }
}

pub enum ProcessOutputHandle {
    Inherited,
    Discarded,
    CapturedAtCompletion,
    Stream(ProcessByteStream),
    Recorded(RecordedOutputTail),
}

impl fmt::Debug for ProcessOutputHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Inherited => "ProcessOutputHandle::Inherited",
            Self::Discarded => "ProcessOutputHandle::Discarded",
            Self::CapturedAtCompletion => "ProcessOutputHandle::CapturedAtCompletion",
            Self::Stream(_) => "ProcessOutputHandle::Stream(..)",
            Self::Recorded(_) => "ProcessOutputHandle::Recorded(..)",
        })
    }
}

pub enum ProcessByteEvent {
    Chunk { sequence: u64, bytes: Box<[u8]> },
    End,
}

impl fmt::Debug for ProcessByteEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Chunk { sequence, bytes } => formatter
                .debug_struct("ProcessByteEvent::Chunk")
                .field("sequence", sequence)
                .field("byte_count", &bytes.len())
                .finish(),
            Self::End => formatter.write_str("ProcessByteEvent::End"),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RecordTailRevision {
    pub sequence: u64,
    pub bytes: Box<[u8]>,
    pub observed_bytes: u64,
    pub retained_bytes: u64,
    pub disposition: RecordDisposition,
}

impl fmt::Debug for RecordTailRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordTailRevision")
            .field("sequence", &self.sequence)
            .field("tail_byte_count", &self.bytes.len())
            .field("observed_bytes", &self.observed_bytes)
            .field("retained_bytes", &self.retained_bytes)
            .field("disposition", &self.disposition)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordTailEvent {
    Revision(RecordTailRevision),
    End,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ProcessInputError {
    #[error("process input is closed")]
    Closed,
    #[error("process input I/O failed")]
    Io,
    #[error("process input owner is unavailable")]
    OwnerUnavailable,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ProcessOutputError {
    #[error("process output I/O failed")]
    Io,
    #[error("process output owner is unavailable")]
    OwnerUnavailable,
}

pub struct ProcessInputCompletion {
    completion: oneshot::Receiver<Result<(), ProcessInputError>>,
}

impl fmt::Debug for ProcessInputCompletion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProcessInputCompletion(..)")
    }
}

impl ProcessInputCompletion {
    pub async fn wait(self) -> Result<(), ProcessInputError> {
        self.completion.await.unwrap_or(Err(ProcessInputError::OwnerUnavailable))
    }
}

pub struct ProcessInputWriter {
    stdin: Option<ChildStdin>,
}

impl fmt::Debug for ProcessInputWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProcessInputWriter(..)")
    }
}

impl ProcessInputWriter {
    pub(crate) fn new(stdin: ChildStdin) -> Self {
        Self { stdin: Some(stdin) }
    }

    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), ProcessInputError> {
        let stdin = self.stdin.as_mut().ok_or(ProcessInputError::Closed)?;
        stdin.write_all(bytes).await.map_err(map_input_io)
    }

    pub async fn flush(&mut self) -> Result<(), ProcessInputError> {
        let stdin = self.stdin.as_mut().ok_or(ProcessInputError::Closed)?;
        stdin.flush().await.map_err(map_input_io)
    }

    pub async fn close(mut self) -> Result<(), ProcessInputError> {
        let mut stdin = self.stdin.take().ok_or(ProcessInputError::Closed)?;
        stdin.shutdown().await.map_err(map_input_io)
    }
}

pub struct ProcessByteStream {
    channel: Arc<ByteChannel>,
    ended: bool,
}

impl fmt::Debug for ProcessByteStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProcessByteStream(..)")
    }
}

impl ProcessByteStream {
    pub async fn next(&mut self) -> Result<ProcessByteEvent, ProcessOutputError> {
        if self.ended {
            return Ok(ProcessByteEvent::End);
        }
        loop {
            let notified = self.channel.available.notified();
            let outcome = {
                let mut state = self.channel.state.lock().expect("byte channel lock poisoned");
                if let Some(chunk) = state.chunks.pop_front() {
                    state.queued_bytes -= chunk.bytes.len();
                    Some(Ok(ProcessByteEvent::Chunk {
                        sequence: chunk.sequence,
                        bytes: chunk.bytes,
                    }))
                } else if state.finished {
                    self.ended = true;
                    Some(Ok(ProcessByteEvent::End))
                } else if !state.owner_available {
                    Some(Err(ProcessOutputError::OwnerUnavailable))
                } else {
                    None
                }
            };
            match outcome {
                Some(result) => {
                    self.channel.capacity.notify_waiters();
                    return result;
                }
                None => notified.await,
            }
        }
    }
}

impl Drop for ProcessByteStream {
    fn drop(&mut self) {
        let mut state = self.channel.state.lock().expect("byte channel lock poisoned");
        state.receiver_open = false;
        drop(state);
        self.channel.capacity.notify_waiters();
    }
}

pub struct RecordedOutputTail {
    updates: watch::Receiver<RecordTailUpdate>,
    ended: bool,
}

impl fmt::Debug for RecordedOutputTail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RecordedOutputTail(..)")
    }
}

impl RecordedOutputTail {
    pub async fn next(&mut self) -> Result<RecordTailEvent, ProcessOutputError> {
        if self.ended {
            return Ok(RecordTailEvent::End);
        }
        self.updates.changed().await.map_err(|_| ProcessOutputError::OwnerUnavailable)?;
        match self.updates.borrow_and_update().clone() {
            RecordTailUpdate::Pending => Err(ProcessOutputError::OwnerUnavailable),
            RecordTailUpdate::Revision(revision) => Ok(RecordTailEvent::Revision(revision)),
            RecordTailUpdate::End => {
                self.ended = true;
                Ok(RecordTailEvent::End)
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct OutputPumpCompletion {
    pub(crate) report: OutputReport,
    pub(crate) failure: Option<ObservedProcessFailure>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ObservedProcessFailure {
    pub(crate) sequence: u64,
    pub(crate) kind: ProcessFailureKind,
}

pub(crate) fn earliest_observed_failure(
    first: Option<ObservedProcessFailure>,
    second: Option<ObservedProcessFailure>,
) -> Option<ObservedProcessFailure> {
    match (first, second) {
        (Some(first), Some(second)) if second.sequence < first.sequence => Some(second),
        (Some(first), _) => Some(first),
        (None, second) => second,
    }
}

pub(crate) fn unavailable_partial_output(
    policy: OutputPolicy,
    recorded_path: Option<RecordedOutputPath>,
) -> PartialOutputReport {
    match policy {
        OutputPolicy::Inherit => PartialOutputReport::Inherited,
        OutputPolicy::Discard => PartialOutputReport::Discarded,
        OutputPolicy::Capture(_) => PartialOutputReport::Unavailable(UnavailableOutput::Capture),
        OutputPolicy::Stream(_) => PartialOutputReport::Unavailable(UnavailableOutput::Stream),
        OutputPolicy::Record(_) => {
            PartialOutputReport::Unavailable(UnavailableOutput::Record { path: recorded_path })
        }
    }
}

pub(crate) fn partial_from_output_report(report: OutputReport) -> PartialOutputReport {
    match report {
        OutputReport::Inherited => PartialOutputReport::Inherited,
        OutputReport::Discarded => PartialOutputReport::Discarded,
        OutputReport::Captured(report) => PartialOutputReport::Captured(PartialCaptureReport {
            bytes: report.bytes,
            observed_bytes: report.observed_bytes,
            retained_bytes: report.retained_bytes,
            disposition: report.disposition,
        }),
        OutputReport::Streamed(report) => PartialOutputReport::Streamed(PartialStreamReport {
            observed_bytes: report.observed_bytes,
            streamed_bytes: report.streamed_bytes,
        }),
        OutputReport::Recorded(report) => PartialOutputReport::Recorded(PartialRecordReport {
            path: report.path,
            observed_bytes: report.observed_bytes,
            retained_bytes: report.retained_bytes,
            disposition: report.disposition,
            availability: report.availability,
            final_tail: report.final_tail,
        }),
    }
}

pub(crate) fn spawn_once_input(
    mut stdin: ChildStdin,
    bytes: PrivateBytes,
) -> (ProcessInputCompletion, JoinHandle<Result<(), ProcessInputError>>) {
    let (completion_tx, completion_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let bytes = bytes.into_bytes();
        let write_result = stdin.write_all(bytes.as_slice()).await.map_err(map_input_io);
        let close_result = stdin.shutdown().await.map_err(map_input_io);
        let result = write_result.and(close_result);
        let _ = completion_tx.send(result);
        result
    });
    (ProcessInputCompletion { completion: completion_rx }, task)
}

pub(crate) fn spawn_output_pump<R>(
    reader: R,
    policy: OutputPolicy,
    stream: ProcessStream,
    prepared_record: Option<(std::fs::File, RecordedOutputPath)>,
    observation_sequence: Arc<AtomicU64>,
) -> (
    ProcessOutputHandle,
    JoinHandle<OutputPumpCompletion>,
    mpsc::UnboundedReceiver<ObservedProcessFailure>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (failure_tx, failure_rx) = mpsc::unbounded_channel();
    match policy {
        OutputPolicy::Inherit => (
            ProcessOutputHandle::Inherited,
            tokio::spawn(async {
                OutputPumpCompletion { report: OutputReport::Inherited, failure: None }
            }),
            failure_rx,
        ),
        OutputPolicy::Discard => (
            ProcessOutputHandle::Discarded,
            tokio::spawn(async {
                OutputPumpCompletion { report: OutputReport::Discarded, failure: None }
            }),
            failure_rx,
        ),
        OutputPolicy::Capture(policy) => (
            ProcessOutputHandle::CapturedAtCompletion,
            tokio::spawn(pump_capture(reader, policy, stream, observation_sequence, failure_tx)),
            failure_rx,
        ),
        OutputPolicy::Stream(policy) => {
            let channel = Arc::new(ByteChannel::new(policy.in_flight_byte_budget.get()));
            let output = ProcessByteStream { channel: Arc::clone(&channel), ended: false };
            let sender = ByteSender { channel, finished: false };
            (
                ProcessOutputHandle::Stream(output),
                tokio::spawn(pump_stream(reader, sender, stream, observation_sequence, failure_tx)),
                failure_rx,
            )
        }
        OutputPolicy::Record(policy) => {
            let (file, path) = prepared_record
                .expect("record output file is prepared before the target process is spawned");
            let (updates, output) = recorded_tail();
            (
                ProcessOutputHandle::Recorded(output),
                tokio::spawn(pump_record(
                    reader,
                    tokio::fs::File::from_std(file),
                    path,
                    policy,
                    stream,
                    observation_sequence,
                    updates,
                    failure_tx,
                )),
                failure_rx,
            )
        }
    }
}

async fn pump_capture<R>(
    mut reader: R,
    policy: super::CapturePolicy,
    stream: ProcessStream,
    observation_sequence: Arc<AtomicU64>,
    failure_tx: mpsc::UnboundedSender<ObservedProcessFailure>,
) -> OutputPumpCompletion
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0u8; READ_BUFFER_BYTES.min(policy.limit.get())];
    let mut retained = Vec::with_capacity(policy.limit.get().min(READ_BUFFER_BYTES));
    let mut observed_bytes = 0u64;
    let mut truncated = false;
    let mut failure = None;

    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                observed_bytes = observed_bytes.saturating_add(read as u64);
                let remaining = policy.limit.get().saturating_sub(retained.len());
                let retaining = remaining.min(read);
                retained.extend_from_slice(&buffer[..retaining]);
                if retaining < read {
                    truncated = true;
                    if policy.overflow == CaptureOverflow::FailAndTerminate {
                        publish_failure(
                            &mut failure,
                            &failure_tx,
                            ProcessFailureKind::OutputLimitExceeded { stream },
                            &observation_sequence,
                        );
                    }
                }
            }
            Err(_) => {
                publish_failure(
                    &mut failure,
                    &failure_tx,
                    ProcessFailureKind::OutputIo { stream },
                    &observation_sequence,
                );
                break;
            }
        }
    }

    let retained_bytes = retained.len() as u64;
    OutputPumpCompletion {
        report: OutputReport::Captured(CaptureReport {
            bytes: retained.into_boxed_slice(),
            observed_bytes,
            retained_bytes,
            disposition: if truncated {
                CaptureDisposition::Truncated
            } else {
                CaptureDisposition::Complete
            },
        }),
        failure,
    }
}

async fn pump_stream<R>(
    mut reader: R,
    mut sender: ByteSender,
    stream: ProcessStream,
    observation_sequence: Arc<AtomicU64>,
    failure_tx: mpsc::UnboundedSender<ObservedProcessFailure>,
) -> OutputPumpCompletion
where
    R: AsyncRead + Unpin,
{
    let mut buffer = vec![0u8; READ_BUFFER_BYTES.min(sender.byte_budget())];
    let mut observed_bytes = 0u64;
    let mut streamed_bytes = 0u64;
    let mut consumer_lost = false;
    let mut failure = None;

    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                observed_bytes = observed_bytes.saturating_add(read as u64);
                let sequence = next_observation_sequence(&observation_sequence);
                if !consumer_lost {
                    let bytes = buffer[..read].to_vec().into_boxed_slice();
                    match sender.send(ByteChunk { sequence, bytes }).await {
                        Ok(()) => streamed_bytes = streamed_bytes.saturating_add(read as u64),
                        Err(()) => {
                            consumer_lost = true;
                            publish_failure(
                                &mut failure,
                                &failure_tx,
                                ProcessFailureKind::RequiredConsumerLost { stream },
                                &observation_sequence,
                            );
                        }
                    }
                }
            }
            Err(_) => {
                publish_failure(
                    &mut failure,
                    &failure_tx,
                    ProcessFailureKind::OutputIo { stream },
                    &observation_sequence,
                );
                break;
            }
        }
    }
    if sender.finish().is_err() {
        publish_failure(
            &mut failure,
            &failure_tx,
            ProcessFailureKind::RequiredConsumerLost { stream },
            &observation_sequence,
        );
    }

    OutputPumpCompletion {
        report: OutputReport::Streamed(StreamReport { observed_bytes, streamed_bytes }),
        failure,
    }
}

#[allow(clippy::too_many_arguments)]
async fn pump_record<R>(
    reader: R,
    file: tokio::fs::File,
    path: RecordedOutputPath,
    policy: RecordPolicy,
    stream: ProcessStream,
    observation_sequence: Arc<AtomicU64>,
    updates: watch::Sender<RecordTailUpdate>,
    failure_tx: mpsc::UnboundedSender<ObservedProcessFailure>,
) -> OutputPumpCompletion
where
    R: AsyncRead + Unpin,
{
    let mut failure = None;
    let evidence = drain_record(
        reader,
        file,
        policy.durable_limit,
        policy.live_tail_byte_budget.get(),
        policy.overflow,
        |progress| {
            let sequence = next_observation_sequence(&observation_sequence);
            updates.send_replace(RecordTailUpdate::Revision(RecordTailRevision {
                sequence,
                bytes: progress.final_tail.to_vec().into_boxed_slice(),
                observed_bytes: progress.observed_bytes,
                retained_bytes: progress.retained_bytes,
                disposition: progress.disposition,
            }));
        },
        |kind| {
            let kind = match kind {
                RecordDrainFailure::Io => ProcessFailureKind::OutputIo { stream },
                RecordDrainFailure::LimitExceeded => {
                    ProcessFailureKind::OutputLimitExceeded { stream }
                }
            };
            publish_failure(&mut failure, &failure_tx, kind, &observation_sequence);
        },
    )
    .await;
    updates.send_replace(RecordTailUpdate::End);

    OutputPumpCompletion {
        report: OutputReport::Recorded(RecordReport {
            path,
            observed_bytes: evidence.observed_bytes,
            retained_bytes: evidence.retained_bytes,
            disposition: evidence.disposition,
            availability: evidence.availability,
            final_tail: evidence.final_tail,
        }),
        failure,
    }
}

pub(crate) struct RecordDrainEvidence {
    pub(crate) observed_bytes: u64,
    pub(crate) retained_bytes: u64,
    pub(crate) disposition: RecordDisposition,
    pub(crate) availability: RecordAvailability,
    pub(crate) final_tail: Box<[u8]>,
}

pub(crate) struct RecordDrainProgress<'a> {
    pub(crate) observed_bytes: u64,
    pub(crate) retained_bytes: u64,
    pub(crate) disposition: RecordDisposition,
    pub(crate) final_tail: &'a [u8],
}

#[derive(Clone, Copy)]
pub(crate) enum RecordDrainFailure {
    Io,
    LimitExceeded,
}

pub(crate) async fn drain_record<R, P, F>(
    mut reader: R,
    mut file: tokio::fs::File,
    limit: RecordLimit,
    final_tail_byte_budget: usize,
    overflow: RecordOverflow,
    mut publish_progress: P,
    mut publish_failure: F,
) -> RecordDrainEvidence
where
    R: AsyncRead + Unpin,
    P: FnMut(RecordDrainProgress<'_>),
    F: FnMut(RecordDrainFailure),
{
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    let mut tail = VecDeque::with_capacity(final_tail_byte_budget.min(READ_BUFFER_BYTES));
    let mut observed_bytes = 0u64;
    let mut retained_bytes = 0u64;
    let mut truncated = false;
    let mut output_available = true;

    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                observed_bytes = observed_bytes.saturating_add(read as u64);
                extend_bounded_tail(&mut tail, &buffer[..read], final_tail_byte_budget);
                let retain_limit = record_remaining(limit, retained_bytes);
                let retaining = read.min(retain_limit);
                let mut written = 0usize;
                if output_available && retaining > 0 {
                    match write_counted(&mut file, &buffer[..retaining]).await {
                        Ok(count) => written = count,
                        Err((count, _)) => {
                            written = count;
                            output_available = false;
                            publish_failure(RecordDrainFailure::Io);
                        }
                    }
                }
                retained_bytes = retained_bytes.saturating_add(written as u64);
                if retaining < read || written < retaining {
                    truncated = true;
                    if retaining < read && overflow == RecordOverflow::FailAndTerminate {
                        publish_failure(RecordDrainFailure::LimitExceeded);
                    }
                }
                publish_progress(RecordDrainProgress {
                    observed_bytes,
                    retained_bytes,
                    disposition: if truncated {
                        RecordDisposition::Truncated
                    } else {
                        RecordDisposition::Complete
                    },
                    final_tail: tail.make_contiguous(),
                });
            }
            Err(_) => {
                output_available = false;
                publish_failure(RecordDrainFailure::Io);
                break;
            }
        }
    }

    if output_available && (file.flush().await.is_err() || file.sync_all().await.is_err()) {
        output_available = false;
        publish_failure(RecordDrainFailure::Io);
    }

    RecordDrainEvidence {
        observed_bytes,
        retained_bytes,
        disposition: if truncated {
            RecordDisposition::Truncated
        } else {
            RecordDisposition::Complete
        },
        availability: if output_available {
            RecordAvailability::Available
        } else {
            RecordAvailability::Unavailable
        },
        final_tail: tail.into_iter().collect::<Vec<_>>().into_boxed_slice(),
    }
}

fn map_input_io(error: io::Error) -> ProcessInputError {
    match error.kind() {
        io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected => ProcessInputError::Closed,
        _ => ProcessInputError::Io,
    }
}

fn publish_failure(
    failure: &mut Option<ObservedProcessFailure>,
    sender: &mpsc::UnboundedSender<ObservedProcessFailure>,
    kind: ProcessFailureKind,
    observation_sequence: &AtomicU64,
) {
    if failure.is_some() {
        return;
    }
    let observed =
        ObservedProcessFailure { sequence: next_observation_sequence(observation_sequence), kind };
    *failure = Some(observed);
    let _ = sender.send(observed);
}

fn next_observation_sequence(sequence: &AtomicU64) -> u64 {
    sequence.fetch_add(1, Ordering::Relaxed).saturating_add(1)
}

fn record_remaining(limit: RecordLimit, retained_bytes: u64) -> usize {
    match limit {
        RecordLimit::Unlimited => usize::MAX,
        RecordLimit::Bytes(limit) => {
            usize::try_from(limit.get().saturating_sub(retained_bytes)).unwrap_or(usize::MAX)
        }
    }
}

async fn write_counted(
    file: &mut tokio::fs::File,
    bytes: &[u8],
) -> Result<usize, (usize, io::Error)> {
    let mut written = 0usize;
    while written < bytes.len() {
        match file.write(&bytes[written..]).await {
            Ok(0) => {
                return Err((
                    written,
                    io::Error::new(io::ErrorKind::WriteZero, "record output write returned zero"),
                ));
            }
            Ok(count) => written += count,
            Err(error) => return Err((written, error)),
        }
    }
    Ok(written)
}

fn extend_bounded_tail(tail: &mut VecDeque<u8>, bytes: &[u8], budget: usize) {
    if bytes.len() >= budget {
        tail.clear();
        tail.extend(bytes[bytes.len() - budget..].iter().copied());
        return;
    }
    let excess = tail.len().saturating_add(bytes.len()).saturating_sub(budget);
    tail.drain(..excess);
    tail.extend(bytes.iter().copied());
}

pub(crate) fn recorded_output_path(run_dir: &Path, stream: ProcessStream) -> PathBuf {
    run_dir.join(match stream {
        ProcessStream::Stdout => "stdout.bin",
        ProcessStream::Stderr => "stderr.bin",
        ProcessStream::Stdin => "stdin.bin",
    })
}

pub(crate) fn create_private_record_file(path: &Path) -> Result<std::fs::File, ProcessOutputError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path).map_err(|_| ProcessOutputError::Io)?;
    #[cfg(unix)]
    {
        file.sync_all().map_err(|_| ProcessOutputError::Io)?;
        let directory = std::fs::File::open(path.parent().ok_or(ProcessOutputError::Io)?)
            .map_err(|_| ProcessOutputError::Io)?;
        directory.sync_all().map_err(|_| ProcessOutputError::Io)?;
    }
    Ok(file)
}

#[derive(Clone)]
enum RecordTailUpdate {
    Pending,
    Revision(RecordTailRevision),
    End,
}

fn recorded_tail() -> (watch::Sender<RecordTailUpdate>, RecordedOutputTail) {
    let (updates, receiver) = watch::channel(RecordTailUpdate::Pending);
    (updates, RecordedOutputTail { updates: receiver, ended: false })
}

struct ByteChunk {
    sequence: u64,
    bytes: Box<[u8]>,
}

struct ByteChannelState {
    chunks: VecDeque<ByteChunk>,
    queued_bytes: usize,
    finished: bool,
    owner_available: bool,
    receiver_open: bool,
}

struct ByteChannel {
    byte_budget: usize,
    state: Mutex<ByteChannelState>,
    available: Notify,
    capacity: Notify,
}

impl ByteChannel {
    fn new(byte_budget: usize) -> Self {
        Self {
            byte_budget,
            state: Mutex::new(ByteChannelState {
                chunks: VecDeque::new(),
                queued_bytes: 0,
                finished: false,
                owner_available: true,
                receiver_open: true,
            }),
            available: Notify::new(),
            capacity: Notify::new(),
        }
    }
}

struct ByteSender {
    channel: Arc<ByteChannel>,
    finished: bool,
}

impl ByteSender {
    fn byte_budget(&self) -> usize {
        self.channel.byte_budget
    }

    async fn send(&self, chunk: ByteChunk) -> Result<(), ()> {
        let mut chunk = Some(chunk);
        loop {
            let notified = self.channel.capacity.notified();
            let sent = {
                let mut state = self.channel.state.lock().expect("byte channel lock poisoned");
                if !state.receiver_open {
                    return Err(());
                }
                let byte_count = chunk.as_ref().expect("chunk retained while waiting").bytes.len();
                if state.queued_bytes.saturating_add(byte_count) <= self.channel.byte_budget {
                    state.queued_bytes += byte_count;
                    state.chunks.push_back(chunk.take().expect("chunk sent once"));
                    true
                } else {
                    false
                }
            };
            if sent {
                self.channel.available.notify_one();
                return Ok(());
            }
            notified.await;
        }
    }

    fn finish(&mut self) -> Result<(), ()> {
        let mut state = self.channel.state.lock().expect("byte channel lock poisoned");
        let receiver_open = state.receiver_open;
        state.finished = true;
        self.finished = true;
        drop(state);
        self.channel.available.notify_waiters();
        receiver_open.then_some(()).ok_or(())
    }
}

impl Drop for ByteSender {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut state = self.channel.state.lock().expect("byte channel lock poisoned");
        state.owner_available = false;
        drop(state);
        self.channel.available.notify_waiters();
    }
}
