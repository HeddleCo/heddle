// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_init_in_existing_repo_is_idempotent() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();
    let state_before = status_json(temp.path())["state"]["state_id"].clone();

    let result = heddle(&["init"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "reinitializing an existing repo should succeed: {:?}",
        result.err()
    );
    assert_eq!(
        status_json(temp.path())["state"]["state_id"],
        state_before,
        "idempotent init must preserve the current state"
    );
}

#[test]
fn test_init_in_nested_directory() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let nested = temp.path().join("nested");
    fs::create_dir(&nested).unwrap();

    let result = heddle(&["init"], Some(&nested));
    assert!(result.is_ok() || result.unwrap_err().contains("already"));
}

#[test]
fn test_switch_nonexistent_thread() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    let result = heddle(&["thread", "switch", "nonexistent"], Some(temp.path()));
    assert!(result.is_err(), "switching to a missing thread should fail");
}

#[test]
fn test_snapshot_in_non_repo() {
    let temp = TempDir::new().unwrap();

    let result = heddle(&["capture", "-m", "Test"], Some(temp.path()));
    assert!(result.is_err(), "snapshot outside repo should fail");
}

#[test]
fn test_land_nonexistent_thread() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    let result = heddle(&["land", "--thread", "nonexistent"], Some(temp.path()));
    assert!(result.is_err(), "landing a missing thread should fail");
}

#[test]
fn test_track_create_duplicate() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();

    let _result = heddle(&["thread", "create", "feature"], Some(temp.path()));
}

#[test]
fn test_revert_initial_state() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "first").unwrap();
    heddle(&["capture", "-m", "First"], Some(temp.path())).unwrap();

    let log = heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
    let first_commit = log
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();

    let result = heddle(&["revert", first_commit], Some(temp.path()));
    assert!(
        result.is_ok(),
        "revert initial state should work: {:?}",
        result.err()
    );

    assert_not_exists(
        temp.path().join("file.txt"),
        "reverted file should be removed",
    );
}

#[test]
fn test_diff_binary_files() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let binary: Vec<u8> = (0..100).map(|i| (i * 7) as u8).collect();
    fs::write(temp.path().join("data.bin"), &binary).unwrap();
    heddle(&["capture", "-m", "Binary"], Some(temp.path())).unwrap();

    let mut modified = binary.clone();
    modified[50] = 0xFF;
    fs::write(temp.path().join("data.bin"), &modified).unwrap();

    let result = heddle(&["diff"], Some(temp.path()));
    assert!(result.is_ok() || result.unwrap_err().contains("binary"));
}

#[test]
fn test_status_symlink() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("target.txt"), "target").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink("target.txt", temp.path().join("link.txt")).unwrap();
    }

    let result = heddle(&["status"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "status with symlink should work: {:?}",
        result.err()
    );
}

#[test]
fn test_very_long_filename() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let long_name = "a".repeat(200) + ".txt";
    fs::write(temp.path().join(&long_name), "content").unwrap();

    let result = heddle(&["capture", "-m", "Long filename"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "long filename should work: {:?}",
        result.err()
    );
}

#[test]
fn test_unicode_filename() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let names = vec!["文件.txt", "🎉party.txt", "café.txt", "naïve.txt"];

    for name in names {
        fs::write(temp.path().join(name), format!("content of {}", name)).unwrap();
    }

    let result = heddle(&["capture", "-m", "Unicode"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "unicode filenames should work: {:?}",
        result.err()
    );
}

#[test]
fn test_empty_commit_message() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    let _result = heddle(&["capture", "-m", ""], Some(temp.path()));
}

#[test]
fn test_special_chars_in_commit_message() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    let messages = vec![
        "Message with \"quotes\"",
        "Message with \\ backslash",
        "Message with \n newline",
        "Message with 🎉 emoji",
    ];

    for msg in messages {
        fs::write(temp.path().join("trigger.txt"), msg).unwrap();
        let result = heddle(&["capture", "-m", msg], Some(temp.path()));
        assert!(
            result.is_ok(),
            "message '{}' failed: {:?}",
            msg,
            result.err()
        );
    }
}
