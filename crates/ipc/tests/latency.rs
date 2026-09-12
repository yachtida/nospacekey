//! Zenzai runtime benchmark (Phase 0).
//!
//! The ignored test is intentionally an IPC-level benchmark. It starts the
//! release engine on a private pipe, feeds a fixed corpus, and records the
//! same `Convert`/`LiveConvert` deadlines used by the product. The default
//! run is 1,000 requests per operation; the PowerShell wrapper supplies smoke
//! counts for local checks.
//!
//! CPU mode requires a real Zenzai model path. A missing model is an error,
//! not an invitation to measure classic conversion. Vulkan mode sets an
//! explicit backend request and requires a backend evidence line before the
//! first inference; this keeps an unsupported Vulkan build from being
//! reported as a CPU result.
//!
//! Run through `scripts/bench-zenzai-runtime.ps1`.

use ipc::client::{current_session_id, EngineClient};
use ipc::protocol::{Request, Response};
use serde::Serialize;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const CONVERT_DEADLINE: Duration = Duration::from_millis(1_200);
const LIVE_CONVERT_DEADLINE: Duration = Duration::from_millis(400);
const DEFAULT_REQUESTS: usize = 1_000;
const DEFAULT_INFERENCE_LIMIT: u32 = 1;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_CPU_TARGET_SAMPLE_RATIO: f64 = 0.90;
const DEFAULT_WARMUP_TIMEOUT: Duration = Duration::from_secs(60);

/// The corpus is deliberately stable: changing it invalidates comparison
/// with the baseline and should be a reviewed benchmark change.
const INPUT_CORPUS: &[&str] = &[
    "nihongowonyuuryokusuru",
    "watashinonamaeha",
    "kyounotenkinoyohou",
    "ashitahachikakutoshokai",
    "toukyounoekiikimasu",
    "konoapuriwotsukatteimasu",
    "kanjihenkanwokakunin",
    "yoyakunojikandesu",
];

const QUALITY_PREFIXES: &[&str] = &[
    "watashiha",
    "watashitachiha",
    "anataha",
    "kareha",
    "kanojoha",
    "senseiha",
    "gakuseiha",
    "gakuseitachiha",
    "tomodachiha",
    "kodomotachiha",
    "otoutoha",
    "hahaoyaha",
    "chichiha",
    "kyoudaiha",
    "shainha",
    "kachouha",
    "buchouha",
    "ishaha",
    "kangoshiha",
    "kyoushiha",
];

const QUALITY_SUFFIXES: &[&str] = &[
    "gakkouheikimasu",
    "kaishaniikimasu",
    "ienikaerimasu",
    "honwoyomimasu",
    "shinbunwoyomimasu",
    "tegamiwokakimasu",
    "gohanwotabemasu",
    "ochawonomimasu",
    "tenkiwokakuninshimasu",
    "benkyoushimasu",
];
const QUALITY_CORPUS_SIZE: usize = QUALITY_PREFIXES.len() * QUALITY_SUFFIXES.len();

fn quality_input(index: usize) -> String {
    let corpus_index = index % QUALITY_CORPUS_SIZE;
    let prefix = QUALITY_PREFIXES[corpus_index / QUALITY_SUFFIXES.len()];
    let suffix = QUALITY_SUFFIXES[corpus_index % QUALITY_SUFFIXES.len()];
    format!("{prefix}{suffix}")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Backend {
    Cpu,
    Vulkan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BenchmarkMode {
    Latency,
    Quality,
}

impl BenchmarkMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "latency" => Ok(Self::Latency),
            "quality" => Ok(Self::Quality),
            _ => Err(format!(
                "benchmark mode must be latency or quality, got {value:?}"
            )),
        }
    }
}

impl Backend {
    fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "cpu" => Ok(Self::Cpu),
            "vulkan" => Ok(Self::Vulkan),
            _ => Err(format!("backend must be Cpu or Vulkan, got {value:?}")),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "Cpu",
            Self::Vulkan => "Vulkan",
        }
    }

    fn deadline(self, operation: Operation) -> Duration {
        let _ = self;
        operation.deadline()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operation {
    Convert,
    LiveConvert,
}

impl Operation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Convert => "Convert",
            Self::LiveConvert => "LiveConvert",
        }
    }

    fn deadline(self) -> Duration {
        match self {
            Self::Convert => CONVERT_DEADLINE,
            Self::LiveConvert => LIVE_CONVERT_DEADLINE,
        }
    }

    fn request(self, session: i64, index: usize) -> Request {
        match self {
            Self::Convert => Request::Convert {
                session,
                left_context: None,
            },
            Self::LiveConvert => Request::LiveConvert {
                session,
                seq: index as u64,
                left_context: None,
                auto_commit: false,
            },
        }
    }

    fn accepts(self, response: &Response, index: usize) -> Result<(), String> {
        match (self, response) {
            (Self::Convert, Response::Candidates { .. }) => Ok(()),
            (Self::LiveConvert, Response::LiveResult { seq, .. }) if *seq == index as u64 => Ok(()),
            (_, Response::Error { message }) => Err(message.clone()),
            (Self::LiveConvert, Response::LiveResult { seq, .. }) => Err(format!(
                "LiveConvert sequence mismatch: expected {}, got {seq}",
                index
            )),
            (_, other) => Err(format!("unexpected {} response: {other:?}", self.as_str())),
        }
    }
}

#[derive(Clone, Debug)]
struct BenchmarkConfig {
    backend: Backend,
    mode: BenchmarkMode,
    model_path: PathBuf,
    engine_path: PathBuf,
    runtime_dir: PathBuf,
    source_engine_path: PathBuf,
    source_runtime_dir: PathBuf,
    convert_requests: usize,
    live_convert_requests: usize,
    inference_limit: u32,
    connect_timeout: Duration,
    warmup_timeout: Duration,
    log_dir: PathBuf,
    cpu_contention_percent: u8,
    cpu_contention_duration: Duration,
}

