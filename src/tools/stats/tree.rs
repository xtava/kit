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

pub struct ProcessForest {
    roots: Vec<ProcessIdentity>,
    by_key: HashMap<ProcessIdentity, usize>,
    parent: HashMap<ProcessIdentity, Option<ProcessIdentity>>,
    children: HashMap<Option<ProcessIdentity>, Vec<ProcessIdentity>>,
    family_cpu: HashMap<ProcessIdentity, f32>,
    family_memory: HashMap<ProcessIdentity, u64>,
    descendants: HashMap<ProcessIdentity, usize>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FamilyMember {
    pub key: ProcessIdentity,
    pub family_cpu_percent: f32,
    pub family_memory_bytes: u64,
    pub descendant_count: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FamilyView {
    pub direct_children: Vec<FamilyMember>,
    pub hot_descendants: Vec<FamilyMember>,
    pub memory_descendants: Vec<FamilyMember>,
    pub descendant_count: usize,
}

impl ProcessForest {
    pub fn new(processes: &[ProcessSample]) -> Self {
        let by_key = processes
            .iter()
            .enumerate()
            .map(|(index, process)| (process.identity, index))
            .collect::<HashMap<_, _>>();
        let by_pid = processes
            .iter()
            .map(|process| (process.identity.pid(), process.identity))
            .collect::<HashMap<_, _>>();
        let mut parent = HashMap::<ProcessIdentity, Option<ProcessIdentity>>::new();
        for process in processes {
            let process_parent = process
                .parent_pid
                .and_then(|pid| by_pid.get(&pid).copied())
                .filter(|key| *key != process.identity);
            parent.insert(process.identity, process_parent);
        }
        let mut all = processes.iter().map(|process| process.identity).collect::<Vec<_>>();
        all.sort_by(identity_cmp);
        let mut roots = all
            .iter()
            .copied()
            .filter(|key| parent.get(key).copied().flatten().is_none())
            .collect::<Vec<_>>();
        repair_unreachable_roots(&mut roots, &all, &mut parent);
        roots.sort_by(identity_cmp);
        roots.dedup();
        let mut children = HashMap::<Option<ProcessIdentity>, Vec<ProcessIdentity>>::new();
        for key in &all {
            children.entry(parent.get(key).copied().flatten()).or_default().push(*key);
        }
        let (family_cpu, family_memory, descendants) =
            aggregate(&roots, &children, &by_key, processes);
        Self { roots, by_key, parent, children, family_cpu, family_memory, descendants }
    }

    pub fn process<'a>(
        &self,
        processes: &'a [ProcessSample],
        key: ProcessIdentity,
    ) -> Option<&'a ProcessSample> {
        self.by_key.get(&key).and_then(|index| processes.get(*index))
    }

    pub fn parent(&self, key: ProcessIdentity) -> Option<ProcessIdentity> {
        self.parent.get(&key).copied().flatten()
    }

