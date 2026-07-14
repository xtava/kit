//! Named, serializable system-sampling types shared by the headless and interactive projections.

use std::path::PathBuf;

use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ProcessKey {
    pub pid: u32,
    pub start_token: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessIdentity {
    Stable { key: ProcessKey },
    SnapshotOnly { snapshot_sequence: u64, pid: u32, reason: IdentityUnavailable },
}

impl ProcessIdentity {
    pub fn stable(key: ProcessKey) -> Self {
        Self::Stable { key }
    }

    pub fn pid(self) -> u32 {
        match self {
            Self::Stable { key } => key.pid,
            Self::SnapshotOnly { pid, .. } => pid,
        }
    }

    pub fn stable_key(self) -> Option<ProcessKey> {
        match self {
            Self::Stable { key } => Some(key),
            Self::SnapshotOnly { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityUnavailable {
    PermissionDenied,
    ProcessDisappeared,
    NativeRecordUnavailable,
    UnsupportedPlatform,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CapabilityState {
    Available,
    Unsupported { reason: &'static str },
}

impl CapabilityState {
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::Unsupported { reason } => Some(reason),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct HostCapabilities {
    pub last_observed_core: CapabilityState,
    pub threads: CapabilityState,
    pub resources: CapabilityState,
    pub graceful_terminate: CapabilityState,
    pub force_terminate: CapabilityState,
    pub code_profile: CapabilityState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleReadiness {
    #[default]
    Warming,
    Ready,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Running,
    Sleeping,
    Waiting,
    Stopped,
    Zombie,
    Dead,
    Unknown,
}

impl ProcessState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Sleeping => "sleeping",
            Self::Waiting => "waiting",
            Self::Stopped => "stopped",
            Self::Zombie => "zombie",
            Self::Dead => "dead",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ThreadKey {
    pub tid: u32,
    pub start_token: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DetailRequestKind {
    Threads { process: ProcessKey },
    Resources { process: ProcessKey },
    Core { logical_index: u16 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct DetailRequest {
    pub request_id: u64,
    pub kind: DetailRequestKind,
}

#[derive(Clone, Debug, Serialize)]
pub struct DetailSnapshot {
    pub request: DetailRequest,
    pub sampled_at_ms: u64,
    pub collection_duration_ms: u64,
    pub outcome: DetailOutcome,
    pub warnings: Vec<SampleWarning>,
}

impl DetailSnapshot {
    pub fn threads(&self) -> Option<&[ThreadSample]> {
        match self.payload()? {
            DetailPayload::Threads { rows, .. } | DetailPayload::Core { rows, .. } => Some(rows),
            DetailPayload::Resources(_) => None,
        }
    }

    pub fn resources(&self) -> Option<&ResourceSample> {
        match self.payload()? {
            DetailPayload::Resources(resources) => Some(resources),
            DetailPayload::Threads { .. } | DetailPayload::Core { .. } => None,
        }
    }

    pub fn payload(&self) -> Option<&DetailPayload> {
        match &self.outcome {
            DetailOutcome::Warming { payload } | DetailOutcome::Ready { payload } => Some(payload),
            DetailOutcome::Unavailable { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DetailOutcome {
    Warming { payload: DetailPayload },
    Ready { payload: DetailPayload },
    Unavailable { reason: DetailUnavailable },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DetailPayload {
    Threads { process: ProcessKey, rows: Vec<ThreadSample> },
    Resources(ResourceSample),
    Core { logical_index: u16, rows: Vec<ThreadSample> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetailUnavailable {
    PermissionDenied,
    Unsupported,
    TargetGone,
    TargetReplaced,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum Observed<T> {
    Value(T),
    Warming,
    PermissionDenied,
    Unsupported,
    TargetGone,
    Failed,
}

impl<T> Observed<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Value(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ResourceSample {
    pub executable: Observed<PathBuf>,
    pub current_directory: Observed<PathBuf>,
    pub virtual_bytes: Observed<u64>,
    pub open_resources: Observed<u64>,
    pub open_resource_label: &'static str,
    pub read_bytes: Observed<u64>,
    pub write_bytes: Observed<u64>,
    pub read_bytes_per_second: Observed<f64>,
    pub write_bytes_per_second: Observed<f64>,
    pub io_label: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatsSnapshot {
    pub sequence: u64,
    pub sampled_at_ms: u64,
    pub interval_ms: u64,
    pub collection_duration_ms: u64,
    pub readiness: SampleReadiness,
    pub host: HostCapabilities,
    pub system: SystemSample,
    pub processes: Vec<ProcessSample>,
    pub warnings: Vec<SampleWarning>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SystemSample {
    pub global_cpu_percent: f32,
    pub cpus: Vec<CpuSample>,
    pub total_memory_bytes: u64,
    pub used_memory_bytes: u64,
    pub total_swap_bytes: u64,
    pub used_swap_bytes: u64,
    pub process_count: usize,
    pub thread_count: usize,
    pub load_average: [f64; 3],
    pub uptime_seconds: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CpuSample {
    pub logical_index: u16,
    pub usage_percent: f32,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProcessSample {
    pub identity: ProcessIdentity,
    pub parent_pid: Option<u32>,
    pub name: String,
    pub command: String,
    pub user: Option<String>,
    pub state: ProcessState,
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    pub started_at_ms: u64,
    pub run_time_seconds: u64,
    pub last_cpu: Option<u16>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ThreadSample {
    pub key: ThreadKey,
    pub process: ProcessKey,
    pub name: String,
    pub cpu_percent: f32,
    pub last_cpu: Option<u16>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SampleWarning {
    pub pid: Option<u32>,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_stable_identity_exposes_an_actionable_key() {
        let key = ProcessKey { pid: 42, start_token: 7 };
        assert_eq!(ProcessIdentity::stable(key).stable_key(), Some(key));
        assert_eq!(
            ProcessIdentity::SnapshotOnly {
                snapshot_sequence: 3,
                pid: 42,
                reason: IdentityUnavailable::PermissionDenied,
            }
            .stable_key(),
            None
        );
    }
}
