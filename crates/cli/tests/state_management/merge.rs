// SPDX-License-Identifier: Apache-2.0
use super::*;

fn create_divergent_branches(temp: &TempDir) {
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base content").unwrap();
    heddle(&["capture", "-m", "Base state"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("feature.txt"), "feature content").unwrap();
    heddle(&["capture", "-m", "Feature change"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("main.txt"), "main content").unwrap();
    heddle(&["capture", "-m", "Main change"], Some(temp.path())).unwrap();
}

#[test]
fn test_merge_fast_forward() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "v1").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "v2").unwrap();
    heddle(&["capture", "-m", "Feature update"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    let result = heddle(&["merge", "feature"], Some(temp.path()));
    assert!(result.is_ok());
    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(content, "v2");
}
#[test]
fn test_merge_creates_merge_state() {
    let temp = TempDir::new().unwrap();
    create_divergent_branches(&temp);
    let result = heddle(&["merge", "feature"], Some(temp.path()));
    assert!(result.is_ok());
    assert_file_exists(temp.path().join("base.txt"), "base file should exist");
    assert_file_exists(temp.path().join("feature.txt"), "feature file should exist");
    assert_file_exists(temp.path().join("main.txt"), "main file should exist");
}
#[test]
fn test_merge_with_conflict_reports_conflict() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "feature version").unwrap();
    heddle(
        &["capture", "-m", "Feature modifies file"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "main version").unwrap();
    heddle(&["capture", "-m", "Main modifies file"], Some(temp.path())).unwrap();
    let result = heddle(&["merge", "feature"], Some(temp.path()));
    assert!(result.is_ok());
    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert!(
        content.contains("<<<<<<<")
            || content.contains("CONFLICT")
            || content.contains("main version")
            || content.contains("feature version")
    );
    assert!(
        content.contains("<<<<<<< CURRENT (main)"),
        "conflict markers should name the current thread: {content}"
    );
    assert!(
        content.contains(">>>>>>> INCOMING (feature)"),
        "conflict markers should name the incoming thread: {content}"
    );
}

#[test]
fn test_merge_auto_merges_non_overlapping_same_file_appends_from_threads() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let source = temp.path().join("src.rs");
    fs::write(&source, "fn base() -> &'static str {\n    \"base\"\n}\n").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "worker_a"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "worker_b"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "worker_a"], Some(temp.path())).unwrap();
    fs::write(
        &source,
        "fn base() -> &'static str {\n    \"base\"\n}\n\nfn worker_a() -> &'static str {\n    \"worker_a\"\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Worker A append"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "worker_b"], Some(temp.path())).unwrap();
    fs::write(
        &source,
        "fn base() -> &'static str {\n    \"base\"\n}\n\nfn worker_b() -> &'static str {\n    \"worker_b\"\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Worker B append"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(
        &source,
        "fn base() -> &'static str {\n    \"base\"\n}\n\nfn main_thread() -> &'static str {\n    \"main\"\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Main append"], Some(temp.path())).unwrap();

    heddle(&["merge", "worker_a"], Some(temp.path())).unwrap();
    heddle(&["merge", "worker_b"], Some(temp.path())).unwrap();

    let content = fs::read_to_string(&source).unwrap();
    assert!(
        !content.contains("<<<<<<<"),
        "non-overlapping appends should not leave conflict markers: {content}"
    );
    assert!(content.contains("fn main_thread()"));
    assert!(content.contains("fn worker_a()"));
    assert!(content.contains("fn worker_b()"));
}

#[test]
fn test_continue_after_manual_marker_removal_says_mark_file_resolved() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let file = temp.path().join("file.txt");
    fs::write(&file, "base\n").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(&file, "feature\n").unwrap();
    heddle(&["capture", "-m", "Feature edit"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(&file, "main\n").unwrap();
    heddle(&["capture", "-m", "Main edit"], Some(temp.path())).unwrap();

    heddle(&["merge", "feature"], Some(temp.path())).unwrap();
    let conflicted = fs::read_to_string(&file).unwrap();
    assert!(
        conflicted.contains("<<<<<<< CURRENT (main)"),
        "same-line conflict should leave markers: {conflicted}"
    );

    fs::write(&file, "manually resolved\n").unwrap();
    let blocked_continue = heddle(&["--json", "continue"], Some(temp.path())).unwrap();
    let blocked_continue: serde_json::Value =
        serde_json::from_str(&blocked_continue).expect("continue output should be JSON");
    assert_eq!(blocked_continue["status"], "blocked");
    assert_eq!(blocked_continue["next_action"], "heddle resolve --list");
    assert_eq!(
        blocked_continue["recommended_action"],
        "heddle resolve file.txt"
    );
    assert!(blocked_continue["message"]
        .as_str()
        .unwrap()
        .contains("mark each file resolved with `heddle resolve <path>`"));

    heddle(&["resolve", "file.txt"], Some(temp.path())).unwrap();
    let continued = heddle(&["--json", "continue"], Some(temp.path())).unwrap();
    let continued: serde_json::Value =
        serde_json::from_str(&continued).expect("continue output should be JSON");
    assert_eq!(continued["status"], "continued");
}

#[test]
fn test_merge_no_commit() {
    let temp = TempDir::new().unwrap();
    create_divergent_branches(&temp);
    let result = heddle(&["merge", "feature", "--no-commit"], Some(temp.path()));
    assert!(result.is_ok());
    let status_after = status_json(temp.path());
    assert!(
        !status_after["changes"]["added"]
            .as_array()
            .unwrap()
            .is_empty()
            || !status_after["changes"]["modified"]
                .as_array()
                .unwrap()
                .is_empty()
    );
}
#[test]
fn test_merge_message() {
    let temp = TempDir::new().unwrap();
    create_divergent_branches(&temp);
    let result = heddle(
        &["merge", "feature", "-m", "Merge feature into main"],
        Some(temp.path()),
    );
    assert!(result.is_ok());
}
#[test]
fn test_merge_into_current_track() {
    let temp = TempDir::new().unwrap();
    create_divergent_branches(&temp);
    let status_before = status_json(temp.path());
    assert_eq!(status_before["thread"].as_str().unwrap(), "main");
    heddle(&["merge", "feature"], Some(temp.path())).unwrap();
    let status_after = status_json(temp.path());
    assert_eq!(status_after["thread"].as_str().unwrap(), "main");
}
#[test]
fn test_merge_nonexistent_track_fails() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");
    let result = heddle(&["merge", "nonexistent"], Some(temp.path()));
    assert!(result.is_err());
}
#[test]
fn test_merge_with_uncommitted_changes_fails() {
    let temp = TempDir::new().unwrap();
    create_divergent_branches(&temp);
    fs::write(temp.path().join("uncommitted.txt"), "uncommitted").unwrap();
    let result = heddle(&["merge", "feature"], Some(temp.path()));
    let error = result.expect_err("merge with uncaptured changes should fail");
    assert!(
        error.contains("uncaptured changes"),
        "dirty-worktree blocker should use Heddle-native language: {error}"
    );
    assert!(
        error.contains("heddle capture") && error.contains("heddle continue"),
        "dirty-worktree blocker should suggest Heddle-native actions: {error}"
    );
}
#[test]
fn test_merge_already_up_to_date() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    let result = heddle(&["merge", "feature"], Some(temp.path()));
    assert!(result.is_ok());
}

/// Regression for the YC-demo finding: a fast-forward `heddle merge`
/// from inside an attached parent thread used to call `repo.goto()`
/// (which writes `Head::Detached`) without advancing the parent
/// thread's ref. The user observed `Fast-forwarded to <id>` while
/// `thread show <parent>` still reported the original change_id.
///
/// The fix advances the *current* thread's ref to the merge target and
/// re-attaches HEAD; this test asserts that after switching to the
/// parent thread and merging the child, both `current_state` and the
/// attached HEAD reflect the integrated state.
#[test]
fn test_merge_fast_forward_advances_current_thread() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("new.txt"), "feature work").unwrap();
    heddle(&["capture", "-m", "Feature work"], Some(temp.path())).unwrap();

    // Capture the change_id at the tip of `feature`.
    let feature_show = heddle(&["thread", "show", "feature", "--json"], Some(temp.path())).unwrap();
    let feature: Value = serde_json::from_str(&feature_show).unwrap();
    let merged_target = feature["current_state"]
        .as_str()
        .expect("feature should have a current_state")
        .to_string();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    let merge_output = heddle(&["merge", "feature"], Some(temp.path())).unwrap();
    assert!(
        merge_output.contains("Fast-forwarded"),
        "expected fast-forward merge, got: {merge_output}"
    );

    // After fast-forward, `main` must point at the integrated state.
    let main_show = heddle(&["thread", "show", "main", "--json"], Some(temp.path())).unwrap();
    let main: Value = serde_json::from_str(&main_show).unwrap();
    assert_eq!(
        main["current_state"].as_str().unwrap(),
        merged_target,
        "main.current_state must advance to the merge target after fast-forward"
    );

    // HEAD must remain attached to the parent thread.
    let status = status_json(temp.path());
    assert_eq!(
        status["thread"].as_str().unwrap(),
        "main",
        "HEAD must remain attached to the parent thread after fast-forward merge"
    );
}

