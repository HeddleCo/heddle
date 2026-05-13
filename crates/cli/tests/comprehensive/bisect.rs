// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_bisect_finds_regression() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let mut commit_ids = Vec::new();
    for i in 1..=10 {
        fs::write(temp.path().join("counter.txt"), format!("{}", i)).unwrap();
        heddle(
            &["capture", "-m", &format!("Commit {}", i)],
            Some(temp.path()),
        )
        .unwrap();

        let log = heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
        let commit_id = log
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
        commit_ids.push(commit_id.to_string());
    }

    heddle(&["bisect", "start"], Some(temp.path())).unwrap();
    heddle(&["bisect", "bad"], Some(temp.path())).unwrap();
    heddle(&["bisect", "good", &commit_ids[4]], Some(temp.path())).unwrap();

    let _result = heddle(&["bisect", "run", "cat counter.txt"], Some(temp.path()));
}

#[test]
fn test_bisect_reset_cleans_state() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Commit"], Some(temp.path())).unwrap();

    heddle(&["bisect", "start"], Some(temp.path())).unwrap();
    heddle(&["bisect", "reset"], Some(temp.path())).unwrap();

    let bisect_path = temp.path().join(".heddle/BISECT_STATE");
    assert_not_exists(&bisect_path, "bisect state should be cleaned");
}

#[test]
fn test_bisect_without_start_fails() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    let result = heddle(&["bisect", "good", "HEAD"], Some(temp.path()));
    assert!(result.is_err(), "bisect without start should fail");
}

#[test]
fn test_bisect_invalid_good_bad() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 1..=3 {
        fs::write(temp.path().join(format!("file{}.txt", i)), format!("{}", i)).unwrap();
        heddle(&["capture", "-m", &format!("C{}", i)], Some(temp.path())).unwrap();
    }

    heddle(&["bisect", "start"], Some(temp.path())).unwrap();

    let result = heddle(&["bisect", "bad", "HEAD"], Some(temp.path()));
    assert!(result.is_ok());

    let _result = heddle(&["bisect", "good", "HEAD~2"], Some(temp.path()));
}