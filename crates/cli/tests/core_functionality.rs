// SPDX-License-Identifier: Apache-2.0
//! Core functionality E2E tests.
//!
//! These tests cover essential command paths that users rely on.

use std::{process::Command, str};

use repo::Repository;
use serde_json::Value;
use tempfile::TempDir;

#[path = "support/mod.rs"]
mod cli_test_support;

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
    if let Some(cwd) = cwd {
        cli_test_support::heddle(args, Some(cwd), envs)
    } else {
        let temp = TempDir::new().map_err(|error| error.to_string())?;
        cli_test_support::heddle(args, Some(temp.path()), envs)
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
        "hs-exampletrack\n",
    )
    .unwrap();
}