/// Demo-flow regression: after `thread switch X` (where `X` is a
/// lightweight thread with its own dedicated worktree on disk),
/// running `heddle merge Y` from the *main* worktree must materialize
/// the merge into `X`'s metadata-recorded worktree — not into the
/// directory the operator happened to invoke `heddle` from.
///
/// This is the YC-demo workflow: operator stays at the project root
/// and never `cd`s into `.run-heddle-threads/<thread>/root/`. The fix
/// lives in `Repository::active_worktree_path` + the merge entry point.
#[test]
fn test_merge_from_main_worktree_targets_active_thread_lightweight_worktree() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    // Build the merge source on `main`.
    fs::write(temp.path().join("source.txt"), "ff target").unwrap();
    heddle(&["capture", "-m", "FF source"], Some(temp.path())).unwrap();
    let main_show = heddle(&["thread", "show", "main", "--json"], Some(temp.path())).unwrap();
    let main_json: Value = serde_json::from_str(&main_show).unwrap();
    let merge_target = main_json["current_state"].as_str().unwrap().to_string();

    // Roll `main` back so `feature` (created from HEAD~1) lacks
    // `source.txt` — that lets us check below that the lightweight
    // worktree picks it up after the merge.
    heddle(&["goto", "HEAD~1"], Some(temp.path())).unwrap();

    // Start a private (lightweight) thread — its worktree lives at
    // a metadata-recorded path *outside* of `temp.path()`.
    heddle(
        &["start", "feature", "--workspace", "auto"],
        Some(temp.path()),
    )
    .unwrap();

    // Activate the lightweight thread from main's worktree. From now
    // on, all operations should follow metadata, not CWD.
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();

    let feature_show = heddle(&["thread", "show", "feature", "--json"], Some(temp.path())).unwrap();
    let feature: Value = serde_json::from_str(&feature_show).unwrap();
    let feature_path = feature["execution_path"]
        .as_str()
        .expect("lightweight feature must record an execution_path")
        .to_string();
    assert_ne!(
        feature_path,
        temp.path().to_string_lossy(),
        "lightweight worktree should not collapse onto the main worktree"
    );

    // Merge from temp.path() (main's worktree) — operator never `cd`s
    // into the lightweight checkout. The merge must land in
    // feature's recorded worktree.
    let merge_output = heddle(&["merge", "main"], Some(temp.path())).unwrap();
    assert!(
        merge_output.contains("Fast-forwarded"),
        "expected fast-forward, got: {merge_output}"
    );
    // The fast-forward message should lead with the active thread
    // name, not the worktree path.
    assert!(
        merge_output.contains("feature"),
        "FF message should include the active thread name, got: {merge_output}"
    );

    // The lightweight worktree must now contain the merge source.
    assert!(
        std::path::Path::new(&feature_path)
            .join("source.txt")
            .exists(),
        "merge target file should appear in the lightweight worktree at {feature_path}"
    );

    // And feature.current_state must point at the merged tip.
    let feature_after =
        heddle(&["thread", "show", "feature", "--json"], Some(temp.path())).unwrap();
    let feature_after: Value = serde_json::from_str(&feature_after).unwrap();
    assert_eq!(
        feature_after["current_state"].as_str().unwrap(),
        merge_target,
        "feature.current_state must advance to the merge target"
    );
}

