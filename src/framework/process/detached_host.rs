use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::OpenOptions,
    io::{Read, Write},
    num::NonZeroU64,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::{process::Command, task::JoinHandle};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::framework::{process::ProcessEnvironment, AtomicFileWriter};

use super::{
    output::{
        create_private_record_file, drain_record, RecordAvailability, RecordDisposition,
        RecordDrainEvidence,
    },
    receipt::{
        systemd_unit_name, DetachedCommitGrant, DetachedLaunchRecovery,
        PersistedDetachedLaunchIntent,
    },
    report::{
        LeaderExitObservation, PersistedDetachedInfrastructureFailure, PersistedDetachedOutput,
        PersistedDetachedTarget, PersistedDetachedTerminal,
    },
    spec::{RecordLimit, RecordOverflow},
    DetachedOutputPolicy, DetachedProcessSpec, EnvironmentBase, LeaderExit, ProcessRunId,
    SignalNumber,
};

pub(crate) const HOST_MODE: &str = "__kit-internal-detached-io-host";
pub(crate) const COMMIT_FILE: &str = "detached-commit.json";
pub(crate) const TERMINAL_FILE: &str = "detached-terminal.json";
pub(crate) const STDOUT_FILE: &str = "stdout.bin";
pub(crate) const STDERR_FILE: &str = "stderr.bin";

const MAX_HOST_SPEC_BYTES: u64 = 1024 * 1024;
const MAX_FINAL_TAIL_BYTES: usize = 64 * 1024;
pub(crate) const HOST_FAILURE_EXIT: i32 = 125;
const LAUNCH_INTENT_FILE: &str = "detached-launch.json";
const COMMIT_RETRY: Duration = Duration::from_millis(25);

#[cfg(test)]
extern "C" fn route_internal_process_mode_under_libtest() {
    let Some(mode) = std::env::args_os().nth(1) else {
        return;
    };
    if mode != OsStr::new(HOST_MODE)
        && mode != OsStr::new(super::platform::attached::ATTACHED_GUARD_MODE)
    {
        return;
    }

    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        std::process::exit(HOST_FAILURE_EXIT);
    };
    let code = runtime.block_on(run_detached_io_host_entry()).unwrap_or(HOST_FAILURE_EXIT);
    std::process::exit(code);
}

// Internal process owners re-execute the current binary. Unit tests run under libtest rather than
// `main`, so the framework must claim its two exact private modes before libtest consumes them.
#[cfg(test)]
#[used]
#[unsafe(link_section = ".init_array")]
static LIBTEST_PROCESS_HOST_ENTRY: extern "C" fn() = route_internal_process_mode_under_libtest;

#[derive(Serialize, Deserialize)]
#[serde(tag = "version", rename_all = "kebab-case", deny_unknown_fields)]
enum HostSpecEnvelope {
    V1 {
        run_id: ProcessRunId,
        commit_nonce: Uuid,
        command: StoredCommand,
        stdout: StoredOutputPolicy,
        stderr: StoredOutputPolicy,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "version", rename_all = "kebab-case", deny_unknown_fields)]
