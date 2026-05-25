// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn status_text_counts_dirty_worktree_paths() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("dirty.txt"), "pending").unwrap();

    let text = heddle(&["--output", "text", "status"], Some(temp.path())).unwrap();
    assert!(
        text.contains("Changed paths: 1"),
        "dirty worktree should not render as zero changed paths: {text}"
    );
    assert!(
        text.contains("Changes not yet saved") && text.contains("dirty.txt"),
        "status should still list the dirty file: {text}"
    );
    assert!(
        text.contains("Health: work in progress")
            && text.contains("Coordination: work in progress")
            && text.contains("Lifecycle: active")
            && text.contains("Work in progress")
            && !text.contains("Coordination: blocked")
            && !text.contains("Lifecycle: blocked")
            && !text.contains("Blocked by"),
        "ordinary dirty work should read like work in progress, not failure: {text}"
    );
    assert!(
        !text.contains("Tracked changes: 0"),
        "old contradictory label should not appear: {text}"
    );
}

#[test]
fn merged_thread_list_reads_integrated_not_actionable() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    git_commit_all(temp.path(), "seed");
    heddle(&["adopt"], Some(temp.path())).unwrap();

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/polish",
                "--workspace",
                "auto",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread_path.join("polish.txt"), "premium").unwrap();

    let shipped: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "ship", "--thread", "feature/polish"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(shipped["status"], "shipped");

    heddle(&["thread", "refresh", "feature/polish"], Some(temp.path())).unwrap();
    let listed: Value = serde_json::from_str(
        &heddle(&["thread", "list", "--output", "json"], Some(temp.path())).unwrap(),
    )
    .unwrap();
    let thread = listed["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["name"] == "feature/polish")
        .expect("merged thread should be listed");
    assert_eq!(thread["thread_state"], "merged");
    assert_eq!(thread["coordination_status"], "clean");

    let text = heddle(&["--output", "text", "thread", "list"], Some(temp.path())).unwrap();
    let row = text
        .lines()
        .find(|line| line.contains("feature/polish"))
        .unwrap_or("");
    assert!(
        row.contains("clean") && !row.contains("ahead") && !row.contains("stale"),
        "merged thread row should not look actionable: {text}"
    );
    assert!(
        text.contains("lifecycle: merged"),
        "merged state should be visible: {text}"
    );
}

#[test]
fn branch_create_reports_ref_only_not_isolated_checkout() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let created: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "branch", "feature/ref-only"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let thread = &created["thread"];
    assert_eq!(thread["is_isolated"], false, "{created}");
    assert!(thread["path"].is_null(), "{created}");
    assert!(thread["execution_path"].is_null(), "{created}");
    assert_eq!(thread["visibility"], "ref_only", "{created}");

    let show = heddle(
        &["--output", "text", "thread", "show", "feature/ref-only"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        show.contains("Checkout: no dedicated checkout") && !show.contains("Path:"),
        "ref-only thread should not be described as an isolated materialized checkout: {show}"
    );
}

#[test]
fn status_does_not_advertise_ready_thread_for_another_target() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/ready-main",
                "--workspace",
                "materialized",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread_path.join("feature.txt"), "ready").unwrap();
    heddle(
        &["--output", "json", "ready", "-m", "ready for main"],
        Some(&thread_path),
    )
    .unwrap();

    heddle(&["thread", "create", "support/other"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "support/other"], Some(temp.path())).unwrap();
    let status: Value =
        serde_json::from_str(&heddle(&["--output", "json", "status"], Some(temp.path())).unwrap())
            .unwrap();

    assert_ne!(
        status["recommended_action"], "heddle merge feature/ready-main --preview",
        "status on a non-target thread must not suggest merging target-main work into the active thread: {status}"
    );
    assert_eq!(
        status["verification"]["workflow_status"], "clean",
        "{status}"
    );
    assert!(
        status["verification"]["workflow_summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("target another thread")),
        "workflow summary should explain the scoped ready thread: {status}"
    );
}

#[test]
fn human_thread_and_status_output_use_polished_labels() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["start", "feature/visible", "--workspace", "materialized"],
        Some(temp.path()),
    )
    .unwrap();

    let status = heddle(&["--output", "text", "status"], Some(temp.path())).unwrap();
    let show = heddle(
        &["--output", "text", "thread", "show", "feature/visible"],
        Some(temp.path()),
    )
    .unwrap();
    let list = heddle(&["--output", "text", "thread", "list"], Some(temp.path())).unwrap();
    let workspace = heddle(
        &["--output", "text", "workspace", "show"],
        Some(temp.path()),
    )
    .unwrap();
    let doctor = heddle(&["--output", "text", "doctor"], Some(temp.path())).unwrap();
    let captures = heddle(
        &["--output", "text", "thread", "captures", "feature/visible"],
        Some(temp.path()),
    )
    .unwrap();
    let combined = format!("{status}\n{show}\n{list}\n{workspace}\n{doctor}\n{captures}");
    for leaked in [
        " materialized",
        "[materialized",
        "thread mode:",
        "Mode: materialized",
        "Last 5 captures",
        "Captures on",
        "No captures recorded",
        "thread_state",
        "Freshness: unknown",
        "freshness: unknown",
        "tip only",
        "history imported",
    ] {
        assert!(
            !combined.contains(leaked),
            "human output should not leak {leaked:?}: {combined}"
        );
    }

    let json: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/visible"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    // `--workspace visible` (alias of heavy) without `--path` resolves
    // to ThreadMode::Materialized: a real on-disk checkout managed by
    // Heddle. Materialized is reserved for explicit `--path` callers.
    assert_eq!(json["thread_mode"], "materialized");
    assert_eq!(json["freshness"], "current");
}

#[test]
fn status_output_modes_are_explicit_under_non_tty_capture() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let default = heddle(&["status"], Some(temp.path())).unwrap();
    serde_json::from_str::<Value>(&default)
        .unwrap_or_else(|err| panic!("default non-TTY status should be JSON: {err}: {default}"));

    let json = heddle(&["--output", "json", "status"], Some(temp.path())).unwrap();
    serde_json::from_str::<Value>(&json)
        .unwrap_or_else(|err| panic!("--output json status should be JSON: {err}: {json}"));

    let text = heddle(&["--output", "text", "status"], Some(temp.path())).unwrap();
    assert!(
        text.contains("Heddle status") && serde_json::from_str::<Value>(&text).is_err(),
        "--output text should force human output: {text}"
    );
}

fn init_git_repo(path: &std::path::Path) {
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(path)
        .output()
        .expect("git init should run");
    std::process::Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(path)
        .output()
        .expect("git config email should run");
    std::process::Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(path)
        .output()
        .expect("git config name should run");
}

fn git_commit_all(path: &std::path::Path, message: &str) {
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .expect("git add should run");
    let output = std::process::Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(path)
        .output()
        .expect("git commit should run");
    assert!(
        output.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
