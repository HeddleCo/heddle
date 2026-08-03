// SPDX-License-Identifier: Apache-2.0

mod fixture;
mod gates;
mod measurement;
mod profile;

use std::{env, path::Path, time::Duration};

use fixture::PerfFixture;
use gates::{enforce_contract, print_runner_fingerprint};
use measurement::{CaseKind, NegativeControl, measure_case};

use super::*;

const DEFAULT_SAMPLES: usize = 20;

#[ignore = "release-only instant core-loop contract; run with `TMPDIR=/home/scratch cargo test --release -p heddle-cli --test cli_integration core_loop_release_contract -- --ignored --nocapture`"]
#[test]
fn core_loop_release_contract() {
    if std::hint::black_box(cfg!(debug_assertions)) {
        panic!("core-loop performance contract requires cargo test --release");
    }
    let samples = env_usize("HEDDLE_PERF_SAMPLES", DEFAULT_SAMPLES);
    assert!(samples >= 5, "HEDDLE_PERF_SAMPLES must be at least 5");
    let negative = NegativeControl::from_env();
    let record_only = env_flag("HEDDLE_PERF_RECORD_ONLY");
    let binary = Path::new(env!("CARGO_BIN_EXE_heddle"));

    println!("instant core-loop release contract");
    print_runner_fingerprint(binary, samples, negative);

    let mut results = Vec::new();
    if matches!(negative, NegativeControl::None | NegativeControl::Latency) {
        results.push(measure_case(
            binary,
            None,
            CaseKind::Version,
            samples,
            negative,
        ));
    }
    if negative == NegativeControl::None {
        results.push(measure_case(
            binary,
            None,
            CaseKind::Help,
            samples,
            negative,
        ));
    }

    if !matches!(negative, NegativeControl::Latency) {
        let fixture_sizes: &[usize] = if negative == NegativeControl::DuplicateOpen {
            &[10_000]
        } else {
            &[10_000, 100_000]
        };
        for &path_count in fixture_sizes {
            let mut fixture = PerfFixture::new(binary, path_count);
            if negative == NegativeControl::FullScan {
                fixture.disable_warm_path();
            }
            results.push(measure_case(
                binary,
                Some(&mut fixture),
                CaseKind::StatusClean,
                samples,
                negative,
            ));
            if negative == NegativeControl::None {
                fixture.make_dirty("dirty status\n");
                std::thread::sleep(Duration::from_millis(50));
                results.push(measure_case(
                    binary,
                    Some(&mut fixture),
                    CaseKind::StatusDirty,
                    samples,
                    negative,
                ));
                results.push(measure_case(
                    binary,
                    Some(&mut fixture),
                    CaseKind::CaptureOne,
                    samples,
                    negative,
                ));
            }
        }
    }

    for result in &results {
        result.print();
    }
    if record_only {
        println!("GATES skipped: HEDDLE_PERF_RECORD_ONLY=1");
    } else {
        enforce_contract(&results);
        println!("GATES green: latency, scale invariance, repository opens, zero network");
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_flag(name: &str) -> bool {
    env::var(name).is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
}
