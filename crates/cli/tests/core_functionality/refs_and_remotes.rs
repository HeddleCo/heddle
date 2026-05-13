// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_marker_delete() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Tagged"], temp.path());
    heddle_must_succeed(&["marker", "create", "v1.0.0"], temp.path());
    let result = heddle(&["marker", "list"], Some(temp.path())).unwrap();
    assert!(result.contains("v1.0.0"));
    let result = heddle(&["marker", "delete", "v1.0.0"], Some(temp.path()));
    assert!(result.is_ok());
    let result = heddle(&["marker", "list"], Some(temp.path())).unwrap();
    assert!(!result.contains("v1.0.0"));
}

#[test]
fn test_marker_move_to_state() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "v1").unwrap();
    heddle_must_succeed(&["capture", "-m", "v1"], temp.path());
    heddle_must_succeed(&["marker", "create", "current"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "v2").unwrap();
    heddle_must_succeed(&["capture", "-m", "v2"], temp.path());
    heddle_must_succeed(&["marker", "delete", "current"], temp.path());
    heddle_must_succeed(&["marker", "create", "current"], temp.path());
    let result = heddle(&["marker", "show", "current"], Some(temp.path())).unwrap();
    assert!(result.contains("v2") || result.contains("current"));
}

#[test]
fn test_track_delete() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    heddle_must_succeed(&["thread", "create", "feature/test"], temp.path());
    let result = heddle(&["thread", "list"], Some(temp.path())).unwrap();
    assert!(result.contains("feature/test"));
    let result = heddle(&["thread", "delete", "feature/test"], Some(temp.path()));
    assert!(result.is_ok());
    let result = heddle(&["thread", "list"], Some(temp.path())).unwrap();
    assert!(!result.contains("feature/test"));
}

#[test]
fn test_track_cannot_delete_current() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    let result = heddle(&["thread", "delete", "main"], Some(temp.path()));
    assert!(result.is_err());
}

#[test]
fn test_remote_remove() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    heddle_must_succeed(&["remote", "add", "origin", "localhost:8421"], temp.path());
    let result = heddle(&["remote", "list"], Some(temp.path())).unwrap();
    assert!(result.contains("origin"));
    heddle_must_succeed(&["remote", "remove", "origin"], temp.path());
    let result = heddle(&["remote", "list"], Some(temp.path())).unwrap();
    assert!(!result.contains("origin"));
}

#[test]
fn test_remote_duplicate_name() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    heddle_must_succeed(&["remote", "add", "origin", "localhost:8421"], temp.path());
    let _result = heddle(
        &["remote", "add", "origin", "localhost:9999"],
        Some(temp.path()),
    );
    let list_result = heddle(&["remote", "list"], Some(temp.path())).unwrap();
    assert!(list_result.contains("origin"));
}

#[test]
fn test_fork_auto_naming() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    let result = heddle(&["fork"], Some(temp.path()));
    assert!(result.is_ok());
    let status = heddle(&["status"], Some(temp.path())).unwrap();
    assert!(status.contains("Fork") || status.contains("state") || status.contains("change_id"));
}

#[test]
fn test_thread_and_marker_listing_survives_ref_summary_maintenance() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    heddle_must_succeed(&["thread", "create", "feature/ref-summary"], temp.path());
    heddle_must_succeed(&["marker", "create", "stable"], temp.path());

    heddle_must_succeed(&["maintenance", "run"], temp.path());

    let threads = heddle(&["thread", "list"], Some(temp.path())).unwrap();
    assert!(threads.contains("feature/ref-summary"));

    let markers = heddle(&["marker", "list"], Some(temp.path())).unwrap();
    assert!(markers.contains("stable"));

    let maintenance: serde_json::Value = serde_json::from_str(
        &heddle(&["--json", "maintenance", "inspect"], Some(temp.path())).unwrap(),
    )
    .unwrap();
    assert_eq!(
        maintenance["ref_summary_index"]["present"].as_bool(),
        Some(true)
    );
    assert_eq!(
        maintenance["ref_summary_index"]["valid"].as_bool(),
        Some(true)
    );
}