// SPDX-License-Identifier: Apache-2.0

use std::{env, fs, path::Path, process::Command};

use serde::Deserialize;

use super::measurement::{CaseKind, CaseResult, NegativeControl};

const WARM_CLEAN_STATUS_100K_P95_MS: f64 = 50.0;
const ONE_PATH_STATUS_100K_P95_MS: f64 = 75.0;
const ONE_PATH_CAPTURE_100K_P95_MS: f64 = 100.0;
const ONE_PATH_CAPTURE_100K_MAX_OBJECT_DECODES: f64 = 1_010.0;
const BOUNDED_LOCAL_READ_P95_MS: f64 = 100.0;

#[derive(Deserialize)]
struct BaselineFile {
    profiles: Vec<BaselineProfile>,
}

#[derive(Deserialize)]
struct BaselineProfile {
    name: String,
    cases: Vec<BaselineCase>,
    scale_cases: Vec<ScaleBaseline>,
}

#[derive(Deserialize)]
struct ScaleBaseline {
    case: String,
    measured_wall_ratio: f64,
    gate_wall_ratio: f64,
    measured_directory_ratio: f64,
    gate_directory_ratio: f64,
    disposition: String,
}

#[derive(Deserialize)]
struct BaselineCase {
    case: String,
    paths: usize,
    target_p95_ms: f64,
    measured_p95_ms: f64,
    gate_p95_ms: f64,
    disposition: String,
}

pub(super) fn print_runner_fingerprint(binary: &Path, samples: usize, negative: NegativeControl) {
    let cpu = fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|contents| {
            contents
                .lines()
                .find_map(|line| {
                    line.strip_prefix("model name\t: ")
                        .or_else(|| line.strip_prefix("Model\t\t: "))
                })
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());
    let kernel = command_output("uname", &["-sr"]);
    let rustc = command_output("rustc", &["--version"]);
    let cores = std::thread::available_parallelism().map_or(0, usize::from);
    let runner = env::var("RUNNER_NAME").unwrap_or_else(|_| "local".to_string());
    let binary_bytes = fs::metadata(binary).map_or(0, |metadata| metadata.len());
    println!(
        "RUNNER os={} arch={} kernel={kernel:?} cpu={cpu:?} cores={cores} rustc={rustc:?} runner={runner:?} binary_bytes={binary_bytes} samples={samples} negative={negative:?}",
        env::consts::OS,
        env::consts::ARCH,
    );
}

