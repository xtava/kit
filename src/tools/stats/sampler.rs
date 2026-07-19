//! Persistent sysinfo sampling and latest-only worker publication.

use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
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
    CpuSample, DetailCompleteness, DetailData, DetailOutcome, DetailRequest, DetailRequestKind,
    DetailSnapshot, DetailUnavailable, IdentityUnavailable, Observed, ProcessIdentity, ProcessKey,
    ProcessSample, ProcessState, SampleReadiness, SampleWarning, StatsSnapshot, SystemSample,
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ThreadKey {
    tid: u64,
    start_token: Option<u64>,
}

#[derive(Default)]
struct ThreadDelta {
    previous_cpu_seconds: HashMap<ThreadKey, f64>,
    previous_sample: Option<Instant>,
}

#[derive(Default)]
struct ResourceDelta {
    previous_io: Option<(Instant, u64, u64)>,
}

enum DetailCollectorState {
    Threads { request_id: u64, process: ProcessKey, delta: ThreadDelta, scan: Option<ThreadScan> },
    Resources { request_id: u64, process: ProcessKey, delta: ResourceDelta },
    Core { request_id: u64, logical_index: u16, delta: ThreadDelta, scan: Option<ThreadScan> },
}

impl DetailCollectorState {
    fn new(request: DetailRequest) -> Self {
        match request.kind {
            DetailRequestKind::Threads { process } => Self::Threads {
                request_id: request.request_id,
                process,
                delta: ThreadDelta::default(),
                scan: None,
            },
            DetailRequestKind::Resources { process } => Self::Resources {
                request_id: request.request_id,
                process,
                delta: ResourceDelta::default(),
            },
            DetailRequestKind::Core { logical_index } => Self::Core {
                request_id: request.request_id,
                logical_index,
                delta: ThreadDelta::default(),
                scan: None,
            },
        }
    }

    fn request(&self) -> DetailRequest {
        match self {
            Self::Threads { request_id, process, .. } => DetailRequest {
                request_id: *request_id,
                kind: DetailRequestKind::Threads { process: *process },
            },
            Self::Resources { request_id, process, .. } => DetailRequest {
                request_id: *request_id,
                kind: DetailRequestKind::Resources { process: *process },
            },
            Self::Core { request_id, logical_index, .. } => DetailRequest {
                request_id: *request_id,
                kind: DetailRequestKind::Core { logical_index: *logical_index },
            },
        }
    }
}

#[derive(Clone, Copy)]
enum ThreadTarget {
    Process(ProcessKey),
    Core(u16),
}

struct ThreadScan {
    target: ThreadTarget,
    process_keys: Vec<ProcessKey>,
    next_process: usize,
    started: Instant,
    sampled_at: Instant,
    elapsed: Duration,
    previous_sample: Option<Instant>,
    previous_cpu_seconds: HashMap<ThreadKey, f64>,
    next_cpu_seconds: HashMap<ThreadKey, f64>,
    threads: Vec<ThreadSample>,
    warnings: Vec<SampleWarning>,
    partial: bool,
}

impl ThreadScan {
    fn new(target: ThreadTarget, delta: &mut ThreadDelta, processes: &[ProcessSample]) -> Self {
        let sampled_at = Instant::now();
        let process_keys = match target {
            ThreadTarget::Process(selected) => processes
                .iter()
                .filter_map(|process| process.identity.stable_key())
                .filter(|key| *key == selected)
                .collect(),
            ThreadTarget::Core(_) => {
                processes.iter().filter_map(|process| process.identity.stable_key()).collect()
            }
        };
        let previous_sample = delta.previous_sample;
        Self {
            target,
            process_keys,
            next_process: 0,
            started: sampled_at,
            sampled_at,
            elapsed: previous_sample
                .map(|previous| sampled_at.saturating_duration_since(previous))
                .unwrap_or_default(),
            previous_sample,
            previous_cpu_seconds: std::mem::take(&mut delta.previous_cpu_seconds),
            next_cpu_seconds: HashMap::new(),
            threads: Vec::new(),
            warnings: Vec::new(),
            partial: false,
        }
    }

