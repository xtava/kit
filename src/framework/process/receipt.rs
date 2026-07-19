use std::{fmt, fs::File, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::framework::{AtomicFileError, AtomicFileWriter};

use super::{ProcessFailureReport, ProcessReport, ProcessRunId, ProcessSupervisor};

const MAX_ENCODED_RECEIPT_BYTES: usize = 4096;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "version", rename_all = "kebab-case", deny_unknown_fields)]
enum DetachedReceiptEnvelope {
    V1 { run_id: ProcessRunId, authority: DetachedAuthorityReceipt },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "version", rename_all = "kebab-case", deny_unknown_fields)]
enum DetachedRecoveryEnvelope {
    V1 { run_id: ProcessRunId, unit_name: String, invocation_id: String },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "kebab-case", deny_unknown_fields)]
#[cfg(target_os = "linux")]
pub(crate) enum PersistedDetachedLaunchIntent {
    Prepared { run_id: ProcessRunId, host_executable: String, capability_path: String },
    Authority { recovery: String },
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "kebab-case", deny_unknown_fields)]
enum DetachedAuthorityReceipt {
    LinuxSystemd { unit_name: String, invocation_id: String },
}

#[derive(Clone, Eq, PartialEq)]
pub struct DetachedProcessReceipt(DetachedReceiptEnvelope);

#[must_use = "commit the detached receipt or explicitly roll the launch back"]
pub struct DetachedLaunchTransaction {
    supervisor: ProcessSupervisor,
    receipt: DetachedProcessReceipt,
    commit: DetachedCommitGrant,
    _launch_lock: File,
}

pub(crate) struct DetachedCommitGrant {
    run_dir: PathBuf,
    path: PathBuf,
    bytes: Zeroizing<Box<[u8]>>,
}

impl DetachedCommitGrant {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(run_dir: PathBuf, path: PathBuf, bytes: Box<[u8]>) -> Self {
        Self { run_dir, path, bytes: Zeroizing::new(bytes) }
    }

    fn publish(&self) -> Result<(), AtomicFileError> {
        AtomicFileWriter::new(&self.run_dir, "detached-launch.lock", ".detached-commit")
            .replace(&self.path, self.bytes.as_ref())
    }
}

impl fmt::Debug for DetachedLaunchTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DetachedLaunchTransaction")
            .field("run_id", &self.receipt.run_id())
            .finish_non_exhaustive()
    }
}

impl DetachedLaunchTransaction {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(
        supervisor: ProcessSupervisor,
        receipt: DetachedProcessReceipt,
        commit: DetachedCommitGrant,
        launch_lock: File,
    ) -> Self {
        Self { supervisor, receipt, commit, _launch_lock: launch_lock }
    }

    pub fn receipt(&self) -> &DetachedProcessReceipt {
        &self.receipt
    }

    pub fn commit(self) -> Result<DetachedProcessReceipt, DetachedCommitError> {
        if let Err(source) = self.commit.publish() {
            return Err(DetachedCommitError { transaction: Box::new(self), source });
        }
        Ok(self.receipt)
    }

    pub async fn rollback<E>(self, cause: E) -> Result<E, DetachedRollbackError<E>> {
        let receipt = self.receipt.clone();
        let supervisor = self.supervisor.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let transaction = self;
            let rollback = supervisor.abandon_detached_transaction(&transaction.receipt).await;
            let _ = sender.send(rollback);
        });
        let rollback = receiver.await.unwrap_or(Err(DetachedControlError::OwnerTaskFailed));
        match rollback {
            Ok(()) => Ok(cause),
            Err(rollback_error) => Err(DetachedRollbackError { cause, receipt, rollback_error }),
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) async fn abandon(self) {
        let supervisor = self.supervisor.clone();
        let _ = supervisor.abandon_detached_transaction(&self.receipt).await;
    }
}

pub struct DetachedCommitError {
    transaction: Box<DetachedLaunchTransaction>,
    source: AtomicFileError,
}

impl DetachedCommitError {
    pub fn transaction(&self) -> &DetachedLaunchTransaction {
        &self.transaction
    }

    pub fn into_transaction(self) -> DetachedLaunchTransaction {
        *self.transaction
    }
}

impl fmt::Debug for DetachedCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DetachedCommitError")
            .field("run_id", &self.transaction.receipt.run_id())
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for DetachedCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "publish detached launch commit for run {}",
            self.transaction.receipt.run_id()
        )
    }
}

impl std::error::Error for DetachedCommitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub struct DetachedRollbackError<E> {
    cause: E,
    receipt: DetachedProcessReceipt,
    rollback_error: DetachedControlError,
}

impl<E> DetachedRollbackError<E> {
    pub fn cause(&self) -> &E {
        &self.cause
    }

    pub fn receipt(&self) -> &DetachedProcessReceipt {
        &self.receipt
    }

