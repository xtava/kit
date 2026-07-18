use std::{
    ffi::{CStr, OsStr, OsString},
    path::Path,
    time::Duration,
};

use futures_util::StreamExt;
use systemd_zbus::{ActiveState, ManagerProxy, Mode, ServiceProxy, UnitProxy};
use tokio::sync::Mutex;
use uuid::Uuid;
use zbus::{
    zvariant::{OwnedObjectPath, Value},
    Connection,
};

use crate::framework::process::{
    DetachedLifetimeRequirement, DetachedProcessSpec, DetachedUnavailable, LeaderExit,
    ProcessRunId, SignalNumber,
};

use super::super::{
    detached_host::{DetachedHostCapability, HOST_MODE},
    receipt::systemd_unit_name,
};

const JOB_TIMEOUT: Duration = Duration::from_secs(30);
static ATTACHED_DELEGATION: Mutex<bool> = Mutex::const_new(false);

pub(crate) async fn ensure_attached_delegation() -> Result<(), String> {
    let mut delegated = ATTACHED_DELEGATION.lock().await;
    if *delegated {
        return Ok(());
    }
    let backend = SystemdBackend::connect(DetachedLifetimeRequirement::InvocationIndependent)
        .await
        .map_err(|error| error.to_string())?;
    backend.create_attached_delegation().await?;
    *delegated = true;
    Ok(())
}