enum CommitEnvelope {
    V1 { run_id: ProcessRunId, nonce: Uuid },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCommand {
    program: Vec<u8>,
    arguments: Vec<Vec<u8>>,
    working_directory: Vec<u8>,
    environment: Vec<(Vec<u8>, Vec<u8>)>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum StoredOutputPolicy {
    Discarded,
    Recorded { limit: NonZeroU64 },
}

impl StoredOutputPolicy {
    pub(crate) fn from_public(policy: DetachedOutputPolicy) -> Self {
        match policy {
            DetachedOutputPolicy::Discard => Self::Discarded,
            DetachedOutputPolicy::Record(policy) => Self::Recorded { limit: policy.limit },
        }
    }
}

pub(crate) struct DetachedHostCapability {
    executable: PathBuf,
    path: PathBuf,
    commit: Option<DetachedCommitGrant>,
}

impl DetachedHostCapability {
    pub(crate) fn prepare(
        run_id: ProcessRunId,
        run_dir: &Path,
        spec: &DetachedProcessSpec,
    ) -> Result<Self, ()> {
        let environment = materialize_environment(&spec.command.environment)?;
        let executable = resolve_executable(&spec.command.program, &environment)?;
        if spec.command.arguments.iter().any(|argument| argument.as_bytes().contains(&0)) {
            return Err(());
        }
        let command = StoredCommand {
            program: executable.as_os_str().as_bytes().to_vec(),
            arguments: spec
                .command
                .arguments
                .iter()
                .map(|argument| argument.as_bytes().to_vec())
                .collect(),
            working_directory: spec.command.working_directory.as_os_str().as_bytes().to_vec(),
            environment: environment
                .into_iter()
                .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
                .collect(),
        };
        let commit_nonce = Uuid::new_v4();
        let envelope = HostSpecEnvelope::V1 {
            run_id,
            commit_nonce,
            command,
            stdout: StoredOutputPolicy::from_public(spec.stdout),
            stderr: StoredOutputPolicy::from_public(spec.stderr),
        };
        let bytes = Zeroizing::new(serde_json::to_vec(&envelope).map_err(|_| ())?);
        if bytes.len() as u64 > MAX_HOST_SPEC_BYTES {
            return Err(());
        }
        let capability_path =
            run_dir.join(format!(".detached-host-{}.json", Uuid::new_v4().simple()));
        write_capability(&capability_path, bytes.as_slice())?;
        let host_executable = resolve_host_executable()?;
        if !host_executable.is_absolute() || !executable_file(&host_executable) {
            let _ = std::fs::remove_file(&capability_path);
            return Err(());
        }
        let commit_path = run_dir.join(COMMIT_FILE);
        let commit = serde_json::to_vec(&CommitEnvelope::V1 { run_id, nonce: commit_nonce })
            .map_err(|_| ())?
            .into_boxed_slice();
        Ok(Self {
            executable: host_executable,
            path: capability_path,
            commit: Some(DetachedCommitGrant::new(run_dir.to_path_buf(), commit_path, commit)),
        })
    }

    pub(crate) fn executable(&self) -> &Path {
        &self.executable
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn into_commit_grant(mut self) -> DetachedCommitGrant {
        self.commit.take().expect("detached host capability has one commit grant")
    }
}

fn resolve_host_executable() -> Result<PathBuf, ()> {
    let current = std::env::current_exe().map_err(|_| ())?;
    (current.is_absolute() && executable_file(&current)).then_some(current).ok_or(())
}

impl Drop for DetachedHostCapability {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[doc(hidden)]
pub async fn run_detached_io_host_entry() -> Option<i32> {
    let mut arguments = std::env::args_os();
    let _executable = arguments.next();
    let mode = arguments.next()?;
    if mode == OsStr::new(super::platform::attached::ATTACHED_GUARD_MODE) {
        let Some(owner_pid) = arguments.next() else {
            return Some(HOST_FAILURE_EXIT);
        };
        if arguments.next().is_some() {
            return Some(HOST_FAILURE_EXIT);
        }
        return Some(super::platform::attached::run_attached_guard_entry(&owner_pid));
    }
    if mode != OsStr::new(HOST_MODE) {
        return None;
    }
    let Some(capability) = arguments.next() else {
        return Some(HOST_FAILURE_EXIT);
    };
    if arguments.next().is_some() {
        return Some(HOST_FAILURE_EXIT);
    }
    let outcome = run_host(PathBuf::from(capability)).await;
    Some(match outcome {
        Ok(HostExit::Code(code)) => code,
        Ok(HostExit::Signal(signal)) => mirror_signal(signal),
        Err(()) => HOST_FAILURE_EXIT,
    })
}

enum HostExit {
    Code(i32),
    Signal(i32),
}

async fn run_host(capability: PathBuf) -> Result<HostExit, ()> {
    let spec = read_and_remove_capability(&capability)?;
    let (run_id, commit_nonce, command, stdout_policy, stderr_policy) = match spec {
        HostSpecEnvelope::V1 { run_id, commit_nonce, command, stdout, stderr } => {
            (run_id, commit_nonce, command, stdout, stderr)
        }
    };
    let run_dir = capability.parent().ok_or(())?;
    if run_dir.file_name().and_then(OsStr::to_str) != Some(run_id.to_string().as_str()) {
        return Err(());
    }
    validate_private_directory(run_dir)?;
    // Register before observing the commit so rollback cannot race the signal guard. An
    // uncommitted host exits on SIGTERM; a committed host keeps the guard alive while systemd sends
    // the same SIGTERM to the payload cgroup and the host drains/publishes its terminal evidence.
    let mut termination_guard =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|_| ())?;
    publish_host_authority(run_dir, run_id)?;
    let commit = wait_for_commit(run_dir, run_id, commit_nonce, &mut termination_guard).await?;
    // systemd owns cgroup termination and sends SIGTERM to every member. Keep the host alive for
    // the graceful phase so the payload receives SIGTERM while its owner can still wait, drain both
    // streams, and publish exact terminal evidence. A forced cgroup SIGKILL remains unhandled and
    // therefore cannot be mistaken for a completed payload.
    let (terminal, exit) = run_committed_payload(
        run_dir,
        run_id,
        command,
        stdout_policy,
        stderr_policy,
        commit,
        &mut termination_guard,
    )
    .await?;
    publish_terminal(run_dir, &terminal)?;
    drop(termination_guard);
    Ok(exit)
}

async fn run_committed_payload(
    run_dir: &Path,
    run_id: ProcessRunId,
    command: StoredCommand,
    stdout_policy: StoredOutputPolicy,
    stderr_policy: StoredOutputPolicy,
    commit: CommitWaitOutcome,
    termination: &mut tokio::signal::unix::Signal,
) -> Result<(PersistedDetachedTerminal, HostExit), ()> {
    let started = Instant::now();
    let stdout_path = run_dir.join(STDOUT_FILE);
    let stderr_path = run_dir.join(STDERR_FILE);
    let stdout = match prepare_output(stdout_policy, &stdout_path) {
        Ok(stdout) => stdout,
        Err(()) => {
            return infrastructure_failure(InfrastructureFailureContext {
                run_id,
                leader_exit: LeaderExitObservation::NotObserved,
                stdout_policy,
                stderr_policy,
                stdout_path: &stdout_path,
                stderr_path: &stderr_path,
                stdout: None,
                stderr: None,
                elapsed: started.elapsed(),
            })
        }
    };
    let stderr = match prepare_output(stderr_policy, &stderr_path) {
        Ok(stderr) => stderr,
        Err(()) => {
            return infrastructure_failure(InfrastructureFailureContext {
                run_id,
                leader_exit: LeaderExitObservation::NotObserved,
                stdout_policy,
                stderr_policy,
                stdout_path: &stdout_path,
                stderr_path: &stderr_path,
                stdout: None,
                stderr: None,
                elapsed: started.elapsed(),
            })
        }
    };
    let mut process = Command::new(OsString::from_vec(command.program));
    process
        .args(command.arguments.into_iter().map(OsString::from_vec))
        .current_dir(OsString::from_vec(command.working_directory))
        .env_clear()
        .envs(
            command
                .environment
                .into_iter()
                .map(|(name, value)| (OsString::from_vec(name), OsString::from_vec(value))),
        )
        .stdin(Stdio::null())
        .stdout(stdout.stdio())
        .stderr(stderr.stdio());

    // If StopUnit won before target creation, do not create a payload after systemd already sent the
    // cgroup SIGTERM.
    if commit == CommitWaitOutcome::TerminationAfterCommit || termination_pending(termination).await
    {
        return completed_before_spawn(run_id, stdout, stderr, started.elapsed());
    }
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(_) => {
            return infrastructure_failure(InfrastructureFailureContext {
                run_id,
                leader_exit: LeaderExitObservation::NotObserved,
                stdout_policy,
                stderr_policy,
                stdout_path: &stdout_path,
                stderr_path: &stderr_path,
                stdout: None,
                stderr: None,
                elapsed: started.elapsed(),
            })
        }
    };
    // Close the only remaining race: StopUnit may have signalled the cgroup after the pre-spawn
    // check but before this child joined it. A queued host notification proves that ordering is
    // possible, so explicitly deliver the same graceful signal to the new leader. That delivery may
    // be redundant when the child joined just before systemd signalled; if TERM arrives after this
    // check, systemd reaches the child directly through the cgroup. SIGKILL remains systemd-owned.
    if termination_pending(termination).await && terminate_payload_leader(&child).is_err() {
        let _ = child.start_kill();
        let leader_exit = child
            .wait()
            .await
            .ok()
            .map(leader_exit)
            .map(LeaderExitObservation::Observed)
            .unwrap_or(LeaderExitObservation::NotObserved);
        return infrastructure_failure(InfrastructureFailureContext {
            run_id,
            leader_exit,
            stdout_policy,
            stderr_policy,
            stdout_path: &stdout_path,
            stderr_path: &stderr_path,
            stdout: None,
            stderr: None,
            elapsed: started.elapsed(),
        });
    }
    if stdout.requires_pipe() && child.stdout.is_none()
        || stderr.requires_pipe() && child.stderr.is_none()
    {
        let _ = child.start_kill();
        let leader_exit = child
            .wait()
            .await
            .ok()
            .map(leader_exit)
            .map(LeaderExitObservation::Observed)
            .unwrap_or(LeaderExitObservation::NotObserved);
        return infrastructure_failure(InfrastructureFailureContext {
            run_id,
            leader_exit,
            stdout_policy,
            stderr_policy,
            stdout_path: &stdout_path,
            stderr_path: &stderr_path,
            stdout: None,
            stderr: None,
            elapsed: started.elapsed(),
        });
    }
    let stdout = stdout.start(child.stdout.take());
    let stderr = stderr.start_stderr(child.stderr.take());
    let status = child.wait().await;
    let (leader_exit, child_wait_succeeded) = match status {
        Ok(status) => (LeaderExitObservation::Observed(leader_exit(status)), true),
        Err(_) => {
            let _ = child.start_kill();
            let leader_exit = child
                .wait()
                .await
                .ok()
                .map(leader_exit)
                .map(LeaderExitObservation::Observed)
                .unwrap_or(LeaderExitObservation::NotObserved);
            (leader_exit, false)
        }
    };
    let (stdout, stderr) = tokio::join!(stdout.finish(), stderr.finish());
    let elapsed = started.elapsed();
    let (stdout, stderr) = match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) if child_wait_succeeded => (stdout, stderr),
        (stdout, stderr) => {
            return infrastructure_failure(InfrastructureFailureContext {
                run_id,
                leader_exit,
                stdout_policy,
                stderr_policy,
                stdout_path: &stdout_path,
                stderr_path: &stderr_path,
                stdout: stdout.ok(),
                stderr: stderr.ok(),
                elapsed,
            })
        }
    };
    let LeaderExitObservation::Observed(target_exit) = leader_exit else {
        unreachable!("a successful child wait records its exit")
    };
    let terminal = PersistedDetachedTarget {
        run_id,
        leader_exit: LeaderExitObservation::Observed(target_exit),
        stdout,
        stderr,
        elapsed_micros: duration_micros(elapsed),
    };
    let exit = match target_exit {
        LeaderExit::Code(code) => HostExit::Code(code),
        LeaderExit::Signal(signal) => HostExit::Signal(signal.get()),
    };
    Ok((PersistedDetachedTerminal::Completed(terminal), exit))
}

