// SPDX-License-Identifier: Apache-2.0
//! End-to-end guards for the silent-corruption class of bug behind
//! heddle#93 — non-merge CLI paths used `get_tree(...)?.unwrap_or_default()`
//! at every subtree-load site, so a missing tree was indistinguishable
//! from an empty one and presentation paths (`status`, `ready`, `stash
//! show`) silently rendered "no content" while mutation paths (`revert`,
//! legacy tree-rewrite commands silently operated against an empty baseline.
//!
//! Mirrors `merge_store_integrity.rs` (the heddle#90 lock for the merge
//! engine). Each test introduces targeted corruption (deletes the loose
//! tree object backing a captured state) and asserts the CLI command
//! fails loud with a diagnostic naming the missing hash and pointing at
//! `heddle fsck` — pre-fix the same scenario produced silent, plausible-
//! looking output that masked store corruption.

use std::{fs, path::Path};

use objects::{
    object::{ContentHash, ThreadName},
    store::ObjectStore,
};
use repo::Repository;
use tempfile::TempDir;

use super::heddle;

/// Walk the loose-objects tree at `.heddle/objects/trees/<prefix>/<rest>`
/// and remove the file whose hash matches `target`. Returns whether a
/// matching file was actually deleted, so the test can assert the
/// tampering set up the corruption it intended.
fn delete_loose_tree(repo_root: &Path, target_hex: &str) -> bool {
    let prefix = &target_hex[..2];
    let rest = &target_hex[2..];
    let path = repo_root
        .join(".heddle/objects/trees")
        .join(prefix)
        .join(rest);
    match fs::remove_file(&path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!("failed to delete loose tree at {path:?}: {error}"),
    }
}

/// Look up the captured state's tree hash via a short-lived
/// `Repository` handle, closing it before the subprocess runs so no
/// in-memory cache can hide the corruption.
fn current_state_tree_hex(repo_root: &Path) -> String {
    let repo = Repository::open(repo_root).unwrap();
    let tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread must have a tip after capture");
    let state = repo
        .store()
        .get_state(&tip)
        .unwrap()
        .expect("state must exist after capture");
    state.tree.to_hex()
}

/// Assert the CLI error names the missing-tree diagnostic, includes the
/// missing hash so the operator can correlate with `heddle fsck` output,
/// and points at the recovery command so they have a next step instead
/// of just a stack trace. Used by every test in this module — the
/// contract is the same regardless of which command surfaced the error.
fn assert_missing_tree_error(err: &str, tree_hex: &str) {
    assert!(
        err.contains("missing") && err.contains("tree"),
        "error must surface the missing-tree diagnostic so the operator can tell \
         store corruption from a normal absent-state case; got: {err}"
    );
    assert!(
        err.contains(tree_hex),
        "error must include the missing tree's hash so the operator can correlate \
         with `heddle fsck` output; got: {err}"
    );
    assert!(
        err.contains("heddle fsck"),
        "error must point at the recovery command so the operator has a next step \
         instead of just a stack trace; got: {err}"
    );
}

/// `heddle status` — presentation path that compares the worktree
/// against the current state's baseline tree. Pre-#93 a missing tree
/// silently became `Tree::default()`, so `status` reported every tracked
/// file as `added` — masking the corruption with a plausible-looking
/// "you have a lot of new content" diff.
#[test]
fn test_status_missing_tree_fails_loud_not_silent_empty() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("a.txt"), "hello\n").unwrap();
    heddle(&["capture", "-m", "initial"], Some(temp.path())).unwrap();

    let tree_hex = current_state_tree_hex(temp.path());
    assert!(
        delete_loose_tree(temp.path(), &tree_hex),
        "test setup: expected to find loose tree at hash {tree_hex} to delete",
    );

    let err = heddle(&["status"], Some(temp.path())).expect_err(
        "status against a corrupt baseline tree must fail loud; \
         pre-#93 it silently rendered the entire worktree as 'added' content",
    );
    assert_missing_tree_error(&err, &tree_hex);
}

