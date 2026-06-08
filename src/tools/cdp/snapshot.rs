//! Accessibility-tree snapshot with a ref system. `Accessibility.getFullAXTree` → a compact,
//! indented tree an agent can read, with `@eN` refs on the interactive and named-content nodes that
//! `click`/`fill` then resolve to backend DOM nodes. Refs are document-scoped — re-snap after a
//! navigation (the daemon invalidates them on url change).

use serde::Serialize;
use serde_json::Value;

const INTERACTIVE: &[&str] = &[
    "button", "link", "textbox", "checkbox", "radio", "combobox", "listbox", "menuitem",
    "menuitemcheckbox", "menuitemradio", "option", "searchbox", "slider", "spinbutton", "switch",
    "tab", "treeitem", "scrollbar",
];
const CONTENT: &[&str] = &[
    "heading", "cell", "columnheader", "rowheader", "listitem", "img", "article", "banner",
    "navigation", "main", "complementary", "contentinfo", "form", "region", "alert", "alertdialog",
    "dialog", "status", "tooltip",
];
const SKIP: &[&str] = &["InlineTextBox", "LineBreak", "StaticText", "none", "generic", "GenericContainer"];
const STRUCTURAL: &[&str] = &["RootWebArea", "WebArea", "document"];

#[derive(Serialize)]
pub struct RefEntry {
    pub reference: String,
    pub backend: i64,
    pub role: String,
    pub name: String,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub text: String,
    pub refs: Vec<RefEntry>,
}

struct Node {
    role: String,
    name: String,
    value: Option<String>,
    props: Vec<(String, String)>,
    backend: Option<i64>,
    children: Vec<usize>,
    reference: Option<String>,
}

/// Build a snapshot from a `getFullAXTree` result. In `interactive` mode only ref-bearing nodes
/// (and their structural ancestors) are rendered — the compact view.
pub fn build(tree: &Value, interactive: bool) -> Snapshot {
    let raw = tree.get("nodes").and_then(Value::as_array).cloned().unwrap_or_default();

    let mut index = std::collections::HashMap::new();
    let mut nodes: Vec<Node> = Vec::new();
    let mut child_ids: Vec<Vec<String>> = Vec::new();
    let mut parent_ids: Vec<Option<String>> = Vec::new();

    for node in &raw {
        if node.get("ignored").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let role = ax_string(node, "role");
        if SKIP.contains(&role.as_str()) {
            continue;
        }
        let id = node.get("nodeId").and_then(Value::as_str).unwrap_or_default().to_owned();
        index.insert(id, nodes.len());
        child_ids.push(string_list(node, "childIds"));
        parent_ids.push(node.get("parentId").and_then(Value::as_str).map(str::to_owned));
        nodes.push(Node {
            role,
            name: ax_string(node, "name"),
            value: ax_opt(node, "value"),
            props: properties(node),
            backend: node.get("backendDOMNodeId").and_then(Value::as_i64),
            children: Vec::new(),
            reference: None,
        });
    }

    for (position, ids) in child_ids.iter().enumerate() {
        nodes[position].children = ids.iter().filter_map(|id| index.get(id).copied()).collect();
    }
    let roots: Vec<usize> = (0..nodes.len())
        .filter(|&position| match &parent_ids[position] {
            None => true,
            Some(parent) => !index.contains_key(parent),
        })
        .collect();

    let order = dfs_order(&roots, &nodes);
    let refs = assign_refs(&order, &mut nodes);

    let mut lines = Vec::new();
    for &root in &roots {
        render(root, &nodes, 0, interactive, &mut lines);
    }

    Snapshot { text: lines.join("\n"), refs }
}

fn dfs_order(roots: &[usize], nodes: &[Node]) -> Vec<usize> {
    let mut order = Vec::new();
    let mut seen = vec![false; nodes.len()];
    let mut stack: Vec<usize> = roots.iter().rev().copied().collect();
    while let Some(position) = stack.pop() {
        if seen[position] {
            continue;
        }
        seen[position] = true;
        order.push(position);
        for &child in nodes[position].children.iter().rev() {
            stack.push(child);
        }
    }
    order
}

fn assign_refs(order: &[usize], nodes: &mut [Node]) -> Vec<RefEntry> {
    let mut refs = Vec::new();
    let mut next = 1;
    for &position in order {
        if !is_candidate(&nodes[position]) {
            continue;
        }
        let reference = format!("e{next}");
        next += 1;
        let backend = nodes[position].backend.expect("candidate has backend");
        refs.push(RefEntry {
            reference: reference.clone(),
            backend,
            role: nodes[position].role.clone(),
            name: nodes[position].name.clone(),
        });
        nodes[position].reference = Some(reference);
    }
    refs
}

fn is_candidate(node: &Node) -> bool {
    node.backend.is_some()
        && (INTERACTIVE.contains(&node.role.as_str())
            || (CONTENT.contains(&node.role.as_str()) && !node.name.is_empty()))
}

fn render(position: usize, nodes: &[Node], depth: usize, interactive: bool, out: &mut Vec<String>) {
    let node = &nodes[position];
    if STRUCTURAL.contains(&node.role.as_str()) {
        for &child in &node.children {
            render(child, nodes, depth, interactive, out);
        }
        return;
    }

    let show = !interactive || node.reference.is_some() || has_ref_descendant(position, nodes);
    if show {
        let mut parts: Vec<String> = Vec::new();
        if let Some(reference) = &node.reference {
            parts.push(format!("[@{reference}]"));
        }
        parts.push(node.role.clone());
        if !node.name.is_empty() {
            parts.push(format!("\"{}\"", truncate(&node.name, 80)));
        }
        if let Some(value) = &node.value {
            parts.push(format!("value=\"{}\"", truncate(value, 60)));
        }
        for (key, value) in &node.props {
            if value == "true" {
                parts.push(key.clone());
            } else {
                parts.push(format!("{key}={value}"));
            }
        }
        out.push(format!("{}- {}", "  ".repeat(depth), parts.join(" ")));
    }

    let child_depth = if show { depth + 1 } else { depth };
    for &child in &node.children {
        render(child, nodes, child_depth, interactive, out);
    }
}

fn has_ref_descendant(position: usize, nodes: &[Node]) -> bool {
    nodes[position]
        .children
        .iter()
        .any(|&child| nodes[child].reference.is_some() || has_ref_descendant(child, nodes))
}

fn properties(node: &Value) -> Vec<(String, String)> {
    let Some(props) = node.get("properties").and_then(Value::as_array) else {
        return Vec::new();
    };
    props
        .iter()
        .filter_map(|prop| {
            let name = prop.get("name").and_then(Value::as_str)?;
            let value = prop.get("value").and_then(|value| value.get("value"))?;
            let rendered = match value {
                Value::Bool(flag) => flag.to_string(),
                Value::Number(number) => number.to_string(),
                Value::String(text) => text.clone(),
                _ => return None,
            };
            let keep = matches!(
                name,
                "checked" | "expanded" | "selected" | "disabled" | "pressed" | "required"
                    | "level" | "valuenow" | "haspopup" | "invalid"
            ) && rendered != "false"
                && rendered != "none";
            keep.then(|| (name.to_owned(), rendered))
        })
        .collect()
}

fn ax_string(node: &Value, field: &str) -> String {
    node.get(field).and_then(|field| field.get("value")).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn ax_opt(node: &Value, field: &str) -> Option<String> {
    let value = node.get(field)?.get("value")?;
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn string_list(node: &Value, field: &str) -> Vec<String> {
    node.get(field)
        .and_then(Value::as_array)
        .map(|array| array.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}
