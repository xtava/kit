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
    pub tasks: Vec<(u64, TaskStat)>,
    pub failures: Vec<TaskReadFailure>,
}

pub struct TaskReadFailure {
    pub tid: Option<u64>,
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
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    Init,
    #[error("refusing to act on a system process")]
    #[cfg(target_os = "windows")]
    SystemProcess,
    #[error("refusing to act on the monitor itself")]
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    SelfProcess,
    #[error("refusing to terminate critical process {0}")]
    #[cfg(target_os = "windows")]
    Protected(u32),
    #[error("invalid process id {0}")]
    #[cfg(target_os = "linux")]
    InvalidPid(u32),
    #[error("process {0} is no longer available")]
    #[cfg(target_os = "linux")]
    Unavailable(u32),
    #[error("process {pid} was replaced before the action could be performed")]
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    Replaced { pid: u32 },
    #[error("could not {operation} process {pid}: {source}")]
    #[cfg(any(target_os = "linux", target_os = "windows"))]
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
                reason: "local perf produced no actionable DWARF stacks under current host policy",
            },
        }
    }
    #[cfg(target_os = "macos")]
    {
        HostCapabilities {
            last_observed_core: CapabilityState::Unsupported {
                reason: "last-observed CPU is unavailable on this platform",
            },
            threads: CapabilityState::Available,
            resources: CapabilityState::Available,
            graceful_terminate: CapabilityState::Unsupported {
                reason: "safe graceful termination is unavailable on this platform",
            },
            force_terminate: CapabilityState::Unsupported { reason: "macOS is read-only" },
            code_profile: CapabilityState::Unsupported {
                reason: "macOS profiling cannot bind atomically to the selected process generation",
            },
        }
    }
    #[cfg(target_os = "windows")]
    {
        HostCapabilities {
            last_observed_core: CapabilityState::Unsupported {
                reason: "last-observed CPU is unavailable on Windows",
            },
            threads: CapabilityState::Available,
            resources: CapabilityState::Available,
            graceful_terminate: CapabilityState::Unsupported {
                reason: "Windows has no safe generic graceful process termination",
            },
            force_terminate: CapabilityState::Available,
            code_profile: CapabilityState::Unsupported {
                reason: "Windows has no bounded non-elevated generation-bound process collector",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_capability_exposes_the_exact_platform_gap() {
        let CapabilityState::Unsupported { reason } = capabilities().code_profile else {
            panic!("profiling must remain unavailable until its host gate is actionable");
        };
        #[cfg(target_os = "linux")]
        assert!(reason.contains("no actionable DWARF stacks"));
        #[cfg(target_os = "macos")]
        assert!(reason.contains("cannot bind atomically"));
        #[cfg(target_os = "windows")]
        assert!(reason.contains("no bounded non-elevated generation-bound"));
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        assert!(reason.contains("unavailable"));
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub use fallback::{
    read_process_observation, read_process_resources, read_process_tasks, send_action,
};
