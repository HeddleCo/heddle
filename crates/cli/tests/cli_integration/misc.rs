// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_cli_json_output() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).expect("status output should be JSON");
    assert!(
        parsed.get("changes").is_some(),
        "JSON should include changes"
    );
}

#[test]
fn test_cli_diagnose_reports_current_context() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("diagnose.txt"), "pending work").unwrap();

    let output = heddle(&["--output", "text", "diagnose"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Doctor"),
        "diagnose header missing: {output}"
    );
    assert!(
        output.contains("Thread: main"),
        "diagnose should name current thread: {output}"
    );
    assert!(
        output.contains("Changes: 0 modified, 1 added, 0 deleted"),
        "diagnose should summarize dirty worktree: {output}"
    );
    assert!(
        output.contains("Next step: heddle capture"),
        "diagnose should recommend capture for dirty worktree: {output}"
    );
}

#[test]
fn test_cli_diagnose_json_profile() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("diagnose.json"), "pending json work").unwrap();

    let output = heddle(&["diagnose", "--profile", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).expect("diagnose output should be JSON");
    assert_eq!(parsed["thread"]["name"], "main");
    assert_eq!(parsed["changes"]["added"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["changes"]["total"], 1);
    assert_eq!(parsed["health"]["status"], "dirty_worktree");
    assert_eq!(parsed["health"]["recommended_action"], "heddle capture");
    assert!(
        parsed["profile"]["total_ms"].is_number(),
        "profile should include total_ms: {parsed}"
    );
}

#[test]
fn test_cli_works_from_subdirectories() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let subdir = temp.path().join("src/nested");
    std::fs::create_dir_all(&subdir).unwrap();

    let result = heddle(&["status"], Some(&subdir));
    assert!(
        result.is_ok(),
        "Should work from subdirectory: {:?}",
        result.err()
    );
}

#[test]
fn test_cli_no_color_flag() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    assert!(heddle(&["status", "--no-color"], Some(temp.path())).is_ok());
}

#[test]
fn test_cli_fails_closed_on_invalid_user_config() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join(".heddle-user")).unwrap();
    std::fs::write(
        temp.path().join(".heddle-user/config.toml"),
        "output = [not valid toml",
    )
    .unwrap();

    let output = heddle_output(&["status"], Some(temp.path())).unwrap();
    assert!(!output.status.success(), "command unexpectedly succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("TOML parse error"), "stderr was {stderr}");
}