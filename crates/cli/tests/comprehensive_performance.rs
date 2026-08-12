// SPDX-License-Identifier: Apache-2.0
//! Wall-clock contracts isolated from the parallel comprehensive harness.

use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

#[path = "support/mod.rs"]
mod cli_test_support;

fn heddle(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    cli_test_support::heddle(args, cwd, &[])
}

fn status_json(path: &Path) -> Value {
    let output = heddle(&["status", "--output", "json"], Some(path)).unwrap();
    serde_json::from_str(&output).expect("status should return JSON")
}

fn setup_repo_with_file(temp: &TempDir, filename: &str, content: &str) {
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join(filename), content).unwrap();
    heddle(&["capture", "-m", "initial"], Some(temp.path())).unwrap();
}

fn assert_performance<F>(name: &str, f: F, max_duration: Duration)
where
    F: FnOnce(),
{
    let start = Instant::now();
    f();
    let elapsed = start.elapsed();
    assert!(
        elapsed < max_duration,
        "{} took {:?}, expected under {:?}",
        name,
        elapsed,
        max_duration
    );
}

fn performance_budget(release: Duration, debug: Duration) -> Duration {
    if coverage_instrumented() {
        return debug.saturating_mul(3);
    }
    if cfg!(debug_assertions) {
        debug
    } else {
        release
    }
}

fn coverage_instrumented() -> bool {
    std::env::var_os("LLVM_PROFILE_FILE").is_some() || std::env::var_os("CARGO_LLVM_COV").is_some()
}

#[path = "comprehensive/performance.rs"]
mod performance;
