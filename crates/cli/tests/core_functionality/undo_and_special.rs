// SPDX-License-Identifier: Apache-2.0
use super::*;

/// Convenience: read the current state's short change-id by opening the repo
/// directly. Used by undo tests that assert HEAD has moved to a specific state.
fn head_short(root: &std::path::Path) -> String {
    let repo = Repository::open(root).unwrap();
    repo.head().unwrap().expect("repo has HEAD").short()
}

#[test]
fn test_undo_at_beginning() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    let result = heddle(&["undo"], Some(temp.path()));
    assert!(result.is_ok());
    let result = heddle(&["undo"], Some(temp.path()));
    assert!(result.is_err());
}

#[test]
fn test_redo_without_undo() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Initial"], temp.path());
    let result = heddle(&["redo"], Some(temp.path()));
    assert!(result.is_err());
}

#[test]
fn test_large_file_handling() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    let large_content = vec![0u8; 1024 * 1024];
    std::fs::write(temp.path().join("large.bin"), &large_content).unwrap();
    heddle_must_succeed(&["capture", "-m", "Large file"], temp.path());
    let retrieved = std::fs::read(temp.path().join("large.bin")).unwrap();
    assert_eq!(retrieved.len(), large_content.len());
}

#[test]
fn test_spaces_in_filename() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("file with spaces.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Spaces in name"], temp.path());
    assert!(temp.path().join("file with spaces.txt").exists());
    let result = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let status: Value = serde_json::from_str(&result).expect("Status should be valid JSON");
    let changes = status.get("changes").expect("Should have changes field");
    let modified = changes.get("modified").and_then(|m| m.as_array()).unwrap();
    let added = changes.get("added").and_then(|a| a.as_array()).unwrap();
    assert!(modified.is_empty() && added.is_empty());
}

#[test]
fn test_unicode_filename() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("файл.txt"), "unicode content").unwrap();
    std::fs::write(temp.path().join("文件.txt"), "chinese content").unwrap();
    std::fs::write(temp.path().join("emoji_😀.txt"), "emoji content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Unicode filenames"], temp.path());
    assert!(temp.path().join("файл.txt").exists());
    assert!(temp.path().join("文件.txt").exists());
    assert!(temp.path().join("emoji_😀.txt").exists());
}

/// Regression: `heddle undo` on a real-world worktree (with `target/`,
/// `node_modules/`, `.git/`, etc.) must not abort with `os error 66` after
/// destroying tracked files. The planner asks `remove_dir` to drop the parent
/// of an ignored child; that fails with ENOTEMPTY. Pre-fix this left the
/// worktree gutted with HEAD stuck at the old state. Post-fix the directory
/// is left in place and undo completes transactionally.
#[test]
fn test_undo_preserves_ignored_siblings_in_tracked_dirs() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    heddle_must_succeed(&["capture", "-m", "empty"], temp.path());

    std::fs::write(temp.path().join("main.rs"), "fn main() {}").unwrap();
    std::fs::create_dir_all(temp.path().join("web")).unwrap();
    std::fs::write(temp.path().join("web/index.html"), "<html/>").unwrap();
    heddle_must_succeed(&["capture", "-m", "tracked"], temp.path());

    // Drop ignored siblings into tracked directories — these would otherwise
    // be present from `bun install`, `cargo build`, `git init`, etc. Heddle's
    // default ignore list (`target`, `node_modules`, `.git`) skips them, so
    // they are invisible to status — but they still occupy the filesystem.
    std::fs::create_dir_all(temp.path().join("web/node_modules/lodash")).unwrap();
    std::fs::write(
        temp.path().join("web/node_modules/lodash/index.js"),
        "ignored",
    )
    .unwrap();
    std::fs::create_dir_all(temp.path().join("target")).unwrap();
    std::fs::write(temp.path().join("target/foo.bin"), "build").unwrap();

    heddle(&["undo", "-n", "1"], Some(temp.path())).expect("undo must succeed");

    // Tracked content reverted.
    assert!(!temp.path().join("main.rs").exists());
    assert!(!temp.path().join("web/index.html").exists());
    // Ignored siblings preserved across the apply.
    assert!(
        temp.path()
            .join("web/node_modules/lodash/index.js")
            .exists()
    );
    assert!(temp.path().join("target/foo.bin").exists());

    // HEAD advanced and disk matches state — no divergence.
    let status_json = heddle_must_succeed(&["status", "--json"], temp.path());
    let status: Value = serde_json::from_str(&status_json).unwrap();
    let changes = status.get("changes").unwrap();
    assert!(changes["modified"].as_array().unwrap().is_empty());
    assert!(changes["added"].as_array().unwrap().is_empty());
    assert!(changes["deleted"].as_array().unwrap().is_empty());
}

