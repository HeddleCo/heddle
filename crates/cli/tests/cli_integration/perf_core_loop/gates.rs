// SPDX-License-Identifier: Apache-2.0

use std::{env, fs, path::Path, process::Command};

use serde::Deserialize;

use super::measurement::{CaseKind, CaseResult, NegativeControl};

#[derive(Deserialize)]
struct BaselineFile {
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
        let dirs = clean_100k.counter(|counters| counters.directories_scanned);
        let hashes = clean_100k.counter(|counters| counters.files_hashed);
        if dirs.p95 > 10.0 || hashes.p95 > 1.0 {
            failures.push(format!(
                "warm structural gate: clean status @ 100k dirs p95 {:.0} (<=10), files hashed p95 {:.0} (<=1)",
                dirs.p95, hashes.p95
            ));
        }
    }

    if !failures.is_empty() {
        eprintln!("PERF GATE RED");
        for failure in &failures {
            eprintln!("  - {failure}");
        }
        panic!("{} core-loop performance gate(s) failed", failures.len());
    }
}

fn enforce_scale(
    results: &[CaseResult],
    kind: CaseKind,
    baseline: &BaselineFile,
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

fn load_baseline() -> BaselineFile {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/perf/cli-core-loop-baseline.json");
    let contents = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read baseline {}: {error}", path.display()));
    serde_json::from_str(&contents)
        .unwrap_or_else(|error| panic!("parse baseline {}: {error}", path.display()))
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
