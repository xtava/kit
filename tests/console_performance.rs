#![cfg(any(target_os = "linux", target_os = "macos"))]

mod support;

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

use support::console::{
    HeadlessConsoleClient, LocalConsoleHarness, PublicConsole, PublicConsoleOptions,
};

const DEFAULT_SETTLE_SECS: u64 = 10;
const DEFAULT_SAMPLE_SECS: u64 = 30;
const MAX_DURATION_SECS: u64 = 600;
const RUN_OVERHEAD_TIMEOUT_SECS: u64 = 60;
const MOUSE_MAX_TRANSIENT_RSS_BYTES: u64 = 8 * 1024 * 1024;
const MOUSE_RECOVERY_TOLERANCE_BYTES: u64 = 1024 * 1024;
const SESSION_COUNTS: &[usize] = &[1, 10, 20];
const CLEANUP_MARGIN_SECS: u64 = 60;

#[derive(Clone, Copy, Debug)]
struct PerformanceConfig {
    session_count: usize,
    settle: Duration,
    sample: Duration,
    workload: Workload,
}

impl PerformanceConfig {
    fn from_env() -> Result<Self> {
        let session_count = parse_session_count()?;
        let settle = parse_duration("KIT_CONSOLE_PERF_SETTLE_SECS", DEFAULT_SETTLE_SECS)?;
        let sample = parse_duration("KIT_CONSOLE_PERF_SAMPLE_SECS", DEFAULT_SAMPLE_SECS)?;
        let workload = Workload::from_env()?;
        Ok(Self { session_count, settle, sample, workload })
    }