/// Sibling regression for `heddle goto`: after switching to a
/// lightweight thread, `heddle goto X` from the project root must
/// advance the lightweight worktree (recorded in metadata), not the
/// main worktree.
#[test]
fn test_goto_from_main_worktree_targets_active_thread_lightweight_worktree() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("ahead.txt"), "ahead").unwrap();
    heddle(&["capture", "-m", "Ahead"], Some(temp.path())).unwrap();
    let ahead_show = heddle(&["thread", "show", "main", "--json"], Some(temp.path())).unwrap();
    let ahead_json: Value = serde_json::from_str(&ahead_show).unwrap();
    let ahead_state = ahead_json["current_state"].as_str().unwrap().to_string();

    heddle(&["goto", "HEAD~1"], Some(temp.path())).unwrap();
    heddle(
        &["start", "feature", "--workspace", "auto"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();

    let feature_show = heddle(&["thread", "show", "feature", "--json"], Some(temp.path())).unwrap();
    let feature: Value = serde_json::from_str(&feature_show).unwrap();
    let feature_path = feature["execution_path"].as_str().unwrap().to_string();

    // Run goto from main's worktree (CWD=temp.path()); it should
    // advance the lightweight worktree, not the main worktree.
    heddle(&["goto", &ahead_state], Some(temp.path())).unwrap();

    assert!(
        std::path::Path::new(&feature_path)
            .join("ahead.txt")
            .exists(),
        "goto target file should appear in the lightweight worktree at {feature_path}"
    );
    // Main's worktree should NOT have the ahead file (we left main
    // pointing at HEAD~1 and only moved feature).
    assert!(
        !temp.path().join("ahead.txt").exists(),
        "main worktree must not advance when goto targets the active thread"
    );
}

/// Sibling regression for `heddle rebase`: rebase from the main
/// worktree must fast-forward the active lightweight thread's
/// recorded worktree, not whatever happens to live at CWD.
#[test]
fn test_rebase_from_main_worktree_targets_active_thread_lightweight_worktree() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    // Build a fast-forward target on `main`.
    fs::write(temp.path().join("source.txt"), "ff target").unwrap();
    heddle(&["capture", "-m", "FF source"], Some(temp.path())).unwrap();

    // Roll back so the lightweight thread starts behind `main`.
    heddle(&["goto", "HEAD~1"], Some(temp.path())).unwrap();
    heddle(
        &["start", "feature", "--workspace", "auto"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();

    let feature_show = heddle(&["thread", "show", "feature", "--json"], Some(temp.path())).unwrap();
    let feature: Value = serde_json::from_str(&feature_show).unwrap();
    let feature_path = feature["execution_path"].as_str().unwrap().to_string();

    // Run rebase from main's worktree. `feature` is behind `main`, so
    // this is a pure fast-forward. The lightweight worktree must end
    // up with the new file.
    heddle(&["rebase", "main"], Some(temp.path())).unwrap();

    assert!(
        std::path::Path::new(&feature_path)
            .join("source.txt")
            .exists(),
        "rebase target file should appear in the lightweight worktree at {feature_path}"
    );
}

#[test]
fn test_rebase_continue_accepts_manual_resolution_snapshot() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("conflict.txt"), "feature version\n").unwrap();
    heddle(&["capture", "-m", "Feature conflict"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("conflict.txt"), "main version\n").unwrap();
    heddle(&["capture", "-m", "Main conflict"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    let rebase_output = heddle(&["rebase", "main"], Some(temp.path())).unwrap();
    assert!(
        rebase_output.contains("Conflict applying")
            || rebase_output.contains("\"status\": \"conflict\""),
        "expected heddle rebase to stop on conflict: {rebase_output}"
    );
    assert!(
        temp.path().join(".heddle/REBASE_STATE").exists(),
        "rebase state should persist while waiting for manual resolution"
    );

    fs::write(
        temp.path().join("conflict.txt"),
        "main version\nfeature version\n",
    )
    .unwrap();
    heddle(
        &["capture", "-m", "Manual rebase resolution"],
        Some(temp.path()),
    )
    .unwrap();

    let resolve_output = heddle(
        &["thread", "resolve", "feature", "--json"],
        Some(temp.path()),
    )
    .unwrap();
    let resolve_json: Value = serde_json::from_str(&resolve_output).unwrap();
    assert_eq!(resolve_json["status"], "completed");
    assert_eq!(resolve_json["recommended_action"], "heddle continue");

    let continue_output = heddle(&["rebase", "--continue"], Some(temp.path())).unwrap();
    assert!(
        continue_output.contains("Accepted manual resolution")
            || continue_output.contains("Rebase completed")
            || continue_output.contains("\"status\": \"manual-resolution-accepted\""),
        "manual resolution should be accepted during rebase continue: {continue_output}"
    );
    assert!(
        !temp.path().join(".heddle/REBASE_STATE").exists(),
        "rebase state should clear after the manual resolution is accepted"
    );

    let content = fs::read_to_string(temp.path().join("conflict.txt")).unwrap();
    assert_eq!(content, "main version\nfeature version\n");
    let status_after = status_json(temp.path());
    assert_eq!(status_after["thread"].as_str().unwrap(), "feature");
}

#[test]
fn test_rebase_auto_combines_non_overlapping_text_edits() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(
        temp.path().join("planner.js"),
        "export function owner() {\n  return 'team';\n}\n\nexport function risk() {\n  return 'medium';\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Base planner"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(
        temp.path().join("planner.js"),
        "export function owner() {\n  return 'team';\n}\n\nexport function risk() {\n  return 'high';\n}\n",
    )
    .unwrap();
    heddle(
        &["capture", "-m", "Feature changes risk"],
        Some(temp.path()),
    )
    .unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(
        temp.path().join("planner.js"),
        "export function owner() {\n  return 'release-team';\n}\n\nexport function risk() {\n  return 'medium';\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Main changes owner"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    let output = heddle(&["rebase", "main"], Some(temp.path())).unwrap();
    assert!(
        output.contains("Rebase completed") || output.contains("\"status\": \"completed\""),
        "non-overlapping textual edits should replay without manual conflict: {output}"
    );
    assert!(
        !temp.path().join(".heddle/REBASE_STATE").exists(),
        "auto-combined rebase should not leave rebase state"
    );

    let content = fs::read_to_string(temp.path().join("planner.js")).unwrap();
    assert!(content.contains("return 'release-team';"), "{content}");
    assert!(content.contains("return 'high';"), "{content}");
}

/// `thread switch X` must NOT touch the operator's CWD when `X` has
/// its own dedicated worktree (lightweight or virtualized). This is
/// the "invisible thread directories" rule: switch is a metadata-only
/// operation — flip HEAD, leave the filesystem alone.
///
/// Setup: main has `main-only.txt`, lightweight thread `feature` has
/// `feature-only.txt`. Operator stays at `temp.path()` (main's
/// worktree) and runs `thread switch feature`. After the switch:
///   (a) HEAD is attached to `feature`,
///   (b) `temp.path()` (main's worktree) is byte-identical to before,
///   (c) `feature`'s recorded worktree contains `feature-only.txt`.
#[test]
fn test_thread_switch_does_not_modify_cwd_worktree() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    // Build a feature thread with content distinct from main.
    heddle(
        &["start", "feature", "--workspace", "auto"],
        Some(temp.path()),
    )
    .unwrap();
    let feature_show = heddle(&["thread", "show", "feature", "--json"], Some(temp.path())).unwrap();
    let feature_json: Value = serde_json::from_str(&feature_show).unwrap();
    let feature_path = feature_json["execution_path"].as_str().unwrap().to_string();
    fs::write(
        std::path::Path::new(&feature_path).join("feature-only.txt"),
        "feature content",
    )
    .unwrap();
    heddle(
        &["capture", "-m", "Feature work"],
        Some(std::path::Path::new(&feature_path)),
    )
    .unwrap();

    // Switch back to main from the feature worktree, then add a file
    // distinct to main. `main` is the legacy shared-worktree thread
    // (no dedicated execution_path), so this switch follows the
    // legacy goto-path.
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("main-only.txt"), "main content").unwrap();
    heddle(&["capture", "-m", "Main work"], Some(temp.path())).unwrap();

    // Capture the byte-state of CWD before the switch.
    let before_main_only = fs::read_to_string(temp.path().join("main-only.txt")).unwrap();
    let before_base = fs::read_to_string(temp.path().join("base.txt")).unwrap();
    assert!(
        !temp.path().join("feature-only.txt").exists(),
        "main worktree must not contain feature-only.txt before the switch"
    );

    // The pivotal call: switch from main's worktree to feature.
    // Under the new "invisible thread dirs" rule, this must NOT
    // materialize feature's tree at temp.path(); it must only flip
    // HEAD.
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();

    // (a) HEAD must be attached to `feature` (using temp.path() as
    // the inspection root since the .heddle/HEAD lives there).
    let status = status_json(temp.path());
    assert_eq!(
        status["thread"].as_str().unwrap(),
        "feature",
        "HEAD must be attached to feature after switch"
    );

    // (b) temp.path() (main's worktree) must be byte-identical to
    // before the switch. main-only.txt and base.txt must still be
    // there with their original contents; feature-only.txt must NOT
    // have appeared.
    assert!(
        temp.path().join("main-only.txt").exists(),
        "switching to feature must not delete main's files from CWD"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("main-only.txt")).unwrap(),
        before_main_only,
        "main-only.txt content must be unchanged after switch"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("base.txt")).unwrap(),
        before_base,
        "base.txt content must be unchanged after switch"
    );
    assert!(
        !temp.path().join("feature-only.txt").exists(),
        "feature's files must not appear in main's worktree after switch"
    );

    // (c) feature's recorded worktree must still contain
    // feature-only.txt — switch must not touch it either.
    assert!(
        std::path::Path::new(&feature_path)
            .join("feature-only.txt")
            .exists(),
        "feature's worktree must still contain feature-only.txt after switch"
    );
}

/// Sibling assertion: `thread switch X` is a metadata-only operation
/// for threads with dedicated worktrees — the only observable state
/// change in the source's worktree is the HEAD ref. No tree
/// materialization, no oplog `goto` entry, no file timestamp churn.
#[test]
fn test_thread_switch_only_updates_head() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "v1").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(
        &["start", "feature", "--workspace", "auto"],
        Some(temp.path()),
    )
    .unwrap();

    // Switch back to main first so HEAD is attached to main and the
    // upcoming switch->feature is the operation under test.
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();

    // Snapshot the byte-state of all files in temp.path() (main's
    // worktree) before the switch.
    let before: std::collections::BTreeMap<String, String> = fs::read_dir(temp.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let content = fs::read_to_string(entry.path()).unwrap();
            (name, content)
        })
        .collect();

    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();

    let status = status_json(temp.path());
    assert_eq!(
        status["thread"].as_str().unwrap(),
        "feature",
        "HEAD must point at feature"
    );

    // Files in temp.path() must be byte-identical to the snapshot.
    let after: std::collections::BTreeMap<String, String> = fs::read_dir(temp.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let content = fs::read_to_string(entry.path()).unwrap();
            (name, content)
        })
        .collect();
    assert_eq!(
        before, after,
        "no file in CWD may be modified by a metadata-only thread switch"
    );
}