fn completed_before_spawn(
    run_id: ProcessRunId,
    stdout: PreparedOutput,
    stderr: PreparedOutput,
    elapsed: Duration,
) -> Result<(PersistedDetachedTerminal, HostExit), ()> {
    let terminal = PersistedDetachedTarget {
        run_id,
        leader_exit: LeaderExitObservation::NotObserved,
        stdout: stdout.complete_empty()?,
        stderr: stderr.complete_empty()?,
        elapsed_micros: duration_micros(elapsed),
    };
    Ok((PersistedDetachedTerminal::Completed(terminal), HostExit::Code(0)))
}

fn terminate_payload_leader(child: &tokio::process::Child) -> Result<(), ()> {
    let pid = i32::try_from(child.id().ok_or(())?).map_err(|_| ())?;
    let result = unsafe { libc::kill(pid, libc::SIGTERM) };
    if result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(())
    }
}

struct InfrastructureFailureContext<'a> {
    run_id: ProcessRunId,
    leader_exit: LeaderExitObservation,
    stdout_policy: StoredOutputPolicy,
    stderr_policy: StoredOutputPolicy,
    stdout_path: &'a Path,
    stderr_path: &'a Path,
    stdout: Option<PersistedDetachedOutput>,
    stderr: Option<PersistedDetachedOutput>,
    elapsed: Duration,
}

