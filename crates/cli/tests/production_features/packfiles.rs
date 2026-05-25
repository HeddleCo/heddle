// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_gc_creates_packfile() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "initial content");

    // Snapshot batches new blobs straight into a packfile (the
    // perf-hot path), so `packs/` already has at least one entry
    // before gc runs. We only assert that gc *adds* one (it packs
    // the loose trees that snapshot still writes loose).
    let packs_dir = temp.path().join(".heddle").join("packs");
    let pack_count_before = pack_count_in(&packs_dir);

    let result = heddle(&["maintenance", "gc", "--aggressive"], Some(temp.path()));
    assert!(result.is_ok(), "gc failed: {:?}", result.err());

    let output = result.unwrap();
    assert!(
        output.contains("Packed") || output.contains("packed"),
        "gc should report packed objects: {}",
        output
    );

    assert!(packs_dir.exists(), "packs directory should exist after gc");
    let pack_count_after = pack_count_in(&packs_dir);
    assert!(
        pack_count_after > pack_count_before,
        "gc should add a pack: before={}, after={}",
        pack_count_before,
        pack_count_after
    );
}

#[test]
fn test_gc_dry_run() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    // Snapshot may have already produced a pack, so capture the
    // pre-state — dry-run's contract is "don't write," not "no
    // packs exist."
    let packs_dir = temp.path().join(".heddle").join("packs");
    let pack_count_before = pack_count_in(&packs_dir);

    let result = heddle(&["maintenance", "gc", "--dry-run"], Some(temp.path()));
    assert!(result.is_ok(), "gc --dry-run failed: {:?}", result.err());

    let output = result.unwrap();
    assert!(
        output.contains("Would"),
        "dry run should say 'Would': {}",
        output
    );

    let pack_count_after = pack_count_in(&packs_dir);
    assert_eq!(
        pack_count_after, pack_count_before,
        "dry run should not change pack count: before={}, after={}",
        pack_count_before, pack_count_after
    );
}

#[test]
fn test_gc_after_multiple_snapshots() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 0..5 {
        fs::write(temp.path().join("file.txt"), format!("content {}", i)).unwrap();
        heddle(
            &["capture", "-m", &format!("snapshot {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    let result = heddle(&["maintenance", "gc", "--aggressive"], Some(temp.path()));
    assert!(result.is_ok(), "gc failed: {:?}", result.err());

    let status = status_json(temp.path());
    assert!(status["state"]["change_id"].is_string());

    let packs_dir = temp.path().join(".heddle").join("packs");
    assert!(packs_dir.exists(), "packs directory should exist");
}

#[test]
fn test_read_from_packfile() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    // Two snapshots so the gc pass packs at least one real parent
    // state. `heddle` doesn't accept `HEAD^` revspecs the way Git
    // does, so we extract the parent change id from the JSON log
    // and feed it back to `diff`.
    fs::write(temp.path().join("file.txt"), "test content for packfile").unwrap();
    heddle(&["capture", "-m", "initial"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "test content evolved").unwrap();
    heddle(&["capture", "-m", "evolution"], Some(temp.path())).unwrap();

    let result = heddle(&["maintenance", "gc", "--aggressive"], Some(temp.path()));
    assert!(result.is_ok(), "gc failed: {:?}", result.err());

    let show_result = heddle(&["show", "HEAD"], Some(temp.path()));
    assert!(
        show_result.is_ok(),
        "show HEAD failed after gc: {:?}",
        show_result.err()
    );

    let log_json = heddle(&["--output", "json", "log"], Some(temp.path())).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&log_json).unwrap();
    let parent = parsed["states"][0]["parents"][0]
        .as_str()
        .expect("HEAD should expose a parent change_id after two snapshots")
        .to_string();
    let head = parsed["states"][0]["change_id"]
        .as_str()
        .unwrap()
        .to_string();

    let diff_result = heddle(&["diff", &parent, &head], Some(temp.path()));
    assert!(
        diff_result.is_ok(),
        "diff after gc-packed history failed: {:?}",
        diff_result.err()
    );
}

#[test]
fn test_gc_prune_removes_loose_objects() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content to be packed");

    // Snapshot pack-batches blobs (none loose) but still writes
    // trees loose. `gc --prune` packs the loose trees and then
    // deletes the now-redundant loose copies — count both blobs
    // and trees so we don't miss the actual cleanup.
    let blobs_dir = temp.path().join(".heddle").join("objects").join("blobs");
    let trees_dir = temp.path().join(".heddle").join("objects").join("trees");
    let loose_count_before = count_files_recursive(&blobs_dir) + count_files_recursive(&trees_dir);

    let result = heddle(&["maintenance", "gc", "--prune"], Some(temp.path()));
    assert!(result.is_ok(), "gc --prune failed: {:?}", result.err());

    let loose_count_after = count_files_recursive(&blobs_dir) + count_files_recursive(&trees_dir);
    assert!(
        loose_count_after < loose_count_before,
        "prune should remove loose objects: before={}, after={}",
        loose_count_before,
        loose_count_after
    );
}

fn pack_count_in(packs_dir: &std::path::Path) -> usize {
    if !packs_dir.exists() {
        return 0;
    }
    fs::read_dir(packs_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("pack"))
                .count()
        })
        .unwrap_or(0)
}

fn count_files_recursive(dir: &std::path::Path) -> usize {
    if !dir.exists() {
        return 0;
    }
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| {
                    let path = e.path();
                    if path.is_dir() {
                        count_files_recursive(&path)
                    } else {
                        1
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}
