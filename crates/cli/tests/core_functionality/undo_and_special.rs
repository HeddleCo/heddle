// SPDX-License-Identifier: Apache-2.0
use objects::object::ThreadName;

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
    let result = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
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

/// Regression: `heddle undo` on a worktree with explicitly ignored build or
/// dependency output must not abort with `os error 66` after destroying tracked
/// files. The planner asks `remove_dir` to drop the parent of an ignored child;
/// that fails with ENOTEMPTY. Pre-fix this left the
/// worktree gutted with HEAD stuck at the old state. Post-fix the directory
/// is left in place and undo completes transactionally.
#[test]
fn test_undo_preserves_ignored_siblings_in_tracked_dirs() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(
        temp.path().join(".heddleignore"),
        "target/\nnode_modules/\n",
    )
    .unwrap();
    heddle_must_succeed(&["capture", "-m", "ignore colocated git"], temp.path());

    std::fs::write(temp.path().join("main.rs"), "fn main() {}").unwrap();
    std::fs::create_dir_all(temp.path().join("web")).unwrap();
    std::fs::write(temp.path().join("web/index.html"), "<html/>").unwrap();
    heddle_must_succeed(&["capture", "-m", "tracked"], temp.path());

    // Drop explicitly ignored siblings into tracked directories — these would
    // otherwise be present from `bun install`, `cargo build`, `git init`, etc.
    // They are invisible to status because this test named them in
    // `.heddleignore`, but they still occupy the filesystem.
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
    let status_json = heddle_must_succeed(&["status", "--output", "json"], temp.path());
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

    let err = heddle(&["undo", "-n", "1", "--output", "json"], Some(temp.path()))
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

    let err = heddle(&["undo", "-n", "1", "--output", "json"], Some(temp.path()))
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

    let err = heddle(
        &["cherry-pick", &feature_commit, "--output", "json"],
        Some(temp.path()),
    )
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

    let err = heddle(
        &["cherry-pick", &feature_commit, "--output", "json"],
        Some(temp.path()),
    )
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

    let err = heddle(
        &["rebase", "feature", "--output", "json"],
        Some(temp.path()),
    )
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

    let err = heddle(
        &["rebase", "feature", "--output", "json"],
        Some(temp.path()),
    )
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
/// local-only content. This is the unit-level companion to the CLI test
/// above.
#[test]
fn test_undo_with_dotgit_directory_present() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join(".heddleignore"), ".git/\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "empty"], temp.path());

    std::fs::write(temp.path().join("file.txt"), "v1").unwrap();
    heddle_must_succeed(&["capture", "-m", "v1"], temp.path());

    // Simulate a co-located git repo. `.git` is preserved only because this
    // test explicitly names it in `.heddleignore`; Heddle's only built-in
    // ignore is `.heddle` itself.
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

/// heddle#305: `undo` must not silently discard the worktree edits a prior
/// `capture`/`commit` absorbed. Before the reset, undo records the pre-undo
/// state into a durable `undo-recovery` marker in heddle's thread history, so
/// the absorbed content is preserved as a first-class, addressable recovery
/// point — not merely buried in an undone oplog batch — while `redo` still
/// round-trips the content. Pre-fix there was no recovery marker: the pre-undo
/// state was recoverable only via `redo` (fragile) or by knowing the buried
/// change-id, matching the dogfood report that the edits were "not obviously
/// recoverable".
#[test]
fn test_undo_captures_pre_undo_state_into_recovery_marker() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("notes.md"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    // An edit that lived only in the worktree, then captured ("committed").
    std::fs::write(temp.path().join("notes.md"), "FRICTION ONE\nFRICTION TWO\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "friction"], temp.path());
    let friction_state = head_short(temp.path());

    heddle_must_succeed(&["undo"], temp.path());

    // The reset happened: worktree reverted to the parent state.
    assert_eq!(
        std::fs::read_to_string(temp.path().join("notes.md")).unwrap(),
        "base\n",
        "undo must reset the worktree to the parent state"
    );

    // (a) The pre-undo worktree content is captured into thread history: undo
    // records the pre-undo state in the heddle-internal recovery ref (NOT a
    // user marker — see heddle#305 r2).
    let repo = Repository::open(temp.path()).unwrap();
    let recovery = repo
        .refs()
        .get_undo_recovery()
        .unwrap()
        .expect("undo must record the pre-undo state in the internal recovery ref");
    assert_eq!(
        recovery.short(),
        friction_state,
        "the recovery ref must point at the pre-undo (friction) state, not the reset target"
    );

    // (b) `redo` restores the captured content (round-trips the worktree).
    heddle_must_succeed(&["redo"], temp.path());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("notes.md")).unwrap(),
        "FRICTION ONE\nFRICTION TWO\n",
        "redo must restore the friction content to the worktree"
    );
}

/// The recovery marker makes the pre-undo content recoverable by name,
/// independent of the (fragile) redo stack: after undo, `heddle goto
/// undo-recovery` restores the pre-undo worktree even once a divergent capture
/// has been layered on top of the reverted state. This is the data-safety
/// guarantee — nothing absorbed by the undone capture is lost.
#[test]
fn test_undo_recovery_marker_survives_divergent_capture() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("notes.md"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());
    std::fs::write(temp.path().join("notes.md"), "FRICTION ONE\nFRICTION TWO\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "friction"], temp.path());
    let friction_state = head_short(temp.path());

    heddle_must_succeed(&["undo"], temp.path());

    // Diverge: a fresh capture layered onto the reverted worktree.
    std::fs::write(temp.path().join("notes.md"), "different direction\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "diverge"], temp.path());

    // The durable internal recovery ref still pins the pre-undo (friction)
    // state, untouched by the divergent capture.
    let repo = Repository::open(temp.path()).unwrap();
    let recovery = repo
        .refs()
        .get_undo_recovery()
        .unwrap()
        .expect("recovery ref must survive a divergent capture");
    assert_eq!(recovery.short(), friction_state);

    // Recover the pre-undo content via the well-known handle.
    heddle_must_succeed(&["goto", ".undo-recovery"], temp.path());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("notes.md")).unwrap(),
        "FRICTION ONE\nFRICTION TWO\n",
        "the pre-undo content must be recoverable via the durable recovery handle"
    );
}

/// heddle#305 r2: the pre-undo recovery pointer must live in a heddle-internal
/// reserved ref, NOT the user marker namespace. Storing it as a user marker
/// named `undo-recovery` couples heddle bookkeeping to a user-writable name:
/// the `MarkerDelete` undo inverse re-creates user markers with a `Missing`
/// expectation, so a same-named recovery marker pre-written by `undo` would
/// make that inverse fail (`create_marker(undo-recovery, Missing)` collides).
/// Moving recovery out of `refs/markers/` makes the whole class impossible by
/// construction: user `marker create/delete` (and their inverses) can never
/// see or clobber the internal pointer, and vice versa.
#[test]
fn test_undo_recovery_lives_outside_user_marker_namespace() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("notes.md"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());
    std::fs::write(temp.path().join("notes.md"), "FRICTION\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "friction"], temp.path());
    let friction_state = head_short(temp.path());

    heddle_must_succeed(&["undo"], temp.path());

    // (a) recovery must NOT pollute the user marker namespace.
    let markers: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "marker", "list"],
        temp.path(),
    ))
    .unwrap();
    assert!(
        markers["markers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["name"] != "undo-recovery"),
        "recovery bookkeeping must not appear as a user marker"
    );

    // (b) it lives in the heddle-internal recovery ref, pinning the pre-undo
    // state — a namespace the user marker CLI cannot enumerate or collide with.
    let repo = Repository::open(temp.path()).unwrap();
    let recovery = repo
        .refs()
        .get_undo_recovery()
        .unwrap()
        .expect("undo must preserve the pre-undo state in the internal recovery ref");
    assert_eq!(
        recovery.short(),
        friction_state,
        "internal recovery ref must pin the pre-undo (friction) state"
    );

    // (c) the recovery UX is preserved: `goto .undo-recovery` resolves the
    // internal ref and restores the pre-undo content.
    heddle_must_succeed(&["goto", ".undo-recovery"], temp.path());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("notes.md")).unwrap(),
        "FRICTION\n",
        "the pre-undo content must be recoverable via the internal handle"
    );

    // (d) coexistence: a user may now legitimately create and delete their own
    // marker named `undo-recovery` without colliding with — or disturbing —
    // the internal recovery pointer. This is the closed class: the two
    // namespaces are independent.
    heddle_must_succeed(&["marker", "create", "undo-recovery"], temp.path());
    heddle_must_succeed(&["marker", "delete", "undo-recovery"], temp.path());
    let repo = Repository::open(temp.path()).unwrap();
    assert_eq!(
        repo.refs()
            .get_undo_recovery()
            .unwrap()
            .map(|id| id.short()),
        Some(friction_state),
        "user marker create/delete must not touch the internal recovery ref"
    );
}

/// heddle#305 r3: the advertised recovery handle must be UNSHADOWABLE on the
/// READ path too. Worst case: a user already owns a marker literally named
/// `undo-recovery`. The advertised handle `undo` prints (`recovery_marker`)
/// must resolve to the INTERNAL pre-undo state, never the user's ref. r2 fixed
/// the write path (internal ref) but left the advertised handle a bare
/// user-namespace name that `resolve_refspec` resolves to the user ref first —
/// this conformance test closes that direction.
#[test]
fn test_recovery_handle_unshadowable_by_user_marker() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("notes.md"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());
    let base_state = head_short(temp.path());

    // The user owns a marker named `undo-recovery`, pinning the BASE state.
    heddle_must_succeed(&["marker", "create", "undo-recovery"], temp.path());

    std::fs::write(temp.path().join("notes.md"), "FRICTION\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "friction"], temp.path());
    let friction_state = head_short(temp.path());
    assert_ne!(base_state, friction_state);

    let undo: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "undo"],
        temp.path(),
    ))
    .unwrap();
    let advertised = undo["recovery_marker"]
        .as_str()
        .expect("undo advertises a recovery handle");

    // The advertised handle must resolve to the INTERNAL pre-undo (friction)
    // state, NOT the user's same-named marker (which pins base).
    heddle_must_succeed(&["goto", advertised], temp.path());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("notes.md")).unwrap(),
        "FRICTION\n",
        "advertised recovery handle must restore the internal pre-undo state, not the user's ref"
    );

    // The user's own `undo-recovery` marker is untouched and still pins base.
    let repo = Repository::open(temp.path()).unwrap();
    assert_eq!(
        repo.refs()
            .get_marker(&objects::object::MarkerName::new("undo-recovery"))
            .unwrap()
            .map(|id| id.short()),
        Some(base_state),
        "the user's own undo-recovery marker must remain intact and independent"
    );
}

