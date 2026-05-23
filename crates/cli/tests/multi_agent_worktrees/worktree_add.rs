// SPDX-License-Identifier: Apache-2.0
use std::os::unix::fs::symlink;

use super::*;

#[test]
fn materialized_start_writes_base_state_files() {
    let main = setup_repo("hello.txt", "world");
    let thread_dir = TempDir::new().unwrap();

    heddle(
        &[
            "start",
            "feature/materialized",
            "--workspace",
            "materialized",
            "--path",
            thread_dir.path().to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();

    let f = thread_dir.path().join("hello.txt");
    assert!(f.exists(), "base-state file should be materialized");
    assert_eq!(fs::read_to_string(&f).unwrap(), "world");
}

#[test]
fn top_level_start_defaults_to_lightweight_in_auto_mode() {
    let main = setup_repo("hello.txt", "world");

    let output = heddle(
        &["--json", "start", "feature/default-visible"],
        Some(main.path()),
    )
    .unwrap();
    let started: Value = serde_json::from_str(&output).unwrap();

    // Top-level `start` with no `--path` resolves to ThreadMode::Materialized
    // on filesystems that support reflinks (APFS, btrfs, xfs+reflink). On
    // ext4 / HFS+ / NTFS the auto-mode probe downgrades to `solid` so the
    // mode label matches what's actually on disk. Both outcomes are correct
    // — assert the FS-conditional shape.
    let expected_mode = if objects::fs_clone::filesystem_supports_reflink(main.path()) {
        "materialized"
    } else {
        "solid"
    };
    assert_eq!(started["thread"]["thread_mode"], expected_mode);
    assert_eq!(started["path"], started["execution_path"]);
    assert!(
        started["execution_path"].as_str().is_some(),
        "auto-mode thread still has a managed execution path"
    );
}

#[test]
fn materialized_start_honors_from_state() {
    let main = setup_repo("v.txt", "v1");

    fs::write(main.path().join("v.txt"), "v2").unwrap();
    heddle(&["capture", "-m", "v2"], Some(main.path())).unwrap();

    let thread_dir = TempDir::new().unwrap();
    heddle(
        &[
            "start",
            "feature/from-old",
            "--workspace",
            "materialized",
            "--path",
            thread_dir.path().to_str().unwrap(),
            "--from",
            "HEAD~1",
        ],
        Some(main.path()),
    )
    .unwrap();

    let content = fs::read_to_string(thread_dir.path().join("v.txt")).unwrap();
    assert_eq!(content, "v1", "--from HEAD~1 should materialize v1");
}

#[test]
fn thread_promote_preserves_thread_identity() {
    let main = setup_repo("base.txt", "base");
    let thread_dir = TempDir::new().unwrap();

    heddle(
        &["start", "feature/promote-me", "--workspace", "auto"],
        Some(main.path()),
    )
    .unwrap();
    let out = heddle(
        &[
            "--json",
            "thread",
            "promote",
            "feature/promote-me",
            "--path",
            thread_dir.path().to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();

    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["thread"]["id"].as_str(), Some("feature/promote-me"));
    assert_eq!(v["thread"]["mode"].as_str(), Some("solid"));
    assert!(thread_dir.path().join(".heddle").join("HEAD").exists());
}

#[test]
fn thread_drop_removes_materialized_checkout_and_optionally_thread_ref() {
    let main = setup_repo("base.txt", "base");
    let checkout = TempDir::new().unwrap();

    heddle(
        &[
            "start",
            "feature/remove-thread",
            "--workspace",
            "materialized",
            "--path",
            checkout.path().to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();

    heddle(
        &["thread", "drop", "feature/remove-thread", "--delete-thread"],
        Some(main.path()),
    )
    .unwrap();

    assert!(!checkout.path().exists(), "checkout should be deleted");
    let out = heddle(&["--json", "thread", "list"], Some(main.path())).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let threads = v["threads"].as_array().unwrap();
    assert!(
        !threads
            .iter()
            .any(|thread| thread["name"] == "feature/remove-thread")
    );
}

#[test]
fn rejects_symlink_target_path_for_materialized_start() {
    let main = setup_repo("base.txt", "base");
    let temp = TempDir::new().unwrap();
    let real_target = temp.path().join("real-target");
    fs::create_dir(&real_target).unwrap();
    let symlink_target = temp.path().join("linked-target");
    symlink(&real_target, &symlink_target).unwrap();

    let result = heddle(
        &[
            "start",
            "feature/symlink",
            "--workspace",
            "materialized",
            "--path",
            symlink_target.to_str().unwrap(),
        ],
        Some(main.path()),
    );

    assert!(result.is_err(), "symlink target path should be rejected");
}

/// Regression test for the demo-geometry bug: a thread worktree
/// materialized *inside* another thread's worktree (e.g. nested under
/// the parent repo via `--path agents/X`) used to leak files into the
/// parent's tree. The structural fix excludes other threads' recorded
/// worktrees from the parent's scans.
#[test]
fn test_snapshot_excludes_nested_thread_worktrees() {
    let main = setup_repo("hello.txt", "world");
    let nested = main.path().join("agents").join("approach-x");
    fs::create_dir_all(&nested).unwrap();

    heddle(
        &[
            "start",
            "feature/nested-x",
            "--workspace",
            "materialized",
            "--path",
            nested.to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();

    // Edit a file *only* inside the nested child worktree.
    fs::write(nested.join("agent-only.txt"), "child work").unwrap();

    // Switch HEAD back to main so the parent scan is from main's POV.
    heddle(&["thread", "switch", "main"], Some(main.path())).unwrap();

    // Parent's status must be clean: the nested file belongs to the
    // child thread, not to main.
    let status_json = heddle(&["--json", "status"], Some(main.path())).unwrap();
    let status: Value = serde_json::from_str(&status_json).unwrap();
    let added = status["changes"]["added"].as_array().unwrap();
    assert!(
        !added.iter().any(|v| {
            let s = v.as_str().unwrap_or("");
            s.contains("agent-only.txt") || s.contains("agents/approach-x")
        }),
        "parent's status must not include the nested child's files; got added={:?}",
        added
    );

    // Snapshot from main: must be a no-op (no new state created on
    // top of init, OR the new state must not pull in the nested file).
    heddle(&["capture", "-m", "post-nested"], Some(main.path())).unwrap();
    let log = heddle(&["--json", "log"], Some(main.path())).unwrap();
    assert!(
        !log.contains("agent-only.txt"),
        "snapshot must not capture the nested child's file"
    );
}

/// The child thread's `heddle status` from inside its own nested
/// worktree must still see its own changes. Only the parent must
/// exclude nested children.
#[test]
fn test_status_distinguishes_own_worktree_from_nested_threads() {
    let main = setup_repo("hello.txt", "world");
    let nested = main.path().join("agents").join("approach-y");
    fs::create_dir_all(&nested).unwrap();

    heddle(
        &[
            "start",
            "feature/nested-y",
            "--workspace",
            "materialized",
            "--path",
            nested.to_str().unwrap(),
        ],
        Some(main.path()),
    )
    .unwrap();
    fs::write(nested.join("agent-only.txt"), "child work").unwrap();

    // From inside the child's worktree: own changes ARE visible.
    let child_status = heddle(&["--json", "status"], Some(&nested)).unwrap();
    let child: Value = serde_json::from_str(&child_status).unwrap();
    let child_added = child["changes"]["added"].as_array().unwrap();
    assert!(
        child_added
            .iter()
            .any(|v| v.as_str().unwrap_or("").contains("agent-only.txt")),
        "child thread's own status must include its own untracked file; got added={:?}",
        child_added
    );

    // From the parent (after switching off the child thread): clean.
    heddle(&["thread", "switch", "main"], Some(main.path())).unwrap();
    let parent_status = heddle(&["--json", "status"], Some(main.path())).unwrap();
    let parent: Value = serde_json::from_str(&parent_status).unwrap();
    let parent_added = parent["changes"]["added"].as_array().unwrap();
    assert!(
        !parent_added
            .iter()
            .any(|v| v.as_str().unwrap_or("").contains("agent-only.txt")),
        "parent's status must NOT include the nested child's file; got added={:?}",
        parent_added
    );
}

/// `heddle delegate --path-prefix <inside repo>` must print a
/// one-line warning. A sibling path must not.
#[test]
fn test_delegate_warns_when_path_prefix_inside_repo() {
    let main = setup_repo("hello.txt", "world");

    // A path strictly under the repo root: should warn.
    let inside_prefix = main.path().join("agents");
    fs::create_dir_all(&inside_prefix).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd.args([
        "delegate",
        "task-inside",
        "--workspace",
        "materialized",
        "--path-prefix",
    ]);
    cmd.arg(inside_prefix.to_str().unwrap());
    cmd.current_dir(main.path());
    let output = cmd.output().expect("spawn heddle");
    let stderr_inside = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr_inside.contains("nested inside repo root"),
        "expected nested-warning on stderr; got: {}",
        stderr_inside
    );

    // A sibling path (outside repo root): no warning.
    let sibling_temp = TempDir::new().unwrap();
    let mut cmd2 = Command::new(env!("CARGO_BIN_EXE_heddle"));
    cmd2.args([
        "delegate",
        "task-sibling",
        "--workspace",
        "materialized",
        "--path-prefix",
    ]);
    cmd2.arg(sibling_temp.path().to_str().unwrap());
    cmd2.current_dir(main.path());
    let output2 = cmd2.output().expect("spawn heddle");
    let stderr_sibling = String::from_utf8_lossy(&output2.stderr).to_string();
    assert!(
        !stderr_sibling.contains("nested inside repo root"),
        "expected NO nested-warning for sibling path; got: {}",
        stderr_sibling
    );
}
