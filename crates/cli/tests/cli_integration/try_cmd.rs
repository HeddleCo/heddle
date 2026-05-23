// SPDX-License-Identifier: Apache-2.0
//! Coverage for item 3.1 of the heddle 6→8 plan: `heddle try -- <cmd>`.
//!
//! Exercises:
//!   1. `heddle try -- true` succeeds; an ephemeral `try-*` thread
//!      exists with a captured state; parent's HEAD is unchanged.
//!   2. `heddle try -- false` fails (passes through the cmd's exit
//!      code); parent's HEAD is unchanged; the ephemeral thread is
//!      dropped (state == Abandoned).
//!   3. `heddle try --auto-merge -- true` succeeds AND parent's HEAD
//!      advances to the captured state.
//!   4. The working-tree invariant: parent's worktree is byte-identical
//!      after both the success and failure paths.

use std::fs;

use serde_json::Value;
use tempfile::TempDir;

use super::{heddle, heddle_output};

/// Bootstrap a minimal repo with a single capture so the parent has
/// a HEAD. Tests then run `heddle try` against this seeded state and
/// observe what changes (or, more importantly, doesn't).
fn setup_repo() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "init"], Some(temp.path())).unwrap();
    temp
}

/// Resolve the parent thread's HEAD as a full change-id string. We
/// read it via `heddle log --json` rather than poking at refs/ on
/// disk so we exercise the same observation path an agent would.
fn parent_head(repo: &std::path::Path) -> String {
    let raw = heddle(&["--output", "json", "log", "--limit", "1"], Some(repo)).unwrap();
    let value: Value = serde_json::from_str(&raw).unwrap();
    value["states"][0]["change_id_full"]
        .as_str()
        .or_else(|| value["states"][0]["change_id"].as_str())
        .unwrap()
        .to_string()
}

/// Capture the parent's worktree as a sorted (path, contents) pair.
/// Used to assert byte-equivalence before/after `heddle try` runs.
/// We deliberately limit the read to top-level files to keep the
/// invariant check fast and free of `.heddle/` noise (which can move
/// for legitimate reasons during a try — e.g. a new thread record).
fn worktree_snapshot(repo: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in fs::read_dir(repo).unwrap().flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        if name_str.starts_with('.') {
            // Skip .heddle, .heddle-user, .git, etc. The user-facing
            // tracked files are what we care about for the invariant.
            continue;
        }
        if path.is_file() {
            out.push((name_str, fs::read(&path).unwrap()));
        }
    }
    out.sort();
    out
}

#[test]
fn try_succeeds_creates_thread_and_preserves_parent_head() {
    let temp = setup_repo();
    let head_before = parent_head(temp.path());
    let worktree_before = worktree_snapshot(temp.path());

    let raw = heddle(
        &["--output", "json", "try", "--", "true"],
        Some(temp.path()),
    )
    .expect("heddle try -- true should succeed");
    let value: Value = serde_json::from_str(&raw).expect("output should be JSON");

    assert_eq!(value["status"], "completed", "raw output: {raw}");
    assert_eq!(value["exit_code"], 0);

    let thread_name = value["thread"].as_str().unwrap();
    assert!(
        thread_name.starts_with("try-"),
        "thread name should be auto-generated (got {thread_name})"
    );

    // Parent's HEAD must not have advanced.
    let head_after = parent_head(temp.path());
    assert_eq!(
        head_before, head_after,
        "parent HEAD changed without --auto-merge"
    );

    // Working tree invariant.
    assert_eq!(
        worktree_before,
        worktree_snapshot(temp.path()),
        "parent worktree changed during heddle try"
    );

    // The ephemeral thread should still exist (no --auto-merge means
    // we leave it for the user to merge or drop).
    let list_raw = heddle(&["--output", "json", "thread", "list"], Some(temp.path())).unwrap();
    let list: Value = serde_json::from_str(&list_raw).unwrap();
    let names: Vec<&str> = list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(
        names.contains(&thread_name),
        "ephemeral thread should remain after success without --auto-merge; got {names:?}"
    );
}

#[test]
fn try_failure_preserves_parent_head_and_drops_thread() {
    let temp = setup_repo();
    let head_before = parent_head(temp.path());
    let worktree_before = worktree_snapshot(temp.path());

    let output = heddle_output(
        &["--output", "json", "try", "--", "false"],
        Some(temp.path()),
    )
    .expect("spawn heddle");
    assert!(
        !output.status.success(),
        "heddle try -- false should fail (got status {:?})",
        output.status
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "try should pass through the cmd's exit code"
    );

    // Parent HEAD must not move on the failure path.
    let head_after = parent_head(temp.path());
    assert_eq!(head_before, head_after, "parent HEAD changed on failure");

    // Working-tree invariant on failure path.
    assert_eq!(
        worktree_before,
        worktree_snapshot(temp.path()),
        "parent worktree changed when heddle try failed"
    );

    // The ephemeral thread should have been dropped. Look for any
    // `try-*` thread that's still Active (failure leaves them as
    // Abandoned). `thread list` defaults to hiding auto-threads, but
    // `heddle try` doesn't tag its threads as `auto: true` (they're
    // user-driven), so we'd see an Active one if drop didn't run.
    let list_raw = heddle(&["--output", "json", "thread", "list"], Some(temp.path())).unwrap();
    let list: Value = serde_json::from_str(&list_raw).unwrap();
    let active_try: Vec<&str> = list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["thread_state"] == "active")
        .filter_map(|t| t["name"].as_str())
        .filter(|n| n.starts_with("try-"))
        .collect();
    assert!(
        active_try.is_empty(),
        "found active try-* threads after failure: {active_try:?}"
    );
}

