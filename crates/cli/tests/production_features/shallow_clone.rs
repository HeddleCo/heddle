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