    pub fn rollback_error(&self) -> &DetachedControlError {
        &self.rollback_error
    }

    pub fn into_parts(self) -> (E, DetachedProcessReceipt, DetachedControlError) {
        (self.cause, self.receipt, self.rollback_error)
    }
}

impl<E> fmt::Debug for DetachedRollbackError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DetachedRollbackError")
            .field("run_id", &self.receipt.run_id())
            .field("rollback_error", &self.rollback_error)
            .finish_non_exhaustive()
    }
}

impl<E: fmt::Display> fmt::Display for DetachedRollbackError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "detached launch failed: {}; rollback failed: {}; run {}; recovery receipt: {}",
            self.cause,
            self.rollback_error,
            self.receipt.run_id(),
            self.receipt.encode()
        )
    }
}

impl<E> std::error::Error for DetachedRollbackError<E>
where
    E: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct DetachedLaunchRecovery {
    run_id: ProcessRunId,
    unit_name: String,
    invocation_id: String,
}

impl fmt::Debug for DetachedLaunchRecovery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DetachedLaunchRecovery")
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

impl DetachedLaunchRecovery {
    pub fn run_id(&self) -> ProcessRunId {
        self.run_id
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(&DetachedRecoveryEnvelope::V1 {
            run_id: self.run_id,
            unit_name: self.unit_name.clone(),
            invocation_id: self.invocation_id.clone(),
        })
        .expect("detached recovery contains serializable values")
    }

    pub fn decode(value: &str) -> Result<Self, DetachedRecoveryDecodeError> {
        if value.len() > MAX_ENCODED_RECEIPT_BYTES {
            return Err(DetachedRecoveryDecodeError::Malformed);
        }
        let raw = serde_json::from_str::<Value>(value)
            .map_err(|_| DetachedRecoveryDecodeError::Malformed)?;
        match raw.get("version").and_then(Value::as_str) {
            Some("v1") => {}
            Some(_) => return Err(DetachedRecoveryDecodeError::UnsupportedVersion),
            None => return Err(DetachedRecoveryDecodeError::Malformed),
        }
        let DetachedRecoveryEnvelope::V1 { run_id, unit_name, invocation_id } =
            serde_json::from_str(value).map_err(|_| DetachedRecoveryDecodeError::Malformed)?;
        if unit_name != systemd_unit_name(run_id) || !valid_invocation_id(&invocation_id) {
            return Err(DetachedRecoveryDecodeError::IdentityBindingMismatch);
        }
        Ok(Self { run_id, unit_name, invocation_id })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn linux_systemd(
        run_id: ProcessRunId,
        unit_name: String,
        invocation_id: String,
    ) -> Result<Self, DetachedRecoveryDecodeError> {
        if unit_name != systemd_unit_name(run_id) || !valid_invocation_id(&invocation_id) {
            return Err(DetachedRecoveryDecodeError::IdentityBindingMismatch);
        }
        Ok(Self { run_id, unit_name, invocation_id })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn systemd_authority(&self) -> (&str, &str) {
        (&self.unit_name, &self.invocation_id)
    }
}

impl fmt::Debug for DetachedProcessReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DetachedProcessReceipt")
            .field("run_id", &self.run_id())
            .finish_non_exhaustive()
    }
}

impl DetachedProcessReceipt {
    pub fn run_id(&self) -> ProcessRunId {
        match &self.0 {
            DetachedReceiptEnvelope::V1 { run_id, .. } => *run_id,
        }
    }

    pub fn encode(&self) -> String {
        serde_json::to_string(&self.0).expect("detached receipt contains serializable values")
    }

