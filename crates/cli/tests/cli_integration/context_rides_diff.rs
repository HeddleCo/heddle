// SPDX-License-Identifier: Apache-2.0
//! heddle#1459 — `context set` must ride `diff --context`.
//!
//! Isolation (`context set` + `context get`) already works. The field walk
//! failed because Git-overlay worktree diffs skipped the existing
//! `context` / `broader_guidance` report fields.

use std::path::Path;

use serde_json::Value;
use tempfile::TempDir;

use super::{git_hermetic, heddle};

fn assert_annotation_rides(dir: &Path, path: &str, content: &str) {
    let get = heddle(&["context", "get", "--path", path], Some(dir))
        .unwrap_or_else(|err| panic!("context get should pass isolation: {err}"));
    assert!(
        get.contains(content),
        "context get must show the annotation:\n{get}"
    );

    let text = heddle(&["diff", "--context"], Some(dir))
        .unwrap_or_else(|err| panic!("diff --context should succeed: {err}"));
    assert!(
        text.contains("Applicable Context:"),
        "diff --context must render the context header:\n{text}"
    );
    assert!(
        text.contains(content),
        "diff --context must show the annotation after context set:\n{text}"
    );

    let json = heddle(&["diff", "--context", "--output", "json"], Some(dir))
        .unwrap_or_else(|err| panic!("diff --context json should succeed: {err}"));
    let parsed: Value =
        serde_json::from_str(json.trim()).unwrap_or_else(|err| panic!("json diff: {err}\n{json}"));
    assert_eq!(parsed["output_kind"], "diff");
    let entries = parsed["context"]
        .as_array()
        .unwrap_or_else(|| panic!("diff json must keep the existing context field:\n{json}"));
    let found = entries.iter().any(|entry| {
        entry["path"] == path
            && entry["annotations"].as_array().is_some_and(|annotations| {
                annotations
                    .iter()
                    .any(|annotation| annotation["content"] == content)
            })
    });
    assert!(
        found,
        "existing diff context field must carry the annotation:\n{json}"
    );
}

#[test]
fn context_set_rides_native_worktree_diff() {
    let temp = TempDir::new().expect("tempdir");
    let dir = temp.path();
    heddle(&["init"], Some(dir)).expect("init");
    std::fs::write(dir.join("lib.rs"), "fn seed() {}\n").expect("write seed");
    heddle(&["capture", "-m", "seed"], Some(dir)).expect("capture");
    heddle(
        &[
            "context",
            "set",
            "--path",
            "lib.rs",
            "--kind",
            "invariant",
            "-m",
            "must stay lowercase",
        ],
        Some(dir),
    )
    .expect("context set");
    std::fs::write(dir.join("lib.rs"), "fn seed() { dirty(); }\n").expect("dirty file");

    assert_annotation_rides(dir, "lib.rs", "must stay lowercase");
}

#[test]
fn context_set_rides_git_overlay_worktree_diff() {
    let temp = TempDir::new().expect("tempdir");
    let dir = temp.path();
    git_hermetic(&["init", "-q", "-b", "main", "."], dir);
    std::fs::write(dir.join("lib.rs"), "fn seed() {}\n").expect("write seed");
    git_hermetic(&["add", "lib.rs"], dir);
    git_hermetic(&["commit", "-qm", "seed"], dir);
    heddle(&["init"], Some(dir)).expect("heddle init");
    heddle(
        &[
            "context",
            "set",
            "--path",
            "lib.rs",
            "--kind",
            "invariant",
            "-m",
            "overlay annotation must ride",
        ],
        Some(dir),
    )
    .expect("context set");
    std::fs::write(dir.join("lib.rs"), "fn seed() { dirty(); }\n").expect("dirty file");

    assert_annotation_rides(dir, "lib.rs", "overlay annotation must ride");
}

#[test]
fn context_set_rides_clean_native_diff() {
    let temp = TempDir::new().expect("tempdir");
    let dir = temp.path();
    heddle(&["init"], Some(dir)).expect("init");
    std::fs::write(dir.join("lib.rs"), "fn seed() {}\n").expect("write seed");
    heddle(&["capture", "-m", "seed"], Some(dir)).expect("capture");
    heddle(
        &[
            "context",
            "set",
            "--path",
            "lib.rs",
            "--kind",
            "rationale",
            "-m",
            "visible on a clean tree",
        ],
        Some(dir),
    )
    .expect("context set");

    assert_annotation_rides(dir, "lib.rs", "visible on a clean tree");
}
