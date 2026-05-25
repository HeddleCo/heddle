// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn materialized_thread_contains_objectstore_pointer() {
    let main = setup_repo("README.md", "hello");
    let thread_dir = TempDir::new().unwrap();

    heddle(
        &[
            "start",
            "feature/pointer",
            "--workspace",
            "materialized",
            "--path",
            thread_dir.path().to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();

    let heddle_dir = thread_dir.path().join(".heddle");
    assert!(heddle_dir.is_dir(), ".heddle should be a directory");
    let pointer = heddle_dir.join("objectstore");
    assert!(
        pointer.is_file(),
        ".heddle/objectstore should be a regular file"
    );

    let content = fs::read_to_string(&pointer).unwrap();
    assert!(content.starts_with("objectstore:"));
}

#[test]
fn materialized_thread_can_read_shared_history() {
    let main = setup_repo("file.txt", "v1");
    let thread_dir = TempDir::new().unwrap();

    heddle(
        &[
            "start",
            "feature/history",
            "--workspace",
            "materialized",
            "--path",
            thread_dir.path().to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();

    let log_out = heddle(&["--output", "json", "log"], Some(thread_dir.path())).unwrap();
    let log: Value = serde_json::from_str(&log_out).unwrap();
    assert!(!log["states"].as_array().unwrap().is_empty());
}

#[test]
fn status_works_from_materialized_thread_checkout() {
    let main = setup_repo("main.txt", "content");
    let thread_dir = TempDir::new().unwrap();

    heddle(
        &[
            "start",
            "feature/status",
            "--workspace",
            "materialized",
            "--path",
            thread_dir.path().to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();

    let result = heddle(&["status"], Some(thread_dir.path()));
    assert!(result.is_ok(), "status should work from a thread checkout");
}