pub(super) fn enforce_contract(results: &[CaseResult]) {
    let baseline = load_baseline();
    println!("BASELINE profile={}", baseline.name);
    let mut failures = Vec::new();

    for result in results {
        if result
            .samples
            .iter()
            .any(|sample| sample.counters.network_client_initialized)
        {
            failures.push(format!(
                "zero-network gate: {} @ {} paths initialized a network client",
                result.kind.name(),
                result.path_count
            ));
        }
        let expected_opens = result.kind.expected_repo_opens();
        let observed_opens = result.counter(|counters| counters.repository_opens);
        if observed_opens.max != expected_opens as f64 {
            failures.push(format!(
                "repository-open gate: {} @ {} paths max {:.0}, expected {expected_opens}",
                result.kind.name(),
                result.path_count,
                observed_opens.max
            ));
        }
        if result.path_count > 0 {
            let repository_open = result.metric(|sample| sample.repository_open_ms);
            if repository_open.p95 > 2.0 {
                failures.push(format!(
                    "repository-open latency gate: {} @ {} paths p95 {:.3} ms > 2.000 ms",
                    result.kind.name(),
                    result.path_count,
                    repository_open.p95
                ));
            }
        }
        if let Some(contract) = baseline
            .cases
            .iter()
            .find(|case| case.case == result.kind.name() && case.paths == result.path_count)
        {
            let wall = result.metric(|sample| sample.wall_ms);
            println!(
                "BAND case={} paths={} target_p95_ms={:.3} measured_baseline_p95_ms={:.3} gate_p95_ms={:.3} disposition={} observed_p95_ms={:.3}",
                contract.case,
                contract.paths,
                contract.target_p95_ms,
                contract.measured_p95_ms,
                contract.gate_p95_ms,
                contract.disposition,
                wall.p95,
            );
            if wall.p95 > contract.gate_p95_ms {
                failures.push(format!(
                    "latency gate: {} @ {} paths p95 {:.3} ms > {:.3} ms ({})",
                    result.kind.name(),
                    result.path_count,
                    wall.p95,
                    contract.gate_p95_ms,
                    contract.disposition
                ));
            }
        }
    }

    for kind in [CaseKind::StatusClean, CaseKind::StatusDirty] {
        enforce_scale(results, kind, &baseline, &mut failures);
    }
    if let Some(clean_100k) = find(results, CaseKind::StatusClean, 100_000) {
        let wall = clean_100k.metric(|sample| sample.wall_ms);
        println!(
            "TARGET case=status_clean paths=100000 budget_p95_ms={WARM_CLEAN_STATUS_100K_P95_MS:.3} observed_p95_ms={:.3}",
            wall.p95
        );
        if wall.p95 > WARM_CLEAN_STATUS_100K_P95_MS {
            failures.push(format!(
                "instant target gate: clean status @ 100k paths p95 {:.3} ms > {WARM_CLEAN_STATUS_100K_P95_MS:.3} ms",
                wall.p95
            ));
        }
        let dirs = clean_100k.counter(|counters| counters.directories_scanned);
        let hashes = clean_100k.counter(|counters| counters.files_hashed);
        if dirs.p95 > 10.0 || hashes.max > 0.0 {
            failures.push(format!(
                "warm structural gate: clean status @ 100k dirs p95 {:.0} (<=10), files hashed max {:.0} (=0)",
                dirs.p95, hashes.max
            ));
        }
    } else {
        failures.push("instant target gate: missing clean status @ 100k paths".to_string());
    }
    if let Some(dirty_100k) = find(results, CaseKind::StatusDirty, 100_000) {
        let dirs = dirty_100k.counter(|counters| counters.directories_scanned);
        let hashes = dirty_100k.counter(|counters| counters.files_hashed);
        if dirs.p95 > 12.0 || hashes.max > 1.0 {
            failures.push(format!(
                "warm structural gate: one-path status @ 100k dirs p95 {:.0} (<=12), files hashed max {:.0} (<=1)",
                dirs.p95, hashes.max
            ));
        }
    }
    if let Some(capture_100k) = find(results, CaseKind::CaptureOne, 100_000) {
        let dirs = capture_100k.counter(|counters| counters.directories_scanned);
        let hashes = capture_100k.counter(|counters| counters.files_hashed);
        let decodes = capture_100k.counter(|counters| counters.object_decodes);
        if dirs.p95 > 20.0 || hashes.max > 1.0 {
            failures.push(format!(
                "warm structural gate: one-path capture @ 100k dirs p95 {:.0} (<=20), files hashed max {:.0} (<=1)",
                dirs.p95, hashes.max
            ));
        }
        if decodes.max > ONE_PATH_CAPTURE_100K_MAX_OBJECT_DECODES {
            failures.push(format!(
                "warm structural gate: one-path capture @ 100k object decodes max {:.0} (<= {:.0})",
                decodes.max, ONE_PATH_CAPTURE_100K_MAX_OBJECT_DECODES
            ));
        }
    }

    enforce_absolute_p95(
        results,
        CaseKind::StatusDirty,
        ONE_PATH_STATUS_100K_P95_MS,
        &mut failures,
    );
    enforce_absolute_p95(
        results,
        CaseKind::CaptureOne,
        ONE_PATH_CAPTURE_100K_P95_MS,
        &mut failures,
    );
    for kind in [
        CaseKind::DiffOne,
        CaseKind::LogBounded,
        CaseKind::DiffOneRepacked,
        CaseKind::LogBoundedRepacked,
        CaseKind::ThreadListBounded,
    ] {
        enforce_absolute_p95(results, kind, BOUNDED_LOCAL_READ_P95_MS, &mut failures);
    }

    enforce_repacked_reads(results, &mut failures);

    if !failures.is_empty() {
        eprintln!("PERF GATE RED");
        for failure in &failures {
            eprintln!("  - {failure}");
        }
        panic!("{} core-loop performance gate(s) failed", failures.len());
    }
}

