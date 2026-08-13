use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, VecDeque},
};

use petgraph::{
    algo::kosaraju_scc,
    graph::{DiGraph, NodeIndex},
    visit::EdgeRef,
    Direction,
};

use super::protocol::{TraceDirection, TraceEdge, TraceNode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProjectedRelation {
    Target,
    Expanded,
    CycleReference,
    SharedReference,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ProjectionLine {
    pub node: String,
    pub reference: usize,
    pub relation: ProjectedRelation,
    pub edge_index: Option<usize>,
    pub ancestor_continuations: Vec<bool>,
    pub last_sibling: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TraceProjection {
    pub lines: Vec<ProjectionLine>,
    pub references: BTreeMap<String, usize>,
}

struct SemanticGraph<'a> {
    graph: DiGraph<&'a str, usize>,
    indices: BTreeMap<&'a str, NodeIndex>,
}

impl<'a> SemanticGraph<'a> {
    fn build(nodes: &'a BTreeMap<String, TraceNode>, edges: &[TraceEdge]) -> Result<Self, String> {
        let mut graph = DiGraph::new();
        let mut indices = BTreeMap::new();
        for node_id in nodes.keys() {
            indices.insert(node_id.as_str(), graph.add_node(node_id.as_str()));
        }
        for (edge_index, edge) in edges.iter().enumerate() {
            let caller = indices
                .get(edge.caller.as_str())
                .copied()
                .ok_or_else(|| format!("trace edge caller {} is missing", edge.caller))?;
            let callee = indices
                .get(edge.callee.as_str())
                .copied()
                .ok_or_else(|| format!("trace edge callee {} is missing", edge.callee))?;
            graph.add_edge(caller, callee, edge_index);
        }
        Ok(Self { graph, indices })
    }

    fn index(&self, node: &str) -> Result<NodeIndex, String> {
        self.indices.get(node).copied().ok_or_else(|| format!("trace target {node} is missing"))
    }

    fn node_id(&self, index: NodeIndex) -> &'a str {
        self.graph[index]
    }

    fn direction(direction: TraceDirection) -> Direction {
        match direction {
            TraceDirection::Callers => Direction::Incoming,
            TraceDirection::Callees => Direction::Outgoing,
        }
    }

    fn sorted_neighbors(
        &self,
        node: NodeIndex,
        direction: TraceDirection,
        nodes: &BTreeMap<String, TraceNode>,
    ) -> Vec<NodeIndex> {
        let mut neighbors =
            self.graph.neighbors_directed(node, Self::direction(direction)).collect::<Vec<_>>();
        neighbors.sort_by(|left, right| {
            compare_node_ids(self.node_id(*left), self.node_id(*right), nodes)
        });
        neighbors.dedup();
        neighbors
    }

    fn sorted_edges(
        &self,
        node: NodeIndex,
        direction: TraceDirection,
        nodes: &BTreeMap<String, TraceNode>,
    ) -> Vec<(NodeIndex, usize)> {
        let mut edges = self
            .graph
            .edges_directed(node, Self::direction(direction))
            .map(|edge| {
                let neighbor = match direction {
                    TraceDirection::Callers => edge.source(),
                    TraceDirection::Callees => edge.target(),
                };
                (neighbor, *edge.weight())
            })
            .collect::<Vec<_>>();
        edges.sort_by(|(left_node, left_edge), (right_node, right_edge)| {
            compare_node_ids(self.node_id(*left_node), self.node_id(*right_node), nodes)
                .then_with(|| left_edge.cmp(right_edge))
        });
        edges
    }
}

pub(super) fn classify_cycles(
    nodes: &BTreeMap<String, TraceNode>,
    edges: &mut [TraceEdge],
) -> Result<Vec<Vec<String>>, String> {
    let semantic = SemanticGraph::build(nodes, edges)?;
    let components = kosaraju_scc(&semantic.graph);
    let mut component_by_node = BTreeMap::new();
    let mut cyclic_components = BTreeSet::new();
    let mut output = Vec::new();

    for (component_index, component) in components.iter().enumerate() {
        for node in component {
            component_by_node.insert(node.index(), component_index);
        }
        let cyclic = component.len() > 1
            || component
                .first()
                .is_some_and(|node| semantic.graph.find_edge(*node, *node).is_some());
        if !cyclic {
            continue;
        }
        cyclic_components.insert(component_index);
        let mut node_ids =
            component.iter().map(|node| semantic.node_id(*node).to_owned()).collect::<Vec<_>>();
        node_ids.sort_by(|left, right| compare_node_ids(left, right, nodes));
        output.push(node_ids);
    }

    for edge in edges {
        let caller = semantic.index(&edge.caller)?;
        let callee = semantic.index(&edge.callee)?;
        let caller_component = component_by_node.get(&caller.index());
        let callee_component = component_by_node.get(&callee.index());
        edge.cycle = caller_component == callee_component
            && caller_component.is_some_and(|component| cyclic_components.contains(component));
    }

    output.sort_by(|left, right| {
        compare_node_ids(&left[0], &right[0], nodes).then_with(|| left.cmp(right))
    });
    Ok(output)
}

pub(super) fn normalize_call_sites(edges: &mut [TraceEdge]) {
    for edge in edges {
        edge.call_sites.sort();
        edge.call_sites.dedup();
    }
}

pub(super) fn project(
    target: &str,
    direction: TraceDirection,
    nodes: &BTreeMap<String, TraceNode>,
    edges: &[TraceEdge],
) -> Result<TraceProjection, String> {
    let semantic = SemanticGraph::build(nodes, edges)?;
    let target_index = semantic.index(target)?;
    let references = assign_references(&semantic, target_index, direction, nodes);
    let mut states = BTreeMap::from([(target.to_owned(), VisitState::Active)]);
    let mut emitted_edges = BTreeSet::new();
    let mut lines = vec![ProjectionLine {
        node: target.to_owned(),
        reference: references[target],
        relation: ProjectedRelation::Target,
        edge_index: None,
        ancestor_continuations: Vec::new(),
        last_sibling: true,
    }];
    let mut stack = vec![ProjectionFrame {
        node: target.to_owned(),
        edges: semantic.sorted_edges(target_index, direction, nodes),
        next_edge: 0,
        ancestor_continuations: Vec::new(),
    }];

    while let Some(frame) = stack.last_mut() {
        if frame.next_edge == frame.edges.len() {
            states.insert(frame.node.clone(), VisitState::Complete);
            stack.pop();
            continue;
        }

        let (neighbor_index, edge_index) = frame.edges[frame.next_edge];
        let last_sibling = frame.next_edge + 1 == frame.edges.len();
        frame.next_edge += 1;
        if !emitted_edges.insert(edge_index) {
            return Err(format!("trace edge {edge_index} was projected more than once"));
        }

        let neighbor = semantic.node_id(neighbor_index).to_owned();
        let relation = match states.get(&neighbor) {
            Some(VisitState::Active) => ProjectedRelation::CycleReference,
            Some(VisitState::Complete) => ProjectedRelation::SharedReference,
            None => ProjectedRelation::Expanded,
        };
        lines.push(ProjectionLine {
            node: neighbor.clone(),
            reference: references[&neighbor],
            relation,
            edge_index: Some(edge_index),
            ancestor_continuations: frame.ancestor_continuations.clone(),
            last_sibling,
        });

        if relation == ProjectedRelation::Expanded {
            states.insert(neighbor.clone(), VisitState::Active);
            let mut child_continuations = frame.ancestor_continuations.clone();
            child_continuations.push(!last_sibling);
            stack.push(ProjectionFrame {
                node: neighbor,
                edges: semantic.sorted_edges(neighbor_index, direction, nodes),
                next_edge: 0,
                ancestor_continuations: child_continuations,
            });
        }
    }

    if emitted_edges.len() != edges.len() {
        return Err(format!(
            "merged projection reached {} of {} trace edges",
            emitted_edges.len(),
            edges.len()
        ));
    }
    if states.len() != nodes.len() {
        return Err(format!(
            "merged projection reached {} of {} trace nodes",
            states.len(),
            nodes.len()
        ));
    }
    Ok(TraceProjection { lines, references })
}

fn assign_references(
    graph: &SemanticGraph<'_>,
    target: NodeIndex,
    direction: TraceDirection,
    nodes: &BTreeMap<String, TraceNode>,
) -> BTreeMap<String, usize> {
    let mut references = BTreeMap::new();
    let mut queue = VecDeque::from([target]);
    while let Some(current) = queue.pop_front() {
        let node_id = graph.node_id(current);
        if references.contains_key(node_id) {
            continue;
        }
        references.insert(node_id.to_owned(), references.len() + 1);
        for neighbor in graph.sorted_neighbors(current, direction, nodes) {
            if !references.contains_key(graph.node_id(neighbor)) {
                queue.push_back(neighbor);
            }
        }
    }
    references
}

fn compare_node_ids(left: &str, right: &str, nodes: &BTreeMap<String, TraceNode>) -> Ordering {
    let left_node = &nodes[left];
    let right_node = &nodes[right];
    left_node
        .definition
        .file
        .cmp(&right_node.definition.file)
        .then_with(|| left_node.definition.line.cmp(&right_node.definition.line))
        .then_with(|| left_node.definition.character.cmp(&right_node.definition.character))
        .then_with(|| left_node.kind.cmp(&right_node.kind))
        .then_with(|| left_node.name.cmp(&right_node.name))
        .then_with(|| left.cmp(right))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VisitState {
    Active,
    Complete,
}

struct ProjectionFrame {
    node: String,
    edges: Vec<(NodeIndex, usize)>,
    next_edge: usize,
    ancestor_continuations: Vec<bool>,
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use petgraph::{algo::has_path_connecting, graph::DiGraph};

    use super::super::protocol::{TraceEdge, TraceLocation, TraceNode};
    use super::{
        classify_cycles, normalize_call_sites, project, ProjectedRelation, TraceDirection,
    };

    #[test]
    fn diamond_expands_shared_node_once_and_projects_every_edge_once() {
        let nodes = nodes(&[("target", 4), ("left", 2), ("right", 3), ("root", 1)]);
        let edges = vec![
            edge("left", "target"),
            edge("right", "target"),
            edge("root", "left"),
            edge("root", "right"),
        ];

        let projection = project("target", TraceDirection::Callers, &nodes, &edges).unwrap();

        assert_eq!(projection.lines.len(), edges.len() + 1);
        assert_eq!(expanded_nodes(&projection.lines), vec!["target", "left", "root", "right"]);
        assert_eq!(
            projection
                .lines
                .iter()
                .filter(|line| line.relation == ProjectedRelation::SharedReference)
                .map(|line| line.node.as_str())
                .collect::<Vec<_>>(),
            vec!["root"]
        );
        assert_every_edge_once(&projection.lines, edges.len());
    }

    #[test]
    fn completed_scc_classification_handles_self_and_multi_node_cycles() {
        let nodes = nodes(&[("a", 1), ("b", 2), ("self", 3)]);
        let mut edges = vec![edge("a", "b"), edge("b", "a"), edge("self", "self")];

        let components = classify_cycles(&nodes, &mut edges).unwrap();

        assert_eq!(components, vec![vec!["a".to_owned(), "b".to_owned()], vec!["self".to_owned()]]);
        assert!(edges.iter().all(|edge| edge.cycle));
        let cycle_nodes =
            nodes.into_iter().filter(|(id, _)| id != "self").collect::<BTreeMap<_, _>>();
        let projection = project("a", TraceDirection::Callees, &cycle_nodes, &edges[..2]).unwrap();
        assert!(projection
            .lines
            .iter()
            .any(|line| line.relation == ProjectedRelation::CycleReference));
    }

    #[test]
    fn semantic_identity_does_not_merge_equal_display_names() {
        let mut nodes = nodes(&[("target", 3), ("first", 1), ("second", 2)]);
        nodes.get_mut("first").unwrap().name = "run".to_owned();
        nodes.get_mut("second").unwrap().name = "run".to_owned();
        let edges = vec![edge("first", "target"), edge("second", "target")];

        let projection = project("target", TraceDirection::Callers, &nodes, &edges).unwrap();

        assert_eq!(projection.references.len(), 3);
        assert_ne!(projection.references["first"], projection.references["second"]);
        assert_eq!(expanded_nodes(&projection.lines), vec!["target", "first", "second"]);
    }

    #[test]
    fn callsites_are_sorted_and_deduplicated() {
        let mut edges = vec![edge("a", "b")];
        edges[0].call_sites = vec![location(8, 3), location(2, 9), location(8, 3)];

        normalize_call_sites(&mut edges);

        assert_eq!(edges[0].call_sites, vec![location(2, 9), location(8, 3)]);
    }

    #[test]
    fn direction_changes_only_target_outward_orientation() {
        let nodes = nodes(&[("caller", 1), ("callee", 2)]);
        let edges = vec![edge("caller", "callee")];

        let callers = project("callee", TraceDirection::Callers, &nodes, &edges).unwrap();
        let callees = project("caller", TraceDirection::Callees, &nodes, &edges).unwrap();

        assert_eq!(expanded_nodes(&callers.lines), vec!["callee", "caller"]);
        assert_eq!(expanded_nodes(&callees.lines), vec!["caller", "callee"]);
    }

    #[test]
    fn insertion_order_does_not_change_semantic_projection() {
        let nodes = nodes(&[("target", 4), ("left", 2), ("right", 3), ("root", 1)]);
        let forward = vec![
            edge("left", "target"),
            edge("right", "target"),
            edge("root", "left"),
            edge("root", "right"),
        ];
        let reverse = forward.iter().cloned().rev().collect::<Vec<_>>();

        let first = project("target", TraceDirection::Callers, &nodes, &forward).unwrap();
        let second = project("target", TraceDirection::Callers, &nodes, &reverse).unwrap();

        assert_eq!(semantic_lines(&first.lines), semantic_lines(&second.lines));
        assert_eq!(first.references, second.references);
    }

    #[test]
    fn every_reachable_directed_graph_through_four_nodes_satisfies_projection_invariants() {
        for node_count in 1..=4usize {
            let ids = (0..node_count).map(|index| format!("n{index}")).collect::<Vec<_>>();
            let nodes = ids
                .iter()
                .enumerate()
                .map(|(index, id)| (id.clone(), node(id, index as u32 + 1)))
                .collect::<BTreeMap<_, _>>();
            let possible_edges = node_count * node_count;
            for mask in 0u32..(1u32 << possible_edges) {
                let mut raw = DiGraph::<(), ()>::new();
                let indices = (0..node_count).map(|_| raw.add_node(())).collect::<Vec<_>>();
                let mut edges = Vec::new();
                for caller in 0..node_count {
                    for callee in 0..node_count {
                        let bit = caller * node_count + callee;
                        if mask & (1 << bit) == 0 {
                            continue;
                        }
                        raw.add_edge(indices[caller], indices[callee], ());
                        edges.push(edge(&ids[caller], &ids[callee]));
                    }
                }

                for direction in [TraceDirection::Callers, TraceDirection::Callees] {
                    let connected = indices.iter().all(|node| match direction {
                        TraceDirection::Callers => {
                            has_path_connecting(&raw, *node, indices[0], None)
                        }
                        TraceDirection::Callees => {
                            has_path_connecting(&raw, indices[0], *node, None)
                        }
                    });
                    if !connected {
                        continue;
                    }
                    let projection = project(&ids[0], direction, &nodes, &edges).unwrap();
                    assert_eq!(projection.lines.len(), edges.len() + 1);
                    assert_eq!(expanded_nodes(&projection.lines).len(), nodes.len());
                    assert_eq!(projection.references.len(), nodes.len());
                    assert_every_edge_once(&projection.lines, edges.len());
                }
            }
        }
    }

    fn nodes(values: &[(&str, u32)]) -> BTreeMap<String, TraceNode> {
        values.iter().map(|(id, line)| ((*id).to_owned(), node(id, *line))).collect()
    }

    fn node(id: &str, line: u32) -> TraceNode {
        TraceNode {
            id: id.to_owned(),
            name: id.to_owned(),
            detail: None,
            kind: 12,
            definition: TraceLocation { file: PathBuf::from("src/trace.ts"), line, character: 1 },
            generated_aliases: Vec::new(),
            external: false,
        }
    }

    fn edge(caller: &str, callee: &str) -> TraceEdge {
        TraceEdge {
            caller: caller.to_owned(),
            callee: callee.to_owned(),
            call_sites: vec![location(1, 1)],
            cycle: false,
        }
    }

    fn location(line: u32, character: u32) -> TraceLocation {
        TraceLocation { file: PathBuf::from("src/trace.ts"), line, character }
    }

    fn expanded_nodes(lines: &[super::ProjectionLine]) -> Vec<&str> {
        lines
            .iter()
            .filter(|line| {
                matches!(line.relation, ProjectedRelation::Target | ProjectedRelation::Expanded)
            })
            .map(|line| line.node.as_str())
            .collect()
    }

    fn assert_every_edge_once(lines: &[super::ProjectionLine], edge_count: usize) {
        let mut indices = lines.iter().filter_map(|line| line.edge_index).collect::<Vec<_>>();
        indices.sort_unstable();
        assert_eq!(indices, (0..edge_count).collect::<Vec<_>>());
    }

    fn semantic_lines(
        lines: &[super::ProjectionLine],
    ) -> Vec<(&str, usize, ProjectedRelation, Vec<bool>, bool)> {
        lines
            .iter()
            .map(|line| {
                (
                    line.node.as_str(),
                    line.reference,
                    line.relation,
                    line.ancestor_continuations.clone(),
                    line.last_sibling,
                )
            })
            .collect()
    }
}
