//! Persistent sysinfo sampling and latest-only worker publication.

use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sysinfo::{
    CpuRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind, Users,
};
use thiserror::Error;
use tokio::sync::watch;

use super::host;
use super::model::{
    CpuSample, DetailOutcome, DetailPayload, DetailRequest, DetailRequestKind, DetailSnapshot,
    DetailUnavailable, IdentityUnavailable, Observed, ProcessIdentity, ProcessKey, ProcessSample,
    ProcessState, SampleReadiness, SampleWarning, StatsSnapshot, SystemSample, ThreadKey,
    ThreadSample,
};

const MAX_WARNINGS: usize = 32;
const SNAPSHOT_METADATA_RETRY: Duration = Duration::from_secs(60);

#[derive(Debug, Error)]
pub enum SampleError {
    #[error("system information is unavailable on this platform")]
    UnsupportedSystem,
}

pub struct Sampler {
    system: System,
    users: Users,
    interval: Duration,
    previous_sample: Option<Instant>,
    sample_count: u64,
    previous_process_cpu: HashMap<ProcessKey, u64>,
    metadata: MetadataLedger,
    detail_state: Option<DetailCollectorState>,
}

#[derive(Default)]
struct MetadataLedger {
    stable: HashSet<ProcessKey>,
    snapshot: HashMap<SnapshotMetadataKey, Instant>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SnapshotMetadataKey {
    pid: u32,
    public_start_time: u64,
}

struct ObservedProcess {
    pid: Pid,
    identity: ProcessIdentity,
    public_start_time: u64,
    last_cpu: Option<u16>,
}

#[derive(Default)]
struct ThreadDelta {
    previous_ticks: HashMap<ThreadKey, u64>,
    previous_sample: Option<Instant>,
}

#[derive(Default)]
struct ResourceDelta {
    previous_io: Option<(Instant, u64, u64)>,
}

enum DetailCollectorState {
    Threads { request: DetailRequest, delta: ThreadDelta },
    Resources { request: DetailRequest, delta: ResourceDelta },
    Core { request: DetailRequest, delta: ThreadDelta },
}

impl DetailCollectorState {
    fn new(request: DetailRequest) -> Self {
        match request.kind {
            DetailRequestKind::Threads { .. } => {
                Self::Threads { request, delta: ThreadDelta::default() }
            }
            DetailRequestKind::Resources { .. } => {
                Self::Resources { request, delta: ResourceDelta::default() }
            }
            DetailRequestKind::Core { .. } => Self::Core { request, delta: ThreadDelta::default() },
        }
    }

    fn request(&self) -> DetailRequest {
        match self {
            Self::Threads { request, .. }
            | Self::Resources { request, .. }
            | Self::Core { request, .. } => *request,
        }
    }
}

impl MetadataLedger {
    fn refresh_targets(&mut self, observed: &[ObservedProcess], now: Instant) -> Vec<Pid> {
        let live_stable = observed
            .iter()
            .filter_map(|process| process.identity.stable_key())
            .collect::<HashSet<_>>();
        let live_snapshot = observed
            .iter()
            .filter(|process| process.identity.stable_key().is_none())
            .map(|process| SnapshotMetadataKey {
                pid: process.pid.as_u32(),
                public_start_time: process.public_start_time,
            })
            .collect::<HashSet<_>>();
        self.stable.retain(|key| live_stable.contains(key));
        self.snapshot.retain(|key, _| live_snapshot.contains(key));

        observed
            .iter()
            .filter_map(|process| {
                if let Some(key) = process.identity.stable_key() {
                    return self.stable.insert(key).then_some(process.pid);
                }
                let key = SnapshotMetadataKey {
                    pid: process.pid.as_u32(),
                    public_start_time: process.public_start_time,
                };
                let due = self.snapshot.get(&key).is_none_or(|previous| {
                    now.saturating_duration_since(*previous) >= SNAPSHOT_METADATA_RETRY
                });
                if due {
                    self.snapshot.insert(key, now);
                }
                due.then_some(process.pid)
            })
            .collect()
    }
}

impl Sampler {
    pub fn new(interval: Duration) -> Result<Self, SampleError> {
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            return Err(SampleError::UnsupportedSystem);
        }
        let mut system = System::new();
        system.refresh_cpu_list(CpuRefreshKind::nothing().with_cpu_usage());
        Ok(Self {
            system,
            users: Users::new_with_refreshed_list(),
            interval,
            previous_sample: None,
            sample_count: 0,
            previous_process_cpu: HashMap::new(),
            metadata: MetadataLedger::default(),
            detail_state: None,
        })
    }

