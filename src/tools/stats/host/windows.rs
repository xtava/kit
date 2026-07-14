use std::ffi::{c_void, OsString};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;

use super::{ActionError, ProcessAction, ProcessObservation, TaskBatch, TaskReadFailure, TaskStat};
use crate::tools::stats::model::{DetailUnavailable, Observed, ProcessKey, ResourceSample};

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const PROCESS_TERMINATE: u32 = 0x0001;
const THREAD_QUERY_LIMITED_INFORMATION: u32 = 0x0800;
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
const ERROR_NO_MORE_FILES: i32 = 18;
const INVALID_HANDLE_VALUE: *mut c_void = -1_isize as *mut c_void;
const PATH_CAPACITY: usize = 32_768;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTime {
    low: u32,
    high: u32,
}

impl FileTime {
    fn ticks(self) -> u64 {
        (u64::from(self.high) << 32) | u64::from(self.low)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ThreadEntry32 {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_process_id: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IoCounters {
    read_operations: u64,
    write_operations: u64,
    other_operations: u64,
    read_bytes: u64,
    write_bytes: u64,
    other_bytes: u64,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
    fn OpenThread(access: u32, inherit_handle: i32, thread_id: u32) -> *mut c_void;
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut c_void;
    fn Thread32First(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snapshot: *mut c_void, entry: *mut ThreadEntry32) -> i32;
    fn GetProcessTimes(
        process: *mut c_void,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn IsProcessCritical(process: *mut c_void, critical: *mut i32) -> i32;
    fn GetThreadTimes(
        thread: *mut c_void,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn QueryFullProcessImageNameW(
        process: *mut c_void,
        flags: u32,
        name: *mut u16,
        size: *mut u32,
    ) -> i32;
    fn GetProcessHandleCount(process: *mut c_void, count: *mut u32) -> i32;
    fn GetProcessIoCounters(process: *mut c_void, counters: *mut IoCounters) -> i32;
    fn CloseHandle(object: *mut c_void) -> i32;
    fn TerminateProcess(process: *mut c_void, exit_code: u32) -> i32;
}

struct OwnedHandle(*mut c_void);

impl OwnedHandle {
    fn open(access: u32, pid: u32) -> io::Result<Self> {
        // SAFETY: OpenProcess takes plain values and returns either null or an owned kernel handle.
        let handle = unsafe { OpenProcess(access, 0, pid) };
        if handle.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }

    fn snapshot_threads() -> io::Result<Self> {
        // SAFETY: the thread-snapshot flag ignores the process identifier and returns an owned
        // snapshot handle or INVALID_HANDLE_VALUE.
        let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }

    fn raw(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this wrapper exclusively owns the non-null handle returned by OpenProcess.
        unsafe { CloseHandle(self.0) };
    }
}

pub fn read_process_observation(pid: u32) -> io::Result<ProcessObservation> {
    let handle = OwnedHandle::open(PROCESS_QUERY_LIMITED_INFORMATION, pid)?;
    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    // SAFETY: all output pointers are valid for one FileTime and `handle` remains live for the call.
    let result =
        unsafe { GetProcessTimes(handle.raw(), &mut creation, &mut exit, &mut kernel, &mut user) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ProcessObservation { start_token: creation.ticks(), last_cpu: None })
}

pub fn read_process_tasks(pid: u32) -> io::Result<TaskBatch> {
    let snapshot = OwnedHandle::snapshot_threads()?;
    let mut entry = ThreadEntry32 { size: size_of::<ThreadEntry32>() as u32, ..Default::default() };
    let mut tasks = Vec::new();
    let mut failures = Vec::new();
    // SAFETY: `entry` is a correctly sized writable THREADENTRY32 and the snapshot remains live.
    let mut has_entry = unsafe { Thread32First(snapshot.raw(), &mut entry) } != 0;
    if !has_entry {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_NO_MORE_FILES) {
            return Err(error);
        }
    }
    while has_entry {
        if entry.owner_process_id == pid {
            match read_thread(entry.thread_id) {
                Ok(stat) => tasks.push((u64::from(entry.thread_id), stat)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    failures.push(TaskReadFailure { tid: Some(u64::from(entry.thread_id)), error })
                }
            }
        }
        entry.size = size_of::<ThreadEntry32>() as u32;
        // SAFETY: the same initialized entry and live snapshot are valid for the next enumeration.
        has_entry = unsafe { Thread32Next(snapshot.raw(), &mut entry) } != 0;
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(ERROR_NO_MORE_FILES) {
        failures.push(TaskReadFailure { tid: None, error });
    }
    Ok(TaskBatch { tasks, failures })
}

fn read_thread(tid: u32) -> io::Result<TaskStat> {
    // SAFETY: OpenThread takes plain values and returns null or an owned thread handle.
    let raw = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let handle = OwnedHandle(raw);
    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    // SAFETY: all output pointers are valid and the queried thread handle remains live.
    if unsafe { GetThreadTimes(handle.raw(), &mut creation, &mut exit, &mut kernel, &mut user) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(TaskStat {
        name: Observed::Unsupported,
        state: Observed::Unsupported,
        cpu_time_seconds: Observed::Value(
            kernel.ticks().saturating_add(user.ticks()) as f64 / 10_000_000.0,
        ),
        start_token: Some(creation.ticks()),
        last_cpu: Observed::Unsupported,
    })
}

pub fn read_process_resources(pid: u32) -> Result<ResourceSample, DetailUnavailable> {
    let handle = OwnedHandle::open(PROCESS_QUERY_LIMITED_INFORMATION, pid)
        .map_err(|error| detail_unavailable(&error))?;
    let executable = observed(query_executable(handle.raw()));
    let open_resources = observed(query_handle_count(handle.raw()).map(u64::from));
    let (read_bytes, write_bytes) = match query_io(handle.raw()) {
        Ok(counters) => {
            (Observed::Value(counters.read_bytes), Observed::Value(counters.write_bytes))
        }
        Err(error) => (observed_error(&error), observed_error(&error)),
    };
    Ok(ResourceSample {
        executable,
        current_directory: Observed::Unsupported,
        virtual_bytes: Observed::Unsupported,
        open_resources,
        open_resource_label: "handles",
        read_bytes,
        write_bytes,
        read_bytes_per_second: Observed::Warming,
        write_bytes_per_second: Observed::Warming,
        io_label: "process I/O",
    })
}

fn query_executable(process: *mut c_void) -> io::Result<PathBuf> {
    let mut buffer = vec![0_u16; PATH_CAPACITY];
    let mut size = buffer.len() as u32;
    // SAFETY: the UTF-16 buffer is writable for `size` elements and the process handle is live.
    if unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(size as usize);
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

fn query_handle_count(process: *mut c_void) -> io::Result<u32> {
    let mut count = 0;
    // SAFETY: `count` is writable and the process handle is live.
    if unsafe { GetProcessHandleCount(process, &mut count) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(count)
    }
}

fn query_io(process: *mut c_void) -> io::Result<IoCounters> {
    let mut counters = IoCounters::default();
    // SAFETY: `counters` is writable and the process handle is live.
    if unsafe { GetProcessIoCounters(process, &mut counters) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(counters)
    }
}

fn observed<T>(result: io::Result<T>) -> Observed<T> {
    match result {
        Ok(value) => Observed::Value(value),
        Err(error) => observed_error(&error),
    }
}

fn observed_error<T>(error: &io::Error) -> Observed<T> {
    match error.kind() {
        io::ErrorKind::PermissionDenied => Observed::PermissionDenied,
        io::ErrorKind::NotFound => Observed::TargetGone,
        io::ErrorKind::Unsupported => Observed::Unsupported,
        _ => Observed::Failed,
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

pub fn send_action(key: ProcessKey, action: ProcessAction) -> Result<(), ActionError> {
    if key.pid == 0 {
        return Err(ActionError::SystemProcess);
    }
    if key.pid == 1 {
        return Err(ActionError::Init);
    }
    if key.pid == std::process::id() {
        return Err(ActionError::SelfProcess);
    }
    if action == ProcessAction::GracefulTerminate {
        return Err(ActionError::Unsupported {
            reason: "Windows has no safe generic graceful process termination",
        });
    }
    let handle = OwnedHandle::open(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE, key.pid)
        .map_err(|source| ActionError::Io {
            pid: key.pid,
            operation: "open a verified handle for",
            source,
        })?;
    let creation = process_creation_time(handle.raw()).map_err(|source| ActionError::Io {
        pid: key.pid,
        operation: "verify the generation of",
        source,
    })?;
    if creation != key.start_token {
        return Err(ActionError::Replaced { pid: key.pid });
    }
    let mut critical = 0;
    // SAFETY: `critical` is writable and the generation-verified handle is live for the call.
    if unsafe { IsProcessCritical(handle.raw(), &mut critical) } == 0 {
        return Err(ActionError::Io {
            pid: key.pid,
            operation: "inspect critical status of",
            source: io::Error::last_os_error(),
        });
    }
    if critical != 0 {
        return Err(ActionError::Protected(key.pid));
    }
    // SAFETY: the handle is live, generation-verified, non-critical, and has PROCESS_TERMINATE.
    if unsafe { TerminateProcess(handle.raw(), 1) } == 0 {
        return Err(ActionError::Io {
            pid: key.pid,
            operation: action.label(),
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn process_creation_time(handle: *mut c_void) -> io::Result<u64> {
    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    // SAFETY: all output pointers are valid for one FileTime and the caller guarantees a live
    // process handle for the duration of this call.
    let result =
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(creation.ticks())
    }
}
