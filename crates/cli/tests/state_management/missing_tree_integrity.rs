// SPDX-License-Identifier: Apache-2.0
//! End-to-end guards for the silent-corruption class of bug behind
//! heddle#93 — non-merge CLI paths used `get_tree(...)?.unwrap_or_default()`
//! at every subtree-load site, so a missing tree was indistinguishable
//! from an empty one and presentation paths (`status`, `ready`) silently
//! rendered "no content" while `revert` operated against an empty baseline.
//!
//! Mirrors `merge_store_integrity.rs` (the heddle#90 lock for the merge
//! engine). Each test introduces targeted corruption (deletes the loose
//! tree object backing a captured state) and asserts the CLI command
//! fails loud with a diagnostic naming the missing hash and pointing at
//! `heddle fsck` — pre-fix the same scenario produced silent, plausible-
//! looking output that masked store corruption.

use std::{fs, path::Path};

use objects::{object::ThreadName, store::ObjectStore};
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
