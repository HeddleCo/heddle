// SPDX-License-Identifier: Apache-2.0
//! heddle#1152 criterion 2: `heddle status` surfaces existing context.
//!
//! A repository with 1 annotation and 1 open discussion must report both in
//! `status` — text and JSON — so a cold reader (or a resumed agent) learns
//! pinned context exists without being told.

use serde_json::Value;
use tempfile::TempDir;

use super::heddle;

fn setup_with_context() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    // One context annotation on main.rs.
    heddle(
        &[
            "--output",
            "json",
            "context",
            "set",
            "--path",
            "main.rs",
            "--scope",
            "file",
            "--kind",
            "invariant",
            "-m",
            "returns false on timing mismatch",
        ],
        Some(temp.path()),
    )
    .unwrap();

    // One open discussion anchored to main.rs.
    heddle(
        &[
            "--output", "json", "discuss", "open", "main.rs", "main", "review q",
        ],
        Some(temp.path()),
    )
    .unwrap();

    temp
}

#[test]
fn status_json_reports_annotation_and_discussion_counts() {
    let temp = setup_with_context();

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(output.trim()).expect("status output should be JSON");
    assert_eq!(
        parsed["annotation_count"].as_u64(),
        Some(1),
        "status JSON should count the 1 active annotation: {parsed}"
    );
    assert_eq!(
        parsed["open_discussion_count"].as_u64(),
        Some(1),
        "status JSON should count the 1 open discussion: {parsed}"
    );
}

#[test]
fn status_text_names_annotations_and_discussions() {
    let temp = setup_with_context();

    let output = heddle(&["--output", "text", "status"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Context"),
        "status should have a Context section: {output}"
    );
    assert!(
        output.contains("Annotations: 1 active"),
        "status should surface the annotation count: {output}"
    );
    assert!(
        output.contains("Discussions: 1 open"),
        "status should surface the open-discussion count: {output}"
    );
    // The verbs must be named so a cold reader can act on the hint.
    assert!(
        output.contains("heddle context list") && output.contains("heddle discuss list"),
        "status should point at the discovery verbs: {output}"
    );
}

#[test]
fn status_stays_silent_when_no_context_exists() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let output = heddle(&["--output", "text", "status"], Some(temp.path())).unwrap();
    assert!(
        !output.contains("Context\n"),
        "status should not print an empty Context section: {output}"
    );

    let json_output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(json_output.trim()).unwrap();
    assert_eq!(parsed["annotation_count"].as_u64(), Some(0));
    assert_eq!(parsed["open_discussion_count"].as_u64(), Some(0));
}

#[test]
fn resolved_discussions_are_counted_separately() {
    let temp = setup_with_context();
    let listed = heddle(&["--output", "json", "discuss", "list"], Some(temp.path())).unwrap();
    let listed_json: Value = serde_json::from_str(listed.trim()).unwrap();
    let id = listed_json["discussions"][0]["id"]
        .as_str()
        .expect("discuss list should carry discussion id")
        .to_string();
    heddle(
        &["discuss", "resolve", "--mode", "by-edit", &id],
        Some(temp.path()),
    )
    .unwrap();

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(output.trim()).unwrap();
    assert_eq!(parsed["open_discussion_count"].as_u64(), Some(0));
    assert_eq!(parsed["resolved_discussion_count"].as_u64(), Some(1));
}
