use std::collections::HashSet;

use super::model::{
    AgentId, AgentOutput, DebatePolicy, NodeStatus, RunStatus, Stage, SwarmId, SwarmProjection,
};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum NodeId {
    Run(SwarmId),
    Stage { run: SwarmId, stage: Stage },
    Agent { run: SwarmId, stage: Stage, agent: AgentId },
}

impl NodeId {
    pub fn run_id(&self) -> &SwarmId {
        match self {
            Self::Run(run) | Self::Stage { run, .. } | Self::Agent { run, .. } => run,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SnapshotStatus {
    Run(RunStatus),
    Corrupt,
}

#[derive(Clone, Debug)]
pub struct RunSnapshot {
    pub id: SwarmId,
    pub status: SnapshotStatus,
    pub projection: Option<SwarmProjection>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeRow {
    pub id: NodeId,
    pub depth: u16,
    pub label: String,
    pub status: String,
    pub has_children: bool,
}

pub fn project(runs: &[RunSnapshot], collapsed: &HashSet<NodeId>) -> Vec<TreeRow> {
    let mut ordered = runs.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| run_number(&right.id).cmp(&run_number(&left.id)));
    let mut rows = Vec::new();
    for run in ordered {
        let run_id = NodeId::Run(run.id.clone());
        let has_children = run.projection.is_some();
        rows.push(TreeRow {
            id: run_id.clone(),
            depth: 0,
            label: run.id.to_string(),
            status: snapshot_status(&run.status).to_owned(),
            has_children,
        });
        if !has_children || collapsed.contains(&run_id) {
            continue;
        }
        let projection = run.projection.as_ref().expect("checked projection presence");
        for stage in stages(projection.spec.debate) {
            let stage_id = NodeId::Stage { run: run.id.clone(), stage };
            let agents = stage_agents(projection, stage);
            rows.push(TreeRow {
                id: stage_id.clone(),
                depth: 1,
                label: stage_label(stage).to_owned(),
                status: stage_status(projection, stage).to_owned(),
                has_children: !agents.is_empty(),
            });
            if collapsed.contains(&stage_id) {
                continue;
            }
            for node in agents {
                rows.push(TreeRow {
                    id: NodeId::Agent { run: run.id.clone(), stage, agent: node.agent.clone() },
                    depth: 2,
                    label: node
                        .role
                        .as_ref()
                        .map(|role| role.title.clone())
                        .unwrap_or_else(|| node.agent.to_string()),
                    status: agent_stage_status(node, stage).to_owned(),
                    has_children: false,
                });
            }
        }
    }
    rows
}

pub fn parent(id: &NodeId) -> Option<NodeId> {
    match id {
        NodeId::Run(_) => None,
        NodeId::Stage { run, .. } => Some(NodeId::Run(run.clone())),
        NodeId::Agent { run, stage, .. } => Some(NodeId::Stage { run: run.clone(), stage: *stage }),
    }
}

pub fn first_child(rows: &[TreeRow], id: &NodeId) -> Option<NodeId> {
    let index = rows.iter().position(|row| &row.id == id)?;
    let depth = rows[index].depth;
    rows.get(index + 1).filter(|next| next.depth == depth + 1).map(|next| next.id.clone())
}

pub fn normalize_selection(rows: &[TreeRow], selected: Option<NodeId>) -> Option<NodeId> {
    let mut candidate = selected;
    while let Some(id) = candidate {
        if rows.iter().any(|row| row.id == id) {
            return Some(id);
        }
        candidate = parent(&id);
    }
    rows.first().map(|row| row.id.clone())
}

fn stage_agents(projection: &SwarmProjection, stage: Stage) -> Vec<&super::model::SwarmNode> {
    projection
        .nodes
        .iter()
        .filter(|node| node.prompts.iter().any(|prompt| prompt.stage == stage))
        .collect()
}

fn agent_stage_status(node: &super::model::SwarmNode, stage: Stage) -> &'static str {
    if node.outputs.iter().any(|output| output_stage(output) == stage) {
        return "succeeded";
    }
    if node.stage != stage {
        return "queued";
    }
    node_status(node.status)
}

fn output_stage(output: &AgentOutput) -> Stage {
    match output {
        AgentOutput::Planner(_) => Stage::Planning,
        AgentOutput::Expert(_) => Stage::Experts,
        AgentOutput::Rebuttal(_) => Stage::Debate,
        AgentOutput::Devil(_) => Stage::Devil,
        AgentOutput::Synthesis(_) => Stage::Synthesis,
    }
}

fn stage_status(projection: &SwarmProjection, stage: Stage) -> &'static str {
    if projection.completed_stages.contains(&stage) {
        "succeeded"
    } else if projection.active_stage == Some(stage) {
        match projection.status {
            RunStatus::Cancelling => "cancelling",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Orphaned => "orphaned",
            RunStatus::Unavailable => "unavailable",
            _ => "running",
        }
    } else {
        "queued"
    }
}

fn stages(debate: DebatePolicy) -> Vec<Stage> {
    let mut stages = vec![Stage::Planning, Stage::Experts];
    if debate == DebatePolicy::Enabled {
        stages.push(Stage::Debate);
    }
    stages.extend([Stage::Devil, Stage::Synthesis]);
    stages
}

fn node_status(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Queued => "queued",
        NodeStatus::Waiting => "waiting",
        NodeStatus::Running => "running",
        NodeStatus::Succeeded => "succeeded",
        NodeStatus::Failed => "failed",
        NodeStatus::Cancelled => "cancelled",
    }
}

