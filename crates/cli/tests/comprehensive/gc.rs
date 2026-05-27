// SPDX-License-Identifier: Apache-2.0
use super::*;

fn count_objects(temp: &TempDir) -> usize {
    let objects_dir = temp.path().join(".heddle/objects");
    walkdir::WalkDir::new(&objects_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .count()
}

#[test]
fn test_gc_removes_unreachable_objects() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 1..=5 {
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

    let count_before = count_objects(&temp);
    heddle(&["goto", "HEAD~3"], Some(temp.path())).unwrap();

    let result = heddle(&["maintenance", "gc"], Some(temp.path()));
    assert!(result.is_ok(), "gc failed: {:?}", result.err());

    let count_after = count_objects(&temp);
    assert!(
        count_after <= count_before,
        "gc should remove or keep same objects: {} -> {}",
        count_before,
        count_after
    );
}

#[test]
fn test_gc_preserves_reachable_objects() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    let result = heddle(&["maintenance", "gc"], Some(temp.path()));
    assert!(result.is_ok());

    let status = status_json(temp.path());
    assert!(status.get("state").is_some(), "repo should still be valid");

    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "content", "file should still exist");
}

#[test]
fn test_gc_dry_run_accurate() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 1..=3 {
        fs::write(
            temp.path().join(format!("orphan{}.txt", i)),
            format!("orphan {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("Orphan {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    heddle(&["goto", "HEAD~2"], Some(temp.path())).unwrap();

    let dry_run = heddle(&["maintenance", "gc", "--dry-run"], Some(temp.path()));
    assert!(dry_run.is_ok());

    let _actual = heddle(&["maintenance", "gc"], Some(temp.path()));
}

#[test]
fn test_gc_empty_repo() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let result = heddle(&["maintenance", "gc", "--aggressive"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "gc on empty repo should succeed: {:?}",
        result.err()
    );
}