    pub fn sample_overview(&mut self) -> Result<StatsSnapshot, SampleError> {
        let started = Instant::now();
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory().without_tasks(),
        );

        let interval = self
            .previous_sample
            .replace(started)
            .map(|previous| started.saturating_duration_since(previous))
            .unwrap_or(self.interval);
        let sequence = self.sample_count;
        let readiness =
            if sequence > 0 { SampleReadiness::Ready } else { SampleReadiness::Warming };
        self.sample_count += 1;

        let mut warnings = Vec::new();
        let mut observed = Vec::new();

        for process in self.system.processes().values() {
            if process.thread_kind().is_some() {
                continue;
            }
            let sysinfo_pid = process.pid();
            let pid = sysinfo_pid.as_u32();
            let stat = host::read_process_stat(pid);
            let (identity, last_cpu) = match stat {
                Ok(stat) => (
                    ProcessIdentity::stable(ProcessKey { pid, start_token: stat.start_token }),
                    stat.last_cpu,
                ),
                Err(error) => {
                    push_warning(&mut warnings, Some(pid), format!("stat unavailable: {error}"));
                    let reason = match error.kind() {
                        std::io::ErrorKind::PermissionDenied => {
                            IdentityUnavailable::PermissionDenied
                        }
                        std::io::ErrorKind::NotFound => IdentityUnavailable::ProcessDisappeared,
                        std::io::ErrorKind::Unsupported => IdentityUnavailable::UnsupportedPlatform,
                        _ => IdentityUnavailable::NativeRecordUnavailable,
                    };
                    (
                        ProcessIdentity::SnapshotOnly { snapshot_sequence: sequence, pid, reason },
                        None,
                    )
                }
            };
            observed.push(ObservedProcess {
                pid: sysinfo_pid,
                identity,
                public_start_time: process.start_time(),
                last_cpu,
            });
        }

