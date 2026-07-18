use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fmt,
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
    time::Duration,
};

use thiserror::Error;
use zeroize::Zeroizing;

/// Safe operator-facing text for one process run.
#[derive(Clone, Eq, PartialEq)]
pub struct ProcessLabel(String);

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ProcessLabelError {
    #[error("process label is empty")]
    Empty,
    #[error("process label contains a control character")]
    ContainsControlCharacter,
    #[error("process label is longer than 128 characters")]
    TooLong,
}

impl ProcessLabel {
    pub fn new(value: String) -> Result<Self, ProcessLabelError> {
        let mut count = 0usize;
        for character in value.chars() {
            if character.is_control() {
                return Err(ProcessLabelError::ContainsControlCharacter);
            }
            count += 1;
            if count > 128 {
                return Err(ProcessLabelError::TooLong);
            }
        }
        if count == 0 {
            return Err(ProcessLabelError::Empty);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProcessLabel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ProcessLabel").field(&self.0).finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvironmentBase {
    Inherit,
    Empty,
}

#[derive(Clone)]
pub struct ProcessEnvironment {
    pub(crate) base: EnvironmentBase,
    pub(crate) values: BTreeMap<OsString, OsString>,
    pub(crate) removals: BTreeSet<OsString>,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ProcessEnvironmentError {
    #[error("an environment key cannot be both set and removed")]
    ConflictingEntry,
    #[error("environment removals require an inherited environment base")]
    RemovalWithoutInheritedBase,
}

impl ProcessEnvironment {
    pub fn new(
        base: EnvironmentBase,
        values: BTreeMap<OsString, OsString>,
        removals: BTreeSet<OsString>,
    ) -> Result<Self, ProcessEnvironmentError> {
        if removals.iter().any(|key| values.contains_key(key)) {
            return Err(ProcessEnvironmentError::ConflictingEntry);
        }
        if base == EnvironmentBase::Empty && !removals.is_empty() {
            return Err(ProcessEnvironmentError::RemovalWithoutInheritedBase);
        }
        Ok(Self { base, values, removals })
    }
}

impl fmt::Debug for ProcessEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessEnvironment")
            .field("base", &self.base)
            .field("value_count", &self.values.len())
            .field("removal_count", &self.removals.len())
            .finish()
    }
}

#[derive(Clone)]
pub struct CommandSpec {
    pub(crate) program: OsString,
    pub(crate) arguments: Vec<OsString>,
    pub(crate) working_directory: PathBuf,
    pub(crate) environment: ProcessEnvironment,
    pub(crate) label: ProcessLabel,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CommandSpecError {
    #[error("process program is empty")]
    EmptyProgram,
    #[error("process working directory must be absolute")]
    WorkingDirectoryNotAbsolute,
}

impl CommandSpec {
    pub fn new(
        program: OsString,
        arguments: Vec<OsString>,
        working_directory: PathBuf,
        environment: ProcessEnvironment,
        label: ProcessLabel,
    ) -> Result<Self, CommandSpecError> {
        if program.is_empty() {
            return Err(CommandSpecError::EmptyProgram);
        }
        if !working_directory.is_absolute() {
            return Err(CommandSpecError::WorkingDirectoryNotAbsolute);
        }
        Ok(Self { program, arguments, working_directory, environment, label })
    }

    pub fn label(&self) -> &ProcessLabel {
        &self.label
    }
}

impl fmt::Debug for CommandSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandSpec")
            .field("working_directory", &self.working_directory)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// Owned sensitive bytes which are zeroed when released and never printed.
pub struct PrivateBytes(Zeroizing<Vec<u8>>);

impl PrivateBytes {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub(crate) fn into_bytes(self) -> Zeroizing<Vec<u8>> {
        self.0
    }
}

impl fmt::Debug for PrivateBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PrivateBytes([REDACTED])")
    }
}

