//! Accessibility-tree snapshot with a ref system. `Accessibility.getFullAXTree` → a compact,
//! indented tree an agent can read, with `@eN` refs on the interactive and named-content nodes that
//! `click`/`fill` then resolve to backend DOM nodes. Refs are document-scoped — re-snap after a
//! navigation (the daemon invalidates them on url change).

use serde::Serialize;
use serde_json::Value;

const INTERACTIVE: &[&str] = &[
    "button",
    "link",
    "textbox",
    "checkbox",
    "radio",
    "combobox",
    "listbox",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "searchbox",
    "slider",
    "spinbutton",
    "switch",
    "tab",
    "treeitem",
    "scrollbar",
];
const CONTENT: &[&str] = &[
    "heading",
    "cell",
    "columnheader",
    "rowheader",
    "listitem",
    "img",
    "article",
    "banner",
    "navigation",
    "main",
    "complementary",
    "contentinfo",
    "form",
    "region",
    "alert",
    "alertdialog",
    "dialog",
    "status",
    "tooltip",
];
const SKIP: &[&str] = &["InlineTextBox", "LineBreak", "none", "generic", "GenericContainer"];
const STRUCTURAL: &[&str] = &["RootWebArea", "WebArea", "document"];
/// Visible text fragments: pure noise in the rendered tree and never ref-bearing, but kept for
/// semantic diff lines — a text change is exactly what a diff must see.
const TEXT: &str = "StaticText";

#[derive(Clone, Debug, Serialize)]
pub struct RefEntry {
    pub reference: String,
    pub backend: i64,
    pub role: String,
    pub name: String,
}

impl RefEntry {
    /// How an element reads in command output: `button "Save settings" @e23`.
    pub fn display(&self) -> String {
        format!("{} \"{}\" @{}", self.role, self.name.trim(), self.reference)
    }
}

/// Whether `role` is one a snapshot can assign a ref to — the vocabulary a `role:name` locator
/// may scope itself by.
pub fn is_known_role(role: &str) -> bool {
    let role = role.to_lowercase();
    INTERACTIVE.contains(&role.as_str()) || CONTENT.contains(&role.as_str())
}

/// Find the unique ref matching a role/name query. Exact name matches (trimmed,
/// case-insensitive) beat substring matches; zero or many is an error that says what *was* there.
pub fn find<'a>(
    refs: &'a [RefEntry],
    role: Option<&str>,
    name: &str,
) -> Result<&'a RefEntry, String> {
    let wanted = name.trim().to_lowercase();
    // An empty needle substring-matches everything; the real cause is almost always a selector
    // pipeline (grep over a snap) that found nothing — blame that, not the empty string.
    if wanted.is_empty() {
        return Err(
            "empty locator — if a script built this from a grep over `snap`, the grep matched \
             nothing; re-run `kit cdp snap -i` and check the label"
                .to_owned(),
        );
    }
    let pool: Vec<&RefEntry> = refs
        .iter()
        .filter(|entry| role.is_none_or(|role| entry.role.eq_ignore_ascii_case(role)))
        .collect();

    let exact: Vec<&RefEntry> =
        pool.iter().filter(|entry| entry.name.trim().to_lowercase() == wanted).copied().collect();
    let matches = if exact.is_empty() {
        pool.iter().filter(|entry| entry.name.to_lowercase().contains(&wanted)).copied().collect()
    } else {
        exact
    };

    match matches.as_slice() {
        [only] => Ok(only),
        [] => Err(format!(
            "no {} matching '{}' — run `kit cdp snap -i` to see what's on screen",
            role.unwrap_or("element"),
            name.trim()
        )),
        many => {
            let listed: Vec<String> =
                many.iter().take(8).map(|entry| format!("  {}", entry.display())).collect();
            Err(format!("'{}' is ambiguous — candidates:\n{}", name.trim(), listed.join("\n")))
        }
    }
}

#[derive(Serialize)]
pub struct Snapshot {
    pub text: String,
    pub refs: Vec<RefEntry>,
    /// Ref-free, indent-free line per node over the *full* tree — the identity `diff` compares.
    /// Refs renumber and depths shift when unrelated nodes appear; these lines don't.
    #[serde(skip)]
    pub semantic: Vec<String>,
}

/// What changed between two snapshots, by ref-free line identity. A multiset diff: reordering
/// alone is not a change, and a value change reads as one removal plus one addition.
#[derive(Serialize)]
pub struct SnapDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: usize,
}

/// Diff two semantic-line sets. Additions keep new-tree order, removals old-tree order.
pub fn diff(old: &[String], new: &[String]) -> SnapDiff {
    let mut balance: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
    for line in old {
        *balance.entry(line).or_insert(0) -= 1;
    }
    for line in new {
        *balance.entry(line).or_insert(0) += 1;
    }

    let mut surplus = balance.clone();
    let added: Vec<String> = new
        .iter()
        .filter(|line| {
            let count = surplus.get_mut(line.as_str()).expect("every new line is counted");
            let extra = *count > 0;
            if extra {
                *count -= 1;
            }
            extra
        })
        .cloned()
        .collect();
    let removed: Vec<String> = old
        .iter()
        .filter(|line| {
            let count = balance.get_mut(line.as_str()).expect("every old line is counted");
            let missing = *count < 0;
            if missing {
                *count += 1;
            }
            missing
        })
        .cloned()
        .collect();
    let unchanged = new.len() - added.len();
    SnapDiff { added, removed, unchanged }
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
    let semantic = order
        .iter()
        .filter(|&&position| !STRUCTURAL.contains(&nodes[position].role.as_str()))
        .map(|&position| line(&nodes[position], false))
        .collect();

    Snapshot { text: lines.join("\n"), refs, semantic }
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
        && node.role != TEXT
        && (INTERACTIVE.contains(&node.role.as_str())
            || (CONTENT.contains(&node.role.as_str()) && !node.name.is_empty()))
}

