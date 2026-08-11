use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const REGISTRY_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceIdentity {
    pub key: String,
    pub workspace: PathBuf,
    pub launcher: PathBuf,
    pub server_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryRecord {
    pub schema: u32,
    pub identity: ServiceIdentity,
    pub socket_path: PathBuf,
    pub daemon_receipt: String,
    pub token: String,
    pub published_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TraceDirection {
    Callers,
    Callees,
}

impl TraceDirection {
    pub fn label(self) -> &'static str {
        match self {
            Self::Callers => "Callers",
            Self::Callees => "Callees",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "selector", rename_all = "kebab-case", deny_unknown_fields)]
pub enum TraceSelector {
    Position { file: PathBuf, line: u32, character: u32 },
    Symbol { query: String, scope: Option<PathBuf> },
}

impl TraceSelector {
    pub fn display_name(&self) -> String {
        match self {
            Self::Position { file, line, character } => {
                format!("{}:{}:{}", file.display(), line + 1, character + 1)
            }
            Self::Symbol { query, .. } => query.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceLimits {
    pub max_depth: u32,
    pub max_nodes: usize,
    pub max_paths: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ServiceCommand {
    Ping,
    Inspect,
    Trace {
        selector: TraceSelector,
        direction: TraceDirection,
        limits: TraceLimits,
    },
    Stop,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceRequest {
    pub token: String,
    pub request_id: String,
    pub command: ServiceCommand,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildIdentity {
    pub run_id: String,
    pub generation: u64,
    pub started_at_ms: u64,
    pub launcher: PathBuf,
    pub server_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceInfo {
    pub key: String,
    pub instance_id: String,
    pub started_at_ms: u64,
    pub request_count: u64,
    pub state: String,
    pub workspace: PathBuf,
    pub child: ChildIdentity,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceReply {
    pub request_id: String,
    pub ok: bool,
    pub fatal: bool,
    pub service: Option<ServiceInfo>,
    pub result: Option<Value>,
    pub error: Option<String>,
}

impl ServiceReply {
    pub fn success(request_id: String, service: ServiceInfo, result: Value) -> Self {
        Self {
            request_id,
            ok: true,
            fatal: false,
            service: Some(service),
            result: Some(result),
            error: None,
        }
    }

    pub fn error(request_id: String, error: impl Into<String>, fatal: bool) -> Self {
        Self {
            request_id,
            ok: false,
            fatal,
            service: None,
            result: None,
            error: Some(error.into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceLocation {
    pub file: PathBuf,
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceCandidate {
    pub name: String,
    pub detail: Option<String>,
    pub kind: u64,
    pub location: TraceLocation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceNode {
    pub id: String,
    pub name: String,
    pub detail: Option<String>,
    pub kind: u64,
    pub definition: TraceLocation,
    pub external: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceEdge {
    pub caller: String,
    pub callee: String,
    pub call_sites: Vec<TraceLocation>,
    pub cycle: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TracePath {
    pub nodes: Vec<String>,
    pub cycle: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceSummary {
    pub roots: usize,
    pub paths: usize,
    pub nodes: usize,
    pub edges: usize,
    pub cycles: usize,
    pub boundaries: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceTiming {
    pub elapsed_ms: u64,
    pub native_requests: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceDiscovery {
    pub scanned_files: usize,
    pub activated_files: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceResult {
    pub status: String,
    pub selector: String,
    pub direction: TraceDirection,
    pub target: Option<String>,
    pub candidates: Vec<TraceCandidate>,
    pub nodes: BTreeMap<String, TraceNode>,
    pub edges: Vec<TraceEdge>,
    pub roots: Vec<String>,
    pub paths: Vec<TracePath>,
    pub summary: TraceSummary,
    pub timing: TraceTiming,
    pub discovery: TraceDiscovery,
    pub truncation_reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TraceOutput {
    pub action: &'static str,
    pub service: ServiceInfo,
    pub result: TraceResult,
    pub ascii: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct InspectEntry {
    pub identity: ServiceIdentity,
    pub status: String,
    pub service: Option<ServiceInfo>,
    pub daemon_run_id: Option<String>,
    pub result: Option<Value>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ManagementOutput {
    pub action: &'static str,
    pub matched: usize,
    pub changed: usize,
    pub services: Vec<InspectEntry>,
}