fn snapshot_status(status: &SnapshotStatus) -> &'static str {
    match status {
        SnapshotStatus::Run(RunStatus::Queued) => "queued",
        SnapshotStatus::Run(RunStatus::Running) => "running",
        SnapshotStatus::Run(RunStatus::Cancelling) => "cancelling",
        SnapshotStatus::Run(RunStatus::Succeeded) => "succeeded",
        SnapshotStatus::Run(RunStatus::Failed) => "failed",
        SnapshotStatus::Run(RunStatus::Cancelled) => "cancelled",
        SnapshotStatus::Run(RunStatus::Orphaned) => "orphaned",
        SnapshotStatus::Run(RunStatus::Unavailable) => "unavailable",
        SnapshotStatus::Corrupt => "corrupt",
    }
}

fn stage_label(stage: Stage) -> &'static str {
    match stage {
        Stage::Planning => "Planning",
        Stage::Experts => "Experts",
        Stage::Debate => "Debate",
        Stage::Devil => "Devil's advocate",
        Stage::Synthesis => "Synthesis",
    }
}

fn run_number(id: &SwarmId) -> u64 {
    id.as_str().strip_prefix("swarm-").and_then(|number| number.parse().ok()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::swarm::model::{ReasoningEffort, SwarmSpec, SWARM_SCHEMA_VERSION};

    fn projection(id: &str) -> SwarmProjection {
        SwarmProjection::new(SwarmSpec {
            schema_version: SWARM_SCHEMA_VERSION,
            id: SwarmId::new(id).unwrap(),
            prompt: "prompt".to_owned(),
            working_directory: std::env::current_dir().unwrap(),
            model: None,
            reasoning: ReasoningEffort::High,
            debate: DebatePolicy::Enabled,
            created_at_ms: 1,
            retry_limit: 1,
        })
        .unwrap()
    }

    #[test]
    fn run_order_collapse_and_stable_identity_are_deterministic() {
        let runs = vec![
            RunSnapshot {
                id: SwarmId::new("swarm-1").unwrap(),
                status: SnapshotStatus::Run(RunStatus::Queued),
                projection: Some(projection("swarm-1")),
                error: None,
            },
            RunSnapshot {
                id: SwarmId::new("swarm-2").unwrap(),
                status: SnapshotStatus::Run(RunStatus::Queued),
                projection: Some(projection("swarm-2")),
                error: None,
            },
        ];
        let rows = project(&runs, &HashSet::new());
        assert_eq!(rows[0].id, NodeId::Run(SwarmId::new("swarm-2").unwrap()));
        let selected = rows[1].id.clone();
        let mut collapsed = HashSet::new();
        collapsed.insert(rows[0].id.clone());
        let changed = project(&runs, &collapsed);
        assert_eq!(parent(&selected), Some(rows[0].id.clone()));
        assert_eq!(normalize_selection(&changed, Some(selected)), Some(rows[0].id.clone()));
    }
}
