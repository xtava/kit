//! Persistent sysinfo sampling and latest-only worker publication.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sysinfo::{CpuRefreshKind, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind, Users};
use thiserror::Error;
use tokio::sync::watch;

use super::linux;
use super::model::{
    CpuSample, DetailScope, ProcessKey, ProcessSample, SampleWarning, StatsSnapshot, SystemSample,
    ThreadKey, ThreadSample,
};

const MAX_WARNINGS: usize = 32;
const CORE_DETAIL_INTERVAL: Duration = Duration::from_secs(4);

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
    detail_scope: DetailScope,
    previous_task_ticks: HashMap<ThreadKey, u64>,
    previous_task_sample: Option<Instant>,
    last_core_detail: Option<Instant>,
    cached_threads: Vec<ThreadSample>,
    threads_warmed_up: bool,
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
            detail_scope: DetailScope::None,
            previous_task_ticks: HashMap::new(),
            previous_task_sample: None,
            last_core_detail: None,
            cached_threads: Vec::new(),
            threads_warmed_up: false,
        })
    }

    pub fn sample(&mut self, detail_scope: DetailScope) -> Result<StatsSnapshot, SampleError> {
        let started = Instant::now();
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_cmd(UpdateKind::OnlyIfNotSet)
                .with_user(UpdateKind::OnlyIfNotSet)
                .without_tasks(),
        );

        let interval = self
            .previous_sample
            .replace(started)
            .map(|previous| started.saturating_duration_since(previous))
            .unwrap_or(self.interval);
        let warmed_up = self.sample_count > 0;
        self.sample_count += 1;

        let mut warnings = Vec::new();
        let mut processes = Vec::new();

        for process in self.system.processes().values() {
            if process.thread_kind().is_some() {
                continue;
            }
            let pid = process.pid().as_u32();
            let stat = linux::read_process_stat(pid);
            let (start_token, last_cpu, identity_verified) = match stat {
                Ok(stat) => (stat.start_token, stat.last_cpu, true),
                Err(error) => {
                    push_warning(&mut warnings, Some(pid), format!("stat unavailable: {error}"));
                    (process.start_time(), None, false)
                }
            };
            let key = ProcessKey { pid, start_token };
            let name = process.name().to_string_lossy().into_owned();
            let command = command(process.cmd(), &name);
            let user = process
                .user_id()
                .and_then(|id| self.users.get_user_by_id(id))
                .map(|user| user.name().to_owned());
            processes.push(ProcessSample {
                key,
                identity_verified,
                parent_pid: process.parent().map(|parent| parent.as_u32()),
                name,
                command,
                user,
                status: format!("{:?}", process.status()),
                cpu_percent: finite_percent(process.cpu_usage()),
                rss_bytes: process.memory(),
                virtual_memory_bytes: process.virtual_memory(),
                started_at_ms: process.start_time().saturating_mul(1_000),
                run_time_seconds: process.run_time(),
                last_cpu,
            });
        }

        processes.sort_by_key(|process| process.key.pid);
        let mut threads = self.collect_threads(detail_scope, &processes, &mut warnings);
        threads.sort_by_key(|task| (task.process.pid, task.key.tid));
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
            thread_count: threads.len(),
            load_average: [load.one, load.five, load.fifteen],
            uptime_seconds: System::uptime(),
        };

        Ok(StatsSnapshot {
            sampled_at_ms: now_ms(),
            interval_ms: duration_ms(interval),
            sample_duration_ms: duration_ms(started.elapsed()),
            warmed_up,
            detail_scope,
            threads_warmed_up: self.threads_warmed_up,
            system,
            processes,
            threads,
            warnings,
        })
    }

    fn collect_threads(
        &mut self,
        detail_scope: DetailScope,
        processes: &[ProcessSample],
        warnings: &mut Vec<SampleWarning>,
    ) -> Vec<ThreadSample> {
        if detail_scope != self.detail_scope {
            self.detail_scope = detail_scope;
            self.previous_task_ticks.clear();
            self.previous_task_sample = None;
            self.last_core_detail = None;
            self.cached_threads.clear();
            self.threads_warmed_up = false;
        }
        if detail_scope == DetailScope::None {
            return Vec::new();
        }

        let now = Instant::now();
        if self.threads_warmed_up
            && matches!(detail_scope, DetailScope::Core(_))
            && self.last_core_detail.is_some_and(|previous| {
                now.saturating_duration_since(previous) < CORE_DETAIL_INTERVAL
            })
        {
            return self.cached_threads.clone();
        }

        let selected_process = match detail_scope {
            DetailScope::Process(key) => Some(key),
            DetailScope::Core(_) => None,
            DetailScope::None => unreachable!("the empty detail scope returns above"),
        };
        let elapsed = self
            .previous_task_sample
            .map(|previous| now.saturating_duration_since(previous))
            .unwrap_or_default();
        let ticks_per_second = rustix::param::clock_ticks_per_second().max(1) as f64;
        let mut next_ticks = HashMap::new();
        let mut threads = Vec::new();

        for process in processes {
            if selected_process.is_some_and(|key| process.key != key) {
                continue;
            }
            let tasks = match linux::read_process_tasks(process.key.pid) {
                Ok(tasks) => tasks,
                Err(error) => {
                    push_warning(
                        warnings,
                        Some(process.key.pid),
                        format!("tasks unavailable: {error}"),
                    );
                    continue;
                }
            };
            for (tid, stat) in tasks {
                let key = ThreadKey { tid, start_token: stat.start_token };
                let delta = self
                    .previous_task_ticks
                    .get(&key)
                    .map(|previous| stat.cpu_ticks.saturating_sub(*previous));
                next_ticks.insert(key, stat.cpu_ticks);
                let cpu_percent = delta
                    .filter(|_| !elapsed.is_zero())
                    .map(|delta| delta as f64 / ticks_per_second / elapsed.as_secs_f64() * 100.0)
                    .unwrap_or(0.0) as f32;
                if let DetailScope::Core(core) = detail_scope {
                    if stat.last_cpu != Some(core) {
                        continue;
                    }
                }
                threads.push(ThreadSample {
                    key,
                    process: process.key,
                    name: stat.name,
                    cpu_percent: finite_percent(cpu_percent),
                    last_cpu: stat.last_cpu,
                });
            }
        }

        self.threads_warmed_up = self.previous_task_sample.is_some();
        self.previous_task_ticks = next_ticks;
        self.previous_task_sample = Some(now);
        if matches!(detail_scope, DetailScope::Core(_)) {
            self.last_core_detail = Some(now);
        }
        self.cached_threads = threads.clone();
        threads
    }
}