    pub fn family_view(
        &self,
        selected: ProcessIdentity,
        processes: &[ProcessSample],
    ) -> Option<FamilyView> {
        self.process(processes, selected)?;
        let member = |key| FamilyMember {
            key,
            family_cpu_percent: self.family_cpu.get(&key).copied().unwrap_or_default(),
            family_memory_bytes: self.family_memory.get(&key).copied().unwrap_or_default(),
            descendant_count: self.descendants.get(&key).copied().unwrap_or_default(),
        };
        let mut direct_children = self
            .children
            .get(&Some(selected))
            .into_iter()
            .flatten()
            .copied()
            .map(member)
            .collect::<Vec<_>>();
        direct_children.sort_by(|left, right| {
            right
                .family_cpu_percent
                .total_cmp(&left.family_cpu_percent)
                .then_with(|| identity_cmp(&left.key, &right.key))
        });

        let mut descendant_keys = Vec::new();
        let mut stack = direct_children.iter().rev().map(|child| child.key).collect::<Vec<_>>();
        let mut seen = HashSet::new();
        while let Some(key) = stack.pop() {
            if !seen.insert(key) {
                continue;
            }
            descendant_keys.push(key);
            stack.extend(self.children.get(&Some(key)).into_iter().flatten().rev().copied());
        }
        let mut hot_descendants = descendant_keys.iter().copied().map(member).collect::<Vec<_>>();
        hot_descendants.sort_by(|left, right| {
            self.process(processes, right.key)
                .expect("forest identity indexes its source snapshot")
                .cpu_percent
                .total_cmp(
                    &self
                        .process(processes, left.key)
                        .expect("forest identity indexes its source snapshot")
                        .cpu_percent,
                )
                .then_with(|| identity_cmp(&left.key, &right.key))
        });
        let mut memory_descendants = descendant_keys.into_iter().map(member).collect::<Vec<_>>();
        memory_descendants.sort_by(|left, right| {
            self.process(processes, right.key)
                .expect("forest identity indexes its source snapshot")
                .rss_bytes
                .cmp(
                    &self
                        .process(processes, left.key)
                        .expect("forest identity indexes its source snapshot")
                        .rss_bytes,
                )
                .then_with(|| identity_cmp(&left.key, &right.key))
        });
        Some(FamilyView {
            direct_children,
            hot_descendants,
            memory_descendants,
            descendant_count: self.descendants.get(&selected).copied().unwrap_or_default(),
        })
    }

    pub fn project(&self, processes: &[ProcessSample], query: TreeQuery<'_>) -> Vec<TreeRow> {
        if processes.is_empty() {
            return Vec::new();
        }
        let mut roots = self.roots.clone();
        roots.sort_by(|left, right| {
            compare(*left, *right, &self.by_key, processes, query.sort, query.descending)
        });
        self.project_ordered(processes, query, roots)
    }

    fn project_ordered(
        &self,
        processes: &[ProcessSample],
        query: TreeQuery<'_>,
        roots: Vec<ProcessIdentity>,
    ) -> Vec<TreeRow> {
        let filter = query.filter.trim().to_ascii_lowercase();
        let (matches, required) = if filter.is_empty() {
            (HashSet::new(), HashSet::new())
        } else {
            let matches = processes
                .iter()
                .filter(|process| matches_filter(process, &filter))
                .map(|process| process.identity)
                .collect::<HashSet<_>>();
            let mut required = matches.clone();
            for key in &matches {
                let mut cursor = self.parent.get(key).copied().flatten();
                let mut seen = HashSet::new();
                while let Some(ancestor) = cursor {
                    if !seen.insert(ancestor) {
                        break;
                    }
                    required.insert(ancestor);
                    cursor = self.parent.get(&ancestor).copied().flatten();
                }
            }
            (matches, required)
        };

        let projection_roots = query.focus.map_or(roots, |key| vec![key]);
        let mut rows = Vec::new();
        let mut emitted = HashSet::new();
        let mut stack =
            projection_roots.iter().rev().copied().map(|key| (key, 0_u16)).collect::<Vec<_>>();
        while let Some((key, depth)) = stack.pop() {
            if !self.by_key.contains_key(&key) || !emitted.insert(key) {
                continue;
            }
            if !filter.is_empty() && !required.contains(&key) {
                continue;
            }
            let process_children =
                self.children.get(&Some(key)).map(Vec::as_slice).unwrap_or_default();
            rows.push(TreeRow {
                key,
                depth,
                has_children: !process_children.is_empty(),
                hidden_descendants: self.descendants.get(&key).copied().unwrap_or_default(),
                family_cpu_percent: self.family_cpu.get(&key).copied().unwrap_or_default(),
                family_memory_bytes: self.family_memory.get(&key).copied().unwrap_or_default(),
                is_match: !filter.is_empty() && matches.contains(&key),
                is_context: !filter.is_empty()
                    && required.contains(&key)
                    && !matches.contains(&key),
            });
            let collapsed = query.collapsed.contains(&key) && filter.is_empty();
            if !collapsed {
                let mut ordered_children = process_children.to_vec();
                ordered_children.sort_by(|left, right| {
                    compare(*left, *right, &self.by_key, processes, query.sort, query.descending)
                });
                stack.extend(ordered_children.into_iter().rev().map(|child| (child, depth + 1)));
            }
        }
        rows
    }
}

