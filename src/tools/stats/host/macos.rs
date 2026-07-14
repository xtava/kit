use std::io;
use std::mem::{size_of, MaybeUninit};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use super::{ActionError, ProcessAction, ProcessObservation, TaskBatch, TaskReadFailure, TaskStat};
use crate::tools::stats::model::{
    DetailUnavailable, Observed, ProcessKey, ProcessState, ResourceSample,
};

const PROC_PIDTBSDINFO: libc::c_int = 3;
const PROC_PIDTASKINFO: libc::c_int = 4;
const PROC_PIDTHREADINFO: libc::c_int = 5;
const PROC_PIDLISTTHREADS: libc::c_int = 6;
const PROC_PIDLISTFDS: libc::c_int = 1;
const PATH_CAPACITY: usize = 4_096;
const THREAD_LIST_SLACK: usize = 16;
const THREAD_LIST_RETRIES: usize = 3;

#[repr(C)]
struct ProcBsdInfo {
    pbi_flags: u32,
    pbi_status: u32,
    pbi_xstatus: u32,
    pbi_pid: u32,
    pbi_ppid: u32,
    pbi_uid: u32,
    pbi_gid: u32,
    pbi_ruid: u32,
    pbi_rgid: u32,
    pbi_svuid: u32,
    pbi_svgid: u32,
    rfu_1: u32,
    pbi_comm: [libc::c_char; 16],
    pbi_name: [libc::c_char; 32],
    pbi_nfiles: u32,
    pbi_pgid: u32,
    pbi_pjobc: u32,
    e_tdev: u32,
    e_tpgid: u32,
    pbi_nice: i32,
    pbi_start_tvsec: u64,
    pbi_start_tvusec: u64,
}

#[repr(C)]
#[derive(Default)]
struct ProcTaskInfo {
    virtual_size: u64,
    resident_size: u64,
    total_user: u64,
    total_system: u64,
    threads_user: u64,
    threads_system: u64,
    policy: i32,
    faults: i32,
    pageins: i32,
    cow_faults: i32,
    messages_sent: i32,
    messages_received: i32,
    syscalls_mach: i32,
    syscalls_unix: i32,
    context_switches: i32,
    thread_count: i32,
    running_count: i32,
    priority: i32,
}

#[repr(C)]
struct ProcThreadInfo {
    user_time: u64,
    system_time: u64,
    cpu_usage: i32,
    policy: i32,
    run_state: i32,
    flags: i32,
    sleep_time: i32,
    current_priority: i32,
    priority: i32,
    max_priority: i32,
    name: [libc::c_char; 64],
}

impl Default for ProcThreadInfo {
    fn default() -> Self {
        Self {
            user_time: 0,
            system_time: 0,
            cpu_usage: 0,
            policy: 0,
            run_state: 0,
            flags: 0,
            sleep_time: 0,
            current_priority: 0,
            priority: 0,
            max_priority: 0,
            name: [0; 64],
        }
    }
}

#[repr(C)]
struct ProcFdInfo {
    fd: i32,
    fd_type: u32,
}

#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
    fn proc_pidpath(pid: libc::c_int, buffer: *mut libc::c_void, buffersize: u32) -> libc::c_int;
}

pub fn read_process_observation(pid: u32) -> io::Result<ProcessObservation> {
    let pid = libc::c_int::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "PID exceeds c_int"))?;
    let mut info = MaybeUninit::<ProcBsdInfo>::zeroed();
    let expected = size_of::<ProcBsdInfo>();
    // SAFETY: `info` points to an initialized-size writable buffer and proc_pidinfo writes at most
    // the supplied byte count. A short result is rejected before the value is assumed initialized.
    let written = unsafe {
        proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), expected as libc::c_int)
    };
    if written != expected as libc::c_int {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the exact structure size was written above.
    let info = unsafe { info.assume_init() };
    Ok(ProcessObservation {
        start_token: info
            .pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec),
        last_cpu: None,
    })
}

pub fn read_process_tasks(pid: u32) -> io::Result<TaskBatch> {
    let pid = native_pid(pid)?;
    let task_info = read_task_info(pid)?;
    let thread_count = usize::try_from(task_info.thread_count)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative thread count"))?;
    let thread_ids = read_thread_ids(pid, thread_count.saturating_add(THREAD_LIST_SLACK))?;

    let mut tasks = Vec::with_capacity(thread_ids.len());
    let mut failures = Vec::new();
    for thread_id in thread_ids {
        match read_thread(pid, thread_id) {
            Ok(stat) => tasks.push((thread_id, stat)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => failures.push(TaskReadFailure { tid: Some(thread_id), error }),
        }
    }
    Ok(TaskBatch { tasks, failures })
}

fn read_thread_ids(pid: libc::c_int, mut capacity: usize) -> io::Result<Vec<u64>> {
    capacity = capacity.max(THREAD_LIST_SLACK);
    for _ in 0..THREAD_LIST_RETRIES {
        let mut thread_ids = vec![0_u64; capacity];
        let written = call_pidinfo(
            pid,
            PROC_PIDLISTTHREADS,
            0,
            thread_ids.as_mut_ptr().cast(),
            byte_len(&thread_ids)?,
        )? as usize;
        if written % size_of::<u64>() != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "unaligned thread list"));
        }
        let count = written / size_of::<u64>();
        if count < capacity {
            thread_ids.truncate(count);
            return Ok(thread_ids);
        }
        capacity = capacity.checked_mul(2).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "thread list is too large")
        })?;
    }
    Err(io::Error::other("thread list changed during every collection attempt"))
}