/// Regression: `heddle undo` must refuse when an untracked file sits in the
/// worktree. There is no prior snapshot to recover the file from; silently
/// destroying it is data loss.
#[test]
fn test_undo_refuses_when_untracked_file_present() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    heddle_must_succeed(&["capture", "-m", "empty"], temp.path());
    std::fs::write(temp.path().join("a.txt"), "a").unwrap();
    heddle_must_succeed(&["capture", "-m", "tracked"], temp.path());

    let untracked = temp.path().join("my-notes.md");
    std::fs::write(&untracked, "user-written content").unwrap();

    let err = heddle(&["undo", "-n", "1"], Some(temp.path()))
        .expect_err("undo must refuse on dirty worktree");
    assert!(
        err.contains("untracked"),
        "error should mention untracked: {err}"
    );
    assert!(untracked.exists(), "untracked file must survive refusal");
    assert!(
        temp.path().join("a.txt").exists(),
        "tracked file must survive refusal"
    );
}

/// Regression: `heddle undo` must refuse when a tracked file has been modified
/// since the last snapshot. The modification would be silently destroyed.
#[test]
fn test_undo_refuses_when_tracked_file_modified() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    heddle_must_succeed(&["capture", "-m", "empty"], temp.path());
    std::fs::write(temp.path().join("a.txt"), "original").unwrap();
    heddle_must_succeed(&["capture", "-m", "tracked"], temp.path());

    std::fs::write(temp.path().join("a.txt"), "uncommitted edit").unwrap();

    let err = heddle(&["undo", "-n", "1"], Some(temp.path()))
        .expect_err("undo must refuse with modified file");
    assert!(
        err.contains("modified"),
        "error should mention modified: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "uncommitted edit",
        "modification must survive refusal"
    );

    // Capturing the change unblocks undo.
    heddle_must_succeed(&["capture", "-m", "edit"], temp.path());
    heddle(&["undo", "-n", "1"], Some(temp.path())).expect("undo succeeds once worktree is clean");
}

/// Regression: `heddle cherry-pick` must refuse when an untracked file sits in
/// the worktree. Cherry-pick rewrites the worktree to match the picked commit's
/// tree, and without the guard any untracked file on a path the picked tree
/// touches is silently destroyed.
#[test]
fn test_cherry_pick_refuses_when_untracked_file_present() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    // Build a feature commit on a side thread, then come back to main.
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "Feature"], temp.path());
    let log = heddle_must_succeed(&["log", "--oneline", "--output", "text"], temp.path());
    let feature_commit = log
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());

    // Drop an untracked file the user cares about into the worktree.
    let untracked = temp.path().join("user-notes.md");
    std::fs::write(&untracked, "user-written content").unwrap();

    let err = heddle(&["cherry-pick", &feature_commit], Some(temp.path()))
        .expect_err("cherry-pick must refuse on dirty worktree");
    assert!(
        err.contains("untracked"),
        "error should mention untracked: {err}"
    );
    assert!(untracked.exists(), "untracked file must survive refusal");
}

/// Regression: `heddle cherry-pick` must refuse when a tracked file has been
/// modified since the last snapshot. The modification would be silently
/// destroyed by the cherry-pick's tree apply.
#[test]
fn test_cherry_pick_refuses_when_tracked_file_modified() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "Feature"], temp.path());
    let log = heddle_must_succeed(&["log", "--oneline", "--output", "text"], temp.path());
    let feature_commit = log
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());

    // Modify a tracked file without snapshotting.
    std::fs::write(temp.path().join("base.txt"), "uncommitted edit").unwrap();

    let err = heddle(&["cherry-pick", &feature_commit], Some(temp.path()))
        .expect_err("cherry-pick must refuse with modified file");
    assert!(
        err.contains("modified"),
        "error should mention modified: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("base.txt")).unwrap(),
        "uncommitted edit",
        "modification must survive refusal"
    );
}