/// Fast-forward merge undo, full restoration: HEAD *and* the merged-into
/// thread ref both rewind to the pre-merge tip. This pins the heddle#99 fix —
/// before it landed, the FF merge recorded an `OpRecord::Goto` whose inverse
/// only rewinds HEAD, stranding the target thread ref at the FF target. The
/// new `OpRecord::FastForward` variant carries the pre-FF tip so undo restores
/// both refs together.
///
/// This test was previously named
/// `test_undo_ff_merge_restores_head_but_strands_thread_ref` and asserted the
/// stranded-ref behavior as a pinned bug. Renamed and the strand assertion
/// flipped when heddle#99 closed.
#[test]
fn test_undo_ff_merge_restores_head_and_thread_ref() {
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
        .get_thread(&ThreadName::new("feature"))
        .unwrap()
        .expect("feature thread still exists")
        .short();
    assert_eq!(
        feature_tip, feature_tip_before,
        "feature thread tip must be unchanged across merge + undo"
    );

    // The heddle#99 fix: undoing an FF merge restores BOTH HEAD and the
    // target thread ref to the pre-merge state. Recording the FF as
    // `OpRecord::FastForward` (instead of `OpRecord::Goto`) gives the
    // inverse the thread context it needs.
    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, main_tip_before,
        "undo of FF merge must restore the `main` thread ref to its pre-merge tip \
         (heddle#99 — was stranded at the FF target before the fix)"
    );

    // HEAD must remain attached to `main` so subsequent ops record on the
    // expected lane. Pre-fix the strand also left HEAD detached, which
    // silently broke scope filtering on the next undo.
    match repo.head_ref().unwrap() {
        refs::Head::Attached { thread } => assert_eq!(
            thread, "main",
            "HEAD must stay attached to `main` after FF undo"
        ),
        refs::Head::Detached { state } => panic!(
            "HEAD must stay attached to `main`; got detached at {}",
            state.short()
        ),
    }
}

/// Destructive-boundary protection covers `OpRecord::FastForward` too: if
/// the pre-FF state is gone from the object store (gc --prune past the live
/// oplog window, or oplog backup restored without its objects), undo must
/// refuse loudly with a clear message instead of half-rewinding the worktree
/// or panicking deep inside `goto`. Mirrors the `Goto`/`Snapshot` coverage
/// in `test_undo_refuses_when_prior_state_missing` but exercises the new
/// FF arm specifically.
#[test]
fn test_undo_ff_merge_refuses_when_pre_target_state_missing() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());
    let main_tip_before = head_short(temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    heddle_must_succeed(&["merge", "feature"], temp.path());

    // Simulate gc reaching past the pre-FF tip. Same shape as the
    // Goto/Snapshot coverage above: drop the loose state file and any
    // pack that could resolve it.
    let state_path = locate_state_loose_file(temp.path(), &main_tip_before)
        .expect("pre-FF state's loose file is present after merge");
    std::fs::remove_file(&state_path).unwrap();
    let packs_dir = temp.path().join(".heddle/packs");
    if packs_dir.exists() {
        for entry in std::fs::read_dir(&packs_dir).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
    }

    let err = heddle(&["undo"], Some(temp.path()))
        .expect_err("undo must refuse when the pre-FF state is missing");
    let lower = err.to_lowercase();
    assert!(
        lower.contains("missing") || lower.contains("gone") || lower.contains("garbage"),
        "error must explain that prior state is missing: {err}"
    );
}

/// FF merge undo + redo round-trip: redo re-applies the FF, advancing both
/// HEAD and the target thread ref back to the source's tip. The source
/// thread is untouched throughout.
#[test]
fn test_redo_ff_merge_restores_head_and_thread_ref() {
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

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    heddle_must_succeed(&["merge", "feature"], temp.path());
    heddle_must_succeed(&["undo"], temp.path());
    assert_eq!(head_short(temp.path()), main_tip_before);

    heddle_must_succeed(&["redo"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        feature_tip_before,
        "redo of FF merge must re-advance HEAD to feature's tip"
    );

    let repo = Repository::open(temp.path()).unwrap();
    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, feature_tip_before,
        "redo of FF merge must re-advance `main` thread ref to feature's tip"
    );
    let feature_tip = repo
        .refs()
        .get_thread(&ThreadName::new("feature"))
        .unwrap()
        .expect("feature thread still exists")
        .short();
    assert_eq!(
        feature_tip, feature_tip_before,
        "feature thread tip stays put across the full merge/undo/redo round-trip"
    );
}

/// Redo of an FF merge must replay the *recorded* operation, not re-derive
/// it from the source thread's current tip. heddle#99 r1 resolved
/// `source_thread → tip` at redo time; if the source thread had advanced
/// between undo and redo, redo silently pulled in commits that were never
/// part of the original merge. The fix records `post_target_id` (the FF
/// result SHA) at recording time and uses it directly on redo, so the
/// replay is deterministic.
///
/// Pinned the bug pre-fix: this test was red before `FastForwardV2` landed.
#[test]
fn test_redo_ff_merge_pins_recorded_tip_when_source_advances() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());
    let main_tip_before = head_short(temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], temp.path());
    let feature_tip_at_ff = head_short(temp.path());

    // FF main → feature, then undo back to main_tip_before.
    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    heddle_must_succeed(&["merge", "feature"], temp.path());
    heddle_must_succeed(&["undo"], temp.path());
    assert_eq!(head_short(temp.path()), main_tip_before);

    // Advance the source thread after undo: a second capture on feature
    // gives it a new tip distinct from the FF target. Pre-fix, redo would
    // pick up *this* tip instead of the recorded FF target.
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work + more").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature again"], temp.path());
    let feature_tip_advanced = head_short(temp.path());
    assert_ne!(
        feature_tip_at_ff, feature_tip_advanced,
        "post-undo capture on feature must produce a new tip distinct from the FF target"
    );

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    heddle_must_succeed(&["redo"], temp.path());

    // The recorded FF target — not feature's current tip — is what redo must
    // restore. HEAD and the `main` thread ref both end at the original FF SHA.
    assert_eq!(
        head_short(temp.path()),
        feature_tip_at_ff,
        "redo of FF merge must replay to the recorded FF target, not source's current tip"
    );
    let repo = Repository::open(temp.path()).unwrap();
    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, feature_tip_at_ff,
        "redo of FF merge must set `main` ref to the recorded FF target, not source's current tip"
    );
    let feature_tip = repo
        .refs()
        .get_thread(&ThreadName::new("feature"))
        .unwrap()
        .expect("feature thread still exists")
        .short();
    assert_eq!(
        feature_tip, feature_tip_advanced,
        "feature thread's own ref is independent of redo — it stays at its new tip"
    );
}

/// Redo of an FF merge must succeed even when the source thread has been
/// deleted after undo. The original merged state is fully recoverable from
/// the recorded `post_target_id`; refusing redo here would punish the user
/// for housekeeping a now-merged feature branch.
///
/// Pinned the bug pre-fix: heddle#99 r1's redo resolved `source_thread → tip`
/// live and errored with "source thread no longer exists" when the user
/// dropped the thread between undo and redo.
#[test]
fn test_redo_ff_merge_succeeds_when_source_deleted() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());
    let main_tip_before = head_short(temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], temp.path());
    let feature_tip_at_ff = head_short(temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    heddle_must_succeed(&["merge", "feature"], temp.path());
    heddle_must_succeed(&["undo"], temp.path());
    assert_eq!(head_short(temp.path()), main_tip_before);

    // Delete the source thread between undo and redo. The legacy CLI
    // shape `thread delete <name>` is translated to
    // `thread drop <name> --delete-thread` by `translate_legacy_args`.
    heddle_must_succeed(&["thread", "delete", "feature"], temp.path());

    heddle_must_succeed(&["redo"], temp.path());

    assert_eq!(
        head_short(temp.path()),
        feature_tip_at_ff,
        "redo must replay to the recorded FF target even when source thread is gone"
    );
    let repo = Repository::open(temp.path()).unwrap();
    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, feature_tip_at_ff,
        "main ref must reach the recorded FF target after redo, source-thread-deletion notwithstanding"
    );
}

/// Symmetric to `test_undo_ff_merge_refuses_when_pre_target_state_missing`:
/// redo also has a destructive-boundary case. The state we'd advance to
/// (`post_target_id`) must still be in the object store; if it has been
/// pruned, redo must refuse with a clear message rather than partially
/// rewinding HEAD past the boundary or panicking deep in `goto`.
#[test]
fn test_redo_ff_merge_refuses_when_post_target_state_missing() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], temp.path());
    let feature_tip_at_ff = head_short(temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    heddle_must_succeed(&["merge", "feature"], temp.path());
    // Undo first: HEAD is now back at main_tip_before, so the FF target SHA
    // is no longer pinned by HEAD and can be removed to simulate a gc that
    // pruned past the redo's reach.
    heddle_must_succeed(&["undo"], temp.path());

    let state_path = locate_state_loose_file(temp.path(), &feature_tip_at_ff)
        .expect("FF target state's loose file is present after undo");
    std::fs::remove_file(&state_path).unwrap();
    let packs_dir = temp.path().join(".heddle/packs");
    if packs_dir.exists() {
        for entry in std::fs::read_dir(&packs_dir).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
    }
    // Also drop the source thread ref so a live-resolve path can't smuggle
    // the SHA back in by reading `feature → tip`. (Belt-and-braces: the new
    // redo arm doesn't read the source thread at all, but locking this down
    // ensures the test fails in the right way if a regression re-introduces
    // a live-resolve fallback.)
    heddle_must_succeed(&["thread", "delete", "feature"], temp.path());

    let err = heddle(&["redo"], Some(temp.path()))
        .expect_err("redo must refuse when the FF target state is missing");
    let lower = err.to_lowercase();
    assert!(
        lower.contains("missing") || lower.contains("gone") || lower.contains("garbage"),
        "error must explain that the redo target state is missing: {err}"
    );
}

