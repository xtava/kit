//! Shared formatting for the headless report and the live TUI — byte sizes, role aggregation, and
//! the labels for roles and target kinds.

use std::collections::BTreeMap;

use crate::tools::scout::model::{Instance, Role, TargetKind, UtilityKind};

pub fn human(kib: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = kib as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Per-instance role aggregation: `(label, count, total_pss)`, heaviest first.
pub fn role_breakdown(instance: &Instance) -> Vec<(String, usize, u64)> {
    let mut aggregate: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    for process in &instance.processes {
        let entry = aggregate.entry(role_label(&process.role)).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += process.pss_kib;
    }
    let mut rows: Vec<(String, usize, u64)> =
        aggregate.into_iter().map(|(label, (count, pss))| (label, count, pss)).collect();
    rows.sort_by(|a, b| b.2.cmp(&a.2));
    rows
}

pub fn role_label(role: &Role) -> String {
    match role {
        Role::Browser => "browser".to_owned(),
        Role::Renderer => "renderer".to_owned(),
        Role::Gpu => "gpu".to_owned(),
        Role::Utility(kind) => format!("utility:{}", utility_label(kind)),
        Role::FileWatcher => "file-watcher".to_owned(),
        Role::Zygote => "zygote".to_owned(),
        Role::Broker => "broker".to_owned(),
        Role::Unknown => "unknown".to_owned(),
    }
}

/// Non-workbench targets aggregated by kind: `(label, count, total_js_kib)`.
pub fn target_groups(instance: &Instance) -> Vec<(&'static str, usize, u64)> {
    let mut groups: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
    for target in &instance.targets {
        if matches!(target.kind, TargetKind::Workbench { .. }) {
            continue;
        }
        let entry = groups.entry(kind_label(&target.kind)).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += target.js_heap_kib.unwrap_or(0);
    }
    groups.into_iter().map(|(label, (count, js))| (label, count, js)).collect()
}

pub fn kind_label(kind: &TargetKind) -> &'static str {
    match kind {
        TargetKind::Workbench { .. } => "workbench",
        TargetKind::ExtensionWebview => "ext-webview",
        TargetKind::Webview => "webview",
        TargetKind::BackgroundWorker => "bg-worker",
        TargetKind::Worker => "worker",
        TargetKind::Page => "page",
        TargetKind::Other => "other",
    }
}

fn utility_label(kind: &UtilityKind) -> &str {
    match kind {
        UtilityKind::Network => "network",
        UtilityKind::Storage => "storage",
        UtilityKind::Audio => "audio",
        UtilityKind::Node => "node",
        UtilityKind::Other(name) => name,
    }
}
