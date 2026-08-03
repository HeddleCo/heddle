// SPDX-License-Identifier: Apache-2.0

use std::{
    env,
    path::Path,
    time::{Duration, Instant},
};

use serde_json::Value;

use super::fixture::{PerfFixture, base_command};
use super::profile::sample_from_trace;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NegativeControl {
    None,
    Latency,
    FullScan,
    DuplicateOpen,
}

impl NegativeControl {
    pub(super) fn from_env() -> Self {
        match env::var("HEDDLE_PERF_NEGATIVE_CONTROL").as_deref() {
            Ok("latency") => Self::Latency,
            Ok("full-scan") => Self::FullScan,
            Ok("duplicate-open") => Self::DuplicateOpen,
            Ok(value) => panic!("unknown HEDDLE_PERF_NEGATIVE_CONTROL `{value}`"),
            Err(_) => Self::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CaseKind {
    Version,
    Help,
    StatusClean,
    StatusDirty,
    CaptureOne,
}

impl CaseKind {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Help => "help",
            Self::StatusClean => "status_clean",
            Self::StatusDirty => "status_one_dirty",
            Self::CaptureOne => "capture_one",
        }
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Version => &["--version"],
            Self::Help => &["help"],
            Self::StatusClean | Self::StatusDirty => &["--output", "json", "status"],
            Self::CaptureOne => &["--output", "json", "capture", "-m", "perf sample"],
        }
    }

