//! Static host dispatch for native process identity, detail, and safe actions.

use std::io;

use thiserror::Error;

use super::model::{CapabilityState, HostCapabilities, Observed, ProcessState};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessObservation {
    pub start_token: u64,
    pub last_cpu: Option<u16>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskStat {
    pub name: Observed<String>,
    pub state: Observed<ProcessState>,
    pub cpu_time_seconds: Observed<f64>,
    pub start_token: Option<u64>,
    pub last_cpu: Observed<u16>,
}

pub struct TaskBatch {
    pub tasks: Vec<(u32, TaskStat)>,
    pub failures: Vec<TaskReadFailure>,
}

pub struct TaskReadFailure {
    pub tid: Option<u32>,
    pub error: io::Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessAction {
    GracefulTerminate,
    ForceTerminate,
}

impl ProcessAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::GracefulTerminate => "terminate",
            Self::ForceTerminate => "force terminate",
        }
    }
}

#[derive(Debug, Error)]
pub enum ActionError {
    #[error("refusing to act on PID 1")]
    Init,
    #[error("refusing to act on a system process")]
    #[cfg(target_os = "windows")]
    SystemProcess,
    #[error("refusing to act on the monitor itself")]
    SelfProcess,
    #[error("refusing to terminate critical process {0}")]
    #[cfg(target_os = "windows")]
    Protected(u32),
    #[error("invalid process id {0}")]
    InvalidPid(u32),
    #[error("process {0} is no longer available")]
    Unavailable(u32),
    #[error("process {pid} was replaced before the action could be performed")]
    Replaced { pid: u32 },
    #[error("could not {operation} process {pid}: {source}")]
    Io {
        pid: u32,
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("process action is unavailable: {reason}")]
    #[cfg(not(target_os = "linux"))]
    Unsupported { reason: &'static str },
}

pub fn capabilities() -> HostCapabilities {
    #[cfg(target_os = "linux")]
    {
        HostCapabilities {
            last_observed_core: CapabilityState::Available,
            threads: CapabilityState::Available,
            resources: CapabilityState::Available,
            graceful_terminate: CapabilityState::Available,
            force_terminate: CapabilityState::Available,
            code_profile: CapabilityState::Unsupported {
                reason: "perf handshake has not been validated for this host",
            },
        }
    }
    #[cfg(target_os = "macos")]
    {
        HostCapabilities {
            last_observed_core: CapabilityState::Unsupported {
                reason: "last-observed CPU is unavailable on this platform",
            },
            threads: CapabilityState::Unsupported {
                reason: "native thread detail is not implemented for this target",
            },
            resources: CapabilityState::Unsupported {
                reason: "native resource detail is not implemented for this target",
            },
            graceful_terminate: CapabilityState::Unsupported {
                reason: "safe graceful termination is unavailable on this platform",
            },
            force_terminate: CapabilityState::Unsupported { reason: "macOS is read-only" },
            code_profile: CapabilityState::Unsupported {
                reason: "bounded code profiling is unavailable on this platform",
            },
        }
    }
    #[cfg(target_os = "windows")]
    {
        HostCapabilities {
            last_observed_core: CapabilityState::Unsupported {
                reason: "last-observed CPU is unavailable on Windows",
            },
            threads: CapabilityState::Unsupported {
                reason: "native thread detail is not implemented for Windows",
            },
            resources: CapabilityState::Unsupported {
                reason: "native resource detail is not implemented for Windows",
            },
            graceful_terminate: CapabilityState::Unsupported {
                reason: "Windows has no safe generic graceful process termination",
            },
            force_terminate: CapabilityState::Available,
            code_profile: CapabilityState::Unsupported {
                reason: "bounded code profiling is unavailable on Windows",
            },
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        HostCapabilities {
            last_observed_core: CapabilityState::Unsupported {
                reason: "last-observed CPU is unavailable on this platform",
            },
            threads: CapabilityState::Unsupported {
                reason: "native thread detail is not implemented for this target",
            },
            resources: CapabilityState::Unsupported {
                reason: "native resource detail is not implemented for this target",
            },
            graceful_terminate: CapabilityState::Unsupported {
                reason: "safe graceful termination is unavailable on this platform",
            },
            force_terminate: CapabilityState::Unsupported {
                reason: "verified force termination is not implemented for this target",
            },
            code_profile: CapabilityState::Unsupported {
                reason: "bounded code profiling is unavailable on this platform",
            },
        }
    }
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{
    read_process_observation, read_process_resources, read_process_tasks, send_action,
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{
    read_process_observation, read_process_resources, read_process_tasks, send_action,
};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::{
    read_process_observation, read_process_resources, read_process_tasks, send_action,
};

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod fallback {
    use std::io;

    use super::{ActionError, ProcessAction, ProcessObservation, TaskBatch};
    use crate::tools::stats::model::{DetailUnavailable, ProcessKey, ResourceSample};

    pub fn read_process_observation(_pid: u32) -> io::Result<ProcessObservation> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native process generation is not implemented for this target",
        ))
    }

    pub fn read_process_tasks(_pid: u32) -> io::Result<TaskBatch> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "native thread detail is not implemented for this target",
        ))
    }

    pub fn read_process_resources(_pid: u32) -> Result<ResourceSample, DetailUnavailable> {
        Err(DetailUnavailable::Unsupported)
    }

    pub fn send_action(_key: ProcessKey, _action: ProcessAction) -> Result<(), ActionError> {
        Err(ActionError::Unsupported {
            reason: "safe native process actions are not implemented for this target",
        })
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub use fallback::{
    read_process_observation, read_process_resources, read_process_tasks, send_action,
};
