//! Focused Linux `/proc/<pid>/stat` enrichment.
//!
//! sysinfo owns process and CPU sampling. This module reads the kernel task fields it does not
//! expose: process generation (field 22) and the CPU a task last executed on (field 39).

use std::io;

use rustix::process::{pidfd_open, pidfd_send_signal, Pid, PidfdFlags, Signal};
use thiserror::Error;

use super::{ActionError, ProcessAction, TaskStat};
use crate::tools::stats::model::{DetailUnavailable, Observed, ProcessKey, ResourceSample};

#[derive(Debug, Error)]
pub enum StatParseError {
    #[error("missing command terminator")]
    MissingCommand,
    #[error("missing stat field {0}")]
    MissingField(u8),
    #[error("invalid stat field {field}: {value}")]
    InvalidField { field: u8, value: String },
}

pub fn read_process_stat(pid: u32) -> io::Result<TaskStat> {
    read_stat_path(format!("/proc/{pid}/stat"))
}

pub fn read_thread_stat(pid: u32, tid: u32) -> io::Result<TaskStat> {
    read_stat_path(format!("/proc/{pid}/task/{tid}/stat"))
}

pub fn read_process_tasks(pid: u32) -> io::Result<Vec<(u32, TaskStat)>> {
    let entries = std::fs::read_dir(format!("/proc/{pid}/task"))?;
    let mut tasks = Vec::new();
    for entry in entries.flatten() {
        let Some(tid) = entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        if let Ok(stat) = read_thread_stat(pid, tid) {
            tasks.push((tid, stat));
        }
    }
    Ok(tasks)
}

pub fn read_process_resources(pid: u32) -> Result<ResourceSample, DetailUnavailable> {
    let root = format!("/proc/{pid}");
    let (read_bytes, write_bytes) = match read_io_counters(&format!("{root}/io")) {
        Ok((read, write)) => (Observed::Value(read), Observed::Value(write)),
        Err(error) => (observed_error(&error), observed_error(&error)),
    };
    Ok(ResourceSample {
        executable: observed(std::fs::read_link(format!("{root}/exe"))),
        current_directory: observed(std::fs::read_link(format!("{root}/cwd"))),
        virtual_bytes: observed(read_virtual_bytes(&format!("{root}/statm"))),
        open_resources: observed(count_directory_entries(&format!("{root}/fd"))),
        open_resource_label: "file descriptors",
        read_bytes,
        write_bytes,
        read_bytes_per_second: Observed::Warming,
        write_bytes_per_second: Observed::Warming,
        io_label: "storage I/O",
    })
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

fn read_virtual_bytes(path: &str) -> io::Result<u64> {
    let text = std::fs::read_to_string(path)?;
    let pages = text
        .split_whitespace()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing statm size"))?
        .parse::<u64>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(pages.saturating_mul(rustix::param::page_size() as u64))
}

fn count_directory_entries(path: &str) -> io::Result<u64> {
    Ok(std::fs::read_dir(path)?.filter_map(Result::ok).count() as u64)
}

fn read_io_counters(path: &str) -> io::Result<(u64, u64)> {
    let text = std::fs::read_to_string(path)?;
    let mut read_bytes = None;
    let mut write_bytes = None;
    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value
            .trim()
            .parse::<u64>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        match name {
            "read_bytes" => read_bytes = Some(value),
            "write_bytes" => write_bytes = Some(value),
            _ => {}
        }
    }
    match (read_bytes, write_bytes) {
        (Some(read), Some(write)) => Ok((read, write)),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "missing storage I/O counters")),
    }
}

