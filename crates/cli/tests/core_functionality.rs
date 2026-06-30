// SPDX-License-Identifier: Apache-2.0
//! Core functionality E2E tests.
//!
//! These tests cover essential command paths that users rely on.

use std::{process::Command, str};

use repo::Repository;
use serde_json::Value;
use tempfile::TempDir;

#[path = "core_functionality/diff_and_status.rs"]
mod diff_and_status;
#[path = "core_functionality/file_operations.rs"]
mod file_operations;
#[path = "core_functionality/history_navigation.rs"]
mod history_navigation;
#[path = "core_functionality/log_and_errors.rs"]
mod log_and_errors;
#[path = "core_functionality/maintenance.rs"]
mod maintenance;
#[path = "core_functionality/refs_and_remotes.rs"]
mod refs_and_remotes;
#[path = "core_functionality/undo_and_special.rs"]
mod undo_and_special;

fn heddle(args: &[&str], cwd: Option<&std::path::Path>) -> Result<String, String> {
    heddle_with_env(args, cwd, &[])
}

fn heddle_with_env(
    args: &[&str],
    cwd: Option<&std::path::Path>,
    envs: &[(&str, &str)],
) -> Result<String, String> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args(args);
    // Pin a principal identity so captures don't refuse under bare
    // CI environments. Explicit `envs` overrides win because they're
    // applied after.
    cmd.env("HEDDLE_PRINCIPAL_NAME", "Heddle Test")
        .env("HEDDLE_PRINCIPAL_EMAIL", "test@heddle.dev");
    cmd.envs(envs.iter().copied());

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    } else {
        let temp = TempDir::new().map_err(|e| e.to_string())?;
        cmd.current_dir(temp.path());
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

fn heddle_must_succeed(args: &[&str], cwd: &std::path::Path) -> String {
    heddle(args, Some(cwd)).unwrap_or_else(|err| panic!("Command failed: {:?}\n{}", args, err))
}

fn write_nested_tracked_heddle_fixture(root: &std::path::Path, head: &str) {
    std::fs::create_dir_all(root.join("examples/calculator/.heddle/refs/threads")).unwrap();
    std::fs::write(root.join("examples/calculator/.heddle/HEAD"), head).unwrap();
    std::fs::write(
        root.join("examples/calculator/.heddleignore"),
        "target/\n*.log\n",
    )
    .unwrap();
    std::fs::write(
        root.join("examples/calculator/.heddle/refs/threads/main"),
        "hd-exampletrack\n",
    )
    .unwrap();
}
