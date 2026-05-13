// SPDX-License-Identifier: Apache-2.0
use cli::config::UserConfig;

use super::*;

fn init_git_repo(path: &std::path::Path) {
    let status = Command::new("git")
        .arg("init")
        .current_dir(path)
        .status()
        .expect("git init should run");
    assert!(status.success(), "git init should succeed");

    let status = Command::new("git")
        .args(["config", "user.name", "Heddle Test"])
        .current_dir(path)
        .status()
        .expect("git config user.name should run");
    assert!(status.success());

    let status = Command::new("git")
        .args(["config", "user.email", "heddle@example.com"])
        .current_dir(path)
        .status()
        .expect("git config user.email should run");
    assert!(status.success());

    let status = Command::new("git")
        .args(["checkout", "-b", "feature/drop-in"])
        .current_dir(path)
        .status()
        .expect("git checkout -b should run");
    assert!(status.success());
}

fn git_commit_all(path: &std::path::Path, message: &str) {
    let status = Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .status()
        .expect("git add should run");
    assert!(status.success());

    let status = Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(path)
        .status()
        .expect("git commit should run");
    assert!(status.success());
}

fn git(args: &[&str], path: &std::path::Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(path)
        .status()
        .unwrap_or_else(|err| panic!("git {:?} should run: {}", args, err));
    assert!(status.success(), "git {:?} should succeed", args);
}

#[test]
fn test_cli_capture_blocks_large_git_overlay_deletion_without_force() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::create_dir_all(temp.path().join("web")).unwrap();
    for index in 0..30 {
        std::fs::write(
            temp.path().join("web").join(format!("file-{index}.txt")),
            "tracked",
        )
        .unwrap();
    }
    git_commit_all(temp.path(), "seed web tree");
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::remove_dir_all(temp.path().join("web")).unwrap();
    let error = heddle(&["capture", "-m", "remove web"], Some(temp.path()))
        .expect_err("large deletion capture should require --force");
    assert!(
        error.contains("Large capture safety check") && error.contains("heddle capture --force"),
        "large capture should explain the guardrail and escape hatch: {error}"
    );

    let forced = heddle(
        &["capture", "--force", "-m", "remove web intentionally"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        forced.contains("Captured state"),
        "forced large capture should proceed: {forced}"
    );
}

fn seed_git_history(path: &std::path::Path, commit_count: usize) {
    for revision in 0..commit_count {
        std::fs::write(
            path.join("tracked.txt"),
            format!("tracked revision {revision}"),
        )
        .unwrap();
        git_commit_all(path, &format!("seed revision {revision}"));
    }
}

#[test]
fn test_cli_init_creates_repository() {
    let temp = TempDir::new().unwrap();

    let result = heddle(&["init"], Some(temp.path()));
    assert!(result.is_ok(), "Failed to init: {:?}", result.err());

    let heddle_dir = temp.path().join(".heddle");
    assert!(heddle_dir.exists(), ".heddle directory should exist");
    assert!(
        heddle_dir.join("config.toml").exists(),
        "config.toml should exist"
    );
    assert!(heddle_dir.join("HEAD").exists(), "HEAD should exist");
    assert!(
        heddle_dir.join("objects").exists(),
        "objects directory should exist"
    );
}

#[test]
fn test_cli_init_fails_on_existing_repo() {
    let temp = TempDir::new().unwrap();
    assert!(heddle(&["init"], Some(temp.path())).is_ok());
    assert!(heddle(&["init"], Some(temp.path())).is_err());
}

#[test]
fn test_cli_init_in_git_repo_bootstraps_sidecar() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());

    let output = heddle(&["init"], Some(temp.path())).unwrap();
    assert!(
        output.contains("sidecar"),
        "expected sidecar language: {output}"
    );

    let status = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert_eq!(parsed["storage_model"], "git+heddle-sidecar");
}

#[test]
fn test_cli_status_bootstraps_plain_git_repo_and_adopts_current_branch() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("plain.txt"), "drop-in status").unwrap();

    let status = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert_eq!(parsed["storage_model"], "git+heddle-sidecar");
    assert_eq!(parsed["thread"], "feature/drop-in");
    assert_eq!(parsed["state"], Value::Null);
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "expected plain.txt in added paths: {parsed}"
    );
    assert!(temp.path().join(".heddle").exists());
}

#[test]
fn test_cli_color_force_emits_ansi_for_human_status() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());

    let output = heddle_output_with_env(
        &["--output", "text", "status"],
        Some(temp.path()),
        &[("CLICOLOR_FORCE", "1")],
    )
    .unwrap();
    assert!(output.status.success());
    let stdout = str::from_utf8(&output.stdout).unwrap_or("");
    assert!(
        stdout.contains("\x1b["),
        "forced color should preserve ANSI escapes in captured stdout: {stdout:?}"
    );
}

#[test]
fn test_cli_status_surfaces_git_import_hint_for_other_branches() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    let status = Command::new("git")
        .args(["branch", "support/import-me"])
        .current_dir(temp.path())
        .status()
        .expect("git branch should run");
    assert!(status.success());

    let output = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .all(|value| value != "tracked.txt"),
        "tracked git baseline file should not appear dirty: {parsed}"
    );

    // Import-hint information has moved to `heddle bridge git status
    // --json`; per-command outputs no longer carry it.
    let bridge_output = heddle(&["bridge", "git", "status", "--json"], Some(temp.path())).unwrap();
    let bridge: Value = serde_json::from_str(&bridge_output).unwrap();
    assert_eq!(bridge["git_overlay_import_hint"]["missing_branch_count"], 1);
    assert_eq!(
        bridge["git_overlay_import_hint"]["missing_branches"][0],
        "support/import-me"
    );
    assert_eq!(
        bridge["git_overlay_import_hint"]["recommended_command"],
        "heddle bridge git import --ref support/import-me"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_distinguishes_modified_and_untracked() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::write(temp.path().join("tracked.txt"), "tracked but modified").unwrap();
    std::fs::write(temp.path().join("plain.txt"), "new file").unwrap();

    let output = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();

    assert!(
        parsed["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "tracked.txt"),
        "tracked git file should show as modified: {parsed}"
    );
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "new file should show as added: {parsed}"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_respects_gitignore() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join(".gitignore"), "ignored.log\n").unwrap();
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::write(temp.path().join("ignored.log"), "ignore me").unwrap();
    std::fs::write(temp.path().join("visible.txt"), "show me").unwrap();

    let output = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    let added = parsed["changes"]["added"].as_array().unwrap();

    assert!(
        added.iter().any(|value| value == "visible.txt"),
        "visible file should be present: {parsed}"
    );
    assert!(
        added.iter().all(|value| value != "ignored.log"),
        "ignored file should stay hidden: {parsed}"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_handles_detached_head() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    git(&["checkout", "--detach", "HEAD"], temp.path());
    std::fs::write(temp.path().join("plain.txt"), "detached work").unwrap();

    let output = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();

    assert!(
        parsed["thread"].is_null(),
        "detached HEAD should not fake a thread: {parsed}"
    );
    assert!(
        parsed["git_overlay_import_hint"].is_null(),
        "detached HEAD should not emit branch import hint: {parsed}"
    );
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "detached worktree changes should still show up: {parsed}"
    );
}