    fn run_timeout(self) -> Result<Duration> {
        self.settle
            .checked_add(self.sample)
            .and_then(|duration| {
                duration.checked_add(Duration::from_secs(RUN_OVERHEAD_TIMEOUT_SECS))
            })
            .context("compute Console performance verifier deadline")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Workload {
    Idle,
    Input,
    Mouse,
    Output,
    Resize,
}

impl Workload {
    fn from_env() -> Result<Self> {
        match env::var("KIT_CONSOLE_PERF_WORKLOAD") {
            Ok(value) => match value.as_str() {
                "idle" => Ok(Self::Idle),
                "input" => Ok(Self::Input),
                "mouse" => Ok(Self::Mouse),
                "output" => Ok(Self::Output),
                "resize" => Ok(Self::Resize),
                _ => anyhow::bail!(
                    "KIT_CONSOLE_PERF_WORKLOAD must be idle, input, mouse, output, or resize; got \
                     {value:?}"
                ),
            },
            Err(env::VarError::NotPresent) => Ok(Self::Idle),
            Err(error) => Err(error).context("read KIT_CONSOLE_PERF_WORKLOAD"),
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Input => "input",
            Self::Mouse => "mouse",
            Self::Output => "output",
            Self::Resize => "resize",
        }
    }

    fn session_script(self, marker: &str, sleep_seconds: u64) -> String {
        match self {
            Self::Input => format!("printf '{marker}\\n'; exec /bin/cat"),
            Self::Mouse => {
                format!("printf '{marker}\\n'; printf '\\033[?1003h\\033[?1006h'; exec /bin/cat")
            }
            Self::Output => format!(
                "printf '{marker}\\n'; i=0; while :; do printf 'kit-output-%08d\\n' \"$i\"; \
                 i=$((i + 1)); sleep 0.05; done"
            ),
            Self::Idle | Self::Resize => {
                format!("printf '{marker}\\n'; exec /bin/sleep {sleep_seconds}")
            }
        }
    }
}

#[derive(Clone, Debug)]
struct ProcessSample {
    pid: u32,
    ppid: u32,
    cpu_seconds: f64,
    rss_bytes: u64,
    threads: u64,
    fds: Option<u64>,
    executable: Option<String>,
    start_identity: Option<u64>,
}

#[derive(Serialize)]
struct ProcessTreeMeasurement {
    root_pid: u32,
    binary_path: Option<String>,
    process_count: usize,
    cpu_percent_one_core: f64,
    rss_bytes: u64,
    threads: u64,
    fds: Option<u64>,
}

#[derive(Serialize)]
struct PerformanceResult {
    schema: &'static str,
    platform: &'static str,
    source_revision: &'static str,
    source_dirty: bool,
    upstream_wezterm_revision: &'static str,
    maintained_wezterm_tree: &'static str,
    kit_binary_path: String,
    workload: &'static str,
    session_count: usize,
    settle_seconds: f64,
    sample_seconds: f64,
    client: ProcessTreeMeasurement,
    agent: ProcessTreeMeasurement,
    total_cpu_percent_one_core: f64,
    total_rss_bytes: u64,
    total_threads: u64,
    total_fds: Option<u64>,
    pty_output_bytes: usize,
    mouse_rss: Option<MouseRssMeasurement>,
    console_trace: Option<ConsolePerfTrace>,
}

#[derive(Serialize)]
struct MouseRssMeasurement {
    baseline_bytes: u64,
    peak_bytes: u64,
    transient_growth_bytes: u64,
    after_workload_bytes: u64,
    after_recovery_bytes: u64,
    recovery_threshold_bytes: u64,
    recovery_seconds: Option<f64>,
}

struct RssSample {
    elapsed: Duration,
    bytes: u64,
}

struct RssSampler {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<Result<Vec<RssSample>>>>,
}

impl RssSampler {
    fn start(client_pid: u32, agent_pid: u32, started: Instant) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let sampler_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("kit-console-rss-sampler".to_owned())
            .spawn(move || {
                let mut samples = Vec::new();
                while !sampler_stop.load(Ordering::Acquire) {
                    let processes = capture_processes()?;
                    samples.push(RssSample {
                        elapsed: started.elapsed(),
                        bytes: combined_tree_rss(client_pid, agent_pid, &processes),
                    });
                    thread::sleep(Duration::from_millis(100));
                }
                Ok(samples)
            })
            .context("start Console RSS sampler")?;
        Ok(Self { stop, thread: Some(thread) })
    }

    fn finish(
        mut self,
        baseline_bytes: u64,
        after_workload_bytes: u64,
        workload_elapsed: Duration,
    ) -> Result<MouseRssMeasurement> {
        self.stop.store(true, Ordering::Release);
        let samples = self
            .thread
            .take()
            .expect("Console RSS sampler thread is present")
            .join()
            .map_err(|_| anyhow::anyhow!("Console RSS sampler panicked"))??;
        ensure!(!samples.is_empty(), "Console RSS sampler produced no samples");
        let peak_bytes = samples
            .iter()
            .filter(|sample| sample.elapsed <= workload_elapsed)
            .map(|sample| sample.bytes)
            .max()
            .unwrap_or(after_workload_bytes);
        let after_recovery_bytes =
            samples.last().map_or(after_workload_bytes, |sample| sample.bytes);
        let recovery_threshold_bytes =
            baseline_bytes.saturating_add(MOUSE_RECOVERY_TOLERANCE_BYTES);
        let recovery_seconds = samples
            .iter()
            .find(|sample| {
                sample.elapsed >= workload_elapsed && sample.bytes <= recovery_threshold_bytes
            })
            .map(|sample| sample.elapsed.saturating_sub(workload_elapsed).as_secs_f64());
        Ok(MouseRssMeasurement {
            baseline_bytes,
            peak_bytes,
            transient_growth_bytes: peak_bytes.saturating_sub(baseline_bytes),
            after_workload_bytes,
            after_recovery_bytes,
            recovery_threshold_bytes,
            recovery_seconds,
        })
    }
}

impl Drop for RssSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Deserialize, Serialize)]
struct ConsolePerfTrace {
    schema: String,
    redraws: u64,
    snapshots: u64,
    list_panes: u64,
    terminal_projections: u64,
    activity_screen_reads: u64,
    input_latency: InputLatencyTrace,
}

#[derive(Deserialize, Serialize)]
struct InputLatencyTrace {
    key: LatencyTrace,
    paste: LatencyTrace,
    mouse: LatencyTrace,
    resize: LatencyTrace,
}

#[derive(Deserialize, Serialize)]
struct LatencyTrace {
    count: u64,
    total_nanoseconds: u64,
    max_nanoseconds: u64,
    bucket_upper_microseconds: Vec<u64>,
    bucket_counts: Vec<u64>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "settles and samples a real Console process tree"]
