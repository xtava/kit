//! Pure process-family projection for the interactive investigator.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use super::model::{ProcessIdentity, ProcessSample};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeSort {
    Cpu,
    Memory,
    Pid,
    Name,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TreeRow {
    pub key: ProcessIdentity,
    pub depth: u16,
    pub has_children: bool,
    pub hidden_descendants: usize,
    pub family_cpu_percent: f32,
    pub family_memory_bytes: u64,
    pub is_match: bool,
    pub is_context: bool,
}

pub struct TreeQuery<'a> {
    pub collapsed: &'a HashSet<ProcessIdentity>,
    pub focus: Option<ProcessIdentity>,
    pub filter: &'a str,
    pub sort: TreeSort,
    pub descending: bool,
}

/// Repairs the sampled parent relation into a deterministic forest and projects visible rows.
///
/// Missing parents become roots. Cycles are cut at the first not-yet-emitted node in sibling order.
/// Filtering retains matching rows and their ancestors. Collapse only hides descendants when no
/// active filter needs an ancestry path through that branch.
pub fn project(processes: &[ProcessSample], query: TreeQuery<'_>) -> Vec<TreeRow> {
    if processes.is_empty() {
        return Vec::new();
    }

    let by_key =
        processes.iter().map(|process| (process.identity, process)).collect::<HashMap<_, _>>();
    let by_pid = processes
        .iter()
        .map(|process| (process.identity.pid(), process.identity))
        .collect::<HashMap<_, _>>();
    let mut children = HashMap::<Option<ProcessIdentity>, Vec<ProcessIdentity>>::new();
    let mut parent = HashMap::<ProcessIdentity, Option<ProcessIdentity>>::new();
    for process in processes {
        let process_parent = process
            .parent_pid
            .and_then(|pid| by_pid.get(&pid).copied())
            .filter(|key| *key != process.identity);
        parent.insert(process.identity, process_parent);
        children.entry(process_parent).or_default().push(process.identity);
    }
    for siblings in children.values_mut() {
        siblings
            .sort_by(|left, right| compare(*left, *right, &by_key, query.sort, query.descending));
    }

    let mut roots = children.get(&None).cloned().unwrap_or_default();
    let mut all = processes.iter().map(|process| process.identity).collect::<Vec<_>>();
    all.sort_by(|left, right| compare(*left, *right, &by_key, query.sort, query.descending));
    repair_unreachable_roots(&mut roots, &all, &parent);

    let (family_cpu, family_memory, descendants) = aggregate(&roots, &children, &by_key);
    let filter = query.filter.trim().to_ascii_lowercase();
    let matches = processes
        .iter()
        .filter(|process| matches_filter(process, &filter))
        .map(|process| process.identity)
        .collect::<HashSet<_>>();
    let mut required = matches.clone();
    if !filter.is_empty() {
        for key in &matches {
            let mut cursor = parent.get(key).copied().flatten();
            let mut seen = HashSet::new();
            while let Some(ancestor) = cursor {
                if !seen.insert(ancestor) {
                    break;
                }
                required.insert(ancestor);
                cursor = parent.get(&ancestor).copied().flatten();
            }
        }
    }

    let projection_roots = query.focus.map_or_else(|| roots.clone(), |key| vec![key]);
    let mut rows = Vec::new();
    let mut emitted = HashSet::new();
    let mut stack =
        projection_roots.iter().rev().copied().map(|key| (key, 0_u16)).collect::<Vec<_>>();
    while let Some((key, depth)) = stack.pop() {
        if !by_key.contains_key(&key) || !emitted.insert(key) {
            continue;
        }
        if !filter.is_empty() && !required.contains(&key) {
            continue;
        }
        let process_children = children.get(&Some(key)).map(Vec::as_slice).unwrap_or_default();
        rows.push(TreeRow {
            key,
            depth,
            has_children: !process_children.is_empty(),
            hidden_descendants: descendants.get(&key).copied().unwrap_or_default(),
            family_cpu_percent: family_cpu.get(&key).copied().unwrap_or_default(),
            family_memory_bytes: family_memory.get(&key).copied().unwrap_or_default(),
            is_match: !filter.is_empty() && matches.contains(&key),
            is_context: !filter.is_empty() && required.contains(&key) && !matches.contains(&key),
        });
        let collapsed = query.collapsed.contains(&key) && filter.is_empty();
        if !collapsed {
            stack.extend(process_children.iter().rev().copied().map(|child| (child, depth + 1)));
        }
    }
    rows
}

fn compare(
    left: ProcessIdentity,
    right: ProcessIdentity,
    processes: &HashMap<ProcessIdentity, &ProcessSample>,
    sort: TreeSort,
    descending: bool,
) -> Ordering {
    let left_process = processes[&left];
    let right_process = processes[&right];
    let order = match sort {
        TreeSort::Cpu => left_process.cpu_percent.total_cmp(&right_process.cpu_percent),
        TreeSort::Memory => left_process.rss_bytes.cmp(&right_process.rss_bytes),
        TreeSort::Pid => left.pid().cmp(&right.pid()),
        TreeSort::Name => {
            left_process.name.to_ascii_lowercase().cmp(&right_process.name.to_ascii_lowercase())
        }
    }
    .then_with(|| left.pid().cmp(&right.pid()));
    if descending {
        order.reverse()
    } else {
        order
    }
}