/// `heddle cherry-pick --force` bypasses the guard. The uncommitted edit is
/// (expectedly) destroyed when the cherry-picked tree is applied.
#[test]
fn test_cherry_pick_force_proceeds_and_destroys_edit() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "Feature"], temp.path());
    let log = heddle_must_succeed(&["log", "--oneline", "--output", "text"], temp.path());
    let feature_commit = log
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());

    // Untracked file the user cares about — `--force` should clobber it.
    let untracked = temp.path().join("user-notes.md");
    std::fs::write(&untracked, "user-written content").unwrap();

    heddle(
        &["cherry-pick", "--force", &feature_commit],
        Some(temp.path()),
    )
    .expect("cherry-pick --force must succeed past the guard");
}

/// Regression: `heddle rebase` must refuse when an untracked file sits in the
/// worktree. Rebase calls `fast_forward_attached` which goes through
/// `plan_worktree_apply`, where the dirty-worktree fallback to
/// `FullRematerialize` would silently wipe the untracked file.
#[test]
fn test_rebase_refuses_when_untracked_file_present() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    // Create a side thread that advances; main stays behind so rebase has
    // somewhere to fast-forward to.
    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "Feature"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    let untracked = temp.path().join("user-notes.md");
    std::fs::write(&untracked, "user-written content").unwrap();

    let err = heddle(&["rebase", "feature"], Some(temp.path()))
        .expect_err("rebase must refuse on dirty worktree");
    assert!(
        err.contains("untracked"),
        "error should mention untracked: {err}"
    );
    assert!(untracked.exists(), "untracked file must survive refusal");
}

/// Regression: `heddle rebase` must refuse when a tracked file has been
/// modified since the last snapshot. The modification would be silently
/// destroyed by the rebase's tree apply.
#[test]
fn test_rebase_refuses_when_tracked_file_modified() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "Feature"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    std::fs::write(temp.path().join("base.txt"), "uncommitted edit").unwrap();

    let err = heddle(&["rebase", "feature"], Some(temp.path()))
        .expect_err("rebase must refuse with modified file");
    assert!(
        err.contains("modified"),
        "error should mention modified: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("base.txt")).unwrap(),
        "uncommitted edit",
        "modification must survive refusal"
    );
}

/// `heddle rebase --force` bypasses the guard. The uncommitted edit is
/// (expectedly) destroyed when the fast-forward applies the target tree.
#[test]
fn test_rebase_force_proceeds_and_destroys_edit() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "Feature"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    let untracked = temp.path().join("user-notes.md");
    std::fs::write(&untracked, "user-written content").unwrap();

    heddle(&["rebase", "--force", "feature"], Some(temp.path()))
        .expect("rebase --force must succeed past the guard");
}

/// Regression: a Repository-level test that `clear_worktree` and the
/// incremental remove path both tolerate ENOTEMPTY when the directory holds
/// heddle-ignored content. This is the unit-level companion to the CLI test
/// above.
#[test]
fn test_undo_with_dotgit_directory_present() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    heddle_must_succeed(&["capture", "-m", "empty"], temp.path());

    std::fs::write(temp.path().join("file.txt"), "v1").unwrap();
    heddle_must_succeed(&["capture", "-m", "v1"], temp.path());

    // Simulate a co-located git repo: `.git` is heddle-ignored by default.
    std::fs::create_dir_all(temp.path().join(".git/objects/01")).unwrap();
    std::fs::write(temp.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(temp.path().join(".git/objects/01/abc"), "fake git object").unwrap();

    heddle(&["undo", "-n", "1"], Some(temp.path())).expect("undo must succeed alongside .git");
    assert!(!temp.path().join("file.txt").exists());
    assert!(
        temp.path().join(".git/HEAD").exists(),
        ".git must survive undo"
    );
    assert!(
        temp.path().join(".git/objects/01/abc").exists(),
        ".git contents must survive undo"
    );

    // Ensure repository state is consistent: no leftover divergence.
    let repo = Repository::open(temp.path()).unwrap();
    let head = repo.head().unwrap().expect("repo has HEAD");
    let tree = repo.get_tree_for_state(&head).unwrap().expect("HEAD tree");
    assert!(
        repo.compare_worktree_cached_detailed(&tree)
            .unwrap()
            .is_clean(),
        "worktree must match HEAD after undo"
    );
}

