// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_shallow_clone_depth_1() {
    let remote_temp = TempDir::new().unwrap();
    let local_path = remote_temp
        .path()
        .parent()
        .unwrap()
        .join("shallow_clone_test_1");

    if local_path.exists() {
        fs::remove_dir_all(&local_path).ok();
    }

    heddle(&["init"], Some(remote_temp.path())).unwrap();

    for i in 0..5 {
        fs::write(
            remote_temp.path().join("file.txt"),
            format!("content {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("commit {}", i)],
            Some(remote_temp.path()),
        )
        .unwrap();
    }

    let result = heddle(
        &[
            "clone",
            &remote_temp.path().display().to_string(),
            &local_path.display().to_string(),
            "--depth",
            "1",
        ],
        None,
    );
    assert!(result.is_ok(), "shallow clone failed: {:?}", result.err());

    assert!(local_path.join(".heddle").exists());
    assert!(local_path.join("file.txt").exists());

    let shallow_path = local_path.join(".heddle").join("shallow");
    assert!(shallow_path.exists(), "shallow file should exist");

    fs::remove_dir_all(&local_path).ok();
}

#[test]
fn test_shallow_clone_depth_0() {
    let remote_temp = TempDir::new().unwrap();
    let local_path = remote_temp
        .path()
        .parent()
        .unwrap()
        .join("shallow_clone_test_0");

    if local_path.exists() {
        fs::remove_dir_all(&local_path).ok();
    }

    heddle(&["init"], Some(remote_temp.path())).unwrap();

    fs::write(remote_temp.path().join("file.txt"), "initial").unwrap();
    heddle(&["capture", "-m", "initial"], Some(remote_temp.path())).unwrap();

    fs::write(remote_temp.path().join("file.txt"), "second").unwrap();
    heddle(&["capture", "-m", "second"], Some(remote_temp.path())).unwrap();

    let result = heddle(
        &[
            "clone",
            &remote_temp.path().display().to_string(),
            &local_path.display().to_string(),
            "--depth",
            "0",
        ],
        None,
    );
    assert!(
        result.is_ok(),
        "shallow clone depth 0 failed: {:?}",
        result.err()
    );

    let log_result = heddle(&["log", "--output", "json"], Some(&local_path)).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&log_result).expect("log should be JSON");
    let states = parsed["states"].as_array().expect("states should be array");
    // 2 user snapshots. `init` does not create a persisted bootstrap state.
    assert_eq!(states.len(), 2, "depth 0 should behave like a full clone");

    fs::remove_dir_all(&local_path).ok();
}

/// Performance characterization for partial (shallow) clone — issue #238 AC4.
///
/// "Clone a representative large repo with and without partial-clone,
/// document numbers." A wall-clock benchmark on linux/linux is neither
/// hermetic nor CI-stable, so we measure the metric that actually drives
/// transfer cost and is deterministic: the count of objects copied,
/// reported by `clone --output json` (`objects` field). A deep synthetic
/// history stands in for the large repo; `--depth 1` must copy strictly
/// fewer objects than a full clone, and only a small bounded number
/// (tip + immediate parents), independent of history length.
#[test]
fn test_partial_clone_copies_fewer_objects_than_full() {
    const HISTORY_DEPTH: usize = 25;

    let remote_temp = TempDir::new().unwrap();
    let base = remote_temp.path().parent().unwrap();
    let full_path = base.join("partial_clone_full");
    let shallow_path = base.join("partial_clone_shallow");
    for path in [&full_path, &shallow_path] {
        if path.exists() {
            fs::remove_dir_all(path).ok();
        }
    }

    heddle(&["init"], Some(remote_temp.path())).unwrap();
    // Each commit rewrites the same file with distinct content, so every
    // generation introduces a fresh blob + tree + state. A full clone must
    // ferry all of them; a depth-1 clone only needs the tip and its
    // immediate parents.
    for i in 0..HISTORY_DEPTH {
        fs::write(
            remote_temp.path().join("file.txt"),
            format!("revision {i}\n"),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("commit {i}")],
            Some(remote_temp.path()),
        )
        .unwrap();
    }

    let objects_copied = |dest: &std::path::Path, depth: Option<&str>| -> u64 {
        let mut args = vec![
            "--output",
            "json",
            "clone",
            remote_temp.path().to_str().unwrap(),
            dest.to_str().unwrap(),
        ];
        if let Some(depth) = depth {
            args.push("--depth");
            args.push(depth);
        }
        let out = heddle(&args, None).expect("clone should succeed");
        let parsed: Value = serde_json::from_str(&out).expect("clone output should be JSON");
        parsed["objects"]
            .as_u64()
            .expect("clone JSON should report an `objects` count")
    };

    let full_objects = objects_copied(&full_path, None);
    let shallow_objects = objects_copied(&shallow_path, Some("1"));

    // Documented numbers (visible under `cargo test -- --nocapture`).
    eprintln!(
        "partial-clone perf [{HISTORY_DEPTH}-commit history]: full clone copied {full_objects} objects; --depth 1 copied {shallow_objects} objects ({:.1}x fewer)",
        full_objects as f64 / shallow_objects.max(1) as f64,
    );

    assert!(
        shallow_objects < full_objects,
        "partial clone must copy fewer objects than a full clone: shallow={shallow_objects}, full={full_objects}"
    );
    // The shallow clone's cost is bounded by the depth window, not the
    // history length: tip + immediate parents only. A generous ceiling
    // (well under the full count for a 25-commit history) pins that the
    // depth boundary actually truncates the walk.
    assert!(
        shallow_objects <= 12,
        "depth-1 clone should copy only the tip + immediate parents, got {shallow_objects} objects"
    );

    for path in [&full_path, &shallow_path] {
        fs::remove_dir_all(path).ok();
    }
}

#[test]
fn test_normal_clone_no_depth() {
    let remote_temp = TempDir::new().unwrap();
    let local_path = remote_temp
        .path()
        .parent()
        .unwrap()
        .join("normal_clone_test");

    if local_path.exists() {
        fs::remove_dir_all(&local_path).ok();
    }

    heddle(&["init"], Some(remote_temp.path())).unwrap();

    for i in 0..3 {
        fs::write(
            remote_temp.path().join("file.txt"),
            format!("content {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("commit {}", i)],
            Some(remote_temp.path()),
        )
        .unwrap();
    }

    let result = heddle(
        &[
            "clone",
            &remote_temp.path().display().to_string(),
            &local_path.display().to_string(),
        ],
        None,
    );
    assert!(result.is_ok(), "normal clone failed: {:?}", result.err());

    let log_result = heddle(&["log", "--output", "json"], Some(&local_path)).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&log_result).expect("log should be JSON");
    let states = parsed["states"].as_array().expect("states should be array");
    // 3 user snapshots. `init` does not create a persisted bootstrap state.
    assert_eq!(states.len(), 3, "normal clone should have all 3 states");

    fs::remove_dir_all(&local_path).ok();
}
