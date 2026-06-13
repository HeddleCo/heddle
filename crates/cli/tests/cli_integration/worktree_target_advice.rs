// SPDX-License-Identifier: Apache-2.0
//! JSON error envelopes for materialized worktree target refusals.

use std::{fs, str};

use serde_json::Value;
use tempfile::TempDir;

use super::{assert_json_recovery_advice_fields, heddle, heddle_output};

fn setup_repo() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "init"], Some(temp.path())).unwrap();
    temp
}

#[test]
fn start_path_non_empty_target_uses_typed_advice() {
    let temp = setup_repo();
    let target_parent = TempDir::new().unwrap();
    let target = target_parent.path().join("occupied-checkout");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("existing.txt"), "do not mix\n").unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "start",
            "occupied",
            "--path",
            target.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("invoke start path refusal");
    assert!(
        !output.status.success(),
        "start --path should refuse a non-empty target"
    );
    let stdout = str::from_utf8(&output.stdout).expect("stdout should be utf8");
    assert!(
        stdout.trim().is_empty(),
        "JSON-mode worktree target refusal should keep stdout quiet: {stdout}"
    );
    let stderr = str::from_utf8(&output.stderr).expect("stderr should be utf8");
    let envelope: Value = serde_json::from_str(stderr).expect("stderr should be JSON");
    assert_eq!(envelope["kind"], "worktree_target_not_empty");
    assert_eq!(
        envelope["primary_command"],
        "heddle start <name> --path <empty-path>"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("empty path")),
        "worktree target hint should name the safe retry: {envelope}"
    );
    assert_json_recovery_advice_fields(&envelope, stderr);
}