    fn restore_delta(self, delta: &mut ThreadDelta) {
        delta.previous_cpu_seconds = self.previous_cpu_seconds;
        delta.previous_sample = self.previous_sample;
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
            let observation = host::read_process_observation(pid);
            let (identity, last_cpu) = match observation {
                Ok(observation) => (
                    ProcessIdentity::stable(ProcessKey {
                        pid,
                        start_token: observation.start_token,
                    }),
                    observation.last_cpu,
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

    #[cfg(test)]
    pub fn sample_detail(
        &mut self,
        request: DetailRequest,
        processes: &[ProcessSample],
    ) -> DetailSnapshot {
        self.poll_detail(request, processes, None)
            .expect("an unbounded detail poll completes its current scan")
    }

    fn poll_detail(
        &mut self,
        request: DetailRequest,
        processes: &[ProcessSample],
        deadline: Option<Instant>,
    ) -> Option<DetailSnapshot> {
        if self.detail_state.as_ref().is_none_or(|state| state.request() != request) {
            self.detail_state = Some(DetailCollectorState::new(request));
        }
        match self.detail_state.as_mut().expect("detail state was just initialized") {
            DetailCollectorState::Threads { request_id, process, delta, scan } => poll_thread_scan(
                *request_id,
                ThreadTarget::Process(*process),
                delta,
                scan,
                processes,
                deadline,
            ),
            DetailCollectorState::Resources { request_id, process, delta } => {
                Some(collect_resources(*request_id, *process, delta))
            }
            DetailCollectorState::Core { request_id, logical_index, delta, scan } => {
                poll_thread_scan(
                    *request_id,
                    ThreadTarget::Core(*logical_index),
                    delta,
                    scan,
                    processes,
                    deadline,
                )
            }
        }
    }
}

fn poll_thread_scan(
    request_id: u64,
    target: ThreadTarget,
    delta: &mut ThreadDelta,
    scan: &mut Option<ThreadScan>,
    processes: &[ProcessSample],
    deadline: Option<Instant>,
) -> Option<DetailSnapshot> {
    if scan.is_none() {
        *scan = Some(ThreadScan::new(target, delta, processes));
    }
    let current = scan.as_mut().expect("thread scan was just initialized");
    if current.process_keys.is_empty() {
        let current = scan.take().expect("thread scan exists");
        let started = current.started;
        current.restore_delta(delta);
        return Some(unavailable_thread_detail(
            request_id,
            target,
            started,
            DetailUnavailable::TargetGone,
        ));
    }
    while current.next_process < current.process_keys.len() {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return None;
        }
        let process = current.process_keys[current.next_process];
        current.next_process += 1;
        if let Err(reason) = collect_process_threads(current, process) {
            if matches!(target, ThreadTarget::Process(_)) {
                let current = scan.take().expect("thread scan exists");
                let started = current.started;
                current.restore_delta(delta);
                return Some(unavailable_thread_detail(request_id, target, started, reason));
            }
            current.partial = true;
            push_warning(
                &mut current.warnings,
                Some(process.pid),
                format!("core detail skipped a {reason:?} process"),
            );
        }
    }

    let mut completed = scan.take().expect("completed thread scan exists");
    completed.threads.sort_by(|left, right| {
        observed_cpu(&right.cpu_percent)
            .total_cmp(&observed_cpu(&left.cpu_percent))
            .then_with(|| left.tid.cmp(&right.tid))
    });
    let outcome = DetailOutcome::Available {
        readiness: if completed.previous_sample.is_some() {
            SampleReadiness::Ready
        } else {
            SampleReadiness::Warming
        },
        completeness: if completed.partial {
            DetailCompleteness::Partial
        } else {
            DetailCompleteness::Complete
        },
        value: completed.threads,
    };
    let detail = match target {
        ThreadTarget::Process(process) => DetailData::Threads { process, outcome },
        ThreadTarget::Core(logical_index) => DetailData::Core { logical_index, outcome },
    };
    delta.previous_cpu_seconds = completed.next_cpu_seconds;
    delta.previous_sample = Some(completed.sampled_at);
    Some(DetailSnapshot {
        request_id,
        sampled_at_ms: now_ms(),
        collection_duration_ms: duration_ms(completed.started.elapsed()),
        detail,
        warnings: completed.warnings,
    })
}

fn collect_process_threads(
    scan: &mut ThreadScan,
    process: ProcessKey,
) -> Result<(), DetailUnavailable> {
    verify_process(process)?;
    let batch = host::read_process_tasks(process.pid)
        .map_err(|error| detail_unavailable_from_io(&error))?;
    scan.partial |= !batch.failures.is_empty();
    for failure in batch.failures {
        push_warning(
            &mut scan.warnings,
            Some(process.pid),
            match failure.tid {
                Some(tid) => format!("thread {tid} unavailable: {}", failure.error),
                None => format!("task directory entry unavailable: {}", failure.error),
            },
        );
        if let Some(tid) = failure.tid {
            let kind = failure.error.kind();
            scan.threads.push(ThreadSample {
                tid,
                process,
                name: observed_thread_error(kind),
                state: observed_thread_error(kind),
                cpu_percent: observed_thread_error(kind),
                accumulated_cpu_seconds: observed_thread_error(kind),
                last_cpu: observed_thread_error(kind),
            });
        }
    }
    verify_process(process)?;
    let core = match scan.target {
        ThreadTarget::Process(_) => None,
        ThreadTarget::Core(logical_index) => Some(logical_index),
    };
    for (tid, stat) in batch.tasks {
        let key = ThreadKey { tid, start_token: stat.start_token };
        let cpu_time = stat.cpu_time_seconds.value().copied();
        let previous = scan.previous_cpu_seconds.get(&key);
        if let Some(cpu_time) = cpu_time {
            scan.next_cpu_seconds.insert(key, cpu_time);
        }
        if core.is_some_and(|core| stat.last_cpu.value() != Some(&core)) {
            continue;
        }
        let cpu_percent = match cpu_time {
            Some(cpu_time) => previous
                .filter(|_| !scan.elapsed.is_zero())
                .map(|previous| {
                    Observed::Value(finite_percent(
                        ((cpu_time - *previous).max(0.0) / scan.elapsed.as_secs_f64() * 100.0)
                            as f32,
                    ))
                })
                .unwrap_or(Observed::Warming),
            None => observed_reason(&stat.cpu_time_seconds),
        };
        scan.threads.push(ThreadSample {
            tid,
            process,
            name: stat.name,
            state: stat.state,
            cpu_percent,
            accumulated_cpu_seconds: stat.cpu_time_seconds,
            last_cpu: stat.last_cpu,
        });
    }
    Ok(())
}

fn observed_cpu(cpu: &Observed<f32>) -> f32 {
    cpu.value().copied().unwrap_or(f32::NEG_INFINITY)
}

fn observed_thread_error<T>(kind: std::io::ErrorKind) -> Observed<T> {
    match kind {
        std::io::ErrorKind::PermissionDenied => Observed::PermissionDenied,
        std::io::ErrorKind::NotFound => Observed::TargetGone,
        std::io::ErrorKind::Unsupported => Observed::Unsupported,
        _ => Observed::Failed,
    }
}

fn observed_reason<T, U>(value: &Observed<T>) -> Observed<U> {
    match value {
        Observed::Value(_) => Observed::Failed,
        Observed::Warming => Observed::Warming,
        Observed::PermissionDenied => Observed::PermissionDenied,
        Observed::Unsupported => Observed::Unsupported,
        Observed::TargetGone => Observed::TargetGone,
        Observed::Failed => Observed::Failed,
    }
}

fn detail_unavailable_from_io(error: &std::io::Error) -> DetailUnavailable {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => DetailUnavailable::PermissionDenied,
        std::io::ErrorKind::NotFound => DetailUnavailable::TargetGone,
        std::io::ErrorKind::Unsupported => DetailUnavailable::Unsupported,
        _ => DetailUnavailable::Failed,
    }
}

fn collect_resources(
    request_id: u64,
    process: ProcessKey,
    delta: &mut ResourceDelta,
) -> DetailSnapshot {
    let started = Instant::now();
    let now = Instant::now();
    if let Err(reason) = verify_process(process) {
        return unavailable_resource_detail(request_id, process, started, reason);
    }
    let mut resources = match host::read_process_resources(process.pid) {
        Ok(resources) => resources,
        Err(reason) => return unavailable_resource_detail(request_id, process, started, reason),
    };
    if let Err(reason) = verify_process(process) {
        return unavailable_resource_detail(request_id, process, started, reason);
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
    let completeness = if [
        resources.executable.value().is_some(),
        resources.current_directory.value().is_some(),
        resources.virtual_bytes.value().is_some(),
        resources.open_resources.value().is_some(),
        resources.read_bytes.value().is_some(),
        resources.write_bytes.value().is_some(),
    ]
    .into_iter()
    .all(|available| available)
    {
        DetailCompleteness::Complete
    } else {
        DetailCompleteness::Partial
    };
    DetailSnapshot {
        request_id,
        sampled_at_ms: now_ms(),
        collection_duration_ms: duration_ms(started.elapsed()),
        detail: DetailData::Resources {
            process,
            outcome: DetailOutcome::Available {
                readiness: if current.is_some() && previous.is_none() {
                    SampleReadiness::Warming
                } else {
                    SampleReadiness::Ready
                },
                completeness,
                value: resources,
            },
        },
        warnings: Vec::new(),
    }
}

pub struct SamplerWorker {
    running: Arc<AtomicBool>,
    refresh_requested: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    request: watch::Sender<Option<DetailRequest>>,
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
                        if let Some(detail) = sampler.poll_detail(
                            detail_request,
                            &latest_snapshot.processes,
                            Some(next_overview),
                        ) {
                            if *request_receiver.borrow() == Some(detail_request) {
                                detail_sender.send_replace(Some(Arc::new(detail)));
                            }
                            next_detail = Some(
                                Instant::now() + detail_interval(interval, detail_request.kind),
                            );
                        } else {
                            next_detail = Some(
                                (Instant::now() + Duration::from_millis(1)).min(next_overview),
                            );
                        }
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
            Self { running, refresh_requested, handle: Some(handle), request },
            overview_receiver,
            detail_receiver,
        ))
    }

    pub fn set_detail(&self, request: Option<DetailRequest>) {
        self.request.send_replace(request);
        if let Some(handle) = &self.handle {
            handle.thread().unpark();
        }
    }

    pub fn refresh(&self) {
        self.refresh_requested.store(true, Ordering::Release);
        if let Some(handle) = &self.handle {
            handle.thread().unpark();
        }
    }
}