#[test]
fn test_cli_status_surfaces_git_import_hint_for_many_branches() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    for branch in 0..12 {
        git(
            &["branch", &format!("support/import-{branch}")],
            temp.path(),
        );
    }

    // Import-hint information has moved to `heddle bridge git status
    // --json`; per-command outputs no longer carry it.
    let output = heddle(&["bridge", "git", "status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();

    assert_eq!(
        parsed["git_overlay_import_hint"]["missing_branch_count"],
        12
    );
    assert_eq!(
        parsed["git_overlay_import_hint"]["missing_branches"]
            .as_array()
            .unwrap()
            .len(),
        12
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_reports_staged_deletions() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::remove_file(temp.path().join("tracked.txt")).unwrap();
    git(&["add", "-A"], temp.path());

    let output = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["changes"]["deleted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "tracked.txt"),
        "staged deletion should show as deleted: {parsed}"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_works_from_subdirectory() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    let nested = temp.path().join("src/nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(temp.path().join("plain.txt"), "new file").unwrap();

    let output = heddle(&["status", "--json"], Some(&nested)).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["thread"], "feature/drop-in");
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "status from subdir should still see repo-root changes: {parsed}"
    );
}

#[test]
fn test_cli_diagnose_in_plain_git_repo_uses_git_baseline() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::write(temp.path().join("tracked.txt"), "tracked but modified").unwrap();
    std::fs::write(temp.path().join("plain.txt"), "new file").unwrap();

    let output = heddle(&["diagnose", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert_eq!(parsed["changes"]["total"], 2);
    assert!(
        parsed["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "tracked.txt"),
        "diagnose should report tracked modification: {parsed}"
    );
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "diagnose should report untracked addition: {parsed}"
    );
}

#[test]
fn test_cli_thread_list_in_plain_git_repo_respects_detached_head() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["checkout", "--detach", "HEAD"], temp.path());

    let output = heddle(&["thread", "list", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["current"].is_null(),
        "thread list should not claim a current branch in detached HEAD: {parsed}"
    );
}

#[test]
fn test_cli_workspace_in_plain_git_repo_respects_detached_head() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["checkout", "--detach", "HEAD"], temp.path());

    let output = heddle(&["workspace", "show", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["current_thread"].is_null(),
        "workspace should not claim a current thread in detached HEAD: {parsed}"
    );
}

#[test]
fn test_cli_show_head_in_plain_git_repo_surfaces_import_hint() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/import-me"], temp.path());

    let output = heddle(&["show", "HEAD", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert!(parsed["change_id"].as_str().is_some());

    // Import-hint information has moved to `heddle bridge git status
    // --json`; per-command outputs no longer carry it.
    let bridge_output = heddle(&["bridge", "git", "status", "--json"], Some(temp.path())).unwrap();
    let bridge: Value = serde_json::from_str(&bridge_output).unwrap();
    assert_eq!(
        bridge["git_overlay_import_hint"]["missing_branches"][0],
        "support/import-me"
    );
}

#[test]
fn test_cli_log_in_plain_git_repo_surfaces_import_hint() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/import-me"], temp.path());

    let output = heddle(&["log", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert!(
        !parsed["states"].as_array().unwrap().is_empty(),
        "log should bootstrap and show at least one state: {parsed}"
    );

    // Import-hint information has moved to `heddle bridge git status
    // --json`; per-command outputs no longer carry it.
    let bridge_output = heddle(&["bridge", "git", "status", "--json"], Some(temp.path())).unwrap();
    let bridge: Value = serde_json::from_str(&bridge_output).unwrap();
    assert_eq!(
        bridge["git_overlay_import_hint"]["missing_branches"][0],
        "support/import-me"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_handles_mixed_staged_and_unstaged_changes() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    std::fs::write(temp.path().join("delete.txt"), "delete me").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::write(temp.path().join("tracked.txt"), "tracked but modified").unwrap();
    std::fs::remove_file(temp.path().join("delete.txt")).unwrap();
    git(&["add", "delete.txt"], temp.path());
    std::fs::write(temp.path().join("plain.txt"), "new file").unwrap();

    let output = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["changes"]["modified"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "tracked.txt"),
        "modified tracked file missing: {parsed}"
    );
    assert!(
        parsed["changes"]["deleted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "delete.txt"),
        "staged deletion missing: {parsed}"
    );
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "untracked addition missing: {parsed}"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_handles_git_rename_as_delete_plus_add() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("old_name.txt"), "rename me").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::rename(
        temp.path().join("old_name.txt"),
        temp.path().join("new_name.txt"),
    )
    .unwrap();
    git(&["add", "-A"], temp.path());

    let output = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["changes"]["deleted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "old_name.txt"),
        "git rename should expose deleted old path: {parsed}"
    );
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "new_name.txt"),
        "git rename should expose added new path: {parsed}"
    );
}

#[test]
fn test_cli_ready_in_plain_git_repo_captures_mixed_git_state() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::write(temp.path().join("tracked.txt"), "tracked but modified").unwrap();
    std::fs::write(temp.path().join("plain.txt"), "new file").unwrap();

    let ready: Value =
        serde_json::from_str(&heddle(&["--json", "ready"], Some(temp.path())).unwrap()).unwrap();
    assert_eq!(ready["captured"], true);

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--json"], Some(temp.path())).unwrap()).unwrap();
    assert!(status["state"]["change_id"].as_str().is_some());
    assert!(status["changes"]["added"].as_array().unwrap().is_empty());
    assert!(status["changes"]["modified"].as_array().unwrap().is_empty());
}

#[test]
fn test_cli_compare_in_plain_git_repo_bootstraps_from_git_overlay_head() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::write(temp.path().join("tracked.txt"), "tracked but modified").unwrap();

    let output = heddle(&["compare", "HEAD", "HEAD"], Some(temp.path())).unwrap();
    // `compare` emits JSON now; assert the schema is present and
    // resolved a state on both sides instead of grepping for legacy
    // human-text markers.
    let parsed: Value = serde_json::from_str(&output)
        .unwrap_or_else(|err| panic!("compare output should be JSON: {err}; raw: {output}"));
    assert!(
        parsed["state_a"].as_str().is_some(),
        "compare must resolve state_a: {output}"
    );
    assert!(
        parsed["state_b"].as_str().is_some(),
        "compare must resolve state_b: {output}"
    );
    assert!(
        parsed["summary"].is_object(),
        "compare must include a summary block: {output}"
    );
}

#[test]
fn test_cli_merge_preview_rejects_dirty_plain_git_repo_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--json",
                "start",
                "feature/preview-thread",
                "--workspace",
                "private",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let thread_path = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread_path.join("thread.txt"), "thread work").unwrap();
    heddle(&["capture", "-m", "Thread capture"], Some(&thread_path)).unwrap();

    std::fs::write(temp.path().join("dirty.txt"), "dirty main worktree").unwrap();

    let err = heddle(
        &["merge", "feature/preview-thread", "--preview"],
        Some(temp.path()),
    )
    .unwrap_err();
    assert!(
        err.contains("uncommitted changes") || err.contains("Cannot merge"),
        "merge preview should reject dirty current worktree: {err}"
    );
}

#[test]
fn test_cli_compare_head_head_bootstraps_in_plain_git_repo() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    let output = heddle(&["compare", "HEAD", "HEAD"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        parsed["summary"]["total"], 0,
        "compare HEAD HEAD should succeed and be empty: {parsed}"
    );
}

#[test]
fn test_cli_diff_head_to_worktree_in_plain_git_repo_uses_git_overlay_baseline() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    std::fs::write(temp.path().join("tracked.txt"), "tracked but modified").unwrap();

    let output = heddle(&["diff", "HEAD"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["path"] == "tracked.txt"),
        "diff from HEAD should reflect tracked modification: {parsed}"
    );
}

#[test]
fn test_cli_status_in_plain_git_repo_handles_deeper_history_and_many_branches() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    seed_git_history(temp.path(), 8);

    for branch in 0..20 {
        git(
            &["branch", &format!("support/history-{branch}")],
            temp.path(),
        );
    }
    std::fs::write(temp.path().join("plain.txt"), "new file").unwrap();

    let output = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["thread"], "feature/drop-in");
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "plain.txt"),
        "plain file should remain visible in larger git fixture: {parsed}"
    );

    // Import-hint information has moved to `heddle bridge git status
    // --json`; per-command outputs no longer carry it.
    let bridge_output = heddle(&["bridge", "git", "status", "--json"], Some(temp.path())).unwrap();
    let bridge: Value = serde_json::from_str(&bridge_output).unwrap();
    assert_eq!(
        bridge["git_overlay_import_hint"]["missing_branch_count"],
        20
    );
}