pub fn send_action(key: ProcessKey, action: ProcessAction) -> Result<(), ActionError> {
    if key.pid == 1 {
        return Err(ActionError::Init);
    }
    if key.pid == std::process::id() {
        return Err(ActionError::SelfProcess);
    }
    let pid = i32::try_from(key.pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or(ActionError::InvalidPid(key.pid))?;
    let pidfd = pidfd_open(pid, PidfdFlags::empty()).map_err(|source| ActionError::Io {
        pid: key.pid,
        operation: "open a verified handle for",
        source: io::Error::from_raw_os_error(source.raw_os_error()),
    })?;
    let current = read_process_stat(key.pid).map_err(|_| ActionError::Unavailable(key.pid))?;
    if current.start_token != key.start_token {
        return Err(ActionError::Replaced { pid: key.pid });
    }
    let signal = match action {
        ProcessAction::GracefulTerminate => Signal::TERM,
        ProcessAction::ForceTerminate => Signal::KILL,
    };
    pidfd_send_signal(&pidfd, signal).map_err(|source| ActionError::Io {
        pid: key.pid,
        operation: action.label(),
        source: io::Error::from_raw_os_error(source.raw_os_error()),
    })
}

fn read_stat_path(path: String) -> io::Result<TaskStat> {
    let text = std::fs::read_to_string(path)?;
    parse_stat(&text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn parse_stat(text: &str) -> Result<TaskStat, StatParseError> {
    let command_end = text.rfind(") ").ok_or(StatParseError::MissingCommand)?;
    let command_start = text.find('(').ok_or(StatParseError::MissingCommand)? + 1;
    let fields = text[command_end + 2..].split_whitespace().collect::<Vec<_>>();
    let user_ticks = parse_field::<u64>(&fields, 14, 11)?;
    let system_ticks = parse_field::<u64>(&fields, 15, 12)?;
    let start = parse_field::<u64>(&fields, 22, 19)?;
    let last_cpu = fields
        .get(36)
        .map(|value| {
            value
                .parse::<u16>()
                .map_err(|_| StatParseError::InvalidField { field: 39, value: (*value).to_owned() })
        })
        .transpose()?;
    Ok(TaskStat {
        name: text[command_start..command_end].to_owned(),
        cpu_ticks: user_ticks.saturating_add(system_ticks),
        start_token: start,
        last_cpu,
    })
}

fn parse_field<T>(fields: &[&str], number: u8, index: usize) -> Result<T, StatParseError>
where
    T: std::str::FromStr,
{
    let value = fields.get(index).ok_or(StatParseError::MissingField(number))?;
    value
        .parse()
        .map_err(|_| StatParseError::InvalidField { field: number, value: (*value).to_owned() })
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn parses_start_token_and_last_cpu_with_tricky_command() {
        let stat = "77 (name with ) parens) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 7";
        assert_eq!(
            parse_stat(stat).unwrap(),
            TaskStat {
                name: "name with ) parens".to_owned(),
                cpu_ticks: 23,
                start_token: 4242,
                last_cpu: Some(7),
            }
        );
    }

    #[test]
    fn accepts_stat_without_optional_processor_field() {
        let stat = "1 (init) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 99";
        assert_eq!(
            parse_stat(stat).unwrap(),
            TaskStat { name: "init".to_owned(), cpu_ticks: 0, start_token: 99, last_cpu: None }
        );
    }

    #[test]
    fn rejects_truncated_stat() {
        assert!(matches!(parse_stat("1 (x) S 0"), Err(StatParseError::MissingField(14))));
    }

    #[test]
    fn stale_generation_is_refused_without_acting() {
        let mut child = Command::new("sleep").arg("10").stdout(Stdio::null()).spawn().unwrap();
        let stat = read_process_stat(child.id()).unwrap();
        let stale = ProcessKey { pid: child.id(), start_token: stat.start_token + 1 };
        assert!(matches!(
            send_action(stale, ProcessAction::GracefulTerminate),
            Err(ActionError::Replaced { .. })
        ));
        assert!(child.try_wait().unwrap().is_none());
        let current = ProcessKey { pid: child.id(), start_token: stat.start_token };
        send_action(current, ProcessAction::ForceTerminate).unwrap();
        let _ = child.wait();
    }

    #[test]
    fn pidfd_terminates_the_exact_disposable_child() {
        let mut child = Command::new("sleep").arg("10").stdout(Stdio::null()).spawn().unwrap();
        let stat = read_process_stat(child.id()).unwrap();
        let key = ProcessKey { pid: child.id(), start_token: stat.start_token };
        send_action(key, ProcessAction::GracefulTerminate).unwrap();
        for _ in 0..20 {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        send_action(key, ProcessAction::ForceTerminate).unwrap();
        let _ = child.wait();
        panic!("child did not exit after graceful termination");
    }
}
