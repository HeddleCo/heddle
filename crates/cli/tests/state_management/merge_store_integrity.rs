// SPDX-License-Identifier: Apache-2.0
//! End-to-end guards for the silent-corruption class of bug behind
//! heddle#90 — recursive merge used `get_tree(...).unwrap_or_default()`
//! at every subtree-load site, so a missing subtree was indistinguishable
//! from an empty one and the merger happily produced a "clean" merge
//! that erased every file under the subtree.
//!
//! These tests drive the real `heddle merge` binary against a real
//! on-disk store. The first one introduces targeted corruption (deletes
//! a single subtree object) and asserts the merge fails loud rather
//! than committing data loss. The second one exercises a legitimate
//! "tree doesn't exist at this path" case (a directory rename) and
//! asserts the new error path doesn't false-positive.
use objects::store::ObjectStore;
use std::{fs, path::Path};

use objects::object::ThreadName;
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

/// Red-commit guard for heddle#90: a subtree that the merge wants to
/// recurse into MUST be present in the object store. If it isn't —
/// dropped pack, corrupted file, broken partial-fetch — the merge
/// MUST fail with a clear diagnostic instead of silently substituting
/// `Tree::default()` and producing a "clean" merge that erased every
/// file under that subtree.
///
/// Pre-#90 this test produced a successful merge whose result
/// dropped `sub/a.txt`; post-fix it fails with an error naming the
/// missing subtree.
#[test]
fn test_merge_missing_base_subtree_fails_loud_not_silent_erase() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    // Base state: two top-level entries, one of them a subdirectory
    // with content. Both branches will need to recurse into the
    // subdirectory during merge.
    fs::create_dir(temp.path().join("sub")).unwrap();
    fs::write(temp.path().join("sub/a.txt"), "base content\n").unwrap();
    fs::write(temp.path().join("top.txt"), "top base\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();

    // Feature side: modify the file inside sub/ so the merger has a
    // real reason to recurse into the subtree.
    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("sub/a.txt"), "feature content\n").unwrap();
    heddle(&["capture", "-m", "feature edits sub"], Some(temp.path())).unwrap();

    // Main side: modify the file inside sub/ differently. Forces the
    // three-way merger to load all three subtrees (base, ours, theirs)
    // rather than fast-forwarding past a side that didn't touch the
    // directory.
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("sub/a.txt"), "main content\n").unwrap();
    heddle(&["capture", "-m", "main edits sub"], Some(temp.path())).unwrap();

    // Locate the base-state's `sub/` subtree hash so we know which
    // loose object to delete. Opening a fresh `Repository` reads the
    // committed state; closing it before the merge subprocess
    // guarantees no in-process tree cache hides the corruption.
    let sub_hash_hex = {
        let repo = Repository::open(temp.path()).unwrap();
        let main_tip = repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap()
            .unwrap();
        let main_state = repo.store().get_state(&main_tip).unwrap().unwrap();
        // The merge base IS the initial state: main and feature both
        // forked from the initial capture, then captured once on each
        // side. We need the base state's tree, not main's tip tree.
        let feature_tip = repo
            .refs()
            .get_thread(&ThreadName::new("feature"))
            .unwrap()
            .unwrap();
        let feature_state = repo.store().get_state(&feature_tip).unwrap().unwrap();
        // Both tips have a single parent — the base capture.
        let base_change_id = main_state.parents[0];
        assert_eq!(
            base_change_id, feature_state.parents[0],
            "test setup: main and feature must share a single merge base",
        );
        let base_state = repo.store().get_state(&base_change_id).unwrap().unwrap();
        let base_tree = repo.store().get_tree(&base_state.tree).unwrap().unwrap();
        let sub_entry = base_tree
            .entries()
            .iter()
            .find(|e| e.name == "sub")
            .expect("base tree must contain the `sub` directory");
        assert!(
            sub_entry.is_tree(),
            "`sub` must be a Tree entry, was: {sub_entry:?}"
        );
        sub_entry.hash.to_hex()
    };

    // Tamper with the on-disk store. The merge subprocess starts cold
    // (no in-memory caches), so this is the exact failure mode a
    // user with a corrupt object would hit.
    let deleted = delete_loose_tree(temp.path(), &sub_hash_hex);
    assert!(
        deleted,
        "test setup: expected to find loose tree at hash {sub_hash_hex} to delete",
    );

    // The merge MUST fail. Pre-fix it succeeded and `sub/a.txt`
    // vanished from the merged tree without conflict markers.
    let result = heddle(&["--output", "json", "merge", "feature"], Some(temp.path()));
    let err = result.expect_err(
        "merge against a corrupt subtree must fail loud; \
         a clean Ok here means the silent-corruption bug regressed",
    );
    assert!(
        err.contains("missing from the object store"),
        "merge error must surface the missing-subtree diagnostic so the \
         operator can tell store corruption from a normal merge conflict; \
         got: {err}"
    );
    assert!(
        err.contains(&sub_hash_hex),
        "merge error must include the missing subtree's hash so the \
         operator can correlate with `heddle fsck` output; got: {err}"
    );
    assert!(
        err.contains("heddle fsck"),
        "merge error must point at the recovery command so the operator \
         has a next step instead of just a stack trace; got: {err}"
    );

    // Belt-and-suspenders: the merge must not have produced a partial
    // result on disk that overwrites `sub/a.txt` with the silent-
    // -default content. The worktree file should still hold main's
    // pre-merge content.
    let on_disk = fs::read_to_string(temp.path().join("sub/a.txt")).unwrap();
    assert_eq!(
        on_disk, "main content\n",
        "failed merge must not partially apply: sub/a.txt should still hold \
         main's pre-merge content, got: {on_disk:?}"
    );
}

