//! Target plane: for each instance that exposes a CDP port, discover its windows/webviews/workers,
//! probe each for JS heap + DOM, and best-effort correlate the heaviest renderers to them.

mod client;
mod discovery;
mod http;
mod ports;

use futures_util::{stream, StreamExt};

use crate::tools::scout::model::{Instance, Role, Target, TargetKind};

const PROBE_CONCURRENCY: usize = 8;

pub async fn enrich(instances: &mut [Instance]) {
    let mains: Vec<u32> = instances.iter().map(|instance| instance.root_pid).collect();
    let ports_by_pid = ports::listening_ports(&mains);

    for instance in instances.iter_mut() {
        let Some(port) = resolve_cdp_port(ports_by_pid.get(&instance.root_pid)).await else {
            continue;
        };
        instance.debug_port = Some(port);

        let probeable: Vec<discovery::RawTarget> = discovery::fetch_targets(port)
            .await
            .into_iter()
            .filter(discovery::RawTarget::is_probeable)
            .collect();

        let mut targets = probe_all(probeable).await;
        correlate(instance, &mut targets);
        targets.sort_by(|a, b| b.js_heap_kib.unwrap_or(0).cmp(&a.js_heap_kib.unwrap_or(0)));
        instance.targets = targets;
    }
}

/// The first of a main's listening ports that actually speaks CDP.
async fn resolve_cdp_port(candidates: Option<&Vec<u16>>) -> Option<u16> {
    for &port in candidates? {
        if discovery::is_cdp(port).await {
            return Some(port);
        }
    }
    None
}

async fn probe_all(raw: Vec<discovery::RawTarget>) -> Vec<Target> {
    stream::iter(raw)
        .map(|target| async move {
            let metrics = match &target.ws_url {
                Some(ws_url) => client::probe(ws_url).await,
                None => return None,
            };
            Some(Target {
                kind: target.classify(),
                js_heap_kib: metrics.js_heap_kib,
                dom_nodes: metrics.dom_nodes,
                listeners: metrics.listeners,
                documents: metrics.documents,
                id: target.id,
                title: target.title,
                url: target.url,
                pid: None,
            })
        })
        .buffer_unordered(PROBE_CONCURRENCY)
        .filter_map(|target| async move { target })
        .collect()
        .await
}

/// Rank-match: the heaviest workbench windows belong to the heaviest renderer processes. CDP gives
/// no pid per target and `/proc` gives no url per pid, so we pair them by descending size. Records
/// the chosen pid on each window and the window's id on each matched renderer.
fn correlate(instance: &mut Instance, targets: &mut [Target]) {
    let mut renderers: Vec<(u32, u64)> = instance
        .processes
        .iter()
        .filter(|process| process.role == Role::Renderer)
        .map(|process| (process.pid, process.pss_kib))
        .collect();
    renderers.sort_by(|a, b| b.1.cmp(&a.1));

    let mut windows: Vec<usize> = targets
        .iter()
        .enumerate()
        .filter(|(_, target)| matches!(target.kind, TargetKind::Workbench { .. }))
        .map(|(index, _)| index)
        .collect();
    windows.sort_by(|&a, &b| {
        targets[b].js_heap_kib.unwrap_or(0).cmp(&targets[a].js_heap_kib.unwrap_or(0))
    });

    for (rank, &index) in windows.iter().enumerate() {
        let Some(&(pid, _)) = renderers.get(rank) else {
            break;
        };
        targets[index].pid = Some(pid);
        if let Some(process) = instance.processes.iter_mut().find(|process| process.pid == pid) {
            process.target_id = Some(targets[index].id.clone());
        }
    }
}