/// Divergent target/source histories are stale until refreshed. The old
/// direct non-fast-forward merge path used to synthesize a merge state here;
/// the stricter verification model refuses before writing an undoable op.
#[test]
fn test_stale_non_ff_merge_refuses_without_moving_threads() {
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

    let err = heddle(&["merge", "feature"], Some(temp.path()))
        .expect_err("stale divergent merge must refuse before mutation");
    assert!(
        err.contains("Thread 'feature' is stale") && err.contains("heddle thread refresh feature"),
        "stale merge should explain the refresh path: {err}"
    );
    assert_eq!(
        head_short(temp.path()),
        main_tip_before,
        "stale merge refusal must leave HEAD at main's pre-merge tip"
    );

    let repo = Repository::open(temp.path()).unwrap();

    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, main_tip_before,
        "stale merge refusal must leave the `main` thread ref unchanged"
    );

    let feature_tip = repo
        .refs()
        .get_thread(&ThreadName::new("feature"))
        .unwrap()
        .expect("feature thread still exists")
        .short();
    assert_eq!(
        feature_tip, feature_tip_before,
        "stale merge refusal must leave the source thread unchanged"
    );
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
        lower.contains("push") || lower.contains("fetch") || lower.contains("cross-worktree"),
        "--help should call out what is NOT undoable: {help}"
    );
    // The worktree-attached refusal is a 0.3 contract — `--help` must
    // surface it so users hit by the refusal can find the teardown
    // path without reading source. See docs/design/cross-thread-undo.md.
    assert!(
        lower.contains("worktree") || lower.contains("--path"),
        "--help should mention the worktree-attached ThreadCreate refusal: {help}"
    );
    assert!(
        lower.contains("thread drop") || lower.contains("--delete-thread"),
        "--help should redirect users to the teardown command for the worktree case: {help}"
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
    let blob_hash =
        objects::object::Blob::from_slice(b"api_token = \"super-secret-leaked-value\"\n").hash();
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

#[test]
fn test_redo_of_undone_redact_refuses() {
    // Counterpart to the undo test: once a `Redact` batch has been
    // undone with `--allow-redact-undo`, `heddle redo` refuses to
    // re-apply it. The OpRecord doesn't preserve the full `Redaction`
    // (reason, redactor, signature) needed for a faithful re-apply,
    // so we surface a clear error instead of silently no-op'ing.
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

    // Undo with the opt-in. The Redact batch is now marked undone and
    // sits as the next redoable batch.
    heddle(&["undo", "--allow-redact-undo"], Some(temp.path()))
        .expect("undo of Redact must succeed with --allow-redact-undo");

    // Redo refuses with a message that names Redact + points the user
    // at re-running `heddle redact apply`.
    let err =
        heddle(&["redo"], Some(temp.path())).expect_err("redo of an undone Redact must refuse");
    let lower = err.to_lowercase();
    assert!(
        lower.contains("redact"),
        "redo refusal must mention Redact: {err}"
    );
    assert!(
        lower.contains("redact apply") || lower.contains("re-apply"),
        "redo refusal should redirect to `heddle redact apply`: {err}"
    );

    // Refusal is atomic — no state mutated. Redact list still 0 (the
    // record was removed by the undo and the refused redo did not
    // re-create it).
    let list: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "redact", "list"],
        temp.path(),
    ))
    .unwrap();
    assert_eq!(
        list["count"].as_u64().unwrap(),
        0,
        "redo refusal must not re-create the redaction: {list:?}",
    );
}

#[test]
fn test_undo_redact_preserves_sibling_redactions_on_same_blob() {
    // When two redactions target the same blob (e.g. `--all-states`
    // propagation, or a redact + a later refinement on a different
    // path), undoing one must leave the other intact: the per-blob
    // sidecar is rewritten in place rather than deleted wholesale.
    let (temp, state_a) = setup_repo_with_secret();
    // A second capture so we have two states referencing the same
    // blob (the secrets file content is unchanged between captures,
    // so the blob hash is identical — that's the trigger for the
    // shared-sidecar code path).
    std::fs::write(temp.path().join("trailing.txt"), "trailing").unwrap();
    heddle_must_succeed(&["capture", "-m", "trailing"], temp.path());
    let state_b = head_short(temp.path());

    heddle_must_succeed(
        &[
            "redact",
            "apply",
            &state_a,
            "--path",
            "config/secrets.toml",
            "--reason",
            "leaked credential (state_a)",
        ],
        temp.path(),
    );
    heddle_must_succeed(
        &[
            "redact",
            "apply",
            &state_b,
            "--path",
            "config/secrets.toml",
            "--reason",
            "leaked credential (state_b)",
        ],
        temp.path(),
    );

    let list_before: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "redact", "list"],
        temp.path(),
    ))
    .unwrap();
    assert_eq!(
        list_before["count"].as_u64().unwrap(),
        2,
        "two redactions on the same blob expected: {list_before:?}",
    );

    // Undo the most-recent Redact (state_b) — the state_a redaction
    // must survive untouched.
    heddle(&["undo", "--allow-redact-undo"], Some(temp.path()))
        .expect("undo of single Redact succeeds");

    let list_after: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "redact", "list"],
        temp.path(),
    ))
    .unwrap();
    assert_eq!(
        list_after["count"].as_u64().unwrap(),
        1,
        "exactly one redaction should remain after one undo: {list_after:?}",
    );
    let surviving_reason = list_after["redactions"][0]["reason"].as_str().unwrap();
    assert_eq!(
        surviving_reason, "leaked credential (state_a)",
        "the state_a redaction must survive when state_b's is undone"
    );
}

#[test]
fn test_undo_redact_removes_exact_record_when_multiple_target_same_triple() {
    // Two `redact apply` invocations on the same (state, path) — a
    // refinement pass where the operator updates `--reason` (or adds
    // `--sign-with`) without first undoing the prior declaration.
    // Each invocation writes a distinct `Redaction` (different reason →
    // different content hash) into the per-blob sidecar.
    //
    // `heddle undo --allow-redact-undo -n 1` must remove the SECOND
    // (most-recent) record because that's the one the most-recent op
    // batch references. A naive `(state, path)` match would pick the
    // first record in the sidecar and silently undo the wrong one,
    // leaving the recently-refined reason behind.
    let (temp, state) = setup_repo_with_secret();

    heddle_must_succeed(
        &[
            "redact",
            "apply",
            &state,
            "--path",
            "config/secrets.toml",
            "--reason",
            "initial: leaked credential",
        ],
        temp.path(),
    );
    heddle_must_succeed(
        &[
            "redact",
            "apply",
            &state,
            "--path",
            "config/secrets.toml",
            "--reason",
            "refined: leaked api token v2",
        ],
        temp.path(),
    );

    let list_before: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "redact", "list"],
        temp.path(),
    ))
    .unwrap();
    assert_eq!(
        list_before["count"].as_u64().unwrap(),
        2,
        "two redactions on same (blob,state,path) expected: {list_before:?}",
    );

    // Undo the most-recent batch (the refined-reason redact). The
    // initial-reason redaction must survive intact.
    heddle(&["undo", "--allow-redact-undo"], Some(temp.path()))
        .expect("undo of refined Redact succeeds");

    let list_after: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "redact", "list"],
        temp.path(),
    ))
    .unwrap();
    assert_eq!(
        list_after["count"].as_u64().unwrap(),
        1,
        "exactly one redaction must remain after one undo: {list_after:?}",
    );
    let surviving_reason = list_after["redactions"][0]["reason"].as_str().unwrap();
    assert_eq!(
        surviving_reason, "initial: leaked credential",
        "the initial (older) redaction must survive — undoing the refined batch must remove the refined record, not the initial one"
    );
}

#[test]
fn test_undo_preview_refuses_redact_without_allow_flag() {
    // `heddle undo --preview` must mirror the real command's refusals
    // rather than optimistically saying "Would undo ..." Pre-fix it
    // short-circuited before the redaction safety check and lied about
    // the outcome.
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

    let err = heddle(&["undo", "--preview"], Some(temp.path())).expect_err(
        "undo --preview against a Redact batch must refuse without --allow-redact-undo",
    );
    let lower = err.to_lowercase();
    assert!(
        lower.contains("redact"),
        "preview refusal must name the redaction cause: {err}"
    );
    assert!(
        lower.contains("--allow-redact-undo") || lower.contains("allow-redact-undo"),
        "preview refusal must point at the opt-in flag: {err}"
    );
}

#[test]
fn test_undo_preview_refuses_redact_when_blob_already_purged() {
    // Parallel to the non-preview case: undoing across a purged
    // redaction must refuse with the irreversibility/audit-trail
    // message, and `--preview` must surface that refusal honestly.
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

    let err = heddle(
        &["undo", "-n", "2", "--preview", "--allow-redact-undo"],
        Some(temp.path()),
    )
    .expect_err("undo --preview across a purged redaction must refuse");
    let lower = err.to_lowercase();
    assert!(
        lower.contains("purge") || lower.contains("irreversible"),
        "preview refusal must name purge/irreversibility: {err}"
    );
}

#[test]
fn test_redo_preview_refuses_redact_chain() {
    // Mirror of the undo `--preview` honesty rule on the redo side:
    // `heddle redo --preview` against a previously-undone Redact must
    // surface the same "no re-apply path" refusal the real `redo`
    // would surface, not advertise "Would redo …".
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
    heddle(&["undo", "--allow-redact-undo"], Some(temp.path()))
        .expect("undo of Redact must succeed with --allow-redact-undo");

    let err = heddle(&["redo", "--preview"], Some(temp.path()))
        .expect_err("redo --preview of an undone Redact must refuse");
    let lower = err.to_lowercase();
    assert!(
        lower.contains("redact"),
        "redo preview refusal must mention Redact: {err}"
    );
    assert!(
        lower.contains("redact apply") || lower.contains("re-apply"),
        "redo preview refusal should redirect to `heddle redact apply`: {err}"
    );
}

// ---------------------------------------------------------------------------
// Cross-thread undo coverage (heddle#23 r2)
//
// These tests pin the contract laid out in docs/design/cross-thread-undo.md:
// undo of `ThreadCreate` must keep ref state and ThreadManager metadata in
// lockstep, and must refuse rather than orphan a materialized sibling
// worktree.
// ---------------------------------------------------------------------------

/// Bootstrap a repo with an initial snapshot so `ensure_current_state` is
/// happy when we go on to `heddle thread create`. Returns the temp dir so
/// the caller keeps it alive for the rest of the test.
fn bootstrap_repo_with_initial_state() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());
    temp
}

