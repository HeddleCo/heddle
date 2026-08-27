// SPDX-License-Identifier: Apache-2.0
//! Last-turn diff/review bases are anchored by the live harness session stamp.

use std::{fs, path::Path};

use serde_json::Value;
use tempfile::TempDir;

use super::{heddle_env, heddle_output_env, heddle_output_with_stdin};

fn init_with_human_base() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    heddle_env(&["init"], Some(temp.path()), &[]).expect("init");
    fs::write(temp.path().join("base.txt"), "base\n").expect("write human base");
    heddle_env(&["capture", "-m", "human base"], Some(temp.path()), &[]).expect("capture base");
    temp
}

fn stamp_session(repo: &Path, session_id: &str) {
    let payload = format!(r#"{{"model":"gpt-5.6-sol","session_id":"{session_id}"}}"#);
    let output =
        heddle_output_with_stdin(&["integration", "stamp", "codex"], repo, payload.as_str())
            .expect("run harness stamp");
    assert!(
        output.status.success(),
        "stamp failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn capture_json(repo: &Path, path: &str, message: &str) -> Value {
    fs::write(repo.join(path), format!("{message}\n")).expect("write capture file");
    let raw = heddle_env(
        &["capture", "-m", message, "--output", "json"],
        Some(repo),
        &[],
    )
    .expect("capture");
    serde_json::from_str(&raw).expect("capture JSON")
}

fn changed_paths(diff: &Value) -> Vec<&str> {
    let changes = &diff["changes"];
    let entries: Vec<&Value> = if let Some(flat) = changes.as_array() {
        flat.iter().collect()
    } else {
        ["modified", "added", "deleted"]
            .into_iter()
            .flat_map(|kind| {
                changes[kind]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    entries
        .into_iter()
        .filter_map(|entry| entry["path"].as_str())
        .collect()
}

#[test]
fn two_agent_captures_in_one_session_anchor_last_turn_to_the_first() {
    let temp = init_with_human_base();
    stamp_session(temp.path(), "session-a");
    let first = capture_json(temp.path(), "first.txt", "first agent capture");
    capture_json(temp.path(), "second.txt", "second agent capture");
    fs::write(temp.path().join("dirty.txt"), "dirty\n").expect("write dirty file");

    let raw = heddle_env(
        &["diff", "--base", "last-turn", "--output", "json"],
        Some(temp.path()),
        &[],
    )
    .expect("last-turn diff");
    let diff: Value = serde_json::from_str(&raw).expect("diff JSON");
    assert_eq!(diff["base"], "last-turn");
    assert_eq!(diff["from_state"], first["state_id"]);
    let paths = changed_paths(&diff);
    assert!(
        paths.contains(&"second.txt"),
        "second capture missing: {diff}"
    );
    assert!(paths.contains(&"dirty.txt"), "dirty change missing: {diff}");
    assert!(!paths.contains(&"first.txt"), "base capture leaked: {diff}");

    let raw = heddle_env(
        &["diff", "--base", "last-turn", "HEAD", "--output", "json"],
        Some(temp.path()),
        &[],
    )
    .expect("last-turn state target diff");
    let captured_diff: Value = serde_json::from_str(&raw).expect("target diff JSON");
    assert_eq!(captured_diff["from_state"], first["state_id"]);
    assert_eq!(changed_paths(&captured_diff), vec!["second.txt"]);

    let raw = heddle_env(
        &[
            "review",
            "show",
            "HEAD",
            "--base",
            "last-turn",
            "--output",
            "json",
        ],
        Some(temp.path()),
        &[],
    )
    .expect("last-turn review");
    let review: Value = serde_json::from_str(&raw).expect("review JSON");
    assert_eq!(review["base"], "last-turn");
    assert_eq!(review["files_changed"], 1);
}

#[test]
fn new_harness_session_starts_a_new_last_turn_base() {
    let temp = init_with_human_base();
    stamp_session(temp.path(), "session-a");
    capture_json(temp.path(), "session-a.txt", "session a capture");

    stamp_session(temp.path(), "session-b");
    let session_b = capture_json(temp.path(), "session-b.txt", "session b capture");
    fs::write(temp.path().join("after-b.txt"), "after b\n").expect("write dirty file");

    let raw = heddle_env(
        &["diff", "--base", "last-turn", "--output", "json"],
        Some(temp.path()),
        &[],
    )
    .expect("session b last-turn diff");
    let diff: Value = serde_json::from_str(&raw).expect("diff JSON");
    assert_eq!(diff["from_state"], session_b["state_id"]);
    let paths = changed_paths(&diff);
    assert_eq!(paths, vec!["after-b.txt"]);
}

#[test]
fn human_only_dirty_worktree_has_no_last_turn_base() {
    let temp = init_with_human_base();
    fs::write(temp.path().join("human-dirty.txt"), "dirty\n").expect("write dirty file");

    let output = heddle_output_env(&["diff", "--base", "last-turn"], Some(temp.path()), &[])
        .expect("run refused diff");
    assert!(!output.status.success(), "last-turn must fail closed");
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(
        diagnostic.contains("no agent turn") && diagnostic.contains("last-turn"),
        "refusal should name the missing base: {diagnostic}"
    );
}
