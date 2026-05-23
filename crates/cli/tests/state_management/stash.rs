// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_stash_saves_changes() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "original");
    fs::write(temp.path().join("file.txt"), "modified").unwrap();
    fs::write(temp.path().join("new.txt"), "new file").unwrap();
    let status_before = status_json(temp.path());
    assert!(
        !status_before["changes"]["modified"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let result = heddle(&["stash", "push"], Some(temp.path()));
    assert!(result.is_ok());
    let status_after = status_json(temp.path());
    assert!(
        status_after["changes"]["modified"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "original");
    assert_file_not_exists(temp.path().join("new.txt"), "new file should be removed");
}
#[test]
fn test_stash_list() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");
    fs::write(temp.path().join("file.txt"), "change 1").unwrap();
    heddle(&["stash", "push", "-m", "first change"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "change 2").unwrap();
    heddle(&["stash", "push", "-m", "second change"], Some(temp.path())).unwrap();
    let result = heddle(&["stash", "list", "--output", "text"], Some(temp.path()));
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(
        output.contains("first change")
            || output.contains("stash@{")
            || output.contains("second change")
    );
}
#[test]
fn test_stash_pop() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "original");
    fs::write(temp.path().join("file.txt"), "stashed changes").unwrap();
    heddle(&["stash", "push"], Some(temp.path())).unwrap();
    let result = heddle(&["stash", "pop"], Some(temp.path()));
    assert!(result.is_ok());
    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "stashed changes");
}
#[test]
fn test_stash_apply_keeps_stash() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "original");
    fs::write(temp.path().join("file.txt"), "stashed changes").unwrap();
    heddle(&["stash", "push"], Some(temp.path())).unwrap();
    let result = heddle(&["stash", "apply"], Some(temp.path()));
    assert!(result.is_ok());
    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "stashed changes");
    let list_result = heddle(&["stash", "list", "--output", "text"], Some(temp.path()));
    let output = list_result.unwrap();
    assert!(output.contains("stash@{"));
}
#[test]
fn test_stash_drop() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "original");
    fs::write(temp.path().join("file.txt"), "stashed").unwrap();
    heddle(&["stash", "push"], Some(temp.path())).unwrap();
    let result = heddle(&["stash", "drop"], Some(temp.path()));
    assert!(result.is_ok());
    let list_result = heddle(&["stash", "list", "--output", "text"], Some(temp.path()));
    let output = list_result.unwrap();
    assert!(!output.contains("stash@") || output.contains("No stashes"));
}
#[test]
fn test_stash_clear() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "original");
    for i in 1..=3 {
        fs::write(temp.path().join("file.txt"), format!("change {}", i)).unwrap();
        heddle(&["stash", "push"], Some(temp.path())).unwrap();
    }
    let result = heddle(&["stash", "clear"], Some(temp.path()));
    assert!(result.is_ok());
    let list_result = heddle(&["stash", "list", "--output", "text"], Some(temp.path()));
    let output = list_result.unwrap();
    assert!(output.trim().is_empty() || output.contains("No stashes"));
}
#[test]
fn test_stash_show() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "original");
    fs::write(temp.path().join("file.txt"), "modified").unwrap();
    fs::write(temp.path().join("new.txt"), "new").unwrap();
    heddle(&["stash", "push"], Some(temp.path())).unwrap();
    let result = heddle(&["stash", "show"], Some(temp.path()));
    assert!(result.is_ok());
    let output = result.unwrap();
    assert!(output.contains("file.txt") || output.contains("new.txt"));
}
#[test]
fn test_stash_on_clean_worktree_fails() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");
    let result = heddle(&["stash"], Some(temp.path()));
    assert!(result.is_err());
}