/// Complementary guard: the new error path is a real corruption
/// signal, not a "tree was legitimately absent" false positive. A
/// directory that exists on one side and was renamed-away on the
/// other still merges cleanly (the rename-detector flattens the trees
/// before merging, so neither side asks for a missing subtree hash).
///
/// Without this test the fix to `require_subtree` could over-tighten:
/// any merge that touches a directory which was added/renamed/deleted
/// on the other side would start failing as "missing subtree" even
/// though the tree layer is intact. That would be a worse bug than the
/// silent corruption — a CI-breaking false-positive blocking real
/// merges.
#[test]
fn test_merge_with_directory_rename_succeeds_no_false_positive() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    // Base: a directory `old_dir/` with some content, plus an
    // independent top-level file so the merge has something
    // unambiguously non-conflicting to keep.
    fs::create_dir(temp.path().join("old_dir")).unwrap();
    fs::write(temp.path().join("old_dir/file.txt"), "content\n").unwrap();
    fs::write(temp.path().join("keep.txt"), "keep\n").unwrap();
    heddle(&["capture", "-m", "base"], Some(temp.path())).unwrap();

    // Feature side: rename old_dir/ → new_dir/. Heddle's rename
    // detector should pick this up so the merge doesn't see it as
    // "deleted old_dir + added new_dir".
    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::create_dir(temp.path().join("new_dir")).unwrap();
    fs::rename(
        temp.path().join("old_dir/file.txt"),
        temp.path().join("new_dir/file.txt"),
    )
    .unwrap();
    fs::remove_dir(temp.path().join("old_dir")).unwrap();
    heddle(
        &["capture", "-m", "rename old_dir to new_dir"],
        Some(temp.path()),
    )
    .unwrap();

    // Main side: touch the unrelated top-level file so the merge has
    // a non-trivial integration to do.
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("keep.txt"), "keep edited\n").unwrap();
    heddle(&["capture", "-m", "main edits keep"], Some(temp.path())).unwrap();

    // Direct merge now refuses stale threads before doing semantic
    // planning. Refresh first so this test keeps exercising the store
    // integrity path, not the freshness gate.
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "refresh", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();

    // The merge MUST succeed; the directory rename is a legitimate
    // "this subtree path doesn't exist on the other side" scenario,
    // and the require_subtree fix must not over-trigger here.
    let result = heddle(&["merge", "feature"], Some(temp.path()));
    result.expect(
        "merge across a clean directory rename must succeed; \
         a require_subtree failure here is a false-positive regression",
    );

    // Sanity: the rename actually applied and the keep.txt edit
    // survived.
    assert!(
        temp.path().join("new_dir/file.txt").exists(),
        "merged worktree must contain the renamed file at new_dir/file.txt"
    );
    assert!(
        !temp.path().join("old_dir").exists(),
        "merged worktree must not retain the pre-rename old_dir/"
    );
    let keep = fs::read_to_string(temp.path().join("keep.txt")).unwrap();
    assert_eq!(
        keep, "keep edited\n",
        "main's edit to keep.txt must survive the merge"
    );
}