impl BenchmarkConfig {
    fn from_env() -> Result<Self, String> {
        let backend = Backend::parse(
            &std::env::var("NOSPACEKEY_BENCH_BACKEND").unwrap_or_else(|_| "Cpu".into()),
        )?;
        let mode = BenchmarkMode::parse(
            &std::env::var("NOSPACEKEY_BENCH_MODE").unwrap_or_else(|_| "latency".into()),
        )?;
        let model_path = required_path_env("NOSPACEKEY_ZENZAI_WEIGHT", "Zenzai model")?;
        if !model_path.is_file() {
            return Err(format!(
                "Zenzai model does not exist: {}",
                model_path.display()
            ));
        }

        let engine_path = std::env::var_os("NOSPACEKEY_BENCH_ENGINE_EXE")
            .map(PathBuf::from)
            .unwrap_or_else(default_release_engine_path);
        if !engine_path.is_file() {
            return Err(format!(
                "release engine executable does not exist: {}",
                engine_path.display()
            ));
        }
        let runtime_dir = std::env::var_os("NOSPACEKEY_ZENZAI_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_runtime_dir(backend));
        validate_runtime_dir(&runtime_dir, backend)?;
        let source_engine_path =
            required_path_env("NOSPACEKEY_BENCH_SOURCE_ENGINE_EXE", "source engine")?;
        let source_runtime_dir =
            required_path_env("NOSPACEKEY_BENCH_SOURCE_RUNTIME_DIR", "source runtime")?;
        if !source_engine_path.is_file() {
            return Err(format!(
                "source engine executable does not exist: {}",
                source_engine_path.display()
            ));
        }
        validate_runtime_dir(&source_runtime_dir, backend)?;
        if backend == Backend::Cpu
            && !source_runtime_dir.components().any(|component| {
                component
                    .as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("zenzai-cpu-runtime")
            })
        {
            return Err(format!(
                "CPU baseline source runtime must be last-test-logs/zenzai-cpu-runtime: {}",
                source_runtime_dir.display()
            ));
        }

        let convert_requests = env_usize("NOSPACEKEY_BENCH_CONVERT_COUNT", DEFAULT_REQUESTS)?;
        let live_convert_requests =
            env_usize("NOSPACEKEY_BENCH_LIVE_CONVERT_COUNT", DEFAULT_REQUESTS)?;
        let inference_limit =
            env_u32("NOSPACEKEY_ZENZAI_INFERENCE_LIMIT", DEFAULT_INFERENCE_LIMIT)?;
        if !(1..=10).contains(&inference_limit) {
            return Err(format!(
                "NOSPACEKEY_ZENZAI_INFERENCE_LIMIT must be 1..=10, got {inference_limit}"
            ));
        }
        let connect_timeout = env_duration_secs(
            "NOSPACEKEY_BENCH_CONNECT_TIMEOUT_SEC",
            DEFAULT_CONNECT_TIMEOUT,
        )?;
        let warmup_timeout = env_duration_secs(
            "NOSPACEKEY_BENCH_WARMUP_TIMEOUT_SEC",
            DEFAULT_WARMUP_TIMEOUT,
        )?;
        let log_dir = std::env::var_os("NOSPACEKEY_BENCH_LOG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        fs::create_dir_all(&log_dir)
            .map_err(|e| format!("create benchmark log directory {}: {e}", log_dir.display()))?;
        let cpu_contention_percent = env_u8("NOSPACEKEY_BENCH_CPU_CONTENTION_PERCENT", 0)?;
        if cpu_contention_percent > 100 {
            return Err(format!(
                "NOSPACEKEY_BENCH_CPU_CONTENTION_PERCENT must be 0..=100, got {cpu_contention_percent}"
            ));
        }
        let cpu_contention_duration = env_duration_secs(
            "NOSPACEKEY_BENCH_CPU_CONTENTION_DURATION_SEC",
            if cpu_contention_percent > 0 {
                Duration::from_secs(600)
            } else {
                Duration::ZERO
            },
        )?;
        if cpu_contention_percent > 0 && cpu_contention_duration.is_zero() {
            return Err(
                "NOSPACEKEY_BENCH_CPU_CONTENTION_DURATION_SEC must be positive when contention is enabled".into(),
            );
        }

        Ok(Self {
            backend,
            mode,
            model_path,
            engine_path,
            runtime_dir,
            source_engine_path,
            source_runtime_dir,
            convert_requests,
            live_convert_requests,
            inference_limit,
            connect_timeout,
            warmup_timeout,
            log_dir,
            cpu_contention_percent,
            cpu_contention_duration,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
struct LatencySummary {
    sample_count: usize,
    p50_ms: Option<f64>,
    p95_ms: Option<f64>,
    p99_ms: Option<f64>,
    max_ms: Option<f64>,
    mean_ms: Option<f64>,
    timeouts: u64,
    errors: u64,
    total_engine_cpu_seconds: Option<f64>,
    engine_cpu_seconds_per_completed_request: Option<f64>,
}

impl LatencySummary {
    fn from_samples(
        samples: &[f64],
        timeouts: u64,
        errors: u64,
        total_engine_cpu_seconds: Option<f64>,
    ) -> Self {
        let completed = samples.len();
        Self {
            sample_count: completed,
            p50_ms: percentile(samples, 0.50),
            p95_ms: percentile(samples, 0.95),
            p99_ms: percentile(samples, 0.99),
            max_ms: samples.iter().copied().reduce(f64::max),
            mean_ms: (!samples.is_empty())
                .then(|| samples.iter().sum::<f64>() / samples.len() as f64),
            timeouts,
            errors,
            total_engine_cpu_seconds,
            engine_cpu_seconds_per_completed_request: total_engine_cpu_seconds
                .filter(|_| completed > 0)
                .map(|seconds| seconds / completed as f64),
        }
    }
}

#[derive(Debug, Serialize)]
struct OperationReport {
    operation: &'static str,
    deadline_ms: u64,
    requested: usize,
    completed: usize,
    first_inference_ms: Option<f64>,
    warm: LatencySummary,
    all: LatencySummary,
    cold_load: ColdLoadReport,
    process_restarts: u32,
    engine_crashes: u32,
    evidence: RuntimeEvidence,
    quality: Option<QualityReport>,
    messages: Vec<String>,
    log_path: String,
}

#[derive(Clone, Debug, Serialize)]
struct QualityReport {
    corpus_size: usize,
    corpus: Vec<String>,
    completed: usize,
    top1: Vec<Option<String>>,
    top5: Vec<Vec<String>>,
    session_switches_requested: usize,
    session_switches_completed: usize,
    session_switch_errors: u64,
    session_switch_timeouts: u64,
}

#[derive(Debug, Serialize)]
struct ColdLoadReport {
    spawn_to_connect_ms: Option<f64>,
    service_listening_ms: Option<f64>,
    warmup_ms: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct RuntimeEvidence {
    gpu_active: bool,
    decode_verified: bool,
    decode_attempts: Option<u64>,
    offloaded_13_of_13: bool,
    device: Option<String>,
    backend: Option<String>,
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    schema_version: u32,
    status: &'static str,
    backend: &'static str,
    model_path: String,
    engine_path: String,
    runtime_dir: String,
    source_engine_path: String,
    source_runtime_dir: String,
    inference_limit: u32,
    corpus: Vec<String>,
    deadlines_ms: Deadlines,
    requested: RequestedCounts,
    cpu_contention_percent: u8,
    cpu_contention_duration_ms: u64,
    cpu_utilization: Option<CpuUtilizationReport>,
    contention_workload: Option<ContentionWorkloadReport>,
    engine_crashes: u32,
    timeouts: u64,
    errors: u64,
    evidence: RuntimeEvidence,
    quality: Option<QualityReport>,
    operations: Vec<OperationReport>,
    fatal_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ContentionWorkloadReport {
    requested_duration_ms: u64,
    sessions_requested: usize,
    sessions_started: usize,
    rounds_requested: usize,
    rounds_completed: usize,
    convert_completed: usize,
    live_convert_completed: usize,
    candidate_moves: usize,
    commits: usize,
    timeouts: u64,
    errors: u64,
    engine_crashes: u32,
    process_restarts: u32,
    zenzai_fallback_events: u32,
    failure_stages: Vec<&'static str>,
    evidence: RuntimeEvidence,
    gate_passed: bool,
}

#[derive(Debug, Serialize)]
struct Deadlines {
    convert: u64,
    live_convert: u64,
}

#[derive(Debug, Serialize)]
struct RequestedCounts {
    convert: usize,
    live_convert: usize,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct SystemCpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}

#[derive(Clone, Debug, Serialize)]
struct CpuUtilizationSample {
    elapsed_ms: u64,
    percent: f64,
}

#[derive(Clone, Debug, Serialize)]
struct CpuUtilizationReport {
    target_percent: u8,
    requested_duration_ms: u64,
    measured_duration_ms: u64,
    sample_interval_ms: u64,
    samples: Vec<CpuUtilizationSample>,
    sample_count: usize,
    average_percent: Option<f64>,
    minimum_percent: Option<f64>,
    samples_at_or_above_target: usize,
    sufficient_samples_ratio: f64,
    gate_passed: bool,
    measurement_error: Option<String>,
}

impl CpuUtilizationReport {
    fn from_samples(
        target_percent: u8,
        requested_duration: Duration,
        sample_interval: Duration,
        values: Vec<f64>,
    ) -> Self {
        let samples: Vec<CpuUtilizationSample> = values
            .into_iter()
            .enumerate()
            .map(|(index, percent)| CpuUtilizationSample {
                elapsed_ms: sample_interval.as_millis() as u64 * (index as u64 + 1),
                percent,
            })
            .collect();
        Self::from_measurements(
            target_percent,
            requested_duration,
            sample_interval,
            sample_interval.saturating_mul(samples.len() as u32),
            samples,
            None,
        )
    }

    fn from_measurements(
        target_percent: u8,
        requested_duration: Duration,
        sample_interval: Duration,
        measured_duration: Duration,
        samples: Vec<CpuUtilizationSample>,
        measurement_error: Option<String>,
    ) -> Self {
        let values: Vec<f64> = samples.iter().map(|sample| sample.percent).collect();
        let sample_count = values.len();
        let average_percent =
            (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64);
        let minimum_percent = values.iter().copied().reduce(f64::min);
        let samples_at_or_above_target = values
            .iter()
            .filter(|value| **value >= f64::from(target_percent))
            .count();
        let sufficient_samples_ratio = if sample_count == 0 {
            0.0
        } else {
            samples_at_or_above_target as f64 / sample_count as f64
        };
        let gate_passed = measurement_error.is_none()
            && measured_duration >= requested_duration
            && average_percent.is_some_and(|value| value >= f64::from(target_percent))
            && sufficient_samples_ratio >= MIN_CPU_TARGET_SAMPLE_RATIO;
        Self {
            target_percent,
            requested_duration_ms: requested_duration.as_millis() as u64,
            measured_duration_ms: measured_duration.as_millis() as u64,
            sample_interval_ms: sample_interval.as_millis() as u64,
            samples,
            sample_count,
            average_percent,
            minimum_percent,
            samples_at_or_above_target,
            sufficient_samples_ratio,
            gate_passed,
            measurement_error,
        }
    }
}

fn cpu_utilization_percent(previous: SystemCpuTimes, current: SystemCpuTimes) -> Option<f64> {
    let idle_delta = current.idle.checked_sub(previous.idle)?;
    let kernel_delta = current.kernel.checked_sub(previous.kernel)?;
    let user_delta = current.user.checked_sub(previous.user)?;
    let total_delta = kernel_delta.checked_add(user_delta)?;
    (total_delta > 0).then(|| (1.0 - idle_delta as f64 / total_delta as f64) * 100.0)
}

#[cfg(windows)]
fn read_system_cpu_times() -> Result<SystemCpuTimes, String> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetSystemTimes;

    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetSystemTimes(
            Some(&mut idle as *mut _),
            Some(&mut kernel as *mut _),
            Some(&mut user as *mut _),
        )
        .map_err(|error| format!("GetSystemTimes failed: {error}"))?;
    }
    let to_u64 =
        |value: FILETIME| (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime);
    Ok(SystemCpuTimes {
        idle: to_u64(idle),
        kernel: to_u64(kernel),
        user: to_u64(user),
    })
}

#[cfg(not(windows))]
fn read_system_cpu_times() -> Result<SystemCpuTimes, String> {
    Err("system CPU utilization requires Windows GetSystemTimes".into())
}

struct CpuUtilizationMonitor {
    stop: Arc<AtomicBool>,
    samples: Arc<Mutex<Vec<CpuUtilizationSample>>>,
    measurement_error: Arc<Mutex<Option<String>>>,
    worker: Option<std::thread::JoinHandle<()>>,
    target_percent: u8,
    requested_duration: Duration,
    sample_interval: Duration,
    started: Instant,
}

impl CpuUtilizationMonitor {
    fn start(target_percent: u8, requested_duration: Duration) -> Result<Self, String> {
        let sample_interval = Duration::from_secs(1);
        let previous = read_system_cpu_times()?;
        let stop = Arc::new(AtomicBool::new(false));
        let samples = Arc::new(Mutex::new(Vec::new()));
        let measurement_error = Arc::new(Mutex::new(None));
        let worker_stop = Arc::clone(&stop);
        let worker_samples = Arc::clone(&samples);
        let worker_error = Arc::clone(&measurement_error);
        let started = Instant::now();
        let worker = std::thread::spawn(move || {
            let mut previous = previous;
            loop {
                std::thread::sleep(sample_interval);
                if worker_stop.load(Ordering::Relaxed) {
                    break;
                }
                let current = match read_system_cpu_times() {
                    Ok(value) => value,
                    Err(error) => {
                        *worker_error.lock().expect("CPU error mutex poisoned") = Some(error);
                        break;
                    }
                };
                if let Some(percent) = cpu_utilization_percent(previous, current) {
                    worker_samples
                        .lock()
                        .expect("CPU samples mutex poisoned")
                        .push(CpuUtilizationSample {
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            percent,
                        });
                }
                previous = current;
            }
        });
        Ok(Self {
            stop,
            samples,
            measurement_error,
            worker: Some(worker),
            target_percent,
            requested_duration,
            sample_interval,
            started,
        })
    }

    fn finish_until(mut self, deadline: Instant) -> CpuUtilizationReport {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            std::thread::sleep(remaining);
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let samples = self
            .samples
            .lock()
            .expect("CPU samples mutex poisoned")
            .clone();
        let measurement_error = self
            .measurement_error
            .lock()
            .expect("CPU error mutex poisoned")
            .clone();
        CpuUtilizationReport::from_measurements(
            self.target_percent,
            self.requested_duration,
            self.sample_interval,
            self.started.elapsed(),
            samples,
            measurement_error,
        )
    }
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Optional contention is bounded to this benchmark process and is disabled
/// by default. The stop flag prevents worker threads from surviving a failed
/// connection or a Ctrl-C-triggered test teardown.
struct CpuContentionGuard {
    stop: Arc<AtomicBool>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl CpuContentionGuard {
    fn start(percent: u8) -> Self {
        if percent == 0 {
            return Self {
                stop: Arc::new(AtomicBool::new(false)),
                workers: Vec::new(),
            };
        }
        let stop = Arc::new(AtomicBool::new(false));
        let workers_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(64);
        let mut workers = Vec::with_capacity(workers_count);
        for _ in 0..workers_count {
            let stop_flag = Arc::clone(&stop);
            workers.push(std::thread::spawn(move || {
                let period = Duration::from_millis(20);
                // Acceptance uses measured system load; a small reserve avoids overshooting
                // the requested target on machines with unrelated background activity.
                let duty_percent = percent.saturating_sub(2);
                let on = period.mul_f64(f64::from(duty_percent) / 100.0);
                while !stop_flag.load(Ordering::Relaxed) {
                    let busy_until = Instant::now() + on;
                    while Instant::now() < busy_until {
                        std::hint::spin_loop();
                    }
                    if on < period {
                        std::thread::sleep(period - on);
                    }
                }
            }));
        }
        Self { stop, workers }
    }
}

impl Drop for CpuContentionGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn percentile(samples: &[f64], p: f64) -> Option<f64> {
    if samples.is_empty() || !(0.0..=1.0).contains(&p) {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted.get(index).copied()
}

fn run_operation(config: &BenchmarkConfig, operation: Operation) -> OperationReport {
    let requested = match operation {
        Operation::Convert => config.convert_requests,
        Operation::LiveConvert => config.live_convert_requests,
    };
    let nonce = BENCHMARK_NONCE.fetch_add(1, Ordering::Relaxed);
    let log_path = config.log_dir.join(format!(
        "nospacekey-zenzai-bench-{}-{}-{}.log",
        std::process::id(),
        nonce,
        operation.as_str().to_ascii_lowercase()
    ));
    let mut messages = Vec::new();
    let mut samples = Vec::new();
    let mut timeouts = 0_u64;
    let mut errors = 0_u64;
    let mut engine_crashes = 0_u32;
    let mut evidence = RuntimeEvidence::default();
    let capture_quality = config.mode == BenchmarkMode::Quality && operation == Operation::Convert;
    let mut quality_top1 = Vec::new();
    let mut quality_top5 = Vec::new();
    let quality_session_switches_requested = if capture_quality { 100 } else { 0 };
    let mut quality_session_switches_completed = 0;
    let mut quality_session_switch_errors = 0_u64;
    let mut quality_session_switch_timeouts = 0_u64;
    let mut first_inference_ms = None;
    let mut cold_load = ColdLoadReport {
        spawn_to_connect_ms: None,
        service_listening_ms: None,
        warmup_ms: None,
    };
    let process_restarts = 0;

    let pipe = format!(
        r"\\.\pipe\nospacekey-engine-bench-{}-{}.s{}",
        std::process::id(),
        nonce,
        current_session_id()
    );
    let spawn_started = Instant::now();
    let mut child = match spawn_engine(config, &pipe, &log_path) {
        Ok(child) => ChildGuard::new(child),
        Err(error) => {
            return failed_operation_report(
                operation,
                requested,
                log_path,
                0,
                format!("spawn release engine: {error}"),
            )
        }
    };
    let process_id = child.id();
    let mut client = match connect_to_engine(&mut child, &pipe, config.connect_timeout) {
        Ok(client) => {
            cold_load.spawn_to_connect_ms = Some(elapsed_ms(spawn_started));
            client
        }
        Err(error) => {
            return failed_operation_report(
                operation,
                requested,
                log_path,
                child_crash_count(&mut child),
                format!("connect_to({pipe}): {error}"),
            )
        }
    };

    // The log line is emitted after service construction and warm-up. CPU
    // mode requires an explicit Zenzai marker; classic conversion is not a
    // baseline. Vulkan requires the stronger GPU evidence below.
    cold_load.service_listening_ms = wait_for_log_value(
        &log_path,
        "ev=coldstart stage=listening",
        "total_ms=",
        config.connect_timeout,
    );
    cold_load.warmup_ms = wait_for_log_value(
        &log_path,
        "ev=coldstart stage=warmup",
        "ms=",
        config.warmup_timeout,
    );
    if config.backend == Backend::Vulkan {
        match wait_for_vulkan_evidence(&log_path, Duration::from_secs(5)) {
            Ok(value) => evidence = value,
            Err(message) => {
                errors += 1;
                messages.push(message);
                drop(client);
                return OperationReport {
                    operation: operation.as_str(),
                    deadline_ms: operation.deadline().as_millis() as u64,
                    requested,
                    completed: 0,
                    first_inference_ms,
                    warm: LatencySummary::from_samples(&[], timeouts, errors, None),
                    all: LatencySummary::from_samples(&[], timeouts, errors, None),
                    cold_load,
                    process_restarts,
                    engine_crashes,
                    evidence,
                    quality: None,
                    messages,
                    log_path: log_path.display().to_string(),
                };
            }
        }
    } else if !wait_for_cpu_zenzai_evidence(&log_path, &config.model_path, config.connect_timeout) {
        messages.push(
            "CPU baseline did not prove Zenzai mode and the requested model loaded; refusing to measure classic conversion".into(),
        );
        errors += 1;
        drop(client);
        return OperationReport {
            operation: operation.as_str(),
            deadline_ms: operation.deadline().as_millis() as u64,
            requested,
            completed: 0,
            first_inference_ms,
            warm: LatencySummary::from_samples(&[], timeouts, errors, None),
            all: LatencySummary::from_samples(&[], timeouts, errors, None),
            cold_load,
            process_restarts,
            engine_crashes,
            evidence,
            quality: None,
            messages,
            log_path: log_path.display().to_string(),
        };
    }
    for index in 0..requested {
        let setup_deadline = Instant::now() + config.connect_timeout;
        let session = match client.request_within(&Request::StartSession, setup_deadline) {
            Ok(Response::Session { session, .. }) => session,
            Ok(other) => {
                errors += 1;
                messages.push(format!(
                    "request {index}: unexpected StartSession response: {other:?}"
                ));
                break;
            }
            Err(error) => {
                errors += 1;
                if error.kind() == io::ErrorKind::TimedOut {
                    timeouts += 1;
                }
                messages.push(format!("request {index}: StartSession failed: {error}"));
                engine_crashes += child_crash_count(&mut child);
                break;
            }
        };
        let quality_input_value = capture_quality.then(|| quality_input(index));
        let input = quality_input_value
            .as_deref()
            .unwrap_or(INPUT_CORPUS[index % INPUT_CORPUS.len()]);
        if let Err(error) = client.request_within(
            &Request::Insert {
                session,
                text: input.to_string(),
                style: None,
            },
            Instant::now() + config.connect_timeout,
        ) {
            errors += 1;
            if error.kind() == io::ErrorKind::TimedOut {
                timeouts += 1;
            }
            messages.push(format!("request {index}: Insert failed: {error}"));
            engine_crashes += child_crash_count(&mut child);
            break;
        }

        let started = Instant::now();
        let response = client.request_within(
            &operation.request(session, index),
            started + config.backend.deadline(operation),
        );
        let elapsed = elapsed_ms(started);
        if index == 0 {
            first_inference_ms = Some(elapsed);
        }
        let mut request_failed = false;
        match response {
            Ok(response) => match operation.accepts(&response, index) {
                Ok(()) => {
                    if capture_quality {
                        if let Response::Candidates { candidates } = &response {
                            quality_top1.push(candidates.first().cloned());
                            quality_top5.push(candidates.iter().take(5).cloned().collect());
                        } else {
                            errors += 1;
                            request_failed = true;
                            messages
                                .push("quality Convert response did not contain candidates".into());
                        }
                    }
                    if config.backend == Backend::Vulkan && !evidence.decode_verified {
                        if wait_for_vulkan_decode_evidence(&log_path, Duration::from_secs(3)) {
                            evidence.decode_verified = true;
                        } else {
                            errors += 1;
                            request_failed = true;
                            messages.push(
                                "Vulkan decode evidence was not observed after a successful IPC response"
                                    .into(),
                            );
                        }
                    }
                    if !request_failed {
                        samples.push(elapsed);
                    }
                }
                Err(message) => {
                    errors += 1;
                    request_failed = true;
                    messages.push(format!("request {index}: {message}"));
                }
            },
            Err(error) => {
                errors += 1;
                if error.kind() == io::ErrorKind::TimedOut {
                    timeouts += 1;
                }
                messages.push(format!(
                    "request {index}: {} failed: {error}",
                    operation.as_str()
                ));
                request_failed = true;
            }
        }

        if request_failed {
            engine_crashes += child_crash_count(&mut child);
            // A timed-out or failed frame may leave a partial response on the
            // pipe. Do not send EndSession and miscount cleanup as another
            // benchmark timeout.
            break;
        }
        if let Err(error) = client.request_within(
            &Request::EndSession { session },
            Instant::now() + config.connect_timeout,
        ) {
            errors += 1;
            if error.kind() == io::ErrorKind::TimedOut {
                timeouts += 1;
            }
            messages.push(format!("request {index}: EndSession failed: {error}"));
            engine_crashes += child_crash_count(&mut child);
            break;
        }
    }

    if capture_quality && errors == 0 {
        let first_session = client.request_within(
            &Request::StartSession,
            Instant::now() + config.connect_timeout,
        );
        let second_session = client.request_within(
            &Request::StartSession,
            Instant::now() + config.connect_timeout,
        );
        let sessions = match (first_session, second_session) {
            (
                Ok(Response::Session { session: first, .. }),
                Ok(Response::Session {
                    session: second, ..
                }),
            ) => Some((first, second)),
            (first_result, second_result) => {
                for result in [first_result, second_result] {
                    if let Err(error) = result {
                        quality_session_switch_errors += 1;
                        if error.kind() == io::ErrorKind::TimedOut {
                            quality_session_switch_timeouts += 1;
                        }
                    }
                }
                engine_crashes += child_crash_count(&mut child);
                None
            }
        };
        if let Some((first, second)) = sessions {
            for index in 0..quality_session_switches_requested {
                let session = if index % 2 == 0 { first } else { second };
                let input = quality_input(index + 1_000);
                if let Err(error) = client.request_within(
                    &Request::Insert {
                        session,
                        text: input.to_string(),
                        style: None,
                    },
                    Instant::now() + config.connect_timeout,
                ) {
                    quality_session_switch_errors += 1;
                    if error.kind() == io::ErrorKind::TimedOut {
                        quality_session_switch_timeouts += 1;
                    }
                    engine_crashes += child_crash_count(&mut child);
                    break;
                }
                let candidates = match client.request_within(
                    &Request::Convert {
                        session,
                        left_context: None,
                    },
                    Instant::now() + CONVERT_DEADLINE,
                ) {
                    Ok(Response::Candidates { candidates }) if !candidates.is_empty() => candidates,
                    Ok(_) => {
                        quality_session_switch_errors += 1;
                        break;
                    }
                    Err(error) => {
                        quality_session_switch_errors += 1;
                        if error.kind() == io::ErrorKind::TimedOut {
                            quality_session_switch_timeouts += 1;
                        }
                        engine_crashes += child_crash_count(&mut child);
                        break;
                    }
                };
                let mut commit_result = None;
                for candidate_index in 0..candidates.len() {
                    let move_result = client.request_within(
                        &Request::MoveClause {
                            session,
                            offset: 0,
                            base_index: candidate_index as u32,
                            left_context: None,
                        },
                        Instant::now() + config.connect_timeout,
                    );
                    match move_result {
                        Ok(Response::ClauseView { .. }) => {
                            commit_result = Some(client.request_within(
                                &Request::CommitClauses { session },
                                Instant::now() + config.connect_timeout,
                            ));
                            break;
                        }
                        // A non-covering candidate is not a valid clause seed. Try the
                        // next cached candidate before falling back for an older baseline.
                        Ok(Response::Error { .. }) => continue,
                        Ok(other) => {
                            commit_result = Some(Ok(other));
                            break;
                        }
                        Err(error) => {
                            commit_result = Some(Err(error));
                            break;
                        }
                    }
                }
                let commit_result = commit_result.unwrap_or_else(|| {
                    // Older preserved baselines may not know clause navigation. Their
                    // cached candidate commit remains valid, but must still consume all input.
                    client.request_within(
                        &Request::Commit { session, index: 0 },
                        Instant::now() + config.connect_timeout,
                    )
                });
                match commit_result {
                    Ok(Response::Committed { reading, .. }) if reading.is_empty() => {
                        quality_session_switches_completed += 1;
                    }
                    Ok(Response::Committed { .. }) => {
                        quality_session_switch_errors += 1;
                        messages.push(format!(
                            "quality session switch {index}: commit left unread input"
                        ));
                        break;
                    }
                    Ok(other) => {
                        quality_session_switch_errors += 1;
                        messages.push(format!(
                            "quality session switch {index}: unexpected commit response: {other:?}"
                        ));
                        break;
                    }
                    Err(error) => {
                        quality_session_switch_errors += 1;
                        if error.kind() == io::ErrorKind::TimedOut {
                            quality_session_switch_timeouts += 1;
                        }
                        messages.push(format!(
                            "quality session switch {index}: commit failed: {error}"
                        ));
                        engine_crashes += child_crash_count(&mut child);
                        break;
                    }
                }
            }
            for session in [first, second] {
                if let Err(error) = client.request_within(
                    &Request::EndSession { session },
                    Instant::now() + config.connect_timeout,
                ) {
                    quality_session_switch_errors += 1;
                    if error.kind() == io::ErrorKind::TimedOut {
                        quality_session_switch_timeouts += 1;
                    }
                }
            }
        }
        errors += quality_session_switch_errors;
        timeouts += quality_session_switch_timeouts;
    }

    let quality = capture_quality.then(|| QualityReport {
        corpus_size: 200,
        corpus: (0..QUALITY_CORPUS_SIZE).map(quality_input).collect(),
        completed: quality_top1.len(),
        top1: quality_top1,
        top5: quality_top5,
        session_switches_requested: quality_session_switches_requested,
        session_switches_completed: quality_session_switches_completed,
        session_switch_errors: quality_session_switch_errors,
        session_switch_timeouts: quality_session_switch_timeouts,
    });
    engine_crashes += child_crash_count(&mut child);
    let total_cpu_seconds = engine_process_tree_cpu_seconds(process_id, &log_path);
    drop(client);
    let completed = samples.len();
    let warm_samples = samples.get(1..).unwrap_or(&[]);
    let warm_timeouts = timeouts;
    let warm_errors = errors;
    OperationReport {
        operation: operation.as_str(),
        deadline_ms: operation.deadline().as_millis() as u64,
        requested,
        completed,
        first_inference_ms,
        warm: LatencySummary::from_samples(warm_samples, warm_timeouts, warm_errors, None),
        all: LatencySummary::from_samples(&samples, timeouts, errors, total_cpu_seconds),
        cold_load,
        process_restarts,
        engine_crashes,
        evidence,
        quality,
        messages,
        log_path: log_path.display().to_string(),
    }
}

fn failed_operation_report(
    operation: Operation,
    requested: usize,
    log_path: PathBuf,
    engine_crashes: u32,
    message: String,
) -> OperationReport {
    OperationReport {
        operation: operation.as_str(),
        deadline_ms: operation.deadline().as_millis() as u64,
        requested,
        completed: 0,
        first_inference_ms: None,
        warm: LatencySummary::from_samples(&[], 0, 1, None),
        all: LatencySummary::from_samples(&[], 0, 1, None),
        cold_load: ColdLoadReport {
            spawn_to_connect_ms: None,
            service_listening_ms: None,
            warmup_ms: None,
        },
        process_restarts: 0,
        engine_crashes,
        evidence: RuntimeEvidence::default(),
        quality: None,
        messages: vec![message],
        log_path: log_path.display().to_string(),
    }
}

fn spawn_engine(config: &BenchmarkConfig, pipe: &str, log_path: &Path) -> io::Result<Child> {
    let stderr = File::create(log_path)?;
    let benchmark_memory_dir = config.log_dir.join("memory");
    let mut path_entries = vec![config.runtime_dir.clone()];
    if let Some(path) = std::env::var_os("PATH") {
        path_entries.extend(std::env::split_paths(&path));
    }
    let path = std::env::join_paths(path_entries)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut command = Command::new(&config.engine_path);
    command
        .arg(pipe)
        .env("NOSPACEKEY_ZENZAI", "on")
        .env("NOSPACEKEY_ZENZAI_WEIGHT", &config.model_path)
        .env(
            "NOSPACEKEY_ZENZAI_INFERENCE_LIMIT",
            config.inference_limit.to_string(),
        )
        .env("NOSPACEKEY_ZENZAI_BACKEND", config.backend.as_str())
        .env("NOSPACEKEY_ZENZAI_RUNTIME_DIR", &config.runtime_dir)
        .env("NOSPACEKEY_MEMORY_DIR", benchmark_memory_dir)
        .env("NOSPACEKEY_LEARNING", "0")
        .env("PATH", path)
        .env("NOSPACEKEY_LOG", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    command.spawn()
}

fn connect_to_engine(
    child: &mut ChildGuard,
    pipe: &str,
    timeout: Duration,
) -> io::Result<EngineClient> {
    let deadline = Instant::now() + timeout;
    let mut last_error = None;
    loop {
        if let Some(status) = child.child.try_wait()? {
            let detail = status
                .code()
                .map(|code| format!("exit code {code}"))
                .unwrap_or_else(|| format!("status {status:?}"));
            let suffix = last_error
                .as_ref()
                .map(|error: &io::Error| format!("; last connect error: {error}"))
                .unwrap_or_default();
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                format!("engine exited before pipe {pipe} became available ({detail}){suffix}"),
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(last_error.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "engine pipe connection timed out")
            }));
        }
        match EngineClient::connect_to(pipe, remaining.min(Duration::from_millis(100))) {
            Ok(client) => return Ok(client),
            Err(error) => {
                last_error = Some(error);
                std::thread::sleep(Duration::from_millis(5).min(remaining));
            }
        }
    }
}

fn finalize_contention_workload(mut report: ContentionWorkloadReport) -> ContentionWorkloadReport {
    report.gate_passed = report.sessions_started == report.sessions_requested
        && report.rounds_requested == report.rounds_completed
        && report.rounds_completed > 0
        && report.convert_completed + report.live_convert_completed == report.rounds_completed
        && report.candidate_moves > 0
        && report.commits == report.rounds_completed
        && report.timeouts == 0
        && report.errors == 0
        && report.engine_crashes == 0
        && report.process_restarts == 0
        && report.zenzai_fallback_events == 0;
    report
}

fn run_contention_workload(
    config: &BenchmarkConfig,
    deadline: Instant,
) -> ContentionWorkloadReport {
    let mut report = ContentionWorkloadReport {
        requested_duration_ms: config.cpu_contention_duration.as_millis() as u64,
        sessions_requested: 2,
        sessions_started: 0,
        rounds_requested: 0,
        rounds_completed: 0,
        convert_completed: 0,
        live_convert_completed: 0,
        candidate_moves: 0,
        commits: 0,
        timeouts: 0,
        errors: 0,
        engine_crashes: 0,
        process_restarts: 0,
        zenzai_fallback_events: 0,
        failure_stages: Vec::new(),
        evidence: RuntimeEvidence::default(),
        gate_passed: false,
    };
    let nonce = BENCHMARK_NONCE.fetch_add(1, Ordering::Relaxed);
    let log_path = config
        .log_dir
        .join(format!("nospacekey-zenzai-contention-{nonce}.log"));
    let pipe = format!(
        r"\\.\pipe\nospacekey-engine-contention-{}-{}.s{}",
        std::process::id(),
        nonce,
        current_session_id()
    );
    let mut child = match spawn_engine(config, &pipe, &log_path) {
        Ok(child) => ChildGuard::new(child),
        Err(_) => {
            report.errors += 1;
            report.failure_stages.push("spawn_engine");
            report.zenzai_fallback_events = count_zenzai_fallback_events(&log_path);
            return finalize_contention_workload(report);
        }
    };
    let mut client = match connect_to_engine(&mut child, &pipe, config.connect_timeout) {
        Ok(client) => client,
        Err(_) => {
            report.errors += 1;
            report.failure_stages.push("connect_engine");
            report.engine_crashes += child_crash_count(&mut child);
            report.zenzai_fallback_events = count_zenzai_fallback_events(&log_path);
            return finalize_contention_workload(report);
        }
    };
    if config.backend == Backend::Vulkan {
        match wait_for_vulkan_evidence(&log_path, Duration::from_secs(5)) {
            Ok(evidence) => report.evidence = evidence,
            Err(_) => {
                report.errors += 1;
                report.failure_stages.push("vulkan_evidence");
                report.zenzai_fallback_events = count_zenzai_fallback_events(&log_path);
                return finalize_contention_workload(report);
            }
        }
    } else if !wait_for_cpu_zenzai_evidence(&log_path, &config.model_path, config.connect_timeout) {
        report.errors += 1;
        report.failure_stages.push("cpu_evidence");
        report.zenzai_fallback_events = count_zenzai_fallback_events(&log_path);
        return finalize_contention_workload(report);
    }

    let mut sessions = Vec::with_capacity(report.sessions_requested);
    for _ in 0..report.sessions_requested {
        match client.request_within(
            &Request::StartSession,
            Instant::now() + config.connect_timeout,
        ) {
            Ok(Response::Session { session, .. }) => {
                sessions.push(session);
                report.sessions_started += 1;
            }
            Err(error) => {
                report.errors += 1;
                report.failure_stages.push("start_session");
                if error.kind() == io::ErrorKind::TimedOut {
                    report.timeouts += 1;
                }
                break;
            }
            Ok(_) => {
                report.errors += 1;
                report.failure_stages.push("start_session_response");
                break;
            }
        }
    }
    if sessions.len() != report.sessions_requested {
        report.engine_crashes += child_crash_count(&mut child);
        report.zenzai_fallback_events = count_zenzai_fallback_events(&log_path);
        return finalize_contention_workload(report);
    }

    let mut round = 0_usize;
    let shutdown_reserve = (config.cpu_contention_duration / 20).min(Duration::from_secs(12));
    while Instant::now() + shutdown_reserve < deadline {
        let session = sessions[round % sessions.len()];
        report.rounds_requested += 1;
        let input = INPUT_CORPUS[round % INPUT_CORPUS.len()];
        let insert = client.request_within(
            &Request::Insert {
                session,
                text: input.to_string(),
                style: None,
            },
            Instant::now() + config.connect_timeout,
        );
        if let Err(error) = insert {
            report.errors += 1;
            report.failure_stages.push("insert");
            if error.kind() == io::ErrorKind::TimedOut {
                report.timeouts += 1;
            }
            break;
        }

        if round % 2 == 0 {
            let candidates = match client.request_within(
                &Request::Convert {
                    session,
                    left_context: None,
                },
                Instant::now() + CONVERT_DEADLINE,
            ) {
                Ok(Response::Candidates { candidates }) if !candidates.is_empty() => candidates,
                Ok(_) => {
                    report.errors += 1;
                    report.failure_stages.push("convert_response");
                    break;
                }
                Err(error) => {
                    report.errors += 1;
                    report.failure_stages.push("convert");
                    if error.kind() == io::ErrorKind::TimedOut {
                        report.timeouts += 1;
                    }
                    break;
                }
            };
            let mut moved = false;
            for candidate_index in 0..candidates.len().min(1) {
                match client.request_within(
                    &Request::MoveClause {
                        session,
                        offset: 0,
                        base_index: candidate_index as u32,
                        left_context: None,
                    },
                    Instant::now() + config.connect_timeout,
                ) {
                    Ok(Response::ClauseView { .. }) => {
                        report.candidate_moves += 1;
                        moved = true;
                        break;
                    }
                    Ok(Response::Error { .. }) => continue,
                    Ok(_) => {
                        report.errors += 1;
                        report.failure_stages.push("move_clause_response");
                        break;
                    }
                    Err(error) => {
                        report.errors += 1;
                        report.failure_stages.push("move_clause");
                        if error.kind() == io::ErrorKind::TimedOut {
                            report.timeouts += 1;
                        }
                        break;
                    }
                }
            }
            if report.errors > 0 {
                break;
            }
            if !moved {
                match client.request_within(
                    &Request::Commit { session, index: 0 },
                    Instant::now() + config.connect_timeout,
                ) {
                    Ok(Response::Committed { reading, .. }) if reading.is_empty() => {
                        report.commits += 1;
                        report.convert_completed += 1;
                    }
                    Ok(_) => {
                        report.errors += 1;
                        report.failure_stages.push("commit_response");
                        break;
                    }
                    Err(error) => {
                        report.errors += 1;
                        report.failure_stages.push("commit");
                        if error.kind() == io::ErrorKind::TimedOut {
                            report.timeouts += 1;
                        }
                        break;
                    }
                }
                if report.errors > 0 {
                    break;
                }
                report.rounds_completed += 1;
                round += 1;
                continue;
            }
            match client.request_within(
                &Request::CommitClauses { session },
                Instant::now() + config.connect_timeout,
            ) {
                Ok(Response::Committed { reading, .. }) if reading.is_empty() => {
                    report.commits += 1;
                    report.convert_completed += 1;
                }
                Ok(_) => {
                    report.errors += 1;
                    report.failure_stages.push("commit_clauses_response");
                    break;
                }
                Err(error) => {
                    report.errors += 1;
                    report.failure_stages.push("commit_clauses");
                    if error.kind() == io::ErrorKind::TimedOut {
                        report.timeouts += 1;
                    }
                    break;
                }
            }
        } else {
            let sequence = round as u64;
            match client.request_within(
                &Request::LiveConvert {
                    session,
                    seq: sequence,
                    left_context: None,
                    auto_commit: false,
                },
                Instant::now() + LIVE_CONVERT_DEADLINE,
            ) {
                Ok(Response::LiveResult { seq, .. }) if seq == sequence => {}
                Ok(_) => {
                    report.errors += 1;
                    report.failure_stages.push("live_convert_response");
                    break;
                }
                Err(error) => {
                    report.errors += 1;
                    report.failure_stages.push("live_convert");
                    if error.kind() == io::ErrorKind::TimedOut {
                        report.timeouts += 1;
                    }
                    break;
                }
            }
            match client.request_within(
                &Request::Commit { session, index: 0 },
                Instant::now() + config.connect_timeout,
            ) {
                Ok(Response::Committed { reading, .. }) if reading.is_empty() => {
                    report.commits += 1;
                    report.live_convert_completed += 1;
                }
                Ok(_) => {
                    report.errors += 1;
                    report.failure_stages.push("live_commit_response");
                    break;
                }
                Err(error) => {
                    report.errors += 1;
                    report.failure_stages.push("live_commit");
                    if error.kind() == io::ErrorKind::TimedOut {
                        report.timeouts += 1;
                    }
                    break;
                }
            }
        }
        if report.errors > 0 {
            break;
        }
        report.rounds_completed += 1;
        round += 1;
    }
    for session in sessions {
        if let Err(error) = client.request_within(
            &Request::EndSession { session },
            Instant::now() + config.connect_timeout,
        ) {
            report.errors += 1;
            report.failure_stages.push("end_session");
            if error.kind() == io::ErrorKind::TimedOut {
                report.timeouts += 1;
            }
        }
    }
    report.engine_crashes += child_crash_count(&mut child);
    report.zenzai_fallback_events = count_zenzai_fallback_events(&log_path);
    finalize_contention_workload(report)
}

fn count_zenzai_fallback_events(path: &Path) -> u32 {
    fs::read_to_string(path)
        .map(|contents| {
            contents
                .lines()
                .filter(|line| {
                    let lower = line.to_ascii_lowercase();
                    contains_zenzai_fallback(&lower)
                })
                .count() as u32
        })
        .unwrap_or(0)
}

fn contains_zenzai_fallback(contents: &str) -> bool {
    contents.contains("ev=zenzai_classic")
        || contents.contains("ev=zenzai_disabled")
        || contents.contains("ev=zenzai_worker_fallback")
}

fn benchmark_from_env() -> Result<BenchmarkReport, String> {
    let config = BenchmarkConfig::from_env()?;
    let contention_monitor_deadline = Instant::now() + config.cpu_contention_duration;
    let cpu_monitor = if config.cpu_contention_percent > 0 {
        Some(CpuUtilizationMonitor::start(
            config.cpu_contention_percent,
            config.cpu_contention_duration,
        )?)
    } else {
        None
    };
    let _contention = CpuContentionGuard::start(config.cpu_contention_percent);
    let operations = vec![
        run_operation(&config, Operation::Convert),
        run_operation(&config, Operation::LiveConvert),
    ];
    let has_errors = operations.iter().any(|operation| operation.all.errors > 0);
    let engine_crashes = operations
        .iter()
        .map(|operation| operation.engine_crashes)
        .sum();
    let timeouts = operations
        .iter()
        .map(|operation| operation.all.timeouts)
        .sum();
    let errors = operations
        .iter()
        .map(|operation| operation.all.errors)
        .sum();
    let evidence = operations
        .iter()
        .find(|operation| operation.evidence.gpu_active)
        .map(|operation| operation.evidence.clone())
        .unwrap_or_default();
    let quality = operations
        .iter()
        .find_map(|operation| operation.quality.clone());
    let quality_failed = quality.as_ref().is_some_and(|value| {
        value.completed != value.corpus_size
            || value.corpus_size != 200
            || value.session_switches_completed != 100
            || value.session_switch_errors != 0
            || value.session_switch_timeouts != 0
    });
    let contention_workload = if config.cpu_contention_percent > 0 {
        Some(run_contention_workload(
            &config,
            contention_monitor_deadline,
        ))
    } else {
        None
    };
    let cpu_utilization =
        cpu_monitor.map(|monitor| monitor.finish_until(contention_monitor_deadline));
    let contention_failed = config.cpu_contention_percent > 0
        && (match cpu_utilization.as_ref() {
            Some(report) => !report.gate_passed,
            None => true,
        } || match contention_workload.as_ref() {
            Some(report) => !report.gate_passed,
            None => true,
        });
    Ok(BenchmarkReport {
        schema_version: 1,
        status: if has_errors || quality_failed || contention_failed {
            "error"
        } else {
            "ok"
        },
        backend: config.backend.as_str(),
        model_path: config.model_path.display().to_string(),
        engine_path: config.engine_path.display().to_string(),
        runtime_dir: config.runtime_dir.display().to_string(),
        source_engine_path: config.source_engine_path.display().to_string(),
        source_runtime_dir: config.source_runtime_dir.display().to_string(),
        inference_limit: config.inference_limit,
        corpus: INPUT_CORPUS.iter().map(|s| (*s).to_string()).collect(),
        deadlines_ms: Deadlines {
            convert: CONVERT_DEADLINE.as_millis() as u64,
            live_convert: LIVE_CONVERT_DEADLINE.as_millis() as u64,
        },
        requested: RequestedCounts {
            convert: config.convert_requests,
            live_convert: config.live_convert_requests,
        },
        cpu_contention_percent: config.cpu_contention_percent,
        cpu_contention_duration_ms: config.cpu_contention_duration.as_millis() as u64,
        cpu_utilization,
        contention_workload,
        engine_crashes,
        timeouts,
        errors,
        evidence,
        quality,
        operations,
        fatal_error: None,
    })
}

fn write_report(report: &BenchmarkReport) {
    let json = serde_json::to_string_pretty(report).expect("serialize benchmark report");
    if let Some(path) = std::env::var_os("NOSPACEKEY_BENCH_JSON").map(PathBuf::from) {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).expect("create benchmark report directory");
        }
        fs::write(&path, format!("{json}\n")).expect("write benchmark report");
        println!("ev=bench_json path={}", path.display());
    }
    println!("{json}");
}

#[test]
#[ignore]
fn zenzai_runtime_benchmark() {
    match benchmark_from_env() {
        Ok(report) => {
            let failed = report.status != "ok";
            write_report(&report);
            assert!(!failed, "benchmark gate failed; inspect JSON report");
        }
        Err(error) => {
            eprintln!("zenzai benchmark setup failed: {error}");
            panic!("zenzai benchmark setup failed: {error}");
        }
    }
}

fn required_path_env(name: &str, label: &str) -> Result<PathBuf, String> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| {
            format!("{label} path is required in {name}; classic mode is not a baseline")
        })
}

fn env_usize(name: &str, default: usize) -> Result<usize, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|_| format!("{name} must be a non-negative integer, got {value:?}")),
        Err(_) => Ok(default),
    }
}

