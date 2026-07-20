use std::{process::ExitStatus, sync::Arc};

#[cfg(target_os = "linux")]
use std::{
    io::{Read, Write},
    process::Stdio,
    time::Duration,
};

use processkit::{Mechanism, ProcessGroup, ProcessGroupOptions, Signal};
use tokio::process::{Child, Command};
#[cfg(target_os = "linux")]
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::JoinHandle,
};

use super::super::{ContainmentAvailability, ProcessStartError};
use crate::framework::process::{ContainmentRequirement, ContainmentStrength, TerminationPolicy};

pub(crate) struct AttachedGroup {
    group: Arc<ProcessGroup>,
    strength: ContainmentStrength,
    #[cfg(target_os = "linux")]
    guardian: Option<AttachedGuardian>,
}

#[cfg(target_os = "linux")]
pub(crate) const ATTACHED_GUARD_MODE: &str = "__kit-internal-attached-cgroup-guard";

#[cfg(target_os = "linux")]
const GUARD_READY: u8 = 0x52;
#[cfg(target_os = "linux")]
const GUARD_DISARM: u8 = 0x44;
#[cfg(target_os = "linux")]
const GUARD_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_os = "linux")]
struct AttachedGuardian {
    pid: u32,
    control: tokio::net::UnixStream,
    owner: JoinHandle<Result<ExitStatus, std::io::Error>>,
}

impl AttachedGroup {
    pub(crate) async fn create(
        requirement: ContainmentRequirement,
        termination: TerminationPolicy,
    ) -> Result<Self, ProcessStartError> {
        let options = ProcessGroupOptions::default()
            .shutdown_timeout(termination.grace_period)
            .escalate_to_kill(true);
        let group = ProcessGroup::with_options(options.clone()).map_err(|source| {
            ProcessStartError::ContainmentSetupFailed { message: source.to_string() }
        })?;
        #[cfg(target_os = "linux")]
        let group = if requirement == ContainmentRequirement::CompleteTree
            && group.mechanism() == Mechanism::ProcessGroup
        {
            super::linux_systemd::ensure_attached_delegation()
                .await
                .map_err(|message| ProcessStartError::ContainmentSetupFailed { message })?;
            ProcessGroup::with_options(options).map_err(|source| {
                ProcessStartError::ContainmentSetupFailed { message: source.to_string() }
            })?
        } else {
            group
        };
        let strength = match group.mechanism() {
            Mechanism::CgroupV2 | Mechanism::JobObject => ContainmentStrength::CompleteTree,
            Mechanism::ProcessGroup => ContainmentStrength::ProcessGroup,
            _ => {
                return Err(ProcessStartError::ContainmentUnavailable {
                    required: requirement,
                    available: ContainmentAvailability::Unavailable,
                });
            }
        };
        if requirement == ContainmentRequirement::CompleteTree
            && strength != ContainmentStrength::CompleteTree
        {
            return Err(ProcessStartError::ContainmentUnavailable {
                required: requirement,
                available: ContainmentAvailability::ProcessGroupOnly,
            });
        }
        let group = Arc::new(group);
        #[cfg(target_os = "linux")]
        let guardian = if strength == ContainmentStrength::CompleteTree {
            Some(start_cgroup_guardian(&group).await?)
        } else {
            None
        };
        Ok(Self {
            group,
            strength,
            #[cfg(target_os = "linux")]
            guardian,
        })
    }

    pub(crate) fn spawn(&self, command: Command) -> Result<Child, ProcessStartError> {
        let mut command = command;
        if self.strength == ContainmentStrength::ProcessGroup {
            arm_parent_death(&mut command);
        }
        self.group
            .spawn(command)
            .map_err(|source| ProcessStartError::SpawnFailed { message: source.to_string() })
    }

    pub(crate) fn strength(&self) -> ContainmentStrength {
        self.strength
    }

