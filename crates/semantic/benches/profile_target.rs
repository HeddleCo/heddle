// SPDX-License-Identifier: Apache-2.0
//! Standalone profiling target. Runs many iterations of the function-level
//! merge driver against a synthetic 10k-line / 1000-function Rust file and
//! emits a flamegraph SVG using `pprof-rs` (pthread signals, no perf
//! permissions required).
//!
//! Output: writes the flamegraph to the path in `HEDDLE_PROFILE_OUT`
//! (default `/tmp/semantic-merge-flame.svg`). Iterations controlled by
//! `HEDDLE_PROFILE_ITERS` (default 100).

use std::path::Path;

use merge::ConflictMarkers;
use pprof::ProfilerGuardBuilder;
use semantic::semantic_three_way_merge;

const MARKERS: ConflictMarkers<'static> = ConflictMarkers {
    ours: "OURS",
    theirs: "THEIRS",
};

fn synth_file(n: usize, suffix: &str) -> String {
    let mut s = String::with_capacity(n * 60);
    for i in 0..n {
        s.push_str(&format!("fn fn_{i}() {{ let x = {i}{suffix}; }}\n\n"));
    }
    s
}

fn main() {
    let n: usize = std::env::var("HEDDLE_PROFILE_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000);
    let iters: usize = std::env::var("HEDDLE_PROFILE_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let out_path = std::env::var("HEDDLE_PROFILE_OUT")
        .unwrap_or_else(|_| "/tmp/semantic-merge-flame.svg".to_string());

    let base = synth_file(n, "");
    let mut ours = base.clone();
    let mut theirs = base.clone();
    for i in 0..(n / 2) {
        ours = ours.replace(
            &format!("fn fn_{i}() {{ let x = {i}; }}"),
            &format!("fn fn_{i}() {{ let x = {i}_OURS; }}"),
        );
    }
    for i in (n / 2)..n {
        theirs = theirs.replace(
            &format!("fn fn_{i}() {{ let x = {i}; }}"),
            &format!("fn fn_{i}() {{ let x = {i}_THEIRS; }}"),
        );
    }
    let path = Path::new("synth.rs");

    eprintln!(
        "profiling: n={n} functions, iters={iters}, out={out_path}"
    );

    let guard = ProfilerGuardBuilder::default()
        .frequency(997)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
        .expect("pprof profiler");

    for _ in 0..iters {
        let outcome = semantic_three_way_merge(
            base.as_bytes(),
            ours.as_bytes(),
            theirs.as_bytes(),
            path,
            MARKERS,
        );
        std::hint::black_box(outcome);
    }

    let report = guard.report().build().expect("pprof report");
    let file = std::fs::File::create(&out_path).expect("create flamegraph file");
    report.flamegraph(file).expect("flamegraph write");
    eprintln!("wrote flamegraph to {out_path}");
}