/// `heddle ready` — presentation path that asks `worktree_dirty?` to
/// gate the readiness report. Pre-#93 a missing baseline tree silently
/// became `Tree::default()`, so the dirty check ran against an empty
/// baseline and reported "dirty" for every tracked file regardless of
/// real state.
#[test]
fn test_ready_missing_tree_fails_loud_not_silent_empty() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("a.txt"), "hello\n").unwrap();
    heddle(&["capture", "-m", "initial"], Some(temp.path())).unwrap();

    let tree_hex = current_state_tree_hex(temp.path());
    assert!(
        delete_loose_tree(temp.path(), &tree_hex),
        "test setup: expected to find loose tree at hash {tree_hex} to delete",
    );

    let err = heddle(&["ready"], Some(temp.path())).expect_err(
        "ready against a corrupt baseline tree must fail loud; \
         pre-#93 it silently reported the worktree as dirty against an empty baseline",
    );
    assert_missing_tree_error(&err, &tree_hex);
}

/// `heddle revert <state>` — mutation path that loads the target
/// state's parent tree to compute the inverse diff. Pre-#93 a missing
/// parent tree silently became `Tree::default()`, so the revert diff
/// treated the parent as empty and produced an inverse that "removed"
/// every file in the state — quietly clobbering the worktree on apply.
#[test]
fn test_revert_missing_parent_tree_fails_loud_not_silent_empty() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("a.txt"), "first\n").unwrap();
    let first_capture = heddle(&["capture", "-m", "first"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("a.txt"), "second\n").unwrap();
    heddle(&["capture", "-m", "second"], Some(temp.path())).unwrap();

    // Parent of "second" is "first" — that's the tree we tamper with.
    // Pull the parent-tree hash via the on-disk state graph rather than
    // hardcoding by index so the test stays robust to future
    // intermediate-state insertions.
    let parent_tree_hex = {
        let repo = Repository::open(temp.path()).unwrap();
        let tip = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .unwrap();
        let second_state = repo.store().get_state(&tip).unwrap().unwrap();
        let parent_id = second_state
            .first_parent()
            .expect("second state must have a parent");
        let parent_state = repo.store().get_state(parent_id).unwrap().unwrap();
        parent_state.tree.to_hex()
    };
    assert!(
        delete_loose_tree(temp.path(), &parent_tree_hex),
        "test setup: expected to find loose tree at parent hash {parent_tree_hex}",
    );

    // We need the short change-id for "second" to feed to revert. Pull
    // it from the captured stdout of the second capture if available;
    // otherwise resolve it via @ (current).
    let _ = first_capture;
    let err = heddle(&["revert", "@"], Some(temp.path())).expect_err(
        "revert against a state whose parent tree is corrupt must fail loud; \
         pre-#93 it silently computed an inverse diff against an empty baseline",
    );
    assert_missing_tree_error(&err, &parent_tree_hex);
}

/// `heddle stash apply` (driven via `heddle stash pop`) — mutation
/// path that loads a stash's tree to write its files back to the
/// worktree. Pre-#93 a missing stash tree silently became
/// `Tree::default()`, so apply ran a zero-entry loop and reported
/// success without restoring anything.
#[test]
fn test_stash_pop_missing_tree_fails_loud_not_silent_noop() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("a.txt"), "baseline\n").unwrap();
    heddle(&["capture", "-m", "baseline"], Some(temp.path())).unwrap();

    // Dirty the worktree so `stash push` has something to stash.
    fs::write(temp.path().join("a.txt"), "dirty\n").unwrap();
    fs::write(temp.path().join("b.txt"), "new file\n").unwrap();
    heddle(&["stash", "push"], Some(temp.path())).unwrap();

    // Find the stash entry's tree hash by reading the stash manifest
    // directly. Stashes live under `.heddle/stash/` — the manifest
    // contains the tree hash hex string.
    let stash_tree_hex = {
        let repo = Repository::open(temp.path()).unwrap();
        let stash = repo
            .stash_manager()
            .top()
            .unwrap()
            .expect("stash must exist after push");
        // tree_hash is stored as the hex string already.
        ContentHash::from_hex(&stash.tree_hash)
            .expect("stash tree hash must be valid hex")
            .to_hex()
    };
    assert!(
        delete_loose_tree(temp.path(), &stash_tree_hex),
        "test setup: expected to find loose tree at stash hash {stash_tree_hex}",
    );

    let err = heddle(&["stash", "pop"], Some(temp.path())).expect_err(
        "stash pop against a corrupt stash tree must fail loud; \
         pre-#93 it silently completed with no entries applied",
    );
    assert_missing_tree_error(&err, &stash_tree_hex);
}

