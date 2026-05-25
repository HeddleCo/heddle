// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_clean_removes_untracked_files() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "tracked.txt", "tracked content");
    fs::write(temp.path().join("untracked.txt"), "untracked content").unwrap();
    assert_file_exists(
        temp.path().join("untracked.txt"),
        "untracked file should exist before clean",
    );
    let result = heddle(&["clean", "--force"], Some(temp.path()));
    assert!(result.is_ok());
    assert_file_not_exists(
        temp.path().join("untracked.txt"),
        "untracked file should be removed",
    );
    assert_file_exists(
        temp.path().join("tracked.txt"),
        "tracked file should remain",
    );
}
#[test]
fn test_clean_requires_force_flag() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "tracked.txt", "content");
    fs::write(temp.path().join("untracked.txt"), "untracked").unwrap();
    let result = heddle(&["clean"], Some(temp.path()));
    assert!(result.is_err());
    assert_file_exists(temp.path().join("untracked.txt"), "file should still exist");
}
#[test]
fn test_clean_dry_run_does_not_remove() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "tracked.txt", "content");
    fs::write(temp.path().join("untracked.txt"), "untracked").unwrap();
    let result = heddle(&["clean", "--dry-run", "--force"], Some(temp.path()));
    assert!(result.is_ok());
    assert_file_exists(
        temp.path().join("untracked.txt"),
        "dry-run should not remove files",
    );
}
#[test]
fn test_clean_removes_untracked_directories() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "tracked.txt", "content");
    fs::create_dir_all(temp.path().join("untracked_dir/nested")).unwrap();
    fs::write(temp.path().join("untracked_dir/file.txt"), "content").unwrap();
    fs::write(temp.path().join("untracked_dir/nested/deep.txt"), "deep").unwrap();
    let result = heddle(&["clean", "--force"], Some(temp.path()));
    assert!(result.is_ok());
    assert_file_not_exists(
        temp.path().join("untracked_dir"),
        "untracked directory should be removed",
    );
}
#[test]
fn test_clean_preserves_modified_files() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "original");
    fs::write(temp.path().join("file.txt"), "modified").unwrap();
    let result = heddle(&["clean", "--force"], Some(temp.path()));
    assert!(result.is_ok());
    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "modified");
}
#[test]
fn test_clean_preserves_deleted_files_from_index() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");
    fs::remove_file(temp.path().join("file.txt")).unwrap();
    let result = heddle(&["clean", "--force"], Some(temp.path()));
    assert!(result.is_ok());
    assert_file_not_exists(
        temp.path().join("file.txt"),
        "deleted file should stay deleted",
    );
}
#[test]
fn test_clean_empty_repo() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("untracked.txt"), "content").unwrap();
    let result = heddle(&["clean", "--force"], Some(temp.path()));
    assert!(result.is_ok());
    assert_file_not_exists(
        temp.path().join("untracked.txt"),
        "untracked file should be removed",
    );
}
#[test]
fn test_clean_respects_heddleignore() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join(".heddleignore"), "build/\n*.log\n").unwrap();
    fs::write(temp.path().join("tracked.txt"), "content").unwrap();
    heddle(&["capture", "-m", "initial"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("test.log"), "log content").unwrap();
    fs::create_dir_all(temp.path().join("build")).unwrap();
    fs::write(temp.path().join("build/output.txt"), "build output").unwrap();
    fs::write(temp.path().join("other.txt"), "other content").unwrap();
    let result = heddle(&["clean", "--force"], Some(temp.path()));
    assert!(result.is_ok());
    assert_file_exists(
        temp.path().join("test.log"),
        "ignored .log file should be preserved",
    );
    assert_file_exists(
        temp.path().join("build"),
        "ignored build/ dir should be preserved",
    );
    assert_file_not_exists(
        temp.path().join("other.txt"),
        "non-ignored file should be removed",
    );
}
#[test]
fn test_clean_json_output() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "tracked.txt", "content");
    fs::write(temp.path().join("untracked.txt"), "untracked").unwrap();
    fs::write(temp.path().join("another.txt"), "another").unwrap();
    let result = heddle(&["clean", "--force", "--output", "json"], Some(temp.path()));
    assert!(result.is_ok());
    let output: Value = serde_json::from_str(&result.unwrap()).expect("output should be JSON");
    assert!(output.get("removed").is_some());
    let removed = output["removed"]
        .as_array()
        .expect("'removed' should be array");
    assert_eq!(removed.len(), 2);
}