fn detail_interval(overview_interval: Duration, kind: DetailRequestKind) -> Duration {
    overview_interval.max(kind.minimum_interval())
}

fn verify_process(key: ProcessKey) -> Result<(), DetailUnavailable> {
    match host::read_process_observation(key.pid) {
        Ok(observation) if observation.start_token == key.start_token => Ok(()),
        Ok(_) => Err(DetailUnavailable::TargetReplaced),
        Err(error) => Err(detail_unavailable_from_io(&error)),
    }
}

fn unavailable_thread_detail(
    request_id: u64,
    target: ThreadTarget,
    started: Instant,
    reason: DetailUnavailable,
) -> DetailSnapshot {
    let detail = match target {
        ThreadTarget::Process(process) => {
            DetailData::Threads { process, outcome: DetailOutcome::Unavailable(reason) }
        }
        ThreadTarget::Core(logical_index) => {
            DetailData::Core { logical_index, outcome: DetailOutcome::Unavailable(reason) }
        }
    };
    DetailSnapshot {
        request_id,
        sampled_at_ms: now_ms(),
        collection_duration_ms: duration_ms(started.elapsed()),
        detail,
        warnings: Vec::new(),
    }
}

fn unavailable_resource_detail(
    request_id: u64,
    process: ProcessKey,
    started: Instant,
    reason: DetailUnavailable,
) -> DetailSnapshot {
    DetailSnapshot {
        request_id,
        sampled_at_ms: now_ms(),
        collection_duration_ms: duration_ms(started.elapsed()),
        detail: DetailData::Resources { process, outcome: DetailOutcome::Unavailable(reason) },
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
    #[cfg(target_os = "linux")]
    use std::process::{Command, Stdio};

    #[cfg(target_os = "linux")]
    use rustix::fd::OwnedFd;
    #[cfg(target_os = "linux")]
    use rustix::process::{pidfd_open, pidfd_send_signal, Pid, PidfdFlags, Signal};

    use super::*;

    #[cfg(target_os = "linux")]
    struct DisposableChild {
        child: std::process::Child,
        pidfd: OwnedFd,
    }

    #[cfg(target_os = "linux")]
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

    fn sampled_process(key: ProcessKey) -> ProcessSample {
        ProcessSample {
            identity: ProcessIdentity::stable(key),
            parent_pid: None,
            name: "fixture".into(),
            command: "/bin/fixture".into(),
            user: Some("user".into()),
            state: ProcessState::Running,
            cpu_percent: 1.0,
            rss_bytes: 1,
            started_at_ms: 0,
            run_time_seconds: 1,
            last_cpu: Some(0),
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

    #[test]
    fn overdue_core_scan_yields_before_the_next_native_process_read() {
        let process = sampled_process(ProcessKey { pid: 999_999, start_token: 1 });
        let mut delta = ThreadDelta::default();
        let mut scan = None;

        let detail = poll_thread_scan(
            1,
            ThreadTarget::Core(0),
            &mut delta,
            &mut scan,
            &[process],
            Some(Instant::now()),
        );

        assert!(detail.is_none());
        assert_eq!(scan.as_ref().unwrap().next_process, 0);
    }

    #[test]
    fn task_read_errors_keep_their_domain_reason() {
        for (kind, expected) in [
            (std::io::ErrorKind::PermissionDenied, DetailUnavailable::PermissionDenied),
            (std::io::ErrorKind::NotFound, DetailUnavailable::TargetGone),
            (std::io::ErrorKind::Unsupported, DetailUnavailable::Unsupported),
            (std::io::ErrorKind::InvalidData, DetailUnavailable::Failed),
        ] {
            let error = std::io::Error::new(kind, "fixture");
            assert_eq!(detail_unavailable_from_io(&error), expected);
        }
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
    #[cfg(target_os = "linux")]
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
        assert_eq!(thread.last_cpu, Observed::Value(0));
        assert!(
            thread.cpu_percent.value().is_some_and(|cpu| *cpu > 50.0),
            "busy task reported {:?} CPU",
            thread.cpu_percent
        );

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
        const DETAIL_SLICE: Duration = Duration::from_millis(25);
        let interval = Duration::from_secs(1);
        for _ in 0..WARMUPS {
            thread::sleep(interval);
            let overview = sampler.sample_overview().unwrap();
            if let Some(kind) = kind {
                benchmark_detail(sampler, kind, &overview.processes, DETAIL_SLICE, None);
            }
        }

        let mut durations_us = Vec::with_capacity(SAMPLES);
        let mut detail_slices_us = Vec::new();
        for _ in 0..SAMPLES {
            thread::sleep(interval);
            let started = Instant::now();
            let overview = sampler.sample_overview().unwrap();
            if let Some(kind) = kind {
                benchmark_detail(
                    sampler,
                    kind,
                    &overview.processes,
                    DETAIL_SLICE,
                    Some(&mut detail_slices_us),
                );
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
        if matches!(kind, Some(DetailRequestKind::Core { .. })) {
            detail_slices_us.sort_unstable();
            let slice_p95 = detail_slices_us
                [(detail_slices_us.len() * 95 / 100).min(detail_slices_us.len() - 1)];
            let slice_max = detail_slices_us[detail_slices_us.len() - 1];
            println!(
                "{{\"schema\":1,\"surface\":\"core_detail_slices\",\"slice_budget_ms\":{},\"samples\":{},\"p95_us\":{slice_p95},\"max_us\":{slice_max}}}",
                DETAIL_SLICE.as_millis(),
                detail_slices_us.len()
            );
            assert!(slice_p95 <= 50_000, "core slice p95 {slice_p95}us exceeds the 50ms gate");
            assert!(
                slice_max <= 100_000,
                "core slice maximum {slice_max}us exceeds the 100ms gate"
            );
        } else {
            assert!(p95 <= 50_000, "sampler p95 {p95}us exceeds the 50ms gate");
            assert!(maximum <= 100_000, "sampler maximum {maximum}us exceeds the 100ms gate");
        }
    }

    fn benchmark_detail(
        sampler: &mut Sampler,
        kind: DetailRequestKind,
        processes: &[ProcessSample],
        slice_budget: Duration,
        mut slices_us: Option<&mut Vec<u128>>,
    ) {
        let request = DetailRequest { request_id: 1, kind };
        loop {
            let started = Instant::now();
            let completed =
                sampler.poll_detail(request, processes, Some(started + slice_budget)).is_some();
            if let Some(slices_us) = &mut slices_us {
                slices_us.push(started.elapsed().as_micros());
            }
            if completed {
                break;
            }
        }
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
