#[cfg(target_os = "linux")]
mod linux {
    use std::{
        io::Read,
        path::{Path, PathBuf},
        time::Duration,
    };

    use serde::{Deserialize, Serialize};

    use crate::framework::{AtomicFileTryLock, AtomicFileWriter};

    use super::super::{
        detached_host::{
            prepare_record_files, DetachedHostCapability, StoredOutputPolicy, COMMIT_FILE,
            HOST_FAILURE_EXIT, STDERR_FILE, STDOUT_FILE, TERMINAL_FILE,
        },
        output::{
            partial_from_output_report, RecordAvailability, RecordDisposition, RecordReport,
            RecordedOutputPath,
        },
        platform::linux_systemd::{
            AuthorityResolution, SystemdBackend, SystemdControlError, SystemdLaunchError,
            SystemdRuntimeStatus, SystemdTerminalEvidence,
        },
        receipt::{
            DetachedControlError, DetachedLaunchRecovery, DetachedLaunchTransaction,
            DetachedProcessReceipt, DetachedProcessStatus, DetachedProcessTerminal,
            DetachedStartError, DetachedUnavailable, PendingDetachedLaunch,
            PendingDetachedLaunchPhase, PersistedDetachedLaunchIntent,
        },
        report::{
            CompletionCause, DescendantDisposition, LeaderExit, LeaderExitObservation,
            OutputReport, PartialOutputReport, PersistedDetachedCompletedReport,
            PersistedDetachedFailureReport, PersistedDetachedInfrastructureFailure,
            PersistedDetachedOutput, PersistedDetachedReport, PersistedDetachedTerminal,
            ProcessFailureKind, ProcessFailureReport, ProcessReport, ProcessRunId,
            TerminationDisposition,
        },
        session::ContainmentStrength,
        spec::{DetachedLifetimeRequirement, DetachedProcessSpec},
        supervisor::RunDirectory,
        ProcessSupervisor,
    };

    const MANIFEST_FILE: &str = "detached.json";
    const LAUNCH_INTENT_FILE: &str = "detached-launch.json";
    const REPORT_FILE: &str = "detached-report.json";
    const RELEASE_FILE: &str = "detached-release.json";
    const MAX_PROCESS_DURATION: Duration = Duration::from_secs(10 * 365 * 24 * 60 * 60);
    const MAX_FINAL_TAIL_BYTES: usize = 64 * 1024;
    const MAX_METADATA_BYTES: u64 = 1024 * 1024;