        let metadata_targets = self.metadata.refresh_targets(&observed, started);
        if !metadata_targets.is_empty() {
            self.system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&metadata_targets),
                false,
                ProcessRefreshKind::nothing()
                    .with_cmd(UpdateKind::Always)
                    .with_user(UpdateKind::Always)
                    .without_tasks(),
            );
        }

        let mut next_process_cpu = HashMap::with_capacity(observed.len());
        let mut processes = Vec::with_capacity(observed.len());
        for observed in observed {
            let Some(process) = self.system.process(observed.pid) else {
                continue;
            };
            let name = process.name().to_string_lossy().into_owned();
            let command = command(process.cmd(), &name);
            let user = process
                .user_id()
                .and_then(|id| self.users.get_user_by_id(id))
                .map(|user| user.name().to_owned());
            let cpu_percent = if let Some(key) = observed.identity.stable_key() {
                let accumulated = process.accumulated_cpu_time();
                let cpu = self
                    .previous_process_cpu
                    .get(&key)
                    .filter(|_| !interval.is_zero())
                    .map(|previous| {
                        accumulated.saturating_sub(*previous) as f64
                            / interval.as_millis().max(1) as f64
                            * 100.0
                    })
                    .unwrap_or_default() as f32;
                next_process_cpu.insert(key, accumulated);
                cpu
            } else {
                process.cpu_usage()
            };
            processes.push(ProcessSample {
                identity: observed.identity,
                parent_pid: process.parent().map(|parent| parent.as_u32()),
                name,
                command,
                user,
                state: process_state(process.status()),
                cpu_percent: finite_percent(cpu_percent),
                rss_bytes: process.memory(),
                started_at_ms: process.start_time().saturating_mul(1_000),
                run_time_seconds: process.run_time(),
                last_cpu: observed.last_cpu,
            });
        }
        self.previous_process_cpu = next_process_cpu;

        processes.sort_by_key(|process| process.identity.pid());
        let cpus = self
            .system
            .cpus()
            .iter()
            .enumerate()
            .map(|(index, cpu)| CpuSample {
                logical_index: index.min(u16::MAX as usize) as u16,
                usage_percent: finite_percent(cpu.cpu_usage()),
            })
            .collect::<Vec<_>>();
        let load = System::load_average();
        let system = SystemSample {
            global_cpu_percent: finite_percent(self.system.global_cpu_usage()),
            cpus,
            total_memory_bytes: self.system.total_memory(),
            used_memory_bytes: self.system.used_memory(),
            total_swap_bytes: self.system.total_swap(),
            used_swap_bytes: self.system.used_swap(),
            process_count: processes.len(),
            thread_count: 0,
            load_average: [load.one, load.five, load.fifteen],
            uptime_seconds: System::uptime(),
        };

        Ok(StatsSnapshot {
            sequence,
            sampled_at_ms: now_ms(),
            interval_ms: duration_ms(interval),
            collection_duration_ms: duration_ms(started.elapsed()),
            readiness,
            host: host::capabilities(),
            system,
            processes,
            warnings,
        })
    }

    pub fn sample_detail(
        &mut self,
        request: DetailRequest,
        processes: &[ProcessSample],
    ) -> DetailSnapshot {
        if self.detail_state.as_ref().is_none_or(|state| state.request() != request) {
            self.detail_state = Some(DetailCollectorState::new(request));
        }
        match self.detail_state.as_mut().expect("detail state was just initialized") {
            DetailCollectorState::Threads { delta, .. } => {
                let DetailRequestKind::Threads { process } = request.kind else {
                    unreachable!("collector state and request are constructed together")
                };
                collect_threads(request, Some(process), None, delta, processes)
            }
            DetailCollectorState::Resources { delta, .. } => {
                let DetailRequestKind::Resources { process } = request.kind else {
                    unreachable!("collector state and request are constructed together")
                };
                collect_resources(request, process, delta)
            }
            DetailCollectorState::Core { delta, .. } => {
                let DetailRequestKind::Core { logical_index } = request.kind else {
                    unreachable!("collector state and request are constructed together")
                };
                collect_threads(request, None, Some(logical_index), delta, processes)
            }
        }
    }
}

