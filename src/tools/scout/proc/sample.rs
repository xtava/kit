//! Reading `/proc`. The whole process table is read cheaply (cmdline + status) so the fleet can be
//! grouped structurally by walking `ppid`; the costlier `smaps_rollup` (PSS) is read only for the
//! processes that end up in an instance.

use std::collections::HashMap;

/// One process as read from `/proc`, before classification. No memory yet — that's [`read_smaps`].
pub struct RawProc {
    pub pid: u32,
    pub ppid: u32,
    pub threads: u16,
    pub args: Vec<String>,
}

/// Every process on the machine that has a cmdline (kernel threads are skipped), keyed by pid.
pub fn read_all() -> HashMap<u32, RawProc> {
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(_) => return HashMap::new(),
    };

    let mut all = HashMap::new();
    for entry in entries.flatten() {
        let pid = match entry.file_name().to_str().and_then(|name| name.parse::<u32>().ok()) {
            Some(pid) => pid,
            None => continue,
        };
        let args = match read_cmdline(pid) {
            Some(args) => args,
            None => continue,
        };
        let (ppid, threads) = read_status(pid).unwrap_or((0, 0));
        all.insert(pid, RawProc { pid, ppid, threads, args });
    }
    all
}

/// `(pss, rss, swap)` in KiB from `/proc/<pid>/smaps_rollup`; zeros if it can't be read.
pub fn read_smaps(pid: u32) -> (u64, u64, u64) {
    let text = match std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) {
        Ok(text) => text,
        Err(_) => return (0, 0, 0),
    };
    let mut pss_kib = 0;
    let mut rss_kib = 0;
    let mut swap_kib = 0;
    for line in text.lines() {
        let mut tokens = line.split_whitespace();
        let key = tokens.next();
        let value = tokens.next().and_then(|kib| kib.parse::<u64>().ok()).unwrap_or(0);
        match key {
            Some("Pss:") => pss_kib += value,
            Some("Rss:") => rss_kib = value,
            Some("Swap:") => swap_kib = value,
            _ => {}
        }
    }
    (pss_kib, rss_kib, swap_kib)
}

fn read_cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = raw
        .split(|&byte| byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    if args.is_empty() {
        None
    } else {
        Some(args)
    }
}

fn read_status(pid: u32) -> Option<(u32, u16)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let mut ppid = 0;
    let mut threads = 0;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("PPid:") {
            ppid = value.trim().parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("Threads:") {
            threads = value.trim().parse().unwrap_or(0);
        }
    }
    Some((ppid, threads))
}
