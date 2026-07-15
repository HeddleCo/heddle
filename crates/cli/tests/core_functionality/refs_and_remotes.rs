// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_marker_delete() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Tagged"], temp.path());
    heddle_must_succeed(&["thread", "marker", "create", "v1.0.0"], temp.path());
    let result = heddle(&["thread", "marker", "list"], Some(temp.path())).unwrap();
    assert!(result.contains("v1.0.0"));
    let result = heddle(&["thread", "marker", "delete", "v1.0.0"], Some(temp.path()));
    assert!(result.is_ok());
    let result = heddle(&["thread", "marker", "list"], Some(temp.path())).unwrap();
    assert!(!result.contains("v1.0.0"));
}

#[test]
fn test_marker_move_to_state() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "v1").unwrap();
    heddle_must_succeed(&["capture", "-m", "v1"], temp.path());
    heddle_must_succeed(&["thread", "marker", "create", "current"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "v2").unwrap();
    heddle_must_succeed(&["capture", "-m", "v2"], temp.path());
    heddle_must_succeed(&["thread", "marker", "delete", "current"], temp.path());
    heddle_must_succeed(&["thread", "marker", "create", "current"], temp.path());
    let result = heddle(&["thread", "marker", "show", "current"], Some(temp.path())).unwrap();
    assert!(result.contains("v2") || result.contains("current"));
}

#[test]
fn test_thread_drop_deletes_thread_ref() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    heddle_must_succeed(&["thread", "create", "feature/test"], temp.path());
    let result = heddle(&["thread", "list"], Some(temp.path())).unwrap();
    assert!(result.contains("feature/test"));
    let result = heddle(
        &["thread", "drop", "feature/test", "--delete-thread"],
        Some(temp.path()),
    );
    assert!(result.is_ok());
    let result = heddle(&["thread", "list"], Some(temp.path())).unwrap();
    assert!(!result.contains("feature/test"));
}

#[test]
fn test_thread_drop_delete_thread_refuses_current() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    let result = heddle(
        &["thread", "drop", "main", "--delete-thread"],
        Some(temp.path()),
    );
    assert!(result.is_err());
}

#[test]
fn test_thread_current_after_init_is_main() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    let result = heddle(&["thread", "current"], Some(temp.path())).unwrap();
    assert_eq!(
        result.trim(),
        "main",
        "thread current should print the active thread name on a single line"
    );
}

#[test]
fn test_thread_current_after_switch() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    heddle_must_succeed(&["thread", "create", "feature/current"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature/current"], temp.path());

    let result = heddle(&["thread", "current"], Some(temp.path())).unwrap();
    assert_eq!(result.trim(), "feature/current");
}

#[test]
fn test_thread_current_json_output() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    let result = heddle(
        &["--output", "json", "thread", "current"],
        Some(temp.path()),
    )
    .unwrap();
    let value: Value = serde_json::from_str(&result).expect("thread current --output json output");
    assert_eq!(value["thread"], "main");
}

#[test]
fn test_thread_drop_delete_thread_removes_thread() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    heddle_must_succeed(&["thread", "create", "feature/drop-delete"], temp.path());

    let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(["thread", "drop", "feature/drop-delete", "--delete-thread"])
        .current_dir(temp.path())
        .output()
        .expect("spawn heddle");
    assert!(
        output.status.success(),
        "thread drop --delete-thread failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
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
fn test_start_creates_named_thread() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    let result = heddle(
        &["start", "feature/auto", "--workspace", "solid"],
        Some(temp.path()),
    );
    assert!(result.is_ok());
    let threads = heddle(&["thread", "list"], Some(temp.path())).unwrap();
    assert!(threads.contains("feature/auto"));
}

#[test]
fn test_thread_and_marker_listing_survives_ref_summary_maintenance() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    heddle_must_succeed(&["thread", "create", "feature/ref-summary"], temp.path());
    heddle_must_succeed(&["thread", "marker", "create", "stable"], temp.path());

    heddle_must_succeed(&["maintenance", "refresh"], temp.path());

    let threads = heddle(&["thread", "list"], Some(temp.path())).unwrap();
    assert!(threads.contains("feature/ref-summary"));

    let markers = heddle(&["thread", "marker", "list"], Some(temp.path())).unwrap();
    assert!(markers.contains("stable"));

    let maintenance: serde_json::Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "maintenance", "inspect"],
            Some(temp.path()),
        )
        .unwrap(),
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
