// SPDX-License-Identifier: Apache-2.0
//! `heddle clone` must never start the filesystem-monitor helper daemon.
//!
//! A clone materializes a worktree and exits. It has no ongoing worktree to
//! watch, but a monitor-backed status walk during clone verification used to
//! spawn `heddle-fsmonitor-worker` as a **child of the clone process** — so
//! anything waiting on the clone's process tree (CI harnesses, `strace -f`,
//! `/usr/bin/time`) blocked on the helper's idle lifetime long after the clone
//! itself had finished. A hosted git.git clone took 351 s of process-tree wall
//! against 35 s of actual clone work, with ~59k `accept4`-EAGAIN /
//! `clock_nanosleep` pairs from the helper's accept loop in between
//! (HeddleCo/heddle#1243).
//!
//! These tests are Linux-only because that is where the native backend is
//! supported (`fsmonitor::native_backend_supported`); elsewhere `auto` resolves
//! to "off" and the assertions would pass without proving anything.
#![cfg(target_os = "linux")]

use std::path::Path;

use tempfile::TempDir;

use super::*;

/// Files the monitor's start seam writes into a repository's state directory.
///
/// The two lock files are created **synchronously by the calling process** in
/// `fsmonitor::try_spawn_local_helper_with`, before the daemon is spawned, so
/// their absence right after a command returns is a deterministic "this command
/// never tried to start the helper" — there is no race against daemon startup.
const MONITOR_START_MARKERS: [&str; 3] = [
    ".heddle/state/monitor-helper-start.lock",
    ".heddle/state/monitor-helper-lifetime.lock",
    ".heddle/state/monitor-helper.json",
];

fn monitor_start_markers(root: &Path) -> Vec<&'static str> {
    MONITOR_START_MARKERS
        .into_iter()
        .filter(|relative| root.join(relative).exists())
        .collect()
}

/// Stop any helper a test started so the daemon does not outlive the `TempDir`
/// it is watching.
fn stop_monitor_helper(root: &Path) {
    let _ = repo::shutdown_local_monitor_helper(root);
}

fn status_with_native_monitor(root: &Path) {
    let output = heddle_output_with_env(&["status"], Some(root), &[("HEDDLE_FSMONITOR", "native")])
        .expect("run status in cloned repo");
    assert!(
        output.status.success(),
        "status in cloned repo failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn clone_does_not_start_the_fsmonitor_helper() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    std::fs::create_dir_all(&source).unwrap();
    heddle(&["init"], Some(&source)).expect("init source repo");
    std::fs::write(source.join("tracked.txt"), "tracked\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(&source)).expect("capture source repo");

    // `--output json` matters: on the native lane the clone only builds its
    // Repository Verification State — the walk that used to reach the monitor —
    // when it has a JSON envelope to put it in. A text-mode clone would pass
    // this assertion even with the coupling still in place.
    heddle(
        &[
            "--output",
            "json",
            "clone",
            source.to_str().expect("source path is utf8"),
            destination.to_str().expect("destination path is utf8"),
        ],
        Some(temp.path()),
    )
    .expect("clone source repo");

    assert_eq!(
        monitor_start_markers(&destination),
        Vec::<&str>::new(),
        "clone must not start the fsmonitor helper for the repository it just materialized",
    );

    // The negative case: the same predicate has to be able to observe a start,
    // otherwise the assertion above would hold for a clone that never ran. The
    // monitor is deferred to the cloned repo's first status, not removed.
    status_with_native_monitor(&destination);
    assert!(
        !monitor_start_markers(&destination).is_empty(),
        "post-clone status should still start the fsmonitor helper on demand",
    );

    stop_monitor_helper(&destination);
    stop_monitor_helper(&source);
}

#[test]
fn git_overlay_clone_does_not_start_the_fsmonitor_helper() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let destination = temp.path().join("work");
    let git_repo = SleyRepository::init_bare(&origin).expect("init bare git origin");
    let commit = git_commit_with_tree(
        &git_repo,
        Some("refs/heads/main"),
        git_empty_tree_oid(&git_repo),
        "seed",
        &[],
    );
    git_set_reference(&git_repo, "HEAD", commit);

    // No `--output json` here: the Git-import lane builds its verification
    // state unconditionally, so text mode already exercises the seam.
    heddle(
        &[
            "clone",
            origin.to_str().expect("origin path is utf8"),
            destination.to_str().expect("destination path is utf8"),
        ],
        Some(temp.path()),
    )
    .expect("clone bare git origin");

    assert_eq!(
        monitor_start_markers(&destination),
        Vec::<&str>::new(),
        "git-overlay clone must not start the fsmonitor helper either",
    );

    status_with_native_monitor(&destination);
    assert!(
        !monitor_start_markers(&destination).is_empty(),
        "post-clone status should still start the fsmonitor helper on demand",
    );

    stop_monitor_helper(&destination);
}