    pub(crate) fn terminate(&self) -> Result<TerminationRequest, processkit::Error> {
        match self.group.signal(Signal::Term) {
            Ok(()) => Ok(TerminationRequest::Graceful),
            Err(processkit::Error::Unsupported { .. }) => {
                self.group.kill_all()?;
                Ok(TerminationRequest::Forced)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn force_kill(&self) -> Result<(), processkit::Error> {
        self.group.kill_all()
    }

    pub(crate) fn members(&self) -> Result<Vec<u32>, processkit::Error> {
        self.group.members()
    }

    pub(crate) fn target_members(&self) -> Result<Vec<u32>, processkit::Error> {
        let members = self.group.members()?;
        #[cfg(target_os = "linux")]
        {
            let mut members = members;
            if let Some(guardian) = &self.guardian {
                members.retain(|pid| *pid != guardian.pid);
            }
            Ok(members)
        }
        #[cfg(not(target_os = "linux"))]
        Ok(members)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn has_guardian(&self) -> bool {
        self.guardian.is_some()
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn has_guardian(&self) -> bool {
        false
    }

    #[cfg(target_os = "linux")]
    pub(crate) async fn guardian_exited(&mut self) -> Result<ExitStatus, ()> {
        let guardian = self.guardian.as_mut().ok_or(())?;
        (&mut guardian.owner).await.map_err(|_| ())?.map_err(|_| ())
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) async fn guardian_exited(&mut self) -> Result<ExitStatus, ()> {
        std::future::pending().await
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn acknowledge_guardian_exit(&mut self) {
        self.guardian = None;
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn acknowledge_guardian_exit(&mut self) {}

    #[cfg(target_os = "linux")]
    pub(crate) async fn disarm_guardian(&mut self) -> Result<(), ()> {
        let Some(mut guardian) = self.guardian.take() else {
            return Ok(());
        };
        guardian.control.write_all(&[GUARD_DISARM]).await.map_err(|_| ())?;
        guardian.control.shutdown().await.map_err(|_| ())?;
        let status = tokio::time::timeout(GUARD_HANDSHAKE_TIMEOUT, guardian.owner)
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?
            .map_err(|_| ())?;
        status.success().then_some(()).ok_or(())
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) async fn disarm_guardian(&mut self) -> Result<(), ()> {
        Ok(())
    }

    #[cfg(target_os = "linux")]
    pub(crate) async fn reap_guardian_after_kill(&mut self) -> Result<(), ()> {
        let Some(guardian) = self.guardian.take() else {
            return Ok(());
        };
        drop(guardian.control);
        tokio::time::timeout(GUARD_HANDSHAKE_TIMEOUT, guardian.owner)
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?
            .map(|_| ())
            .map_err(|_| ())
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) async fn reap_guardian_after_kill(&mut self) -> Result<(), ()> {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn arm_parent_death(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    let owner_pid = std::process::id();
    // ProcessKit's raw ProcessGroup::spawn seam cannot carry its kill_on_parent_death option.
    // Mirror that option only for the process-group fallback; complete-tree containment has Kit's
    // cgroup guardian, and process_group(0) runs before pre_exec. The parent check closes the race
    // where Kit exits between fork and this hook executing.
    unsafe {
        command.as_std_mut().pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() as u32 != owner_pid {
                libc::_exit(0);
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn arm_parent_death(_command: &mut Command) {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminationRequest {
    Graceful,
    Forced,
}

#[cfg(target_os = "linux")]
async fn start_cgroup_guardian(
    group: &Arc<ProcessGroup>,
) -> Result<AttachedGuardian, ProcessStartError> {
    let executable = std::env::current_exe().map_err(|source| {
        ProcessStartError::ContainmentSetupFailed { message: source.to_string() }
    })?;
    let (parent, child) = std::os::unix::net::UnixStream::pair().map_err(|source| {
        ProcessStartError::ContainmentSetupFailed { message: source.to_string() }
    })?;
    parent.set_nonblocking(true).map_err(|source| ProcessStartError::ContainmentSetupFailed {
        message: source.to_string(),
    })?;

    let mut command = Command::new(executable);
    command
        .arg(ATTACHED_GUARD_MODE)
        .arg(std::process::id().to_string())
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(child)))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = group.spawn(command).map_err(|source| {
        ProcessStartError::ContainmentSetupFailed { message: source.to_string() }
    })?;
    let pid = child.id().ok_or_else(|| ProcessStartError::ContainmentSetupFailed {
        message: String::from("attached cgroup guardian has no process identity"),
    })?;
    let owner = tokio::spawn(async move { child.wait().await });
    let mut control = tokio::net::UnixStream::from_std(parent).map_err(|source| {
        ProcessStartError::ContainmentSetupFailed { message: source.to_string() }
    })?;
    let mut ready = [0_u8; 1];
    match tokio::time::timeout(GUARD_HANDSHAKE_TIMEOUT, control.read_exact(&mut ready)).await {
        Ok(Ok(_)) if ready == [GUARD_READY] => Ok(AttachedGuardian { pid, control, owner }),
        _ => {
            let _ = group.kill_all();
            let _ = tokio::time::timeout(GUARD_HANDSHAKE_TIMEOUT, owner).await;
            Err(ProcessStartError::ContainmentSetupFailed {
                message: String::from("attached cgroup guardian failed its readiness handshake"),
            })
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn run_attached_guard_entry(owner_pid: &std::ffi::OsStr) -> i32 {
    let Some(owner_pid) = owner_pid.to_str().and_then(|value| value.parse::<u32>().ok()) else {
        return 125;
    };
    let current_parent =
        rustix::process::getppid().and_then(|pid| u32::try_from(pid.as_raw_pid()).ok());
    if current_parent != Some(owner_pid) {
        return 125;
    }
    // The guardian must outlive graceful signals sent to the payload cgroup. It exits only when the
    // attached owner explicitly disarms it, or atomically kills the cgroup when that owner dies.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    let Some(mut cgroup_kill) = open_own_cgroup_kill(owner_pid) else {
        return 125;
    };
    use std::os::fd::FromRawFd;

    // SAFETY: the guardian is launched with its private full-duplex control socket as descriptor
    // zero. This process takes sole ownership of that descriptor and never constructs `Stdin`.
    let mut control = unsafe { std::os::unix::net::UnixStream::from_raw_fd(libc::STDIN_FILENO) };
    if control.write_all(&[GUARD_READY]).is_err() || control.flush().is_err() {
        return 125;
    }
    let mut instruction = [0_u8; 1];
    match control.read(&mut instruction) {
        Ok(1) if instruction == [GUARD_DISARM] => 0,
        Ok(0) | Ok(1) | Err(_) => {
            let _ = cgroup_kill.write_all(b"1");
            let _ = cgroup_kill.flush();
            125
        }
        Ok(_) => unreachable!("one-byte guardian read cannot exceed its buffer"),
    }
}

#[cfg(target_os = "linux")]
fn open_own_cgroup_kill(owner_pid: u32) -> Option<std::fs::File> {
    let membership = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative =
        membership.lines().find_map(|line| line.strip_prefix("0::"))?.trim_start_matches('/');
    if relative.is_empty() {
        return None;
    }
    let cgroup_name = std::path::Path::new(relative).file_name()?.to_str()?;
    if !cgroup_name.starts_with(format!("processkit-{owner_pid}-").as_str()) {
        return None;
    }
    ["/sys/fs/cgroup", "/sys/fs/cgroup/unified"].into_iter().find_map(|root| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(std::path::Path::new(root).join(relative).join("cgroup.kill"))
            .ok()
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{
        path::Path,
        process::{Command as StdCommand, Stdio},
        time::{Duration, Instant},
    };

    use tokio::process::Command;
    use uuid::Uuid;

    use super::AttachedGroup;
    use crate::framework::process::{ContainmentRequirement, TerminationPolicy};

    const HELPER_PID_FILE: &str = "KIT_ATTACHED_PARENT_DEATH_PID_FILE";

    #[test]
    #[ignore = "re-executed by attached_child_dies_when_its_kit_owner_is_killed"]
    fn attached_parent_death_helper() {
        let Some(pid_file) = std::env::var_os(HELPER_PID_FILE) else { return };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async move {
            let group = AttachedGroup::create(
                ContainmentRequirement::ExplicitProcessGroup,
                TerminationPolicy::new(Duration::from_millis(100)),
            )
            .await
            .unwrap();
            let mut command = Command::new("sleep");
            command.arg("60").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            let child = group.spawn(command).unwrap();
            std::fs::write(pid_file, child.id().unwrap().to_string()).unwrap();

            std::future::pending::<()>().await;
            drop((child, group));
        });
    }

    #[test]
    fn attached_child_dies_when_its_kit_owner_is_killed() {
        let directory =
            std::env::temp_dir().join(format!("kit-attached-parent-death-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let pid_file = directory.join("child.pid");
        let mut owner = StdCommand::new(std::env::current_exe().unwrap())
            .arg("attached_parent_death_helper")
            .arg("--ignored")
            .arg("--nocapture")
            .env(HELPER_PID_FILE, &pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        if !wait_until(Duration::from_secs(5), || pid_file.is_file()) {
            let _ = owner.kill();
            let _ = owner.wait();
            panic!("attached owner did not publish its child pid");
        }
        let child_pid = std::fs::read_to_string(&pid_file).unwrap().trim().parse::<u32>().unwrap();
        owner.kill().unwrap();
        owner.wait().unwrap();

        let child_died = wait_until(Duration::from_secs(5), || process_is_dead(child_pid));
        if !child_died {
            unsafe {
                libc::kill(child_pid as libc::pid_t, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_dir_all(directory);
        assert!(child_died, "attached child {child_pid} survived its Kit owner");
    }

    fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        predicate()
    }

    fn process_is_dead(pid: u32) -> bool {
        let path = format!("/proc/{pid}/stat");
        let Ok(stat) = std::fs::read_to_string(Path::new(&path)) else { return true };
        stat.rsplit_once(") ")
            .and_then(|(_, fields)| fields.chars().next())
            .is_some_and(|state| state == 'Z')
    }
}