pub struct SamplerWorker {
    running: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    scope: watch::Sender<DetailScope>,
}

impl SamplerWorker {
    pub fn start(
        mut sampler: Sampler,
    ) -> Result<(Self, watch::Receiver<Arc<StatsSnapshot>>), SampleError> {
        let interval = sampler.interval;
        let first = Arc::new(sampler.sample(DetailScope::None)?);
        let (sender, receiver) = watch::channel(first);
        let (scope, mut scope_receiver) = watch::channel(DetailScope::None);
        let running = Arc::new(AtomicBool::new(true));
        let thread_running = Arc::clone(&running);
        let handle = thread::spawn(move || {
            while thread_running.load(Ordering::Acquire) {
                thread::park_timeout(interval);
                if !thread_running.load(Ordering::Acquire) {
                    break;
                }
                let detail_scope = *scope_receiver.borrow_and_update();
                match sampler.sample(detail_scope) {
                    Ok(snapshot) => sender.send_replace(Arc::new(snapshot)),
                    Err(_) => break,
                };
                if sender.is_closed() {
                    break;
                }
            }
        });
        Ok((Self { running, handle: Some(handle), scope }, receiver))
    }

    pub fn set_detail_scope(&self, scope: DetailScope) {
        self.scope.send_replace(scope);
        self.refresh();
    }

    pub fn refresh(&self) {
        if let Some(handle) = &self.handle {
            handle.thread().unpark();
        }
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

    #[tokio::test]
    async fn worker_publishes_a_measured_snapshot_and_stops() {
        let interval = sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(Duration::from_millis(250));
        let sampler = Sampler::new(interval).unwrap();
        let (worker, mut receiver) = SamplerWorker::start(sampler).unwrap();
        receiver.changed().await.unwrap();
        assert!(receiver.borrow().warmed_up);
        assert!(!receiver.borrow().system.cpus.is_empty());
        drop(worker);
    }

    #[test]
    #[ignore = "30-second local performance gate"]
    fn benchmark_overview_current_machine() {
        benchmark_scope(DetailScope::None);
    }

    #[test]
    #[ignore = "30-second local performance gate"]
    fn benchmark_process_detail_current_machine() {
        let interval = Duration::from_secs(1);
        let mut sampler = Sampler::new(interval).unwrap();
        let snapshot = sampler.sample(DetailScope::None).unwrap();
        let process = snapshot
            .processes
            .iter()
            .find(|process| process.key.pid == std::process::id())
            .or_else(|| snapshot.processes.first())
            .expect("at least the benchmark process is visible")
            .key;
        benchmark_samples(&mut sampler, DetailScope::Process(process));
    }

    #[test]
    #[ignore = "30-second local performance gate"]
    fn benchmark_core_detail_current_machine() {
        benchmark_scope(DetailScope::Core(0));
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
        let first = sampler.sample(DetailScope::None).unwrap();
        let key = first
            .processes
            .iter()
            .find(|process| process.key.pid == child.child.id())
            .expect("pinned process appears in overview")
            .key;
        thread::sleep(interval);
        let _ = sampler.sample(DetailScope::Core(0)).unwrap();
        thread::sleep(Duration::from_secs(2));
        let focused = sampler.sample(DetailScope::Core(0)).unwrap();

        let thread = focused
            .threads
            .iter()
            .find(|thread| thread.process == key)
            .expect("pinned task appears in focused core view");
        assert_eq!(thread.last_cpu, Some(0));
        assert!(thread.cpu_percent > 50.0, "busy task reported only {}% CPU", thread.cpu_percent);

        super::super::signal::send(key, super::super::signal::ProcessSignal::Kill).unwrap();
        let _ = child.child.wait();
    }

    fn benchmark_scope(scope: DetailScope) {
        let interval = Duration::from_secs(1);
        let mut sampler = Sampler::new(interval).unwrap();
        let _ = sampler.sample(DetailScope::None).unwrap();
        benchmark_samples(&mut sampler, scope);
    }

    fn benchmark_samples(sampler: &mut Sampler, scope: DetailScope) {
        let interval = Duration::from_secs(1);
        let mut durations = Vec::new();
        for _ in 0..30 {
            thread::sleep(interval);
            durations.push(sampler.sample(scope).unwrap().sample_duration_ms);
        }
        durations.sort_unstable();
        let p50 = durations[durations.len() / 2];
        let p95 = durations[(durations.len() * 95 / 100).min(durations.len() - 1)];
        println!(
            "stats sampler {scope:?} benchmark: p50={p50}ms p95={p95}ms samples={durations:?}"
        );
        assert!(p95 < 500, "sampler p95 {p95}ms exceeds the 500ms gate");
    }
}
