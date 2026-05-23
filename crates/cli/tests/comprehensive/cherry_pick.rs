// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_cherry_pick_conflict() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("file.txt"), "v1").unwrap();
    heddle(&["capture", "-m", "V1"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "feature v2").unwrap();
    heddle(&["capture", "-m", "Feature V2"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "main v2").unwrap();
    heddle(&["capture", "-m", "Main V2"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    let log = heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
    let feature_commit = log
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    let result = heddle(&["cherry-pick", feature_commit], Some(temp.path()));

    assert!(result.is_ok() || result.unwrap_err().contains("conflict"));
}

#[test]
fn test_cherry_pick_already_applied() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("file.txt"), "original").unwrap();
    heddle(&["capture", "-m", "Original"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("file.txt"), "modified").unwrap();
    heddle(&["capture", "-m", "Modified"], Some(temp.path())).unwrap();

    let log = heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
    let commit_id = log
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();

    heddle(&["goto", "HEAD~1"], Some(temp.path())).unwrap();
    let result = heddle(&["cherry-pick", commit_id], Some(temp.path()));

    assert!(
        result.is_ok(),
        "should handle already-applied: {:?}",
        result.err()
    );
}

#[test]
fn test_cherry_pick_nonexistent_commit() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    let result = heddle(&["cherry-pick", "hd-deadbeef1234"], Some(temp.path()));
    assert!(result.is_err(), "should fail for nonexistent commit");
}

#[test]
fn test_cherry_pick_multiple_commits() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 1..=3 {
        fs::write(
            temp.path().join(format!("file{}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("Commit {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    let log = heddle(&["log", "--oneline", "--output", "text"], Some(temp.path())).unwrap();
    let commits: Vec<&str> = log
        .lines()
        .take(2)
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();

    heddle(&["goto", "HEAD~2"], Some(temp.path())).unwrap();
    for commit in commits.iter().rev() {
        let result = heddle(&["cherry-pick", commit], Some(temp.path()));
        assert!(
            result.is_ok(),
            "cherry-pick {} failed: {:?}",
            commit,
            result.err()
        );
    }

    for i in 1..=3 {
        assert_exists(
            temp.path().join(format!("file{}.txt", i)),
            &format!("file {} should exist", i),
        );
    }
}