fn collect_threads(
    request: DetailRequest,
    selected: Option<ProcessKey>,
    core: Option<u16>,
    delta: &mut ThreadDelta,
    processes: &[ProcessSample],
) -> DetailSnapshot {
    let started = Instant::now();
    let now = Instant::now();
    let mut warnings = Vec::new();
    let mut selected_seen = selected.is_none();
    let elapsed = delta
        .previous_sample
        .map(|previous| now.saturating_duration_since(previous))
        .unwrap_or_default();
    let ticks_per_second = rustix::param::clock_ticks_per_second().max(1) as f64;
    let mut next_ticks = HashMap::new();
    let mut threads = Vec::new();

    for process in processes {
        let Some(process_key) = process.identity.stable_key() else { continue };
        if selected.is_some_and(|selected| selected != process_key) {
            continue;
        }
        selected_seen = true;
        if let Err(reason) = verify_process(process_key) {
            if selected.is_some() {
                return unavailable_detail(request, started, reason);
            }
            push_warning(
                &mut warnings,
                Some(process_key.pid),
                format!("core detail skipped a {reason:?} process"),
            );
            continue;
        }
        let tasks = match host::read_process_tasks(process_key.pid) {
            Ok(tasks) => tasks,
            Err(error) => {
                push_warning(
                    &mut warnings,
                    Some(process_key.pid),
                    format!("tasks unavailable: {error}"),
                );
                continue;
            }
        };
        if let Err(reason) = verify_process(process_key) {
            if selected.is_some() {
                return unavailable_detail(request, started, reason);
            }
            push_warning(
                &mut warnings,
                Some(process_key.pid),
                format!("core detail discarded a {reason:?} process"),
            );
            continue;
        }
        for (tid, stat) in tasks {
            let key = ThreadKey { tid, start_token: stat.start_token };
            let previous = delta.previous_ticks.get(&key);
            next_ticks.insert(key, stat.cpu_ticks);
            if core.is_some_and(|core| stat.last_cpu != Some(core)) {
                continue;
            }
            let cpu_percent = previous
                .filter(|_| !elapsed.is_zero())
                .map(|previous| {
                    stat.cpu_ticks.saturating_sub(*previous) as f64
                        / ticks_per_second
                        / elapsed.as_secs_f64()
                        * 100.0
                })
                .unwrap_or_default() as f32;
            threads.push(ThreadSample {
                key,
                process: process_key,
                name: stat.name,
                cpu_percent: finite_percent(cpu_percent),
                last_cpu: stat.last_cpu,
            });
        }
    }
    if !selected_seen {
        return unavailable_detail(request, started, DetailUnavailable::TargetGone);
    }

    let warmed_up = delta.previous_sample.is_some();
    delta.previous_ticks = next_ticks;
    delta.previous_sample = Some(now);
    threads.sort_by(|left, right| {
        right
            .cpu_percent
            .total_cmp(&left.cpu_percent)
            .then_with(|| left.key.tid.cmp(&right.key.tid))
    });
    let payload = match request.kind {
        DetailRequestKind::Threads { process } => DetailPayload::Threads { process, rows: threads },
        DetailRequestKind::Core { logical_index } => {
            DetailPayload::Core { logical_index, rows: threads }
        }
        DetailRequestKind::Resources { .. } => {
            unreachable!("resource requests have a dedicated collector")
        }
    };
    DetailSnapshot {
        request,
        sampled_at_ms: now_ms(),
        collection_duration_ms: duration_ms(started.elapsed()),
        outcome: if warmed_up {
            DetailOutcome::Ready { payload }
        } else {
            DetailOutcome::Warming { payload }
        },
        warnings,
    }
}

fn collect_resources(
    request: DetailRequest,
    process: ProcessKey,
    delta: &mut ResourceDelta,
) -> DetailSnapshot {
    let started = Instant::now();
    let now = Instant::now();
    if let Err(reason) = verify_process(process) {
        return unavailable_detail(request, started, reason);
    }
    let mut resources = match host::read_process_resources(process.pid) {
        Ok(resources) => resources,
        Err(reason) => return unavailable_detail(request, started, reason),
    };
    if let Err(reason) = verify_process(process) {
        return unavailable_detail(request, started, reason);
    }
    let previous = delta.previous_io;
    let current = match (&resources.read_bytes, &resources.write_bytes) {
        (Observed::Value(read), Observed::Value(write)) => Some((*read, *write)),
        _ => None,
    };
    if let (Some((previous_at, previous_read, previous_write)), Some((read, write))) =
        (previous, current)
    {
        let elapsed = now.saturating_duration_since(previous_at).as_secs_f64();
        if elapsed > 0.0 {
            resources.read_bytes_per_second =
                Observed::Value(read.saturating_sub(previous_read) as f64 / elapsed);
            resources.write_bytes_per_second =
                Observed::Value(write.saturating_sub(previous_write) as f64 / elapsed);
        }
    }
    if let Some((read, write)) = current {
        delta.previous_io = Some((now, read, write));
    }
    let payload = DetailPayload::Resources(resources);
    DetailSnapshot {
        request,
        sampled_at_ms: now_ms(),
        collection_duration_ms: duration_ms(started.elapsed()),
        outcome: if current.is_some() && previous.is_none() {
            DetailOutcome::Warming { payload }
        } else {
            DetailOutcome::Ready { payload }
        },
        warnings: Vec::new(),
    }
}

