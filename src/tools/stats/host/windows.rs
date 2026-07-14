use std::ffi::c_void;
use std::io;

use super::{ActionError, ProcessAction, ProcessObservation, TaskBatch};
use crate::tools::stats::model::{DetailUnavailable, ProcessKey, ResourceSample};

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const PROCESS_TERMINATE: u32 = 0x0001;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileTime {
    low: u32,
    high: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
    fn GetProcessTimes(
        process: *mut c_void,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn IsProcessCritical(process: *mut c_void, critical: *mut i32) -> i32;
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
    Ok(ProcessObservation {
        start_token: (u64::from(creation.high) << 32) | u64::from(creation.low),
        last_cpu: None,
    })
}

pub fn read_process_tasks(_pid: u32) -> io::Result<TaskBatch> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows thread detail is not implemented in this build",
    ))
}

pub fn read_process_resources(_pid: u32) -> Result<ResourceSample, DetailUnavailable> {
    Err(DetailUnavailable::Unsupported)
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
        Ok((u64::from(creation.high) << 32) | u64::from(creation.low))
    }
}