fn infrastructure_failure(
    context: InfrastructureFailureContext<'_>,
) -> Result<(PersistedDetachedTerminal, HostExit), ()> {
    let InfrastructureFailureContext {
        run_id,
        leader_exit,
        stdout_policy,
        stderr_policy,
        stdout_path,
        stderr_path,
        stdout,
        stderr,
        elapsed,
    } = context;
    let failure = PersistedDetachedInfrastructureFailure {
        run_id,
        leader_exit,
        stdout: match stdout {
            Some(stdout) => stdout,
            None => interrupted_output(stdout_policy, stdout_path)?,
        },
        stderr: match stderr {
            Some(stderr) => stderr,
            None => interrupted_output(stderr_policy, stderr_path)?,
        },
        elapsed_micros: duration_micros(elapsed),
    };
    Ok((
        PersistedDetachedTerminal::InfrastructureFailure(failure),
        HostExit::Code(HOST_FAILURE_EXIT),
    ))
}

enum PreparedOutput {
    Discarded,
    Recorded { file: std::fs::File, limit: NonZeroU64 },
}

impl PreparedOutput {
    fn stdio(&self) -> Stdio {
        match self {
            Self::Discarded => Stdio::null(),
            Self::Recorded { .. } => Stdio::piped(),
        }
    }

