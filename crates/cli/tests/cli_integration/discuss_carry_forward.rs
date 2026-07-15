// SPDX-License-Identifier: Apache-2.0
//! Native collaboration records remain addressable independently of source HEAD.

use serde_json::Value;
use tempfile::TempDir;

use super::heddle;

fn setup() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(temp.path().join("other.rs"), "fn f() {}\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    temp
}

fn json(out: &str) -> Value {
    serde_json::from_str(out.trim()).expect("valid JSON output")
}

fn open(temp: &TempDir) -> Value {
    json(
        &heddle(
            &[
                "--output", "json", "discuss", "open", "main.rs", "main", "review q",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
}

#[test]
fn discussion_identity_survives_source_state_advance() {
    let temp = setup();
    let opened = open(&temp);
    let id = opened["discussion"]["id"].as_str().unwrap();

    std::fs::write(
        temp.path().join("other.rs"),
        "fn f() { println!(\"x\"); }\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "advance"], Some(temp.path())).unwrap();

    let shown = json(
        &heddle(
            &["--output", "json", "discuss", "show", id],
            Some(temp.path()),
        )
        .unwrap(),
    );
    assert_eq!(shown["discussion"]["id"].as_str(), Some(id));
}

#[test]
fn append_writes_a_new_collaboration_operation() {
    let temp = setup();
    let opened = open(&temp);
    let id = opened["discussion"]["id"].as_str().unwrap();
    let root = opened["operation_id"].as_str().unwrap();

    let appended = json(
        &heddle(
            &["--output", "json", "discuss", "append", id, "second"],
            Some(temp.path()),
        )
        .unwrap(),
    );
    assert_ne!(appended["operation_id"].as_str(), Some(root));
    assert_eq!(appended["discussion"]["turns"].as_array().unwrap().len(), 2);
    assert_eq!(
        appended["discussion"]["turns"][1]["body"].as_str(),
        Some("second")
    );
}

#[test]
fn reopen_compensates_resolution_without_erasing_it() {
    let temp = setup();
    let opened = open(&temp);
    let id = opened["discussion"]["id"].as_str().unwrap();
    heddle(
        &[
            "discuss",
            "resolve",
            id,
            "--mode",
            "dismiss",
            "--reason",
            "not applicable",
        ],
        Some(temp.path()),
    )
    .unwrap();
    let reopened = json(
        &heddle(
            &[
                "--output",
                "json",
                "discuss",
                "reopen",
                id,
                "--reason",
                "new evidence",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    );
    assert_eq!(reopened["discussion"]["status"].as_str(), Some("open"));
    assert!(
        reopened["discussion"]["head_operation_ids"]
            .as_array()
            .is_some_and(|heads| heads.len() == 1)
    );
}
