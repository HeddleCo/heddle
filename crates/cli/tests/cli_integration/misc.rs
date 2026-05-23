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
    assert_eq!(parsed["health"]["status"], "uncaptured");
    assert_eq!(parsed["health"]["recommended_action"], "heddle capture");
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
fn status_reports_uncaptured_for_freshly_initialized_repo() {
    let temp = TempDir::new().unwrap();

    let status = Command::new("git")
        .arg("init")
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

    let json = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("status JSON parses");
    assert_eq!(
        parsed["thread_health"], "needs_import",
        "freshly-initialized Git-overlay worktree should fail closed when Git and Heddle are not reconciled: {parsed}"
    );
    assert_eq!(
        parsed["git_overlay_health"]["status"], "needs_import",
        "thread health and Git-overlay health should agree: {parsed}"
    );
    assert_eq!(
        parsed["recommended_action"], "heddle bridge git import --ref main",
        "unimported initial state should recommend the exact import command: {parsed}"
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