fn enforce_repacked_reads(results: &[CaseResult], failures: &mut Vec<String>) {
    let Some(diff) = find(results, CaseKind::DiffOneRepacked, 100_000) else {
        failures.push("repacked read gate: missing diff_one_repacked @ 100000 paths".to_string());
        return;
    };
    let diff_decompressions = diff.counter(|counters| counters.pack_frame_decompressions);
    let diff_blob_hashes = diff.counter(|counters| counters.pack_blob_bodies_hashed);
    let diff_state_decodes = diff.counter(|counters| counters.pack_state_frames_decoded);
    println!(
        "REPACKED_READ case=diff_one_repacked frame_decompressions={} blob_bodies_hashed={} state_frames_decoded={}",
        diff_decompressions, diff_blob_hashes, diff_state_decodes
    );
    if diff_decompressions.max > 2.0 || diff_blob_hashes.max > 128.0 || diff_state_decodes.max > 1.0
    {
        failures.push(format!(
            "repacked diff gate: frame decompressions max {:.0} (<=2), blob bodies hashed max {:.0} (<=128), state frames decoded max {:.0} (<=1)",
            diff_decompressions.max, diff_blob_hashes.max, diff_state_decodes.max
        ));
    }

    let Some(log) = find(results, CaseKind::LogBoundedRepacked, 100_000) else {
        failures
            .push("repacked read gate: missing log_bounded_repacked @ 100000 paths".to_string());
        return;
    };
    let log_decompressions = log.counter(|counters| counters.pack_frame_decompressions);
    let log_cache_hits = log.counter(|counters| counters.pack_frame_cache_hits);
    let log_state_decodes = log.counter(|counters| counters.pack_state_frames_decoded);
    println!(
        "REPACKED_READ case=log_bounded_repacked frame_decompressions={} frame_cache_hits={} state_frames_decoded={}",
        log_decompressions, log_cache_hits, log_state_decodes
    );
    if log_decompressions.max > 1.0 || log_state_decodes.max > 1.0 || log_cache_hits.p50 < 19.0 {
        failures.push(format!(
            "repacked log gate: frame decompressions max {:.0} (<=1), state frames decoded max {:.0} (<=1), frame cache hits p50 {:.0} (>=19)",
            log_decompressions.max, log_state_decodes.max, log_cache_hits.p50
        ));
    }
}

fn enforce_absolute_p95(
    results: &[CaseResult],
    kind: CaseKind,
    budget_ms: f64,
    failures: &mut Vec<String>,
) {
    let Some(result) = find(results, kind, 100_000) else {
        failures.push(format!(
            "absolute latency gate: missing {} @ 100000 paths",
            kind.name()
        ));
        return;
    };
    let observed = result.metric(|sample| sample.wall_ms).p95;
    println!(
        "TARGET case={} paths=100000 budget_p95_ms={budget_ms:.3} observed_p95_ms={observed:.3}",
        kind.name()
    );
    if observed > budget_ms {
        failures.push(format!(
            "absolute latency gate: {} @ 100000 paths p95 {observed:.3} ms > {budget_ms:.3} ms",
            kind.name()
        ));
    }
}

fn enforce_scale(
    results: &[CaseResult],
    kind: CaseKind,
    baseline: &BaselineProfile,
    failures: &mut Vec<String>,
) {
    let (Some(small), Some(large)) = (find(results, kind, 10_000), find(results, kind, 100_000))
    else {
        return;
    };
    let small_wall = small.metric(|sample| sample.wall_ms).p95.max(0.001);
    let large_wall = large.metric(|sample| sample.wall_ms).p95;
    let wall_ratio = large_wall / small_wall;
    let small_dirs = small
        .counter(|counters| counters.directories_scanned)
        .p95
        .max(1.0);
    let large_dirs = large.counter(|counters| counters.directories_scanned).p95;
    let directory_ratio = large_dirs / small_dirs;
    let contract = baseline
        .scale_cases
        .iter()
        .find(|case| case.case == kind.name())
        .unwrap_or_else(|| panic!("missing scale baseline for {}", kind.name()));
    println!(
        "SCALE case={} wall_ratio={wall_ratio:.3} measured_wall_ratio={:.3} gate_wall_ratio={:.3} directory_ratio={directory_ratio:.3} measured_directory_ratio={:.3} gate_directory_ratio={:.3} disposition={}",
        kind.name(),
        contract.measured_wall_ratio,
        contract.gate_wall_ratio,
        contract.measured_directory_ratio,
        contract.gate_directory_ratio,
        contract.disposition,
    );
    if wall_ratio > contract.gate_wall_ratio || directory_ratio > contract.gate_directory_ratio {
        failures.push(format!(
            "scale-invariance gate: {} 10k→100k wall {wall_ratio:.3}x (gate {:.3}x), dirs {directory_ratio:.3}x (gate {:.3}x)",
            kind.name(), contract.gate_wall_ratio, contract.gate_directory_ratio
        ));
    }
}

fn find(results: &[CaseResult], kind: CaseKind, path_count: usize) -> Option<&CaseResult> {
    results
        .iter()
        .find(|result| result.kind == kind && result.path_count == path_count)
}

fn load_baseline() -> BaselineProfile {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/perf/cli-core-loop-baseline.json");
    let contents = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read baseline {}: {error}", path.display()));
    let baseline: BaselineFile = serde_json::from_str(&contents)
        .unwrap_or_else(|error| panic!("parse baseline {}: {error}", path.display()));
    let profile_name =
        env::var("HEDDLE_PERF_BASELINE").unwrap_or_else(|_| "local-calibration".to_string());
    baseline
        .profiles
        .into_iter()
        .find(|profile| profile.name == profile_name)
        .unwrap_or_else(|| panic!("missing performance baseline profile {profile_name:?}"))
}

fn command_output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