#[test]
fn test_cli_log_in_plain_git_repo_handles_deeper_history_and_many_branches() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    seed_git_history(temp.path(), 6);

    for branch in 0..10 {
        git(&["branch", &format!("support/log-{branch}")], temp.path());
    }

    let output = heddle(&["log", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert!(
        !parsed["states"].as_array().unwrap().is_empty(),
        "log should still return bootstrap/history state in deeper fixture: {parsed}"
    );

    // Import-hint information has moved to `heddle bridge git status
    // --json`; per-command outputs no longer carry it.
    let bridge_output = heddle(&["bridge", "git", "status", "--json"], Some(temp.path())).unwrap();
    let bridge: Value = serde_json::from_str(&bridge_output).unwrap();
    assert_eq!(
        bridge["git_overlay_import_hint"]["missing_branch_count"],
        10
    );
}

#[test]
fn test_cli_status_tracks_git_branch_switch_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/switch-me"], temp.path());

    let first: Value =
        serde_json::from_str(&heddle(&["status", "--json"], Some(temp.path())).unwrap()).unwrap();
    assert_eq!(first["thread"], "feature/drop-in");

    git(&["checkout", "support/switch-me"], temp.path());
    std::fs::write(temp.path().join("switch.txt"), "switched").unwrap();

    let second: Value =
        serde_json::from_str(&heddle(&["status", "--json"], Some(temp.path())).unwrap()).unwrap();
    assert_eq!(second["thread"], "support/switch-me");
    assert!(
        second["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "switch.txt"),
        "dirty files should still be reported after branch switch: {second}"
    );

    // Import-hint information has moved to `heddle bridge git status
    // --json`; per-command outputs no longer carry it.
    let bridge_output = heddle(&["bridge", "git", "status", "--json"], Some(temp.path())).unwrap();
    let bridge: Value = serde_json::from_str(&bridge_output).unwrap();
    assert!(
        bridge["git_overlay_import_hint"]["missing_branches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "feature/drop-in"),
        "after switching branches, the old branch should become importable history: {bridge}"
    );
}

#[test]
fn test_cli_workspace_tracks_git_branch_switch_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/workspace-switch"], temp.path());

    let _ = heddle(&["workspace", "show", "--json"], Some(temp.path())).unwrap();
    git(&["checkout", "support/workspace-switch"], temp.path());

    let output = heddle(&["workspace", "show", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["current_thread"], "support/workspace-switch");
}

#[test]
fn test_cli_thread_list_tracks_git_branch_switch_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/thread-switch"], temp.path());

    let _ = heddle(&["thread", "list", "--json"], Some(temp.path())).unwrap();
    git(&["checkout", "support/thread-switch"], temp.path());

    let output = heddle(&["thread", "list", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["current"], "support/thread-switch");
}

#[test]
fn test_cli_status_handles_detached_head_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    let _ = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    git(&["checkout", "--detach", "HEAD"], temp.path());

    let output = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert!(
        parsed["thread"].is_null(),
        "detached HEAD should clear current thread: {parsed}"
    );
    assert!(
        parsed["git_overlay_import_hint"].is_null(),
        "detached HEAD should clear import hint after bootstrap too: {parsed}"
    );
}

#[test]
fn test_cli_bridge_git_import_clears_import_hint_for_existing_branches() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/import-me"], temp.path());

    let before: Value = serde_json::from_str(
        &heddle(&["bridge", "git", "status", "--json"], Some(temp.path())).unwrap(),
    )
    .unwrap();
    assert_eq!(before["git_overlay_import_hint"]["missing_branch_count"], 1);

    let import_output = heddle(&["bridge", "import", "--path", "."], Some(temp.path())).unwrap();
    let parsed_import: serde_json::Value =
        serde_json::from_str(&import_output).unwrap_or(serde_json::Value::Null);
    let synced = parsed_import["branches_synced"].as_u64().unwrap_or(0);
    assert!(
        synced >= 1 || import_output.contains("Synced") || import_output.contains("branches"),
        "bridge import should sync local branches: {import_output}"
    );

    let after: Value = serde_json::from_str(
        &heddle(&["bridge", "git", "status", "--json"], Some(temp.path())).unwrap(),
    )
    .unwrap();
    assert!(
        after["git_overlay_import_hint"].is_null(),
        "importing Git branches should clear the import hint: {after}"
    );

    let threads: Value =
        serde_json::from_str(&heddle(&["thread", "list", "--json"], Some(temp.path())).unwrap())
            .unwrap();
    assert!(
        threads["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["name"] == "support/import-me"),
        "thread list should include imported Git branch: {threads}"
    );
}

#[test]
fn test_cli_bridge_git_import_ref_imports_only_selected_branch() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/import-me"], temp.path());
    git(&["branch", "support/leave-alone"], temp.path());

    let import_output = heddle(
        &[
            "bridge",
            "import",
            "--path",
            ".",
            "--ref",
            "support/import-me",
        ],
        Some(temp.path()),
    )
    .unwrap();
    let parsed_import: Value = serde_json::from_str(&import_output).unwrap_or(Value::Null);
    assert!(
        parsed_import["branches_synced"].as_u64() == Some(1)
            || import_output.contains("Synced 1 branches to threads"),
        "ref-scoped import should sync only one branch: {import_output}"
    );

    let threads: Value =
        serde_json::from_str(&heddle(&["thread", "list", "--json"], Some(temp.path())).unwrap())
            .unwrap();
    assert!(
        threads["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["name"] == "support/import-me"
                && thread["history_imported"] == true),
        "selected branch should be imported: {threads}"
    );
    assert!(
        threads["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["name"] == "support/leave-alone"
                && thread["history_imported"] == false),
        "unselected branch should remain tip-only: {threads}"
    );
}

#[test]
fn test_cli_show_git_only_branch_tip_suggests_ref_scoped_import() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/git-only"], temp.path());

    let output = heddle(&["show", "support/git-only", "--json"], Some(temp.path()))
        .unwrap_err()
        .to_string();
    assert!(
        output.contains("heddle bridge git import --ref support/git-only"),
        "show should recommend a ref-scoped import for git-only branch tips: {output}"
    );
}

#[test]
fn test_cli_show_git_only_tag_suggests_ref_scoped_import() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["tag", "v1.0.0"], temp.path());

    let output = heddle(&["show", "v1.0.0", "--json"], Some(temp.path()))
        .unwrap_err()
        .to_string();
    assert!(
        output.contains("heddle bridge git import --ref v1.0.0"),
        "show should recommend a ref-scoped import for git-only tags: {output}"
    );
}

#[test]
fn test_cli_diff_git_only_branch_tip_suggests_ref_scoped_import() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/git-only"], temp.path());

    let output = heddle(
        &["diff", "HEAD", "support/git-only", "--json"],
        Some(temp.path()),
    )
    .unwrap_err()
    .to_string();
    assert!(
        output.contains("heddle bridge git import --ref support/git-only"),
        "diff should recommend a ref-scoped import for git-only branch tips: {output}"
    );
}

#[test]
fn test_cli_compare_git_only_tag_suggests_ref_scoped_import() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["tag", "v1.0.0"], temp.path());

    let output = heddle(&["compare", "HEAD", "v1.0.0", "--json"], Some(temp.path()))
        .unwrap_err()
        .to_string();
    assert!(
        output.contains("heddle bridge git import --ref v1.0.0"),
        "compare should recommend a ref-scoped import for git-only tags: {output}"
    );
}