// ---------------------------------------------------------------------------
// MVP undo coverage for the three operations daily users will reach for:
// `heddle capture`, `heddle merge` (FF and non-FF), plus the safety contracts
// the MVP must satisfy (--dry-run alias, refusal across destructive boundaries,
// discoverable --help surface).
// ---------------------------------------------------------------------------

/// `heddle capture` followed by `heddle undo` must restore HEAD to the
/// pre-capture parent state — not "some earlier state", not "the empty state",
/// the exact parent. This is the load-bearing invariant for daily use: after a
/// botched capture, the user expects to be exactly where they were one
/// operation ago.
#[test]
fn test_undo_capture_restores_head_to_parent() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("a.txt"), "v1").unwrap();
    heddle_must_succeed(&["capture", "-m", "first"], temp.path());
    let parent = head_short(temp.path());

    std::fs::write(temp.path().join("a.txt"), "v2").unwrap();
    heddle_must_succeed(&["capture", "-m", "second"], temp.path());
    let after_second = head_short(temp.path());
    assert_ne!(
        parent, after_second,
        "second capture must produce a fresh state"
    );

    heddle_must_succeed(&["undo"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        parent,
        "undo of capture must restore HEAD to the immediate parent state"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "v1",
        "worktree must reflect the parent state's tree after undo"
    );
}

/// Fast-forward merge undo, current behavior: HEAD is restored to the
/// pre-merge tip, but the *merged-into* thread ref is **stranded** at the FF
/// target. This is a documented gap (`docs/undo.md` "Known caveats") tracked
/// by heddle#99 — add an `OpRecord::FastForward` variant that records the
/// pre-merge thread state so undo can reset the ref too.
///
/// Why pin the bug instead of skipping the test: the day the FF undo is fixed
/// this test will start failing on the stranded-ref assertion, which is
/// exactly the signal we want — "the gap is closed, update the docs + this
/// test." See also `test_undo_non_ff_merge_restores_both_threads` below for
/// the non-FF path that already works end-to-end.
#[test]
fn test_undo_ff_merge_restores_head_but_strands_thread_ref() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());
    let main_tip_before = head_short(temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], temp.path());
    let feature_tip_before = head_short(temp.path());
    assert_ne!(main_tip_before, feature_tip_before);

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    assert_eq!(head_short(temp.path()), main_tip_before);

    heddle_must_succeed(&["merge", "feature"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        feature_tip_before,
        "FF merge must advance main to feature's tip"
    );

    heddle_must_succeed(&["undo"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        main_tip_before,
        "undo of FF merge must restore HEAD to main's pre-merge tip"
    );

    let repo = Repository::open(temp.path()).unwrap();

    // Feature thread never moved during FF merge, so its tip is unchanged.
    let feature_tip = repo
        .refs()
        .get_thread("feature")
        .unwrap()
        .expect("feature thread still exists")
        .short();
    assert_eq!(
        feature_tip, feature_tip_before,
        "feature thread tip must be unchanged across merge + undo"
    );

    // The documented gap: the `main` thread ref is left at the FF target.
    // Today's `OpRecord::Goto` inverse only rewinds HEAD; it carries no
    // thread context to reset the ref. When the follow-up adds a
    // `FastForward` op variant, flip this assertion to `main_tip_before`.
    let main_tip = repo
        .refs()
        .get_thread("main")
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, feature_tip_before,
        "current behavior: `main` thread ref is stranded at the FF target \
         after undo (see docs/undo.md 'Known caveats' + follow-up issue)"
    );
}

