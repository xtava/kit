//! Headless rendering of a [`Survey`] — system line, then per-instance PSS with a role breakdown
//! and, where a debug port was found, the CDP target plane.

use super::format::{human, role_breakdown, target_groups};
use crate::tools::scout::model::{Instance, Survey, TargetKind};
use crate::tools::scout::proc::total_pss;

pub fn print_table(survey: &Survey) {
    let system = &survey.system;
    let used = system.total_kib.saturating_sub(system.available_kib);
    println!(
        "scout · {} instance(s) · system {} / {} · swap {} / {}\n",
        survey.instances.len(),
        human(used),
        human(system.total_kib),
        human(system.swap_used_kib),
        human(system.swap_total_kib),
    );

    for instance in &survey.instances {
        let port = match instance.debug_port {
            Some(port) => format!(":{port}"),
            None => "[no debug]".to_owned(),
        };
        println!(
            "{:<24} {:<11} pid {:<8} {:>10}  {} procs",
            instance.name,
            port,
            instance.root_pid,
            human(total_pss(instance)),
            instance.processes.len(),
        );
        for (label, count, pss) in role_breakdown(instance) {
            println!("    {label:<16} {count:>3} × {:>10}", human(pss));
        }
        print_targets(instance);
        println!();
    }
}

fn print_targets(instance: &Instance) {
    if instance.targets.is_empty() {
        return;
    }
    let total_js: u64 = instance.targets.iter().filter_map(|target| target.js_heap_kib).sum();
    println!("    {:<16} {:>3}   {:>10} js", "targets", instance.targets.len(), human(total_js));

    for target in &instance.targets {
        let TargetKind::Workbench { workspace } = &target.kind else {
            continue;
        };
        println!(
            "        ⊞ workspace {:<10} {:>9} js   {} nodes",
            workspace.chars().take(8).collect::<String>(),
            target.js_heap_kib.map(human).unwrap_or_else(|| "—".to_owned()),
            target.dom_nodes.map(|nodes| nodes.to_string()).unwrap_or_else(|| "—".to_owned()),
        );
    }

    for (label, count, js) in target_groups(instance) {
        println!("        {label:<13} ×{count:<3} {:>10} js", human(js));
    }
}