pub struct SamplerWorker {
    running: Arc<AtomicBool>,
    refresh_requested: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    request: watch::Sender<Option<DetailRequest>>,
    next_request_id: AtomicU64,
}

type WorkerStart = (
    SamplerWorker,
    watch::Receiver<Arc<StatsSnapshot>>,
    watch::Receiver<Option<Arc<DetailSnapshot>>>,
);

impl SamplerWorker {
    pub fn start(mut sampler: Sampler) -> Result<WorkerStart, SampleError> {
        let interval = sampler.interval;
        let first = Arc::new(sampler.sample_overview()?);
        let (overview_sender, overview_receiver) = watch::channel(first);
        let (detail_sender, detail_receiver) = watch::channel(None);
        let (request, mut request_receiver) = watch::channel(None);
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let refresh_requested = Arc::new(AtomicBool::new(false));
        let thread_refresh_requested = Arc::clone(&refresh_requested);
        let handle = thread::spawn(move || {
            let minimum_overview_interval = interval.max(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
            let mut latest_snapshot = Arc::clone(&overview_sender.borrow());
            let mut last_overview = Instant::now();
            let mut next_overview = last_overview + minimum_overview_interval;
            let mut active_request = None;
            let mut next_detail = None;
            while thread_running.load(Ordering::Acquire) {
                let requested = *request_receiver.borrow_and_update();
                if requested != active_request {
                    active_request = requested;
                    next_detail = requested.map(|_| Instant::now());
                    if requested.is_none() {
                        sampler.detail_state = None;
                        detail_sender.send_replace(None);
                    }
                }
                let now = Instant::now();
                if thread_refresh_requested.swap(false, Ordering::AcqRel)
                    && now.saturating_duration_since(last_overview) >= minimum_overview_interval
                {
                    next_overview = now;
                }

                if now >= next_overview {
                    match sampler.sample_overview() {
                        Ok(snapshot) => {
                            latest_snapshot = Arc::new(snapshot);
                            overview_sender.send_replace(Arc::clone(&latest_snapshot));
                        }
                        Err(_) => break,
                    }
                    last_overview = Instant::now();
                    next_overview = last_overview + minimum_overview_interval;
                } else if let (Some(detail_request), Some(due)) = (active_request, next_detail) {
                    if now >= due {
                        let detail =
                            sampler.sample_detail(detail_request, &latest_snapshot.processes);
                        if *request_receiver.borrow() == Some(detail_request) {
                            detail_sender.send_replace(Some(Arc::new(detail)));
                        }
                        next_detail =
                            Some(Instant::now() + detail_interval(interval, detail_request.kind));
                    }
                }

                if overview_sender.is_closed() {
                    break;
                }
                let wake_at = next_detail.map_or(next_overview, |detail| detail.min(next_overview));
                thread::park_timeout(wake_at.saturating_duration_since(Instant::now()));
            }
        });
        Ok((
            Self {
                running,
                refresh_requested,
                handle: Some(handle),
                request,
                next_request_id: AtomicU64::new(1),
            },
            overview_receiver,
            detail_receiver,
        ))
    }

    pub fn request_detail(&self, kind: Option<DetailRequestKind>) -> Option<DetailRequest> {
        let request = kind.map(|kind| DetailRequest {
            request_id: self.next_request_id.fetch_add(1, Ordering::Relaxed),
            kind,
        });
        self.request.send_replace(request);
        if let Some(handle) = &self.handle {
            handle.thread().unpark();
        }
        request
    }

    pub fn refresh(&self) {
        self.refresh_requested.store(true, Ordering::Release);
        if let Some(handle) = &self.handle {
            handle.thread().unpark();
        }
    }
}

fn detail_interval(overview_interval: Duration, kind: DetailRequestKind) -> Duration {
    let minimum = match kind {
        DetailRequestKind::Core { .. } | DetailRequestKind::Resources { .. } => {
            Duration::from_secs(4)
        }
        DetailRequestKind::Threads { .. } => {
            if cfg!(target_os = "windows") {
                Duration::from_secs(4)
            } else {
                Duration::from_secs(2)
            }
        }
    };
    overview_interval.max(minimum)
}

fn verify_process(key: ProcessKey) -> Result<(), DetailUnavailable> {
    match host::read_process_stat(key.pid) {
        Ok(stat) if stat.start_token == key.start_token => Ok(()),
        Ok(_) => Err(DetailUnavailable::TargetReplaced),
        Err(error) => Err(match error.kind() {
            std::io::ErrorKind::PermissionDenied => DetailUnavailable::PermissionDenied,
            std::io::ErrorKind::NotFound => DetailUnavailable::TargetGone,
            std::io::ErrorKind::Unsupported => DetailUnavailable::Unsupported,
            _ => DetailUnavailable::Failed,
        }),
    }
}

fn unavailable_detail(
    request: DetailRequest,
    started: Instant,
    reason: DetailUnavailable,
) -> DetailSnapshot {
    DetailSnapshot {
        request,
        sampled_at_ms: now_ms(),
        collection_duration_ms: duration_ms(started.elapsed()),
        outcome: DetailOutcome::Unavailable { reason },
        warnings: Vec::new(),
    }
}

impl Drop for SamplerWorker {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

fn command(args: &[std::ffi::OsString], fallback: &str) -> String {
    if args.is_empty() {
        fallback.to_owned()
    } else {
        args.iter().map(|arg| arg.to_string_lossy()).collect::<Vec<_>>().join(" ")
    }
}

fn process_state(status: sysinfo::ProcessStatus) -> ProcessState {
    use sysinfo::ProcessStatus;

    match status {
        ProcessStatus::Run | ProcessStatus::Waking => ProcessState::Running,
        ProcessStatus::Sleep | ProcessStatus::Idle | ProcessStatus::Parked => {
            ProcessState::Sleeping
        }
        ProcessStatus::UninterruptibleDiskSleep
        | ProcessStatus::Wakekill
        | ProcessStatus::LockBlocked => ProcessState::Waiting,
        ProcessStatus::Stop | ProcessStatus::Tracing => ProcessState::Stopped,
        ProcessStatus::Zombie => ProcessState::Zombie,
        ProcessStatus::Dead => ProcessState::Dead,
        ProcessStatus::Unknown(_) => ProcessState::Unknown,
    }
}

fn finite_percent(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(duration_ms).unwrap_or(0)
}

fn push_warning(warnings: &mut Vec<SampleWarning>, pid: Option<u32>, message: String) {
    if warnings.len() < MAX_WARNINGS {
        warnings.push(SampleWarning { pid, message });
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use rustix::fd::OwnedFd;
    use rustix::process::{pidfd_open, pidfd_send_signal, Pid, PidfdFlags, Signal};

    use super::*;

    struct DisposableChild {
        child: std::process::Child,
        pidfd: OwnedFd,
    }

    impl Drop for DisposableChild {
        fn drop(&mut self) {
            let _ = pidfd_send_signal(&self.pidfd, Signal::KILL);
            let _ = self.child.wait();
        }
    }

    fn observed(identity: ProcessIdentity, public_start_time: u64) -> ObservedProcess {
        ObservedProcess {
            pid: sysinfo::Pid::from_u32(identity.pid()),
            identity,
            public_start_time,
            last_cpu: None,
        }
    }

    #[test]
    fn metadata_is_attempted_once_per_stable_generation() {
        let now = Instant::now();
        let first_key = ProcessKey { pid: 42, start_token: 7 };
        let second_key = ProcessKey { pid: 42, start_token: 8 };
        let mut ledger = MetadataLedger::default();

        assert_eq!(
            ledger.refresh_targets(&[observed(ProcessIdentity::stable(first_key), 1)], now),
            [sysinfo::Pid::from_u32(42)]
        );
        assert!(ledger
            .refresh_targets(&[observed(ProcessIdentity::stable(first_key), 1)], now)
            .is_empty());
        assert_eq!(
            ledger.refresh_targets(&[observed(ProcessIdentity::stable(second_key), 1)], now),
            [sysinfo::Pid::from_u32(42)]
        );
        assert_eq!(ledger.stable, HashSet::from([second_key]));
    }

    #[test]
    fn snapshot_only_metadata_retries_on_a_bounded_generation_hint() {
        let now = Instant::now();
        let identity = ProcessIdentity::SnapshotOnly {
            snapshot_sequence: 1,
            pid: 42,
            reason: IdentityUnavailable::PermissionDenied,
        };
        let mut ledger = MetadataLedger::default();

        assert_eq!(
            ledger.refresh_targets(&[observed(identity, 10)], now),
            [sysinfo::Pid::from_u32(42)]
        );
        assert!(ledger
            .refresh_targets(
                &[observed(identity, 10)],
                now + SNAPSHOT_METADATA_RETRY - Duration::from_millis(1),
            )
            .is_empty());
        assert_eq!(
            ledger.refresh_targets(&[observed(identity, 10)], now + SNAPSHOT_METADATA_RETRY,),
            [sysinfo::Pid::from_u32(42)]
        );
        assert_eq!(
            ledger.refresh_targets(&[observed(identity, 11)], now + SNAPSHOT_METADATA_RETRY,),
            [sysinfo::Pid::from_u32(42)]
        );
        assert_eq!(ledger.snapshot.len(), 1);
    }

    #[tokio::test]
    async fn worker_publishes_a_measured_snapshot_and_stops() {
        let interval = sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(250));
        let sampler = Sampler::new(interval).unwrap();
        let (worker, mut receiver, _details) = SamplerWorker::start(sampler).unwrap();
        receiver.changed().await.unwrap();
        assert_eq!(receiver.borrow().readiness, SampleReadiness::Ready);
        assert!(!receiver.borrow().system.cpus.is_empty());
        drop(worker);
    }

    #[test]
    #[ignore = "35-second local performance gate"]
    fn benchmark_overview_current_machine() {
        benchmark_scope(None);
    }

    #[test]
    #[ignore = "35-second local performance gate"]
    fn benchmark_process_detail_current_machine() {
        let interval = Duration::from_secs(1);
        let mut sampler = Sampler::new(interval).unwrap();
        let snapshot = sampler.sample_overview().unwrap();
        let process = snapshot
            .processes
            .iter()
            .find(|process| process.identity.pid() == std::process::id())
            .or_else(|| snapshot.processes.first())
            .expect("at least the benchmark process is visible")
            .identity
            .stable_key()
            .expect("benchmark target has a stable identity");
        benchmark_samples(&mut sampler, Some(DetailRequestKind::Threads { process }));
    }

    #[test]
    #[ignore = "35-second local performance gate"]
    fn benchmark_core_detail_current_machine() {
        benchmark_scope(Some(DetailRequestKind::Core { logical_index: 0 }));
    }

    #[test]
    #[ignore = "spawns a disposable CPU-pinned busy process"]
    fn controlled_pinned_process_appears_on_its_core() {
        let child = Command::new("taskset")
            .args(["-c", "0", "yes"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = Pid::from_raw(child.id() as i32).unwrap();
        let pidfd = pidfd_open(pid, PidfdFlags::empty()).unwrap();
        let mut child = DisposableChild { child, pidfd };
        thread::sleep(Duration::from_millis(100));
        assert!(
            child.child.try_wait().unwrap().is_none(),
            "taskset workload exited before sampling"
        );

        let interval = Duration::from_secs(1);
        let mut sampler = Sampler::new(interval).unwrap();
        let first = sampler.sample_overview().unwrap();
        let key = first
            .processes
            .iter()
            .find(|process| process.identity.pid() == child.child.id())
            .expect("pinned process appears in overview")
            .identity
            .stable_key()
            .expect("disposable child has a stable identity");
        thread::sleep(interval);
        let request =
            DetailRequest { request_id: 1, kind: DetailRequestKind::Core { logical_index: 0 } };
        let overview = sampler.sample_overview().unwrap();
        let _ = sampler.sample_detail(request, &overview.processes);
        thread::sleep(Duration::from_secs(2));
        let overview = sampler.sample_overview().unwrap();
        let focused = sampler.sample_detail(request, &overview.processes);

        let thread = focused
            .threads()
            .into_iter()
            .flatten()
            .find(|thread| thread.process == key)
            .expect("pinned task appears in focused core view");
        assert_eq!(thread.last_cpu, Some(0));
        assert!(thread.cpu_percent > 50.0, "busy task reported only {}% CPU", thread.cpu_percent);

        super::super::host::send_action(key, super::super::host::ProcessAction::ForceTerminate)
            .unwrap();
        let _ = child.child.wait();
    }

    fn benchmark_scope(kind: Option<DetailRequestKind>) {
        let interval = Duration::from_secs(1);
        let mut sampler = Sampler::new(interval).unwrap();
        let _ = sampler.sample_overview().unwrap();
        benchmark_samples(&mut sampler, kind);
    }

    fn benchmark_samples(sampler: &mut Sampler, kind: Option<DetailRequestKind>) {
        const WARMUPS: usize = 5;
        const SAMPLES: usize = 30;
        let interval = Duration::from_secs(1);
        for _ in 0..WARMUPS {
            thread::sleep(interval);
            let overview = sampler.sample_overview().unwrap();
            if let Some(kind) = kind {
                let _ = sampler
                    .sample_detail(DetailRequest { request_id: 1, kind }, &overview.processes);
            }
        }

        let mut durations_us = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            thread::sleep(interval);
            let started = Instant::now();
            let overview = sampler.sample_overview().unwrap();
            if let Some(kind) = kind {
                let _ = sampler
                    .sample_detail(DetailRequest { request_id: 1, kind }, &overview.processes);
            }
            durations_us.push(started.elapsed().as_micros());
        }
        durations_us.sort_unstable();
        let p50 = durations_us[SAMPLES / 2];
        let p95 = durations_us[(SAMPLES * 95 / 100).min(SAMPLES - 1)];
        let maximum = durations_us[SAMPLES - 1];
        println!(
            "{{\"schema\":1,\"surface\":\"legacy_sampler_control\",\"scope\":\"{}\",\"warmups\":{WARMUPS},\"samples\":{SAMPLES},\"p50_us\":{p50},\"p95_us\":{p95},\"max_us\":{maximum}}}",
            scope_label(kind)
        );
        assert!(p95 <= 50_000, "sampler p95 {p95}us exceeds the 50ms gate");
        assert!(maximum <= 100_000, "sampler maximum {maximum}us exceeds the 100ms gate");
    }

    fn scope_label(kind: Option<DetailRequestKind>) -> &'static str {
        match kind {
            None => "overview",
            Some(DetailRequestKind::Threads { .. }) => "selected_process",
            Some(DetailRequestKind::Resources { .. }) => "selected_resources",
            Some(DetailRequestKind::Core { .. }) => "focused_core",
        }
    }
}
