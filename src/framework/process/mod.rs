//! Backend-independent process supervision, evidence, and durable detached control.
//!
//! Callers describe commands and I/O policy. This module owns containment, raw-byte draining,
//! cancellation, reaping, and the evidence needed to prove a run has finished.

mod output;
mod receipt;
mod report;
mod session;
mod spec;
mod supervisor;

#[cfg(test)]
pub(crate) mod test_support;

mod detached;

#[cfg(target_os = "linux")]
mod detached_host;

mod platform;

pub use output::{
    CaptureDisposition, CaptureReport, PartialCaptureReport, PartialRecordReport,
    PartialStreamReport, ProcessByteEvent, ProcessByteStream, ProcessInputCompletion,
    ProcessInputError, ProcessInputHandle, ProcessInputWriter, ProcessOutputError,
    ProcessOutputHandle, RecordAvailability, RecordDisposition, RecordReport, RecordTailEvent,
    RecordTailRevision, RecordedOutputPath, RecordedOutputTail, StreamReport, UnavailableOutput,
};
pub use receipt::{
    DetachedCommitError, DetachedControlError, DetachedLaunchRecovery, DetachedLaunchTransaction,
    DetachedProcessReceipt, DetachedProcessStatus, DetachedProcessTerminal,
    DetachedReceiptDecodeError, DetachedRecoveryDecodeError, DetachedRollbackError,
    DetachedStartError, DetachedUnavailable, PendingDetachedLaunch, PendingDetachedLaunchPhase,
};
pub use report::{
    CompletionCause, DescendantDisposition, LeaderExit, LeaderExitObservation, OutputReport,
    PartialOutputReport, ProcessFailureKind, ProcessFailureReport, ProcessReport, ProcessRunId,
    ProcessStream, SignalNumber, TerminationDisposition,
};
pub use session::{
    ContainmentStrength, ControlAcknowledgement, ProcessControl, ProcessControlError,
    ProcessSession, StartedProcess,
};
pub use spec::{
    CaptureOverflow, CapturePolicy, CommandSpec, CommandSpecError, ContainmentRequirement,
    DetachedLifetimeRequirement, DetachedOutputPolicy, DetachedProcessSpec, DetachedRecordPolicy,
    DetachedRecordPolicyError, EnvironmentBase, InputPolicy, OutputPolicy, PrivateBytes,
    ProcessDeadline, ProcessEnvironment, ProcessEnvironmentError, ProcessLabel, ProcessLabelError,
    ProcessSpec, RecordLimit, RecordOverflow, RecordPolicy, StreamPolicy, TerminationPolicy,
};
pub use supervisor::{
    ContainmentAvailability, PreparedCompleteTreeReadinessError, PreparedCompleteTreeUnavailable,
    PreparedProcessRun, ProcessPrepareError, ProcessPrivateStorageAvailability, ProcessStartError,
    ProcessSupervisor, ProcessSupervisorBootstrapError, ProcessSupervisorCapabilities,
    ProcessWorkspace, ProcessWorkspaceError,
};

pub(in crate::framework) use report::leader_exit;
pub(in crate::framework) use supervisor::tokio_command;

#[cfg(target_os = "linux")]
#[doc(hidden)]
pub use detached_host::run_detached_io_host_entry;

#[cfg(not(target_os = "linux"))]
#[doc(hidden)]
pub async fn run_detached_io_host_entry() -> Option<i32> {
    None
}