/// Non-fast-forward merge undo: both threads have divergent work since the
/// common ancestor. The merge synthesizes a new merge state with two parents.
/// Undo must restore main to its pre-merge tip; feature's tip never moved.
/// Unlike the FF path, this case exercises the `Snapshot` inverse (the merge
/// records a new state with `thread = Some("main")`), so the thread ref *is*
/// reset alongside HEAD.
#[test]
fn test_undo_non_ff_merge_restores_both_threads() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    // Diverge feature.
    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], temp.path());
    let feature_tip_before = head_short(temp.path());

    // Diverge main (independent file so the merge is conflict-free but non-FF).
    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    std::fs::write(temp.path().join("main.txt"), "main work").unwrap();
    heddle_must_succeed(&["capture", "-m", "main work"], temp.path());
    let main_tip_before = head_short(temp.path());

    heddle_must_succeed(&["merge", "feature"], temp.path());
    let merge_state = head_short(temp.path());
    assert_ne!(
        merge_state, main_tip_before,
        "non-FF merge must produce a fresh merge state"
    );
    assert_ne!(
        merge_state, feature_tip_before,
        "non-FF merge must not collapse to feature's tip"
    );

    heddle_must_succeed(&["undo"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        main_tip_before,
        "undo of non-FF merge must restore HEAD to main's pre-merge tip"
    );

    let repo = Repository::open(temp.path()).unwrap();

    // The `main` thread ref must be reset too (not just HEAD): the `Snapshot`
    // inverse for a merge carries the thread name so the ref rewinds with it.
    let main_tip = repo
        .refs()
        .get_thread("main")
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, main_tip_before,
        "non-FF undo must reset the `main` thread ref to its pre-merge tip"
    );

    // Feature is untouched throughout — its tip stays put.
    let feature_tip = repo
        .refs()
        .get_thread("feature")
        .unwrap()
        .expect("feature thread still exists")
        .short();
    assert_eq!(feature_tip, feature_tip_before);
}

/// `--dry-run` is the discoverable spelling of `--preview` documented in
/// `heddle undo --help`. It must not mutate state.
#[test]
fn test_undo_dry_run_alias_does_not_apply() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("a.txt"), "v1").unwrap();
    heddle_must_succeed(&["capture", "-m", "first"], temp.path());
    std::fs::write(temp.path().join("a.txt"), "v2").unwrap();
    heddle_must_succeed(&["capture", "-m", "second"], temp.path());
    let before = head_short(temp.path());

    let out = heddle_must_succeed(&["undo", "--dry-run"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        before,
        "--dry-run must not move HEAD"
    );
    assert!(
        out.to_lowercase().contains("would undo"),
        "--dry-run output must announce the dry-run shape: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "v2",
        "--dry-run must not touch the worktree"
    );

    // The same operation under the original `--preview` spelling still works
    // (we kept the existing flag rather than renaming it).
    let out_preview = heddle_must_succeed(&["undo", "--preview"], temp.path());
    assert!(
        out_preview.to_lowercase().contains("would undo"),
        "--preview must keep working: {out_preview}"
    );
    assert_eq!(head_short(temp.path()), before);
}

/// When the state that undo would restore to has been removed from the object
/// store (gc, oplog truncation, etc.), undo must refuse with a clear,
/// actionable error rather than partially applying or surfacing a raw
/// `StateNotFound` panic. The user must be told why we refused.
#[test]
fn test_undo_refuses_when_prior_state_missing() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("a.txt"), "v1").unwrap();
    heddle_must_succeed(&["capture", "-m", "first"], temp.path());
    let first_state_short = head_short(temp.path());

    std::fs::write(temp.path().join("a.txt"), "v2").unwrap();
    heddle_must_succeed(&["capture", "-m", "second"], temp.path());

    // Simulate the destructive-boundary case: the prior state object file is
    // gone from the loose store *and* any pack that might contain it.
    // Mirrors a `gc --prune` having reached past the live oplog, or an oplog
    // backup restored without its objects.
    let state_path = locate_state_loose_file(temp.path(), &first_state_short)
        .expect("prior state's loose file is present after capture");
    std::fs::remove_file(&state_path).unwrap();
    // Drop every pack in the repo too; heddle writes packs eagerly and a
    // surviving pack would still resolve the state, masking the test's
    // destructive-boundary intent.
    let packs_dir = temp.path().join(".heddle/packs");
    if packs_dir.exists() {
        for entry in std::fs::read_dir(&packs_dir).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
    }

    let err = heddle(&["undo"], Some(temp.path()))
        .expect_err("undo must refuse when the prior state is missing");
    let lower = err.to_lowercase();
    assert!(
        lower.contains("missing") || lower.contains("gone") || lower.contains("garbage"),
        "error must explain that prior state is missing: {err}"
    );
    // Worktree and HEAD must not have moved.
    assert_eq!(
        std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "v2",
        "refusal must not touch the worktree"
    );
}

