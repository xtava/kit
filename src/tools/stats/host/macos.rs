use std::io;
use std::mem::{size_of, MaybeUninit};

use super::{ActionError, ProcessAction, ProcessObservation, TaskBatch};
use crate::tools::stats::model::{DetailUnavailable, ProcessKey, ResourceSample};

const PROC_PIDTBSDINFO: libc::c_int = 3;

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

#[link(name = "proc")]
unsafe extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
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

pub fn read_process_tasks(_pid: u32) -> io::Result<TaskBatch> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "macOS thread detail is not implemented in this build",
    ))
}

pub fn read_process_resources(_pid: u32) -> Result<ResourceSample, DetailUnavailable> {
    Err(DetailUnavailable::Unsupported)
}

pub fn send_action(_key: ProcessKey, _action: ProcessAction) -> Result<(), ActionError> {
    Err(ActionError::Unsupported { reason: "macOS process actions are read-only" })
}
