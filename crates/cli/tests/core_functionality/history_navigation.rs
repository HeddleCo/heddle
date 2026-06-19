// SPDX-License-Identifier: Apache-2.0
use objects::store::BlockingObjectStore;

use super::*;

#[test]
fn test_long_history_traversal() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    for i in 1..=10 {
        std::fs::write(temp.path().join("version.txt"), format!("v{}", i)).unwrap();
        heddle_must_succeed(&["capture", "-m", &format!("Version {}", i)], temp.path());
    }
    heddle_must_succeed(&["switch", "HEAD~5"], temp.path());
    let content = std::fs::read_to_string(temp.path().join("version.txt")).unwrap();
    assert_eq!(content, "v5");
    heddle_must_succeed(&["switch", "HEAD~4"], temp.path());
    let content = std::fs::read_to_string(temp.path().join("version.txt")).unwrap();
    assert_eq!(content, "v1");
}

#[test]
fn test_short_change_id_resolution() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Test"], temp.path());
    let json_output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let status: Value = serde_json::from_str(&json_output).unwrap();
    let full_id = status["state"]["change_id"].as_str().unwrap();
    let short_id = &full_id[..8];
    let result = heddle(&["show", short_id], Some(temp.path()));
    assert!(result.is_ok());
}

#[test]
fn test_collapse_two_states() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "v1").unwrap();
    heddle_must_succeed(&["capture", "-m", "State 1"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "v2").unwrap();
    heddle_must_succeed(&["capture", "-m", "State 2"], temp.path());
    let repo = Repository::open(temp.path()).unwrap();
    let states: Vec<_> = repo
        .store()
        .list_states()
        .unwrap()
        .into_iter()
        .take(2)
        .collect();
    let result = heddle(
        &[
            "collapse",
            &states[0].to_string_full(),
            &states[1].to_string_full(),
            "--into",
            "Combined",
        ],
        Some(temp.path()),
    );
    assert!(result.is_ok());
    let status = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&status).expect("Status should be JSON");
    let intent = parsed
        .get("state")
        .and_then(|s| s.get("intent"))
        .and_then(|i| i.as_str())
        .unwrap_or("");
    assert!(intent.contains("Combined"));
}

#[test]
fn test_show_no_arg_defaults_to_head() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Test"], temp.path());

    let no_arg = heddle(&["show", "--output", "json"], Some(temp.path()))
        .expect("show with no arg should succeed");
    let explicit_head = heddle(&["show", "HEAD", "--output", "json"], Some(temp.path()))
        .expect("show HEAD should succeed");

    let no_arg_json: Value = serde_json::from_str(&no_arg).expect("no-arg show JSON");
    let head_json: Value = serde_json::from_str(&explicit_head).expect("HEAD show JSON");
    assert_eq!(
        no_arg_json, head_json,
        "bare `show` should render the same state as `show HEAD`"
    );
}
