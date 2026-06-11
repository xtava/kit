//! scout's target plane: for each instance that exposes a CDP port, list its targets via the shared
//! [`crate::cdp`] engine, probe each for JS heap + DOM, classify them into scout's recon-flavoured
//! [`TargetKind`] (Workbench/workspace), and best-effort correlate the heaviest renderers to them.
//!
//! The protocol lives in `crate::cdp`; what's scout-specific is the *recon meaning* — the workspace
//! grouping and the renderer↔window correlation.

use futures_util::{stream, StreamExt};

use crate::cdp;
use crate::tools::scout::model::{Instance, Role, Target, TargetKind};

const PROBE_CONCURRENCY: usize = 8;

pub async fn enrich(instances: &mut [Instance]) {
    let mains: Vec<u32> = instances.iter().map(|instance| instance.root_pid).collect();
    let ports_by_pid = cdp::listening_ports(&mains);

    for instance in instances.iter_mut() {
        let Some(port) = resolve_cdp_port(ports_by_pid.get(&instance.root_pid)).await else {
            continue;
        };
        instance.debug_port = Some(port);

        let probeable: Vec<cdp::Target> = cdp::targets(port)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(cdp::Target::is_inspectable)
            .collect();

        let mut targets = probe_all(probeable).await;
        correlate(instance, &mut targets);
        targets.sort_by_key(|target| std::cmp::Reverse(target.js_heap_kib.unwrap_or(0)));
        instance.targets = targets;
    }
}

/// The first of a main's listening ports that actually speaks browser-level CDP.
async fn resolve_cdp_port(candidates: Option<&Vec<u16>>) -> Option<u16> {
    for &port in candidates? {
        if cdp::is_cdp(port).await {
            return Some(port);
        }
    }
    None
}

async fn probe_all(raw: Vec<cdp::Target>) -> Vec<Target> {
    stream::iter(raw)
        .map(|target| async move {
            let ws_url = target.ws_url.clone()?;
            let metrics = cdp::probe_target(&ws_url).await;
            Some(Target {
                kind: classify(&target),
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

/// Map a generic engine target to scout's recon-flavoured kind — the workspace/workbench meaning
/// the engine deliberately doesn't carry.
fn classify(target: &cdp::Target) -> TargetKind {
    if target.url.starts_with("vscode-webview://") || target.url.starts_with("chrome-extension://")
    {
        TargetKind::ExtensionWebview
    } else if let Some(workspace) = workspace_id(&target.url) {
        TargetKind::Workbench { workspace }
    } else if target.url.contains("background-worker") {
        TargetKind::BackgroundWorker
    } else if matches!(
        target.kind,
        cdp::TargetKind::Worker | cdp::TargetKind::SharedWorker | cdp::TargetKind::ServiceWorker
    ) {
        TargetKind::Worker
    } else if target.kind == cdp::TargetKind::Webview {
        TargetKind::Webview
    } else if matches!(target.kind, cdp::TargetKind::Page | cdp::TargetKind::Iframe) {
        TargetKind::Page
    } else {
        TargetKind::Other
    }
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
    renderers.sort_by_key(|renderer| std::cmp::Reverse(renderer.1));

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

fn workspace_id(url: &str) -> Option<String> {
    let id = url.split("/workspace/").nth(1)?.split('/').next()?;
    (!id.is_empty()).then(|| id.to_owned())
}