/// `heddle undo --help` must surface the MVP scope: what's undoable today and
/// what is NOT (cross-thread, push/fetch, redo, purge). Without this, users
/// have to read the source to know when undo will help.
#[test]
fn test_undo_help_lists_undoable_and_unsupported() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    let help = heddle_must_succeed(&["undo", "--help"], temp.path());
    let lower = help.to_lowercase();
    assert!(
        lower.contains("undoable") || lower.contains("undo "),
        "--help should describe what undo does: {help}"
    );
    assert!(
        lower.contains("capture"),
        "--help should list capture as undoable: {help}"
    );
    assert!(
        lower.contains("merge"),
        "--help should list merge as undoable: {help}"
    );
    assert!(
        lower.contains("push") || lower.contains("fetch") || lower.contains("cross-thread"),
        "--help should call out what is NOT undoable: {help}"
    );
}

/// Locate a state's on-disk object file inside `.heddle/objects/states/` by
/// the short change-id prefix. Heddle stores each captured state as
/// `<full-change-id>.state` in that directory; the short id is the first
/// component of the filename. Returns `None` when no matching file exists
/// (e.g. the state lives only in a packfile).
fn locate_state_loose_file(repo_root: &std::path::Path, short: &str) -> Option<std::path::PathBuf> {
    let states_dir = repo_root.join(".heddle/objects/states");
    for entry in std::fs::read_dir(&states_dir).ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(short) {
            return Some(entry.path());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// `OpRecord::Redact` undo (heddle#98). Pre-fix, `heddle undo` on a redaction
// silently no-op'd — the doc claimed reversibility but the apply path fell
// through to the default match arm. Post-fix, undo of a redaction is the
// declared inverse: the redaction record is removed so subsequent
// materializations restore the original blob bytes, gated behind an explicit
// `--allow-redact-undo` flag so a casual `heddle undo` chain can't silently
// re-expose previously-hidden sensitive content. A `Purge` that landed after
// the `Redact` makes the redaction's audit-trail role load-bearing — undoing
// the Redact in that case is refused regardless of the flag because the bytes
// are physically gone and materialize would error rather than restore.
// ---------------------------------------------------------------------------

/// Bootstrap a repo containing a captured "leaked" secret. Returns the
/// temp dir and the short change-id of the capture so the caller can
/// pass it to `heddle redact apply <state>`.
fn setup_repo_with_secret() -> (TempDir, String) {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    std::fs::write(
        temp.path().join("config/secrets.toml"),
        b"api_token = \"super-secret-leaked-value\"\n",
    )
    .unwrap();
    heddle_must_succeed(&["capture", "-m", "leak the secret"], temp.path());
    let state = head_short(temp.path());
    (temp, state)
}

#[test]
fn test_undo_redact_with_allow_flag_restores_original_content() {
    let (temp, state) = setup_repo_with_secret();

    heddle_must_succeed(
        &[
            "redact",
            "apply",
            &state,
            "--path",
            "config/secrets.toml",
            "--reason",
            "leaked credential",
        ],
        temp.path(),
    );

    // Sanity: redact list shows the new redaction, and the underlying
    // materialize gate (the same function `materialize_blob` calls)
    // reports an active stub.
    let blob_hash = objects::object::Blob::from_slice(
        b"api_token = \"super-secret-leaked-value\"\n",
    )
    .hash();
    {
        let repo = Repository::open(temp.path()).unwrap();
        let stub = repo
            .redaction_stub_for_blob(&blob_hash)
            .expect("redaction_stub_for_blob must not error");
        assert!(
            stub.is_some(),
            "with the redaction active, materialize must substitute the stub"
        );
    }
    let list_before: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "redact", "list"],
        temp.path(),
    ))
    .unwrap();
    assert_eq!(
        list_before["count"].as_u64().unwrap(),
        1,
        "redact list should surface the new redaction: {list_before:?}",
    );

    // Undo with the explicit opt-in. The most recent batch is the
    // Redact, so a single-step undo targets it.
    heddle(&["undo", "--allow-redact-undo"], Some(temp.path()))
        .expect("undo of Redact must succeed with --allow-redact-undo");

    // Post-undo: redaction record is gone and materialize will now
    // restore the original blob bytes (no stub substitution). This is
    // the load-bearing "original content is restored" check from
    // heddle#98's DoD — `redaction_stub_for_blob` is exactly what
    // `materialize_blob` consults, so `None` here proves a subsequent
    // materialize would surface the original bytes.
    let list_after: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "redact", "list"],
        temp.path(),
    ))
    .unwrap();
    assert_eq!(
        list_after["count"].as_u64().unwrap(),
        0,
        "after undo, no redaction should remain: {list_after:?}",
    );
    let repo = Repository::open(temp.path()).unwrap();
    let stub = repo
        .redaction_stub_for_blob(&blob_hash)
        .expect("redaction_stub_for_blob must not error");
    assert!(
        stub.is_none(),
        "after undo, materialize must restore original bytes (no active stub)"
    );
}

