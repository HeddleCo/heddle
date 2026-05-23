// SPDX-License-Identifier: Apache-2.0
//! Coverage for item 1.2 of the heddle 6→8 plan: every state-taking
//! verb must accept the short ID form printed by `heddle log --json`.
//!
//! Before this fix, `heddle log --json` returned `change_id` in the
//! short form (`hd-…12 chars`), but `heddle review show`, `heddle
//! discuss list`, and a couple of others rejected anything that wasn't
//! a 16-byte full ID. The CLI's own JSON shape was unparseable by its
//! own commands. This test pins the contract.

use std::fs;

use serde_json::Value;
use tempfile::TempDir;

use super::heddle;

/// Bootstrap a repo with a single capture so we have a real change ID
/// to feed into every verb.
fn setup_repo() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("hello.txt"), "world\n").unwrap();
    heddle(&["capture", "-m", "init"], Some(temp.path())).unwrap();
    temp
}

/// Pull the short ID `heddle log --json` advertises. This is the
/// observation path an agent or wrapper script would take, so the
/// downstream commands have to understand exactly this string.
fn first_short_id(repo: &std::path::Path) -> String {
    let raw = heddle(&["--output", "json", "log", "--limit", "1"], Some(repo)).unwrap();
    let value: Value = serde_json::from_str(&raw).unwrap();
    value["states"][0]["change_id"]
        .as_str()
        .expect("log --json should expose change_id")
        .to_string()
}

/// `heddle review show <SHORT>` was the headline regression: hosted
/// review demanded full IDs. This pins the fix.
#[test]
fn review_show_accepts_short_id() {
    let temp = setup_repo();
    let short = first_short_id(temp.path());
    let raw = heddle(&["review", "show", &short, "--json"], Some(temp.path()))
        .expect("review show should accept short IDs");
    let value: Value = serde_json::from_str(&raw).expect("review show output should be JSON");
    // Server normalizes back to the full form on the way out, but it
    // must round-trip to a state with a matching prefix.
    let returned = value["change_id"].as_str().expect("change_id present");
    assert!(
        returned.starts_with(&short),
        "round-trip should resolve to the same state: short={short}, returned={returned}"
    );
}

#[test]
fn show_accepts_short_id() {
    let temp = setup_repo();
    let short = first_short_id(temp.path());
    let raw = heddle(&["show", &short, "--output", "json"], Some(temp.path()))
        .expect("show should accept short IDs");
    let value: Value = serde_json::from_str(&raw).expect("show output should be JSON");
    assert_eq!(value["change_id"].as_str(), Some(short.as_str()));
}

#[test]
fn diff_accepts_short_id() {
    let temp = setup_repo();
    let short = first_short_id(temp.path());
    let raw = heddle(&["--output", "json", "diff", &short], Some(temp.path()))
        .expect("diff should accept short IDs");
    let value: Value = serde_json::from_str(&raw).expect("diff output should be JSON");
    assert_eq!(value["from_state"].as_str(), Some(short.as_str()));
}

#[test]
fn compare_accepts_short_id() {
    let temp = setup_repo();
    // Make a second snapshot so we have two distinct states to compare.
    fs::write(temp.path().join("two.txt"), "two\n").unwrap();
    heddle(&["capture", "-m", "second"], Some(temp.path())).unwrap();
    let short_b = first_short_id(temp.path());
    // First state is HEAD~1, but we want to exercise short IDs on both
    // sides, so resolve the first via log too.
    let raw = heddle(
        &["--output", "json", "log", "--limit", "5"],
        Some(temp.path()),
    )
    .unwrap();
    let log_val: Value = serde_json::from_str(&raw).unwrap();
    let short_a = log_val["states"][1]["change_id"].as_str().unwrap();

    let _output = heddle(
        &["--output", "json", "compare", short_a, &short_b],
        Some(temp.path()),
    )
    .expect("compare should accept short IDs on both sides");
}

#[test]
fn discuss_list_accepts_short_id() {
    let temp = setup_repo();
    let short = first_short_id(temp.path());
    let raw = heddle(
        &["--output", "json", "discuss", "list", "--state", &short],
        Some(temp.path()),
    )
    .expect("discuss list --state should accept short IDs");
    let value: Value = serde_json::from_str(&raw).expect("discuss list output should be JSON");
    assert!(value["discussions"].is_array());
}

#[test]
fn cherry_pick_accepts_short_id() {
    let temp = setup_repo();
    // cherry-pick needs a non-current commit. Capture another snapshot,
    // then cherry-pick the older one (HEAD~1) by short ID.
    fs::write(temp.path().join("two.txt"), "two\n").unwrap();
    heddle(&["capture", "-m", "second"], Some(temp.path())).unwrap();
    let raw = heddle(
        &["--output", "json", "log", "--limit", "5"],
        Some(temp.path()),
    )
    .unwrap();
    let log_val: Value = serde_json::from_str(&raw).unwrap();
    let older_short = log_val["states"][1]["change_id"].as_str().unwrap();

    // `--no-commit` keeps this test scoped to argument parsing — we
    // care that the verb accepts the short form, not that the merge
    // semantics succeed.
    heddle(
        &["cherry-pick", older_short, "--no-commit"],
        Some(temp.path()),
    )
    .expect("cherry-pick should accept short IDs");
}

#[test]
fn revert_accepts_short_id() {
    let temp = setup_repo();
    fs::write(temp.path().join("two.txt"), "two\n").unwrap();
    heddle(&["capture", "-m", "second"], Some(temp.path())).unwrap();
    let short = first_short_id(temp.path());

    heddle(&["revert", &short, "--no-commit"], Some(temp.path()))
        .expect("revert should accept short IDs");
}

#[test]
fn blame_accepts_short_id() {
    let temp = setup_repo();
    let short = first_short_id(temp.path());
    let _ = heddle(
        &["blame", "hello.txt", "--state", &short],
        Some(temp.path()),
    )
    .expect("blame --state should accept short IDs");
}

#[test]
fn log_since_accepts_short_id() {
    let temp = setup_repo();
    fs::write(temp.path().join("two.txt"), "two\n").unwrap();
    heddle(&["capture", "-m", "second"], Some(temp.path())).unwrap();
    // The first capture's short id, used as a `--since` lower bound.
    let raw = heddle(
        &["--output", "json", "log", "--limit", "5"],
        Some(temp.path()),
    )
    .unwrap();
    let log_val: Value = serde_json::from_str(&raw).unwrap();
    let oldest_short = log_val["states"][1]["change_id"].as_str().unwrap();

    heddle(
        &["--output", "json", "log", "--since", oldest_short],
        Some(temp.path()),
    )
    .expect("log --since should accept short IDs");
}

#[test]
fn marker_then_show_accepts_marker_name() {
    // Marker names are the third resolution form alongside short
    // and full IDs. Pin the contract.
    let temp = setup_repo();
    heddle(&["marker", "create", "milestone-1"], Some(temp.path())).unwrap();
    let raw = heddle(
        &["show", "milestone-1", "--output", "json"],
        Some(temp.path()),
    )
    .expect("show should accept marker names");
    let value: Value = serde_json::from_str(&raw).expect("show output should be JSON");
    assert!(value["change_id"].is_string());
}

#[test]
fn unknown_state_id_yields_state_not_found() {
    let temp = setup_repo();
    let result = heddle(&["show", "hd-zzzzzzzzzzzz"], Some(temp.path()));
    let err = result.expect_err("unknown id should fail");
    assert!(
        err.contains("State not found"),
        "expected `State not found` message, got: {err}"
    );
}