pub(crate) struct SystemdBackend {
    connection: Connection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SystemdLaunchError {
    Request,
    Job,
    Authority,
    RecoveryRequired { unit_name: String, invocation_id: String },
    UnrecoverableAuthority,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SystemdControlError {
    Request,
    Job,
    AuthorityMismatch,
    Evidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SystemdAuthority {
    pub unit_name: String,
    pub invocation_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SystemdRuntimeStatus {
    Running,
    Stopping,
    Terminal(SystemdTerminalEvidence),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SystemdTerminalEvidence {
    pub leader_exit: LeaderExit,
    pub elapsed: Duration,
    pub forced: bool,
    pub external_termination: bool,
}

pub(crate) enum AuthorityResolution {
    Missing,
    Present(PinnedSystemdAuthority),
}

pub(crate) struct PinnedSystemdAuthority {
    unit_name: String,
    invocation_id: String,
    unit_path: OwnedObjectPath,
    status: SystemdRuntimeStatus,
}

impl PinnedSystemdAuthority {
    pub(crate) fn status(&self) -> SystemdRuntimeStatus {
        self.status
    }
}

pub(crate) struct SystemdStopResult {
    pub evidence: SystemdTerminalEvidence,
    pub stop_accepted: bool,
}

impl SystemdBackend {
    pub async fn connect(
        lifetime: DetachedLifetimeRequirement,
    ) -> Result<Self, DetachedUnavailable> {
        if lifetime == DetachedLifetimeRequirement::LogoutIndependent && !linger_is_enabled() {
            return Err(DetachedUnavailable::PersistentUserManagerUnavailable);
        }
        let connection = match Connection::session().await {
            Ok(connection) => connection,
            Err(_) => {
                let address =
                    format!("unix:path=/run/user/{}/bus", rustix::process::getuid().as_raw());
                zbus::connection::Builder::address(address.as_str())
                    .map_err(|_| DetachedUnavailable::SessionBusUnavailable)?
                    .build()
                    .await
                    .map_err(|_| DetachedUnavailable::SessionBusUnavailable)?
            }
        };
        let manager = ManagerProxy::new(&connection)
            .await
            .map_err(|_| DetachedUnavailable::UserManagerUnavailable)?;
        manager.version().await.map_err(|_| DetachedUnavailable::UserManagerUnavailable)?;
        Ok(Self { connection })
    }

    async fn create_attached_delegation(&self) -> Result<(), String> {
        let manager = self
            .manager()
            .await
            .map_err(|_| String::from("connect to the systemd user manager for containment"))?;
        manager
            .subscribe()
            .await
            .map_err(|_| String::from("subscribe to systemd containment jobs"))?;
        let mut removed = manager
            .receive_job_removed()
            .await
            .map_err(|_| String::from("observe systemd containment jobs"))?;
        let pid = u32::try_from(rustix::process::getpid().as_raw_pid())
            .map_err(|_| String::from("resolve Kit PID for delegated containment"))?;
        let unit_name = format!("kit-attached-{}-{}.scope", pid, Uuid::new_v4().simple());
        let properties = vec![
            ("Description", Value::new(String::from("Kit attached process containment"))),
            ("PIDs", Value::new(vec![pid])),
            ("Delegate", Value::new(true)),
            ("CollectMode", Value::new(String::from("inactive-or-failed"))),
        ];
        let job = manager
            .start_transient_unit(&unit_name, Mode::Fail, &properties, &[])
            .await
            .map_err(|_| String::from("create delegated systemd containment scope"))?;
        let result = wait_for_job(&mut removed, &job)
            .await
            .map_err(|_| String::from("await delegated systemd containment scope"))?;
        if result != "done" {
            return Err(String::from("delegated systemd containment scope did not start"));
        }
        let path = manager
            .get_unit(&unit_name)
            .await
            .map_err(|_| String::from("resolve delegated systemd containment scope"))?;
        let unit = UnitProxy::builder(&self.connection)
            .path(path)
            .map_err(|_| String::from("bind delegated systemd containment scope"))?
            .build()
            .await
            .map_err(|_| String::from("bind delegated systemd containment scope"))?;
        if unit
            .id()
            .await
            .map_err(|_| String::from("validate delegated systemd containment scope"))?
            != unit_name
        {
            return Err(String::from("delegated systemd containment scope identity mismatch"));
        }
        let contains_kit = manager
            .get_unit_processes(&unit_name)
            .await
            .map_err(|_| String::from("validate delegated systemd containment membership"))?
            .iter()
            .any(|process| process.pid == pid);
        if !contains_kit {
            return Err(String::from("delegated systemd containment scope does not own Kit"));
        }
        Ok(())
    }

    pub async fn launch(
        &self,
        run_id: ProcessRunId,
        spec: &DetachedProcessSpec,
        host: &DetachedHostCapability,
    ) -> Result<SystemdAuthority, SystemdLaunchError> {
        let manager = self.manager().await.map_err(|_| SystemdLaunchError::Request)?;
        manager.subscribe().await.map_err(|_| SystemdLaunchError::Request)?;
        let mut removed =
            manager.receive_job_removed().await.map_err(|_| SystemdLaunchError::Request)?;
        let unit_name = systemd_unit_name(run_id);
        let properties = transient_properties(spec, host)?;
        let job = match manager.start_transient_unit(&unit_name, Mode::Fail, &properties, &[]).await
        {
            Ok(job) => job,
            Err(zbus::Error::MethodError(_, _, _)) => return Err(SystemdLaunchError::Request),
            Err(_) => {
                return Err(self.launch_failure_after_request(run_id, host, None).await);
            }
        };
        let job_result = wait_for_job(&mut removed, &job).await;
        if !matches!(job_result.as_deref(), Ok("done")) {
            let authority = self.read_authority(&manager, &unit_name, None).await.ok();
            return Err(self
                .launch_failure_after_request(run_id, host, authority)
                .await
                .with_kind(SystemdLaunchError::Job));
        }
        match self.read_authority(&manager, &unit_name, None).await {
            Ok(authority) => {
                if wait_for_capability_consumption(host.path()).await.is_ok() {
                    Ok(authority)
                } else {
                    Err(self
                        .launch_failure_after_request(run_id, host, Some(authority))
                        .await
                        .with_kind(SystemdLaunchError::Request))
                }
            }
            Err(_) => Err(self
                .launch_failure_after_request(run_id, host, None)
                .await
                .with_kind(SystemdLaunchError::Authority)),
        }
    }

    pub async fn resolve(
        &self,
        unit_name: &str,
        invocation_id: &str,
    ) -> Result<AuthorityResolution, SystemdControlError> {
        let manager = self.manager().await.map_err(|_| SystemdControlError::Request)?;
        let unit_path = match manager.get_unit(unit_name).await {
            Ok(path) => path,
            Err(error) if is_no_such_unit(&error) => return Ok(AuthorityResolution::Missing),
            Err(_) => return Err(SystemdControlError::Request),
        };
        manager.ref_unit(unit_name).await.map_err(|_| SystemdControlError::Request)?;
        let result =
            self.validate_and_read(&manager, unit_name, invocation_id, unit_path.clone()).await;
        match result {
            Ok((status, unit_path)) => Ok(AuthorityResolution::Present(PinnedSystemdAuthority {
                unit_name: unit_name.to_string(),
                invocation_id: invocation_id.to_string(),
                unit_path,
                status,
            })),
            Err(error) => {
                let _ = manager.unref_unit(unit_name).await;
                Err(error)
            }
        }
    }

    pub async fn stop(
        &self,
        authority: &mut PinnedSystemdAuthority,
    ) -> Result<SystemdStopResult, SystemdControlError> {
        let manager = self.manager().await.map_err(|_| SystemdControlError::Request)?;
        authority.unit_path = self
            .validate_identity(
                &manager,
                &authority.unit_name,
                &authority.invocation_id,
                &authority.unit_path,
            )
            .await?;
        let unit = UnitProxy::builder(&self.connection)
            .path(authority.unit_path.clone())
            .map_err(|_| SystemdControlError::Evidence)?
            .build()
            .await
            .map_err(|_| SystemdControlError::Evidence)?;
        let active = unit.active_state().await.map_err(|_| SystemdControlError::Evidence)?;
        let sub_state = unit.sub_state().await.map_err(|_| SystemdControlError::Evidence)?;
        if terminal_state(active, &sub_state) {
            let evidence =
                self.read_terminal(&authority.unit_name, authority.unit_path.clone()).await?;
            authority.status = SystemdRuntimeStatus::Terminal(evidence);
            return Ok(SystemdStopResult { evidence, stop_accepted: false });
        }
        let stop_accepted = authority.status == SystemdRuntimeStatus::Running;

        manager.subscribe().await.map_err(|_| SystemdControlError::Request)?;
        let mut removed =
            manager.receive_job_removed().await.map_err(|_| SystemdControlError::Request)?;
        let job = manager
            .stop_unit(&authority.unit_name, Mode::Fail)
            .await
            .map_err(|_| SystemdControlError::Request)?;
        if wait_for_job(&mut removed, &job).await.map_err(|_| SystemdControlError::Job)? != "done" {
            return Err(SystemdControlError::Job);
        }
        let evidence =
            self.read_terminal(&authority.unit_name, authority.unit_path.clone()).await?;
        authority.status = SystemdRuntimeStatus::Terminal(evidence);
        Ok(SystemdStopResult { evidence, stop_accepted })
    }

    pub async fn release(
        &self,
        authority: PinnedSystemdAuthority,
    ) -> Result<(), SystemdControlError> {
        let manager = self.manager().await.map_err(|_| SystemdControlError::Request)?;
        let deactivated = self.deactivate_completed(&manager, &authority.unit_name).await;
        let reset = if deactivated.is_ok() {
            manager
                .reset_failed_unit(&authority.unit_name)
                .await
                .map_err(|_| SystemdControlError::Request)
        } else {
            Ok(())
        };
        let unref = manager
            .unref_unit(&authority.unit_name)
            .await
            .map_err(|_| SystemdControlError::Request);
        deactivated?;
        reset?;
        unref
    }

    pub async fn unpin(
        &self,
        authority: PinnedSystemdAuthority,
    ) -> Result<(), SystemdControlError> {
        let manager = self.manager().await.map_err(|_| SystemdControlError::Request)?;
        manager.unref_unit(&authority.unit_name).await.map_err(|_| SystemdControlError::Request)
    }

    async fn deactivate_completed(
        &self,
        manager: &ManagerProxy<'_>,
        unit_name: &str,
    ) -> Result<(), SystemdControlError> {
        let path = match manager.get_unit(unit_name).await {
            Ok(path) => path,
            Err(error) if is_no_such_unit(&error) => return Ok(()),
            Err(_) => return Err(SystemdControlError::Request),
        };
        let unit = UnitProxy::builder(&self.connection)
            .path(path)
            .map_err(|_| SystemdControlError::Evidence)?
            .build()
            .await
            .map_err(|_| SystemdControlError::Evidence)?;
        let active = unit.active_state().await.map_err(|_| SystemdControlError::Evidence)?;
        if !matches!(active, ActiveState::Inactive | ActiveState::Failed) {
            manager.subscribe().await.map_err(|_| SystemdControlError::Request)?;
            let mut removed =
                manager.receive_job_removed().await.map_err(|_| SystemdControlError::Request)?;
            let job = manager
                .stop_unit(unit_name, Mode::Fail)
                .await
                .map_err(|_| SystemdControlError::Request)?;
            if wait_for_job(&mut removed, &job).await.map_err(|_| SystemdControlError::Job)?
                != "done"
            {
                return Err(SystemdControlError::Job);
            }
        }
        Ok(())
    }

    pub async fn stop_and_release(
        &self,
        authority: &SystemdAuthority,
    ) -> Result<(), SystemdControlError> {
        let mut pinned = match self.resolve(&authority.unit_name, &authority.invocation_id).await? {
            AuthorityResolution::Missing => return Ok(()),
            AuthorityResolution::Present(pinned) => pinned,
        };
        let stop = self.stop(&mut pinned).await;
        if let Err(error) = stop {
            let _ = self.unpin(pinned).await;
            return Err(error);
        }
        self.release(pinned).await
    }

    pub async fn recover_launch(
        &self,
        unit_name: &str,
        invocation_id: &str,
    ) -> Result<(), SystemdControlError> {
        let authority = SystemdAuthority {
            unit_name: unit_name.to_string(),
            invocation_id: invocation_id.to_string(),
        };
        self.stop_and_release(&authority).await
    }

    pub async fn recover_prepared_launch(
        &self,
        run_id: ProcessRunId,
        host_executable: &str,
        capability_path: &str,
        expected_invocation_id: Option<&str>,
    ) -> Result<(), SystemdControlError> {
        let manager = self.manager().await.map_err(|_| SystemdControlError::Request)?;
        let unit_name = systemd_unit_name(run_id);
        match manager.get_unit(&unit_name).await {
            Ok(_) => {}
            Err(error) if is_no_such_unit(&error) => return Ok(()),
            Err(_) => return Err(SystemdControlError::Request),
        }
        let authority = self
            .read_authority(&manager, &unit_name, expected_invocation_id)
            .await
            .map_err(|_| SystemdControlError::Evidence)?;
        let mut pinned = match self.resolve(&unit_name, &authority.invocation_id).await? {
            AuthorityResolution::Missing => return Ok(()),
            AuthorityResolution::Present(pinned) => pinned,
        };
        let starts = async {
            let service = ServiceProxy::builder(&self.connection)
                .path(pinned.unit_path.clone())
                .map_err(|_| SystemdControlError::Evidence)?
                .build()
                .await
                .map_err(|_| SystemdControlError::Evidence)?;
            service.exec_start().await.map_err(|_| SystemdControlError::Evidence)
        }
        .await;
        let starts = match starts {
            Ok(starts) => starts,
            Err(error) => {
                let _ = self.unpin(pinned).await;
                return Err(error);
            }
        };
        let expected_arguments = [host_executable, HOST_MODE, capability_path];
        let matches = starts.len() == 1
            && starts[0].binary_path == host_executable
            && starts[0].arguments.iter().map(String::as_str).eq(expected_arguments);
        if !matches {
            let _ = self.unpin(pinned).await;
            return Err(SystemdControlError::AuthorityMismatch);
        }
        if let Err(error) = self.stop(&mut pinned).await {
            let _ = self.unpin(pinned).await;
            return Err(error);
        }
        self.release(pinned).await
    }

    async fn launch_failure_after_request(
        &self,
        run_id: ProcessRunId,
        host: &DetachedHostCapability,
        authority: Option<SystemdAuthority>,
    ) -> SystemdLaunchError {
        let cleanup = match authority.as_ref() {
            Some(authority) => self.stop_and_release(authority).await.map_err(|_| ()),
            None => match (host.executable().to_str(), host.path().to_str()) {
                (Some(host_executable), Some(capability_path)) => self
                    .recover_prepared_launch(run_id, host_executable, capability_path, None)
                    .await
                    .map_err(|_| ()),
                _ => Err(()),
            },
        };
        if cleanup.is_ok() {
            return SystemdLaunchError::Request;
        }
        match authority {
            Some(authority) => SystemdLaunchError::RecoveryRequired {
                unit_name: authority.unit_name,
                invocation_id: authority.invocation_id,
            },
            None => SystemdLaunchError::UnrecoverableAuthority,
        }
    }

    async fn manager(&self) -> zbus::Result<ManagerProxy<'_>> {
        ManagerProxy::new(&self.connection).await
    }

    async fn read_authority(
        &self,
        manager: &ManagerProxy<'_>,
        unit_name: &str,
        expected_invocation: Option<&str>,
    ) -> Result<SystemdAuthority, SystemdLaunchError> {
        let path = manager.get_unit(unit_name).await.map_err(|_| SystemdLaunchError::Authority)?;
        let unit = UnitProxy::builder(&self.connection)
            .path(path.clone())
            .map_err(|_| SystemdLaunchError::Authority)?
            .build()
            .await
            .map_err(|_| SystemdLaunchError::Authority)?;
        if unit.id().await.map_err(|_| SystemdLaunchError::Authority)? != unit_name {
            return Err(SystemdLaunchError::Authority);
        }
        let invocation = unit.invocation_id().await.map_err(|_| SystemdLaunchError::Authority)?;
        let invocation_id = encode_invocation(&invocation).ok_or(SystemdLaunchError::Authority)?;
        if expected_invocation.is_some_and(|expected| expected != invocation_id) {
            return Err(SystemdLaunchError::Authority);
        }
        let reverse = manager
            .get_unit_by_invocation_id(&invocation)
            .await
            .map_err(|_| SystemdLaunchError::Authority)?;
        let reverse_unit = UnitProxy::builder(&self.connection)
            .path(reverse)
            .map_err(|_| SystemdLaunchError::Authority)?
            .build()
            .await
            .map_err(|_| SystemdLaunchError::Authority)?;
        if reverse_unit.id().await.map_err(|_| SystemdLaunchError::Authority)? != unit_name
            || encode_invocation(
                &reverse_unit.invocation_id().await.map_err(|_| SystemdLaunchError::Authority)?,
            )
            .as_deref()
                != Some(invocation_id.as_str())
        {
            return Err(SystemdLaunchError::Authority);
        }
        Ok(SystemdAuthority { unit_name: unit_name.to_string(), invocation_id })
    }

    async fn validate_and_read(
        &self,
        manager: &ManagerProxy<'_>,
        unit_name: &str,
        invocation_id: &str,
        unit_path: OwnedObjectPath,
    ) -> Result<(SystemdRuntimeStatus, OwnedObjectPath), SystemdControlError> {
        let unit_path =
            self.validate_identity(manager, unit_name, invocation_id, &unit_path).await?;
        let unit = UnitProxy::builder(&self.connection)
            .path(unit_path.clone())
            .map_err(|_| SystemdControlError::Evidence)?
            .build()
            .await
            .map_err(|_| SystemdControlError::Evidence)?;
        let state = unit.active_state().await.map_err(|_| SystemdControlError::Evidence)?;
        let sub_state = unit.sub_state().await.map_err(|_| SystemdControlError::Evidence)?;
        let status = match state {
            ActiveState::Deactivating => SystemdRuntimeStatus::Stopping,
            state if terminal_state(state, &sub_state) => SystemdRuntimeStatus::Terminal(
                self.read_terminal(unit_name, unit_path.clone()).await?,
            ),
            _ => SystemdRuntimeStatus::Running,
        };
        Ok((status, unit_path))
    }

    async fn validate_identity(
        &self,
        manager: &ManagerProxy<'_>,
        unit_name: &str,
        invocation_id: &str,
        unit_path: &OwnedObjectPath,
    ) -> Result<OwnedObjectPath, SystemdControlError> {
        let unit = UnitProxy::builder(&self.connection)
            .path(unit_path.clone())
            .map_err(|_| SystemdControlError::Evidence)?
            .build()
            .await
            .map_err(|_| SystemdControlError::Evidence)?;
        if unit.id().await.map_err(|_| SystemdControlError::Evidence)? != unit_name {
            return Err(SystemdControlError::AuthorityMismatch);
        }
        let invocation = unit.invocation_id().await.map_err(|_| SystemdControlError::Evidence)?;
        if encode_invocation(&invocation).as_deref() != Some(invocation_id) {
            return Err(SystemdControlError::AuthorityMismatch);
        }
        let reverse = manager
            .get_unit_by_invocation_id(&invocation)
            .await
            .map_err(|_| SystemdControlError::Evidence)?;
        let reverse_unit = UnitProxy::builder(&self.connection)
            .path(reverse.clone())
            .map_err(|_| SystemdControlError::Evidence)?
            .build()
            .await
            .map_err(|_| SystemdControlError::Evidence)?;
        if reverse_unit.id().await.map_err(|_| SystemdControlError::Evidence)? != unit_name
            || encode_invocation(
                &reverse_unit.invocation_id().await.map_err(|_| SystemdControlError::Evidence)?,
            )
            .as_deref()
                != Some(invocation_id)
        {
            return Err(SystemdControlError::AuthorityMismatch);
        }
        Ok(reverse)
    }

    async fn read_terminal(
        &self,
        unit_name: &str,
        unit_path: OwnedObjectPath,
    ) -> Result<SystemdTerminalEvidence, SystemdControlError> {
        let manager = self.manager().await.map_err(|_| SystemdControlError::Request)?;
        if !manager
            .get_unit_processes(unit_name)
            .await
            .map_err(|_| SystemdControlError::Evidence)?
            .is_empty()
        {
            return Err(SystemdControlError::Evidence);
        }
        let service = ServiceProxy::builder(&self.connection)
            .path(unit_path)
            .map_err(|_| SystemdControlError::Evidence)?
            .build()
            .await
            .map_err(|_| SystemdControlError::Evidence)?;
        let code = service.exec_main_code().await.map_err(|_| SystemdControlError::Evidence)?;
        let status = service.exec_main_status().await.map_err(|_| SystemdControlError::Evidence)?;
        let result = service.result().await.map_err(|_| SystemdControlError::Evidence)?;
        let started = service
            .exec_main_start_timestamp_monotonic()
            .await
            .map_err(|_| SystemdControlError::Evidence)?;
        let exited = service
            .exec_main_exit_timestamp_monotonic()
            .await
            .map_err(|_| SystemdControlError::Evidence)?;
        let leader_exit = match code {
            libc::CLD_EXITED => LeaderExit::Code(status),
            libc::CLD_KILLED | libc::CLD_DUMPED => LeaderExit::Signal(SignalNumber::new(status)),
            _ => return Err(SystemdControlError::Evidence),
        };
        let elapsed = Duration::from_micros(exited.saturating_sub(started));
        Ok(SystemdTerminalEvidence {
            leader_exit,
            elapsed,
            forced: status == libc::SIGKILL || result == "timeout",
            external_termination: matches!(code, libc::CLD_KILLED | libc::CLD_DUMPED)
                || matches!(result.as_str(), "signal" | "core-dump" | "watchdog" | "oom-kill"),
        })
    }
}

impl SystemdLaunchError {
    fn with_kind(self, kind: Self) -> Self {
        match self {
            recovery @ Self::RecoveryRequired { .. } => recovery,
            Self::UnrecoverableAuthority => Self::UnrecoverableAuthority,
            _ => kind,
        }
    }
}

fn transient_properties(
    spec: &DetachedProcessSpec,
    host: &DetachedHostCapability,
) -> Result<Vec<(&'static str, Value<'static>)>, SystemdLaunchError> {
    let executable = path_string(host.executable())?;
    let arguments = vec![executable.clone(), HOST_MODE.to_string(), path_string(host.path())?];
    let timeout = u64::try_from(spec.termination.grace_period.as_micros())
        .map_err(|_| SystemdLaunchError::Request)?;
    Ok(vec![
        ("Description", Value::new(spec.command.label.as_str().to_string())),
        ("Type", Value::new(String::from("exec"))),
        ("ExitType", Value::new(String::from("cgroup"))),
        ("ExecStart", Value::new(vec![(executable, arguments, false)])),
        ("StandardInput", Value::new(String::from("null"))),
        ("StandardOutput", Value::new(String::from("null"))),
        ("StandardError", Value::new(String::from("null"))),
        ("UMask", Value::new(0o077_u32)),
        ("KillMode", Value::new(String::from("control-group"))),
        ("KillSignal", Value::new(libc::SIGTERM)),
        ("SendSIGKILL", Value::new(true)),
        ("Restart", Value::new(String::from("no"))),
        ("TimeoutStopUSec", Value::new(timeout)),
        ("RemainAfterExit", Value::new(true)),
    ])
}

fn os_string(value: &OsStr) -> Result<String, SystemdLaunchError> {
    value.to_str().map(str::to_owned).ok_or(SystemdLaunchError::Request)
}

fn path_string(value: &Path) -> Result<String, SystemdLaunchError> {
    os_string(value.as_os_str())
}

async fn wait_for_job(
    removed: &mut systemd_zbus::JobRemovedStream,
    expected: &OwnedObjectPath,
) -> Result<String, ()> {
    let wait = async {
        while let Some(signal) = removed.next().await {
            let arguments = signal.args().map_err(|_| ())?;
            if arguments.job == expected.as_ref() {
                return Ok(arguments.result.to_string());
            }
        }
        Err(())
    };
    tokio::time::timeout(JOB_TIMEOUT, wait).await.map_err(|_| ())?
}

async fn wait_for_capability_consumption(path: &Path) -> Result<(), ()> {
    let wait = async {
        loop {
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Ok(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                Err(_) => return Err(()),
            }
        }
    };
    tokio::time::timeout(JOB_TIMEOUT, wait).await.map_err(|_| ())?
}

fn encode_invocation(value: &[u8]) -> Option<String> {
    if value.len() != 16 {
        return None;
    }
    let mut encoded = String::with_capacity(32);
    for byte in value {
        use std::fmt::Write;
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Some(encoded)
}

fn terminal_state(state: ActiveState, sub_state: &str) -> bool {
    matches!(state, ActiveState::Inactive | ActiveState::Failed)
        || (state == ActiveState::Active && sub_state == "exited")
}

fn is_no_such_unit(error: &zbus::Error) -> bool {
    match error {
        zbus::Error::MethodError(name, _, _) => {
            name.as_str() == "org.freedesktop.systemd1.NoSuchUnit"
        }
        _ => false,
    }
}

fn linger_is_enabled() -> bool {
    let Some(user_name) = current_user_name() else {
        return false;
    };
    let path = Path::new("/var/lib/systemd/linger").join(user_name);
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
}

fn current_user_name() -> Option<OsString> {
    let uid = rustix::process::getuid().as_raw();
    let capacity = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let capacity = if capacity <= 0 { 16_384 } else { usize::try_from(capacity).ok()? };
    let mut buffer = vec![0_u8; capacity];
    let mut password = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            password.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    let password = unsafe { password.assume_init() };
    if password.pw_name.is_null() {
        return None;
    }
    let name = unsafe { CStr::from_ptr(password.pw_name) };
    use std::os::unix::ffi::OsStringExt;
    Some(OsString::from_vec(name.to_bytes().to_vec()))
}