/// `heddle clean --force` — destructive mutation path that loads the
/// current state's tree to distinguish tracked from untracked files.
/// Pre-#93 a missing tree silently became `Tree::default()`, so the
/// detailed-status comparison reported every tracked file as
/// `untracked` and `clean --force` deleted them all — a corrupt repo
/// silently became an empty worktree. The highest-stakes site in the
/// Rule-7 sweep; this test pins the loud-failure contract.
#[test]
fn test_clean_force_missing_tree_fails_loud_not_wipe_tracked_files() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("tracked.txt"), "tracked\n").unwrap();
    heddle(&["capture", "-m", "initial"], Some(temp.path())).unwrap();

    let tree_hex = current_state_tree_hex(temp.path());
    assert!(
        delete_loose_tree(temp.path(), &tree_hex),
        "test setup: expected to find loose tree at hash {tree_hex} to delete",
    );

    let err = heddle(&["clean", "--force"], Some(temp.path())).expect_err(
        "clean --force against a corrupt baseline tree must fail loud; \
         pre-#93 it silently deleted every tracked file in the worktree",
    );
    assert_missing_tree_error(&err, &tree_hex);

    // Belt-and-suspenders: the tracked file must still exist on disk.
    // If the migration regressed, clean would have already deleted it.
    assert!(
        temp.path().join("tracked.txt").exists(),
        "failed clean must not partially delete: tracked.txt should still exist",
    );
}

/// `heddle switch <state>` — mutation path that loads the *current*
/// state's tree to verify the worktree is clean before switching.
/// Pre-#93 a missing current tree silently became `Tree::default()`,
/// so the cleanliness check compared the worktree to an empty baseline
/// and bailed with "uncommitted changes" — masking the corruption with
/// a confusing-but-plausible error rather than a fail-loud diagnostic.
#[test]
fn test_goto_missing_current_tree_fails_loud_not_silent_dirty() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("a.txt"), "first\n").unwrap();
    heddle(&["capture", "-m", "first"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("a.txt"), "second\n").unwrap();
    heddle(&["capture", "-m", "second"], Some(temp.path())).unwrap();

    // Tamper with the current (second) state's tree — `goto` reads it
    // to verify worktree cleanliness when force is false. Capture the
    // first state's state_id at the same time so we have a real goto
    // target (heddle has no `main~1` rev-parse syntax).
    let (current_tree_hex, first_state_id) = {
        let repo = Repository::open(temp.path()).unwrap();
        let tip = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .unwrap();
        let second_state = repo.store().get_state(&tip).unwrap().unwrap();
        let parent_id = second_state
            .first_parent()
            .copied()
            .expect("second state must have a parent");
        (second_state.tree.to_hex(), parent_id.to_string())
    };
    assert!(
        delete_loose_tree(temp.path(), &current_tree_hex),
        "test setup: expected to find loose tree at current hash {current_tree_hex}",
    );

    // Try to goto the first state — the cleanliness check on the
    // current tree must fail loud, not bail with a misleading
    // "uncommitted changes" message.
    let err = heddle(&["switch", &first_state_id], Some(temp.path())).expect_err(
        "goto against a corrupt current tree must fail loud; \
         pre-#93 it silently treated the worktree as dirty against an empty baseline",
    );
    assert_missing_tree_error(&err, &current_tree_hex);
}