/// After undoing a plain `heddle thread create`, the ref must be gone *and*
/// the ThreadManager record that `cmd_thread_create` wrote alongside the
/// ref must be removed. Pre-fix the inverse only deleted the ref; the
/// record lingered and surfaced as a phantom entry in `heddle thread show`
/// (which reads from the record store, not the ref store). Cross-thread
/// contract rule 4 (refs and ThreadManager metadata must mirror each
/// other) — see docs/design/cross-thread-undo.md.
#[test]
fn test_undo_thread_create_removes_record_when_no_worktree() {
    use repo::ThreadManager;

    let temp = bootstrap_repo_with_initial_state();

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());

    // Sanity: the record was written by `cmd_thread_create` (thread.rs:1636).
    {
        let repo = Repository::open(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());
        let record = manager
            .find_by_thread("feature")
            .unwrap()
            .expect("`thread create` writes a ThreadManager record");
        assert!(
            record.materialized_path.is_none(),
            "plain `thread create` must not materialize a worktree"
        );
    }

    heddle_must_succeed(&["undo"], temp.path());

    let repo = Repository::open(temp.path()).unwrap();

    // The ref is gone — that part already worked pre-fix.
    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("feature"))
            .unwrap()
            .is_none(),
        "undo of `thread create` must delete the thread ref"
    );

    // The fix: the ThreadManager record must also be gone so subsequent
    // `heddle thread show` / `thread list` don't surface a phantom entry.
    let manager = ThreadManager::new(repo.heddle_dir());
    assert!(
        manager.find_by_thread("feature").unwrap().is_none(),
        "undo of `thread create` must remove the matching ThreadManager record \
         (heddle#23 r2 — cross-thread undo contract rule 4)"
    );
}

/// Undoing `heddle start <name> --path <wt>` would orphan the materialized
/// worktree directory at `<wt>` — the inverse deletes the thread ref but
/// has no way to atomically tear the worktree down (it lives in another
/// directory and may have uncommitted work in flight). Per the cross-thread
/// contract rule 5, undo refuses with a clear message pointing the user at
/// the manual teardown path (`heddle thread drop --delete-thread`).
#[test]
fn test_undo_thread_create_refuses_with_materialized_worktree() {
    let temp = bootstrap_repo_with_initial_state();

    let wt_path = temp.path().join("feature-wt");
    heddle_must_succeed(
        &[
            "start",
            "feature",
            "--path",
            wt_path.to_str().unwrap(),
            "--workspace",
            "solid",
        ],
        temp.path(),
    );

    // Sanity: the worktree was materialized.
    assert!(
        wt_path.exists(),
        "`heddle start --path` must materialize the requested worktree"
    );
    {
        let repo = Repository::open(temp.path()).unwrap();
        assert!(
            repo.refs()
                .get_thread(&ThreadName::new("feature"))
                .unwrap()
                .is_some(),
            "feature thread ref must exist after `start --path`"
        );
    }

    let err = heddle(&["undo"], Some(temp.path()))
        .expect_err("undo of `start --path` must refuse so the worktree isn't orphaned");
    let lower = err.to_lowercase();
    assert!(
        lower.contains("worktree") || lower.contains("materialized"),
        "refusal must name the worktree concern: {err}"
    );
    assert!(
        err.contains("feature-wt") || err.contains("feature"),
        "refusal must surface the affected thread or worktree path: {err}"
    );
    assert!(
        lower.contains("thread drop") || lower.contains("--delete-thread"),
        "refusal must point at the teardown command: {err}"
    );

    // Refusal must be pre-flight: nothing was mutated.
    let repo = Repository::open(temp.path()).unwrap();
    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("feature"))
            .unwrap()
            .is_some(),
        "thread ref must survive a refused undo (pre-flight refusal — \
         consistent with the redaction/dirty-worktree gates)"
    );
    assert!(
        wt_path.exists(),
        "worktree directory must survive a refused undo"
    );
}

/// `heddle thread rename` is a batch `[ThreadCreate(new), ThreadDelete(old)]`
/// (see oplog_records.rs::record_thread_rename). The cross-thread inverse
/// must round-trip both halves: the new name's ref deleted, the old name's
/// ref restored, no orphan ThreadManager record under the new name.
///
/// Regression guard: passes today because `cmd_thread_rename` never writes
/// a record under the new name in the first place, so the inverse vacuously
/// satisfies the "no orphan under new name" assertion. Catches a future
/// regression that introduced a forward-path record write without a
/// matching inverse cleanup.
#[test]
fn test_undo_thread_rename_round_trips_refs_and_record() {
    use repo::ThreadManager;

    let temp = bootstrap_repo_with_initial_state();

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "rename", "feature", "feature-v2"], temp.path());

    {
        let repo = Repository::open(temp.path()).unwrap();
        assert!(
            repo.refs()
                .get_thread(&ThreadName::new("feature-v2"))
                .unwrap()
                .is_some(),
            "rename forward path must create `feature-v2`"
        );
        assert!(
            repo.refs()
                .get_thread(&ThreadName::new("feature"))
                .unwrap()
                .is_none(),
            "rename forward path must remove `feature`"
        );
    }

    heddle_must_succeed(&["undo"], temp.path());

    let repo = Repository::open(temp.path()).unwrap();

    // Refs: rename rolled back.
    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("feature"))
            .unwrap()
            .is_some(),
        "undo of rename must restore the old name's ref"
    );
    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("feature-v2"))
            .unwrap()
            .is_none(),
        "undo of rename must delete the new name's ref"
    );

    // ThreadManager: no orphan under the new name. (The record under the
    // old name is a separate pre-existing concern: `cmd_thread_rename`
    // doesn't update the record forward, so the record is still keyed at
    // `feature` throughout. The contract here is just "the inverse mustn't
    // *introduce* a divergence" — we don't try to heal forward-path bugs.)
    let manager = ThreadManager::new(repo.heddle_dir());
    assert!(
        manager.find_by_thread("feature-v2").unwrap().is_none(),
        "undo of rename must not leave a ThreadManager record under the new name \
         (heddle#23 r2 — cross-thread undo contract rule 4)"
    );
}

/// After `thread create` → `undo` → `redo`, the ThreadManager record must
/// be present again with the same structural metadata (mode, base_state,
/// id) it had pre-undo. Pre-fix `apply_redo_entry`'s `ThreadCreate` arm
/// only restored the ref via `set_thread`; the record stayed gone because
/// undo had destroyed it with no snapshot for redo to read back. Phantom
/// shape: post-redo the thread ref exists but `heddle thread show`,
/// `thread cd`, and any record-keyed command (delegate, integration
/// policy) silently degrade. heddle#23 r2 Codex P1 (thread 3254698975).
#[test]
fn test_redo_thread_create_restores_manager_record() {
    use repo::ThreadManager;

    let temp = bootstrap_repo_with_initial_state();

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());

    // Capture the structurally load-bearing fields the redo must restore.
    let (orig_id, orig_mode, orig_base_state, orig_target_thread) = {
        let repo = Repository::open(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());
        let record = manager
            .find_by_thread("feature")
            .unwrap()
            .expect("`thread create` writes a ThreadManager record");
        (
            record.id.clone(),
            record.mode.clone(),
            record.base_state.clone(),
            record.target_thread.clone(),
        )
    };

    heddle_must_succeed(&["undo"], temp.path());
    heddle_must_succeed(&["redo"], temp.path());

    let repo = Repository::open(temp.path()).unwrap();

    // The ref is back — that part already worked pre-fix.
    assert!(
        repo.refs()
            .get_thread(&ThreadName::new("feature"))
            .unwrap()
            .is_some(),
        "redo of `thread create` must restore the thread ref"
    );

    // The fix: the ThreadManager record must also be back, with the
    // structural fields the original record had. Without this, record-
    // backed commands (`thread cd`, delegate, integration policy)
    // silently degrade after an undo/redo round-trip.
    let manager = ThreadManager::new(repo.heddle_dir());
    let restored = manager.find_by_thread("feature").unwrap().expect(
        "redo of `thread create` must recreate the ThreadManager record \
         (heddle#23 r2 Codex P1 — record/redo symmetry, cross-thread \
         undo contract rule 4)",
    );
    assert_eq!(restored.id, orig_id, "id must round-trip");
    assert_eq!(
        format!("{:?}", restored.mode),
        format!("{:?}", orig_mode),
        "mode must round-trip"
    );
    assert_eq!(
        restored.base_state, orig_base_state,
        "base_state must round-trip"
    );
    assert_eq!(
        restored.target_thread, orig_target_thread,
        "target_thread must round-trip"
    );
}

/// `heddle undo --preview` (alias `--dry-run`) must surface the
/// worktree-attached refusal pre-mutation, matching the same
/// preview-honesty rule used by the redaction gate at undo.rs:88.
/// Pre-fix `--preview` would happily print "Would undo …" for a chain
/// the real `undo` would reject, then the user runs the real command
/// and is surprised by the refusal.
#[test]
fn test_undo_preview_surfaces_worktree_refusal() {
    let temp = bootstrap_repo_with_initial_state();

    let wt_path = temp.path().join("feature-wt");
    heddle_must_succeed(
        &[
            "start",
            "feature",
            "--path",
            wt_path.to_str().unwrap(),
            "--workspace",
            "solid",
        ],
        temp.path(),
    );

    let err = heddle(&["undo", "--preview"], Some(temp.path())).expect_err(
        "`undo --preview` must refuse a worktree-attached ThreadCreate \
                     instead of advertising 'Would undo …'",
    );
    let lower = err.to_lowercase();
    assert!(
        lower.contains("worktree") || lower.contains("materialized"),
        "preview refusal must name the worktree concern: {err}"
    );
}

// ---------------------------------------------------------------------------
// heddle#110 — Rule-7 sweep for the remaining `fast_forward_attached`
// callers (rebase / pull / ship / merge-abort). Each daily-use command
// recorded an implicit `OpRecord::Goto` for its FF, and the `Goto`
// inverse only rewinds HEAD — silently stranding the attached thread
// ref at the post-FF target. heddle#99 closed the bug for `merge` by
// emitting `OpRecord::FastForwardV2` instead; this PR extends the same
// pattern to the other call sites. Per-site tests below pin each fix.
// ---------------------------------------------------------------------------