#[test]
fn test_cli_thread_list_marks_tip_only_branch_with_ref_scoped_import_action() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/git-only"], temp.path());

    let threads: Value =
        serde_json::from_str(&heddle(&["thread", "list", "--json"], Some(temp.path())).unwrap())
            .unwrap();
    let thread = threads["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["name"] == "support/git-only")
        .expect("support/git-only should be visible as a tip-only mirror");
    assert_eq!(thread["history_imported"], false);
    assert_eq!(thread["thread_health"], "tip_only");
    assert_eq!(
        thread["recommended_action"],
        "heddle bridge git import --ref support/git-only"
    );
}

#[test]
fn test_cli_bridge_git_import_ref_imports_only_selected_tag() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["tag", "v1.0.0"], temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked next").unwrap();
    git_commit_all(temp.path(), "second commit");
    git(&["tag", "v2.0.0"], temp.path());

    let import_output = heddle(
        &["bridge", "import", "--path", ".", "--ref", "v1.0.0"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed_import: Value = serde_json::from_str(&import_output).unwrap_or(Value::Null);
    assert!(
        parsed_import["tags_synced"].as_u64() == Some(1)
            || import_output.contains("Synced 1 tags to markers"),
        "expected selected tag import output: {import_output}"
    );

    let v1 = heddle(&["show", "v1.0.0", "--json"], Some(temp.path())).unwrap();
    let parsed_v1: Value = serde_json::from_str(&v1).unwrap();
    assert!(parsed_v1["change_id"].as_str().is_some());

    let v2_err = heddle(&["show", "v2.0.0", "--json"], Some(temp.path()))
        .unwrap_err()
        .to_string();
    assert!(
        v2_err.contains("heddle bridge git import --ref v2.0.0"),
        "unselected tag should remain import-only: {v2_err}"
    );
}

#[test]
fn test_cli_bridge_git_import_defaults_to_current_repo_even_after_mirror_exists() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    heddle(&["bridge", "import", "--path", "."], Some(temp.path())).unwrap();

    git(&["branch", "support/import-latest"], temp.path());

    let import_output = heddle(&["bridge", "import"], Some(temp.path())).unwrap();
    let parsed_import: Value = serde_json::from_str(&import_output).unwrap_or(Value::Null);
    let synced = parsed_import["branches_synced"].as_u64().unwrap_or(0);
    assert!(
        synced >= 1 || import_output.contains("Synced") || import_output.contains("branches"),
        "expected live current repo import, not stale mirror import: {import_output}"
    );

    let threads: Value =
        serde_json::from_str(&heddle(&["thread", "list", "--json"], Some(temp.path())).unwrap())
            .unwrap();
    assert!(
        threads["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["name"] == "support/import-latest"
                && thread["history_imported"] == true),
        "default import should read the current repo and pick up the latest branch: {threads}"
    );
}

#[test]
fn test_cli_diagnose_tracks_git_branch_switch_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/diagnose-switch"], temp.path());

    let _ = heddle(&["diagnose", "--json"], Some(temp.path())).unwrap();
    git(&["checkout", "support/diagnose-switch"], temp.path());
    std::fs::write(temp.path().join("diag.txt"), "dirty").unwrap();

    let output = heddle(&["diagnose", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert_eq!(
        parsed["git_overlay_import_hint"]["missing_branches"][0],
        "feature/drop-in"
    );
    assert!(
        parsed["changes"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "diag.txt"),
        "diagnose should still reflect dirty state after branch switch: {parsed}"
    );
}

#[test]
fn test_cli_show_head_tracks_git_branch_switch_after_bootstrap() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/show-switch"], temp.path());

    let before: Value =
        serde_json::from_str(&heddle(&["show", "HEAD", "--json"], Some(temp.path())).unwrap())
            .unwrap();
    git(&["checkout", "support/show-switch"], temp.path());

    let output = heddle(&["show", "HEAD", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert!(parsed["change_id"].as_str().is_some());
    assert_ne!(
        parsed["change_id"], before["change_id"],
        "show HEAD should follow the switched Git branch, not stale bootstrap state: {parsed}"
    );
}

#[test]
fn test_cli_ready_captures_current_git_branch_after_switch() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");
    git(&["branch", "support/ready-switch"], temp.path());

    let _ = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    git(&["checkout", "support/ready-switch"], temp.path());
    std::fs::write(temp.path().join("ready.txt"), "capture me").unwrap();

    let ready: Value =
        serde_json::from_str(&heddle(&["--json", "ready"], Some(temp.path())).unwrap()).unwrap();
    assert_eq!(ready["captured"], true);

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--json"], Some(temp.path())).unwrap()).unwrap();
    assert_eq!(status["thread"], "support/ready-switch");
    assert!(status["state"]["change_id"].as_str().is_some());
    assert!(status["changes"]["added"].as_array().unwrap().is_empty());
}

#[test]
fn test_cli_workspace_surfaces_git_import_hint_in_text_output() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("tracked.txt"), "tracked").unwrap();
    git_commit_all(temp.path(), "seed branch");

    let status = Command::new("git")
        .args(["branch", "support/import-me"])
        .current_dir(temp.path())
        .status()
        .expect("git branch should run");
    assert!(status.success());

    let output = heddle(&["workspace", "show"], Some(temp.path())).unwrap();
    assert!(
        output.contains("support/import-me"),
        "missing branch hint: {output}"
    );
    assert!(
        output.contains("heddle bridge git import"),
        "missing import command: {output}"
    );
}

#[test]
fn test_cli_init_with_principal() {
    let temp = TempDir::new().unwrap();

    let result = heddle(
        &[
            "init",
            "--principal-name",
            "Test User",
            "--principal-email",
            "test@example.com",
        ],
        Some(temp.path()),
    );
    assert!(result.is_ok());

    let config_path = temp.path().join(".heddle-user/config.toml");
    let config = UserConfig::load(&config_path).unwrap();
    let principal = config.principal.expect("principal should be set");
    assert_eq!(principal.name, "Test User");
    assert_eq!(principal.email, "test@example.com");
}

#[test]
fn test_cli_status_on_empty_repo() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle(&["status"], Some(temp.path())).unwrap();
    assert!(
        output.contains("On thread: main") || output.contains("main"),
        "Should show current thread"
    );
}

#[test]
fn test_cli_status_shows_untracked_files() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("test.txt"), "hello").unwrap();

    let output = heddle(&["status"], Some(temp.path())).unwrap();
    assert!(
        output.contains("test.txt") || output.contains("added") || output.contains("untracked"),
        "Should show untracked file: {}",
        output
    );
}

#[test]
fn test_cli_snapshot_creates_state() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("hello.txt"), "world").unwrap();

    let output = heddle(&["capture", "-m", "Initial commit"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Created state") || output.contains("hd-"),
        "Should show created state: {}",
        output
    );
}

/// Concurrent writers on the same checkout must not race the Git
/// index. We assert that:
///
///   1. A leftover `index.lock` causes `heddle checkpoint` to bail
///      with the structured `IndexAlreadyDirty` skip reason instead
///      of clobbering the index.
///   2. Once the lock is removed, the next checkpoint succeeds and
///      cleans up its own lock (no stale `.git/index.lock` remains).
#[test]
fn test_cli_checkpoint_skips_when_git_index_is_locked() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("hello.txt"), "world").unwrap();
    heddle(&["capture", "-m", "initial"], Some(temp.path())).unwrap();

    // Simulate another writer holding the canonical Git index lock.
    let lock_path = temp.path().join(".git").join("index.lock");
    std::fs::write(&lock_path, b"").unwrap();

    let blocked = heddle(
        &["--json", "checkpoint", "-m", "blocked checkpoint"],
        Some(temp.path()),
    )
    .expect_err("checkpoint must refuse to write through a locked index");
    assert!(
        blocked.contains("locked") || blocked.contains("index"),
        "checkpoint must explain the index-lock conflict: {blocked}"
    );
    assert!(
        lock_path.exists(),
        "checkpoint must not delete an externally-held index.lock"
    );

    // Drop the foreign lock and retry. Subsequent checkpoint should
    // succeed and tidy its own index.lock so the directory is left
    // clean.
    std::fs::remove_file(&lock_path).unwrap();
    heddle(
        &["checkpoint", "-m", "post-unlock checkpoint"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        !lock_path.exists(),
        "successful checkpoint must release its index.lock; found leftover at {}",
        lock_path.display()
    );
}

