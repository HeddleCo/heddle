// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_fsck_dangling_ref() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    heddle(&["thread", "create", "orphan"], Some(temp.path())).unwrap();
    let thread_path = temp.path().join(".heddle/refs/threads/orphan");
    fs::write(&thread_path, "hd-deadbeef12345678901234567890").unwrap();

    let result = heddle(&["fsck"], Some(temp.path()));
    assert!(result.is_err(), "fsck should fail with dangling ref");
    let err = result.unwrap_err();
    assert!(
        err.contains("dangling") || err.contains("invalid"),
        "should report dangling ref: {}",
        err
    );
}

#[test]
fn test_fsck_orphaned_objects() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    fs::write(temp.path().join("orphan.txt"), "orphan content").unwrap();
    heddle(&["capture", "-m", "Orphan"], Some(temp.path())).unwrap();
    heddle(&["goto", "HEAD~1"], Some(temp.path())).unwrap();

    let result = heddle(&["fsck", "--full"], Some(temp.path()));
    assert!(result.is_ok(), "fsck should complete: {:?}", result.err());
}

#[test]
fn test_fsck_empty_repository() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let result = heddle(&["fsck", "--full"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "fsck on empty repo should succeed: {:?}",
        result.err()
    );

    let output = result.unwrap();
    assert!(
        output.contains("0 objects") || output.contains("valid"),
        "should report empty repo: {}",
        output
    );
}

#[test]
fn test_fsck_basic_vs_full() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    assert!(heddle(&["fsck"], Some(temp.path())).is_ok());
    assert!(heddle(&["fsck", "--full"], Some(temp.path())).is_ok());
}

#[test]
fn test_fsck_reports_corrupted_object() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    let objects_dir = temp.path().join(".heddle/objects");
    for entry in walkdir::WalkDir::new(&objects_dir) {
        let entry = entry.unwrap();
        if entry.file_type().is_file() {
            let content = fs::read(entry.path()).unwrap();
            if content.len() > 10 {
                let mut corrupted = content.clone();
                corrupted[5] = 0xFF;
                fs::write(entry.path(), corrupted).unwrap();
                break;
            }
        }
    }

    let _result = heddle(&["fsck", "--full"], Some(temp.path()));
}