/// Rebase fast-forward (ancestor path): when `current → target` is a
/// pure ancestor relation, `heddle rebase target` short-circuits to a
/// single `fast_forward_attached` call. Undo must restore both HEAD
/// and the rebased thread's ref to its pre-rebase tip — pre-fix the
/// ref was stranded at `target` while HEAD rewound to the pre-rebase
/// state, leaving the repo in a divergent shape.
#[test]
fn test_undo_rebase_ancestor_ff_restores_thread_ref() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    // Build feature past main: feature is a strict descendant of main.
    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], temp.path());
    let feature_tip = head_short(temp.path());

    // Back on main; main is an ancestor of feature, so rebase is a
    // pure FF that flows through `rebase/mod.rs:177`.
    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    let main_tip_before = head_short(temp.path());
    heddle_must_succeed(&["rebase", "feature"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        feature_tip,
        "ancestor rebase must FF main to feature's tip"
    );

    heddle_must_succeed(&["undo"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        main_tip_before,
        "undo of rebase FF must restore HEAD to main's pre-rebase tip"
    );
    let repo = Repository::open(temp.path()).unwrap();
    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, main_tip_before,
        "undo of rebase FF must restore main thread ref to pre-rebase tip \
         (heddle#110 — was stranded at the FF target before the fix)"
    );
    match repo.head_ref().unwrap() {
        refs::Head::Attached { thread } => assert_eq!(
            thread, "main",
            "HEAD must stay attached to main after rebase FF undo"
        ),
        refs::Head::Detached { state } => panic!(
            "HEAD must stay attached to main; got detached at {}",
            state.short()
        ),
    }
}

/// Rebase replay: when the threads have diverged, rebase replays
/// each commit one at a time. Each replay step records its own
/// `OpRecord::FastForwardV2`, so a single `heddle undo` after the
/// rebase rewinds exactly one replayed commit — and the thread ref
/// rewinds with it. Pre-fix, the ref was stranded at the last
/// replayed commit while HEAD rewound to the prior step.
#[test]
fn test_undo_rebase_replay_restores_thread_ref() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    // feature diverges from main on a different file.
    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature commit"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    std::fs::write(temp.path().join("main.txt"), "main work").unwrap();
    heddle_must_succeed(&["capture", "-m", "main commit"], temp.path());
    let main_tip_before = head_short(temp.path());

    // Rebase main onto feature: replays main's commit on top of
    // feature, flowing through `rebase_ops.rs:284` (apply_commit).
    heddle_must_succeed(&["rebase", "feature"], temp.path());
    let after_rebase = head_short(temp.path());
    assert_ne!(
        after_rebase, main_tip_before,
        "rebase replay must produce a fresh tip distinct from the pre-rebase main"
    );

    // Single undo rewinds the last (and only) replayed commit.
    heddle_must_succeed(&["undo"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        main_tip_before,
        "undo of rebase replay must restore HEAD to main's pre-rebase tip"
    );
    let repo = Repository::open(temp.path()).unwrap();
    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, main_tip_before,
        "undo of rebase replay must restore main thread ref to pre-rebase tip \
         (heddle#110 — was stranded at the replay tip before the fix)"
    );
    match repo.head_ref().unwrap() {
        refs::Head::Attached { thread } => assert_eq!(thread, "main"),
        refs::Head::Detached { state } => panic!(
            "HEAD must stay attached to main; got detached at {}",
            state.short()
        ),
    }
}

/// Pull (local sync): `heddle pull <source>` advances the local
/// thread ref to the pulled state and, when the pulled thread is the
/// current checkout, materializes the worktree via
/// `fast_forward_attached`. Undo must restore the local thread ref
/// to its pre-pull tip — pre-fix the implicit `OpRecord::Goto` left
/// the local ref stranded at the pulled state on undo.
#[test]
fn test_undo_pull_local_restores_thread_ref() {
    let source = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();

    // Source repo: two states on `main` so the second pull has a
    // distinct pre-pull tip to restore on undo.
    heddle_must_succeed(&["init"], source.path());
    std::fs::write(source.path().join("a.txt"), "v1").unwrap();
    heddle_must_succeed(&["capture", "-m", "source v1"], source.path());

    // Target repo: start from a baseline capture so `main` exists
    // locally. Pull then advances it.
    heddle_must_succeed(&["init"], target.path());
    heddle_must_succeed(&["capture", "-m", "target init"], target.path());
    let source_path = source.path().to_str().unwrap().to_string();
    heddle_must_succeed(&["pull", &source_path, "--thread", "main"], target.path());
    let main_after_first_pull = head_short(target.path());

    // Advance source so the second pull has somewhere to FF to.
    std::fs::write(source.path().join("a.txt"), "v2").unwrap();
    heddle_must_succeed(&["capture", "-m", "source v2"], source.path());

    heddle_must_succeed(&["pull", &source_path, "--thread", "main"], target.path());
    let main_after_second_pull = head_short(target.path());
    assert_ne!(
        main_after_first_pull, main_after_second_pull,
        "second pull must advance main to a new state"
    );

    heddle_must_succeed(&["undo"], target.path());
    assert_eq!(
        head_short(target.path()),
        main_after_first_pull,
        "undo of pull must restore HEAD to pre-pull tip"
    );
    let repo = Repository::open(target.path()).unwrap();
    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, main_after_first_pull,
        "undo of pull must restore main thread ref to pre-pull tip \
         (heddle#110 — was stranded at the pulled state before the fix)"
    );
}

/// Resolve --abort (merge abort): aborting a conflicted 3-way merge
/// calls `fast_forward_attached(merge_state.ours)` to clean the
/// worktree back to the pre-merge state. During a 3-way conflict
/// merge HEAD never moves (only the worktree gets conflict markers),
/// so the FF is a worktree reset and the recorded
/// `FastForwardV2`'s pre/post target ids are equal. The contract
/// pinned here is that undo of the abort leaves the thread ref at
/// `ours` — same as before the abort — rather than stranding it
/// elsewhere. Pre-fix the implicit `OpRecord::Goto` happened to
/// produce the same observable end state here (no strand because
/// pre = post), but the migration to `FastForwardV2` keeps the
/// invariant uniform across all `fast_forward_attached` call sites
/// and future-proofs against a partial-apply merge variant that
/// might move HEAD before abort.
#[test]
fn test_undo_resolve_abort_keeps_thread_ref_at_ours() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("conflict.txt"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("conflict.txt"), "feature edit\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature edit"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    std::fs::write(temp.path().join("conflict.txt"), "main edit\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "main edit"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    let feature_tip_before = head_short(temp.path());
    let refresh = heddle(
        &["thread", "refresh", "feature", "--output", "json"],
        Some(temp.path()),
    );
    assert!(
        refresh
            .as_ref()
            .is_err_and(|err| err.contains("thread_refresh_conflicted")),
        "refresh should create a durable conflict state: {refresh:?}"
    );

    heddle_must_succeed(&["resolve", "--abort"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        feature_tip_before,
        "abort must leave HEAD at feature's pre-refresh tip"
    );

    // Undo the abort — `FastForwardV2 { pre = post = feature_tip_before }`
    // so the observable state is unchanged.
    heddle_must_succeed(&["undo"], temp.path());
    let repo = Repository::open(temp.path()).unwrap();
    let feature_tip = repo
        .refs()
        .get_thread(&ThreadName::new("feature"))
        .unwrap()
        .expect("feature thread still exists")
        .short();
    assert_eq!(
        feature_tip, feature_tip_before,
        "undo of refresh abort must leave feature thread ref at the pre-refresh tip"
    );
    match repo.head_ref().unwrap() {
        refs::Head::Attached { thread } => assert_eq!(thread, "feature"),
        refs::Head::Detached { state } => panic!(
            "HEAD must stay attached to feature; got detached at {}",
            state.short()
        ),
    }
}

/// Ship (manual-resolution adopt path): `heddle ship` calls
/// `adopt_manual_resolution`, which fast-forwards the current
/// attached thread to a manually-resolved tip. Undo must restore
/// the attached thread's ref to its pre-ship tip — pre-fix the
/// implicit `OpRecord::Goto` left the ref stranded at the adopted
/// state.
///
/// The ship-via-manual-resolution path requires a materialized
/// thread workspace and `integration_policy.manual_resolution_state`
/// set. We bootstrap that here by `heddle start --workspace
/// materialized`, capturing work in the side worktree, then running
/// `thread resolve` from main to flip the resolution flag. `heddle
/// ship --thread <feature>` then enters `adopt_manual_resolution`,
/// whose `fast_forward_attached` call we're pinning.
///
/// In environments where ship can't reach the adopt branch (no
/// git-overlay, no hosted config), we fall back to asserting that
/// the migration didn't break the `thread resolve` flow itself.
#[test]
fn test_undo_ship_manual_resolution_restores_thread_ref() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("a.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    // Create a materialized side worktree for feature so ship can
    // open the thread's checkout.
    let feature_wt = temp.path().join("feature-wt");
    heddle_must_succeed(
        &[
            "start",
            "feature",
            "--path",
            feature_wt.to_str().unwrap(),
            "--workspace",
            "materialized",
        ],
        temp.path(),
    );
    std::fs::write(feature_wt.join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], &feature_wt);

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    let main_tip_before = head_short(temp.path());

    // `thread resolve` flips manual_resolution_state on feature
    // when the thread merges cleanly. This is the trigger
    // `adopt_manual_resolution` looks for during ship.
    let _ = heddle(&["thread", "resolve", "feature"], Some(temp.path()));

    let ship = heddle(
        &["--output", "json", "ship", "--thread", "feature"],
        Some(temp.path()),
    );
    let ship_out = match ship {
        Ok(out) => out,
        Err(err) => {
            panic!("ship failed: {err}");
        }
    };
    assert!(
        ship_out.contains("\"status\":\"shipped\"") || ship_out.contains("\"status\": \"shipped\""),
        "ship must reach the manual-resolution adopt path: {ship_out}"
    );
    let after_ship = head_short(temp.path());
    assert_ne!(
        after_ship, main_tip_before,
        "ship must advance main; otherwise the FF is a no-op and there's nothing to undo: {ship_out}"
    );

    heddle_must_succeed(&["undo"], temp.path());
    let repo = Repository::open(temp.path()).unwrap();
    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, main_tip_before,
        "undo of ship must restore main thread ref to pre-ship tip \
         (heddle#110 — was stranded at the adopted state before the fix)"
    );
}

