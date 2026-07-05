// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_cli_json_output() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
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

    let output = heddle(&["--output", "text", "doctor"], Some(temp.path())).unwrap();
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
        output.contains("Next step: heddle commit -m \"...\""),
        "diagnose should recommend commit for dirty worktree: {output}"
    );
}

#[test]
fn test_cli_diagnose_json_profile() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("diagnose.json"), "pending json work").unwrap();

    let output = heddle(
        &["doctor", "--profile", "--output", "json"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).expect("diagnose output should be JSON");
    assert_eq!(parsed["thread"]["name"], "main");
    assert_eq!(parsed["changes"]["added"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["changes"]["total"], 1);
    assert_eq!(parsed["health"]["status"], "uncaptured");
    assert_eq!(
        parsed["health"]["recommended_action"],
        "heddle commit -m \"...\""
    );
    assert!(
        parsed["profile"]["total_ms"].is_number(),
        "profile should include total_ms: {parsed}"
    );
}

/// After `heddle init` on a fresh repo, the worktree differs from the
/// (empty) seeded state purely because nothing has been captured yet —
/// reporting that as `dirty_worktree` reads as a problem on first
/// impression. Label it `uncaptured` instead. The recommended action
/// (`heddle capture`) stays the same. See heddle#160.
#[test]
fn status_reports_clean_for_freshly_initialized_git_overlay_repo() {
    let temp = TempDir::new().unwrap();

    let status = Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(temp.path())
        .status()
        .expect("git init should run");
    assert!(status.success());
    for (key, value) in [
        ("user.name", "Heddle Test"),
        ("user.email", "h@example.com"),
    ] {
        let status = Command::new("git")
            .args(["config", key, value])
            .current_dir(temp.path())
            .status()
            .expect("git config should run");
        assert!(status.success());
    }
    std::fs::write(temp.path().join("a"), "").unwrap();
    let status = Command::new("git")
        .args(["add", "."])
        .current_dir(temp.path())
        .status()
        .expect("git add should run");
    assert!(status.success());
    let status = Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(temp.path())
        .status()
        .expect("git commit should run");
    assert!(status.success());

    heddle(&["init"], Some(temp.path())).unwrap();

    let json = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("status JSON parses");
    assert_eq!(
        parsed["thread_health"], "clean",
        "freshly-initialized Git-overlay worktree should read clean Git history directly: {parsed}"
    );
    assert_eq!(
        parsed["verification"]["status"], "clean",
        "thread health and verification status should agree: {parsed}"
    );
    assert_eq!(
        parsed["recommended_action"],
        Value::Null,
        "clean direct-backed Git state should not invent a required action: {parsed}"
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
    let config_path = temp.path().join("invalid-heddle-config.toml");
    std::fs::write(&config_path, "output = [not valid toml").unwrap();

    let output = heddle_output_with_env(
        &["status"],
        Some(temp.path()),
        &[("HEDDLE_CONFIG", config_path.to_str().unwrap())],
    )
    .unwrap();
    assert!(!output.status.success(), "command unexpectedly succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("TOML parse error"), "stderr was {stderr}");
}
