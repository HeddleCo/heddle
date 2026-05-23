// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_full_workflow_basic() {
    let temp = TempDir::new().unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("main.rs"), "fn main() {}").unwrap();
    heddle(&["capture", "-m", "Initial commit"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("feature.rs"), "pub fn feature() {}").unwrap();
    heddle(&["capture", "-m", "Add feature"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    heddle(&["merge", "feature"], Some(temp.path())).unwrap();

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

    heddle(&["redo"], Some(temp.path())).unwrap();
    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "v3", "redo should restore v3");
}

#[test]
fn test_stash_workflow() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "original");

    fs::write(temp.path().join("file.txt"), "modified").unwrap();
    fs::write(temp.path().join("new.txt"), "new file").unwrap();

    heddle(&["stash", "push", "-m", "WIP"], Some(temp.path())).unwrap();

    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "original", "stash should restore original");
    assert_not_exists(temp.path().join("new.txt"), "stashed file should be gone");

    heddle(&["stash", "pop"], Some(temp.path())).unwrap();

    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "modified", "pop should restore modifications");
    assert_exists(temp.path().join("new.txt"), "pop should restore new file");
}
