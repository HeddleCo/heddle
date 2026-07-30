// SPDX-License-Identifier: Apache-2.0
//! Comprehensive test suite for Heddle VCS.
//!
//! Tests cover: edge cases, error paths, performance, concurrency, platform compatibility

use std::{
    fs,
    path::Path,
    process::{Command, Output},
    str,
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

#[path = "comprehensive/blame.rs"]
mod blame_comprehensive;
#[path = "comprehensive/concurrency.rs"]
mod concurrency;
#[path = "comprehensive/error_paths.rs"]
mod error_paths;
#[path = "comprehensive/fsck.rs"]
mod fsck_comprehensive;
#[path = "comprehensive/gc.rs"]
mod gc_comprehensive;
#[path = "comprehensive/integration.rs"]
mod integration;
#[path = "comprehensive/performance.rs"]
mod performance;
#[path = "comprehensive/platform_compat.rs"]
mod platform_compat;
#[path = "comprehensive/resolve.rs"]
mod resolve_comprehensive;

fn heddle(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let output = heddle_output(args, cwd)?;
    let stdout = str::from_utf8(&output.stdout).unwrap_or("").to_string();
    let stderr = str::from_utf8(&output.stderr).unwrap_or("").to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "Exit code: {:?}\nstdout: {}\nstderr: {}",
            output.status.code(),
            stdout,
            stderr
        ))
    }
}

fn heddle_output(args: &[&str], cwd: Option<&Path>) -> Result<Output, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(args);

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    // Heddle refuses captures without an accountable principal. Pin
    // the identity here so the suite is deterministic regardless of
    // the runner's git global config or shell env.
    cmd.env("HEDDLE_PRINCIPAL_NAME", "Heddle Test")
        .env("HEDDLE_PRINCIPAL_EMAIL", "test@heddle.dev");

    cmd.output().map_err(|e| e.to_string())
}

fn assert_undo_requires_hard(cwd: &Path) {
    let output = heddle_output(&["--output", "json", "undo"], Some(cwd))
        .expect("plain undo should return typed safety advice");
    assert_eq!(output.status.code(), Some(74));
    assert!(output.stdout.is_empty());
    let envelope: Value =
        serde_json::from_slice(&output.stderr).expect("undo refusal should be JSON");
    assert_eq!(envelope["kind"], "undo_requires_hard", "{envelope}");
    assert_eq!(
        envelope["unsafe_condition"],
        "the selected undo operation materializes an earlier saved tree",
        "{envelope}"
    );
    assert_eq!(
        envelope["would_change"],
        "captured worktree files would be replaced by the selected operation's prior tree",
        "{envelope}"
    );
    assert_eq!(
        envelope["preserved"], "repository state and worktree files were left unchanged",
        "{envelope}"
    );
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

fn create_merge_conflict(temp: &TempDir) {
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "base content").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "feature content").unwrap();
    heddle(&["capture", "-m", "Feature commit"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "main content").unwrap();
    heddle(&["capture", "-m", "Main commit"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    let refresh = heddle(
        &["--output", "json", "thread", "refresh", "feature"],
        Some(temp.path()),
    );
    assert!(
        refresh
            .as_ref()
            .is_err_and(|err| err.contains("thread_refresh_conflicted")),
        "refresh should create a durable conflict state: {refresh:?}"
    );
}

fn assert_exists(path: impl AsRef<Path>, msg: &str) {
    assert!(path.as_ref().exists(), "{}: {:?}", msg, path.as_ref());
}

fn assert_not_exists(path: impl AsRef<Path>, msg: &str) {
    assert!(!path.as_ref().exists(), "{}: {:?}", msg, path.as_ref());
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
        // Coverage instrumentation distorts wall-clock perf checks enough that
        // the production/debug budget is no longer a meaningful signal. Keep
        // these bounded so accidental hangs still fail loudly, but don't fail
        // CI coverage because llvm-cov added subprocess overhead.
        return debug.saturating_mul(3);
    }
    if cfg!(debug_assertions) {
        // Comprehensive perf checks run in the default parallel harness beside
        // other subprocess-heavy tests. Keep debug-mode budgets loose enough to
        // avoid scheduler-noise flakes while release budgets guard production
        // expectations.
        debug
    } else {
        release
    }
}

fn coverage_instrumented() -> bool {
    std::env::var_os("LLVM_PROFILE_FILE").is_some() || std::env::var_os("CARGO_LLVM_COV").is_some()
}