fn render(position: usize, nodes: &[Node], depth: usize, interactive: bool, out: &mut Vec<String>) {
    let node = &nodes[position];
    if STRUCTURAL.contains(&node.role.as_str()) || node.role == TEXT {
        for &child in &node.children {
            render(child, nodes, depth, interactive, out);
        }
        return;
    }

    let show = !interactive || node.reference.is_some() || has_ref_descendant(position, nodes);
    if show {
        out.push(format!("{}- {}", "  ".repeat(depth), line(node, true)));
    }

    let child_depth = if show { depth + 1 } else { depth };
    for &child in &node.children {
        render(child, nodes, child_depth, interactive, out);
    }
}

/// One node as a line: role, name, value, and the meaningful props — with or without its ref.
fn line(node: &Node, with_ref: bool) -> String {
    if node.role == TEXT {
        return format!("text \"{}\"", truncate(&node.name, 80));
    }
    let mut parts: Vec<String> = Vec::new();
    if with_ref {
        if let Some(reference) = &node.reference {
            parts.push(format!("[@{reference}]"));
        }
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
    parts.join(" ")
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
                "checked"
                    | "expanded"
                    | "selected"
                    | "disabled"
                    | "pressed"
                    | "required"
                    | "level"
                    | "valuenow"
                    | "haspopup"
                    | "invalid"
            ) && rendered != "false"
                && rendered != "none";
            keep.then(|| (name.to_owned(), rendered))
        })
        .collect()
}

fn ax_string(node: &Value, field: &str) -> String {
    node.get(field)
        .and_then(|field| field.get("value"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(reference: &str, role: &str, name: &str) -> RefEntry {
        RefEntry { reference: reference.into(), backend: 1, role: role.into(), name: name.into() }
    }

    /// An exact name beats a substring superset: `button:Save` must pick "Save", not complain that
    /// "Save settings" also contains it.
    #[test]
    fn exact_name_beats_substring_matches() {
        let refs = [
            entry("e1", "button", "Save settings"),
            entry("e2", "button", "Save"),
            entry("e3", "link", "Save"),
        ];
        assert_eq!(find(&refs, Some("button"), "Save").unwrap().reference, "e2");
        assert_eq!(find(&refs, None, "save settings").unwrap().reference, "e1");
    }

    /// Accessible names often carry label whitespace (`"Name "`); matching must trim both sides.
    #[test]
    fn matching_trims_and_ignores_case() {
        let refs = [entry("e1", "textbox", "Name ")];
        assert_eq!(find(&refs, Some("textbox"), "name").unwrap().reference, "e1");
    }

    #[test]
    fn ambiguity_lists_candidates_and_absence_names_the_role() {
        let refs = [entry("e1", "button", "fetch ok"), entry("e2", "button", "fetch slow")];
        let ambiguous = find(&refs, Some("button"), "fetch").unwrap_err();
        assert!(ambiguous.contains("@e1") && ambiguous.contains("@e2"), "{ambiguous}");
        let missing = find(&refs, Some("button"), "nope").unwrap_err();
        assert!(missing.contains("no button matching 'nope'"), "{missing}");
    }

    /// A role filter scopes the pool before matching — the same name under another role is invisible.
    #[test]
    fn role_filter_scopes_the_pool() {
        let refs = [entry("e1", "button", "go"), entry("e2", "link", "go")];
        assert_eq!(find(&refs, Some("link"), "go").unwrap().reference, "e2");
        assert!(find(&refs, None, "go").is_err(), "two roles share the name — ambiguous");
    }

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    /// The verification-without-assertions primitive: a value change is a remove+add pair, a new
    /// node an addition, and what stayed is counted, not echoed.
    #[test]
    fn diff_reports_changes_as_remove_add_pairs() {
        let old = lines(&["button \"Save\"", "output \"counter\" value=\"3\""]);
        let new = lines(&["button \"Save\"", "output \"counter\" value=\"4\"", "alert \"Saved\""]);
        let changes = diff(&old, &new);
        assert_eq!(changes.removed, lines(&["output \"counter\" value=\"3\""]));
        assert_eq!(changes.added, lines(&["output \"counter\" value=\"4\"", "alert \"Saved\""]));
        assert_eq!(changes.unchanged, 1);
    }

    /// Reordering alone is not a change — refs renumber and layout shifts, but identity holds.
    #[test]
    fn diff_ignores_reordering() {
        let old = lines(&["a", "b", "c"]);
        let new = lines(&["c", "a", "b"]);
        let changes = diff(&old, &new);
        assert!(changes.added.is_empty() && changes.removed.is_empty());
        assert_eq!(changes.unchanged, 3);
    }

    /// Duplicates count: two identical list items shrinking to one is a removal, not "unchanged".
    #[test]
    fn diff_counts_duplicates() {
        let old = lines(&["listitem \"item\"", "listitem \"item\""]);
        let new = lines(&["listitem \"item\""]);
        let changes = diff(&old, &new);
        assert_eq!(changes.removed.len(), 1);
        assert!(changes.added.is_empty());
        assert_eq!(changes.unchanged, 1);
    }
}
