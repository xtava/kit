//! Focused Linux `/proc/<pid>/stat` enrichment.
//!
//! sysinfo owns process and CPU sampling. This module reads the kernel task fields it does not
//! expose: process generation (field 22) and the CPU a task last executed on (field 39).

use std::io;

use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskStat {
    pub name: String,
    pub cpu_ticks: u64,
    pub start_token: u64,
    pub last_cpu: Option<u16>,
}

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
}
