// SPDX-License-Identifier: Apache-2.0
use objects::object::ThreadName;

use super::*;

/// Convenience: read the current state's short change-id by opening the repo
/// directly. Used by undo tests that assert HEAD has moved to a specific state.
fn head_short(root: &std::path::Path) -> String {
    let repo = Repository::open(root).unwrap();
    repo.head().unwrap().expect("repo has HEAD").short()
}

/// Drop pack store contents so a removed loose state cannot be resolved via pack.
/// Handles L8 journal subdirs (`.staging`, `.install-intent`, `.pack-locks`) that
/// plain `remove_file` cannot delete (PermissionDenied / EISDIR on directories).
fn wipe_pack_store(repo_root: &std::path::Path) {
    let packs_dir = repo_root.join(".heddle/packs");
    if !packs_dir.exists() {
        return;
    }
    for entry in std::fs::read_dir(&packs_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }
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
    let result = heddle(&["undo", "--redo"], Some(temp.path()));
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
    // A fresh worktree has nothing to capture: `capture` now refuses an
    // empty/clean tree as a noop. Write tracked content first so the
    // baseline capture has something to save (two states are needed so
    // `undo -n 1` has a prior snapshot to target).
    std::fs::write(temp.path().join("seed.txt"), "seed").unwrap();
    heddle_must_succeed(&["capture", "-m", "seed"], temp.path());
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
    // `capture` refuses an empty/clean tree as a noop, so seed tracked
    // content for the baseline before the tracked-file capture below.
    std::fs::write(temp.path().join("seed.txt"), "seed").unwrap();
    heddle_must_succeed(&["capture", "-m", "seed"], temp.path());
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
    heddle_must_succeed(&["undo", "--redo"], temp.path());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("notes.md")).unwrap(),
        "FRICTION ONE\nFRICTION TWO\n",
        "redo must restore the friction content to the worktree"
    );
}

#[test]
fn test_undo_recover_survives_divergent_capture() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("notes.md"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());
    std::fs::write(temp.path().join("notes.md"), "FRICTION ONE\nFRICTION TWO\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "friction"], temp.path());
    let friction_state = head_short(temp.path());

    let undo: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "undo"],
        temp.path(),
    ))
    .unwrap();
    assert_eq!(undo["next_action"], "heddle undo --recover");

    std::fs::write(temp.path().join("notes.md"), "different direction\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "diverge"], temp.path());
    let divergent_tip = head_short(temp.path());
    let head_before = Repository::open(temp.path())
        .unwrap()
        .refs()
        .read_head()
        .unwrap();

    let recovered: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "undo", "--recover"],
        temp.path(),
    ))
    .unwrap();
    assert_eq!(recovered["output_kind"], "undo_recover");
    assert_eq!(recovered["action"], "recover");
    assert_eq!(recovered["status"], "completed");
    assert_eq!(recovered["recovery_state"], friction_state);
    assert!(
        recovered["recommended_action"]
            .as_str()
            .is_some_and(|action| action.starts_with("heddle capture")),
        "{recovered}"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("notes.md")).unwrap(),
        "FRICTION ONE\nFRICTION TWO\n"
    );

    let repo = Repository::open(temp.path()).unwrap();
    assert_eq!(
        repo.head().unwrap().map(|state| state.short()),
        Some(divergent_tip)
    );
    assert_eq!(repo.refs().read_head().unwrap(), head_before);
    let status: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "status"],
        temp.path(),
    ))
    .unwrap();
    assert_eq!(status["verification"]["worktree_state"], "dirty");
}

