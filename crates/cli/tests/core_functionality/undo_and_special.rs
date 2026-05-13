// SPDX-License-Identifier: Apache-2.0
use super::*;

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