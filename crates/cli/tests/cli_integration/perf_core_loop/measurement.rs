// SPDX-License-Identifier: Apache-2.0

use std::{
    env,
    path::Path,
    time::{Duration, Instant},
};

use serde_json::Value;

use super::{
    fixture::{PerfFixture, base_command},
    profile::sample_from_trace,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NegativeControl {
    None,
    Latency,
    FullScan,
    SubtreeSkip,
    EagerPackIndex,
    DuplicateOpen,
}

impl NegativeControl {
    pub(super) fn from_env() -> Self {
        match env::var("HEDDLE_PERF_NEGATIVE_CONTROL").as_deref() {
            Ok("latency") => Self::Latency,
            Ok("full-scan") => Self::FullScan,
            Ok("subtree-skip") => Self::SubtreeSkip,
            Ok("eager-pack-index") => Self::EagerPackIndex,
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
    DiffOne,
    LogBounded,
    DiffOneRepacked,
    LogBoundedRepacked,
    ThreadListBounded,
}

impl CaseKind {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Help => "help",
            Self::StatusClean => "status_clean",
            Self::StatusDirty => "status_one_dirty",
            Self::CaptureOne => "capture_one",
            Self::DiffOne => "diff_one",
            Self::LogBounded => "log_bounded",
            Self::DiffOneRepacked => "diff_one_repacked",
            Self::LogBoundedRepacked => "log_bounded_repacked",
            Self::ThreadListBounded => "thread_list_bounded",
        }
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Version => &["--version"],
            Self::Help => &["help"],
            Self::StatusClean | Self::StatusDirty => &["--output", "json", "status"],
            Self::CaptureOne => &["--output", "json", "capture", "-m", "perf sample"],
            Self::DiffOne | Self::DiffOneRepacked => &["--output", "json", "diff"],
            Self::LogBounded | Self::LogBoundedRepacked => {
                &["--output", "json", "log", "--limit", "20"]
            }
            Self::ThreadListBounded => &["--output", "json", "thread", "list"],
        }
    }

    pub(super) fn expected_repo_opens(self) -> u64 {
        match self {
            Self::Version | Self::Help => 0,
            Self::StatusClean | Self::StatusDirty => 1,
            Self::CaptureOne => 2,
            Self::DiffOne
            | Self::LogBounded
            | Self::DiffOneRepacked
            | Self::LogBoundedRepacked
            | Self::ThreadListBounded => 1,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Sample {
    pub wall_ms: f64,
    pub profile_total_ms: f64,
    pub startup_ms: f64,
    pub warm_repository_ms: f64,
    pub repository_open_ms: f64,
    pub current_state_ms: f64,
    pub verification_ms: f64,
    pub worktree_status_ms: f64,
    pub worktree_index_load_ms: f64,
    pub worktree_compare_ms: f64,
    pub worktree_index_save_ms: f64,
    pub thread_summary_ms: f64,
    pub snapshot_ms: f64,
    pub snapshot_tree_walk_ms: f64,
    pub snapshot_blob_prep_ms: f64,
    pub snapshot_blob_write_ms: f64,
    pub snapshot_tree_write_ms: f64,
    pub snapshot_state_ref_oplog_ms: f64,
    pub snapshot_atomic_execute_ms: f64,
    pub snapshot_ref_publish_ms: f64,
    pub snapshot_state_create_ms: f64,
    pub snapshot_captured_path_count_ms: f64,
    pub snapshot_post_verification_ms: f64,
    pub snapshot_thread_metadata_ms: f64,
    pub snapshot_preflight_ms: f64,
    pub snapshot_attribution_ms: f64,
    pub snapshot_execute_save_ms: f64,
    pub snapshot_previous_state_ms: f64,
    pub snapshot_previous_state_head_ms: f64,
    pub snapshot_previous_state_cache_read_ms: f64,
    pub snapshot_previous_state_cache_decode_ms: f64,
    pub snapshot_previous_state_cache_validate_ms: f64,
    pub snapshot_previous_state_store_read_ms: f64,
    pub snapshot_signature_lookup_ms: f64,
    pub snapshot_output_build_ms: f64,
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
    pub ancestors_visited: u64,
    pub history_objects_decoded: u64,
    pub pack_frame_decompressions: u64,
    pub pack_frame_cache_hits: u64,
    pub pack_blob_bodies_hashed: u64,
    pub pack_state_frames_decoded: u64,
}

impl Sample {
    fn combine(mut self, other: Self) -> Self {
        self.wall_ms += other.wall_ms;
        self.profile_total_ms += other.profile_total_ms;
        self.startup_ms += other.startup_ms;
        self.warm_repository_ms += other.warm_repository_ms;
        self.repository_open_ms += other.repository_open_ms;
        self.current_state_ms += other.current_state_ms;
        self.verification_ms += other.verification_ms;
        self.worktree_status_ms += other.worktree_status_ms;
        self.worktree_index_load_ms += other.worktree_index_load_ms;
        self.worktree_compare_ms += other.worktree_compare_ms;
        self.worktree_index_save_ms += other.worktree_index_save_ms;
        self.thread_summary_ms += other.thread_summary_ms;
        self.snapshot_ms += other.snapshot_ms;
        self.snapshot_tree_walk_ms += other.snapshot_tree_walk_ms;
        self.snapshot_blob_prep_ms += other.snapshot_blob_prep_ms;
        self.snapshot_blob_write_ms += other.snapshot_blob_write_ms;
        self.snapshot_tree_write_ms += other.snapshot_tree_write_ms;
        self.snapshot_state_ref_oplog_ms += other.snapshot_state_ref_oplog_ms;
        self.snapshot_atomic_execute_ms += other.snapshot_atomic_execute_ms;
        self.snapshot_ref_publish_ms += other.snapshot_ref_publish_ms;
        self.snapshot_state_create_ms += other.snapshot_state_create_ms;
        self.snapshot_captured_path_count_ms += other.snapshot_captured_path_count_ms;
        self.snapshot_post_verification_ms += other.snapshot_post_verification_ms;
        self.snapshot_thread_metadata_ms += other.snapshot_thread_metadata_ms;
        self.snapshot_preflight_ms += other.snapshot_preflight_ms;
        self.snapshot_attribution_ms += other.snapshot_attribution_ms;
        self.snapshot_execute_save_ms += other.snapshot_execute_save_ms;
        self.snapshot_previous_state_ms += other.snapshot_previous_state_ms;
        self.snapshot_previous_state_head_ms += other.snapshot_previous_state_head_ms;
        self.snapshot_previous_state_cache_read_ms += other.snapshot_previous_state_cache_read_ms;
        self.snapshot_previous_state_cache_decode_ms +=
            other.snapshot_previous_state_cache_decode_ms;
        self.snapshot_previous_state_cache_validate_ms +=
            other.snapshot_previous_state_cache_validate_ms;
        self.snapshot_previous_state_store_read_ms += other.snapshot_previous_state_store_read_ms;
        self.snapshot_signature_lookup_ms += other.snapshot_signature_lookup_ms;
        self.snapshot_output_build_ms += other.snapshot_output_build_ms;
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
        self.ancestors_visited += other.ancestors_visited;
        self.history_objects_decoded += other.history_objects_decoded;
        self.pack_frame_decompressions += other.pack_frame_decompressions;
        self.pack_frame_cache_hits += other.pack_frame_cache_hits;
        self.pack_blob_bodies_hashed += other.pack_blob_bodies_hashed;
        self.pack_state_frames_decoded += other.pack_state_frames_decoded;
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
            "RESULT case={} mode={} paths={} samples={} wall_ms={} total_ms={} startup_ms={} warm_repo_ms={} repo_open_ms={} current_state_ms={} verification_ms={} worktree_status_ms={} worktree_index_load_ms={} worktree_compare_ms={} worktree_index_save_ms={} thread_summary_ms={} snapshot_ms={} snapshot_tree_walk_ms={} snapshot_blob_prep_ms={} snapshot_blob_write_ms={} snapshot_tree_write_ms={} snapshot_state_ref_oplog_ms={} snapshot_atomic_execute_ms={} snapshot_ref_publish_ms={} snapshot_state_create_ms={} snapshot_captured_path_count_ms={} snapshot_post_verification_ms={} snapshot_thread_metadata_ms={} snapshot_preflight_ms={} snapshot_attribution_ms={} snapshot_execute_save_ms={} snapshot_previous_state_ms={} snapshot_previous_state_head_ms={} snapshot_previous_state_cache_read_ms={} snapshot_previous_state_cache_decode_ms={} snapshot_previous_state_cache_validate_ms={} snapshot_previous_state_store_read_ms={} snapshot_signature_lookup_ms={} snapshot_output_build_ms={} monitor_ms={} render_ms={} network_ms={}",
            self.kind.name(),
            if self.path_count == 0 {
                "cold_process"
            } else {
                "warm_repo"
            },
            self.path_count,
            self.samples.len(),
            wall,
            self.metric(|sample| sample.profile_total_ms),
            self.metric(|sample| sample.startup_ms),
            self.metric(|sample| sample.warm_repository_ms),
            self.metric(|sample| sample.repository_open_ms),
            self.metric(|sample| sample.current_state_ms),
            self.metric(|sample| sample.verification_ms),
            self.metric(|sample| sample.worktree_status_ms),
            self.metric(|sample| sample.worktree_index_load_ms),
            self.metric(|sample| sample.worktree_compare_ms),
            self.metric(|sample| sample.worktree_index_save_ms),
            self.metric(|sample| sample.thread_summary_ms),
            self.metric(|sample| sample.snapshot_ms),
            self.metric(|sample| sample.snapshot_tree_walk_ms),
            self.metric(|sample| sample.snapshot_blob_prep_ms),
            self.metric(|sample| sample.snapshot_blob_write_ms),
            self.metric(|sample| sample.snapshot_tree_write_ms),
            self.metric(|sample| sample.snapshot_state_ref_oplog_ms),
            self.metric(|sample| sample.snapshot_atomic_execute_ms),
            self.metric(|sample| sample.snapshot_ref_publish_ms),
            self.metric(|sample| sample.snapshot_state_create_ms),
            self.metric(|sample| sample.snapshot_captured_path_count_ms),
            self.metric(|sample| sample.snapshot_post_verification_ms),
            self.metric(|sample| sample.snapshot_thread_metadata_ms),
            self.metric(|sample| sample.snapshot_preflight_ms),
            self.metric(|sample| sample.snapshot_attribution_ms),
            self.metric(|sample| sample.snapshot_execute_save_ms),
            self.metric(|sample| sample.snapshot_previous_state_ms),
            self.metric(|sample| sample.snapshot_previous_state_head_ms),
            self.metric(|sample| sample.snapshot_previous_state_cache_read_ms),
            self.metric(|sample| sample.snapshot_previous_state_cache_decode_ms),
            self.metric(|sample| sample.snapshot_previous_state_cache_validate_ms),
            self.metric(|sample| sample.snapshot_previous_state_store_read_ms),
            self.metric(|sample| sample.snapshot_signature_lookup_ms),
            self.metric(|sample| sample.snapshot_output_build_ms),
            self.metric(|sample| sample.monitor_ms),
            self.metric(|sample| sample.rendering_ms),
            self.metric(|sample| sample.network_ms),
        );
        println!(
            "COUNTERS case={} paths={} dirs_scanned={} dirs_skipped={} files_hashed={} monitor_paths={} object_decodes={} ref_reads={} oplog_reads={} repo_opens={} network_initialized={} ancestors_visited={} history_objects_decoded={} pack_frame_decompressions={} pack_frame_cache_hits={} pack_blob_bodies_hashed={} pack_state_frames_decoded={}",
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
            self.samples
                .iter()
                .any(|sample| sample.counters.network_client_initialized),
            self.counter(|value| value.ancestors_visited),
            self.counter(|value| value.history_objects_decoded),
            self.counter(|value| value.pack_frame_decompressions),
            self.counter(|value| value.pack_frame_cache_hits),
            self.counter(|value| value.pack_blob_bodies_hashed),
            self.counter(|value| value.pack_state_frames_decoded),
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
        assert!(
            sample.wall_ms.is_finite(),
            "sample {index} should be finite"
        );
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
    let mut command = base_command(binary, cwd);
    command.env("HEDDLE_PROFILE", "jsonl");
    if negative == NegativeControl::SubtreeSkip {
        command.env("HEDDLE_PERF_DISABLE_SUBTREE_SKIP", "1");
    }
    if negative == NegativeControl::EagerPackIndex {
        command.env("HEDDLE_PERF_FORCE_EAGER_PACK_INDEX", "1");
    }
    let output = command.args(args).output().expect("run profiled command");
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