    pub fn decode(value: &str) -> Result<Self, DetachedReceiptDecodeError> {
        if value.len() > MAX_ENCODED_RECEIPT_BYTES {
            return Err(DetachedReceiptDecodeError::Malformed);
        }
        let raw = serde_json::from_str::<Value>(value)
            .map_err(|_| DetachedReceiptDecodeError::Malformed)?;
        match raw.get("version").and_then(Value::as_str) {
            Some("v1") => {}
            Some(_) => return Err(DetachedReceiptDecodeError::UnsupportedVersion),
            None => return Err(DetachedReceiptDecodeError::Malformed),
        }
        let envelope = serde_json::from_str::<DetachedReceiptEnvelope>(value)
            .map_err(|_| DetachedReceiptDecodeError::Malformed)?;
        let receipt = Self(envelope);
        receipt.validate_binding()?;
        Ok(receipt)
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn linux_systemd(
        run_id: ProcessRunId,
        invocation_id: String,
    ) -> Result<Self, DetachedReceiptDecodeError> {
        if !valid_invocation_id(&invocation_id) {
            return Err(DetachedReceiptDecodeError::Malformed);
        }
        Ok(Self(DetachedReceiptEnvelope::V1 {
            run_id,
            authority: DetachedAuthorityReceipt::LinuxSystemd {
                unit_name: systemd_unit_name(run_id),
                invocation_id,
            },
        }))
    }

    pub(crate) fn systemd_authority(&self) -> (&str, &str) {
        match &self.0 {
            DetachedReceiptEnvelope::V1 {
                authority: DetachedAuthorityReceipt::LinuxSystemd { unit_name, invocation_id },
                ..
            } => (unit_name, invocation_id),
        }
    }

    fn validate_binding(&self) -> Result<(), DetachedReceiptDecodeError> {
        let (unit_name, invocation_id) = self.systemd_authority();
        if unit_name != systemd_unit_name(self.run_id()) || !valid_invocation_id(invocation_id) {
            return Err(DetachedReceiptDecodeError::IdentityBindingMismatch);
        }
        Ok(())
    }
}

pub(crate) fn systemd_unit_name(run_id: ProcessRunId) -> String {
    format!("kit-run-{}.service", run_id.as_uuid().simple())
}

fn valid_invocation_id(value: &str) -> bool {
    value.len() == 32
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DetachedProcessStatus {
    Running,
    Stopping,
    Completed(ProcessReport),
    Failed(ProcessFailureReport),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DetachedProcessTerminal {
    Completed(ProcessReport),
    Failed(ProcessFailureReport),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingDetachedLaunchPhase {
    Prepared,
    AuthorityBound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PendingDetachedLaunch {
    run_id: ProcessRunId,
    phase: PendingDetachedLaunchPhase,
}

impl PendingDetachedLaunch {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(run_id: ProcessRunId, phase: PendingDetachedLaunchPhase) -> Self {
        Self { run_id, phase }
    }

    pub fn run_id(&self) -> ProcessRunId {
        self.run_id
    }

    pub fn phase(&self) -> PendingDetachedLaunchPhase {
        self.phase
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DetachedUnavailable {
    #[error("detached processes are unsupported on this platform")]
    UnsupportedPlatform,
    #[error("the systemd user manager is unavailable")]
    UserManagerUnavailable,
    #[error("the systemd user session bus is unavailable")]
    SessionBusUnavailable,
    #[error("a persistent systemd user manager is unavailable")]
    PersistentUserManagerUnavailable,
    #[error("durable detached-process storage is unavailable")]
    DurableStorageUnavailable,
}

#[derive(Debug, Error)]
pub enum DetachedStartError {
    #[error(transparent)]
    Unavailable(#[from] DetachedUnavailable),
    #[error("detached process working directory is unavailable")]
    WorkingDirectoryUnavailable,
    #[error("submit detached start request")]
    StartRequestFailed,
    #[error("detached start job failed")]
    StartJobFailed,
    #[error("validate detached runtime authority")]
    AuthorityValidationFailed,
    #[error("persist detached process receipt")]
    ReceiptPersistFailed,
    #[error(
        "detached launch cleanup is unconfirmed; run `kit process recover-detached --token {}`",
        recovery.encode()
    )]
    RecoveryRequired { recovery: DetachedLaunchRecovery },
    #[error("detached launch created runtime authority whose identity could not be recovered; run `kit process recover-detached --pending`")]
    UnrecoverableAuthority,
    #[error("detached launch owner task failed; run `kit process recover-detached --pending`")]
    OwnerTaskFailed,
}

impl DetachedStartError {
    pub fn recovery(&self) -> Option<&DetachedLaunchRecovery> {
        match self {
            Self::RecoveryRequired { recovery } => Some(recovery),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DetachedReceiptDecodeError {
    #[error("detached receipt version is unsupported")]
    UnsupportedVersion,
    #[error("detached receipt is malformed")]
    Malformed,
    #[error("detached receipt identity binding does not match")]
    IdentityBindingMismatch,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DetachedRecoveryDecodeError {
    #[error("detached recovery version is unsupported")]
    UnsupportedVersion,
    #[error("detached recovery token is malformed")]
    Malformed,
    #[error("detached recovery identity binding does not match")]
    IdentityBindingMismatch,
}

#[derive(Debug, Error)]
pub enum DetachedControlError {
    #[error(transparent)]
    Unavailable(#[from] DetachedUnavailable),
    #[error("another controller is collecting this detached process")]
    ControlBusy,
    #[error("detached runtime authority does not match its receipt")]
    AuthorityMismatch,
    #[error("detached runtime authority and terminal evidence are both unavailable")]
    AuthorityLost,
    #[error("detached process has not reached a persisted terminal state")]
    NotCompleted,
    #[error("detached lifecycle owner task failed")]
    OwnerTaskFailed,
    #[error("submit detached control request")]
    ControlRequestFailed,
    #[error("detached control job failed")]
    ControlJobFailed,
    #[error("detached terminal evidence is unavailable")]
    TerminalEvidenceUnavailable,
    #[error("persist detached terminal evidence")]
    TerminalEvidencePersistFailed,
}