#[test]
fn try_auto_merge_advances_parent_head_on_success() {
    let temp = setup_repo();
    let head_before = parent_head(temp.path());

    // `true` succeeds but doesn't change anything in the worktree;
    // capture inside the thread will be empty. Touch a file inside
    // the thread checkout via a sub-shell so we have something to
    // merge. We do this through a small shell script: write a file
    // in the cwd (which `heddle try` sets to the thread's checkout)
    // and exit 0.
    //
    // The script lives outside the repo so we don't perturb the
    // worktree-invariant accounting in `worktree_snapshot`.
    let script_dir = TempDir::new().unwrap();
    let script_path = script_dir.path().join("touch.sh");
    fs::write(
        &script_path,
        "#!/bin/sh\nset -e\nprintf 'try-output\\n' > try-output.txt\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).unwrap();
    }

    let raw = heddle(
        &[
            "--output",
            "json",
            "try",
            "--auto-merge",
            "--",
            "sh",
            script_path.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("heddle try --auto-merge -- sh script should succeed");
    let value: Value = serde_json::from_str(&raw).expect("output should be JSON");

    assert_eq!(value["status"], "completed", "raw output: {raw}");
    assert_eq!(value["exit_code"], 0);
    assert!(
        value["captured_state"].is_string(),
        "should have a captured_state on success: {raw}"
    );
    assert!(
        value["merge_state"].is_string(),
        "should have a merge_state when --auto-merge is set: {raw}"
    );

    let head_after = parent_head(temp.path());
    assert_ne!(
        head_before, head_after,
        "parent HEAD should advance when --auto-merge is set"
    );

    // The parent's worktree now contains the file the script wrote
    // (because the merge integrated it). This is the *expected*
    // change with --auto-merge; not an invariant violation.
    assert!(
        temp.path().join("try-output.txt").exists(),
        "auto-merge should integrate files written inside the try thread"
    );
}

#[test]
fn try_records_command_in_capture_intent() {
    let temp = setup_repo();
    // Use a script that touches a file so the capture has actual
    // content (some Heddle paths skip empty captures).
    let script_dir = TempDir::new().unwrap();
    let script_path = script_dir.path().join("touch.sh");
    fs::write(
        &script_path,
        "#!/bin/sh\nprintf 'echo-output\\n' > echo-output.txt\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).unwrap();
    }

    let raw = heddle(
        &[
            "--output",
            "json",
            "try",
            "--",
            "sh",
            script_path.to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("heddle try -- sh script should succeed");
    let value: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["status"], "completed");
    let captured = value["captured_state"]
        .as_str()
        .expect("captured_state must be set on success with worktree changes");

    // Inspect the captured state's intent. The intent is rendered
    // into the human-readable `heddle show` output (and the JSON
    // shape if present); a quick contains() is enough for this
    // smoke check.
    let show = heddle(&["show", captured], Some(temp.path())).unwrap();
    assert!(
        show.contains("try:") && show.contains("sh"),
        "captured state intent should record the cmd; got: {show}"
    );
}

#[test]
fn try_with_explicit_name_uses_that_name() {
    let temp = setup_repo();
    let raw = heddle(
        &[
            "--output",
            "json",
            "try",
            "--name",
            "my-explicit-try",
            "--",
            "true",
        ],
        Some(temp.path()),
    )
    .expect("heddle try with --name should succeed");
    let value: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["thread"], "my-explicit-try");
}

/// `heddle try --name <existing>` must refuse rather than resume the
/// existing thread. `start_thread` is create-or-resume, so without
/// the upfront collision check a try would attach to the user's real
/// thread and the failure-path drop would then abandon it.
#[test]
fn try_rejects_existing_thread_name() {
    let temp = setup_repo();
    // Create the thread the user would later collide with.
    heddle(
        &["thread", "create", "feat/already-here"],
        Some(temp.path()),
    )
    .expect("thread create should succeed");

    let err = heddle(
        &[
            "--output",
            "json",
            "try",
            "--name",
            "feat/already-here",
            "--",
            "true",
        ],
        Some(temp.path()),
    )
    .expect_err("try with --name pointing at an existing thread must refuse");
    assert!(
        err.contains("already exists"),
        "error should explain the collision; got: {err}"
    );

    // The existing thread must NOT have been dropped/abandoned.
    let list_raw = heddle(
        &["--output", "json", "thread", "list", "--include-auto"],
        Some(temp.path()),
    )
    .unwrap();
    let list: Value = serde_json::from_str(&list_raw).unwrap();
    let still_present = list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"].as_str() == Some("feat/already-here"));
    assert!(
        still_present,
        "existing thread must survive the rejected try; got {list_raw}"
    );
}