/// Deterministic redo for `rebase`: forward FF → undo → advance the
/// source thread → redo must replay to the recorded post-FF SHA,
/// not re-resolve from the source thread's (now advanced) tip. This
/// pins heddle#99 r2's deterministic-redo contract for the rebase
/// call sites: the recorded FastForwardV2 carries `post_target_id`
/// so the OpRecord is self-sufficient.
#[test]
fn test_redo_rebase_pins_recorded_tip_when_source_advances() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], temp.path());
    let feature_at_rebase = head_short(temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    let main_tip_before = head_short(temp.path());
    // Ancestor FF (main → feature): records FastForwardV2 with
    // post_target_id = feature's current tip.
    heddle_must_succeed(&["rebase", "feature"], temp.path());
    assert_eq!(head_short(temp.path()), feature_at_rebase);

    heddle_must_succeed(&["undo"], temp.path());
    assert_eq!(head_short(temp.path()), main_tip_before);

    // Advance feature after undo. Pre-fix (or under a name-resolve
    // redo), this new tip would be smuggled into the redo target.
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature + more").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature again"], temp.path());
    let feature_advanced = head_short(temp.path());
    assert_ne!(feature_at_rebase, feature_advanced);

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    heddle_must_succeed(&["redo"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        feature_at_rebase,
        "redo of rebase FF must replay to the recorded post-FF SHA, \
         not feature's advanced tip"
    );
    let repo = Repository::open(temp.path()).unwrap();
    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(main_tip, feature_at_rebase);
}

/// Deterministic redo for `pull`: forward pull → undo → advance the
/// remote source thread → redo must replay to the recorded pulled
/// SHA, not re-resolve the remote's current tip. The bug shape is
/// identical to the rebase case above; this pin guarantees the
/// FastForwardV2 contract holds on the pull call site too.
#[test]
fn test_redo_pull_pins_recorded_tip_when_source_advances() {
    let source = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();

    heddle_must_succeed(&["init"], source.path());
    std::fs::write(source.path().join("a.txt"), "v1").unwrap();
    heddle_must_succeed(&["capture", "-m", "source v1"], source.path());

    heddle_must_succeed(&["init"], target.path());
    heddle_must_succeed(&["capture", "-m", "target init"], target.path());
    let source_path = source.path().to_str().unwrap().to_string();
    heddle_must_succeed(&["pull", &source_path, "--thread", "main"], target.path());
    let main_after_first_pull = head_short(target.path());

    std::fs::write(source.path().join("a.txt"), "v2").unwrap();
    heddle_must_succeed(&["capture", "-m", "source v2"], source.path());
    heddle_must_succeed(&["pull", &source_path, "--thread", "main"], target.path());
    let main_after_second_pull = head_short(target.path());

    heddle_must_succeed(&["undo"], target.path());
    assert_eq!(head_short(target.path()), main_after_first_pull);

    // Advance the source past the recorded pull target. Pre-fix
    // redo would re-resolve `main → tip` from the source and pull
    // *this* state, not the originally-pulled SHA.
    std::fs::write(source.path().join("a.txt"), "v3").unwrap();
    heddle_must_succeed(&["capture", "-m", "source v3"], source.path());

    heddle_must_succeed(&["redo"], target.path());
    assert_eq!(
        head_short(target.path()),
        main_after_second_pull,
        "redo of pull must replay to the recorded pulled SHA, \
         not the source's advanced tip"
    );
}

// ---------------------------------------------------------------------------
// heddle#198 — `heddle undo` for `heddle rebase` via transaction grouping.
//
// Pre-fix, `rebase_ops::replay_commits` recorded one `FastForwardV2` op
// per replayed commit, each in its own undo batch. A 3-commit rebase
// therefore needed 3 `heddle undo` invocations to roll back, and an
// undo chain that stopped one or two steps in left the thread tip
// stranded at an intermediate replayed commit. Post-fix, the whole
// rebase is wrapped in a single oplog batch so one undo rewinds the
// whole rebase atomically — matching the "safety net" framing of
// `heddle undo`.
// ---------------------------------------------------------------------------

/// Red commit: rebase replays multiple commits, then a single `heddle
/// undo` must rewind the entire rebase to the pre-rebase thread tip
/// and the rebased thread ref must follow. Pre-fix this needed N undo
/// steps for N replayed commits; one step rewound only the last
/// replay, leaving the thread tip on a synthetic intermediate state.
#[test]
fn test_undo_rebase_replay_multi_commit_rewinds_whole_transaction() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    // feature diverges on a different file so the rebase replays
    // cleanly (no conflict resolution needed).
    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature commit"], temp.path());

    // main accumulates THREE commits on disjoint paths so the rebase
    // produces three apply_commit calls, each of which today records
    // its own FastForwardV2 entry.
    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    std::fs::write(temp.path().join("a.txt"), "a1").unwrap();
    heddle_must_succeed(&["capture", "-m", "a"], temp.path());
    std::fs::write(temp.path().join("b.txt"), "b1").unwrap();
    heddle_must_succeed(&["capture", "-m", "b"], temp.path());
    std::fs::write(temp.path().join("c.txt"), "c1").unwrap();
    heddle_must_succeed(&["capture", "-m", "c"], temp.path());
    let main_tip_before = head_short(temp.path());

    heddle_must_succeed(&["rebase", "feature"], temp.path());
    let after_rebase = head_short(temp.path());
    assert_ne!(
        after_rebase, main_tip_before,
        "rebase replay must produce a fresh tip distinct from the pre-rebase main"
    );

    // The contract: ONE undo rewinds the whole rebase (not N undos
    // for N replayed commits).
    heddle_must_succeed(&["undo"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        main_tip_before,
        "single undo of a multi-commit rebase must restore HEAD to the pre-rebase tip"
    );
    let repo = Repository::open(temp.path()).unwrap();
    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, main_tip_before,
        "single undo of a multi-commit rebase must restore main thread ref to the pre-rebase tip"
    );
    match repo.head_ref().unwrap() {
        refs::Head::Attached { thread } => assert_eq!(thread, "main"),
        refs::Head::Detached { state } => panic!(
            "HEAD must stay attached to main; got detached at {}",
            state.short()
        ),
    }

    // Materializing the pre-rebase tip must still find the original
    // commits' trees in the store — the append-only object store
    // means the rebase's tree mutations don't displace the originals.
    for path in ["a.txt", "b.txt", "c.txt"].iter() {
        assert!(
            temp.path().join(path).exists(),
            "{path} from the original pre-rebase tree must still materialize after undo"
        );
    }
}

/// Redo symmetry for multi-commit rebase: undo then redo must restore
/// the post-rebase tip in a single redo step (matching the single-step
/// undo). Persists across CLI invocations, same as the existing FF
/// redo surface in `test_redo_rebase_pins_recorded_tip_when_source_advances`.
#[test]
fn test_redo_rebase_replay_multi_commit_restores_post_rebase_tip() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature work").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature commit"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    std::fs::write(temp.path().join("a.txt"), "a1").unwrap();
    heddle_must_succeed(&["capture", "-m", "a"], temp.path());
    std::fs::write(temp.path().join("b.txt"), "b1").unwrap();
    heddle_must_succeed(&["capture", "-m", "b"], temp.path());

    heddle_must_succeed(&["rebase", "feature"], temp.path());
    let after_rebase = head_short(temp.path());

    heddle_must_succeed(&["undo"], temp.path());
    heddle_must_succeed(&["redo"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        after_rebase,
        "single redo of a multi-commit rebase must restore HEAD to the post-rebase tip"
    );
    let repo = Repository::open(temp.path()).unwrap();
    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, after_rebase,
        "single redo of a multi-commit rebase must restore main thread ref to the post-rebase tip"
    );
}

/// AC #4: `heddle undo` must refuse to roll back a rebase batch if
/// the worktree is dirty (uncommitted edits to tracked files or
/// untracked content). The general undo guard already covers this;
/// the test pins that rebase batches go through the same path so a
/// future refactor doesn't accidentally bypass it.
#[test]
fn test_undo_rebase_refuses_when_worktree_dirty() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    std::fs::write(temp.path().join("a.txt"), "a").unwrap();
    heddle_must_succeed(&["capture", "-m", "a"], temp.path());
    std::fs::write(temp.path().join("b.txt"), "b").unwrap();
    heddle_must_succeed(&["capture", "-m", "b"], temp.path());
    let main_tip_before = head_short(temp.path());

    heddle_must_succeed(&["rebase", "feature"], temp.path());
    let after_rebase = head_short(temp.path());
    assert_ne!(after_rebase, main_tip_before);

    // Modify a tracked file post-rebase to put the worktree out of
    // sync with HEAD. The rebase batch must NOT be undone while this
    // edit could be silently destroyed by the rewind.
    std::fs::write(temp.path().join("a.txt"), "uncommitted change").unwrap();
    let err = heddle(&["undo", "--output", "json"], Some(temp.path()))
        .expect_err("undo of rebase must refuse on dirty worktree");
    assert!(
        err.contains("modified") || err.contains("dirty") || err.contains("untracked"),
        "refusal must name the dirty-worktree concern: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "uncommitted change",
        "uncommitted edit must survive the refusal"
    );
    // Tip stays at the post-rebase state — no half-undo.
    assert_eq!(head_short(temp.path()), after_rebase);
}

/// AC #5: `heddle undo` must refuse to roll back a rebase batch when
/// a blob reachable from the pre-rebase tree has been purged since
/// (`Redact apply` + `Purge`). The rewind would land HEAD on a tip
/// whose materialize would fail with a missing-blob error; refusing
/// pre-mutation gives operators a single clear message instead.
/// Mirrors the `Redact` inverse's "Refused regardless of the flag
/// when the underlying bytes have since been purged" rule.
#[test]
fn test_undo_rebase_refuses_when_pre_rebase_blob_purged() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    // `secrets.toml` is captured into a blob that the rebase leaves
    // unchanged (only `a.txt` / `b.txt` move on the main side), so
    // the same blob is reachable from both the pre- and post-rebase
    // trees. Purging it then invalidates the pre-rebase rewind.
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    std::fs::write(
        temp.path().join("config/secrets.toml"),
        b"api_token = \"will-be-purged\"\n",
    )
    .unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    std::fs::write(temp.path().join("a.txt"), "a").unwrap();
    heddle_must_succeed(&["capture", "-m", "a"], temp.path());
    std::fs::write(temp.path().join("b.txt"), "b").unwrap();
    heddle_must_succeed(&["capture", "-m", "b"], temp.path());

    heddle_must_succeed(&["rebase", "feature"], temp.path());

    // Need a state id for `redact apply <state> --path …`. After the
    // rebase, the current state contains config/secrets.toml at the
    // same blob hash as the pre-rebase tree.
    let log_json = heddle_must_succeed(&["--output", "json", "log", "--limit", "1"], temp.path());
    let log: Value = serde_json::from_str(&log_json).unwrap();
    let current_state = log["states"][0]["change_id"].as_str().unwrap().to_string();

    heddle_must_succeed(
        &[
            "redact",
            "apply",
            &current_state,
            "--path",
            "config/secrets.toml",
            "--reason",
            "rebase-undo-safety test",
        ],
        temp.path(),
    );
    heddle_must_succeed(
        &[
            "purge",
            "apply",
            &current_state,
            "--path",
            "config/secrets.toml",
            "--force",
        ],
        temp.path(),
    );

    let err = heddle(&["undo", "--allow-redact-undo"], Some(temp.path()))
        .expect_err("undo of rebase must refuse when a pre-rebase blob has been purged");
    assert!(
        err.to_lowercase().contains("purge") || err.to_lowercase().contains("purged"),
        "refusal must name the purge concern: {err}"
    );
}

