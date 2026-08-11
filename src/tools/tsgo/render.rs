use std::{fmt::Write as _, path::Path};

use super::protocol::{ServiceInfo, TraceEdge, TraceLocation, TraceResult};

pub fn trace_text(service: &ServiceInfo, result: &TraceResult) -> String {
    let title = result
        .target
        .as_ref()
        .and_then(|id| result.nodes.get(id))
        .map(|node| node.name.as_str())
        .unwrap_or(result.selector.as_str());
    let workspace_name = display_workspace(&service.workspace);
    let status = title_case(&result.status);
    let resolved = result
        .target
        .as_ref()
        .and_then(|id| result.nodes.get(id))
        .map(|node| location_text(&service.workspace, &node.definition))
        .unwrap_or_else(|| "—".to_owned());

    let mut output = String::new();
    let _ = writeln!(output, "Call Trace › {title}\n");
    let _ = writeln!(output, "Trace Details");
    detail(&mut output, "Workspace", &workspace_name);
    detail(&mut output, "Direction", result.direction.label());
    detail(&mut output, "Status", &status);
    detail(&mut output, "Resolved Target", &resolved);
    detail(&mut output, "Elapsed", &format!("{} ms", result.timing.elapsed_ms));
    detail(&mut output, "Instance", &service.instance_id);
    detail(&mut output, "Child", &service.child.run_id);
    detail(&mut output, "Request", &service.request_count.to_string());

    let _ = writeln!(output, "\nSummary");
    let _ = writeln!(
        output,
        "{} Roots  ·  {} Paths  ·  {} Nodes  ·  {} Edges",
        result.summary.roots,
        result.summary.paths,
        result.summary.nodes,
        result.summary.edges
    );
    if result.summary.cycles > 0 || result.summary.boundaries > 0 || result.summary.truncated {
        let _ = writeln!(
            output,
            "{} Cycles  ·  {} Boundaries  ·  {}",
            result.summary.cycles,
            result.summary.boundaries,
            if result.summary.truncated { "Truncated" } else { "Complete" }
        );
    }

    if !result.candidates.is_empty() && result.target.is_none() {
        let _ = writeln!(output, "\nCandidates");
        for candidate in &result.candidates {
            let name = candidate
                .detail
                .as_ref()
                .map(|detail| format!("{}  {detail}", candidate.name))
                .unwrap_or_else(|| candidate.name.clone());
            let _ = writeln!(
                output,
                "{:<44} {}",
                name,
                location_text(&service.workspace, &candidate.location)
            );
        }
    }

    if !result.paths.is_empty() {
        let _ = writeln!(output, "\nCall Paths");
        for (path_index, path) in result.paths.iter().enumerate() {
            if path_index > 0 {
                output.push('\n');
            }
            let _ = writeln!(output, "# path {}", path_index + 1);
            for (node_index, node_id) in path.nodes.iter().enumerate() {
                let Some(node) = result.nodes.get(node_id) else {
                    continue;
                };
                let prefix = if node_index == 0 {
                    String::new()
                } else {
                    format!("{}└─ ", "   ".repeat(node_index - 1))
                };
                let location = path_location(
                    &service.workspace,
                    &result.edges,
                    &path.nodes,
                    node_index,
                    &node.definition,
                );
                let cycle = if path.cycle && node_index + 1 == path.nodes.len() {
                    "  ⇄"
                } else {
                    ""
                };
                let label = format!("{prefix}{}()", node.name);
                let _ = writeln!(output, "{label:<48} {location}{cycle}");
            }
        }
    }

    if !result.truncation_reasons.is_empty() {
        let _ = writeln!(output, "\nLimits");
        for reason in &result.truncation_reasons {
            let _ = writeln!(output, "- {reason}");
        }
    }

    output.trim_end().to_owned()
}

fn detail(output: &mut String, label: &str, value: &str) {
    let _ = writeln!(output, "{label:<17}{value}");
}

fn path_location(
    workspace: &Path,
    edges: &[TraceEdge],
    nodes: &[String],
    index: usize,
    definition: &TraceLocation,
) -> String {
    if let Some(next) = nodes.get(index + 1) {
        if let Some(site) = edges
            .iter()
            .find(|edge| edge.caller == nodes[index] && edge.callee == *next)
            .and_then(|edge| edge.call_sites.first())
        {
            return location_text(workspace, site);
        }
    }
    location_text(workspace, definition)
}

fn location_text(workspace: &Path, location: &TraceLocation) -> String {
    let file = location.file.strip_prefix(workspace).unwrap_or(&location.file);
    format!("{}:{}", file.display(), location.line)
}

fn display_workspace(workspace: &Path) -> String {
    let raw = workspace
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace");
    let mut characters = raw.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => "Workspace".to_owned(),
    }
}

fn title_case(value: &str) -> String {
    value
        .split('-')
        .map(|word| {
            let mut characters = word.chars();
            match characters.next() {
                Some(first) => first.to_uppercase().chain(characters).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
