//! Named, serializable system-sampling types shared by the headless and interactive projections.

use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ProcessKey {
    pub pid: u32,
    pub start_token: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ThreadKey {
    pub tid: u32,
    pub start_token: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum DetailScope {
    #[default]
    None,
    Process(ProcessKey),
    Core(u16),
}

#[derive(Clone, Debug, Serialize)]
pub struct StatsSnapshot {
    pub sampled_at_ms: u64,
    pub interval_ms: u64,
    pub sample_duration_ms: u64,
    pub warmed_up: bool,
    pub detail_scope: DetailScope,
    pub threads_warmed_up: bool,
    pub system: SystemSample,
    pub processes: Vec<ProcessSample>,
    pub threads: Vec<ThreadSample>,
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
    pub key: ProcessKey,
    pub identity_verified: bool,
    pub parent_pid: Option<u32>,
    pub name: String,
    pub command: String,
    pub user: Option<String>,
    pub status: String,
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    pub virtual_memory_bytes: u64,
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
