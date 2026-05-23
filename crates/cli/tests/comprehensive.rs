// SPDX-License-Identifier: Apache-2.0
//! Comprehensive test suite for Heddle VCS.
//!
//! Tests cover: edge cases, error paths, performance, concurrency, platform compatibility

use std::{
    fs,
    path::Path,
    process::Command,
    str,
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

#[path = "comprehensive/bisect.rs"]
mod bisect_comprehensive;
#[path = "comprehensive/blame.rs"]
mod blame_comprehensive;
#[path = "comprehensive/cherry_pick.rs"]
mod cherry_pick_comprehensive;
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

fn translate_legacy_args(args: &[&str]) -> Vec<String> {
    let mut prefix = Vec::new();
    let mut i = 0;
    while i < args.len() && args[i].starts_with("--") {
        prefix.push(args[i].to_string());
        i += 1;
    }
    let rest = &args[i..];
    let translated = match rest {
        ["thread", "delete", name] => vec![
            "thread".into(),
            "drop".into(),
            (*name).into(),
            "--delete-thread".into(),
        ],
        _ => rest.iter().map(|arg| (*arg).to_string()).collect(),
    };
    prefix.extend(translated);
    prefix
}

fn heddle(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(translate_legacy_args(args));

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let output = cmd.output().map_err(|e| e.to_string())?;
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

fn status_json(path: &Path) -> Value {
    let output = heddle(&["status", "--json"], Some(path)).unwrap();
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

    heddle(&["merge", "feature"], Some(temp.path())).unwrap();
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
