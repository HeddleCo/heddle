// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_log_reverse_order() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "first").unwrap();
    heddle_must_succeed(&["capture", "-m", "First"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "second").unwrap();
    heddle_must_succeed(&["capture", "-m", "Second"], temp.path());
    let result = heddle(&["log", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&result).expect("Log should output valid JSON");
    let states = parsed
        .get("states")
        .and_then(|s| s.as_array())
        .expect("Should have states");
    assert!(states.len() >= 2);
    let first_intent = states[0]
        .get("intent")
        .and_then(|i| i.as_str())
        .unwrap_or("");
    assert!(first_intent.contains("Second"));
}

#[test]
fn test_log_json_output() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Test"], temp.path());
    let result = heddle(&["log", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&result).expect("Log should output valid JSON");
    assert!(parsed.get("states").is_some());
}

#[test]
fn test_invalid_command_arguments() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    let result = heddle(&["goto", "invalid-state-id"], Some(temp.path()));
    assert!(result.is_err());
}

#[test]
fn test_operations_outside_repo() {
    let temp = TempDir::new().unwrap();
    let result = heddle(&["status"], Some(temp.path()));
    assert!(result.is_err());
}

#[test]
fn test_snapshot_empty_repo() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    let result = heddle(&["capture", "-m", "Empty snapshot"], Some(temp.path()));
    assert!(result.is_ok());
}

#[test]
fn test_log_path_filter_shows_only_matching_history() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("src.rs"), "one").unwrap();
    heddle_must_succeed(&["capture", "-m", "src one"], temp.path());

    std::fs::write(temp.path().join("docs.md"), "docs").unwrap();
    heddle_must_succeed(&["capture", "-m", "docs one"], temp.path());

    std::fs::write(temp.path().join("src.rs"), "two").unwrap();
    heddle_must_succeed(&["capture", "-m", "src two"], temp.path());

    let result = heddle(&["log", "--json", "--path", "src.rs"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&result).expect("Log should output valid JSON");
    let intents: Vec<_> = parsed["states"]
        .as_array()
        .expect("states array")
        .iter()
        .filter_map(|state| state["intent"].as_str())
        .collect();

    assert_eq!(intents, vec!["src two", "src one"]);
}

#[test]
fn test_log_path_filter_rejects_parent_traversal() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    let result = heddle(&["log", "--path", "../secret"], Some(temp.path()));
    assert!(result.is_err());
}

/// `heddle history` is a discoverability alias for `heddle log`.
///
/// `history` is the verb users reach for first when they want to see
/// state history; the OSS UX review (heddle#149) caught that it was
/// missing and clap suggested unrelated commands. The alias keeps the
/// natural verb working without splitting the implementation.
#[test]
fn test_history_alias_for_log() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "first").unwrap();
    heddle_must_succeed(&["capture", "-m", "First"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "second").unwrap();
    heddle_must_succeed(&["capture", "-m", "Second"], temp.path());

    let log_out = heddle(&["log", "--json"], Some(temp.path())).unwrap();
    let history_out = heddle(&["history", "--json"], Some(temp.path()))
        .expect("`heddle history` should be accepted as an alias for `heddle log`");
    assert_eq!(log_out, history_out);
}