async fn kit_console_performance_matrix() -> Result<()> {
    wezterm_config::designate_this_as_the_main_thread();
    wezterm_config::common_init(None, &[], true)
        .context("initialize headless WezTerm verifier config")?;

    let config = PerformanceConfig::from_env()?;
    let run_timeout = config.run_timeout()?;
    eprintln!(
        "console-perf phase=starting sessions={} workload={} deadline={}s",
        config.session_count,
        config.workload.label(),
        run_timeout.as_secs()
    );
    let mut harness = LocalConsoleHarness::start().await?;
    let trace_path = harness.runtime_root().join("console-performance-trace.json");
    let mut public_console = None;

    let outcome = tokio::time::timeout(run_timeout, async {
        eprintln!("console-perf phase=creating-sessions");
        let client = HeadlessConsoleClient::connect(&harness).await?;
        let run_id = uuid::Uuid::new_v4().simple().to_string();
        let sleep_seconds = config
            .settle
            .as_secs()
            .checked_add(config.sample.as_secs())
            .and_then(|seconds| seconds.checked_add(CLEANUP_MARGIN_SECS))
            .context("compute verifier session lifetime")?;
        let mut sessions = Vec::with_capacity(config.session_count);

        for index in 0..config.session_count {
            let marker = format!("KIT_CONSOLE_PERF_{run_id}_{index}");
            let script = if index == 0 {
                config.workload.session_script(&marker, sleep_seconds)
            } else {
                Workload::Idle.session_script(&marker, sleep_seconds)
            };
            let session = client.spawn_script(script).await?;
            client.wait_for_output(session.pane_id, &marker).await?;
            sessions.push(session);
        }
        sessions.sort_unstable();
        client.wait_for_topology(&sessions).await?;
        drop(client);

        public_console = Some(PublicConsole::start(
            &harness,
            PublicConsoleOptions {
                performance_trace_path: Some(trace_path.clone()),
                ..PublicConsoleOptions::default()
            },
        )?);
        if config.workload != Workload::Idle {
            let console = public_console.as_mut().context("public Console did not start")?;
            console.clear_output()?;
            console.send(b"\r")?;
            console.wait_for_output(b"you")?;
        }
        eprintln!("console-perf phase=settling duration={}s", config.settle.as_secs());
        tokio::time::sleep(config.settle).await;
        let console = public_console.as_mut().context("public Console did not start")?;

        let client_pid = console.process_id()?;
        let agent_pid = harness.agent_pid();
        let before = capture_processes()?;
        console.clear_output()?;
        let sample_started = Instant::now();
        let rss_sampler = (config.workload == Workload::Mouse)
            .then(|| RssSampler::start(client_pid, agent_pid, sample_started))
            .transpose()?;
        eprintln!("console-perf phase=sampling duration={}s", config.sample.as_secs());
        run_workload(console, config.workload, config.sample).await?;
        let elapsed = sample_started.elapsed();
        let after = capture_processes()?;

        if rss_sampler.is_some() {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        let client = measure_process_tree(client_pid, &before, &after, elapsed)?;
        let agent = measure_process_tree(agent_pid, &before, &after, elapsed)?;
        let baseline_rss = combined_tree_rss(client_pid, agent_pid, &before);
        let after_workload_rss = combined_tree_rss(client_pid, agent_pid, &after);
        let mouse_rss = rss_sampler
            .map(|sampler| sampler.finish(baseline_rss, after_workload_rss, elapsed))
            .transpose()?;
        if config.workload == Workload::Mouse && config.sample >= Duration::from_secs(60) {
            let measurement = mouse_rss.as_ref().context("mouse workload omitted RSS evidence")?;
            ensure!(
                measurement.transient_growth_bytes <= MOUSE_MAX_TRANSIENT_RSS_BYTES,
                "mouse flood transient RSS grew by {} bytes; budget is {} bytes",
                measurement.transient_growth_bytes,
                MOUSE_MAX_TRANSIENT_RSS_BYTES
            );
            ensure!(
                measurement.recovery_seconds.is_some_and(|seconds| seconds <= 2.0),
                "mouse flood RSS did not return within 2 seconds to its steady tolerance: \
                 baseline={} threshold={} after_recovery={}",
                measurement.baseline_bytes,
                measurement.recovery_threshold_bytes,
                measurement.after_recovery_bytes
            );
        }
        let total_fds = client.fds.zip(agent.fds).map(|(left, right)| left + right);
        let result = PerformanceResult {
            schema: "kit-console-performance-v1",
            platform: env::consts::OS,
            source_revision: env!("KIT_SOURCE_REVISION"),
            source_dirty: env!("KIT_SOURCE_DIRTY") == "true",
            upstream_wezterm_revision: env!("KIT_WEZTERM_REVISION"),
            maintained_wezterm_tree: env!("KIT_WEZTERM_RETAINED_TREE"),
            kit_binary_path: fs::canonicalize(env!("CARGO_BIN_EXE_kit"))
                .unwrap_or_else(|_| PathBuf::from(env!("CARGO_BIN_EXE_kit")))
                .to_string_lossy()
                .into_owned(),
            workload: config.workload.label(),
            session_count: config.session_count,
            settle_seconds: config.settle.as_secs_f64(),
            sample_seconds: elapsed.as_secs_f64(),
            total_cpu_percent_one_core: client.cpu_percent_one_core + agent.cpu_percent_one_core,
            total_rss_bytes: client.rss_bytes + agent.rss_bytes,
            total_threads: client.threads + agent.threads,
            total_fds,
            pty_output_bytes: console.output_len()?,
            mouse_rss,
            console_trace: None,
            client,
            agent,
        };
        Ok::<PerformanceResult, anyhow::Error>(result)
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "Console performance verifier exceeded its {}s hard deadline",
            run_timeout.as_secs()
        )
    })
    .and_then(|result| result);

    eprintln!("console-perf phase=cleaning-up");
    let finish_result = match public_console.take() {
        Some(console) => console.finish().map(|_| ()),
        None => Ok(()),
    };
    let trace_result = if finish_result.is_ok() {
        read_console_trace(&trace_path)
    } else {
        Err(anyhow::anyhow!("public Console did not exit cleanly enough to publish its trace"))
    };
    let shutdown_result = harness.shutdown();

    let mut result = outcome?;
    finish_result.context("finish measured public Console")?;
    result.console_trace = Some(trace_result?);
    shutdown_result.context("shutdown measured Console harness")?;
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

async fn run_workload(
    console: &mut PublicConsole,
    workload: Workload,
    duration: Duration,
) -> Result<()> {
    let deadline = Instant::now() + duration;
    let mut iteration = 0_u64;
    while Instant::now() < deadline {
        match workload {
            Workload::Idle | Workload::Output => {
                tokio::time::sleep(deadline.saturating_duration_since(Instant::now())).await;
            }
            Workload::Input => {
                console.send(b"x")?;
                console.send(b"\x1b[200~kit-console-perf\x1b[201~")?;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Workload::Mouse => {
                let mut batch = Vec::with_capacity(1_024);
                for offset in 0..64_u16 {
                    let column = 60 + ((iteration as u16 + offset) % 40);
                    batch.extend_from_slice(format!("\x1b[<35;{column};10M").as_bytes());
                }
                console.send(&batch)?;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Workload::Resize => {
                if iteration.is_multiple_of(2) {
                    console.resize(120, 40)?;
                } else {
                    console.resize(150, 48)?;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        iteration = iteration.saturating_add(1);
    }
    Ok(())
}

fn read_console_trace(path: &std::path::Path) -> Result<ConsolePerfTrace> {
    let source =
        fs::read(path).with_context(|| format!("read Console trace {}", path.display()))?;
    let trace: ConsolePerfTrace =
        serde_json::from_slice(&source).context("decode Console performance trace")?;
    ensure!(trace.schema == "kit-console-perf-trace-v1", "unexpected trace schema");
    for histogram in [
        &trace.input_latency.key,
        &trace.input_latency.paste,
        &trace.input_latency.mouse,
        &trace.input_latency.resize,
    ] {
        ensure!(
            histogram.bucket_upper_microseconds.len() == histogram.bucket_counts.len(),
            "Console trace latency bucket shape is inconsistent"
        );
        ensure!(
            histogram.bucket_counts.iter().sum::<u64>() == histogram.count,
            "Console trace latency count does not match its buckets"
        );
    }
    Ok(trace)
}

fn parse_session_count() -> Result<usize> {
    let Some(value) = env::var_os("KIT_CONSOLE_PERF_SESSIONS") else {
        return Ok(1);
    };
    let rendered = value.to_string_lossy();
    let count = rendered
        .parse::<usize>()
        .with_context(|| format!("KIT_CONSOLE_PERF_SESSIONS must be one of {SESSION_COUNTS:?}"))?;
    ensure!(
        SESSION_COUNTS.contains(&count),
        "KIT_CONSOLE_PERF_SESSIONS must be one of {SESSION_COUNTS:?}, got {count}"
    );
    Ok(count)
}

fn parse_duration(name: &str, default_seconds: u64) -> Result<Duration> {
    let seconds = match env::var_os(name) {
        Some(value) => value
            .to_string_lossy()
            .parse::<u64>()
            .with_context(|| format!("{name} must be an integer number of seconds"))?,
        None => default_seconds,
    };
    ensure!(
        (1..=MAX_DURATION_SECS).contains(&seconds),
        "{name} must be between 1 and {MAX_DURATION_SECS} seconds, got {seconds}"
    );
    Ok(Duration::from_secs(seconds))
}

fn measure_process_tree(
    root_pid: u32,
    before: &HashMap<u32, ProcessSample>,
    after: &HashMap<u32, ProcessSample>,
    elapsed: Duration,
) -> Result<ProcessTreeMeasurement> {
    ensure!(!elapsed.is_zero(), "Console performance sample duration was zero");
    ensure!(after.contains_key(&root_pid), "measured process {root_pid} exited during sampling");

    let before_tree = process_tree(root_pid, before);
    let after_tree = process_tree(root_pid, after);
    let measured_pids = before_tree.union(&after_tree).copied().collect::<HashSet<_>>();
    let cpu_seconds = measured_pids
        .iter()
        .filter_map(|pid| {
            let (start, end) = (before.get(pid)?, after.get(pid)?);
            if start.start_identity.is_some()
                && end.start_identity.is_some()
                && start.start_identity != end.start_identity
            {
                return None;
            }
            Some((end.cpu_seconds - start.cpu_seconds).max(0.0))
        })
        .sum::<f64>();
    let live = after_tree.iter().filter_map(|pid| after.get(pid)).collect::<Vec<_>>();
    let fds = live.iter().try_fold(0_u64, |total, process| process.fds.map(|fds| total + fds));
    let root = after.get(&root_pid).context("measured process disappeared")?;

    Ok(ProcessTreeMeasurement {
        root_pid,
        binary_path: root.executable.clone(),
        process_count: live.len(),
        cpu_percent_one_core: cpu_seconds / elapsed.as_secs_f64() * 100.0,
        rss_bytes: live.iter().map(|process| process.rss_bytes).sum(),
        threads: live.iter().map(|process| process.threads).sum(),
        fds,
    })
}

fn process_tree(root_pid: u32, processes: &HashMap<u32, ProcessSample>) -> HashSet<u32> {
    let mut tree = HashSet::from([root_pid]);
    loop {
        let previous_len = tree.len();
        for process in processes.values() {
            if tree.contains(&process.ppid) {
                tree.insert(process.pid);
            }
        }
        if tree.len() == previous_len {
            return tree;
        }
    }
}

fn combined_tree_rss(
    client_pid: u32,
    agent_pid: u32,
    processes: &HashMap<u32, ProcessSample>,
) -> u64 {
    process_tree(client_pid, processes)
        .into_iter()
        .chain(process_tree(agent_pid, processes))
        .filter_map(|pid| processes.get(&pid))
        .map(|process| process.rss_bytes)
        .sum()
}

#[cfg(target_os = "linux")]
fn capture_processes() -> Result<HashMap<u32, ProcessSample>> {
    let clock_ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    ensure!(clock_ticks > 0, "Linux did not report a valid clock tick rate");
    let clock_ticks = clock_ticks as f64;
    let mut processes = HashMap::new();

    for entry in fs::read_dir("/proc").context("read Linux process table")? {
        let entry = entry.context("read Linux process entry")?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let process_root = entry.path();
        let stat = match fs::read_to_string(process_root.join("stat")) {
            Ok(stat) => stat,
            Err(_) => continue,
        };
        let status = match fs::read_to_string(process_root.join("status")) {
            Ok(status) => status,
            Err(_) => continue,
        };
        let Some(sample) = parse_linux_process(pid, &stat, &status, &process_root, clock_ticks)
        else {
            continue;
        };
        processes.insert(pid, sample);
    }
    Ok(processes)
}

#[cfg(target_os = "linux")]
fn parse_linux_process(
    pid: u32,
    stat: &str,
    status: &str,
    process_root: &std::path::Path,
    clock_ticks: f64,
) -> Option<ProcessSample> {
    // `comm` is parenthesized and may itself contain spaces or `)`, so split after the final `)`.
    let suffix = stat.rsplit_once(") ")?.1.split_whitespace().collect::<Vec<_>>();
    let ppid = suffix.get(1)?.parse::<u32>().ok()?;
    let user_ticks = suffix.get(11)?.parse::<u64>().ok()?;
    let system_ticks = suffix.get(12)?.parse::<u64>().ok()?;
    let start_identity = suffix.get(19)?.parse::<u64>().ok()?;
    let rss_kib = linux_status_value(status, "VmRSS:").unwrap_or(0);
    let threads = linux_status_value(status, "Threads:")?;
    let fds = fs::read_dir(process_root.join("fd"))
        .ok()
        .map(|entries| entries.filter_map(|entry| entry.ok()).count() as u64);
    let executable = fs::read_link(process_root.join("exe"))
        .ok()
        .map(|path| path.to_string_lossy().into_owned());

    Some(ProcessSample {
        pid,
        ppid,
        cpu_seconds: (user_ticks + system_ticks) as f64 / clock_ticks,
        rss_bytes: rss_kib.checked_mul(1024)?,
        threads,
        fds,
        executable,
        start_identity: Some(start_identity),
    })
}

#[cfg(target_os = "linux")]
fn linux_status_value(status: &str, name: &str) -> Option<u64> {
    status.lines().find_map(|line| line.strip_prefix(name))?.split_whitespace().next()?.parse().ok()
}

#[cfg(target_os = "macos")]
fn capture_processes() -> Result<HashMap<u32, ProcessSample>> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,time=,rss=,thcount=,comm="])
        .output()
        .context("read macOS process metrics")?;
    ensure!(output.status.success(), "macOS ps failed: {}", output.status);
    let source = std::str::from_utf8(&output.stdout).context("decode macOS process metrics")?;
    let mut processes = HashMap::new();
    for line in source.lines() {
        let mut fields = line.split_whitespace();
        let Some(pid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(ppid) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let Some(cpu_seconds) = fields.next().and_then(parse_ps_time) else {
            continue;
        };
        let Some(rss_kib) = fields.next().and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        let Some(threads) = fields.next().and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        let executable = match fields.collect::<Vec<_>>().join(" ") {
            executable if executable.is_empty() => None,
            executable => Some(executable),
        };
        processes.insert(
            pid,
            ProcessSample {
                pid,
                ppid,
                cpu_seconds,
                rss_bytes: rss_kib.saturating_mul(1024),
                threads,
                fds: None,
                executable,
                start_identity: None,
            },
        );
    }
    Ok(processes)
}

#[cfg(target_os = "macos")]
fn parse_ps_time(value: &str) -> Option<f64> {
    let (days, clock) = match value.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, value),
    };
    let fields = clock.split(':').collect::<Vec<_>>();
    let (hours, minutes, seconds) = match fields.as_slice() {
        [minutes, seconds] => (0, minutes.parse::<u64>().ok()?, seconds.parse::<f64>().ok()?),
        [hours, minutes, seconds] => {
            (hours.parse::<u64>().ok()?, minutes.parse::<u64>().ok()?, seconds.parse::<f64>().ok()?)
        }
        _ => return None,
    };
    Some(days as f64 * 86_400.0 + hours as f64 * 3_600.0 + minutes as f64 * 60.0 + seconds)
}