    fn requires_pipe(&self) -> bool {
        matches!(self, Self::Recorded { .. })
    }

    fn complete_empty(self) -> Result<PersistedDetachedOutput, ()> {
        match self {
            Self::Discarded => Ok(PersistedDetachedOutput::Discarded),
            Self::Recorded { file, .. } => {
                let metadata = file.metadata().map_err(|_| ())?;
                if !metadata.file_type().is_file()
                    || metadata.uid() != rustix::process::getuid().as_raw()
                    || metadata.mode() & 0o077 != 0
                    || metadata.len() != 0
                {
                    return Err(());
                }
                Ok(PersistedDetachedOutput::Recorded {
                    observed_bytes: 0,
                    retained_bytes: 0,
                    disposition: RecordDisposition::Complete,
                    availability: RecordAvailability::Available,
                    final_tail: Vec::new(),
                })
            }
        }
    }

    fn start(self, reader: Option<tokio::process::ChildStdout>) -> HostOutputOwner {
        match self {
            Self::Discarded => HostOutputOwner::Discarded,
            Self::Recorded { file, limit } => {
                let reader = reader.expect("record pipe was validated after host target spawn");
                HostOutputOwner::Recorded(tokio::spawn(async move {
                    drain_record(
                        reader,
                        tokio::fs::File::from_std(file),
                        RecordLimit::Bytes(limit),
                        usize::try_from(limit.get())
                            .unwrap_or(usize::MAX)
                            .min(MAX_FINAL_TAIL_BYTES),
                        RecordOverflow::DrainWithTruncationEvidence,
                        |_| {},
                        |_| {},
                    )
                    .await
                }))
            }
        }
    }
}

// ChildStdout and ChildStderr are distinct types, so stderr has the matching narrow adapter.
impl PreparedOutput {
    fn start_stderr(self, reader: Option<tokio::process::ChildStderr>) -> HostOutputOwner {
        match self {
            Self::Discarded => HostOutputOwner::Discarded,
            Self::Recorded { file, limit } => {
                let reader = reader.expect("record pipe was validated after host target spawn");
                HostOutputOwner::Recorded(tokio::spawn(async move {
                    drain_record(
                        reader,
                        tokio::fs::File::from_std(file),
                        RecordLimit::Bytes(limit),
                        usize::try_from(limit.get())
                            .unwrap_or(usize::MAX)
                            .min(MAX_FINAL_TAIL_BYTES),
                        RecordOverflow::DrainWithTruncationEvidence,
                        |_| {},
                        |_| {},
                    )
                    .await
                }))
            }
        }
    }
}

enum HostOutputOwner {
    Discarded,
    Recorded(JoinHandle<RecordDrainEvidence>),
}

impl HostOutputOwner {
    async fn finish(self) -> Result<PersistedDetachedOutput, ()> {
        match self {
            Self::Discarded => Ok(PersistedDetachedOutput::Discarded),
            Self::Recorded(task) => {
                let evidence = task.await.map_err(|_| ())?;
                Ok(PersistedDetachedOutput::Recorded {
                    observed_bytes: evidence.observed_bytes,
                    retained_bytes: evidence.retained_bytes,
                    disposition: evidence.disposition,
                    availability: evidence.availability,
                    final_tail: evidence.final_tail.into_vec(),
                })
            }
        }
    }
}

