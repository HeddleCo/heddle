// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_full_workflow_basic() {
    let temp = TempDir::new().unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("main.rs"), "fn main() {}").unwrap();
    heddle(&["capture", "-m", "Initial commit"], Some(temp.path())).unwrap();

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature",
                "--workspace",
                "auto",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let feature_path = std::path::PathBuf::from(
        started["execution_path"]
            .as_str()
            .expect("start should report the feature checkout"),
    );

    fs::write(feature_path.join("feature.rs"), "pub fn feature() {}").unwrap();
    heddle(&["capture", "-m", "Add feature"], Some(&feature_path)).unwrap();
    heddle(
        &["--output", "json", "ready", "--thread", "feature"],
        Some(temp.path()),
    )
    .unwrap();

    let landed: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "land", "--thread", "feature"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(landed["status"], "landed");
    assert_eq!(landed["integrated"], true);

    assert_exists(temp.path().join("main.rs"), "main.rs should exist");
    assert_exists(temp.path().join("feature.rs"), "feature.rs should exist");

    let log = heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
    assert!(
        log.contains("Add feature"),
        "log should show feature commit"
    );
}

#[test]
fn test_undo_redo_workflow() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "v1");

    fs::write(temp.path().join("file.txt"), "v2").unwrap();
    heddle(&["capture", "-m", "V2"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("file.txt"), "v3").unwrap();
    heddle(&["capture", "-m", "V3"], Some(temp.path())).unwrap();

    heddle(&["undo"], Some(temp.path())).unwrap();
    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "v2", "undo should restore v2");

    heddle(&["undo", "--redo"], Some(temp.path())).unwrap();
    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "v3", "redo should restore v3");
}