/// Pending-advances persistence across `heddle rebase --continue`:
/// when the rebase pauses on a conflict mid-replay, the per-commit
/// FF records that *did* apply cleanly before the conflict must
/// survive the pause and end up in the same final batch as the
/// post-resolution FF. Without persistence the buffered records
/// would be lost on the second `heddle` invocation and the rebase
/// would land with only the post-conflict FFs in the oplog, leaving
/// `heddle undo` unable to rewind back past the conflict point.
#[test]
fn test_undo_rebase_continue_preserves_pre_conflict_advances() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    // Conflict happens on conflict.txt only — main has a clean
    // commit on a.txt first, then a commit that conflicts.
    std::fs::write(temp.path().join("conflict.txt"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("conflict.txt"), "feature version\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature edit"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    // Commit #1 (a.txt) — clean against feature; rebase will apply
    // this one successfully and buffer its FF record in
    // RebaseState.pending_advances.
    std::fs::write(temp.path().join("a.txt"), "a1").unwrap();
    heddle_must_succeed(&["capture", "-m", "a"], temp.path());
    // Commit #2 (conflict.txt) — conflicts with feature; rebase
    // pauses here. The buffered FF for commit #1 must survive the
    // pause via the on-disk REBASE_STATE.
    std::fs::write(temp.path().join("conflict.txt"), "main version\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "main conflict"], temp.path());
    let main_tip_before = head_short(temp.path());

    let rebase_output = heddle(&["rebase", "feature"], Some(temp.path())).unwrap_or_else(|out| out);
    assert!(
        rebase_output.contains("Conflict applying")
            || rebase_output.contains("\"status\": \"conflict\""),
        "expected rebase to pause on conflict; got: {rebase_output}"
    );
    assert!(
        temp.path().join(".heddle/REBASE_STATE").exists(),
        "rebase state should persist while waiting for manual resolution"
    );

    // Resolve via a manual capture, then thread resolve + continue
    // (matching the existing test_rebase_continue_accepts_manual_resolution_snapshot
    // pattern in state_management/merge.rs).
    std::fs::write(
        temp.path().join("conflict.txt"),
        "feature version\nmain version\n",
    )
    .unwrap();
    heddle_must_succeed(&["capture", "-m", "Manual rebase resolution"], temp.path());
    let _ = heddle(&["thread", "resolve", "main", "--json"], Some(temp.path()));
    heddle_must_succeed(&["rebase", "--continue"], temp.path());
    assert!(
        !temp.path().join(".heddle/REBASE_STATE").exists(),
        "REBASE_STATE should clear after a successful continue"
    );

    // Single undo must rewind back past BOTH the pre-conflict FF
    // (commit #1) AND the post-resolution FF — i.e. all the way to
    // the pre-rebase tip. If pending_advances were lost across the
    // continue, the undo would stop at the pre-conflict point and
    // strand the tip on a synthetic state.
    heddle_must_succeed(&["undo"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        main_tip_before,
        "single undo must restore HEAD to pre-rebase tip even when the rebase paused on a conflict"
    );
    let repo = Repository::open(temp.path()).unwrap();
    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, main_tip_before,
        "single undo must restore main thread ref to pre-rebase tip across a --continue"
    );
}

/// Redo symmetry after a conflict-paused rebase: the manual-resolution
/// step (the user's `capture -m "Manual resolution"` between the pause
/// and the `--continue`) must be folded into the rebase batch so that
/// `undo` → `redo` lands the thread back on the manual-resolution
/// tip — not on the last cleanly replayed pre-conflict commit.
///
/// Pre-fix (Codex PR #218 P1): `resume_manual_resolution_if_present`
/// advances `current_index` after accepting the captured resolution
/// state but never appends an `OpRecord` to `pending_advances`, so the
/// rebase batch's last FF target is the pre-conflict commit's rebased
/// tip, not the manual-resolution tip. Undo of the batch *appears* to
/// work (the first FF's `pre_target_id` is still the pre-rebase tip),
/// but redo replays only the recorded FFs and lands one commit short.
#[test]
fn test_redo_rebase_continue_restores_manual_resolution_tip() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("conflict.txt"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("conflict.txt"), "feature version\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature edit"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    std::fs::write(temp.path().join("a.txt"), "a1").unwrap();
    heddle_must_succeed(&["capture", "-m", "a"], temp.path());
    std::fs::write(temp.path().join("conflict.txt"), "main version\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "main conflict"], temp.path());
    let main_tip_before = head_short(temp.path());

    let rebase_output = heddle(&["rebase", "feature"], Some(temp.path())).unwrap_or_else(|out| out);
    assert!(
        rebase_output.contains("Conflict applying")
            || rebase_output.contains("\"status\": \"conflict\""),
        "expected rebase to pause on conflict; got: {rebase_output}"
    );

    std::fs::write(
        temp.path().join("conflict.txt"),
        "feature version\nmain version\n",
    )
    .unwrap();
    heddle_must_succeed(&["capture", "-m", "Manual rebase resolution"], temp.path());
    let _ = heddle(&["thread", "resolve", "main", "--json"], Some(temp.path()));
    heddle_must_succeed(&["rebase", "--continue"], temp.path());

    let after_rebase = head_short(temp.path());
    assert_ne!(
        after_rebase, main_tip_before,
        "rebase must produce a fresh tip distinct from pre-rebase main"
    );

    heddle_must_succeed(&["undo"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        main_tip_before,
        "single undo of a conflict-paused rebase must restore HEAD to pre-rebase tip"
    );

    heddle_must_succeed(&["redo"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        after_rebase,
        "single redo must restore HEAD to the manual-resolution tip, \
         not the pre-conflict FF target"
    );

    let repo = Repository::open(temp.path()).unwrap();
    let main_tip = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main thread still exists")
        .short();
    assert_eq!(
        main_tip, after_rebase,
        "single redo must restore main thread ref to the manual-resolution tip \
         across a --continue"
    );
}

/// heddle#198 r2 (Codex PR #218 P2): `rebase --abort` must survive a
/// corrupted `pending_advance=` line in REBASE_STATE. Pre-fix the
/// strict loader's hard-fail on the first decode error blocked both
/// abort and continue, leaving the operator stuck in an in-progress
/// rebase with no CLI recovery path. Abort only needs `original_head`
/// to rewind; the buffered FF history is discarded either way.
#[test]
fn test_rebase_abort_tolerates_corrupted_pending_advance_line() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("conflict.txt"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("conflict.txt"), "feature\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature edit"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    // First commit (clean) — buffered as a pending_advance.
    std::fs::write(temp.path().join("a.txt"), "a").unwrap();
    heddle_must_succeed(&["capture", "-m", "a"], temp.path());
    // Second commit conflicts so the rebase pauses with REBASE_STATE
    // persisting at least one `pending_advance=` line on disk.
    std::fs::write(temp.path().join("conflict.txt"), "main\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "main conflict"], temp.path());
    let main_tip_before = head_short(temp.path());

    let rebase_output = heddle(&["rebase", "feature"], Some(temp.path())).unwrap_or_else(|out| out);
    assert!(
        rebase_output.contains("Conflict applying")
            || rebase_output.contains("\"status\": \"conflict\""),
        "expected rebase to pause on conflict; got: {rebase_output}"
    );

    // Simulate a crash mid-write / hand-edit by mangling the first
    // `pending_advance=` line in place. The strict loader rejects this
    // file outright; the abort loader must skip past it.
    let state_path = temp.path().join(".heddle/REBASE_STATE");
    let body = std::fs::read_to_string(&state_path).unwrap();
    assert!(
        body.contains("pending_advance="),
        "fixture precondition: REBASE_STATE must carry at least one pending_advance entry; got:\n{body}"
    );
    let corrupted: String = body
        .lines()
        .map(|line| {
            if line.starts_with("pending_advance=") {
                "pending_advance=not-hex!!".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&state_path, format!("{corrupted}\n")).unwrap();

    // Continue must still refuse loudly — the strict loader is what
    // protects `--continue` from silently flushing a truncated batch.
    let cont_err = heddle(&["rebase", "--continue"], Some(temp.path()))
        .expect_err("continue must hard-fail on corrupted pending_advance");
    assert!(
        cont_err.contains("pending_advance"),
        "continue refusal must name the corrupted record; got: {cont_err}"
    );

    // Abort must succeed: rewind HEAD to original_head and clear state.
    heddle_must_succeed(&["rebase", "--abort"], temp.path());
    assert_eq!(
        head_short(temp.path()),
        main_tip_before,
        "abort must rewind HEAD to original_head even with a corrupted pending_advance line"
    );
    assert!(
        !temp.path().join(".heddle/REBASE_STATE").exists(),
        "REBASE_STATE must be cleared after a successful abort"
    );
}

/// heddle#355: the atomic `undo`/`redo` migration commits a record-less
/// `TransactionCommit` marker batch as its commit point. That marker carries no
/// user-facing operation, so `undo --list` must filter it out — otherwise every
/// undo would leave a phantom "transaction commit" batch in the history view
/// (and the next `undo` would try to undo it). The raw oplog keeps the marker
/// (it is the dedup/commit sentinel); only the history view hides it.
#[test]
fn test_undo_list_hides_atomic_commit_marker_batches() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("a.txt"), "a").unwrap();
    heddle_must_succeed(&["capture", "-m", "a"], temp.path());
    std::fs::write(temp.path().join("b.txt"), "b").unwrap();
    heddle_must_succeed(&["capture", "-m", "b"], temp.path());

    // Undo appends a record-less atomic commit-marker batch to the oplog.
    heddle_must_succeed(&["undo"], temp.path());

    // The RAW oplog does contain the marker-only commit batch (the sentinel)...
    let repo = Repository::open(temp.path()).unwrap();
    let scope = repo.op_scope();
    let raw = repo
        .oplog()
        .recent_batches_scoped(50, Some(&scope))
        .unwrap();
    assert!(
        raw.iter().any(|batch| batch.is_transaction_marker_only()),
        "the atomic undo must have committed a marker-only sentinel batch"
    );

    // ...but `undo --list` filters it, so no listed batch is marker-only.
    let list = heddle_must_succeed(
        &["--output", "json", "undo", "--list", "--depth", "20"],
        temp.path(),
    );
    let parsed: Value = serde_json::from_str(&list).unwrap();
    let batches = parsed["batches"].as_array().unwrap();
    for batch in batches {
        let ops = batch["operations"].as_array().unwrap();
        let only_markers = !ops.is_empty()
            && ops.iter().all(|op| {
                op["description"]
                    .as_str()
                    .is_some_and(|desc| desc.starts_with("transaction commit"))
            });
        assert!(
            !only_markers,
            "undo --list must not surface a record-less commit-marker batch: {list}"
        );
    }
}

/// A rebase batch must show up in `heddle undo --list` as a SINGLE
/// batch with N entries (one per replayed commit), not N separate
/// batches with one entry each. The JSON contract is the structured
/// surface that downstream tools (and our own integration tests)
/// depend on.
#[test]
fn test_undo_list_shows_multi_commit_rebase_as_single_batch() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feat.txt"), "feature").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    std::fs::write(temp.path().join("a.txt"), "a").unwrap();
    heddle_must_succeed(&["capture", "-m", "a"], temp.path());
    std::fs::write(temp.path().join("b.txt"), "b").unwrap();
    heddle_must_succeed(&["capture", "-m", "b"], temp.path());

    heddle_must_succeed(&["rebase", "feature"], temp.path());

    let list = heddle_must_succeed(
        &["--output", "json", "undo", "--list", "--depth", "10"],
        temp.path(),
    );
    let parsed: Value = serde_json::from_str(&list).expect("list output is JSON");
    let batches = parsed
        .get("batches")
        .and_then(|b| b.as_array())
        .expect("list output has batches array");

    // The most recent batch (index 0 — undo --list is most-recent-first)
    // must be the rebase, and it must carry both replayed-commit ops
    // in one batch.
    let rebase_batch = &batches[0];
    let ops = rebase_batch
        .get("operations")
        .and_then(|o| o.as_array())
        .expect("batch has operations array");
    assert!(
        ops.len() >= 2,
        "multi-commit rebase batch must contain >=2 ops; saw {}: {list}",
        ops.len()
    );
    // Every op in the batch must be a fast-forward — no foreign ops
    // should have been folded into the rebase batch.
    for op in ops {
        let desc = op.get("description").and_then(|d| d.as_str()).unwrap_or("");
        assert!(
            desc.starts_with("fast-forward") || desc.starts_with("transaction commit"),
            "rebase batch entry must be FF or txn-commit marker, got: {desc}"
        );
    }
}

/// `heddle rebase <thread>` against the current thread's own tip is a
/// no-op short-circuit. The JSON output path emits `up_to_date` and
/// must not write a rebase batch to the oplog — pre-#198 a stray
/// `record_ff_advance` on an identical tip would have written a
/// zero-delta FF; the deferred-flush refactor preserves the original
/// short-circuit shape (no batch).
#[test]
fn test_rebase_up_to_date_when_already_at_target_emits_json_and_records_nothing() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("a.txt"), "a").unwrap();
    heddle_must_succeed(&["capture", "-m", "a"], temp.path());

    let batches_before = heddle_must_succeed(
        &["--output", "json", "undo", "--list", "--depth", "20"],
        temp.path(),
    );
    let parsed_before: Value = serde_json::from_str(&batches_before).unwrap();
    let count_before = parsed_before["batches"].as_array().unwrap().len();

    let out = heddle_must_succeed(&["--output", "json", "rebase", "main"], temp.path());
    let parsed: Value = serde_json::from_str(out.trim()).expect("up_to_date json");
    assert_eq!(parsed["status"].as_str(), Some("up_to_date"));

    let batches_after = heddle_must_succeed(
        &["--output", "json", "undo", "--list", "--depth", "20"],
        temp.path(),
    );
    let parsed_after: Value = serde_json::from_str(&batches_after).unwrap();
    let count_after = parsed_after["batches"].as_array().unwrap().len();
    assert_eq!(
        count_before, count_after,
        "no-op rebase must not append a batch to the oplog"
    );
}

/// JSON output for the is_ancestor fast-forward arm. Distinct shape
/// from `up_to_date` — must surface `fast_forwarded` with the target
/// SHA so scripted callers can detect "this rebase materialized as a
/// pure FF" vs the multi-commit replay flow. Also exercises the
/// `flush_rebase_batch(&[advance])` single-FF wrap so `undo --list`
/// shows the FF inside a transaction envelope.
#[test]
fn test_rebase_fast_forwarded_json_lists_target_and_creates_batch() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    // Create feature off main, advance it. main is now a strict ancestor
    // of feature → rebasing main onto feature is a pure FF.
    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("f.txt"), "feature").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "main"], temp.path());

    let out = heddle_must_succeed(&["--output", "json", "rebase", "feature"], temp.path());
    let parsed: Value = serde_json::from_str(out.trim()).expect("fast_forwarded json");
    assert_eq!(parsed["status"].as_str(), Some("fast_forwarded"));
    let to = parsed["to"].as_str().expect("to field present");
    assert!(!to.is_empty());

    // The single-FF arm must still wrap in a rebase batch — i.e.
    // [FF, TransactionCommit] — so undo treats it like any other
    // rebase. Verify via undo --list shape.
    let list = heddle_must_succeed(
        &["--output", "json", "undo", "--list", "--depth", "5"],
        temp.path(),
    );
    let parsed_list: Value = serde_json::from_str(&list).unwrap();
    let top = &parsed_list["batches"][0];
    let ops = top["operations"].as_array().unwrap();
    let has_tc = ops.iter().any(|op| {
        op["description"]
            .as_str()
            .is_some_and(|d| d.starts_with("transaction commit"))
    });
    assert!(
        has_tc,
        "single-FF rebase batch must carry TransactionCommit marker"
    );
}