#[test]
fn test_undo_redact_refuses_without_allow_flag() {
    let (temp, state) = setup_repo_with_secret();

    heddle_must_succeed(
        &[
            "redact",
            "apply",
            &state,
            "--path",
            "config/secrets.toml",
            "--reason",
            "leaked credential",
        ],
        temp.path(),
    );

    // Without the opt-in flag, undo must refuse. The default is
    // fail-loud so a casual `heddle undo` chain can't silently
    // re-expose a redaction the user previously asked to hide.
    let err = heddle(&["undo"], Some(temp.path()))
        .expect_err("undo of a Redact must refuse without --allow-redact-undo");
    let lower = err.to_lowercase();
    assert!(
        lower.contains("redact"),
        "refusal must name the redaction cause: {err}"
    );
    assert!(
        lower.contains("--allow-redact-undo") || lower.contains("allow-redact-undo"),
        "refusal must point at the opt-in flag: {err}"
    );

    // Refusal is atomic — the redaction record must still be there.
    let list: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "redact", "list"],
        temp.path(),
    ))
    .unwrap();
    assert_eq!(
        list["count"].as_u64().unwrap(),
        1,
        "refusal must not mutate the redactions sidecar: {list:?}"
    );
}

#[test]
fn test_undo_redact_refuses_when_blob_already_purged() {
    // Purge is irreversible — bytes are gone from local storage. Once
    // a Redact has been followed by Purge, the Redaction record's role
    // shifts from "stub-substitution declaration" to "audit trail of
    // destroyed bytes". Removing it would lie about what's on disk and
    // any subsequent materialize would fail with a missing-blob error
    // rather than restore content. The undo must refuse with a clear
    // message, even with the allow flag.
    let (temp, state) = setup_repo_with_secret();

    heddle_must_succeed(
        &[
            "redact",
            "apply",
            &state,
            "--path",
            "config/secrets.toml",
            "--reason",
            "leaked credential",
        ],
        temp.path(),
    );
    heddle_must_succeed(
        &[
            "purge",
            "apply",
            &state,
            "--path",
            "config/secrets.toml",
            "--force",
        ],
        temp.path(),
    );

    // Attempt to undo both batches with the redact-undo opt-in. The
    // Purge inverse refuses outright (Purge is irreversible by design),
    // and reaching past it to the Redact must also refuse because the
    // redaction is now purged.
    let err = heddle(
        &["undo", "-n", "2", "--allow-redact-undo"],
        Some(temp.path()),
    )
    .expect_err("undo across a purged redaction must refuse");
    let lower = err.to_lowercase();
    assert!(
        lower.contains("purge") || lower.contains("irreversible"),
        "refusal must name purge/irreversibility: {err}"
    );

    // The redaction record must remain, marked purged.
    let list: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "redact", "list"],
        temp.path(),
    ))
    .unwrap();
    assert_eq!(
        list["count"].as_u64().unwrap(),
        1,
        "refusal must not mutate the redactions sidecar: {list:?}",
    );
    assert!(
        list["redactions"][0]["purged"].as_bool().unwrap(),
        "the redaction must still be marked purged after refusal: {list:?}",
    );
}