#[test]
fn test_undo_recover_ref_lives_outside_user_marker_namespace() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("notes.md"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());
    std::fs::write(temp.path().join("notes.md"), "FRICTION\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "friction"], temp.path());
    let friction_state = head_short(temp.path());

    heddle_must_succeed(&["undo"], temp.path());
    let markers: Value = serde_json::from_str(&heddle_must_succeed(
        &["--output", "json", "thread", "marker", "list"],
        temp.path(),
    ))
    .unwrap();
    assert!(
        markers["markers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|marker| marker["name"] != "undo-recovery")
    );

    heddle_must_succeed(
        &["thread", "marker", "create", "undo-recovery"],
        temp.path(),
    );
    heddle_must_succeed(
        &["thread", "marker", "delete", "undo-recovery"],
        temp.path(),
    );
    let repo = Repository::open(temp.path()).unwrap();
    assert_eq!(
        repo.refs()
            .get_undo_recovery()
            .unwrap()
            .map(|state| state.short()),
        Some(friction_state.clone())
    );

    let head_before_recovery = head_short(temp.path());
    heddle_must_succeed(&["undo", "--recover"], temp.path());
    assert_eq!(head_short(temp.path()), head_before_recovery);
    assert_eq!(
        std::fs::read_to_string(temp.path().join("notes.md")).unwrap(),
        "FRICTION\n"
    );
}

#[test]
fn test_undo_recover_is_unshadowable_by_same_named_user_marker() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("notes.md"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());
    let base_state = head_short(temp.path());
    heddle_must_succeed(
        &["thread", "marker", "create", "undo-recovery"],
        temp.path(),
    );

    std::fs::write(temp.path().join("notes.md"), "FRICTION\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "friction"], temp.path());
    let friction_state = head_short(temp.path());
    heddle_must_succeed(&["undo"], temp.path());
    assert_eq!(
        Repository::open(temp.path())
            .unwrap()
            .refs()
            .get_undo_recovery()
            .unwrap()
            .map(|state| state.short()),
        Some(friction_state)
    );
    heddle_must_succeed(&["undo", "--recover"], temp.path());

    assert_eq!(
        std::fs::read_to_string(temp.path().join("notes.md")).unwrap(),
        "FRICTION\n"
    );
    let repo = Repository::open(temp.path()).unwrap();
    assert_eq!(
        repo.head().unwrap().map(|state| state.short()),
        Some(base_state.clone())
    );
    assert_eq!(
        repo.refs()
            .get_marker(&objects::object::MarkerName::new("undo-recovery"))
            .unwrap()
            .map(|state| state.short()),
        Some(base_state)
    );
}

#[test]
fn test_undo_recover_refuses_without_recovery_state_or_clean_worktree() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("notes.md"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    let absent = heddle(
        &["--output", "json", "undo", "--recover"],
        Some(temp.path()),
    )
    .expect_err("recovery without a preserved state must refuse");
    assert!(absent.contains("undo_recovery_unavailable"), "{absent}");

    std::fs::write(temp.path().join("notes.md"), "recover me\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "recoverable"], temp.path());
    heddle_must_succeed(&["undo"], temp.path());
    std::fs::write(temp.path().join("notes.md"), "unsaved\n").unwrap();
    let dirty = heddle(
        &["--output", "json", "undo", "--recover"],
        Some(temp.path()),
    )
    .expect_err("recovery must refuse a dirty worktree");
    assert!(dirty.contains("dirty_worktree"), "{dirty}");
    assert_eq!(
        std::fs::read_to_string(temp.path().join("notes.md")).unwrap(),
        "unsaved\n"
    );
}

#[test]
fn test_undo_recover_refuses_when_preserved_state_is_missing() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("notes.md"), "base\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());
    std::fs::write(temp.path().join("notes.md"), "recover me\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "recoverable"], temp.path());
    let recovery_state = head_short(temp.path());
    heddle_must_succeed(&["undo"], temp.path());

    let state_path = locate_state_loose_file(temp.path(), &recovery_state)
        .expect("preserved state has a loose object");
    std::fs::remove_file(state_path).unwrap();
    wipe_pack_store(temp.path());

    let missing = heddle(
        &["--output", "json", "undo", "--recover"],
        Some(temp.path()),
    )
    .expect_err("recovery must refuse when its preserved state is missing");
    assert!(missing.contains("undo_recovery_state_missing"), "{missing}");
    assert_eq!(
        std::fs::read_to_string(temp.path().join("notes.md")).unwrap(),
        "base\n"
    );
}

/// Dry-run reports the pending undo without changing repository state.
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
    wipe_pack_store(temp.path());

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
        lower.contains("land"),
        "--help should list land as undoable: {help}"
    );
    assert!(
        lower.contains("--recover") && lower.contains("worktree changes"),
        "--help should explain recovery without moving HEAD: {help}"
    );
    assert!(
        lower.contains("push") || lower.contains("pull") || lower.contains("cross-worktree"),
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
            "redact",
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
    // undone with `--allow-redact-undo`, `heddle undo --redo` refuses to
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
    let err = heddle(&["undo", "--redo"], Some(temp.path()))
        .expect_err("redo of an undone Redact must refuse");
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
            "redact",
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
    // `heddle undo --redo --preview` against a previously-undone Redact must
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

    let err = heddle(&["undo", "--redo", "--preview"], Some(temp.path()))
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
    heddle_must_succeed(&["undo", "--redo"], temp.path());

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

/// heddle#469: `thread refresh` updates both the thread ref and the
/// ThreadManager record's base metadata. Undo must restore the whole
/// thread record, including `base_state`, not just the ref pointer.
#[test]
fn test_undo_thread_refresh_restores_base_state() {
    use repo::ThreadManager;

    let temp = bootstrap_repo_with_initial_state();

    heddle_must_succeed(&["thread", "create", "feature"], temp.path());
    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());
    std::fs::write(temp.path().join("feature.txt"), "feature\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "feature work"], temp.path());
    let feature_tip_before_refresh = head_short(temp.path());

    heddle_must_succeed(&["thread", "switch", "main"], temp.path());
    std::fs::write(temp.path().join("main.txt"), "main\n").unwrap();
    heddle_must_succeed(&["capture", "-m", "main advance"], temp.path());
    let refreshed_base = head_short(temp.path());

    heddle_must_succeed(&["thread", "switch", "feature"], temp.path());

    let (base_before_refresh, current_before_refresh) = {
        let repo = Repository::open(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());
        let record = manager
            .find_by_thread("feature")
            .unwrap()
            .expect("feature record exists before refresh");
        (
            record.base_state.clone(),
            record
                .current_state
                .clone()
                .expect("feature has current state before refresh"),
        )
    };

    heddle_must_succeed(&["thread", "refresh", "feature"], temp.path());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("feature.txt")).unwrap(),
        "feature\n",
        "refresh must keep the feature work materialized"
    );
    assert!(
        temp.path().join("main.txt").exists(),
        "test setup must materialize the refreshed base on disk"
    );

    {
        let repo = Repository::open(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());
        let record = manager
            .find_by_thread("feature")
            .unwrap()
            .expect("feature record exists after refresh");
        assert_eq!(
            record.base_state, refreshed_base,
            "refresh must advance feature's recorded base to main"
        );
        assert_ne!(
            record.base_state, base_before_refresh,
            "test setup must actually change base_state"
        );
    }

    heddle_must_succeed(&["undo"], temp.path());

    let repo = Repository::open(temp.path()).unwrap();
    let feature_ref = repo
        .refs()
        .get_thread(&ThreadName::new("feature"))
        .unwrap()
        .expect("feature ref survives refresh undo")
        .short();
    assert_eq!(
        feature_ref, feature_tip_before_refresh,
        "undo of refresh must restore the feature ref"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("feature.txt")).unwrap(),
        "feature\n",
        "undo of refresh must restore the pre-refresh worktree content"
    );
    assert!(
        !temp.path().join("main.txt").exists(),
        "undo of refresh on the checked-out thread must remove files introduced by refresh"
    );

    let manager = ThreadManager::new(repo.heddle_dir());
    let restored = manager
        .find_by_thread("feature")
        .unwrap()
        .expect("feature record survives refresh undo");
    assert_eq!(
        restored.base_state, base_before_refresh,
        "undo of refresh must restore the prior base_state"
    );
    assert_eq!(
        restored.current_state.as_deref(),
        Some(current_before_refresh.as_str()),
        "undo of refresh must restore the manager record's current_state too"
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

/// Undoing a pull must restore both HEAD and the attached thread ref.
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
    // locally. `capture` refuses an empty tree as a noop, so seed
    // tracked content first; the local pull overwrites this baseline
    // thread ref with the pulled state regardless of divergence.
    heddle_must_succeed(&["init"], target.path());
    std::fs::write(target.path().join("target.txt"), "target init").unwrap();
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
/// `FastForward`'s pre/post target ids are equal. The contract
/// pinned here is that undo of the abort leaves the thread ref at
/// `ours` — same as before the abort — rather than stranding it
/// elsewhere. Pre-fix the implicit `OpRecord::Goto` happened to
/// produce the same observable end state here (no strand because
/// pre = post), but the migration to `FastForward` keeps the
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

    // Undo the abort — `FastForward { pre = post = feature_tip_before }`
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

/// Land (manual-resolution adopt path): `heddle land` calls
/// `adopt_manual_resolution`, which fast-forwards the current
/// attached thread to a manually-resolved tip. Undo must restore
/// the attached thread's ref to its pre-land tip — pre-fix the
/// implicit `OpRecord::Goto` left the ref stranded at the adopted
/// state.
///
/// The land-via-manual-resolution path requires a materialized
/// thread workspace and `integration_policy.manual_resolution_state`
/// set. We bootstrap that here by `heddle start --workspace
/// materialized`, capturing work in the side worktree, then running
/// `thread resolve` from main to flip the resolution flag. `heddle
/// land --thread <feature>` then enters `adopt_manual_resolution`,
/// whose `fast_forward_attached` call we're pinning.
#[test]
fn test_undo_land_manual_resolution_restores_thread_ref() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());

    std::fs::write(temp.path().join("a.txt"), "base").unwrap();
    heddle_must_succeed(&["capture", "-m", "base"], temp.path());

    // Create a materialized side worktree for feature so land can
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
    // `adopt_manual_resolution` looks for during land.
    heddle_must_succeed(&["thread", "resolve", "feature"], temp.path());
    {
        use repo::ThreadManager;

        let repo = Repository::open(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());
        let feature = manager
            .find_by_thread("feature")
            .unwrap()
            .expect("feature thread record exists after resolve");
        assert!(
            feature
                .integration_policy_result
                .manual_resolution_state
                .is_some(),
            "thread resolve must record the state that land will adopt"
        );
    }

    let land = heddle(
        &["--output", "json", "land", "--thread", "feature"],
        Some(temp.path()),
    );
    let land_out = match land {
        Ok(out) => out,
        Err(err) => {
            panic!("land failed: {err}");
        }
    };
    assert!(
        land_out.contains("\"status\":\"landed\"") || land_out.contains("\"status\": \"landed\""),
        "land must reach the manual-resolution adopt path: {land_out}"
    );
    let after_ship = head_short(temp.path());
    assert_ne!(
        after_ship, main_tip_before,
        "land must advance main; otherwise the FF is a no-op and there's nothing to undo: {land_out}"
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
        "undo of land must restore main thread ref to pre-land tip \
         (heddle#110 — was stranded at the adopted state before the fix)"
    );
}

/// Deterministic redo for `pull`: forward FF → undo → advance the
/// source thread → redo must replay to the recorded post-FF SHA,
/// not re-resolve from the source thread's (now advanced) tip. This
/// pins the recorded FastForward contract: `post_target_id` makes the
/// operation self-sufficient.
#[test]
fn test_redo_pull_pins_recorded_tip_when_source_advances() {
    let source = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();

    heddle_must_succeed(&["init"], source.path());
    std::fs::write(source.path().join("a.txt"), "v1").unwrap();
    heddle_must_succeed(&["capture", "-m", "source v1"], source.path());

    // `capture` refuses an empty tree as a noop, so seed tracked content
    // for the target's baseline before the pull overwrites it.
    heddle_must_succeed(&["init"], target.path());
    std::fs::write(target.path().join("target.txt"), "target init").unwrap();
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

    heddle_must_succeed(&["undo", "--redo"], target.path());
    assert_eq!(
        head_short(target.path()),
        main_after_second_pull,
        "redo of pull must replay to the recorded pulled SHA, \
         not the source's advanced tip"
    );
}

/// Internal atomic marker batches must not appear as user-undoable operations.
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