fn read_thread(pid: libc::c_int, thread_id: u64) -> io::Result<TaskStat> {
    let mut info = ProcThreadInfo::default();
    let written = call_pidinfo(
        pid,
        PROC_PIDTHREADINFO,
        thread_id,
        (&mut info as *mut ProcThreadInfo).cast(),
        size_of::<ProcThreadInfo>() as libc::c_int,
    )?;
    if written as usize != size_of::<ProcThreadInfo>() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "short proc_threadinfo"));
    }
    let name_bytes = info.name.map(|byte| byte as u8);
    let name_end = name_bytes.iter().position(|byte| *byte == 0).unwrap_or(name_bytes.len());
    Ok(TaskStat {
        name: Observed::Value(String::from_utf8_lossy(&name_bytes[..name_end]).into_owned()),
        state: Observed::Value(match info.run_state {
            1 => ProcessState::Running,
            2 => ProcessState::Stopped,
            3 | 4 => ProcessState::Waiting,
            5 => ProcessState::Dead,
            _ => ProcessState::Unknown,
        }),
        // XNU's exported proc_threadinfo run-time totals are nanoseconds.
        cpu_time_seconds: Observed::Value(
            info.user_time.saturating_add(info.system_time) as f64 / 1_000_000_000.0,
        ),
        start_token: None,
        last_cpu: Observed::Unsupported,
    })
}

pub fn read_process_resources(pid: u32) -> Result<ResourceSample, DetailUnavailable> {
    let pid = native_pid(pid).map_err(|error| detail_unavailable(&error))?;
    Ok(ResourceSample {
        executable: observed(read_executable(pid)),
        current_directory: Observed::Unsupported,
        virtual_bytes: observed(read_task_info(pid).map(|info| info.virtual_size)),
        open_resources: observed(read_fd_count(pid)),
        open_resource_label: "file descriptors",
        read_bytes: Observed::Unsupported,
        write_bytes: Observed::Unsupported,
        read_bytes_per_second: Observed::Unsupported,
        write_bytes_per_second: Observed::Unsupported,
        io_label: "storage I/O",
    })
}

fn read_executable(pid: libc::c_int) -> io::Result<PathBuf> {
    let mut buffer = vec![0_u8; PATH_CAPACITY];
    // SAFETY: the byte buffer is writable for its declared size and pid is range-checked.
    let written = unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if written <= 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(written as usize);
    Ok(PathBuf::from(std::ffi::OsString::from_vec(buffer)))
}

fn read_task_info(pid: libc::c_int) -> io::Result<ProcTaskInfo> {
    let mut info = ProcTaskInfo::default();
    let written = call_pidinfo(
        pid,
        PROC_PIDTASKINFO,
        0,
        (&mut info as *mut ProcTaskInfo).cast(),
        size_of::<ProcTaskInfo>() as libc::c_int,
    )?;
    if written as usize == size_of::<ProcTaskInfo>() {
        Ok(info)
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidData, "short proc_taskinfo"))
    }
}

fn read_fd_count(pid: libc::c_int) -> io::Result<u64> {
    let bytes = call_pidinfo(pid, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0)?;
    Ok(bytes as u64 / size_of::<ProcFdInfo>() as u64)
}

fn call_pidinfo(
    pid: libc::c_int,
    flavor: libc::c_int,
    arg: u64,
    buffer: *mut libc::c_void,
    buffersize: libc::c_int,
) -> io::Result<libc::c_int> {
    // SAFETY: callers provide either a null size-query buffer or a writable buffer matching size.
    let written = unsafe { proc_pidinfo(pid, flavor, arg, buffer, buffersize) };
    if written <= 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(written)
    }
}

fn byte_len<T>(values: &[T]) -> io::Result<libc::c_int> {
    values
        .len()
        .checked_mul(size_of::<T>())
        .and_then(|bytes| libc::c_int::try_from(bytes).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "native buffer is too large"))
}

fn native_pid(pid: u32) -> io::Result<libc::c_int> {
    libc::c_int::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "PID exceeds c_int"))
}

fn observed<T>(result: io::Result<T>) -> Observed<T> {
    match result {
        Ok(value) => Observed::Value(value),
        Err(error) => match error.kind() {
            io::ErrorKind::PermissionDenied => Observed::PermissionDenied,
            io::ErrorKind::NotFound => Observed::TargetGone,
            io::ErrorKind::Unsupported => Observed::Unsupported,
            _ => Observed::Failed,
        },
    }
}

fn detail_unavailable(error: &io::Error) -> DetailUnavailable {
    match error.kind() {
        io::ErrorKind::PermissionDenied => DetailUnavailable::PermissionDenied,
        io::ErrorKind::NotFound => DetailUnavailable::TargetGone,
        io::ErrorKind::Unsupported => DetailUnavailable::Unsupported,
        _ => DetailUnavailable::Failed,
    }
}

pub fn send_action(_key: ProcessKey, _action: ProcessAction) -> Result<(), ActionError> {
    Err(ActionError::Unsupported { reason: "macOS process actions are read-only" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_layout_matches_the_xnu_contract() {
        assert_eq!(size_of::<ProcBsdInfo>(), 136);
        assert_eq!(size_of::<ProcTaskInfo>(), 96);
        assert_eq!(size_of::<ProcThreadInfo>(), 112);
        assert_eq!(size_of::<ProcFdInfo>(), 8);
    }

    #[test]
    fn current_process_exposes_identity_threads_and_resources() {
        let pid = std::process::id();
        assert!(read_process_observation(pid).unwrap().start_token > 0);

        let tasks = read_process_tasks(pid).unwrap();
        assert!(!tasks.tasks.is_empty());

        let resources = read_process_resources(pid).unwrap();
        assert!(matches!(resources.executable, Observed::Value(_)));
        assert!(matches!(resources.virtual_bytes, Observed::Value(bytes) if bytes > 0));
        assert!(matches!(resources.open_resources, Observed::Value(_)));
    }
}