    pub(super) fn expected_repo_opens(self) -> u64 {
        match self {
            Self::Version | Self::Help => 0,
            Self::StatusClean | Self::StatusDirty => 1,
            Self::CaptureOne => 2,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Sample {
    pub wall_ms: f64,
    pub profile_total_ms: f64,
    pub startup_ms: f64,
    pub warm_repository_ms: f64,
    pub monitor_ms: f64,
    pub rendering_ms: f64,
    pub network_ms: f64,
    pub counters: Counters,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Counters {
    pub directories_scanned: u64,
    pub directories_skipped: u64,
    pub files_hashed: u64,
    pub monitor_changed_paths: u64,
    pub object_decodes: u64,
    pub ref_reads: u64,
    pub oplog_reads: u64,
    pub repository_opens: u64,
    pub network_client_initialized: bool,
}

impl Sample {
    fn combine(mut self, other: Self) -> Self {
        self.wall_ms += other.wall_ms;
        self.profile_total_ms += other.profile_total_ms;
        self.startup_ms += other.startup_ms;
        self.warm_repository_ms += other.warm_repository_ms;
        self.monitor_ms += other.monitor_ms;
        self.rendering_ms += other.rendering_ms;
        self.network_ms += other.network_ms;
        self.counters.add(&other.counters);
        self
    }
}

impl Counters {
    fn add(&mut self, other: &Self) {
        self.directories_scanned += other.directories_scanned;
        self.directories_skipped += other.directories_skipped;
        self.files_hashed += other.files_hashed;
        self.monitor_changed_paths += other.monitor_changed_paths;
        self.object_decodes += other.object_decodes;
        self.ref_reads += other.ref_reads;
        self.oplog_reads += other.oplog_reads;
        self.repository_opens += other.repository_opens;
        self.network_client_initialized |= other.network_client_initialized;
    }
}

pub(super) struct CaseResult {
    pub kind: CaseKind,
    pub path_count: usize,
    pub samples: Vec<Sample>,
}

impl CaseResult {
    pub(super) fn metric(&self, select: impl Fn(&Sample) -> f64) -> Stats {
        Stats::from_values(self.samples.iter().map(select).collect())
    }

    pub(super) fn counter(&self, select: impl Fn(&Counters) -> u64) -> Stats {
        Stats::from_values(
            self.samples
                .iter()
                .map(|sample| select(&sample.counters) as f64)
                .collect(),
        )
    }

    pub(super) fn print(&self) {
        let wall = self.metric(|sample| sample.wall_ms);
        println!(
            "RESULT case={} mode={} paths={} samples={} wall_ms={} total_ms={} startup_ms={} warm_repo_ms={} monitor_ms={} render_ms={} network_ms={}",
            self.kind.name(),
            if self.path_count == 0 { "cold_process" } else { "warm_repo" },
            self.path_count,
            self.samples.len(),
            wall,
            self.metric(|sample| sample.profile_total_ms),
            self.metric(|sample| sample.startup_ms),
            self.metric(|sample| sample.warm_repository_ms),
            self.metric(|sample| sample.monitor_ms),
            self.metric(|sample| sample.rendering_ms),
            self.metric(|sample| sample.network_ms),
        );
        println!(
            "COUNTERS case={} paths={} dirs_scanned={} dirs_skipped={} files_hashed={} monitor_paths={} object_decodes={} ref_reads={} oplog_reads={} repo_opens={} network_initialized={}",
            self.kind.name(),
            self.path_count,
            self.counter(|value| value.directories_scanned),
            self.counter(|value| value.directories_skipped),
            self.counter(|value| value.files_hashed),
            self.counter(|value| value.monitor_changed_paths),
            self.counter(|value| value.object_decodes),
            self.counter(|value| value.ref_reads),
            self.counter(|value| value.oplog_reads),
            self.counter(|value| value.repository_opens),
            self.samples.iter().any(|sample| sample.counters.network_client_initialized),
        );
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Stats {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

impl Stats {
    fn from_values(mut values: Vec<f64>) -> Self {
        values.sort_by(f64::total_cmp);
        Self {
            p50: percentile(&values, 0.50),
            p95: percentile(&values, 0.95),
            p99: percentile(&values, 0.99),
            max: *values.last().expect("performance case has samples"),
        }
    }
}

impl std::fmt::Display for Stats {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "p50={:.3},p95={:.3},p99={:.3},max={:.3}",
            self.p50, self.p95, self.p99, self.max
        )
    }
}

pub(super) fn measure_case(
    binary: &Path,
    mut fixture: Option<&mut PerfFixture>,
    kind: CaseKind,
    sample_count: usize,
    negative: NegativeControl,
) -> CaseResult {
    let path_count = fixture.as_ref().map_or(0, |value| value.path_count);
    let cwd = fixture
        .as_ref()
        .map_or_else(std::env::temp_dir, |value| value.root().to_path_buf());
    let mut samples = Vec::with_capacity(sample_count);
    for index in 0..sample_count {
        if let Some(value) = fixture.as_deref_mut() {
            if kind == CaseKind::CaptureOne {
                value.prepare_capture_sample();
            }
            if negative == NegativeControl::FullScan {
                value.prepare_full_scan_sample();
            }
        }
        let mut sample = run_profiled(binary, kind.args(), &cwd, negative);
        if negative == NegativeControl::DuplicateOpen {
            sample = sample.combine(run_profiled(binary, kind.args(), &cwd, negative));
        }
        assert!(sample.wall_ms.is_finite(), "sample {index} should be finite");
        samples.push(sample);
    }
    CaseResult {
        kind,
        path_count,
        samples,
    }
}

fn run_profiled(binary: &Path, args: &[&str], cwd: &Path, negative: NegativeControl) -> Sample {
    let start = Instant::now();
    if negative == NegativeControl::Latency {
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = base_command(binary, cwd)
        .env("HEDDLE_PROFILE", "jsonl")
        .args(args)
        .output()
        .expect("run profiled command");
    let wall_ms = start.elapsed().as_secs_f64() * 1_000.0;
    assert!(
        output.status.success(),
        "timed {args:?} failed; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).expect("profile stderr is utf8");
    let trace: Value = stderr
        .lines()
        .find_map(|line| serde_json::from_str(line).ok())
        .unwrap_or_else(|| panic!("profile trace missing from stderr: {stderr}"));
    sample_from_trace(wall_ms, &trace)
}

fn percentile(values: &[f64], quantile: f64) -> f64 {
    let index = ((values.len() as f64 * quantile).ceil() as usize)
        .saturating_sub(1)
        .min(values.len() - 1);
    values[index]
}