#[derive(Debug)]
pub enum InputPolicy {
    Closed,
    Once(PrivateBytes),
    Writable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureOverflow {
    FailAndTerminate,
    TruncateWithEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapturePolicy {
    pub(crate) limit: NonZeroUsize,
    pub(crate) overflow: CaptureOverflow,
}

impl CapturePolicy {
    pub fn new(limit: NonZeroUsize, overflow: CaptureOverflow) -> Self {
        Self { limit, overflow }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamPolicy {
    pub(crate) in_flight_byte_budget: NonZeroUsize,
}

impl StreamPolicy {
    pub fn new(in_flight_byte_budget: NonZeroUsize) -> Self {
        Self { in_flight_byte_budget }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordLimit {
    Unlimited,
    Bytes(NonZeroU64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordOverflow {
    FailAndTerminate,
    DrainWithTruncationEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordPolicy {
    pub(crate) live_tail_byte_budget: NonZeroUsize,
    pub(crate) durable_limit: RecordLimit,
    pub(crate) overflow: RecordOverflow,
}

impl RecordPolicy {
    pub fn new(
        live_tail_byte_budget: NonZeroUsize,
        durable_limit: RecordLimit,
        overflow: RecordOverflow,
    ) -> Self {
        Self { live_tail_byte_budget, durable_limit, overflow }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputPolicy {
    Inherit,
    Discard,
    Capture(CapturePolicy),
    Stream(StreamPolicy),
    Record(RecordPolicy),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessDeadline {
    Unlimited,
    After(Duration),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminationPolicy {
    pub(crate) grace_period: Duration,
}

impl TerminationPolicy {
    pub fn new(grace_period: Duration) -> Self {
        Self { grace_period }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContainmentRequirement {
    CompleteTree,
    ExplicitProcessGroup,
}

#[derive(Debug)]
pub struct ProcessSpec {
    pub(crate) command: CommandSpec,
    pub(crate) input: InputPolicy,
    pub(crate) stdout: OutputPolicy,
    pub(crate) stderr: OutputPolicy,
    pub(crate) containment: ContainmentRequirement,
    pub(crate) deadline: ProcessDeadline,
    pub(crate) termination: TerminationPolicy,
}

impl ProcessSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        command: CommandSpec,
        input: InputPolicy,
        stdout: OutputPolicy,
        stderr: OutputPolicy,
        containment: ContainmentRequirement,
        deadline: ProcessDeadline,
        termination: TerminationPolicy,
    ) -> Self {
        Self { command, input, stdout, stderr, containment, deadline, termination }
    }
}

const MAX_DETACHED_RECORD_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DetachedRecordPolicyError {
    #[error("detached record limit exceeds one GiB")]
    LimitTooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DetachedRecordPolicy {
    pub(crate) limit: NonZeroU64,
}

impl DetachedRecordPolicy {
    pub const fn new(limit: NonZeroU64) -> Result<Self, DetachedRecordPolicyError> {
        if limit.get() > MAX_DETACHED_RECORD_BYTES {
            return Err(DetachedRecordPolicyError::LimitTooLarge);
        }
        Ok(Self { limit })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DetachedOutputPolicy {
    Discard,
    Record(DetachedRecordPolicy),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DetachedLifetimeRequirement {
    InvocationIndependent,
    LogoutIndependent,
}

#[derive(Clone, Debug)]
pub struct DetachedProcessSpec {
    pub(crate) command: CommandSpec,
    pub(crate) stdout: DetachedOutputPolicy,
    pub(crate) stderr: DetachedOutputPolicy,
    pub(crate) lifetime: DetachedLifetimeRequirement,
    pub(crate) termination: TerminationPolicy,
}

impl DetachedProcessSpec {
    pub fn new(
        command: CommandSpec,
        stdout: DetachedOutputPolicy,
        stderr: DetachedOutputPolicy,
        lifetime: DetachedLifetimeRequirement,
        termination: TerminationPolicy,
    ) -> Self {
        Self { command, stdout, stderr, lifetime, termination }
    }
}
