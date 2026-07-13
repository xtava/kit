use rustix::process::{pidfd_open, pidfd_send_signal, Pid, PidfdFlags, Signal};
use thiserror::Error;

use super::linux;
use super::model::ProcessKey;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessSignal {
    Terminate,
    Kill,
}

impl ProcessSignal {
    pub fn label(self) -> &'static str {
        match self {
            Self::Terminate => "SIGTERM",
            Self::Kill => "SIGKILL",
        }
    }
}

#[derive(Debug, Error)]
pub enum SignalError {
    #[error("refusing to signal PID 1")]
    Init,
    #[error("refusing to signal the monitor itself")]
    SelfProcess,
    #[error("invalid process id {0}")]
    InvalidPid(u32),
    #[error("process {0} is no longer available")]
    Unavailable(u32),
    #[error("process {pid} was replaced before the signal could be sent")]
    Replaced { pid: u32 },
    #[error("cannot safely address process {pid}: {source}")]
    Pidfd { pid: u32, source: rustix::io::Errno },
    #[error("could not send {signal} to process {pid}: {source}")]
    Send { pid: u32, signal: &'static str, source: rustix::io::Errno },
}

pub fn send(key: ProcessKey, process_signal: ProcessSignal) -> Result<(), SignalError> {
    if key.pid == 1 {
        return Err(SignalError::Init);
    }
    if key.pid == std::process::id() {
        return Err(SignalError::SelfProcess);
    }
    let pid = i32::try_from(key.pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or(SignalError::InvalidPid(key.pid))?;
    let pidfd = pidfd_open(pid, PidfdFlags::empty())
        .map_err(|source| SignalError::Pidfd { pid: key.pid, source })?;
    let current =
        linux::read_process_stat(key.pid).map_err(|_| SignalError::Unavailable(key.pid))?;
    if current.start_token != key.start_token {
        return Err(SignalError::Replaced { pid: key.pid });
    }
    let signal = match process_signal {
        ProcessSignal::Terminate => Signal::TERM,
        ProcessSignal::Kill => Signal::KILL,
    };
    pidfd_send_signal(&pidfd, signal).map_err(|source| SignalError::Send {
        pid: key.pid,
        signal: process_signal.label(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn stale_generation_is_refused_without_signaling() {
        let mut child = Command::new("sleep").arg("10").stdout(Stdio::null()).spawn().unwrap();
        let stat = linux::read_process_stat(child.id()).unwrap();
        let key = ProcessKey { pid: child.id(), start_token: stat.start_token + 1 };
        assert!(matches!(send(key, ProcessSignal::Terminate), Err(SignalError::Replaced { .. })));
        assert!(child.try_wait().unwrap().is_none());
        let key = ProcessKey { pid: child.id(), start_token: stat.start_token };
        send(key, ProcessSignal::Kill).unwrap();
        let _ = child.wait();
    }

    #[test]
    fn pidfd_terminates_the_exact_disposable_child() {
        let mut child = Command::new("sleep").arg("10").stdout(Stdio::null()).spawn().unwrap();
        let stat = linux::read_process_stat(child.id()).unwrap();
        let key = ProcessKey { pid: child.id(), start_token: stat.start_token };
        send(key, ProcessSignal::Terminate).unwrap();
        for _ in 0..20 {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        send(key, ProcessSignal::Kill).unwrap();
        let _ = child.wait();
        panic!("child did not exit after SIGTERM");
    }
}