fn env_u32(name: &str, default: u32) -> Result<u32, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u32>()
            .map_err(|_| format!("{name} must be an unsigned integer, got {value:?}")),
        Err(_) => Ok(default),
    }
}

fn env_u8(name: &str, default: u8) -> Result<u8, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<u8>()
            .map_err(|_| format!("{name} must be an integer 0..=255, got {value:?}")),
        Err(_) => Ok(default),
    }
}

fn env_duration_secs(name: &str, default: Duration) -> Result<Duration, String> {
    match std::env::var(name) {
        Ok(value) => value.parse::<u64>().map(Duration::from_secs).map_err(|_| {
            format!("{name} must be a non-negative integer seconds value, got {value:?}")
        }),
        Err(_) => Ok(default),
    }
}

fn default_release_engine_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join(r"engine-host\.build\x86_64-unknown-windows-msvc\release\NospacekeyEngineHost.exe")
}

fn default_runtime_dir(backend: Backend) -> PathBuf {
    let mut path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join(r"engine-host\vendor\llama");
    if backend == Backend::Vulkan {
        path.push("vulkan");
    }
    path
}

fn validate_runtime_dir(path: &Path, backend: Backend) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!(
            "benchmark runtime directory must be absolute: {}",
            path.display()
        ));
    }
    if !path.is_dir() {
        return Err(format!(
            "benchmark runtime directory does not exist: {}",
            path.display()
        ));
    }
    let mut required = vec!["llama.dll", "ggml.dll", "ggml-base.dll", "ggml-cpu.dll"];
    if backend == Backend::Vulkan {
        required.push("ggml-vulkan.dll");
    }
    let missing: Vec<_> = required
        .into_iter()
        .filter(|name| !path.join(name).is_file())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "benchmark runtime directory {} is missing required DLLs: {}",
            path.display(),
            missing.join(", ")
        ))
    }
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}