/// JSON output for the multi-commit replay entry path. Must emit
/// `started` with the commits count *before* the per-commit
/// `applying` lines. Pairs with the `completed` event at the end of
/// `replay_commits_internal`.
#[test]
fn test_rebase_started_json_announces_commit_count() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("f.txt"), "feature").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    std::fs::write(temp.path().join("a.txt"), "a").unwrap();
    heddle_must_succeed(&["capture", "-m", "a"], temp.path());
    std::fs::write(temp.path().join("b.txt"), "b").unwrap();
    heddle_must_succeed(&["capture", "-m", "b"], temp.path());

    let out = heddle_must_succeed(&["--output", "json", "rebase", "feature"], temp.path());
    // Output is multi-line JSON ND-stream: started, then per-commit
    // applying, then completed. Pull the first line.
    let first = out.lines().next().expect("at least one json line");
    let parsed: Value = serde_json::from_str(first).expect("started json");
    assert_eq!(parsed["status"].as_str(), Some("started"));
    assert_eq!(parsed["commits"].as_u64(), Some(2));
}

/// `heddle rebase --abort` cleans the REBASE_STATE file and rewinds
/// HEAD to `original_head`. Must emit the `aborted` JSON status and
/// must NOT write a rebase batch to the oplog (the abort is a
/// worktree-only rollback — `handle_abort` doesn't go through
/// `flush_rebase_batch`).
#[test]
fn test_rebase_abort_json_clears_state_without_oplog_batch() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("conflict.txt"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("conflict.txt"), "feature\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature edit"], temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    std::fs::write(temp.path().join("conflict.txt"), "main\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "main edit"], temp.path());
    let head_before = head_short(temp.path());

    // Force the rebase into a conflict pause so we have a
    // REBASE_STATE file to abort.
    let _ = heddle(&["rebase", "feature"], Some(temp.path()));
    assert!(
        temp.path().join(".heddle/REBASE_STATE").exists(),
        "rebase should pause and leave REBASE_STATE on disk"
    );

    let batches_before_abort = heddle_must_succeed(
        &["--output", "json", "undo", "--list", "--depth", "20"],
        temp.path(),
    );
    let count_before = serde_json::from_str::<Value>(&batches_before_abort).unwrap()["batches"]
        .as_array()
        .unwrap()
        .len();

    let out = heddle_must_succeed(&["--output", "json", "rebase", "--abort"], temp.path());
    let parsed: Value = serde_json::from_str(out.trim()).expect("aborted json");
    assert_eq!(parsed["status"].as_str(), Some("aborted"));
    assert!(
        !temp.path().join(".heddle/REBASE_STATE").exists(),
        "abort must remove REBASE_STATE"
    );
    assert_eq!(
        head_short(temp.path()),
        head_before,
        "abort must rewind HEAD to original_head"
    );

    let batches_after = heddle_must_succeed(
        &["--output", "json", "undo", "--list", "--depth", "20"],
        temp.path(),
    );
    let count_after = serde_json::from_str::<Value>(&batches_after).unwrap()["batches"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(
        count_before, count_after,
        "abort is worktree-only — no oplog batch should appear"
    );
}

/// `heddle rebase --abort` / `--continue` against a repo with no
/// in-progress rebase must error rather than no-op. Covers the
/// "No rebase in progress" arms in both `handle_abort` and
/// `handle_continue`.
#[test]
fn test_rebase_abort_and_continue_without_state_error() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("a.txt"), "a").unwrap();
    heddle_must_succeed(&["capture", "-m", "a"], temp.path());

    let abort_err = heddle(&["rebase", "--abort"], Some(temp.path()))
        .expect_err("abort with no rebase must error");
    assert!(
        abort_err.contains("No rebase in progress"),
        "got: {abort_err}"
    );

    let cont_err = heddle(&["rebase", "--continue"], Some(temp.path()))
        .expect_err("continue with no rebase must error");
    assert!(
        cont_err.contains("No rebase in progress"),
        "got: {cont_err}"
    );
}