fn prepare_output(policy: StoredOutputPolicy, path: &Path) -> Result<PreparedOutput, ()> {
    match policy {
        StoredOutputPolicy::Discarded => Ok(PreparedOutput::Discarded),
        StoredOutputPolicy::Recorded { limit } => {
            Ok(PreparedOutput::Recorded { file: open_private_record_file(path)?, limit })
        }
    }
}

fn open_private_record_file(path: &Path) -> Result<std::fs::File, ()> {
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| ())?;
    let metadata = file.metadata().map_err(|_| ())?;
    if !metadata.file_type().is_file()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.len() != 0
    {
        return Err(());
    }
    Ok(file)
}

pub(crate) fn prepare_record_files(
    run_dir: &Path,
    stdout: DetachedOutputPolicy,
    stderr: DetachedOutputPolicy,
) -> Result<(), ()> {
    for (policy, name) in [(stdout, STDOUT_FILE), (stderr, STDERR_FILE)] {
        if matches!(policy, DetachedOutputPolicy::Record(_)) {
            create_private_record_file(&run_dir.join(name)).map_err(|_| ())?;
        }
    }
    Ok(())
}

fn read_and_remove_capability(path: &Path) -> Result<HostSpecEnvelope, ()> {
    if !path.is_absolute()
        || !path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with(".detached-host-") && name.ends_with(".json"))
    {
        return Err(());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| ())?;
    let metadata = file.metadata().map_err(|_| ())?;
    if !metadata.file_type().is_file()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_HOST_SPEC_BYTES
    {
        return Err(());
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    file.take(MAX_HOST_SPEC_BYTES + 1).read_to_end(&mut bytes).map_err(|_| ())?;
    let parsed = serde_json::from_slice(bytes.as_slice()).map_err(|_| ());
    let parent = path.parent().ok_or(())?;
    let removed = std::fs::remove_file(path)
        .and_then(|()| std::fs::File::open(parent)?.sync_all())
        .map_err(|_| ());
    removed?;
    parsed
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitWaitOutcome {
    Committed,
    TerminationAfterCommit,
}

async fn wait_for_commit(
    run_dir: &Path,
    run_id: ProcessRunId,
    nonce: Uuid,
    termination: &mut tokio::signal::unix::Signal,
) -> Result<CommitWaitOutcome, ()> {
    loop {
        match read_commit(&run_dir.join(COMMIT_FILE)) {
            Ok(CommitEnvelope::V1 { run_id: committed_run, nonce: committed_nonce })
                if committed_run == run_id && committed_nonce == nonce =>
            {
                return Ok(CommitWaitOutcome::Committed);
            }
            Ok(_) => return Err(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tokio::select! {
                    _ = termination.recv() => {
                        return match read_commit(&run_dir.join(COMMIT_FILE)) {
                            Ok(CommitEnvelope::V1 {
                                run_id: committed_run,
                                nonce: committed_nonce,
                            }) if committed_run == run_id && committed_nonce == nonce => {
                                Ok(CommitWaitOutcome::TerminationAfterCommit)
                            }
                            Ok(_) | Err(_) => Err(()),
                        };
                    }
                    _ = tokio::time::sleep(COMMIT_RETRY) => {}
                }
            }
            Err(_) => return Err(()),
        }
    }
}

async fn termination_pending(termination: &mut tokio::signal::unix::Signal) -> bool {
    tokio::select! {
        biased;
        _ = termination.recv() => true,
        _ = std::future::ready(()) => false,
    }
}

fn read_commit(path: &Path) -> Result<CommitEnvelope, std::io::Error> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_HOST_SPEC_BYTES
    {
        return Err(std::io::Error::other("invalid detached commit capability"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_HOST_SPEC_BYTES + 1).read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map_err(|_| std::io::Error::other("invalid detached commit capability"))
}

fn write_capability(path: &Path, bytes: &[u8]) -> Result<(), ()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(path).map_err(|_| ())?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all()).and_then(|()| {
        std::fs::File::open(path.parent().ok_or(std::io::ErrorKind::InvalidInput)?)?.sync_all()
    });
    if result.is_err() {
        let _ = std::fs::remove_file(path);
        return Err(());
    }
    Ok(())
}