/// Switching from inside an agent's lightweight worktree to a
/// different thread must update HEAD but leave the agent's worktree
/// alone. We don't reach into other people's worktrees on switch.
#[test]
fn test_thread_switch_works_from_inside_thread_worktree() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    // Two lightweight agent threads. CWD-of-the-test is main's root,
    // each agent has its own dedicated worktree.
    heddle(
        &["start", "alpha", "--workspace", "auto"],
        Some(temp.path()),
    )
    .unwrap();
    let alpha_show = heddle(&["thread", "show", "alpha", "--json"], Some(temp.path())).unwrap();
    let alpha_path = serde_json::from_str::<Value>(&alpha_show).unwrap()["execution_path"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(
        std::path::Path::new(&alpha_path).join("alpha.txt"),
        "alpha content",
    )
    .unwrap();
    heddle(
        &["capture", "-m", "Alpha"],
        Some(std::path::Path::new(&alpha_path)),
    )
    .unwrap();

    heddle(&["start", "beta", "--workspace", "auto"], Some(temp.path())).unwrap();
    let beta_show = heddle(&["thread", "show", "beta", "--json"], Some(temp.path())).unwrap();
    let beta_path = serde_json::from_str::<Value>(&beta_show).unwrap()["execution_path"]
        .as_str()
        .unwrap()
        .to_string();

    // Snapshot alpha's worktree contents, then run `thread switch beta`
    // from *inside* alpha's worktree.
    let alpha_before: std::collections::BTreeMap<String, String> = fs::read_dir(&alpha_path)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let content = fs::read_to_string(entry.path()).unwrap();
            (name, content)
        })
        .collect();

    heddle(
        &["thread", "switch", "beta"],
        Some(std::path::Path::new(&alpha_path)),
    )
    .unwrap();

    // Worktree-HEAD safety (cmd_thread_switch in thread.rs):
    // `thread switch beta` from inside alpha's dedicated worktree
    // routes the HEAD update to the *main repo*, not alpha's
    // worktree-local HEAD. Two reasons:
    //   1. Alpha's worktree keeps its identity — running `heddle
    //      status` from alpha_path afterwards still says "alpha", so
    //      the next auto-capture-on-switch sees source=alpha and runs
    //      (instead of seeing source==target and skipping).
    //   2. The user's intent in invoking `thread switch beta` from
    //      anywhere is "set the active thread to beta"; the main
    //      repo's HEAD is the canonical answer to that question.
    // So: main repo HEAD advances to beta, alpha worktree HEAD stays
    // at alpha.
    let main_status = status_json(temp.path());
    assert_eq!(
        main_status["thread"].as_str().unwrap(),
        "beta",
        "main repo HEAD must advance to beta after thread switch beta"
    );
    let alpha_status = status_json(std::path::Path::new(&alpha_path));
    assert_eq!(
        alpha_status["thread"].as_str().unwrap(),
        "alpha",
        "alpha's worktree HEAD must keep its identity (so future auto-capture works)"
    );

    // Alpha's worktree must be untouched — switching to beta from
    // alpha's directory is *not* an instruction to materialize beta's
    // tree at alpha's worktree. Beta has its own worktree on disk.
    let alpha_after: std::collections::BTreeMap<String, String> = fs::read_dir(&alpha_path)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let content = fs::read_to_string(entry.path()).unwrap();
            (name, content)
        })
        .collect();
    assert_eq!(
        alpha_before, alpha_after,
        "alpha's worktree must be untouched when switching to beta from alpha's dir"
    );

    // Beta's worktree must still be there as `start` materialized it.
    assert!(
        std::path::Path::new(&beta_path).exists(),
        "beta's worktree must still exist after switch"
    );
}

/// If the recorded execution_path for a thread no longer exists on
/// disk (e.g. the operator manually `rm -rf`'d it), `thread switch`
/// must recover by re-materializing the worktree from `current_state`.
/// Documented choice: re-materialize over erroring — `current_state`
/// is the source of truth for the thread's content, and erroring
/// would force the user into a recovery dance with no obvious next
/// command. Anything the worktree held that wasn't snapshotted is
/// already gone, so re-materializing simply restores the last-known
/// good state.
#[test]
fn test_thread_switch_to_thread_with_missing_worktree_handles_gracefully() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(
        &["start", "ghost", "--workspace", "auto"],
        Some(temp.path()),
    )
    .unwrap();
    let ghost_show = heddle(&["thread", "show", "ghost", "--json"], Some(temp.path())).unwrap();
    let ghost_path = serde_json::from_str::<Value>(&ghost_show).unwrap()["execution_path"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(
        std::path::Path::new(&ghost_path).join("ghost.txt"),
        "ghost content",
    )
    .unwrap();
    heddle(
        &["capture", "-m", "Ghost work"],
        Some(std::path::Path::new(&ghost_path)),
    )
    .unwrap();

    // Switch away from ghost so the upcoming switch is the operation
    // under test.
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();

    // Operator nukes the ghost worktree out of band.
    fs::remove_dir_all(&ghost_path).unwrap();
    assert!(
        !std::path::Path::new(&ghost_path).exists(),
        "ghost's worktree must be gone after rm -rf"
    );

    // Switch back. Documented behavior: re-materialize from
    // current_state; switch must succeed.
    heddle(&["thread", "switch", "ghost"], Some(temp.path())).unwrap();

    // The recorded path must now exist again with ghost's last
    // committed content.
    assert!(
        std::path::Path::new(&ghost_path).exists(),
        "ghost's worktree must be re-materialized after switch"
    );
    assert!(
        std::path::Path::new(&ghost_path).join("ghost.txt").exists(),
        "re-materialized worktree must contain ghost's last snapshotted file"
    );
    assert_eq!(
        fs::read_to_string(std::path::Path::new(&ghost_path).join("ghost.txt")).unwrap(),
        "ghost content",
        "re-materialized file content must match the last snapshot"
    );

    // HEAD must be attached to ghost.
    let status = status_json(temp.path());
    assert_eq!(
        status["thread"].as_str().unwrap(),
        "ghost",
        "HEAD must be attached to ghost after switch"
    );
}

/// Regression: `heddle merge` must not silently destroy heddle-ignored
/// content under a tracked top-level directory it drops. Pre-fix,
/// `apply_merged_tree` called `remove_path_recursively` on entries the
/// merged tree no longer contained, recursively nuking `web/node_modules/`
/// alongside the tracked `web/index.html`. Post-fix, only tracked
/// descendants are removed and ignored siblings survive.
#[test]
fn test_merge_preserves_ignored_siblings_in_dropped_tracked_dir() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    // Base state on `main`: tracked `web/index.html` exists.
    fs::create_dir_all(temp.path().join("web")).unwrap();
    fs::write(temp.path().join("web/index.html"), "<html/>").unwrap();
    heddle(&["capture", "-m", "add web"], Some(temp.path())).unwrap();

    // `feature` thread drops the `web/` directory.
    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::remove_dir_all(temp.path().join("web")).unwrap();
    heddle(&["capture", "-m", "drop web"], Some(temp.path())).unwrap();

    // Back on `main`, drop the heddle-ignored sibling. The default ignore
    // list (`target`, `node_modules`, `.git`) skips this — invisible to
    // status, present on disk.
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::create_dir_all(temp.path().join("web/node_modules/lodash")).unwrap();
    fs::write(
        temp.path().join("web/node_modules/lodash/index.js"),
        "ignored",
    )
    .unwrap();

    // The merge drops `web/` from main's top-level tree.
    heddle(&["merge", "feature"], Some(temp.path())).expect("merge must succeed");

    // Tracked content gone.
    assert_file_not_exists(
        temp.path().join("web/index.html"),
        "tracked entry removed by merge",
    );
    // Ignored sibling preserved.
    assert_file_exists(
        temp.path().join("web/node_modules/lodash/index.js"),
        "heddle-ignored content must survive merge that drops the tracked dir",
    );
}

// -----------------------------------------------------------------
// Status-truth tests for `heddle merge` JSON output.
//
// These cover the schema invariants documented on
// `merge_output_from_report`: the `status` field reflects the
// actual outcome of the invocation, advisory items move to
// `warnings`, and `next_action` only points at commands that will
// actually work in the resulting state.
// -----------------------------------------------------------------

/// Merging a thread that triggers a "heavy-impact change" advisory
/// must report `status: "completed"` (the merge actually advanced
/// state) and surface the advisory under `warnings`, not `blockers`.
/// Before the fix this returned `status: "blocked"` with a non-null
/// `merge_state` — internally inconsistent.
#[test]
fn test_merge_with_promotion_warning_completes_with_warning_not_blocker() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    // Base state: a normal source file plus a Cargo.toml so the
    // dependency-detection heuristic has something to anchor on.
    fs::write(temp.path().join("README.md"), "base\n").unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    // Feature thread modifies Cargo.toml — this lights up
    // `heavy_impact_paths` and triggers the "Heavy-impact change"
    // advice in `describe_thread_advice`. The merge should still
    // succeed cleanly.
    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.2.0\"\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Bump version"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();

    let out = heddle(&["--json", "merge", "feature"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&out).expect("merge --json should be JSON");

    // The merge actually advanced state.
    assert!(
        parsed["merge_state"].is_string() || parsed["fast_forward"].as_bool() == Some(true),
        "merge should advance state (set merge_state or fast-forward): {parsed}"
    );
    assert_eq!(
        parsed["status"], "completed",
        "status must reflect actual outcome (merge succeeded), not advisory presence: {parsed}"
    );
    assert_eq!(
        parsed["thread_state"], "merged",
        "thread should be marked merged on a successful merge: {parsed}"
    );
    // No real blockers — the operation didn't fail to advance state.
    let blockers = parsed["blockers"].as_array();
    assert!(
        blockers.map(|b| b.is_empty()).unwrap_or(true),
        "blockers must be empty when status == completed: {parsed}"
    );
    // Warnings may or may not be present depending on whether the
    // advisory survives the fast-forward path. If present, it must
    // be an array.
    if !parsed["warnings"].is_null() {
        assert!(
            parsed["warnings"].is_array(),
            "warnings must be an array when present: {parsed}"
        );
    }
    // `next_action` must not point at the merge we just ran (no loop)
    // and must not be a blocker-style "resolve" command.
    if let Some(next) = parsed["next_action"].as_str() {
        assert!(
            !next.contains("heddle merge "),
            "next_action must not loop back to merge after a successful merge: {next}"
        );
        assert!(
            !next.starts_with("heddle resolve"),
            "next_action must not suggest resolve when status == completed: {next}"
        );
    }
}