fn wait_for_log_value(path: &Path, marker: &str, key: &str, timeout: Duration) -> Option<f64> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = fs::read_to_string(path) {
            for line in contents.lines() {
                if line.contains(marker) {
                    if let Some(value) = line
                        .split(key)
                        .nth(1)
                        .and_then(|part| part.split_whitespace().next())
                        .and_then(|part| part.trim_end_matches("ms").parse::<f64>().ok())
                    {
                        return Some(value);
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_cpu_zenzai_evidence(path: &Path, model_path: &Path, timeout: Duration) -> bool {
    let expected_model = model_path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase);
    let Some(expected_model) = expected_model else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = fs::read_to_string(path) {
            let lower = contents.to_ascii_lowercase();
            if contains_zenzai_fallback(&lower)
                || lower.lines().any(|line| {
                    line.contains("starting pipe server") && line.contains("zenzai=false")
                })
            {
                return false;
            }
            let zenzai_enabled = lower
                .lines()
                .any(|line| line.contains("starting pipe server") && line.contains("zenzai=true"));
            if !zenzai_enabled {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            let named_loaded = lower
                .lines()
                .any(|line| line.contains("loaded model") && line.contains(&expected_model));
            let loader_loaded = lower.lines().any(|line| {
                let Some((prefix, suffix)) = line.split_once(" from ") else {
                    return false;
                };
                if !prefix.contains("llama_model_loader")
                    || !prefix.contains("loaded")
                    || !suffix.contains(&expected_model)
                {
                    return false;
                }
                suffix
                    .split_whitespace()
                    .next()
                    .and_then(|path| Path::new(path).file_name())
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case(&expected_model))
            });
            let warmup_marker = lower.contains("ev=coldstart stage=warmup");
            if named_loaded || (loader_loaded && warmup_marker) {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn parse_vulkan_evidence(contents: &str) -> Result<RuntimeEvidence, String> {
    let lower = contents.to_ascii_lowercase();
    if contains_zenzai_fallback(&lower) {
        return Err(
            "Vulkan engine entered classic/disabled state; refusing CPU fallback measurement"
                .into(),
        );
    }
    let active_line = contents
        .lines()
        .find(|line| line.to_ascii_lowercase().contains("ev=zenzai_gpu_active"))
        .ok_or_else(|| "Vulkan gpu_active status was not observed".to_string())?;
    let device = active_line
        .split_once("device=")
        .map(|(_, value)| {
            value
                .split(" decode_attempts=")
                .next()
                .unwrap_or(value)
                .trim()
                .to_string()
        })
        .filter(|value| !value.is_empty());
    let Some(device) = device else {
        return Err("Vulkan gpu_active device identity was empty".into());
    };
    let device_lower = device.to_ascii_lowercase();
    let has_radeon_890m = device_lower.contains("radeon 890m")
        || device_lower.contains("radeon(tm) 890m")
        || (device_lower.contains("radeon") && device_lower.contains("890m"));
    if !has_radeon_890m {
        return Err("Vulkan loader did not identify Radeon 890M".into());
    }
    let decode_attempts = active_line
        .split_once("decode_attempts=")
        .and_then(|(_, value)| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok());
    if decode_attempts.unwrap_or(0) == 0 {
        return Err("Vulkan gpu_active status did not prove a decode attempt".into());
    }
    let worker_ready = contents
        .lines()
        .find(|line| {
            let line = line.to_ascii_lowercase();
            line.contains("ev=zenzai_worker_ready") && line.contains("backend=vulkan")
        })
        .ok_or_else(|| "Vulkan worker readiness was not observed".to_string())?;
    let ready_device = worker_ready
        .split_once("device=")
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Vulkan worker ready device identity was empty".to_string())?;
    if !ready_device.eq_ignore_ascii_case(&device) {
        return Err("Vulkan worker ready device did not match active device".into());
    }
    if !lower.contains("ev=zenzai_probe backend=vulkan") {
        return Err("Vulkan probe backend evidence was not observed".into());
    }
    if !lower.contains("offloaded 13/13 layers to gpu") {
        return Err("Vulkan loader did not prove offloaded 13/13 layers to GPU".into());
    }
    Ok(RuntimeEvidence {
        gpu_active: true,
        decode_verified: true,
        decode_attempts,
        offloaded_13_of_13: true,
        device: Some(device),
        backend: Some("Vulkan".into()),
    })
}

fn wait_for_vulkan_evidence(path: &Path, timeout: Duration) -> Result<RuntimeEvidence, String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = fs::read_to_string(path) {
            match parse_vulkan_evidence(&contents) {
                Ok(value) => return Ok(value),
                Err(message) if contains_zenzai_fallback(&contents.to_ascii_lowercase()) => {
                    return Err(message)
                }
                Err(_) => {}
            }
        }
        if Instant::now() >= deadline {
            return Err("Vulkan GPU evidence timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_vulkan_decode_evidence(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = fs::read_to_string(path) {
            let lower = contents.to_ascii_lowercase();
            if contains_zenzai_fallback(&lower) {
                return false;
            }
            if lower.contains("ev=infer kind=convert")
                || lower.contains("ev=infer kind=live_convert")
            {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn child_crash_count(child: &mut ChildGuard) -> u32 {
    match child.child.try_wait() {
        Ok(Some(status)) if !status.success() => 1,
        _ => 0,
    }
}

#[cfg(windows)]
fn child_process_cpu_seconds(process_id: u32) -> Option<f64> {
    use std::mem::MaybeUninit;
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()? };
    let mut creation = MaybeUninit::<FILETIME>::zeroed();
    let mut exit = MaybeUninit::<FILETIME>::zeroed();
    let mut kernel = MaybeUninit::<FILETIME>::zeroed();
    let mut user = MaybeUninit::<FILETIME>::zeroed();
    let result = unsafe {
        GetProcessTimes(
            handle,
            creation.as_mut_ptr(),
            exit.as_mut_ptr(),
            kernel.as_mut_ptr(),
            user.as_mut_ptr(),
        )
    };
    let seconds = result.ok().map(|_| {
        let kernel = unsafe { kernel.assume_init() };
        let user = unsafe { user.assume_init() };
        let kernel_ticks =
            ((u64::from(kernel.dwHighDateTime)) << 32) | u64::from(kernel.dwLowDateTime);
        let user_ticks = ((u64::from(user.dwHighDateTime)) << 32) | u64::from(user.dwLowDateTime);
        (kernel_ticks + user_ticks) as f64 / 10_000_000.0
    });
    unsafe {
        let _ = CloseHandle(handle);
    }
    seconds
}

#[cfg(not(windows))]
fn child_process_cpu_seconds(_process_id: u32) -> Option<f64> {
    None
}

fn worker_process_ids_from_log(path: &Path) -> Vec<u32> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut ids: Vec<u32> = contents
        .lines()
        .filter_map(|line| {
            line.strip_prefix("ev=zenzai_worker_spawn pid=")?
                .trim()
                .parse()
                .ok()
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn engine_process_tree_cpu_seconds(engine_process_id: u32, log_path: &Path) -> Option<f64> {
    let mut process_ids = worker_process_ids_from_log(log_path);
    process_ids.push(engine_process_id);
    process_ids.sort_unstable();
    process_ids.dedup();
    let samples: Vec<f64> = process_ids
        .into_iter()
        .filter_map(child_process_cpu_seconds)
        .collect();
    (!samples.is_empty()).then(|| samples.into_iter().sum())
}

static BENCHMARK_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::{
        count_zenzai_fallback_events, cpu_utilization_percent, parse_vulkan_evidence, percentile,
        quality_input, wait_for_cpu_zenzai_evidence, worker_process_ids_from_log,
        CpuUtilizationReport, LatencySummary, SystemCpuTimes, QUALITY_CORPUS_SIZE,
        QUALITY_PREFIXES, QUALITY_SUFFIXES,
    };
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn percentile_uses_nearest_rank_on_sorted_samples() {
        let samples = [10.0, 20.0, 30.0, 40.0];
        assert_eq!(percentile(&samples, 0.50), Some(30.0));
        assert_eq!(percentile(&samples, 0.95), Some(40.0));
        assert_eq!(percentile(&[], 0.50), None);
    }

    #[test]
    fn summary_excludes_no_samples_without_fabricated_zeroes() {
        let summary = LatencySummary::from_samples(&[], 2, 3, Some(1.5));
        assert_eq!(summary.sample_count, 0);
        assert_eq!(summary.p50_ms, None);
        assert_eq!(summary.max_ms, None);
        assert_eq!(summary.engine_cpu_seconds_per_completed_request, None);
        assert_eq!(summary.timeouts, 2);
        assert_eq!(summary.errors, 3);
    }

    #[test]
    fn vulkan_evidence_requires_active_decode_device_and_full_offload() {
        let log = "ev=zenzai_probe backend=Vulkan\nusing device Vulkan0 (AMD Radeon 890M)\nev=zenzai_gpu_active device=AMD Radeon(TM) 890M Graphics decode_attempts=1\noffloaded 13/13 layers to GPU\nev=zenzai_worker_ready backend=Vulkan device=AMD Radeon(TM) 890M Graphics\n";
        let evidence = parse_vulkan_evidence(log).expect("complete GPU evidence");
        assert!(evidence.gpu_active);
        assert!(evidence.decode_verified);
        assert_eq!(evidence.decode_attempts, Some(1));
        assert_eq!(
            evidence.device.as_deref(),
            Some("AMD Radeon(TM) 890M Graphics")
        );
    }

    #[test]
    fn vulkan_evidence_rejects_missing_radeon_or_decode_counter() {
        let no_radeon = "ev=zenzai_probe backend=Vulkan\nev=zenzai_gpu_active device=Vulkan0 decode_attempts=1\noffloaded 13/13 layers to GPU\n";
        assert!(parse_vulkan_evidence(no_radeon).is_err());
        let unselected_radeon = "ev=zenzai_probe backend=Vulkan\nusing device Vulkan0 (AMD Radeon 890M)\nev=zenzai_gpu_active device=Other GPU decode_attempts=1\noffloaded 13/13 layers to GPU\n";
        assert!(parse_vulkan_evidence(unselected_radeon).is_err());
        let no_decode = "ev=zenzai_probe backend=Vulkan\nusing device Vulkan0 (AMD Radeon 890M)\nev=zenzai_gpu_active device=AMD Radeon(TM) 890M Graphics decode_attempts=0\noffloaded 13/13 layers to GPU\n";
        assert!(parse_vulkan_evidence(no_decode).is_err());
        let no_worker_ready = "ev=zenzai_probe backend=Vulkan\nev=zenzai_gpu_active device=AMD Radeon(TM) 890M Graphics decode_attempts=1\noffloaded 13/13 layers to GPU\n";
        assert!(parse_vulkan_evidence(no_worker_ready).is_err());
    }

    #[test]
    fn quality_corpus_has_200_distinct_complete_inputs() {
        let corpus: Vec<_> = (0..QUALITY_CORPUS_SIZE).map(quality_input).collect();
        let distinct: std::collections::HashSet<_> = corpus.iter().cloned().collect();
        let expected: std::collections::HashSet<_> = QUALITY_PREFIXES
            .iter()
            .flat_map(|prefix| {
                QUALITY_SUFFIXES
                    .iter()
                    .map(move |suffix| format!("{prefix}{suffix}"))
            })
            .collect();

        assert_eq!(QUALITY_CORPUS_SIZE, 200);
        assert_eq!(corpus.len(), QUALITY_CORPUS_SIZE);
        assert_eq!(distinct.len(), QUALITY_CORPUS_SIZE);
        assert_eq!(distinct, expected);
        assert!(QUALITY_PREFIXES.iter().all(|prefix| prefix.ends_with("ha")));
        assert!(QUALITY_SUFFIXES
            .iter()
            .all(|suffix| suffix.ends_with("masu")));
        assert_eq!(quality_input(0), "watashihagakkouheikimasu");
        assert_eq!(quality_input(199), "kyoushihabenkyoushimasu");
    }

    #[test]
    fn system_cpu_utilization_is_derived_from_idle_delta() {
        let previous = SystemCpuTimes {
            idle: 100,
            kernel: 500,
            user: 400,
        };
        let current = SystemCpuTimes {
            idle: 110,
            kernel: 600,
            user: 500,
        };
        assert_eq!(cpu_utilization_percent(previous, current), Some(95.0));
    }

    #[test]
    fn contention_cpu_gate_requires_measured_utilization_and_duration() {
        let report = CpuUtilizationReport::from_samples(
            90,
            Duration::from_secs(600),
            Duration::from_millis(1_000),
            vec![88.0, 89.0, 90.0],
        );
        assert!(!report.gate_passed);
        assert_eq!(report.minimum_percent, Some(88.0));
        assert_eq!(report.samples_at_or_above_target, 1);
    }

    #[test]
    fn contention_cpu_gate_rejects_an_average_propped_up_by_spikes() {
        let report = CpuUtilizationReport::from_samples(
            90,
            Duration::from_secs(4),
            Duration::from_secs(1),
            vec![100.0, 100.0, 85.0, 85.0],
        );
        assert_eq!(report.average_percent, Some(92.5));
        assert_eq!(report.sufficient_samples_ratio, 0.5);
        assert!(!report.gate_passed);
    }

    #[test]
    fn cpu_evidence_requires_requested_model_and_warmup() {
        let path = std::env::temp_dir().join(format!(
            "nospacekey-latency-cpu-evidence-{}.log",
            std::process::id()
        ));
        fs::write(
            &path,
            "nospacekey-engine starting pipe server (zenzai=true)\nllama_model_loader: loaded meta data from C:/models/ggml-model.gguf\nev=coldstart stage=warmup ms=12\n",
        )
        .expect("write evidence log");
        assert!(wait_for_cpu_zenzai_evidence(
            &path,
            Path::new("C:/models/ggml-model.gguf"),
            Duration::ZERO,
        ));
        assert!(!wait_for_cpu_zenzai_evidence(
            &path,
            Path::new("C:/models/other.gguf"),
            Duration::ZERO,
        ));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn worker_cpu_accounting_uses_only_typed_spawn_records() {
        let path = std::env::temp_dir().join(format!(
            "nospacekey-latency-worker-pids-{}.log",
            std::process::id()
        ));
        fs::write(
            &path,
            "ev=zenzai_worker_spawn pid=42\ninput pid=99\nev=zenzai_worker_spawn pid=7\nev=zenzai_worker_spawn pid=42\n",
        )
        .expect("write worker PID log");
        assert_eq!(worker_process_ids_from_log(&path), vec![7, 42]);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn worker_fallback_is_counted_as_zenzai_fallback() {
        let path = std::env::temp_dir().join(format!(
            "nospacekey-latency-worker-fallback-{}.log",
            std::process::id()
        ));
        fs::write(
            &path,
            "ev=zenzai_worker_fallback reason=timeout\nev=infer kind=convert ms=1\n",
        )
        .expect("write worker fallback log");
        assert_eq!(count_zenzai_fallback_events(&path), 1);
        assert!(parse_vulkan_evidence(
            &fs::read_to_string(&path).expect("read worker fallback log")
        )
        .is_err());
        let _ = fs::remove_file(path);
    }
}