fn publish_terminal(run_dir: &Path, terminal: &PersistedDetachedTerminal) -> Result<(), ()> {
    let writer = AtomicFileWriter::new(run_dir, "detached.lock", ".detached-target");
    let bytes = Zeroizing::new(serde_json::to_vec(terminal).map_err(|_| ())?);
    if bytes.len() as u64 > MAX_HOST_SPEC_BYTES {
        return Err(());
    }
    writer.replace(&run_dir.join(TERMINAL_FILE), bytes.as_slice()).map_err(|_| ())
}

fn interrupted_output(
    policy: StoredOutputPolicy,
    path: &Path,
) -> Result<PersistedDetachedOutput, ()> {
    match policy {
        StoredOutputPolicy::Discarded => Ok(PersistedDetachedOutput::Discarded),
        StoredOutputPolicy::Recorded { limit } => {
            let metadata = std::fs::symlink_metadata(path).map_err(|_| ())?;
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.uid() != rustix::process::getuid().as_raw()
                || metadata.mode() & 0o077 != 0
            {
                return Err(());
            }
            let retained_bytes = metadata.len();
            if retained_bytes > limit.get() {
                return Err(());
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

fn duration_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn publish_host_authority(run_dir: &Path, run_id: ProcessRunId) -> Result<(), ()> {
    let invocation_id = std::env::var("INVOCATION_ID").map_err(|_| ())?;
    let recovery =
        DetachedLaunchRecovery::linux_systemd(run_id, systemd_unit_name(run_id), invocation_id)
            .map_err(|_| ())?;
    let intent = PersistedDetachedLaunchIntent::Authority { recovery: recovery.encode() };
    let bytes = Zeroizing::new(serde_json::to_vec(&intent).map_err(|_| ())?);
    let writer = AtomicFileWriter::new(run_dir, "detached.lock", ".detached-launch");
    writer.replace(&run_dir.join(LAUNCH_INTENT_FILE), bytes.as_slice()).map_err(|_| ())
}

fn materialize_environment(
    environment: &ProcessEnvironment,
) -> Result<BTreeMap<OsString, OsString>, ()> {
    let mut values = BTreeMap::new();
    if environment.base == EnvironmentBase::Inherit {
        values.extend(std::env::vars_os());
    }
    for name in &environment.removals {
        values.remove(name);
    }
    values.extend(environment.values.clone());
    if values.iter().any(|(name, value)| {
        name.is_empty()
            || name.as_bytes().contains(&b'=')
            || name.as_bytes().contains(&0)
            || value.as_bytes().contains(&0)
    }) {
        return Err(());
    }
    Ok(values)
}

fn resolve_executable(
    program: &OsStr,
    environment: &BTreeMap<OsString, OsString>,
) -> Result<PathBuf, ()> {
    let program_path = Path::new(program);
    if program_path.is_absolute() {
        return executable_file(program_path).then(|| program_path.to_path_buf()).ok_or(());
    }
    if program_path.components().count() != 1 {
        return Err(());
    }
    let path_value = environment.get(OsStr::new("PATH")).ok_or(())?;
    for directory in std::env::split_paths(path_value) {
        let candidate = directory.join(program);
        if candidate.is_absolute() && executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    Err(())
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    metadata.is_file() && metadata.mode() & 0o111 != 0
}

fn validate_private_directory(path: &Path) -> Result<(), ()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ())?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(());
    }
    Ok(())
}

fn leader_exit(status: std::process::ExitStatus) -> LeaderExit {
    if let Some(code) = status.code() {
        LeaderExit::Code(code)
    } else {
        LeaderExit::Signal(SignalNumber::new(status.signal().unwrap_or(libc::SIGKILL)))
    }
}

fn mirror_signal(signal: i32) -> i32 {
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
    128_i32.saturating_add(signal)
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::{HOST_FAILURE_EXIT, HOST_MODE};

    fn assert_libtest_dispatches(mode: &str) {
        let status = Command::new(std::env::current_exe().expect("resolve libtest executable"))
            .arg(mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("re-execute libtest process host");
        assert_eq!(status.code(), Some(HOST_FAILURE_EXIT));
    }

    #[test]
    fn libtest_dispatches_attached_guard_entry() {
        assert_libtest_dispatches(super::super::platform::attached::ATTACHED_GUARD_MODE);
    }

    #[test]
    fn libtest_dispatches_detached_host_entry() {
        assert_libtest_dispatches(HOST_MODE);
    }
}