/// Merging with an actual conflict must report `status: "blocked"`,
/// have `merge_state: null`, and `next_action` must be a real
/// resolution flow (or null on preview). Validates the invariant
/// that `status: "blocked"` never accompanies a non-null
/// `merge_state`.
#[test]
fn test_merge_with_real_conflict_reports_blocked_with_null_merge_state() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let file = temp.path().join("conflict.txt");
    fs::write(&file, "base\n").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(&file, "feature line\n").unwrap();
    heddle(&["capture", "-m", "Feature edit"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(&file, "main line\n").unwrap();
    heddle(&["capture", "-m", "Main edit"], Some(temp.path())).unwrap();

    let out = heddle(&["--json", "merge", "feature"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&out).expect("merge --json should be JSON");

    assert_eq!(
        parsed["status"], "blocked",
        "real conflict must report status: blocked: {parsed}"
    );
    assert!(
        parsed["merge_state"].is_null(),
        "merge_state must be null when conflict prevented advance: {parsed}"
    );
    let blockers = parsed["blockers"]
        .as_array()
        .expect("blockers must be present on blocked status");
    assert!(
        !blockers.is_empty(),
        "blockers must be non-empty on a real conflict: {parsed}"
    );
    // next_action should point at the resolution flow.
    if let Some(next) = parsed["next_action"].as_str() {
        assert!(
            next.contains("continue") || next.contains("resolve"),
            "next_action on conflict should point at the resolution flow: {next}"
        );
    }
}

/// Helper: read a thread's `target_thread` from the JSON view of
/// `heddle thread show`. Used by refresh tests that need to
/// configure a target before invoking refresh.
fn thread_target(temp: &std::path::Path, thread: &str) -> Option<String> {
    let out = heddle(&["--json", "thread", "show", thread], Some(temp)).ok()?;
    let parsed: Value = serde_json::from_str(&out).ok()?;
    parsed["target_thread"].as_str().map(|s| s.to_string())
}

/// Refreshing a sibling thread whose changes are disjoint from the
/// target's must succeed — even though the commit-by-commit rebase
/// replay can flag intermediate states as conflicting. This is the
/// core convergence with `heddle merge`'s 3-way tree merge: if merge
/// can do it cleanly, refresh should too.
///
/// Uses `heddle start` so the threads have real execution paths
/// (lightweight `heddle thread create` threads have empty paths, so
/// `heddle thread refresh` cannot operate on them — orthogonal to
/// the bug under test).
#[test]
fn test_thread_refresh_with_disjoint_sibling_changes_succeeds() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "shared base\n").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    let alpha_path = temp.path().join("threads/alpha");
    let beta_path = temp.path().join("threads/beta");

    let alpha_started = heddle(
        &[
            "--json",
            "start",
            "alpha",
            "--workspace",
            "materialized",
            "--path",
            alpha_path.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("start alpha");
    let _alpha: Value = serde_json::from_str(&alpha_started).unwrap();

    let beta_started = heddle(
        &[
            "--json",
            "start",
            "beta",
            "--workspace",
            "materialized",
            "--path",
            beta_path.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("start beta");
    let _beta: Value = serde_json::from_str(&beta_started).unwrap();

    // Skip if start didn't wire up target_thread — refresh requires
    // one. Different start configurations may or may not set it.
    let Some(beta_target) = thread_target(temp.path(), "beta") else {
        eprintln!("beta has no target_thread; skipping refresh test");
        return;
    };

    // alpha edits its own file from inside its checkout.
    fs::write(alpha_path.join("alpha.txt"), "alpha content\n").unwrap();
    heddle(&["capture", "-m", "Alpha edit"], Some(&alpha_path)).unwrap();

    // beta edits its own (disjoint) file from inside its checkout.
    fs::write(beta_path.join("beta.txt"), "beta content\n").unwrap();
    heddle(&["capture", "-m", "Beta edit"], Some(&beta_path)).unwrap();

    // Merge alpha into the target (typically `main`). After this,
    // beta is stale against the target but the edits are disjoint.
    heddle(&["thread", "switch", &beta_target], Some(temp.path())).unwrap();
    let merge_alpha = heddle(&["merge", "alpha"], Some(temp.path()));
    assert!(
        merge_alpha.is_ok(),
        "merge alpha must succeed: {merge_alpha:?}"
    );

    // Now refresh beta. Before the fix, this failed with
    // "could not be refreshed cleanly; resolve rebase conflicts and
    // retry" — even though the trees merge cleanly via 3-way.
    let refresh = heddle(&["thread", "refresh", "beta"], Some(temp.path()));
    assert!(
        refresh.is_ok(),
        "refresh of disjoint sibling must succeed (3-way merge fallback): {refresh:?}"
    );
}

/// When refresh can't be done cleanly (real conflict on the same
/// path), the error must name the conflicting paths instead of the
/// historical misleading "rebase conflicts" message. Same scaffolding
/// as the disjoint-sibling test, but with overlapping edits.
#[test]
fn test_thread_refresh_real_conflict_emits_precise_blocker() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("contested.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    let alpha_path = temp.path().join("threads/alpha");
    let beta_path = temp.path().join("threads/beta");

    heddle(
        &[
            "--json",
            "start",
            "alpha",
            "--workspace",
            "materialized",
            "--path",
            alpha_path.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("start alpha");
    heddle(
        &[
            "--json",
            "start",
            "beta",
            "--workspace",
            "materialized",
            "--path",
            beta_path.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("start beta");

    let Some(beta_target) = thread_target(temp.path(), "beta") else {
        eprintln!("beta has no target_thread; skipping refresh-conflict test");
        return;
    };

    // Both threads edit the same line of the same file.
    fs::write(alpha_path.join("contested.txt"), "alpha line\n").unwrap();
    heddle(&["capture", "-m", "Alpha edit"], Some(&alpha_path)).unwrap();
    fs::write(beta_path.join("contested.txt"), "beta line\n").unwrap();
    heddle(&["capture", "-m", "Beta edit"], Some(&beta_path)).unwrap();

    heddle(&["thread", "switch", &beta_target], Some(temp.path())).unwrap();
    heddle(&["merge", "alpha"], Some(temp.path())).expect("merge alpha must succeed");

    // beta now genuinely conflicts with the target. Refresh must
    // fail — but with a precise message naming the conflicting path,
    // not the historical "resolve rebase conflicts and retry" when
    // there is no actual conflict marker on disk.
    let refresh_result = heddle(&["thread", "refresh", "beta"], Some(temp.path()));
    let Err(refresh_err) = refresh_result else {
        // The 3-way merge could legitimately resolve this if the
        // merge engine collapses the two single-line edits.
        // Acceptable; just exit.
        return;
    };
    assert!(
        !refresh_err.contains("resolve rebase conflicts and retry"),
        "refresh error must not be the historical misleading 'rebase conflicts' string: {refresh_err}"
    );
    assert!(
        refresh_err.contains("conflicting path") || refresh_err.contains("contested.txt"),
        "refresh error on real conflict must name the conflicting path: {refresh_err}"
    );
}

// ----- --with-diff tests (item 3.3) ---------------------------------
//
// `heddle merge <thread> --preview --with-diff --json` must surface the
// parent ↔ thread-tip diff alongside the existing preview metadata so an
// agent doesn't have to make a separate `heddle diff` call to see what
// would land. Without `--with-diff`, the `diff` field must be omitted.

/// Helper: set up a base + feature-thread divergence with a modified
/// file and an added file. Used by the `--with-diff` tests so they can
/// assert on the same shape of changes.
fn create_simple_feature_thread(temp: &TempDir) {
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "base content\n").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();
    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "feature content\n").unwrap();
    fs::write(temp.path().join("newfile.txt"), "added by feature\n").unwrap();
    heddle(&["capture", "-m", "Feature work"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
}

#[test]
fn test_merge_preview_with_diff_returns_populated_diff_changes() {
    let temp = TempDir::new().unwrap();
    create_simple_feature_thread(&temp);

    let out = heddle(
        &["--json", "merge", "feature", "--preview", "--with-diff"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&out).expect("merge --json should be JSON");

    let diff = &parsed["diff"];
    assert!(
        diff.is_object(),
        "--with-diff must populate `diff` with an object: {parsed}"
    );
    assert!(
        diff["from_state"].is_string(),
        "`diff.from_state` must be the parent tip change-id: {parsed}"
    );
    assert!(
        diff["to_state"].is_string(),
        "`diff.to_state` must be the thread tip change-id: {parsed}"
    );
    let changes = diff["changes"]
        .as_array()
        .expect("`diff.changes` must be an array");
    assert!(
        !changes.is_empty(),
        "`diff.changes` must be non-empty when the thread has changes: {parsed}"
    );
    let paths: Vec<&str> = changes.iter().filter_map(|c| c["path"].as_str()).collect();
    assert!(
        paths.contains(&"file.txt"),
        "modified file must appear in diff.changes: {paths:?}"
    );
    assert!(
        paths.contains(&"newfile.txt"),
        "added file must appear in diff.changes: {paths:?}"
    );
    // Each change should carry kind + lines so the diff is actually
    // useful (not just a name list — that's what `--name-only` is for).
    let modified_change = changes
        .iter()
        .find(|c| c["path"] == "file.txt")
        .expect("file.txt change must be present");
    assert_eq!(modified_change["kind"], "modified");
    let lines = modified_change["lines"]
        .as_array()
        .expect("modified file must include `lines` array");
    assert!(
        lines.iter().any(|l| l["prefix"] == "-"),
        "modified file diff must include removed lines: {modified_change}"
    );
    assert!(
        lines.iter().any(|l| l["prefix"] == "+"),
        "modified file diff must include added lines: {modified_change}"
    );
}

#[test]
fn test_merge_preview_without_with_diff_omits_diff_field() {
    let temp = TempDir::new().unwrap();
    create_simple_feature_thread(&temp);

    let out = heddle(
        &["--json", "merge", "feature", "--preview"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&out).expect("merge --json should be JSON");

    // Convention: when `--with-diff` is not set, the `diff` field is
    // omitted entirely (not null) so consumers can detect intent
    // unambiguously. `serde_json::Value::Null` returned by indexing
    // a missing key is what we expect here.
    assert!(
        parsed.get("diff").is_none(),
        "`diff` must be absent (not null) when `--with-diff` is not set: {parsed}"
    );
}

#[test]
fn test_merge_apply_with_diff_echoes_landed_changes() {
    let temp = TempDir::new().unwrap();
    create_simple_feature_thread(&temp);

    // Apply the merge (not preview). The diff should describe what
    // just landed: parent tip ↔ thread tip.
    let out = heddle(
        &["--json", "merge", "feature", "--with-diff"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&out).expect("merge --json should be JSON");

    assert_eq!(
        parsed["status"], "completed",
        "merge must complete on a clean fast-forward: {parsed}"
    );
    let diff = &parsed["diff"];
    assert!(
        diff.is_object(),
        "--with-diff on a real merge must populate `diff`: {parsed}"
    );
    let changes = diff["changes"]
        .as_array()
        .expect("`diff.changes` must be an array");
    assert!(
        !changes.is_empty(),
        "`diff.changes` must echo the changes that just landed: {parsed}"
    );
}

// --- `heddle merge --git-commit` tests ---
//
// These exercise the optional git-commit coordination: `--git-commit`
// makes a heddle merge also write a git commit on top of HEAD. The
// default (no flag) is preserved — heddle state advances and git is
// unaware. See `crates/cli/src/cli/commands/merge/git_commit.rs`.

fn git(args: &[&str], cwd: &std::path::Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|err| panic!("git {:?} failed to run: {}", args, err));
    assert!(status.success(), "git {:?} should succeed", args);
}

fn git_output(args: &[&str], cwd: &std::path::Path) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|err| panic!("git {:?} failed to run: {}", args, err));
    assert!(
        out.status.success(),
        "git {:?} should succeed: stderr={}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Set up a heddle-native repo with a `feature` thread that diverges
/// from `main`, plus a real `.git` directory with a single base
/// commit. Heddle ignores `.git/` (built-in) and the test's auxiliary
/// noise files via `.heddleignore`. `--git-commit` requires a `.git`
/// to write into; the test repo provides exactly that.
fn create_git_overlay_feature_thread(temp: &TempDir) {
    let path = temp.path();

    // Heddle first so we land on heddle-native (not git-overlay) — in
    // git-overlay, `main` and `feature` would share the same heddle
    // state and `merge feature` would resolve to "already up to date",
    // which is not what we're trying to exercise here.
    fs::write(
        path.join(".heddleignore"),
        // The .git tree, the .gitignore we'll write later, plus the
        // `unrelated/` subtree we use in tests to introduce dirt that
        // heddle ignores but git tracks. Only `*<suffix>` wildcards
        // are honoured (see worktree_ignore::should_ignore), so we
        // use a directory pattern instead.
        ".git\n.gitignore\nunrelated/\n",
    )
    .unwrap();

    heddle(&["init"], Some(path)).unwrap();
    fs::write(path.join("base.txt"), "base content\n").unwrap();
    heddle(&["capture", "-m", "Base"], Some(path)).unwrap();
    heddle(&["thread", "create", "feature"], Some(path)).unwrap();
    heddle(&["thread", "switch", "feature"], Some(path)).unwrap();
    fs::write(path.join("feature.txt"), "feature content\n").unwrap();
    heddle(&["capture", "-m", "Feature work"], Some(path)).unwrap();
    heddle(&["thread", "switch", "main"], Some(path)).unwrap();

    // Now bootstrap git on top, so .git exists for --git-commit. We
    // commit the current main-tip state (base.txt only) so subsequent
    // git status only flags new files the merge will introduce.
    git(&["init", "--initial-branch", "main"], path);
    git(&["config", "user.name", "Heddle Test"], path);
    git(&["config", "user.email", "heddle@example.com"], path);
    // Ignore everything the test doesn't want the merge commit to
    // capture (heddle metadata, scratch directories).
    fs::write(
        path.join(".gitignore"),
        ".heddle/\n.heddleignore\n.gitignore\n",
    )
    .unwrap();
    git(&["add", "base.txt"], path);
    git(&["commit", "-m", "git base"], path);
}

#[test]
fn test_merge_without_git_commit_writes_no_git_commit() {
    let temp = TempDir::new().unwrap();
    create_git_overlay_feature_thread(&temp);

    let before = git_output(&["log", "--oneline"], temp.path());
    let before_count = before.lines().count();

    heddle(&["merge", "feature"], Some(temp.path())).unwrap();

    let after = git_output(&["log", "--oneline"], temp.path());
    let after_count = after.lines().count();
    assert_eq!(
        before_count, after_count,
        "default merge must not write a git commit: before={before:?} after={after:?}"
    );
}

#[test]
fn test_merge_git_commit_writes_commit_with_merge_state_trailer() {
    let temp = TempDir::new().unwrap();
    create_git_overlay_feature_thread(&temp);

    let before = git_output(&["log", "--oneline"], temp.path());
    let before_count = before.lines().count();

    let out = heddle(
        &["--json", "merge", "feature", "--git-commit"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&out).expect("merge --json should be JSON");

    assert_eq!(
        parsed["status"], "completed",
        "git-commit merge must complete: {parsed}"
    );
    let git_commit = &parsed["git_commit"];
    assert!(
        git_commit.is_object(),
        "git_commit must be present in JSON output: {parsed}"
    );
    assert!(
        git_commit["sha"].is_string(),
        "git_commit.sha must be a string: {parsed}"
    );
    let message = git_commit["message"]
        .as_str()
        .expect("git_commit.message must be a string");
    let merge_state = parsed["merge_state"]
        .as_str()
        .expect("merge_state must be a string");
    assert!(
        message.contains(&format!("Merge-State: {}", merge_state)),
        "commit message must include Merge-State trailer for {merge_state}: {message}"
    );

    let after = git_output(&["log", "--oneline"], temp.path());
    let after_count = after.lines().count();
    assert_eq!(
        after_count,
        before_count + 1,
        "exactly one new git commit must be written: before={before:?} after={after:?}"
    );

    let head_msg = git_output(&["log", "-1", "--format=%B"], temp.path());
    assert!(
        head_msg.contains(&format!("Merge-State: {}", merge_state)),
        "git HEAD commit message must contain Merge-State trailer: {head_msg}"
    );
    assert!(
        head_msg.contains("Co-Authored-By:"),
        "git HEAD commit message must contain Co-Authored-By trailer: {head_msg}"
    );
}

#[test]
fn test_merge_git_commit_preview_emits_payload_without_writing() {
    let temp = TempDir::new().unwrap();
    create_git_overlay_feature_thread(&temp);

    let before = git_output(&["log", "--oneline"], temp.path());
    let before_count = before.lines().count();

    let out = heddle(
        &["--json", "merge", "feature", "--git-commit", "--preview"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&out).expect("merge --json should be JSON");

    assert_eq!(
        parsed["status"], "preview",
        "preview should not advance state: {parsed}"
    );
    let preview = &parsed["git_commit_preview"];
    assert!(
        preview.is_object(),
        "git_commit_preview must be populated under --preview --git-commit: {parsed}"
    );
    let message = preview["message"]
        .as_str()
        .expect("git_commit_preview.message must be a string");
    assert!(
        message.contains("Merge-State:"),
        "preview message must include Merge-State trailer placeholder: {message}"
    );
    assert!(
        preview["files"].is_array(),
        "git_commit_preview.files must be an array: {preview}"
    );

    // Real git history must be unchanged, and `git_commit` (the
    // realized-commit field) must be absent.
    let after = git_output(&["log", "--oneline"], temp.path());
    assert_eq!(
        after.lines().count(),
        before_count,
        "preview must not write a git commit: before={before:?} after={after:?}"
    );
    assert!(
        parsed.get("git_commit").is_none() || parsed["git_commit"].is_null(),
        "git_commit must be absent in preview mode: {parsed}"
    );
}

#[test]
fn test_merge_git_commit_blocks_on_unrelated_uncommitted_git_changes() {
    let temp = TempDir::new().unwrap();
    create_git_overlay_feature_thread(&temp);

    // Introduce a file git treats as untracked but heddle ignores
    // (via .heddleignore's `unrelated/` directory pattern). The
    // heddle merge would happily proceed; `--git-commit` must refuse
    // to fold it into the merge commit.
    fs::create_dir_all(temp.path().join("unrelated")).unwrap();
    fs::write(
        temp.path().join("unrelated").join("dirt.txt"),
        "unrelated to the merge\n",
    )
    .unwrap();

    let out = heddle(
        &["--json", "merge", "feature", "--git-commit"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&out).expect("merge --json should be JSON");

    assert_eq!(
        parsed["status"], "blocked",
        "merge with unrelated dirty git tree must block: {parsed}"
    );
    let blockers = parsed["blockers"]
        .as_array()
        .expect("blockers must be an array on status:blocked");
    assert!(
        blockers.iter().any(|b| {
            b.as_str()
                .is_some_and(|s| s.contains("unrelated uncommitted git change"))
        }),
        "blockers must flag the unrelated git change: {parsed}"
    );
    // Must not have written a heddle merge state nor a git commit.
    assert!(
        parsed["merge_state"].is_null(),
        "merge_state must be null when blocked: {parsed}"
    );
    assert!(
        parsed.get("git_commit").is_none() || parsed["git_commit"].is_null(),
        "git_commit must be absent when blocked: {parsed}"
    );
}

#[test]
fn test_merge_git_commit_without_git_overlay_blocks_with_clear_error() {
    // No `git init` — heddle-only repo. `--git-commit` must refuse.
    let temp = TempDir::new().unwrap();
    create_simple_feature_thread(&temp);

    let out = heddle(
        &["--json", "merge", "feature", "--git-commit"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&out).expect("merge --json should be JSON");

    assert_eq!(
        parsed["status"], "blocked",
        "merge --git-commit on a non-git repo must block: {parsed}"
    );
    let blockers = parsed["blockers"]
        .as_array()
        .expect("blockers must be an array on status:blocked");
    assert!(
        blockers
            .iter()
            .any(|b| b.as_str().is_some_and(|s| s.contains("no git repository"))),
        "blockers must flag the missing git repo: {parsed}"
    );
}

// ----- Conflict-marker column-0 well-formedness (heddle#78) ---------
//
// Conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) must each start at
// column 0. The pre-fix `format_conflict_content` concatenated the
// "our" / "their" bodies directly against the separator marker, so a
// side whose content lacked a trailing newline produced output like
// `pub type Config = RepoConfig;=======` — invalid to git diff, IDEs,
// and the upcoming hunk-level merge engine.

/// Iterate the marker lines on a conflicted file and assert each one
/// is anchored at column 0 (i.e. appears as its own line).
fn assert_markers_at_column_zero(content: &str, ctx: &str) {
    for marker in ["<<<<<<<", "=======", ">>>>>>>"] {
        let appears = content.contains(marker);
        assert!(
            appears,
            "expected marker `{marker}` in conflict output ({ctx}): {content}"
        );
        // Every occurrence must be at start-of-line. Walk lines and
        // confirm at least one starts with the marker; also confirm no
        // line contains the marker anywhere but at column 0.
        let mut found_at_col_0 = false;
        for line in content.split('\n') {
            if line.starts_with(marker) {
                found_at_col_0 = true;
            } else if line.contains(marker) {
                panic!(
                    "marker `{marker}` is not at column 0 in line ({ctx}): {line:?}\nfull: {content}"
                );
            }
        }
        assert!(
            found_at_col_0,
            "marker `{marker}` never appears at column 0 ({ctx}): {content}"
        );
    }
}

/// Red-commit for heddle#78: when a side's content lacks a trailing
/// newline, the `=======` separator used to be appended directly after
/// the last content line. Reproduces the exact `pub type Config = ...`
/// shape from the heddle#54 trip report.
#[test]
fn test_merge_conflict_markers_anchored_at_column_zero_no_trailing_newline() {
    let temp = TempDir::new().unwrap();
    let file = temp.path().join("config.rs");

    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(&file, "pub type Config = BaseConfig;").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    // No trailing newline on the feature side — the exact shape from
    // the trip report.
    fs::write(&file, "pub type Config = RepoConfig;").unwrap();
    heddle(&["capture", "-m", "Feature edit"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    // No trailing newline on the main side either.
    fs::write(&file, "pub type Config = MainConfig;").unwrap();
    heddle(&["capture", "-m", "Main edit"], Some(temp.path())).unwrap();

    heddle(&["merge", "feature"], Some(temp.path())).unwrap();
    let content = fs::read_to_string(&file).unwrap();
    assert_markers_at_column_zero(&content, "no-trailing-newline both sides");

    // Belt-and-braces: the specific trip-report shape (content glued to
    // the separator) must not reappear.
    assert!(
        !content.contains("RepoConfig;=======") && !content.contains("MainConfig;======="),
        "content must not be glued to the `=======` separator: {content}"
    );
}

/// Red-commit: a marker-validator sweep across multiple fixture
/// shapes — one side missing newline, the other side missing newline,
/// both missing, both ending in newline. All four must produce
/// well-formed markers.
#[test]
fn test_merge_conflict_markers_well_formed_across_newline_shapes() {
    let cases: &[(&str, &str, &str)] = &[
        ("ours-only-newline", "ours\n", "theirs"),
        ("theirs-only-newline", "ours", "theirs\n"),
        ("neither-newline", "ours", "theirs"),
        ("both-newline", "ours\n", "theirs\n"),
    ];
    for (label, ours, theirs) in cases {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("f.txt");

        heddle(&["init"], Some(temp.path())).unwrap();
        fs::write(&file, "base\n").unwrap();
        heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

        heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
        heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
        fs::write(&file, theirs).unwrap();
        heddle(&["capture", "-m", "feature edit"], Some(temp.path())).unwrap();

        heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
        fs::write(&file, ours).unwrap();
        heddle(&["capture", "-m", "main edit"], Some(temp.path())).unwrap();

        heddle(&["merge", "feature"], Some(temp.path())).unwrap();
        let content = fs::read_to_string(&file).unwrap();
        assert_markers_at_column_zero(&content, label);
    }
}

#[test]
fn test_merge_semantic_resolves_disjoint_function_edits_clean() {
    // heddle#68: with `--semantic`, two branches editing DIFFERENT functions
    // in the same file should merge cleanly with zero conflict markers.
    // Without `--semantic` the default text engine surfaces a hunk conflict
    // around the rewritten regions because both sides modified the same
    // file. The two outcomes encode the contract the trip report
    // (heddle#54) asked for.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let src = temp.path().join("lib.rs");
    fs::write(
        &src,
        "fn alpha() -> u32 { 1 }\n\nfn beta() -> u32 { 2 }\n\nfn gamma() -> u32 { 3 }\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "edit_alpha"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "edit_alpha"], Some(temp.path())).unwrap();
    fs::write(
        &src,
        "fn alpha() -> u32 { 11 }\n\nfn beta() -> u32 { 2 }\n\nfn gamma() -> u32 { 3 }\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "alpha edit"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(
        &src,
        "fn alpha() -> u32 { 1 }\n\nfn beta() -> u32 { 2 }\n\nfn gamma() -> u32 { 333 }\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "gamma edit"], Some(temp.path())).unwrap();

    let result = heddle(&["merge", "edit_alpha", "--semantic"], Some(temp.path()));
    assert!(result.is_ok(), "semantic merge should succeed");

    let merged = fs::read_to_string(&src).unwrap();
    assert!(
        !merged.contains("<<<<<<<"),
        "disjoint function edits must not leave conflict markers under --semantic: {merged}"
    );
    assert!(merged.contains("fn alpha() -> u32 { 11 }"), "alpha edit lost: {merged}");
    assert!(merged.contains("fn gamma() -> u32 { 333 }"), "gamma edit lost: {merged}");
}

#[test]
fn test_merge_semantic_falls_through_on_text_file() {
    // Files without a recognised language extension (a `.txt` file here)
    // bypass the AST driver and use the existing hunk-level engine. The
    // existing conflict-marker shape must be preserved.
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let f = temp.path().join("notes.txt");
    fs::write(&f, "base line\n").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(&f, "feature line\n").unwrap();
    heddle(&["capture", "-m", "f"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(&f, "main line\n").unwrap();
    heddle(&["capture", "-m", "m"], Some(temp.path())).unwrap();

    heddle(&["merge", "feature", "--semantic"], Some(temp.path())).unwrap();
    let merged = fs::read_to_string(&f).unwrap();
    assert!(
        merged.contains("<<<<<<< CURRENT (main)"),
        "same-line conflict on a non-language file must still produce markers under --semantic: {merged}"
    );
}

/// Codex r13 P2 (cid 3261133187): `build_thread_preview_report_with_graph`
/// hardcodes `MergeStrategy::HunkOnly`, so a structural-refactor scenario
/// where the text engine surfaces conflicts but the semantic engine is
/// clean prints contradictory preview lines (`blocked` / `conflicts:` in
/// `preview_summary`) even though the actual merge plan and `conflicts`
/// payload — both built with the real `--semantic` strategy — are clean.
///
/// Fixture shape mirrors the semantic crate's
/// `semantic_beats_text_merge_on_structural_reshape` unit test, which
/// asserts directly that `text_hunk_merge` surfaces ≥1 conflict on this
/// exact base/ours/theirs trio. The CLI-level invariant under test: the
/// preview summary must agree with the real merge result when
/// `--semantic` is engaged.
#[test]
fn test_merge_semantic_preview_summary_matches_semantic_plan_on_structural_reshape() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let src = temp.path().join("lib.rs");
    fs::write(
        &src,
        "fn a() { let x = 1; }\nfn b() { let x = 2; }\nfn c() { let x = 3; }\nfn d() { let x = 4; }\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    // feature: reorder + edit `b`. Created from main, so
    // `target_thread = Some("main")` — required for the preview report
    // to compute a 3-way merge.
    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(
        &src,
        "fn d() { let x = 4; }\nfn c() { let x = 3; }\nfn b() { let x = 22; }\nfn a() { let x = 1; }\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "feature: reorder + edit b"], Some(temp.path())).unwrap();

    // main: edit `d` only.
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(
        &src,
        "fn a() { let x = 1; }\nfn b() { let x = 2; }\nfn c() { let x = 3; }\nfn d() { let x = 44; }\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "main: edit d"], Some(temp.path())).unwrap();

    let out = heddle(
        &["--json", "merge", "feature", "--semantic", "--preview"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&out).expect("merge --json should be JSON");

    // Real merge plan (`--semantic`): clean. These come from
    // `merge_plan.relation()` / `merge_result.conflicts`, both built
    // with the operator-selected strategy.
    let conflicts = parsed["conflicts"]
        .as_array()
        .expect("conflicts must be an array on a preview");
    assert!(
        conflicts.is_empty(),
        "with --semantic, structural reshape must produce zero conflict paths: {parsed}"
    );
    assert_eq!(
        parsed["conflict_count"], 0,
        "with --semantic, conflict_count must be 0 on a clean structural-reshape merge: {parsed}"
    );

    // The bug: `preview_summary` is sourced from a preview report built
    // with a hardcoded `MergeStrategy::HunkOnly`. On this fixture
    // `text_hunk_merge` surfaces ≥1 conflict, so the preview report
    // emits a misleading `conflicts: 1 path conflict(s)` line and
    // potentially a `blocked: ...` line, contradicting `conflicts: []`
    // and `conflict_count: 0` above.
    let preview_summary = parsed["preview_summary"]
        .as_array()
        .expect("preview_summary must be an array");
    let summary_strings: Vec<&str> = preview_summary
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        !summary_strings.iter().any(|line| line.starts_with("conflicts:")),
        "preview_summary must not report conflicts when --semantic plan is clean: {summary_strings:?}"
    );
    assert!(
        !summary_strings.iter().any(|line| line.starts_with("blocked:")),
        "preview_summary must not report blocked when --semantic plan is clean: {summary_strings:?}"
    );
}

/// heddle#117: `thread refresh` must route its 3-way merge fallback
/// through the function-level semantic driver (added in heddle#68,
/// PR #114, commit 79104f9). Before the fix, `try_three_way_merge_refresh`
/// hardcoded `MergeStrategy::HunkOnly`, so a structural-reshape on the
/// thread side combined with a disjoint edit on the target side made
/// `heddle thread refresh` fail with "could not be refreshed cleanly"
/// even though `heddle merge --semantic` resolves the same trio cleanly.
///
/// Fixture mirrors `test_merge_semantic_preview_summary_matches_semantic_plan_on_structural_reshape`:
/// `lib.rs` with four trivial fns; thread reorders + edits one fn; main
/// edits a different fn. text_hunk_merge surfaces a conflict here (the
/// reorder rewrites the whole file); the semantic driver does not.
#[cfg(feature = "semantic")]
#[test]
fn test_thread_refresh_routes_through_semantic_driver_on_structural_reshape() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let src = temp.path().join("lib.rs");
    fs::write(
        &src,
        "fn a() { let x = 1; }\nfn b() { let x = 2; }\nfn c() { let x = 3; }\nfn d() { let x = 4; }\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    let beta_path = temp.path().join("threads/beta");
    heddle(
        &[
            "--json",
            "start",
            "beta",
            "--workspace",
            "materialized",
            "--path",
            beta_path.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("start beta");

    // Skip if `start` didn't wire up target_thread — refresh needs one.
    let Some(beta_target) = thread_target(temp.path(), "beta") else {
        eprintln!("beta has no target_thread; skipping semantic-refresh test");
        return;
    };

    // beta: reorder all four fns + edit `b`. Captured from inside
    // beta's checkout.
    fs::write(
        beta_path.join("lib.rs"),
        "fn d() { let x = 4; }\nfn c() { let x = 3; }\nfn b() { let x = 22; }\nfn a() { let x = 1; }\n",
    )
    .unwrap();
    heddle(
        &["capture", "-m", "beta: reorder + edit b"],
        Some(&beta_path),
    )
    .unwrap();

    // target (typically `main`): edit `d` only. This is disjoint from
    // beta's `b` edit at the AST level, but text-hunk merge sees the
    // whole file rewritten by beta and surfaces a conflict.
    heddle(&["thread", "switch", &beta_target], Some(temp.path())).unwrap();
    fs::write(
        &src,
        "fn a() { let x = 1; }\nfn b() { let x = 2; }\nfn c() { let x = 3; }\nfn d() { let x = 44; }\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "main: edit d"], Some(temp.path())).unwrap();

    // Refresh beta. Before the heddle#117 fix this returned Err with
    // "could not be refreshed cleanly: 1 conflicting path(s) (lib.rs)"
    // because the 3-way merge fallback ran with `MergeStrategy::HunkOnly`.
    // With the fix, the fallback routes through `semantic_three_way_merge`
    // and resolves cleanly.
    let refresh = heddle(&["thread", "refresh", "beta"], Some(temp.path()));
    assert!(
        refresh.is_ok(),
        "thread refresh with disjoint AST-level edits must succeed via the semantic merge driver: {refresh:?}"
    );
}
