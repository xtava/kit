use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::output::{
    CaptureReport, PartialCaptureReport, PartialRecordReport, PartialStreamReport,
    RecordAvailability, RecordDisposition, RecordReport, StreamReport, UnavailableOutput,
};
use super::session::ContainmentStrength;

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProcessRunId(Uuid);

impl ProcessRunId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Debug for ProcessRunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ProcessRunId({})", self.0.hyphenated())
    }
}

impl fmt::Display for ProcessRunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0.hyphenated())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignalNumber(i32);

impl SignalNumber {
    pub fn get(self) -> i32 {
        self.0
    }

    pub(crate) fn new(value: i32) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompletionCause {
    Natural,
    Cancelled,
    DeadlineExceeded,
    OwnerDropped,
    ExternalTermination,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LeaderExit {
    Code(i32),
    Signal(SignalNumber),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DescendantDisposition {
    EmptyWhenObserved,
    DrainedNaturallyAfterLeaderExit,
    TerminatedAfterLeaderExit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminationDisposition {
    NotRequested,
    Graceful,
    Forced,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputReport {
    Inherited,
    Discarded,
    Captured(CaptureReport),
    Streamed(StreamReport),
    Recorded(RecordReport),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PartialOutputReport {
    Inherited,
    Discarded,
    Captured(PartialCaptureReport),
    Streamed(PartialStreamReport),
    Recorded(PartialRecordReport),
    Unavailable(UnavailableOutput),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessReport {
    pub run_id: ProcessRunId,
    pub completion: CompletionCause,
    pub leader_exit: LeaderExitObservation,
    pub containment: ContainmentStrength,
    pub descendants: DescendantDisposition,
    pub termination: TerminationDisposition,
    pub stdout: OutputReport,
    pub stderr: OutputReport,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessStream {
    Stdin,
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessFailureKind {
    InputIo,
    OutputIo { stream: ProcessStream },
    OutputLimitExceeded { stream: ProcessStream },
    RequiredConsumerLost { stream: ProcessStream },
    ContainmentLost,
    TerminationUnconfirmed,
    OwnerTaskFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LeaderExitObservation {
    Observed(LeaderExit),
    NotObserved,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessFailureReport {
    pub run_id: ProcessRunId,
    pub failure: ProcessFailureKind,
    pub leader_exit: LeaderExitObservation,
    pub termination: TerminationDisposition,
    pub stdout: PartialOutputReport,
    pub stderr: PartialOutputReport,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum PersistedDetachedOutput {
    Discarded,
    Recorded {
        observed_bytes: u64,
        retained_bytes: u64,
        disposition: RecordDisposition,
        availability: RecordAvailability,
        final_tail: Vec<u8>,
    },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "report", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum PersistedDetachedReport {
    Completed(PersistedDetachedCompletedReport),
    InfrastructureFailure(PersistedDetachedFailureReport),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedDetachedCompletedReport {
    pub run_id: ProcessRunId,
    pub unit_name: String,
    pub invocation_id: String,
    pub completion: CompletionCause,
    pub leader_exit: LeaderExitObservation,
    pub termination: TerminationDisposition,
    pub stdout: PersistedDetachedOutput,
    pub stderr: PersistedDetachedOutput,
    pub elapsed_micros: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedDetachedFailureReport {
    pub run_id: ProcessRunId,
    pub unit_name: String,
    pub invocation_id: String,
    pub leader_exit: LeaderExitObservation,
    pub termination: TerminationDisposition,
    pub stdout: PersistedDetachedOutput,
    pub stderr: PersistedDetachedOutput,
    pub elapsed_micros: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "evidence", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum PersistedDetachedTerminal {
    Completed(PersistedDetachedTarget),
    InfrastructureFailure(PersistedDetachedInfrastructureFailure),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedDetachedTarget {
    pub run_id: ProcessRunId,
    pub leader_exit: LeaderExitObservation,
    pub stdout: PersistedDetachedOutput,
    pub stderr: PersistedDetachedOutput,
    pub elapsed_micros: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedDetachedInfrastructureFailure {
    pub run_id: ProcessRunId,
    pub leader_exit: LeaderExitObservation,
    pub stdout: PersistedDetachedOutput,
    pub stderr: PersistedDetachedOutput,
    pub elapsed_micros: u64,
}