#[test]
fn test_cli_checkpoint_creates_git_commit_and_records_mapping() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("hello.txt"), "world").unwrap();

    heddle(
        &["capture", "-m", "Initial overlay capture"],
        Some(temp.path()),
    )
    .unwrap();
    let output = heddle(
        &["checkpoint", "-m", "Initial Git checkpoint"],
        Some(temp.path()),
    )
    .unwrap();
    let checkpoint: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(checkpoint["summary"], "Initial Git checkpoint");

    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(temp.path())
        .output()
        .expect("git rev-parse should run");
    assert!(head.status.success());
    let git_commit = String::from_utf8(head.stdout).unwrap().trim().to_string();
    assert!(!git_commit.is_empty());

    // Heddle now records change_id provenance on `refs/notes/heddle`
    // inside the bridge mirror at `.heddle/git/` rather than rewriting
    // commit messages with a `Heddle-Change:` trailer — that keeps Git
    // commit SHAs stable across heddle imports/exports. The bridge
    // also mirrors `refs/notes/heddle` from the bridge mirror back
    // into the user's own `.git/` on every checkpoint, so plain
    // `git notes show` from the working directory works without
    // `--git-dir` poking inside `.heddle/`.
    let notes = Command::new("git")
        .args(["notes", "--ref=refs/notes/heddle", "show", &git_commit])
        .current_dir(temp.path())
        .output()
        .expect("git notes show should run");
    assert!(
        notes.status.success(),
        "expected refs/notes/heddle in the user's .git/ to record the checkpoint commit; stderr: {}",
        String::from_utf8_lossy(&notes.stderr)
    );
    let note_body = String::from_utf8(notes.stdout).unwrap();
    assert!(
        note_body.contains("hd-"),
        "note body should embed a Heddle change id: {note_body}"
    );

    let status = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(parsed["git_checkpoint"]["git_commit"], git_commit);
}

#[test]
fn test_cli_checkpoint_bootstraps_current_state_in_plain_git_repo() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("checkpoint.txt"), "checkpoint me").unwrap();

    let _output = heddle(
        &["checkpoint", "-m", "Bootstrap Git checkpoint"],
        Some(temp.path()),
    )
    .unwrap();

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--json"], Some(temp.path())).unwrap()).unwrap();
    assert!(status["state"]["change_id"].as_str().is_some());
    assert!(status["git_checkpoint"]["git_commit"].as_str().is_some());
}

#[test]
fn test_cli_ready_in_git_overlay_auto_captures_initial_state() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("ready.txt"), "capture me").unwrap();

    let ready: Value =
        serde_json::from_str(&heddle(&["--json", "ready"], Some(temp.path())).unwrap()).unwrap();
    assert_eq!(ready["captured"], true);

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--json"], Some(temp.path())).unwrap()).unwrap();
    assert!(status["state"]["change_id"].as_str().is_some());
    assert!(status["git_checkpoint"].is_null());
}

#[test]
fn test_cli_start_bootstraps_current_state_in_plain_git_repo() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("start.txt"), "start from git").unwrap();

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--json",
                "start",
                "feature/overlay-thread",
                "--workspace",
                "private",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(started["name"], "feature/overlay-thread");
    assert!(started["execution_path"].as_str().is_some());

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--json"], Some(temp.path())).unwrap()).unwrap();
    assert!(status["state"]["change_id"].as_str().is_some());
}

#[test]
fn test_cli_marker_create_bootstraps_current_state_in_plain_git_repo() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("marker.txt"), "mark me").unwrap();

    let output = heddle(&["marker", "create", "bootstrap-marker"], Some(temp.path())).unwrap();
    assert!(output.contains("bootstrap-marker"));

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--json"], Some(temp.path())).unwrap()).unwrap();
    assert!(status["state"]["change_id"].as_str().is_some());
}

#[test]
fn test_cli_thread_create_bootstraps_current_state_in_plain_git_repo() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("thread.txt"), "thread me").unwrap();

    let output = heddle(
        &["thread", "create", "feature/create-thread"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(output.contains("feature/create-thread"));

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--json"], Some(temp.path())).unwrap()).unwrap();
    assert!(status["state"]["change_id"].as_str().is_some());
}

#[test]
fn test_cli_show_head_bootstraps_current_state_in_plain_git_repo() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("show.txt"), "show me").unwrap();

    let output = heddle(&["--json", "show", "HEAD"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["repository_capability"], "git-overlay");
    assert!(parsed["change_id"].as_str().is_some());
}

#[test]
fn test_cli_log_bootstraps_current_state_in_plain_git_repo() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("log.txt"), "log me").unwrap();

    let output = heddle(&["log", "--oneline"], Some(temp.path())).unwrap();
    assert!(output.contains("Bootstrap git-overlay"));
}

