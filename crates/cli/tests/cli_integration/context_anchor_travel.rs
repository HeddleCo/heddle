// SPDX-License-Identifier: Apache-2.0
//! CLI coverage for durable context-anchor travel across a file rename.
//!
//! Field gap after heddle#1579 / #1580: worktree capture of `lib.py` →
//! `pkg/greeter.py` left `context check` reporting `file_missing` on the
//! old path because travel could not load the pending `pkg/` subtree.

use serde_json::Value;
use tempfile::TempDir;

use super::heddle;

fn json(output: &str) -> Value {
    serde_json::from_str(output.trim()).expect("valid JSON output")
}

const PYTHON: &str = "def greet(name):\n    return f\"hello {name}\"\n";

#[test]
fn context_check_follows_a_python_mkdir_rename_after_capture() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path();
    heddle(&["init"], Some(dir)).unwrap();
    std::fs::write(dir.join("lib.py"), PYTHON).unwrap();
    heddle(&["capture", "-m", "seed"], Some(dir)).unwrap();
    heddle(
        &[
            "context",
            "set",
            "--path",
            "lib.py",
            "--scope",
            "symbol:greet",
            "--kind",
            "invariant",
            "-m",
            "greet stays a greeting",
        ],
        Some(dir),
    )
    .unwrap();

    std::fs::create_dir(dir.join("pkg")).unwrap();
    std::fs::rename(dir.join("lib.py"), dir.join("pkg/greeter.py")).unwrap();
    heddle(&["capture", "-m", "move lib.py into pkg"], Some(dir)).unwrap();

    let check = json(&heddle(&["--output", "json", "context", "check"], Some(dir)).unwrap());
    assert_eq!(check["output_kind"], "context_check");
    assert_eq!(check["annotations"], 1);
    assert_eq!(check["fresh"], 1);
    assert_eq!(check["stale"], 0);
    let issues = check["issues"].as_array().cloned().unwrap_or_default();
    assert!(
        issues.iter().all(|issue| issue["reason"] != "file_missing"),
        "rename+capture must not leave file_missing on the old path:\n{check}"
    );

    let moved = json(
        &heddle(
            &[
                "--output",
                "json",
                "context",
                "get",
                "--path",
                "pkg/greeter.py",
            ],
            Some(dir),
        )
        .unwrap(),
    );
    assert_eq!(moved["output_kind"], "context_get");
    let annotations = moved["annotations"]
        .as_array()
        .expect("context get on the new path should return annotations");
    assert_eq!(annotations.len(), 1);
    assert_eq!(annotations[0]["content"], "greet stays a greeting");
    assert_eq!(annotations[0]["anchor_status"], "resolved");

    let old = json(
        &heddle(
            &["--output", "json", "context", "get", "--path", "lib.py"],
            Some(dir),
        )
        .unwrap(),
    );
    let old_annotations = old["annotations"].as_array().cloned().unwrap_or_default();
    assert!(
        old_annotations.is_empty(),
        "old path should be empty after travel:\n{old}"
    );

    let listed = json(&heddle(&["--output", "json", "context", "list"], Some(dir)).unwrap());
    let items = listed["items"]
        .as_array()
        .expect("context list should wrap items");
    assert!(
        items.iter().any(|item| item["target"] == "pkg/greeter.py"),
        "list must show the annotation on the new path:\n{listed}"
    );
    assert!(
        items.iter().all(|item| item["target"] != "lib.py"),
        "list must not keep a live annotation on the old path:\n{listed}"
    );
}
