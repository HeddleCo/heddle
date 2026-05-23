// SPDX-License-Identifier: Apache-2.0
//! Integration tests for state-management commands: clean, revert, stash, merge.

use std::{
    fs,
    process::{Command, Output},
    str,
};

use serde_json::Value;
use tempfile::TempDir;

#[path = "state_management/clean.rs"]
mod clean;
#[path = "state_management/merge.rs"]
mod merge;
#[path = "state_management/merge_store_integrity.rs"]
mod merge_store_integrity;
#[path = "state_management/missing_tree_integrity.rs"]
mod missing_tree_integrity;
#[path = "state_management/revert.rs"]
mod revert;
#[path = "state_management/stash.rs"]
mod stash;

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

fn heddle(args: &[&str], cwd: Option<&std::path::Path>) -> Result<String, String> {
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

fn heddle_output(args: &[&str], cwd: Option<&std::path::Path>) -> Result<Output, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(translate_legacy_args(args));

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    cmd.output().map_err(|e| e.to_string())
}

fn status_json(path: &std::path::Path) -> Value {
    let output = heddle(&["status", "--json"], Some(path)).unwrap();
    serde_json::from_str(&output).expect("status output should be JSON")
}

fn setup_repo_with_file(temp: &TempDir, filename: &str, content: &str) {
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join(filename), content).unwrap();
    heddle(&["capture", "-m", "initial"], Some(temp.path())).unwrap();
}

fn assert_file_exists(path: impl AsRef<std::path::Path>, msg: &str) {
    let path = path.as_ref();
    assert!(path.exists(), "{}: {:?}", msg, path);
}

fn assert_file_not_exists(path: impl AsRef<std::path::Path>, msg: &str) {
    let path = path.as_ref();
    assert!(!path.exists(), "{}: {:?}", msg, path);
}