#[test]
fn test_cli_ship_in_git_overlay_auto_checkpoints() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "Bootstrap"], Some(temp.path())).unwrap();

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--json",
                "start",
                "feature/ship-it",
                "--workspace",
                "private",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let thread = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread.join("ship.txt"), "ship me").unwrap();

    let shipped: Value = serde_json::from_str(
        &heddle(
            &["--json", "ship", "--thread", "feature/ship-it"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(shipped["status"], "shipped");
    assert_eq!(shipped["checkpointed"], true);
    assert!(shipped["git_commit"].as_str().is_some());
    assert!(temp.path().join("ship.txt").exists());

    let status: Value =
        serde_json::from_str(&heddle(&["status", "--json"], Some(temp.path())).unwrap()).unwrap();
    assert!(status["git_checkpoint"]["git_commit"].as_str().is_some());
}

#[test]
fn test_parallel_heddle_threads_capture_independently_and_checkpoint_via_git_overlay_root() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "Bootstrap"], Some(temp.path())).unwrap();

    let auth_started: Value = serde_json::from_str(
        &heddle(
            &["--json", "start", "feature/auth", "--workspace", "private"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let search_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--json",
                "start",
                "feature/search",
                "--workspace",
                "private",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();

    let auth_path = std::path::PathBuf::from(auth_started["execution_path"].as_str().unwrap());
    let search_path = std::path::PathBuf::from(search_started["execution_path"].as_str().unwrap());

    std::fs::write(auth_path.join("auth.rs"), "auth v1").unwrap();
    heddle(&["capture", "-m", "auth v1"], Some(&auth_path)).unwrap();
    std::fs::write(auth_path.join("auth.rs"), "auth v2").unwrap();
    let auth_capture: Value = serde_json::from_str(
        &heddle(&["--json", "capture", "-m", "auth v2"], Some(&auth_path)).unwrap(),
    )
    .unwrap();

    std::fs::write(search_path.join("search.rs"), "search v1").unwrap();
    heddle(&["capture", "-m", "search v1"], Some(&search_path)).unwrap();
    std::fs::write(search_path.join("search.rs"), "search v2").unwrap();
    let search_capture: Value = serde_json::from_str(
        &heddle(
            &["--json", "capture", "-m", "search v2"],
            Some(&search_path),
        )
        .unwrap(),
    )
    .unwrap();

    let auth_thread: Value = serde_json::from_str(
        &heddle(&["--json", "inspect", "feature/auth"], Some(temp.path())).unwrap(),
    )
    .unwrap();
    let search_thread: Value = serde_json::from_str(
        &heddle(&["--json", "inspect", "feature/search"], Some(temp.path())).unwrap(),
    )
    .unwrap();
    assert_eq!(
        auth_thread["current_state"].as_str().unwrap(),
        auth_capture["change_id"].as_str().unwrap()
    );
    assert_eq!(
        search_thread["current_state"].as_str().unwrap(),
        search_capture["change_id"].as_str().unwrap()
    );

    let auth_checkpoint_err = heddle(
        &["checkpoint", "-m", "auth direct checkpoint"],
        Some(&auth_path),
    )
    .unwrap_err();
    assert!(
        auth_checkpoint_err.contains("Git-backed repositories")
            || auth_checkpoint_err.contains("git-backed repositories"),
        "isolated auth thread should reject direct checkpoint: {auth_checkpoint_err}"
    );
    let search_checkpoint_err = heddle(
        &["checkpoint", "-m", "search direct checkpoint"],
        Some(&search_path),
    )
    .unwrap_err();
    assert!(
        search_checkpoint_err.contains("Git-backed repositories")
            || search_checkpoint_err.contains("git-backed repositories"),
        "isolated search thread should reject direct checkpoint: {search_checkpoint_err}"
    );

    let auth_ship: Value = serde_json::from_str(
        &heddle(
            &["--json", "ship", "--thread", "feature/auth"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(auth_ship["status"], "shipped");
    assert_eq!(auth_ship["checkpointed"], true);
    assert!(auth_ship["git_commit"].as_str().is_some());

    let search_ship: Value = serde_json::from_str(
        &heddle(
            &["--json", "ship", "--thread", "feature/search"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(search_ship["status"], "shipped");
    assert_eq!(search_ship["checkpointed"], true);
    assert!(search_ship["git_commit"].as_str().is_some());

    assert!(temp.path().join("auth.rs").exists());
    assert!(temp.path().join("search.rs").exists());

    let checkpoint_records_path = temp.path().join(".heddle/state/git-checkpoints.json");
    let checkpoint_records: Value =
        serde_json::from_str(&std::fs::read_to_string(checkpoint_records_path).unwrap()).unwrap();
    let records = checkpoint_records.as_array().unwrap();
    assert!(
        records.len() >= 2,
        "expected at least two git checkpoint records after shipping both threads: {checkpoint_records}"
    );
    assert!(
        records
            .iter()
            .any(|record| record["summary"] == "Ship feature/auth"),
        "shipping auth should create its own git checkpoint record: {checkpoint_records}"
    );
    assert!(
        records
            .iter()
            .any(|record| record["summary"] == "Ship feature/search"),
        "shipping search should create its own git checkpoint record: {checkpoint_records}"
    );
    assert_ne!(
        auth_ship["git_commit"], search_ship["git_commit"],
        "separate shipped threads should produce distinct git commits"
    );

    // Each shipped thread should record a Heddle change id on the
    // `refs/notes/heddle` ref. The bridge write-through mirrors
    // notes from the bridge mirror at `.heddle/git/` back into the
    // user's own `.git/refs/notes/heddle`, so plain `git notes show`
    // from the working dir resolves them.
    for git_commit in [
        auth_ship["git_commit"].as_str().unwrap(),
        search_ship["git_commit"].as_str().unwrap(),
    ] {
        let notes = Command::new("git")
            .args(["notes", "--ref=refs/notes/heddle", "show", git_commit])
            .current_dir(temp.path())
            .output()
            .expect("git notes show should run");
        assert!(
            notes.status.success(),
            "shipped commit {git_commit} should have a heddle note in user .git/; stderr: {}",
            String::from_utf8_lossy(&notes.stderr)
        );
        let note_body = String::from_utf8(notes.stdout).unwrap();
        assert!(
            note_body.contains("hd-"),
            "note for {git_commit} should embed a Heddle change id: {note_body}"
        );
    }
}

#[test]
fn test_parallel_heddle_threads_ship_with_one_stale_refresh_path_and_checkpoint_both() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    git_commit_all(temp.path(), "seed branch");
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "Bootstrap"], Some(temp.path())).unwrap();

    let auth_started: Value = serde_json::from_str(
        &heddle(
            &["--json", "start", "feature/auth", "--workspace", "private"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let auth_path = std::path::PathBuf::from(auth_started["execution_path"].as_str().unwrap());
    std::fs::write(auth_path.join("auth.rs"), "auth work").unwrap();
    heddle(&["capture", "-m", "auth work"], Some(&auth_path)).unwrap();

    std::fs::write(temp.path().join("base.txt"), "base advanced").unwrap();
    heddle(&["capture", "-m", "advance main"], Some(temp.path())).unwrap();

    let auth_before_ship: Value = serde_json::from_str(
        &heddle(
            &["--json", "thread", "show", "feature/auth"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(auth_before_ship["freshness"], "stale");

    let search_started: Value = serde_json::from_str(
        &heddle(
            &[
                "--json",
                "start",
                "feature/search",
                "--workspace",
                "private",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let search_path = std::path::PathBuf::from(search_started["execution_path"].as_str().unwrap());
    std::fs::write(search_path.join("search.rs"), "search work").unwrap();
    heddle(&["capture", "-m", "search work"], Some(&search_path)).unwrap();

    let auth_ship: Value = serde_json::from_str(
        &heddle(
            &["--json", "ship", "--thread", "feature/auth"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(auth_ship["status"], "shipped");
    assert_eq!(auth_ship["checkpointed"], true);
    assert!(auth_ship["git_commit"].as_str().is_some());

    let search_ship: Value = serde_json::from_str(
        &heddle(
            &["--json", "ship", "--thread", "feature/search"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(search_ship["status"], "shipped");
    assert_eq!(search_ship["synced"], false);
    assert_eq!(search_ship["checkpointed"], true);
    assert!(search_ship["git_commit"].as_str().is_some());

    let auth_thread: Value = serde_json::from_str(
        &heddle(
            &["--json", "thread", "show", "feature/auth"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(auth_thread["thread_state"], "merged");
    assert_eq!(
        auth_thread["integration_policy_result"]["status"],
        "auto_integrated"
    );

    let search_thread: Value = serde_json::from_str(
        &heddle(
            &["--json", "thread", "show", "feature/search"],
            Some(temp.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(search_thread["thread_state"], "merged");
    assert_eq!(
        search_thread["integration_policy_result"]["status"],
        "auto_integrated"
    );

    let checkpoint_records_path = temp.path().join(".heddle/state/git-checkpoints.json");
    let checkpoint_records: Value =
        serde_json::from_str(&std::fs::read_to_string(checkpoint_records_path).unwrap()).unwrap();
    let records = checkpoint_records.as_array().unwrap();
    assert!(
        records
            .iter()
            .any(|record| record["summary"] == "Ship feature/auth"),
        "stale auth ship should record a git checkpoint: {checkpoint_records}"
    );
    assert!(
        records
            .iter()
            .any(|record| record["summary"] == "Ship feature/search"),
        "clean search ship should record a git checkpoint: {checkpoint_records}"
    );
}

#[test]
fn test_cli_push_rejects_local_only_git_overlay_repo() {
    let temp = TempDir::new().unwrap();
    init_git_repo(temp.path());
    heddle(&["init"], Some(temp.path())).unwrap();

    // Fresh `git init` repos have no remote. `heddle push` should
    // fail with a clear pointer back at the missing destination
    // rather than silently no-op'ing.
    let err = heddle(&["push"], Some(temp.path())).unwrap_err();
    assert!(
        err.contains("destination") && err.contains("origin"),
        "expected guidance about the missing remote, got: {err}"
    );
}

#[test]
fn test_cli_snapshot_no_agent_ignores_corrupt_session_state() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::create_dir_all(temp.path().join(".heddle/state")).unwrap();
    std::fs::write(
        temp.path().join(".heddle/state/worktree.toml"),
        "not = valid = toml",
    )
    .unwrap();
    std::fs::write(temp.path().join("hello.txt"), "world").unwrap();

    let output = heddle(
        &["capture", "--no-agent", "-m", "Human snapshot"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        output.contains("Created state") || output.contains("hd-"),
        "human snapshot should not require session state: {}",
        output
    );
}

#[test]
fn test_cli_snapshot_with_confidence() {
    let temp = TempDir::new().unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();

    unsafe {
        std::env::set_var("HEDDLE_AGENT_PROVIDER", "test");
        std::env::set_var("HEDDLE_AGENT_MODEL", "test-model");
    }

    let result = heddle(
        &[
            "capture",
            "--intent",
            "Test with confidence",
            "--confidence",
            "0.95",
        ],
        Some(temp.path()),
    );

    unsafe {
        std::env::remove_var("HEDDLE_AGENT_PROVIDER");
        std::env::remove_var("HEDDLE_AGENT_MODEL");
    }

    assert!(result.is_ok());
}

#[test]
fn test_cli_snapshot_without_confidence_records_none() {
    let temp = TempDir::new().unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();

    let output = heddle(
        &["capture", "--intent", "Test without confidence"],
        Some(temp.path()),
    )
    .unwrap();
    let snapshot_json: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert!(
        snapshot_json["confidence"].is_null(),
        "snapshot output should expose absent confidence as null: {snapshot_json:#}"
    );

    let change_id = snapshot_json["change_id"].as_str().unwrap();
    let show_json = heddle(&["show", "--json", change_id], Some(temp.path())).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&show_json).unwrap();
    assert!(
        parsed["confidence"].is_null(),
        "omitted confidence should be stored as null: {parsed:#}"
    );
}

#[test]
fn test_cli_log_shows_history() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 1..=3 {
        std::fs::write(
            temp.path().join(format!("file{}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("Commit {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    let output = heddle(&["log"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Commit 1") || output.contains("hd-"),
        "Should show commits: {}",
        output
    );
}

#[test]
fn test_cli_log_with_limit() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    for i in 1..=5 {
        std::fs::write(
            temp.path().join(format!("file{}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("Commit {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    assert!(heddle(&["log", "--limit", "2"], Some(temp.path())).is_ok());
}

#[test]
fn test_cli_log_limit_caps_json_state_count() {
    // `--limit N` must trim the JSON `states` array to at most N
    // entries, regardless of how much history exists.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    for i in 1..=6 {
        std::fs::write(
            temp.path().join(format!("file{}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("Commit {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    let json = heddle(&["--json", "log", "--limit", "3"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    let states = parsed["states"].as_array().expect("states array");
    assert!(
        states.len() <= 3,
        "`--limit 3` should return at most 3 states, got {}: {}",
        states.len(),
        json
    );
}

#[test]
fn test_cli_log_since_marker_excludes_marker_and_walks_back() {
    // `--since <marker>` walks back until it reaches the marker's
    // state, then returns everything *above* it (newer than the
    // marker, excluding the marker's state itself).
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    // Capture three pre-marker states.
    for i in 1..=3 {
        std::fs::write(
            temp.path().join(format!("pre{}.txt", i)),
            format!("pre {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("Pre-marker {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    // Drop the marker at the current HEAD.
    heddle(&["marker", "create", "checkpoint"], Some(temp.path())).unwrap();

    // Capture two post-marker states.
    for i in 1..=2 {
        std::fs::write(
            temp.path().join(format!("post{}.txt", i)),
            format!("post {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("Post-marker {}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    let json = heddle(
        &["--json", "log", "--since", "checkpoint"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    let states = parsed["states"].as_array().expect("states array");

    // We expect exactly the two post-marker captures.
    assert_eq!(
        states.len(),
        2,
        "`--since checkpoint` should return 2 post-marker states, got: {}",
        json
    );
    let intents: Vec<&str> = states
        .iter()
        .map(|s| s["intent"].as_str().unwrap_or(""))
        .collect();
    assert!(
        intents.iter().any(|i| i.contains("Post-marker 2")),
        "should include Post-marker 2: {:?}",
        intents
    );
    assert!(
        !intents.iter().any(|i| i.contains("Pre-marker")),
        "should not include any Pre-marker states: {:?}",
        intents
    );
}

#[test]
fn test_cli_log_since_with_limit_applies_bound_then_trims() {
    // When `--since` and `--limit` are both set, the bound is applied
    // first (yielding "everything newer than the marker"), then the
    // result is trimmed to `--limit`. So `--limit 2 --since <marker>`
    // returns at most 2 entries even if more captures exist above the
    // bound.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["marker", "create", "start"], Some(temp.path())).unwrap();

    for i in 1..=4 {
        std::fs::write(
            temp.path().join(format!("after{}.txt", i)),
            format!("after {}", i),
        )
        .unwrap();
        heddle(
            &["capture", "-m", &format!("After-{}", i)],
            Some(temp.path()),
        )
        .unwrap();
    }

    let json = heddle(
        &["--json", "log", "--since", "start", "--limit", "2"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    let states = parsed["states"].as_array().expect("states array");
    assert_eq!(
        states.len(),
        2,
        "`--limit 2 --since start` should return exactly 2 states, got: {}",
        json
    );
}

#[test]
fn test_cli_show_state_details() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("test.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Test state"], Some(temp.path())).unwrap();

    let output = heddle(&["show", "HEAD"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Test state") || output.contains("hd-"),
        "Should show state details: {}",
        output
    );
}

#[test]
fn test_cli_diff_shows_changes() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("file.txt"), "original").unwrap();
    heddle(&["capture", "-m", "Original"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "modified").unwrap();

    let output = heddle(&["diff"], Some(temp.path())).unwrap();
    assert!(
        output.contains("file.txt") || output.contains("modified") || output.contains("diff"),
        "Diff should show changes: {}",
        output
    );
}

#[test]
fn test_cli_diff_renders_unified_hunks_with_three_context_lines_by_default() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let original = (1..=9)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(temp.path().join("file.txt"), original).unwrap();
    heddle(&["capture", "-m", "Original"], Some(temp.path())).unwrap();

    let modified = (1..=9)
        .map(|line| {
            if line == 5 {
                "line five changed".to_string()
            } else {
                format!("line {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(temp.path().join("file.txt"), modified).unwrap();

    let output = heddle(&["--output", "text", "diff"], Some(temp.path())).unwrap();
    assert!(
        output.contains("@@"),
        "diff should include hunk headers: {output}"
    );
    assert!(
        output.contains(" line 2") && output.contains(" line 8"),
        "default unified diff should include three surrounding context lines: {output}"
    );
    assert!(
        !output.contains(" line 1") && !output.contains(" line 9"),
        "default unified diff should omit context outside the hunk: {output}"
    );
    assert!(
        output.contains("-line 5") && output.contains("+line five changed"),
        "no-color diff should preserve explicit old/new lines: {output}"
    );

    let tight = heddle(
        &["--output", "text", "diff", "--unified", "1"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        tight.contains(" line 4") && tight.contains(" line 6"),
        "--unified 1 should include one surrounding line: {tight}"
    );
    assert!(
        !tight.contains(" line 3") && !tight.contains(" line 7"),
        "--unified 1 should omit farther context: {tight}"
    );
}

#[cfg(feature = "semantic")]
#[test]
fn test_cli_diff_semantic_still_renders_text_hunks() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::create_dir_all(temp.path().join("src")).unwrap();
    std::fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn answer() -> i32 {\n    41\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Original"], Some(temp.path())).unwrap();

    std::fs::write(
        temp.path().join("src/lib.rs"),
        "pub fn answer() -> i32 {\n    42\n}\n",
    )
    .unwrap();

    let output = heddle(
        &["--output", "text", "diff", "--semantic"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        output.contains("--- a/src/lib.rs"),
        "missing file header: {output}"
    );
    assert!(
        output.contains("@@"),
        "semantic diff should include hunks: {output}"
    );
    assert!(output.contains("-    41"), "missing removed line: {output}");
    assert!(output.contains("+    42"), "missing added line: {output}");
    assert!(
        !output.contains("Binary file or unable to diff"),
        "semantic text diff should not fall back to binary message: {output}"
    );
}

#[test]
fn test_cli_diff_color_renders_modified_lines_as_single_tilde_row() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("file.txt"), "let value = 41;\n").unwrap();
    heddle(&["capture", "-m", "Original"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "let value = 42;\n").unwrap();

    let output = heddle_output_with_env(
        &["--output", "text", "diff", "--unified", "0"],
        Some(temp.path()),
        &[("CLICOLOR_FORCE", "1")],
    )
    .unwrap();
    assert!(output.status.success());
    let stdout = str::from_utf8(&output.stdout).unwrap_or("");
    assert!(
        stdout.contains("\x1b["),
        "forced color should emit ANSI: {stdout:?}"
    );
    assert!(
        stdout.contains("~") && !stdout.contains(" -> "),
        "colored modified line should be a single tilde row without arrow text: {stdout:?}"
    );
    assert!(
        !stdout.contains("-let value = 41;") && !stdout.contains("+let value = 42;"),
        "colored modified line should not render as delete/add churn: {stdout:?}"
    );
}

#[test]
fn test_cli_diff_stat_only() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Original"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("file.txt"), "modified").unwrap();

    assert!(heddle(&["diff", "--stat"], Some(temp.path())).is_ok());
}

#[test]
fn test_cli_goto_changes_worktree() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("version.txt"), "v1").unwrap();
    heddle(&["capture", "-m", "Version 1"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("version.txt"), "v2").unwrap();
    heddle(&["capture", "-m", "Version 2"], Some(temp.path())).unwrap();

    let content = std::fs::read_to_string(temp.path().join("version.txt")).unwrap();
    assert_eq!(content, "v2");

    assert!(heddle(&["goto", "HEAD~1"], Some(temp.path())).is_ok());
    let content = std::fs::read_to_string(temp.path().join("version.txt")).unwrap();
    assert_eq!(content, "v1", "File should be restored to v1");
}

#[test]
fn test_cli_undo_redo() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "State 1"], Some(temp.path())).unwrap();
    let head_after_first = status_json(temp.path());
    let first_id = head_after_first["state"]["change_id"]
        .as_str()
        .expect("state change_id should be string")
        .to_string();
    assert_eq!(head_after_first["thread"].as_str().unwrap_or(""), "main");

    std::fs::write(temp.path().join("file.txt"), "updated").unwrap();
    heddle(&["capture", "-m", "State 2"], Some(temp.path())).unwrap();
    let head_after_second = status_json(temp.path());
    let second_id = head_after_second["state"]["change_id"]
        .as_str()
        .expect("state change_id should be string")
        .to_string();

    assert!(heddle(&["undo"], Some(temp.path())).is_ok());
    let head_after_undo = status_json(temp.path());
    let undo_id = head_after_undo["state"]["change_id"]
        .as_str()
        .expect("state change_id should be string")
        .to_string();
    assert_eq!(head_after_undo["thread"].as_str().unwrap_or(""), "main");
    assert_eq!(undo_id, first_id, "Undo should move to previous state");

    assert!(heddle(&["redo"], Some(temp.path())).is_ok());
    let head_after_redo = status_json(temp.path());
    let redo_id = head_after_redo["state"]["change_id"]
        .as_str()
        .expect("state change_id should be string")
        .to_string();
    assert_eq!(head_after_redo["thread"].as_str().unwrap_or(""), "main");
    assert_eq!(redo_id, second_id, "Redo should restore latest state");
}

/// `heddle show` and `heddle log` must distinguish an unset
/// confidence (`None`) from a low-confidence value. Render the
/// absent case as `Confidence: —` (em dash) and never as `0.00`,
/// which would silently lie about a value the agent never asserted.
///
/// We bypass the `cmd_snapshot` path on purpose: that path layers in
/// the user-config / repo-defaults fallback (0.8), so a `None`
/// confidence can only originate from a non-snapshot writer such as
/// the git bridge import. Putting the state directly via the object
/// store is the smallest reliable way to reproduce that scenario in
/// a CLI integration test.
#[test]
fn test_cli_show_renders_absent_confidence_as_em_dash() {
    use objects::object::{Attribution, Principal, State, Tree};
    use repo::Repository;

    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).expect("init repo");

    let tree = Tree::new();
    let tree_hash = repo.store().put_tree(&tree).expect("put tree");
    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
    let state = State::new(tree_hash, vec![], attribution).with_intent("imported state");
    assert!(
        state.confidence.is_none(),
        "fixture must have None confidence so the test exercises the absent branch",
    );
    repo.store().put_state(&state).expect("put state");
    let short_id = state.change_id.short();
    // Advance the seeded `main` thread to our `None`-confidence state so
    // `heddle log` (which walks from HEAD) actually traverses it.
    repo.refs()
        .set_thread("main", &state.change_id)
        .expect("set main thread");
    drop(repo);

    // Text mode: must show `Confidence: —`, never `Confidence: 0.00`.
    // The integration harness runs the binary as a subprocess, so the
    // auto-detect output format would otherwise pick JSON; force text.
    let show_text =
        heddle(&["--output", "text", "show", &short_id], Some(temp.path())).expect("heddle show");
    assert!(
        show_text.contains("Confidence: —"),
        "show should render an em dash for absent confidence; got:\n{show_text}"
    );
    assert!(
        !show_text.contains("Confidence: 0.00"),
        "show must not render absent confidence as 0.00; got:\n{show_text}"
    );
    assert!(
        !show_text.contains("Confidence: 0%"),
        "show must not render absent confidence as 0%; got:\n{show_text}"
    );

    // JSON mode: an `Option<f32>` field with no `skip_serializing_if`
    // serializes as `null`, and the web app reads it via `?? null`.
    let show_json_str =
        heddle(&["--output", "json", "show", &short_id], Some(temp.path())).expect("show json");
    let show_json: serde_json::Value =
        serde_json::from_str(&show_json_str).expect("show JSON parses");
    assert!(
        show_json["confidence"].is_null(),
        "JSON confidence must be null for absent value, got {show_json:#}"
    );

    // `heddle log` is the high-density, multi-state surface: rendering
    // `Confidence: —` on every entry stacked a noise tax that hurt
    // readability without communicating new information (the absence of
    // a Confidence line already says "no value asserted"). The contract
    // it preserves is the same as `show`: never silently substitute a
    // numeric 0.00 / 0% for an unset confidence. JSON still serializes
    // `confidence: null` so agents distinguish the cases.
    let log_text = heddle(&["--output", "text", "log"], Some(temp.path())).expect("heddle log");
    assert!(
        !log_text.contains("Confidence: 0.00"),
        "log must not render absent confidence as 0.00; got:\n{log_text}"
    );
    assert!(
        !log_text.contains("Confidence: 0%"),
        "log must not render absent confidence as 0%; got:\n{log_text}"
    );
    // The state should still be visible — only the per-entry confidence
    // line is suppressed when unset.
    assert!(
        log_text.contains("imported state"),
        "the absent-confidence state should still appear in the log; got:\n{log_text}"
    );
}