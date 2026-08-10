use std::path::PathBuf;

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

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CallKind {
    Prepare,
    Incoming,
    Outgoing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ServiceCommand {
    Ping,
    Inspect,
    Call {
        kind: CallKind,
        file: PathBuf,
        line: u32,
        character: u32,
        item: usize,
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

#[derive(Clone, Debug, Serialize)]
pub struct QueryOutput {
    pub action: &'static str,
    pub service: ServiceInfo,
    pub result: Value,
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