fn repair_unreachable_roots(
    roots: &mut Vec<ProcessIdentity>,
    ordered: &[ProcessIdentity],
    parent: &HashMap<ProcessIdentity, Option<ProcessIdentity>>,
) {
    let mut reachable = HashSet::new();
    for root in roots.iter().copied() {
        reachable.insert(root);
    }
    for start in ordered.iter().copied() {
        if reachable.contains(&start) {
            continue;
        }
        let mut cursor = Some(start);
        let mut path = Vec::<ProcessIdentity>::new();
        let mut positions = HashMap::new();
        while let Some(key) = cursor {
            if reachable.contains(&key) {
                break;
            }
            if let Some(position) = positions.insert(key, path.len()) {
                let cycle_root = path[position..]
                    .iter()
                    .copied()
                    .min_by_key(|candidate| candidate.pid())
                    .expect("cycle path is non-empty");
                roots.push(cycle_root);
                break;
            }
            path.push(key);
            cursor = parent.get(&key).copied().flatten();
        }
        reachable.extend(path);
    }
}

fn aggregate(
    roots: &[ProcessIdentity],
    children: &HashMap<Option<ProcessIdentity>, Vec<ProcessIdentity>>,
    processes: &HashMap<ProcessIdentity, &ProcessSample>,
) -> (HashMap<ProcessIdentity, f32>, HashMap<ProcessIdentity, u64>, HashMap<ProcessIdentity, usize>)
{
    let mut cpu = HashMap::new();
    let mut memory = HashMap::new();
    let mut descendants = HashMap::new();
    let mut visited = HashSet::new();
    let mut stack = roots.iter().rev().copied().map(|key| (key, false)).collect::<Vec<_>>();
    while let Some((key, exiting)) = stack.pop() {
        if exiting {
            let Some(process) = processes.get(&key) else { continue };
            let mut family_cpu = process.cpu_percent;
            let mut family_memory = process.rss_bytes;
            let mut family_descendants = 0;
            for child in children.get(&Some(key)).into_iter().flatten() {
                family_cpu += cpu.get(child).copied().unwrap_or_default();
                family_memory =
                    family_memory.saturating_add(memory.get(child).copied().unwrap_or_default());
                family_descendants += 1 + descendants.get(child).copied().unwrap_or_default();
            }
            cpu.insert(key, family_cpu);
            memory.insert(key, family_memory);
            descendants.insert(key, family_descendants);
            continue;
        }
        if !visited.insert(key) {
            continue;
        }
        stack.push((key, true));
        stack.extend(
            children
                .get(&Some(key))
                .into_iter()
                .flatten()
                .rev()
                .copied()
                .map(|child| (child, false)),
        );
    }
    (cpu, memory, descendants)
}

fn matches_filter(process: &ProcessSample, filter: &str) -> bool {
    filter.is_empty()
        || process.name.to_ascii_lowercase().contains(filter)
        || process.command.to_ascii_lowercase().contains(filter)
        || process.identity.pid().to_string().contains(filter)
        || process.user.as_deref().unwrap_or_default().to_ascii_lowercase().contains(filter)
}

#[cfg(test)]
mod tests {
    use super::super::model::{ProcessKey, ProcessState};

    use super::*;

    fn process(pid: u32, parent_pid: Option<u32>, name: &str, cpu: f32) -> ProcessSample {
        ProcessSample {
            identity: ProcessIdentity::stable(ProcessKey { pid, start_token: pid as u64 }),
            parent_pid,
            name: name.into(),
            command: format!("/bin/{name}"),
            user: Some("user".into()),
            state: ProcessState::Running,
            cpu_percent: cpu,
            rss_bytes: pid as u64 * 100,
            started_at_ms: 0,
            run_time_seconds: 1,
            last_cpu: Some(0),
        }
    }

    fn query<'a>(collapsed: &'a HashSet<ProcessIdentity>, filter: &'a str) -> TreeQuery<'a> {
        TreeQuery { collapsed, focus: None, filter, sort: TreeSort::Cpu, descending: true }
    }

    #[test]
    fn collapse_keeps_family_totals_and_hides_descendants() {
        let processes = vec![process(1, None, "root", 1.0), process(2, Some(1), "child", 2.0)];
        let collapsed = HashSet::from([processes[0].identity]);
        let rows = project(&processes, query(&collapsed, ""));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hidden_descendants, 1);
        assert_eq!(rows[0].family_cpu_percent, 3.0);
        assert_eq!(rows[0].family_memory_bytes, 300);
    }

    #[test]
    fn filter_preserves_ancestors_and_marks_context() {
        let processes = vec![
            process(1, None, "root", 1.0),
            process(2, Some(1), "branch", 2.0),
            process(3, Some(2), "needle", 3.0),
            process(4, None, "other", 4.0),
        ];
        let rows = project(&processes, query(&HashSet::new(), "needle"));
        assert_eq!(rows.iter().map(|row| row.key.pid()).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert!(rows[0].is_context && rows[1].is_context && rows[2].is_match);
    }

    #[test]
    fn orphan_and_cycle_are_repaired_without_duplicates() {
        let processes = vec![
            process(5, Some(99), "orphan", 5.0),
            process(7, Some(8), "cycle-a", 7.0),
            process(8, Some(7), "cycle-b", 8.0),
        ];
        let rows = project(&processes, query(&HashSet::new(), ""));
        let keys = rows.iter().map(|row| row.key).collect::<HashSet<_>>();
        assert_eq!(rows.len(), 3);
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn focus_projects_only_the_selected_family() {
        let processes = vec![
            process(1, None, "root", 1.0),
            process(2, Some(1), "child", 2.0),
            process(3, None, "other", 3.0),
        ];
        let collapsed = HashSet::new();
        let mut query = query(&collapsed, "");
        query.focus = Some(processes[0].identity);
        let rows = project(&processes, query);
        assert_eq!(rows.iter().map(|row| row.key.pid()).collect::<Vec<_>>(), vec![1, 2]);
    }
}
