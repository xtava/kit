//! Grouping the `/proc` table into Electron instances, structurally.
//!
//! The browser main can carry *no* distinguishing flags — no `--type`, no `--class` — only its
//! argv0 (the app binary) identifies it. So we anchor on the unambiguous processes instead: a
//! Chromium child (`--type=…`) whose argv0 *is* the app binary. From each, we climb `ppid` to the
//! first ancestor without a `--type` — that is the real main, found regardless of its flags. A
//! stray Google Chrome never matches (its argv0 isn't the app binary); a `node` dev-server never
//! matches (it has no `--type`, and nothing climbs to it).

use std::collections::HashMap;

use super::classify;
use super::sample::{self, RawProc};
use crate::tools::scout::model::{Instance, Process};

pub fn scan_fleet(marker: &str) -> Vec<Instance> {
    let all = sample::read_all();
    let marker = marker.to_lowercase();

    let mut groups: HashMap<u32, Vec<u32>> = HashMap::new();
    for child in all.values() {
        let is_app_child =
            classify::process_type(&child.args).is_some() && is_app_binary(&child.args, &marker);
        if is_app_child {
            if let Some(main) = main_of(child.pid, &all) {
                groups.entry(main).or_default().push(child.pid);
            }
        }
    }

    let mut instances: Vec<Instance> = groups
        .into_iter()
        .filter_map(|(main_pid, child_pids)| build_instance(main_pid, child_pids, &all))
        .collect();
    instances.sort_by_key(|instance| std::cmp::Reverse(total_pss(instance)));
    instances
}

/// The fleet's real memory: sum of every process's PSS.
pub fn total_pss(instance: &Instance) -> u64 {
    instance.processes.iter().map(|process| process.pss_kib).sum()
}

fn build_instance(
    main_pid: u32,
    child_pids: Vec<u32>,
    all: &HashMap<u32, RawProc>,
) -> Option<Instance> {
    let main = all.get(&main_pid)?;

    let mut processes: Vec<Process> = std::iter::once(main_pid)
        .chain(child_pids)
        .filter_map(|pid| all.get(&pid))
        .map(|raw| {
            let (pss_kib, rss_kib, swap_kib) = sample::read_smaps(raw.pid);
            Process {
                pid: raw.pid,
                ppid: raw.ppid,
                role: classify::role(&raw.args, rss_kib),
                pss_kib,
                rss_kib,
                swap_kib,
                threads: raw.threads,
                target_id: None,
            }
        })
        .collect();
    processes.sort_by(|a, b| b.pss_kib.cmp(&a.pss_kib));

    Some(Instance {
        name: classify::wm_class(&main.args)
            .map(str::to_owned)
            .unwrap_or_else(|| binary_name(&main.args)),
        root_pid: main_pid,
        debug_port: classify::debug_port(&main.args),
        processes,
        targets: Vec::new(),
    })
}

/// Climb `ppid` from a Chromium child to the first ancestor that has no `--type` — the browser main.
fn main_of(start: u32, all: &HashMap<u32, RawProc>) -> Option<u32> {
    let mut current = start;
    for _ in 0..64 {
        let parent = all.get(&current)?.ppid;
        if classify::process_type(&all.get(&parent)?.args).is_none() {
            return Some(parent);
        }
        current = parent;
    }
    None
}

/// True when the process's argv0 (the executable) is the app binary — case-insensitive, so an
/// installed "MyApp Canary" matches a `--app myapp` survey just as a dev build does.
fn is_app_binary(args: &[String], marker: &str) -> bool {
    args.first().map(|argv0| argv0.to_lowercase().contains(marker)).unwrap_or(false)
}

fn binary_name(args: &[String]) -> String {
    args.first().and_then(|arg| arg.rsplit('/').next()).unwrap_or("electron").to_owned()
}
