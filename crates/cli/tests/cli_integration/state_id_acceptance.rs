// SPDX-License-Identifier: Apache-2.0
//! Coverage for item 1.2 of the heddle 6→8 plan: every state-taking
//! verb must accept the short ID form printed by `heddle log --output json`.
//!
//! Before this fix, `heddle log --output json` returned `state_id` in the
//! short form (`hs-…12 chars`), but state-taking commands rejected
//! anything that wasn't a full state ID. The CLI's own JSON shape was
//! unparseable by its own commands. This test pins the contract.

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

/// Pull the short ID `heddle log --output json` advertises. This is the
/// observation path an agent or wrapper script would take, so the
/// downstream commands have to understand exactly this string.
fn first_short_id(repo: &std::path::Path) -> String {
    let raw = heddle(&["--output", "json", "log", "--limit", "1"], Some(repo)).unwrap();
    let value: Value = serde_json::from_str(&raw).unwrap();
    value["states"][0]["state_id"]
        .as_str()
        .expect("log --output json should expose state_id")
        .to_string()
}

#[test]
fn show_respects_global_repo_argument() {
    let temp = setup_repo();
    let short = first_short_id(temp.path());
    let repo_arg = format!("--repo={}", temp.path().display());
    let raw = heddle(&[repo_arg.as_str(), "--output=json", "show", "HEAD"], None)
        .expect("show --repo should inspect the selected repository");
    let value: Value = serde_json::from_str(&raw).expect("show output should be JSON");
    assert_eq!(
        value["state_id"], short,
        "show --repo HEAD should resolve the selected repository's current state: {value}"
    );
}

#[test]
fn show_accepts_short_id() {
    let temp = setup_repo();
    let short = first_short_id(temp.path());
    let raw = heddle(&["show", &short, "--output", "json"], Some(temp.path()))
        .expect("show should accept short IDs");
    let value: Value = serde_json::from_str(&raw).expect("show output should be JSON");
    assert_eq!(value["state_id"].as_str(), Some(short.as_str()));
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
    let short_a = log_val["states"][1]["state_id"].as_str().unwrap();

    let _output = heddle(
        &["--output", "json", "diff", short_a, &short_b],
        Some(temp.path()),
    )
    .expect("diff should accept short IDs on both sides");
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
        &["query", "--attribution", "hello.txt", "--state", &short],
        Some(temp.path()),
    )
    .expect("query --attribution --state should accept short IDs");
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
    let oldest_short = log_val["states"][1]["state_id"].as_str().unwrap();

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
    heddle(
        &["thread", "marker", "create", "milestone-1"],
        Some(temp.path()),
    )
    .unwrap();
    let raw = heddle(
        &["show", "milestone-1", "--output", "json"],
        Some(temp.path()),
    )
    .expect("show should accept marker names");
    let value: Value = serde_json::from_str(&raw).expect("show output should be JSON");
    assert!(value["state_id"].is_string());
}

#[test]
fn unknown_state_id_yields_state_not_found() {
    let temp = setup_repo();
    let result = heddle(&["show", "hs-zzzzzzzzzzzz"], Some(temp.path()));
    let err = result.expect_err("unknown id should fail");
    assert!(
        err.contains("State not found"),
        "expected `State not found` message, got: {err}"
    );
}

/// `heddle show` used to print `State: hs-… (3a6c582f)` where the
/// parenthetical was `content_hash`. Feeding that hex to `heddle show`
/// produced `State not found`. The printed paren must either be a
/// resolvable spec, or carry a label that makes it obvious it is not.
#[test]
fn show_parenthetical_is_resolvable_spec_or_labeled_not_a_spec() {
    let temp = setup_repo();
    let text = heddle(&["--output", "text", "show"], Some(temp.path())).expect("heddle show");
    let paren = show_state_parenthetical(&text);

    if let Some(hex) = paren.strip_prefix("content_hash ") {
        let labeled_err = heddle(&["show", paren], Some(temp.path()))
            .expect_err("labeled content_hash parenthetical is not a resolvable spec");
        assert!(
            labeled_err.contains("State not found"),
            "show <printed-paren> should fail once the paren is labeled; got: {labeled_err}"
        );
        let hex_err = heddle(&["show", hex], Some(temp.path()))
            .expect_err("content_hash hex is not a state spec");
        assert!(
            hex_err.contains("State not found"),
            "show <content_hash hex> must stay unresolved; got: {hex_err}"
        );
    } else {
        heddle(&["show", paren], Some(temp.path())).unwrap_or_else(|err| {
            panic!(
                "unlabeled parenthetical {paren:?} must be a resolvable spec; show failed: {err}"
            )
        });
    }
}

fn show_state_parenthetical(text: &str) -> &str {
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with("State:"))
        .expect("show should print a State line");
    let start = line
        .rfind('(')
        .expect("State line should include a parenthetical");
    let end = line[start..]
        .find(')')
        .map(|offset| start + offset)
        .expect("State line parenthetical should close");
    line[start + 1..end].trim()
}