/// Repairs the sampled parent relation into a deterministic forest and projects visible rows.
///
/// Missing parents become roots. Cycles are cut at the first not-yet-emitted node in sibling order.
/// Filtering retains matching rows and their ancestors. Collapse only hides descendants when no
/// active filter needs an ancestry path through that branch.
#[cfg(test)]
pub fn project(processes: &[ProcessSample], query: TreeQuery<'_>) -> Vec<TreeRow> {
    ProcessForest::new(processes).project(processes, query)
}

fn identity_cmp(left: &ProcessIdentity, right: &ProcessIdentity) -> Ordering {
    left.cmp(right)
}

fn compare(
    left: ProcessIdentity,
    right: ProcessIdentity,
    by_key: &HashMap<ProcessIdentity, usize>,
    processes: &[ProcessSample],
    sort: TreeSort,
    descending: bool,
) -> Ordering {
    let left_process = &processes[by_key[&left]];
    let right_process = &processes[by_key[&right]];
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
    parent: &mut HashMap<ProcessIdentity, Option<ProcessIdentity>>,
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
                let cycle_root =
                    path[position..].iter().copied().min().expect("cycle path is non-empty");
                parent.insert(cycle_root, None);
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
    by_key: &HashMap<ProcessIdentity, usize>,
    processes: &[ProcessSample],
) -> (HashMap<ProcessIdentity, f32>, HashMap<ProcessIdentity, u64>, HashMap<ProcessIdentity, usize>)
{
    let mut cpu = HashMap::new();
    let mut memory = HashMap::new();
    let mut descendants = HashMap::new();
    let mut visited = HashSet::new();
    let mut stack = roots.iter().rev().copied().map(|key| (key, false)).collect::<Vec<_>>();
    while let Some((key, exiting)) = stack.pop() {
        if exiting {
            let Some(process) = by_key.get(&key).and_then(|index| processes.get(*index)) else {
                continue;
            };
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
        let forest = ProcessForest::new(&processes);
        let rows = forest.project(&processes, query(&HashSet::new(), ""));
        let keys = rows.iter().map(|row| row.key).collect::<HashSet<_>>();
        assert_eq!(rows.len(), 3);
        assert_eq!(keys.len(), 3);
        assert_eq!(forest.parent(processes[0].identity), None);
        assert_eq!(forest.parent(processes[1].identity), None);
        assert_eq!(forest.parent(processes[2].identity), Some(processes[1].identity));

        let family = forest.family_view(processes[1].identity, &processes).unwrap();
        assert_eq!(family.descendant_count, 1);
        assert_eq!(
            family.hot_descendants.iter().map(|member| member.key).collect::<Vec<_>>(),
            vec![processes[2].identity]
        );
        assert!(!family.hot_descendants.iter().any(|member| member.key == processes[1].identity));
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

    #[test]
    fn family_rankings_use_complete_subtrees_and_fixed_tie_breakers() {
        let processes = vec![
            process(1, None, "root", 1.0),
            process(2, Some(1), "branch", 2.0),
            process(3, Some(1), "hot", 8.0),
            process(4, Some(2), "leaf", 10.0),
        ];
        let forest = ProcessForest::new(&processes);
        let family = forest.family_view(processes[0].identity, &processes).unwrap();
        assert_eq!(family.descendant_count, 3);
        assert_eq!(
            family.direct_children.iter().map(|row| row.key.pid()).collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert_eq!(
            family.hot_descendants.iter().map(|row| row.key.pid()).collect::<Vec<_>>(),
            vec![4, 3, 2]
        );
        assert_eq!(
            family.memory_descendants.iter().map(|row| row.key.pid()).collect::<Vec<_>>(),
            vec![4, 3, 2]
        );
    }
}
