// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_revert_creates_inverse_state() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "original content").unwrap();
    heddle(&["capture", "-m", "Add file"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "modified content").unwrap();
    heddle(&["capture", "-m", "Modify file"], Some(temp.path())).unwrap();
    let result = heddle(&["revert", "HEAD"], Some(temp.path()));
    assert!(result.is_ok());
    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "original content");
}
#[test]
fn test_revert_with_custom_message() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Add file"], Some(temp.path())).unwrap();
    let result = heddle(
        &["revert", "HEAD", "-m", "Undo add file"],
        Some(temp.path()),
    );
    assert!(result.is_ok());
}
#[test]
fn test_revert_add_implicitly_removes_file() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("new_file.txt"), "new content").unwrap();
    heddle(&["capture", "-m", "Add new file"], Some(temp.path())).unwrap();
    assert_file_exists(
        temp.path().join("new_file.txt"),
        "file should exist after snapshot",
    );
    let result = heddle(&["revert", "HEAD"], Some(temp.path()));
    assert!(result.is_ok());
    assert_file_not_exists(
        temp.path().join("new_file.txt"),
        "reverting add should remove file",
    );
}
#[test]
fn test_revert_delete_restores_file() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Add file"], Some(temp.path())).unwrap();
    fs::remove_file(temp.path().join("file.txt")).unwrap();
    heddle(&["capture", "-m", "Remove file"], Some(temp.path())).unwrap();
    assert_file_not_exists(temp.path().join("file.txt"), "file should be deleted");
    let result = heddle(&["revert", "HEAD"], Some(temp.path()));
    assert!(result.is_ok());
    assert_file_exists(
        temp.path().join("file.txt"),
        "reverting delete should restore file",
    );
    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "content");
}
#[test]
fn test_revert_no_commit_flag() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "original").unwrap();
    heddle(&["capture", "-m", "Add file"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "modified").unwrap();
    heddle(&["capture", "-m", "Modify file"], Some(temp.path())).unwrap();
    let result = heddle(&["revert", "HEAD", "--no-commit"], Some(temp.path()));
    assert!(result.is_ok());
    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "original");
    let status_after = status_json(temp.path());
    assert!(
        !status_after["changes"]["modified"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
#[test]
fn test_revert_invalid_state() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let result = heddle(&["revert", "nonexistent"], Some(temp.path()));
    assert!(result.is_err());
}

/// Regression: reverting an "Added" tracked directory must not recursively
/// destroy heddle-ignored content the user dropped beside it. Pre-fix,
/// `apply_inverse_changes` called `remove_path_recursively`, nuking
/// `web/node_modules/` along with the tracked `web/index.html`. Post-fix,
/// only tracked descendants are removed and ignored siblings survive.
#[test]
fn test_revert_preserves_ignored_siblings_in_added_tracked_dir() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "empty"], Some(temp.path())).unwrap();

    // Snapshot adds a tracked directory.
    fs::create_dir_all(temp.path().join("web")).unwrap();
    fs::write(temp.path().join("web/index.html"), "<html/>").unwrap();
    heddle(&["capture", "-m", "add web"], Some(temp.path())).unwrap();

    // User drops heddle-ignored content alongside the tracked file. This
    // is invisible to status (default ignore list covers `node_modules`)
    // but lives on disk.
    fs::create_dir_all(temp.path().join("web/node_modules/lodash")).unwrap();
    fs::write(
        temp.path().join("web/node_modules/lodash/index.js"),
        "ignored",
    )
    .unwrap();

    // Reverting the "add web" snapshot should remove `web/index.html` but
    // leave the heddle-ignored sibling in place.
    heddle(&["revert", "HEAD"], Some(temp.path())).expect("revert must succeed");

    assert_file_not_exists(
        temp.path().join("web/index.html"),
        "tracked file must be reverted",
    );
    assert_file_exists(
        temp.path().join("web/node_modules/lodash/index.js"),
        "heddle-ignored content must survive revert",
    );
}