    #[derive(Serialize, Deserialize)]
    struct StoredDetachedRun {
        receipt: String,
        stdout: StoredOutputPolicy,
        stderr: StoredOutputPolicy,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "version", rename_all = "kebab-case", deny_unknown_fields)]
    enum PersistedDetachedRelease {
        V1 { run_id: ProcessRunId, unit_name: String, invocation_id: String },
    }

    impl ProcessSupervisor {
        pub async fn launch_detached(
            &self,
            spec: DetachedProcessSpec,
        ) -> Result<DetachedLaunchTransaction, DetachedStartError> {
            if !spec.command.working_directory.is_dir() {
                return Err(DetachedStartError::WorkingDirectoryUnavailable);
            }
            validate_timing(&spec)?;
            let prepared = self.prepare_detached().map_err(|_| {
                DetachedStartError::Unavailable(DetachedUnavailable::DurableStorageUnavailable)
            })?;
            let run_id = prepared.run_id();
            let mut run_directory = prepared.into_run_directory();
            let run_dir = run_directory.path().to_path_buf();
            prepare_record_files(&run_dir, spec.stdout, spec.stderr).map_err(|_| {
                DetachedStartError::Unavailable(DetachedUnavailable::DurableStorageUnavailable)
            })?;
            let host = DetachedHostCapability::prepare(run_id, &run_dir, &spec)
                .map_err(|_| DetachedStartError::StartRequestFailed)?;
            let launch_writer =
                AtomicFileWriter::new(&run_dir, "detached-launch.lock", ".detached-launch");
            let launch_lock = launch_writer.lock().map_err(|_| {
                DetachedStartError::Unavailable(DetachedUnavailable::DurableStorageUnavailable)
            })?;
            publish_prepared_launch_intent(&run_dir, run_id, &host)?;
            run_directory.retain();
            let supervisor = self.clone();
            let (sender, receiver) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let result = supervisor
                    .launch_detached_owned(spec, run_id, run_directory, host, launch_lock)
                    .await;
                if let Err(abandoned) = sender.send(result) {
                    match abandoned {
                        Ok(transaction) => transaction.abandon().await,
                        Err(DetachedStartError::RecoveryRequired { recovery }) => {
                            let _ = supervisor.recover_detached_launch(&recovery).await;
                        }
                        Err(_) => {}
                    }
                }
            });
            receiver.await.unwrap_or(Err(DetachedStartError::OwnerTaskFailed))
        }

        async fn launch_detached_owned(
            &self,
            spec: DetachedProcessSpec,
            run_id: ProcessRunId,
            run_directory: RunDirectory,
            host: DetachedHostCapability,
            launch_lock: std::fs::File,
        ) -> Result<DetachedLaunchTransaction, DetachedStartError> {
            let run_dir = run_directory.path().to_path_buf();
            let backend = match SystemdBackend::connect(spec.lifetime).await {
                Ok(backend) => backend,
                Err(error) => {
                    discard_run_directory(&run_dir);
                    return Err(error.into());
                }
            };
            let authority = match backend.launch(run_id, &spec, &host).await {
                Ok(authority) => authority,
                Err(error) => {
                    let mapped = map_start_error(run_id, error);
                    if let DetachedStartError::RecoveryRequired { recovery } = &mapped {
                        let _ = publish_launch_intent(&run_dir, recovery);
                    } else if !matches!(mapped, DetachedStartError::UnrecoverableAuthority) {
                        discard_run_directory(&run_dir);
                    }
                    return Err(mapped);
                }
            };
            let recovery = match DetachedLaunchRecovery::linux_systemd(
                run_id,
                authority.unit_name.clone(),
                authority.invocation_id.clone(),
            ) {
                Ok(recovery) => recovery,
                Err(_) => {
                    if backend.stop_and_release(&authority).await.is_err() {
                        return Err(DetachedStartError::UnrecoverableAuthority);
                    }
                    discard_run_directory(&run_dir);
                    return Err(DetachedStartError::AuthorityValidationFailed);
                }
            };
            if publish_launch_intent(&run_dir, &recovery).is_err() {
                if backend.stop_and_release(&authority).await.is_err() {
                    return Err(DetachedStartError::RecoveryRequired { recovery });
                }
                discard_run_directory(&run_dir);
                return Err(DetachedStartError::ReceiptPersistFailed);
            }
            let receipt = match DetachedProcessReceipt::linux_systemd(
                run_id,
                authority.invocation_id.clone(),
            ) {
                Ok(receipt) => receipt,
                Err(_) => {
                    if backend.stop_and_release(&authority).await.is_err() {
                        return Err(DetachedStartError::RecoveryRequired { recovery });
                    }
                    discard_run_directory(&run_dir);
                    return Err(DetachedStartError::AuthorityValidationFailed);
                }
            };
            if authority.unit_name != receipt.systemd_authority().0 {
                if backend.stop_and_release(&authority).await.is_err() {
                    return Err(DetachedStartError::RecoveryRequired { recovery });
                }
                discard_run_directory(&run_dir);
                return Err(DetachedStartError::AuthorityValidationFailed);
            }
            let stored = StoredDetachedRun {
                receipt: receipt.encode(),
                stdout: StoredOutputPolicy::from_public(spec.stdout),
                stderr: StoredOutputPolicy::from_public(spec.stderr),
            };
            if publish_json(&run_dir, MANIFEST_FILE, &stored).is_err() {
                if backend.stop_and_release(&authority).await.is_err() {
                    return Err(DetachedStartError::RecoveryRequired { recovery });
                }
                discard_run_directory(&run_dir);
                return Err(DetachedStartError::ReceiptPersistFailed);
            }
            let commit = host.into_commit_grant();
            Ok(DetachedLaunchTransaction::new(self.clone(), receipt, commit, launch_lock))
        }

        pub async fn inspect_detached(
            &self,
            receipt: &DetachedProcessReceipt,
        ) -> Result<DetachedProcessStatus, DetachedControlError> {
            let context = self.control_context(receipt)?;
            if let Some(report) = context.read_report()? {
                self.release_persisted_detached_report(receipt, &context, &report).await?;
                return Ok(report.public_status());
            }
            let backend =
                SystemdBackend::connect(DetachedLifetimeRequirement::InvocationIndependent).await?;
            let (unit_name, invocation_id) = receipt.systemd_authority();
            let pinned =
                match backend.resolve(unit_name, invocation_id).await.map_err(map_control_error)? {
                    AuthorityResolution::Missing => Err(DetachedControlError::AuthorityLost),
                    AuthorityResolution::Present(pinned) => Ok(pinned),
                }?;
            match pinned.status() {
                SystemdRuntimeStatus::Running => {
                    backend.unpin(pinned).await.map_err(map_control_error)?;
                    Ok(DetachedProcessStatus::Running)
                }
                SystemdRuntimeStatus::Stopping => {
                    backend.unpin(pinned).await.map_err(map_control_error)?;
                    Ok(DetachedProcessStatus::Stopping)
                }
                SystemdRuntimeStatus::Terminal(evidence) => {
                    let report = match context.terminal_report(
                        evidence,
                        observed_completion(evidence),
                        TerminationDisposition::NotRequested,
                    ) {
                        Ok(report) => report,
                        Err(error) => {
                            let _ = backend.unpin(pinned).await;
                            return Err(error);
                        }
                    };
                    if let Err(error) = context.publish_report(unit_name, invocation_id, &report) {
                        if report.is_unpublished_host_failure() {
                            let released = backend.release(pinned).await.map_err(map_control_error);
                            if released.is_ok() {
                                let _ = context.remove();
                            }
                        } else {
                            let _ = backend.unpin(pinned).await;
                        }
                        return Err(error);
                    }
                    backend.release(pinned).await.map_err(map_control_error)?;
                    context.publish_release_complete(receipt)?;
                    Ok(report.public_status())
                }
            }
        }

        pub async fn stop_detached(
            &self,
            receipt: &DetachedProcessReceipt,
        ) -> Result<DetachedProcessTerminal, DetachedControlError> {
            let context = self.control_context(receipt)?;
            if let Some(report) = context.read_report()? {
                self.release_persisted_detached_report(receipt, &context, &report).await?;
                return Ok(report.into_public());
            }
            let backend =
                SystemdBackend::connect(DetachedLifetimeRequirement::InvocationIndependent).await?;
            let (unit_name, invocation_id) = receipt.systemd_authority();
            let mut pinned =
                match backend.resolve(unit_name, invocation_id).await.map_err(map_control_error)? {
                    AuthorityResolution::Missing => {
                        return Err(DetachedControlError::AuthorityLost)
                    }
                    AuthorityResolution::Present(pinned) => pinned,
                };
            let (evidence, completion, termination) = match pinned.status() {
                SystemdRuntimeStatus::Terminal(evidence) => {
                    (evidence, observed_completion(evidence), TerminationDisposition::NotRequested)
                }
                SystemdRuntimeStatus::Running | SystemdRuntimeStatus::Stopping => {
                    let stopped = match backend.stop(&mut pinned).await {
                        Ok(stopped) => stopped,
                        Err(error) => {
                            let _ = backend.unpin(pinned).await;
                            return Err(map_control_error(error));
                        }
                    };
                    let completion = if stopped.stop_accepted {
                        CompletionCause::Cancelled
                    } else {
                        observed_completion(stopped.evidence)
                    };
                    let termination = if stopped.stop_accepted {
                        if stopped.evidence.forced {
                            TerminationDisposition::Forced
                        } else {
                            TerminationDisposition::Graceful
                        }
                    } else {
                        TerminationDisposition::NotRequested
                    };
                    (stopped.evidence, completion, termination)
                }
            };
            let report = match context.terminal_report(evidence, completion, termination) {
                Ok(report) => report,
                Err(error) => {
                    let _ = backend.unpin(pinned).await;
                    return Err(error);
                }
            };
            if let Err(error) = context.publish_report(unit_name, invocation_id, &report) {
                if report.is_unpublished_host_failure() {
                    let released = backend.release(pinned).await.map_err(map_control_error);
                    if released.is_ok() {
                        let _ = context.remove();
                    }
                } else {
                    let _ = backend.unpin(pinned).await;
                }
                return Err(error);
            }
            backend.release(pinned).await.map_err(map_control_error)?;
            context.publish_release_complete(receipt)?;
            Ok(report.into_public())
        }

        async fn release_persisted_detached_report(
            &self,
            receipt: &DetachedProcessReceipt,
            context: &DetachedControlContext,
            report: &DetachedTerminalReport,
        ) -> Result<(), DetachedControlError> {
            if context.release_complete(receipt)? {
                return Ok(());
            }
            let backend =
                SystemdBackend::connect(DetachedLifetimeRequirement::InvocationIndependent).await?;
            let (unit_name, invocation_id) = receipt.systemd_authority();
            let pinned =
                match backend.resolve(unit_name, invocation_id).await.map_err(map_control_error)? {
                    AuthorityResolution::Missing => {
                        context.publish_release_complete(receipt)?;
                        return Ok(());
                    }
                    AuthorityResolution::Present(pinned) => pinned,
                };
            let SystemdRuntimeStatus::Terminal(evidence) = pinned.status() else {
                let _ = backend.unpin(pinned).await;
                return Err(DetachedControlError::TerminalEvidenceUnavailable);
            };
            if let Err(error) = context.reconcile_persisted_terminal(report, evidence) {
                let _ = backend.unpin(pinned).await;
                return Err(error);
            }
            backend.release(pinned).await.map_err(map_control_error)?;
            context.publish_release_complete(receipt)
        }

        pub(crate) async fn abandon_detached_transaction(
            &self,
            receipt: &DetachedProcessReceipt,
        ) -> Result<(), DetachedControlError> {
            let context = self.control_context(receipt)?;
            if context.commit_exists()? {
                return Err(DetachedControlError::AuthorityMismatch);
            }
            let recovery = context.read_launch_recovery()?;
            let (unit_name, invocation_id) = receipt.systemd_authority();
            if recovery.run_id() != receipt.run_id()
                || recovery.systemd_authority() != (unit_name, invocation_id)
            {
                return Err(DetachedControlError::AuthorityMismatch);
            }
            let backend =
                SystemdBackend::connect(DetachedLifetimeRequirement::InvocationIndependent).await?;
            backend.recover_launch(unit_name, invocation_id).await.map_err(map_control_error)?;
            context.remove()
        }

        pub async fn forget_detached(
            &self,
            receipt: &DetachedProcessReceipt,
        ) -> Result<(), DetachedControlError> {
            let run_dir = self.state_root().join(receipt.run_id().to_string());
            match std::fs::symlink_metadata(&run_dir) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let backend =
                        SystemdBackend::connect(DetachedLifetimeRequirement::InvocationIndependent)
                            .await?;
                    let (unit_name, invocation_id) = receipt.systemd_authority();
                    return match backend
                        .resolve(unit_name, invocation_id)
                        .await
                        .map_err(map_control_error)?
                    {
                        AuthorityResolution::Missing => Ok(()),
                        AuthorityResolution::Present(pinned) => {
                            backend.unpin(pinned).await.map_err(map_control_error)?;
                            Err(DetachedControlError::AuthorityLost)
                        }
                    };
                }
                Err(_) => {
                    return Err(DetachedControlError::Unavailable(
                        DetachedUnavailable::DurableStorageUnavailable,
                    ));
                }
            }
            let context = self.control_context(receipt)?;
            let report = context.read_report()?.ok_or(DetachedControlError::NotCompleted)?;
            let backend =
                SystemdBackend::connect(DetachedLifetimeRequirement::InvocationIndependent).await?;
            let (unit_name, invocation_id) = receipt.systemd_authority();
            match backend.resolve(unit_name, invocation_id).await.map_err(map_control_error)? {
                AuthorityResolution::Missing => {}
                AuthorityResolution::Present(pinned) => match pinned.status() {
                    SystemdRuntimeStatus::Running | SystemdRuntimeStatus::Stopping => {
                        backend.unpin(pinned).await.map_err(map_control_error)?;
                        return Err(DetachedControlError::NotCompleted);
                    }
                    SystemdRuntimeStatus::Terminal(evidence) => {
                        if let Err(error) = context.reconcile_persisted_terminal(&report, evidence)
                        {
                            let _ = backend.unpin(pinned).await;
                            return Err(error);
                        }
                        backend.release(pinned).await.map_err(map_control_error)?;
                    }
                },
            }
            context.remove()
        }

        pub async fn recover_detached_launch(
            &self,
            recovery: &DetachedLaunchRecovery,
        ) -> Result<(), DetachedControlError> {
            let run_dir = self.state_root().join(recovery.run_id().to_string());
            validate_run_directory(&run_dir)?;
            let _lock = acquire_launch_lock(&run_dir)?;
            let pending =
                read_pending_launch(&run_dir)?.ok_or(DetachedControlError::AuthorityMismatch)?;
            recover_validated_pending(&run_dir, pending, Some(recovery)).await
        }

        pub fn list_pending_detached_launches(
            &self,
        ) -> Result<Vec<PendingDetachedLaunch>, DetachedControlError> {
            let mut pending = Vec::new();
            for run_dir in detached_run_directories(self.state_root())? {
                let _lock = match try_acquire_launch_lock(&run_dir)? {
                    Some(lock) => lock,
                    None => continue,
                };
                let Some(intent) = read_pending_launch(&run_dir)? else {
                    continue;
                };
                pending.push(intent.public());
            }
            pending.sort_by_key(PendingDetachedLaunch::run_id);
            Ok(pending)
        }

        pub async fn recover_pending_detached_launches(
            &self,
        ) -> Result<Vec<ProcessRunId>, DetachedControlError> {
            let mut recovered = Vec::new();
            for run_dir in detached_run_directories(self.state_root())? {
                let _lock = match try_acquire_launch_lock(&run_dir)? {
                    Some(lock) => lock,
                    None => continue,
                };
                let Some(intent) = read_pending_launch(&run_dir)? else {
                    continue;
                };
                let run_id = intent.run_id();
                recover_validated_pending(&run_dir, intent, None).await?;
                recovered.push(run_id);
            }
            recovered.sort();
            Ok(recovered)
        }

        fn control_context(
            &self,
            receipt: &DetachedProcessReceipt,
        ) -> Result<DetachedControlContext, DetachedControlError> {
            let run_dir = self.state_root().join(receipt.run_id().to_string());
            validate_run_directory(&run_dir)?;
            let writer = AtomicFileWriter::new(&run_dir, "detached.lock", ".detached");
            let lock = match writer.try_lock().map_err(|_| {
                DetachedControlError::Unavailable(DetachedUnavailable::DurableStorageUnavailable)
            })? {
                AtomicFileTryLock::Acquired(lock) => lock,
                AtomicFileTryLock::Busy => return Err(DetachedControlError::ControlBusy),
            };
            let manifest = read_json::<StoredDetachedRun>(&run_dir.join(MANIFEST_FILE))
                .map_err(|_| DetachedControlError::TerminalEvidenceUnavailable)?;
            if manifest.receipt != receipt.encode() {
                return Err(DetachedControlError::AuthorityMismatch);
            }
            Ok(DetachedControlContext {
                run_id: receipt.run_id(),
                run_dir,
                writer,
                manifest,
                _lock: lock,
            })
        }
    }

    struct DetachedControlContext {
        run_id: ProcessRunId,
        run_dir: PathBuf,
        writer: AtomicFileWriter,
        manifest: StoredDetachedRun,
        _lock: std::fs::File,
    }

    enum DetachedTerminalReport {
        Completed(ProcessReport),
        InfrastructureFailure { report: ProcessFailureReport, elapsed: Duration },
    }

    impl DetachedTerminalReport {
        fn public_status(&self) -> DetachedProcessStatus {
            match self {
                Self::Completed(report) => DetachedProcessStatus::Completed(report.clone()),
                Self::InfrastructureFailure { report, .. } => {
                    DetachedProcessStatus::Failed(report.clone())
                }
            }
        }

        fn into_public(self) -> DetachedProcessTerminal {
            match self {
                Self::Completed(report) => DetachedProcessTerminal::Completed(report),
                Self::InfrastructureFailure { report, .. } => {
                    DetachedProcessTerminal::Failed(report)
                }
            }
        }

        fn is_unpublished_host_failure(&self) -> bool {
            matches!(self, Self::InfrastructureFailure { .. })
        }
    }

    impl DetachedControlContext {
        fn commit_exists(&self) -> Result<bool, DetachedControlError> {
            match std::fs::symlink_metadata(self.run_dir.join(COMMIT_FILE)) {
                Ok(metadata)
                    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
                {
                    Ok(true)
                }
                Ok(_) => Err(DetachedControlError::AuthorityMismatch),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(_) => Err(DetachedControlError::Unavailable(
                    DetachedUnavailable::DurableStorageUnavailable,
                )),
            }
        }

        fn read_launch_recovery(&self) -> Result<DetachedLaunchRecovery, DetachedControlError> {
            let intent =
                read_json::<PersistedDetachedLaunchIntent>(&self.run_dir.join(LAUNCH_INTENT_FILE))
                    .map_err(|_| DetachedControlError::AuthorityMismatch)?;
            let PersistedDetachedLaunchIntent::Authority { recovery } = intent else {
                return Err(DetachedControlError::AuthorityMismatch);
            };
            DetachedLaunchRecovery::decode(&recovery)
                .map_err(|_| DetachedControlError::AuthorityMismatch)
        }

        fn read_report(&self) -> Result<Option<DetachedTerminalReport>, DetachedControlError> {
            let path = self.run_dir.join(REPORT_FILE);
            let bytes = match read_bounded_file(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err(DetachedControlError::TerminalEvidenceUnavailable),
            };
            let persisted = serde_json::from_slice::<PersistedDetachedReport>(&bytes)
                .map_err(|_| DetachedControlError::TerminalEvidenceUnavailable)?;
            let receipt = DetachedProcessReceipt::decode(&self.manifest.receipt)
                .map_err(|_| DetachedControlError::TerminalEvidenceUnavailable)?;
            let (unit_name, invocation_id) = receipt.systemd_authority();
            let report = match persisted {
                PersistedDetachedReport::Completed(report) => {
                    validate_persisted_identity(
                        self.run_id,
                        unit_name,
                        invocation_id,
                        report.run_id,
                        &report.unit_name,
                        &report.invocation_id,
                    )?;
                    self.restore_completed_report(report)?
                }
                PersistedDetachedReport::InfrastructureFailure(report) => {
                    validate_persisted_identity(
                        self.run_id,
                        unit_name,
                        invocation_id,
                        report.run_id,
                        &report.unit_name,
                        &report.invocation_id,
                    )?;
                    self.restore_failure_report(report)?
                }
            };
            Ok(Some(report))
        }

        fn release_complete(
            &self,
            receipt: &DetachedProcessReceipt,
        ) -> Result<bool, DetachedControlError> {
            let bytes = match read_bounded_file(&self.run_dir.join(RELEASE_FILE)) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(_) => return Err(DetachedControlError::TerminalEvidenceUnavailable),
            };
            let marker = serde_json::from_slice::<PersistedDetachedRelease>(&bytes)
                .map_err(|_| DetachedControlError::TerminalEvidenceUnavailable)?;
            let PersistedDetachedRelease::V1 { run_id, unit_name, invocation_id } = marker;
            let (expected_unit, expected_invocation) = receipt.systemd_authority();
            if run_id != self.run_id
                || unit_name != expected_unit
                || invocation_id != expected_invocation
            {
                return Err(DetachedControlError::AuthorityMismatch);
            }
            Ok(true)
        }

        fn publish_release_complete(
            &self,
            receipt: &DetachedProcessReceipt,
        ) -> Result<(), DetachedControlError> {
            let (unit_name, invocation_id) = receipt.systemd_authority();
            let marker = PersistedDetachedRelease::V1 {
                run_id: self.run_id,
                unit_name: unit_name.to_owned(),
                invocation_id: invocation_id.to_owned(),
            };
            let bytes = serde_json::to_vec(&marker)
                .map_err(|_| DetachedControlError::TerminalEvidencePersistFailed)?;
            if bytes.len() as u64 > MAX_METADATA_BYTES {
                return Err(DetachedControlError::TerminalEvidencePersistFailed);
            }
            self.writer
                .replace(&self.run_dir.join(RELEASE_FILE), &bytes)
                .map_err(|_| DetachedControlError::TerminalEvidencePersistFailed)
        }

        fn terminal_report(
            &self,
            evidence: SystemdTerminalEvidence,
            completion: CompletionCause,
            termination: TerminationDisposition,
        ) -> Result<DetachedTerminalReport, DetachedControlError> {
            match self.read_terminal()? {
                Some(PersistedDetachedTerminal::Completed(target)) => {
                    if target.run_id != self.run_id {
                        return Err(DetachedControlError::TerminalEvidenceUnavailable);
                    }
                    self.reconcile_terminal_evidence(
                        target.leader_exit,
                        Duration::from_micros(target.elapsed_micros),
                        evidence,
                    )?;
                    let completion = completed_target_cause(target.leader_exit, completion);
                    Ok(DetachedTerminalReport::Completed(ProcessReport {
                        run_id: self.run_id,
                        completion,
                        leader_exit: target.leader_exit,
                        containment: ContainmentStrength::CompleteTree,
                        descendants: DescendantDisposition::EmptyWhenObserved,
                        termination,
                        stdout: self.output_report(
                            self.manifest.stdout,
                            target.stdout,
                            STDOUT_FILE,
                        )?,
                        stderr: self.output_report(
                            self.manifest.stderr,
                            target.stderr,
                            STDERR_FILE,
                        )?,
                        elapsed: Duration::from_micros(target.elapsed_micros),
                    }))
                }
                Some(PersistedDetachedTerminal::InfrastructureFailure(failure)) => {
                    self.infrastructure_failure_report(failure, evidence, termination)
                }
                None if evidence.forced
                    || evidence.external_termination
                    || evidence.leader_exit == LeaderExit::Code(HOST_FAILURE_EXIT) =>
                {
                    self.infrastructure_failure_report(
                        PersistedDetachedInfrastructureFailure {
                            run_id: self.run_id,
                            leader_exit: LeaderExitObservation::NotObserved,
                            stdout: self.interrupted_output(self.manifest.stdout, STDOUT_FILE)?,
                            stderr: self.interrupted_output(self.manifest.stderr, STDERR_FILE)?,
                            elapsed_micros: duration_micros(evidence.elapsed),
                        },
                        evidence,
                        termination,
                    )
                }
                None => Err(DetachedControlError::TerminalEvidenceUnavailable),
            }
        }

        fn reconcile_terminal_evidence(
            &self,
            leader_exit: LeaderExitObservation,
            elapsed: Duration,
            evidence: SystemdTerminalEvidence,
        ) -> Result<(), DetachedControlError> {
            let exit_matches = match leader_exit {
                LeaderExitObservation::Observed(leader_exit) => leader_exit == evidence.leader_exit,
                LeaderExitObservation::NotObserved => {
                    evidence.leader_exit == LeaderExit::Code(0)
                        && !evidence.forced
                        && !evidence.external_termination
                }
            };
            if !exit_matches || elapsed > evidence.elapsed {
                return Err(DetachedControlError::TerminalEvidenceUnavailable);
            }
            Ok(())
        }

        fn reconcile_persisted_terminal(
            &self,
            report: &DetachedTerminalReport,
            evidence: SystemdTerminalEvidence,
        ) -> Result<(), DetachedControlError> {
            match report {
                DetachedTerminalReport::Completed(report) => {
                    self.reconcile_terminal_evidence(report.leader_exit, report.elapsed, evidence)
                }
                DetachedTerminalReport::InfrastructureFailure { elapsed, .. }
                    if *elapsed <= evidence.elapsed
                        && (evidence.leader_exit == LeaderExit::Code(HOST_FAILURE_EXIT)
                            || evidence.forced
                            || evidence.external_termination) =>
                {
                    Ok(())
                }
                DetachedTerminalReport::InfrastructureFailure { .. } => {
                    Err(DetachedControlError::TerminalEvidenceUnavailable)
                }
            }
        }

        fn remove(self) -> Result<(), DetachedControlError> {
            std::fs::remove_dir_all(self.run_dir).map_err(|_| {
                DetachedControlError::Unavailable(DetachedUnavailable::DurableStorageUnavailable)
            })
        }

        fn publish_report(
            &self,
            unit_name: &str,
            invocation_id: &str,
            report: &DetachedTerminalReport,
        ) -> Result<(), DetachedControlError> {
            let persisted = match report {
                DetachedTerminalReport::Completed(report) => {
                    PersistedDetachedReport::Completed(PersistedDetachedCompletedReport {
                        run_id: report.run_id,
                        unit_name: unit_name.to_string(),
                        invocation_id: invocation_id.to_string(),
                        completion: report.completion,
                        leader_exit: report.leader_exit,
                        termination: report.termination,
                        stdout: persisted_output(&report.stdout)?,
                        stderr: persisted_output(&report.stderr)?,
                        elapsed_micros: duration_micros(report.elapsed),
                    })
                }
                DetachedTerminalReport::InfrastructureFailure { report, elapsed } => {
                    PersistedDetachedReport::InfrastructureFailure(PersistedDetachedFailureReport {
                        run_id: report.run_id,
                        unit_name: unit_name.to_string(),
                        invocation_id: invocation_id.to_string(),
                        leader_exit: report.leader_exit,
                        termination: report.termination,
                        stdout: persisted_partial_output(&report.stdout)?,
                        stderr: persisted_partial_output(&report.stderr)?,
                        elapsed_micros: duration_micros(*elapsed),
                    })
                }
            };
            let bytes = serde_json::to_vec(&persisted)
                .map_err(|_| DetachedControlError::TerminalEvidencePersistFailed)?;
            if bytes.len() as u64 > MAX_METADATA_BYTES {
                return Err(DetachedControlError::TerminalEvidencePersistFailed);
            }
            self.writer
                .replace(&self.run_dir.join(REPORT_FILE), &bytes)
                .map_err(|_| DetachedControlError::TerminalEvidencePersistFailed)
        }

        fn restore_completed_report(
            &self,
            persisted: PersistedDetachedCompletedReport,
        ) -> Result<DetachedTerminalReport, DetachedControlError> {
            if completed_target_cause(persisted.leader_exit, persisted.completion)
                != persisted.completion
            {
                return Err(DetachedControlError::TerminalEvidenceUnavailable);
            }
            Ok(DetachedTerminalReport::Completed(ProcessReport {
                run_id: persisted.run_id,
                completion: persisted.completion,
                leader_exit: persisted.leader_exit,
                containment: ContainmentStrength::CompleteTree,
                descendants: DescendantDisposition::EmptyWhenObserved,
                termination: persisted.termination,
                stdout: self.output_report(self.manifest.stdout, persisted.stdout, STDOUT_FILE)?,
                stderr: self.output_report(self.manifest.stderr, persisted.stderr, STDERR_FILE)?,
                elapsed: Duration::from_micros(persisted.elapsed_micros),
            }))
        }

        fn restore_failure_report(
            &self,
            persisted: PersistedDetachedFailureReport,
        ) -> Result<DetachedTerminalReport, DetachedControlError> {
            Ok(DetachedTerminalReport::InfrastructureFailure {
                report: ProcessFailureReport {
                    run_id: persisted.run_id,
                    failure: ProcessFailureKind::OwnerTaskFailed,
                    leader_exit: persisted.leader_exit,
                    termination: persisted.termination,
                    stdout: partial_from_output_report(self.output_report(
                        self.manifest.stdout,
                        persisted.stdout,
                        STDOUT_FILE,
                    )?),
                    stderr: partial_from_output_report(self.output_report(
                        self.manifest.stderr,
                        persisted.stderr,
                        STDERR_FILE,
                    )?),
                },
                elapsed: Duration::from_micros(persisted.elapsed_micros),
            })
        }

        fn output_report(
            &self,
            policy: StoredOutputPolicy,
            output: PersistedDetachedOutput,
            file_name: &str,
        ) -> Result<OutputReport, DetachedControlError> {
            match (policy, output) {
                (StoredOutputPolicy::Discarded, PersistedDetachedOutput::Discarded) => {
                    Ok(OutputReport::Discarded)
                }
                (
                    StoredOutputPolicy::Recorded { limit },
                    PersistedDetachedOutput::Recorded {
                        observed_bytes,
                        retained_bytes,
                        disposition,
                        availability,
                        final_tail,
                    },
                ) => {
                    if retained_bytes > limit.get()
                        || observed_bytes < retained_bytes
                        || final_tail.len() > MAX_FINAL_TAIL_BYTES
                        || final_tail.len() as u64 > observed_bytes
                        || (disposition == RecordDisposition::Complete
                            && observed_bytes != retained_bytes)
                        || (disposition == RecordDisposition::Truncated
                            && observed_bytes == retained_bytes
                            && availability == RecordAvailability::Available)
                        || (disposition == RecordDisposition::Interrupted
                            && (availability != RecordAvailability::Unavailable
                                || observed_bytes != retained_bytes
                                || !final_tail.is_empty()))
                    {
                        return Err(DetachedControlError::TerminalEvidenceUnavailable);
                    }
                    let path = self.run_dir.join(file_name);
                    if output_file_size(&path)? != retained_bytes {
                        return Err(DetachedControlError::TerminalEvidenceUnavailable);
                    }
                    Ok(OutputReport::Recorded(RecordReport {
                        path: RecordedOutputPath::new(path),
                        observed_bytes,
                        retained_bytes,
                        disposition,
                        availability,
                        final_tail: final_tail.into_boxed_slice(),
                    }))
                }
                _ => Err(DetachedControlError::TerminalEvidenceUnavailable),
            }
        }

        fn infrastructure_failure_report(
            &self,
            failure: PersistedDetachedInfrastructureFailure,
            evidence: SystemdTerminalEvidence,
            termination: TerminationDisposition,
        ) -> Result<DetachedTerminalReport, DetachedControlError> {
            let elapsed = Duration::from_micros(failure.elapsed_micros);
            if failure.run_id != self.run_id
                || elapsed > evidence.elapsed
                || !(evidence.leader_exit == LeaderExit::Code(HOST_FAILURE_EXIT)
                    || evidence.forced
                    || evidence.external_termination)
            {
                return Err(DetachedControlError::TerminalEvidenceUnavailable);
            }
            Ok(DetachedTerminalReport::InfrastructureFailure {
                report: ProcessFailureReport {
                    run_id: self.run_id,
                    failure: ProcessFailureKind::OwnerTaskFailed,
                    leader_exit: failure.leader_exit,
                    termination,
                    stdout: partial_from_output_report(self.output_report(
                        self.manifest.stdout,
                        failure.stdout,
                        STDOUT_FILE,
                    )?),
                    stderr: partial_from_output_report(self.output_report(
                        self.manifest.stderr,
                        failure.stderr,
                        STDERR_FILE,
                    )?),
                },
                elapsed,
            })
        }

        fn read_terminal(&self) -> Result<Option<PersistedDetachedTerminal>, DetachedControlError> {
            let bytes = match read_bounded_file(&self.run_dir.join(TERMINAL_FILE)) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err(DetachedControlError::TerminalEvidenceUnavailable),
            };
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| DetachedControlError::TerminalEvidenceUnavailable)
        }

        fn interrupted_output(
            &self,
            policy: StoredOutputPolicy,
            file_name: &str,
        ) -> Result<PersistedDetachedOutput, DetachedControlError> {
            match policy {
                StoredOutputPolicy::Discarded => Ok(PersistedDetachedOutput::Discarded),
                StoredOutputPolicy::Recorded { limit } => {
                    let path = self.run_dir.join(file_name);
                    let retained_bytes = output_file_size(&path)?;
                    if retained_bytes > limit.get() {
                        return Err(DetachedControlError::TerminalEvidenceUnavailable);
                    }
                    Ok(PersistedDetachedOutput::Recorded {
                        observed_bytes: retained_bytes,
                        retained_bytes,
                        disposition: RecordDisposition::Interrupted,
                        availability: RecordAvailability::Unavailable,
                        final_tail: Vec::new(),
                    })
                }
            }
        }
    }

    enum ValidatedPendingLaunch {
        Prepared { run_id: ProcessRunId, host_executable: String, capability_path: String },
        Authority(DetachedLaunchRecovery),
    }

    impl ValidatedPendingLaunch {
        fn run_id(&self) -> ProcessRunId {
            match self {
                Self::Prepared { run_id, .. } => *run_id,
                Self::Authority(recovery) => recovery.run_id(),
            }
        }

        fn public(&self) -> PendingDetachedLaunch {
            let phase = match self {
                Self::Prepared { .. } => PendingDetachedLaunchPhase::Prepared,
                Self::Authority(_) => PendingDetachedLaunchPhase::AuthorityBound,
            };
            PendingDetachedLaunch::new(self.run_id(), phase)
        }
    }

    fn detached_run_directories(state_root: &Path) -> Result<Vec<PathBuf>, DetachedControlError> {
        let entries = std::fs::read_dir(state_root).map_err(|_| {
            DetachedControlError::Unavailable(DetachedUnavailable::DurableStorageUnavailable)
        })?;
        let mut run_dirs = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|_| {
                DetachedControlError::Unavailable(DetachedUnavailable::DurableStorageUnavailable)
            })?;
            let run_dir = entry.path();
            let intent = run_dir.join(LAUNCH_INTENT_FILE);
            match std::fs::symlink_metadata(intent) {
                Ok(metadata)
                    if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
                        && metadata.file_type().is_file()
                        && !metadata.file_type().is_symlink() =>
                {
                    validate_run_directory(&run_dir)?;
                    run_dirs.push(run_dir);
                }
                Ok(_) => return Err(DetachedControlError::AuthorityMismatch),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    return Err(DetachedControlError::Unavailable(
                        DetachedUnavailable::DurableStorageUnavailable,
                    ));
                }
            }
        }
        Ok(run_dirs)
    }

    fn acquire_launch_lock(run_dir: &Path) -> Result<std::fs::File, DetachedControlError> {
        match AtomicFileWriter::new(run_dir, "detached-launch.lock", ".detached-launch")
            .try_lock()
            .map_err(|_| {
                DetachedControlError::Unavailable(DetachedUnavailable::DurableStorageUnavailable)
            })? {
            AtomicFileTryLock::Acquired(lock) => Ok(lock),
            AtomicFileTryLock::Busy => Err(DetachedControlError::ControlBusy),
        }
    }

    fn try_acquire_launch_lock(
        run_dir: &Path,
    ) -> Result<Option<std::fs::File>, DetachedControlError> {
        match AtomicFileWriter::new(run_dir, "detached-launch.lock", ".detached-launch")
            .try_lock()
            .map_err(|_| {
                DetachedControlError::Unavailable(DetachedUnavailable::DurableStorageUnavailable)
            })? {
            AtomicFileTryLock::Acquired(lock) => Ok(Some(lock)),
            AtomicFileTryLock::Busy => Ok(None),
        }
    }

    fn read_pending_launch(
        run_dir: &Path,
    ) -> Result<Option<ValidatedPendingLaunch>, DetachedControlError> {
        match std::fs::symlink_metadata(run_dir.join(COMMIT_FILE)) {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
            {
                return Ok(None);
            }
            Ok(_) => return Err(DetachedControlError::AuthorityMismatch),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(DetachedControlError::Unavailable(
                    DetachedUnavailable::DurableStorageUnavailable,
                ));
            }
        }
        let intent = read_json::<PersistedDetachedLaunchIntent>(&run_dir.join(LAUNCH_INTENT_FILE))
            .map_err(|_| DetachedControlError::AuthorityMismatch)?;
        let pending = match intent {
            PersistedDetachedLaunchIntent::Prepared {
                run_id,
                host_executable,
                capability_path,
            } => {
                let executable = Path::new(&host_executable);
                let capability = Path::new(&capability_path);
                if !executable.is_absolute()
                    || !capability.is_absolute()
                    || capability.parent() != Some(run_dir)
                    || !capability.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
                        name.starts_with(".detached-host-") && name.ends_with(".json")
                    })
                {
                    return Err(DetachedControlError::AuthorityMismatch);
                }
                ValidatedPendingLaunch::Prepared { run_id, host_executable, capability_path }
            }
            PersistedDetachedLaunchIntent::Authority { recovery } => {
                ValidatedPendingLaunch::Authority(
                    DetachedLaunchRecovery::decode(&recovery)
                        .map_err(|_| DetachedControlError::AuthorityMismatch)?,
                )
            }
        };
        if run_dir.file_name().and_then(|name| name.to_str())
            != Some(pending.run_id().to_string().as_str())
        {
            return Err(DetachedControlError::AuthorityMismatch);
        }
        Ok(Some(pending))
    }

    async fn recover_validated_pending(
        run_dir: &Path,
        pending: ValidatedPendingLaunch,
        required_authority: Option<&DetachedLaunchRecovery>,
    ) -> Result<(), DetachedControlError> {
        let backend =
            SystemdBackend::connect(DetachedLifetimeRequirement::InvocationIndependent).await?;
        match pending {
            ValidatedPendingLaunch::Prepared { run_id, host_executable, capability_path } => {
                let expected_invocation_id = match required_authority {
                    Some(recovery) if recovery.run_id() == run_id => {
                        Some(recovery.systemd_authority().1)
                    }
                    Some(_) => return Err(DetachedControlError::AuthorityMismatch),
                    None => None,
                };
                backend
                    .recover_prepared_launch(
                        run_id,
                        &host_executable,
                        &capability_path,
                        expected_invocation_id,
                    )
                    .await
                    .map_err(map_control_error)?
            }
            ValidatedPendingLaunch::Authority(recovery) => {
                if required_authority.is_some_and(|required| required != &recovery) {
                    return Err(DetachedControlError::AuthorityMismatch);
                }
                let (unit_name, invocation_id) = recovery.systemd_authority();
                backend
                    .recover_launch(unit_name, invocation_id)
                    .await
                    .map_err(map_control_error)?;
            }
        }
        std::fs::remove_dir_all(run_dir).map_err(|_| {
            DetachedControlError::Unavailable(DetachedUnavailable::DurableStorageUnavailable)
        })
    }

    fn map_start_error(run_id: ProcessRunId, error: SystemdLaunchError) -> DetachedStartError {
        match error {
            SystemdLaunchError::Request => DetachedStartError::StartRequestFailed,
            SystemdLaunchError::Job => DetachedStartError::StartJobFailed,
            SystemdLaunchError::Authority => DetachedStartError::AuthorityValidationFailed,
            SystemdLaunchError::RecoveryRequired { unit_name, invocation_id } => {
                recovery_error(run_id, unit_name, invocation_id)
            }
            SystemdLaunchError::UnrecoverableAuthority => {
                DetachedStartError::UnrecoverableAuthority
            }
        }
    }

    fn recovery_error(
        run_id: ProcessRunId,
        unit_name: String,
        invocation_id: String,
    ) -> DetachedStartError {
        match DetachedLaunchRecovery::linux_systemd(run_id, unit_name, invocation_id) {
            Ok(recovery) => DetachedStartError::RecoveryRequired { recovery },
            Err(_) => DetachedStartError::UnrecoverableAuthority,
        }
    }

    fn observed_completion(evidence: SystemdTerminalEvidence) -> CompletionCause {
        if evidence.external_termination {
            CompletionCause::ExternalTermination
        } else {
            CompletionCause::Natural
        }
    }

    fn completed_target_cause(
        leader_exit: LeaderExitObservation,
        observed: CompletionCause,
    ) -> CompletionCause {
        match (leader_exit, observed) {
            (LeaderExitObservation::NotObserved, CompletionCause::Cancelled) => {
                CompletionCause::Cancelled
            }
            (LeaderExitObservation::NotObserved, _) => CompletionCause::ExternalTermination,
            (LeaderExitObservation::Observed(_), observed) => observed,
        }
    }

    fn map_control_error(error: SystemdControlError) -> DetachedControlError {
        match error {
            SystemdControlError::Request => DetachedControlError::ControlRequestFailed,
            SystemdControlError::Job => DetachedControlError::ControlJobFailed,
            SystemdControlError::AuthorityMismatch => DetachedControlError::AuthorityMismatch,
            SystemdControlError::Evidence => DetachedControlError::TerminalEvidenceUnavailable,
        }
    }

    fn persisted_output(
        report: &OutputReport,
    ) -> Result<PersistedDetachedOutput, DetachedControlError> {
        match report {
            OutputReport::Recorded(report) => Ok(PersistedDetachedOutput::Recorded {
                observed_bytes: report.observed_bytes,
                retained_bytes: report.retained_bytes,
                disposition: report.disposition,
                availability: report.availability,
                final_tail: report.final_tail.to_vec(),
            }),
            OutputReport::Discarded => Ok(PersistedDetachedOutput::Discarded),
            _ => Err(DetachedControlError::TerminalEvidenceUnavailable),
        }
    }

    fn persisted_partial_output(
        report: &PartialOutputReport,
    ) -> Result<PersistedDetachedOutput, DetachedControlError> {
        match report {
            PartialOutputReport::Recorded(report) => Ok(PersistedDetachedOutput::Recorded {
                observed_bytes: report.observed_bytes,
                retained_bytes: report.retained_bytes,
                disposition: report.disposition,
                availability: report.availability,
                final_tail: report.final_tail.to_vec(),
            }),
            PartialOutputReport::Discarded => Ok(PersistedDetachedOutput::Discarded),
            _ => Err(DetachedControlError::TerminalEvidenceUnavailable),
        }
    }

    fn validate_persisted_identity(
        expected_run_id: ProcessRunId,
        expected_unit_name: &str,
        expected_invocation_id: &str,
        run_id: ProcessRunId,
        unit_name: &str,
        invocation_id: &str,
    ) -> Result<(), DetachedControlError> {
        if run_id != expected_run_id
            || unit_name != expected_unit_name
            || invocation_id != expected_invocation_id
        {
            return Err(DetachedControlError::TerminalEvidenceUnavailable);
        }
        Ok(())
    }

    fn output_file_size(path: &Path) -> Result<u64, DetachedControlError> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| DetachedControlError::TerminalEvidenceUnavailable)?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(DetachedControlError::TerminalEvidenceUnavailable);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != rustix::process::getuid().as_raw() || metadata.mode() & 0o077 != 0
            {
                return Err(DetachedControlError::TerminalEvidenceUnavailable);
            }
        }
        Ok(metadata.len())
    }

    fn validate_timing(spec: &DetachedProcessSpec) -> Result<(), DetachedStartError> {
        let now = tokio::time::Instant::now();
        if spec.termination.grace_period > MAX_PROCESS_DURATION
            || now.checked_add(spec.termination.grace_period).is_none()
        {
            return Err(DetachedStartError::StartRequestFailed);
        }
        Ok(())
    }

    fn publish_launch_intent(run_dir: &Path, recovery: &DetachedLaunchRecovery) -> Result<(), ()> {
        publish_json(
            run_dir,
            LAUNCH_INTENT_FILE,
            &PersistedDetachedLaunchIntent::Authority { recovery: recovery.encode() },
        )
    }

    fn publish_prepared_launch_intent(
        run_dir: &Path,
        run_id: ProcessRunId,
        host: &DetachedHostCapability,
    ) -> Result<(), DetachedStartError> {
        let host_executable = host
            .executable()
            .to_str()
            .map(str::to_owned)
            .ok_or(DetachedStartError::StartRequestFailed)?;
        let capability_path = host
            .path()
            .to_str()
            .map(str::to_owned)
            .ok_or(DetachedStartError::StartRequestFailed)?;
        publish_json(
            run_dir,
            LAUNCH_INTENT_FILE,
            &PersistedDetachedLaunchIntent::Prepared { run_id, host_executable, capability_path },
        )
        .map_err(|_| DetachedStartError::ReceiptPersistFailed)
    }

    fn discard_run_directory(run_dir: &Path) {
        let _ = std::fs::remove_dir_all(run_dir);
    }

    fn publish_json<T: Serialize>(run_dir: &Path, name: &str, value: &T) -> Result<(), ()> {
        let writer = AtomicFileWriter::new(run_dir, "detached.lock", ".detached");
        let lock = writer.lock().map_err(|_| ())?;
        let bytes = serde_json::to_vec(value).map_err(|_| ())?;
        if bytes.len() as u64 > MAX_METADATA_BYTES {
            return Err(());
        }
        let result = writer.replace(&run_dir.join(name), &bytes).map_err(|_| ());
        drop(lock);
        result
    }

    fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, ()> {
        let bytes = read_bounded_file(path).map_err(|_| ())?;
        serde_json::from_slice(&bytes).map_err(|_| ())
    }

    fn read_bounded_file(path: &Path) -> Result<Vec<u8>, std::io::Error> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file()
            || metadata.uid() != rustix::process::getuid().as_raw()
            || metadata.mode() & 0o077 != 0
            || metadata.len() > MAX_METADATA_BYTES
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "detached metadata is invalid",
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_METADATA_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_METADATA_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "detached metadata exceeds its bound",
            ));
        }
        Ok(bytes)
    }

    fn duration_micros(duration: Duration) -> u64 {
        duration.as_micros().min(u128::from(u64::MAX)) as u64
    }

    fn validate_run_directory(path: &Path) -> Result<(), DetachedControlError> {
        let metadata = std::fs::symlink_metadata(path).map_err(|_| {
            DetachedControlError::Unavailable(DetachedUnavailable::DurableStorageUnavailable)
        })?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(DetachedControlError::Unavailable(
                DetachedUnavailable::DurableStorageUnavailable,
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != rustix::process::getuid().as_raw() || metadata.mode() & 0o077 != 0
            {
                return Err(DetachedControlError::Unavailable(
                    DetachedUnavailable::DurableStorageUnavailable,
                ));
            }
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
impl super::ProcessSupervisor {
    pub async fn launch_detached(
        &self,
        spec: super::DetachedProcessSpec,
    ) -> Result<super::DetachedLaunchTransaction, super::DetachedStartError> {
        drop(spec);
        Err(super::DetachedStartError::Unavailable(super::DetachedUnavailable::UnsupportedPlatform))
    }

    pub async fn inspect_detached(
        &self,
        receipt: &super::DetachedProcessReceipt,
    ) -> Result<super::DetachedProcessStatus, super::DetachedControlError> {
        let _ = receipt.run_id();
        Err(super::DetachedControlError::Unavailable(
            super::DetachedUnavailable::UnsupportedPlatform,
        ))
    }

    pub async fn stop_detached(
        &self,
        receipt: &super::DetachedProcessReceipt,
    ) -> Result<super::DetachedProcessTerminal, super::DetachedControlError> {
        let _ = receipt.run_id();
        Err(super::DetachedControlError::Unavailable(
            super::DetachedUnavailable::UnsupportedPlatform,
        ))
    }

    pub(crate) async fn abandon_detached_transaction(
        &self,
        receipt: &super::DetachedProcessReceipt,
    ) -> Result<(), super::DetachedControlError> {
        let _ = receipt.run_id();
        Err(super::DetachedControlError::Unavailable(
            super::DetachedUnavailable::UnsupportedPlatform,
        ))
    }

    pub async fn forget_detached(
        &self,
        receipt: &super::DetachedProcessReceipt,
    ) -> Result<(), super::DetachedControlError> {
        let _ = receipt.run_id();
        Err(super::DetachedControlError::Unavailable(
            super::DetachedUnavailable::UnsupportedPlatform,
        ))
    }

    pub async fn recover_detached_launch(
        &self,
        recovery: &super::DetachedLaunchRecovery,
    ) -> Result<(), super::DetachedControlError> {
        let _ = recovery.run_id();
        Err(super::DetachedControlError::Unavailable(
            super::DetachedUnavailable::UnsupportedPlatform,
        ))
    }

    pub fn list_pending_detached_launches(
        &self,
    ) -> Result<Vec<super::PendingDetachedLaunch>, super::DetachedControlError> {
        Err(super::DetachedControlError::Unavailable(
            super::DetachedUnavailable::UnsupportedPlatform,
        ))
    }

    pub async fn recover_pending_detached_launches(
        &self,
    ) -> Result<Vec<super::ProcessRunId>, super::DetachedControlError> {
        Err(super::DetachedControlError::Unavailable(
            super::DetachedUnavailable::UnsupportedPlatform,
        ))
    }
}
