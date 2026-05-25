// SPDX-License-Identifier: Apache-2.0
//! Typed context annotation recovery advice for empty and missing annotations.

use std::str;

use repo::Repository;
use serde_json::Value;
use tempfile::TempDir;

use super::{assert_json_recovery_advice_fields, heddle, heddle_output};

fn setup_repo_without_context() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    temp
}

fn json_failure(args: &[&str], temp: &TempDir) -> Value {
    let output = heddle_output(args, Some(temp.path())).expect("invoke context failure");
    assert!(
        !output.status.success(),
        "context command should fail for args {args:?}"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON context failure must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = str::from_utf8(&output.stderr).expect("stderr should be utf8");
    let envelope: Value =
        serde_json::from_str(stderr.trim()).expect("context failure should be JSON");
    assert_json_recovery_advice_fields(&envelope, stderr);
    envelope
}

fn assert_context_empty(envelope: &Value) {
    assert_eq!(envelope["kind"], "context_annotations_empty");
    assert_eq!(envelope["primary_command"], "heddle context list");
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle context set")),
        "empty-context advice should explain how to add context: {envelope}"
    );
    assert!(
        envelope["preserved"]
            .as_str()
            .is_some_and(|preserved| preserved.contains("no repository objects")),
        "empty-context advice should assure callers nothing changed: {envelope}"
    );
}

#[test]
fn context_empty_failures_use_typed_json_advice() {
    let temp = setup_repo_without_context();

    for args in [
        vec!["--output", "json", "context", "history", "ann-missing"],
        vec!["--output", "json", "context", "check"],
        vec![
            "--output",
            "json",
            "context",
            "edit",
            "ann-missing",
            "-m",
            "revision",
        ],
        vec![
            "--output",
            "json",
            "context",
            "supersede",
            "ann-missing",
            "-m",
            "replacement",
        ],
    ] {
        let before = Repository::open(temp.path())
            .unwrap()
            .head()
            .unwrap()
            .expect("repo should have HEAD before refusal");
        let envelope = json_failure(&args, &temp);
        let after = Repository::open(temp.path())
            .unwrap()
            .head()
            .unwrap()
            .expect("repo should have HEAD after refusal");
        assert_context_empty(&envelope);
        assert_eq!(
            before, after,
            "empty-context refusal should not move HEAD for args {args:?}"
        );
    }
}

fn setup_repo_with_context() -> (TempDir, String) {
    let temp = setup_repo_without_context();
    heddle(
        &[
            "context",
            "set",
            "--path",
            "main.rs",
            "--scope",
            "file",
            "--kind",
            "rationale",
            "-m",
            "entry point",
        ],
        Some(temp.path()),
    )
    .expect("context set");
    let get = heddle(
        &["--output", "json", "context", "get", "--path", "main.rs"],
        Some(temp.path()),
    )
    .expect("context get");
    let value: Value = serde_json::from_str(&get).expect("context get JSON");
    let annotation_id = value["annotations"][0]["annotation_id"]
        .as_str()
        .expect("annotation id")
        .to_string();
    (temp, annotation_id)
}

#[test]
fn context_missing_annotation_failures_use_typed_json_advice() {
    let (temp, existing_id) = setup_repo_with_context();
    let missing_id = format!("{existing_id}-missing");

    for args in [
        vec![
            "--output",
            "json",
            "context",
            "history",
            missing_id.as_str(),
        ],
        vec![
            "--output",
            "json",
            "context",
            "edit",
            missing_id.as_str(),
            "-m",
            "revision",
        ],
        vec![
            "--output",
            "json",
            "context",
            "supersede",
            missing_id.as_str(),
            "-m",
            "replacement",
        ],
    ] {
        let before = Repository::open(temp.path())
            .unwrap()
            .head()
            .unwrap()
            .expect("repo should have HEAD before refusal");
        let envelope = json_failure(&args, &temp);
        let after = Repository::open(temp.path())
            .unwrap()
            .head()
            .unwrap()
            .expect("repo should have HEAD after refusal");
        assert_eq!(envelope["kind"], "context_annotation_not_found");
        assert_eq!(envelope["primary_command"], "heddle context list");
        assert!(
            envelope["error"]
                .as_str()
                .is_some_and(|error| error.contains(&missing_id)),
            "missing-annotation advice should name the requested id: {envelope}"
        );
        assert!(
            envelope["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("heddle context get --path <path>")),
            "missing-annotation advice should point at annotation ids: {envelope}"
        );
        assert_eq!(
            before, after,
            "missing-annotation refusal should not move HEAD for args {args:?}"
        );
    }
}

#[test]
fn context_empty_text_failure_has_one_next_command() {
    let temp = setup_repo_without_context();
    let output = heddle_output(
        &["--output", "text", "context", "history", "ann-missing"],
        Some(temp.path()),
    )
    .expect("invoke text context failure");
    assert!(!output.status.success(), "text context history should fail");
    assert!(
        output.stdout.is_empty(),
        "text context failure should keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = str::from_utf8(&output.stderr).expect("stderr should be utf8");
    assert!(
        stderr.contains("Error: No context annotations in this repository"),
        "text failure should keep the error calm and direct: {stderr}"
    );
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.starts_with("Next: "))
            .count(),
        1,
        "text failure should print exactly one Next command: {stderr}"
    );
    assert!(
        stderr.contains("Next: heddle context list"),
        "text failure should point at the context list command: {stderr}"
    );
}
