// SPDX-License-Identifier: Apache-2.0
use std::fs;

use objects::object::ChangeId;
use refs::Head;
use serde_json::json;
use tempfile::TempDir;

use crate::{
    ChangedPathFilters, HeddleError, HistoryQuery, RepoConfig, Repository, ThreadFreshness,
    ThreadManager, WorktreeIndex,
};

fn create_test_repo() -> (TempDir, Repository) {
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(temp_dir.path()).unwrap();
    (temp_dir, repo)
}

#[test]
fn test_init_creates_structure() {
    let temp_dir = TempDir::new().unwrap();
    let repo = Repository::init_default(temp_dir.path()).unwrap();

    assert!(temp_dir.path().join(".heddle").exists());
    assert!(temp_dir.path().join(".heddle/config.toml").exists());
    assert!(temp_dir.path().join(".heddle/objects/blobs").exists());
    assert!(temp_dir.path().join(".heddle/objects/trees").exists());
    assert!(temp_dir.path().join(".heddle/objects/states").exists());
    let root_state = repo.head().unwrap().expect("init should seed main state");
    assert_eq!(repo.refs().get_thread("main").unwrap(), Some(root_state));
    let state = repo.store().get_state(&root_state).unwrap().unwrap();
    assert!(state.parents.is_empty());
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
    assert!(tree.is_empty());
}

#[test]
fn test_init_fails_if_exists() {
    let temp_dir = TempDir::new().unwrap();
    Repository::init_default(temp_dir.path()).unwrap();

    let result = Repository::init_default(temp_dir.path());
    assert!(result.is_err());
}

#[test]
fn test_set_shallow_updates_memory_and_persists() {
    let (temp_dir, repo) = create_test_repo();
    let state_id = ChangeId::generate();

    repo.set_shallow(&state_id, &[]).unwrap();

    assert!(repo.is_shallow(&state_id));

    let reopened = Repository::open(temp_dir.path()).unwrap();
    assert!(reopened.is_shallow(&state_id));
}

#[test]
fn test_open_finds_repo() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path().canonicalize().unwrap();
    Repository::init_default(&temp_path).unwrap();

    let sub = temp_path.join("foo/bar");
    fs::create_dir_all(&sub).unwrap();

    let repo = Repository::open(&sub).unwrap();
    assert_eq!(repo.root(), temp_path);
}

#[test]
fn test_snapshot_creates_state() {
    let (temp_dir, repo) = create_test_repo();
    let initial_head = repo.head().unwrap().expect("init should seed main state");

    fs::write(temp_dir.path().join("hello.txt"), "world").unwrap();

    let state = repo
        .snapshot(Some("Initial commit".to_string()), None)
        .unwrap();

    assert_eq!(state.intent, Some("Initial commit".to_string()));
    assert_eq!(state.parents, vec![initial_head]);

    let head = repo.head().unwrap();
    assert_eq!(head, Some(state.change_id));
}

#[test]
fn snapshot_packs_blobs_and_leaves_no_loose_blob_files() {
    // ACID + perf invariant: a successful snapshot must install
    // every new blob through the pack hot path, not as N loose
    // files. If something regresses to per-blob loose writes the
    // snapshot quietly goes back to fsync-per-file and the
    // performance test will catch it later — but this test catches
    // it now, in the unit suite, with a clear assertion.
    let (temp_dir, repo) = create_test_repo();

    for i in 0..50 {
        fs::write(
            temp_dir.path().join(format!("file_{i}.txt")),
            format!("content {i}\n"),
        )
        .unwrap();
    }
    let state = repo.snapshot(None, None).unwrap();

    let blobs_dir = temp_dir.path().join(".heddle/objects/blobs");
    let loose_count = fs::read_dir(&blobs_dir)
        .map(|iter| iter.count())
        .unwrap_or(0);
    assert_eq!(
        loose_count, 0,
        "snapshot left {loose_count} loose blob shards behind; expected 0 (everything in a pack)",
    );

    // The state itself must be reachable through the ref the
    // snapshot returned — no orphaned commit.
    let head = repo.head().unwrap();
    assert_eq!(head, Some(state.change_id));
}

#[test]
fn snapshot_failure_leaves_ref_unchanged() {
    // ACID atomicity: a failed snapshot must not advance the head.
    // Stage an unresolved merge — `snapshot()` checks for one up
    // front and returns `HeddleError::Conflict` before any writes —
    // then assert the head is identical to its pre-call value.
    use objects::object::ChangeId;

    let (temp_dir, repo) = create_test_repo();
    fs::write(temp_dir.path().join("a.txt"), "a").unwrap();
    let baseline = repo.snapshot(None, None).unwrap();

    let theirs = ChangeId::from_bytes([0xff; 16]);
    repo.merge_state_manager()
        .start(
            baseline.change_id,
            theirs,
            None,
            vec!["unresolved.txt".to_string()],
        )
        .unwrap();

    fs::write(temp_dir.path().join("b.txt"), "b").unwrap();
    let result = repo.snapshot(Some("would-fail".to_string()), None);
    assert!(matches!(result, Err(HeddleError::Conflict(_))));

    // Head must still point at the baseline state — not at any
    // half-written successor.
    let head_after = repo.head().unwrap();
    assert_eq!(head_after, Some(baseline.change_id));

    // Clean up so the harness's drop doesn't trip on a stale merge.
    repo.merge_state_manager().abort().unwrap();
}

#[test]
fn test_snapshot_with_parent() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("a.txt"), "a").unwrap();
    let state1 = repo.snapshot(Some("First".to_string()), None).unwrap();

    fs::write(temp_dir.path().join("b.txt"), "b").unwrap();
    let state2 = repo.snapshot(Some("Second".to_string()), None).unwrap();

    assert_eq!(state2.parents, vec![state1.change_id]);
}

#[test]
fn test_snapshot_without_confidence_records_none() {
    let temp_dir = TempDir::new().unwrap();
    Repository::init_default(temp_dir.path()).unwrap();

    let config_path = temp_dir.path().join(".heddle/config.toml");
    let mut config = RepoConfig::load(&config_path).unwrap();
    config.agent.provider = Some("test-provider".to_string());
    config.agent.model = Some("test-model".to_string());
    config.save(&config_path).unwrap();

    let repo = Repository::open(temp_dir.path()).unwrap();
    fs::write(temp_dir.path().join("agent.txt"), "content").unwrap();

    let state = repo.snapshot(None, None).unwrap();
    assert_eq!(state.confidence, None);
}

#[test]
fn test_goto_restores_state() {
    let (temp_dir, repo) = create_test_repo();

    let file_path = temp_dir.path().join("a.txt");
    fs::write(&file_path, "version 1").unwrap();
    let state1 = repo.snapshot(Some("Version 1".to_string()), None).unwrap();

    let tree1 = repo.store().get_tree(&state1.tree).unwrap().unwrap();
    assert!(
        tree1.get("a.txt").is_some(),
        "state1 should have a.txt in tree"
    );

    fs::write(&file_path, "version 2").unwrap();
    let state2 = repo.snapshot(Some("Version 2".to_string()), None).unwrap();

    let tree2 = repo.store().get_tree(&state2.tree).unwrap().unwrap();
    assert!(
        tree2.get("a.txt").is_some(),
        "state2 should have a.txt in tree"
    );
    assert_ne!(
        tree1.get("a.txt").unwrap().hash,
        tree2.get("a.txt").unwrap().hash
    );

    let blob1 = repo
        .store()
        .get_blob(&tree1.get("a.txt").unwrap().hash)
        .unwrap();
    assert!(blob1.is_some(), "blob for a.txt v1 should exist");
    assert_eq!(blob1.unwrap().content_str(), Some("version 1"));

    repo.goto(&state1.change_id).unwrap();

    assert!(file_path.exists(), "a.txt should exist after goto");
    let content = fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "version 1");
}

#[test]
fn test_goto_clears_non_empty_directories() {
    let (temp_dir, repo) = create_test_repo();

    let sub_dir = temp_dir.path().join("subdir");
    fs::create_dir(&sub_dir).unwrap();
    fs::write(sub_dir.join("file.txt"), "content").unwrap();

    let state1 = repo
        .snapshot(Some("With subdir".to_string()), None)
        .unwrap();

    fs::write(temp_dir.path().join("new_file.txt"), "new").unwrap();

    repo.goto(&state1.change_id).unwrap();

    assert!(!temp_dir.path().join("new_file.txt").exists());
    assert!(temp_dir.path().join("subdir").exists());
    assert!(temp_dir.path().join("subdir/file.txt").exists());
}

#[test]
fn test_build_tree_rejects_large_files() {
    let (temp_dir, repo) = create_test_repo();

    let large_file = temp_dir.path().join("large.txt");
    let content = vec![b'a'; 101 * 1024 * 1024]; // 101 MB
    fs::write(&large_file, content).unwrap();

    let result = repo.build_tree(temp_dir.path());
    assert!(matches!(result, Err(HeddleError::InvalidFileSize(_))));
}

#[test]
fn test_compare_worktree_cached_does_not_store_modified_file_blob() {
    let (temp_dir, repo) = create_test_repo();
    let path = temp_dir.path().join("tracked.txt");

    fs::write(&path, "v1").unwrap();
    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
    let blob_count_before = repo.store().list_blobs().unwrap().len();

    fs::write(&path, "v2").unwrap();

    let status = repo.compare_worktree_cached(&tree).unwrap();
    let blob_count_after = repo.store().list_blobs().unwrap().len();

    assert_eq!(
        status.modified,
        vec![std::path::PathBuf::from("tracked.txt")]
    );
    assert_eq!(status.added, Vec::<std::path::PathBuf>::new());
    assert_eq!(status.deleted, Vec::<std::path::PathBuf>::new());
    assert_eq!(blob_count_after, blob_count_before);
}

#[test]
fn test_compare_worktree_cached_does_not_store_added_file_blob() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("tracked.txt"), "v1").unwrap();
    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
    let blob_count_before = repo.store().list_blobs().unwrap().len();

    fs::write(temp_dir.path().join("added.txt"), "new file").unwrap();

    let status = repo.compare_worktree_cached(&tree).unwrap();
    let blob_count_after = repo.store().list_blobs().unwrap().len();

    assert_eq!(status.added, vec![std::path::PathBuf::from("added.txt")]);
    assert_eq!(status.modified, Vec::<std::path::PathBuf>::new());
    assert_eq!(status.deleted, Vec::<std::path::PathBuf>::new());
    assert_eq!(blob_count_after, blob_count_before);
}

#[test]
fn test_compare_worktree_cached_marks_tracked_file_replaced_by_directory_modified() {
    let (temp_dir, repo) = create_test_repo();
    let path = temp_dir.path().join("tracked.txt");

    fs::write(&path, "v1").unwrap();
    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("nested.txt"), "nested").unwrap();

    let status = repo.compare_worktree_cached(&tree).unwrap();

    assert_eq!(
        status.modified,
        vec![std::path::PathBuf::from("tracked.txt")]
    );
}

#[test]
fn test_compare_worktree_cached_reports_nested_additions_under_file_directory_collision() {
    let (temp_dir, repo) = create_test_repo();
    let path = temp_dir.path().join("tracked.txt");

    fs::write(&path, "v1").unwrap();
    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("nested.txt"), "nested").unwrap();

    let status = repo.compare_worktree_cached(&tree).unwrap();

    assert_eq!(
        status.modified,
        vec![std::path::PathBuf::from("tracked.txt")]
    );
    assert_eq!(
        status.added,
        vec![std::path::PathBuf::from("tracked.txt/nested.txt")]
    );
}

#[test]
fn test_compare_worktree_cached_persists_pure_untracked_subtree_results() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("tracked.txt"), "tracked").unwrap();
    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    let untracked_dir = temp_dir.path().join("scratch/nested");
    fs::create_dir_all(&untracked_dir).unwrap();
    fs::write(untracked_dir.join("file.txt"), "scratch").unwrap();

    let first = repo.compare_worktree_cached(&tree).unwrap();
    let index_path = temp_dir.path().join(".heddle/state/index.bin");
    let index = WorktreeIndex::load(&index_path).unwrap();

    assert_eq!(
        first.added,
        vec![std::path::PathBuf::from("scratch/nested/file.txt")]
    );
    assert_eq!(
        index
            .get_untracked_directory("scratch")
            .map(|entry| entry.added_paths.clone()),
        Some(vec!["nested/file.txt".to_string()])
    );

    let second = repo.compare_worktree_cached(&tree).unwrap();
    assert_eq!(second.added, first.added);
    assert_eq!(second.modified, first.modified);
    assert_eq!(second.deleted, first.deleted);
}

#[test]
fn test_compare_worktree_cached_detailed_uses_untracked_subtrees_and_flattens_exactly() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("tracked.txt"), "tracked").unwrap();
    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    fs::create_dir_all(temp_dir.path().join("scratch/nested")).unwrap();
    fs::write(temp_dir.path().join("scratch/nested/file-a.txt"), "a").unwrap();
    fs::write(temp_dir.path().join("scratch/nested/file-b.txt"), "b").unwrap();
    fs::write(temp_dir.path().join("loose.txt"), "loose").unwrap();

    let detailed = repo.compare_worktree_cached_detailed(&tree).unwrap();
    let mut flattened = detailed.clone().into_flat_status();
    let mut exact = repo.compare_worktree_cached(&tree).unwrap();

    flattened.modified.sort();
    flattened.added.sort();
    flattened.deleted.sort();
    exact.modified.sort();
    exact.added.sort();
    exact.deleted.sort();

    assert!(detailed.modified.is_empty());
    assert!(detailed.deleted.is_empty());
    assert_eq!(
        detailed.untracked.files,
        vec![std::path::PathBuf::from("loose.txt")]
    );
    assert_eq!(detailed.untracked.subtrees.len(), 1);
    assert_eq!(
        detailed.untracked.subtrees[0].root,
        std::path::PathBuf::from("scratch")
    );
    assert_eq!(
        detailed.untracked.subtrees[0].relative_files,
        vec![
            "nested/file-a.txt".to_string(),
            "nested/file-b.txt".to_string()
        ]
    );
    assert_eq!(flattened.added, exact.added);
    assert_eq!(flattened.modified, exact.modified);
    assert_eq!(flattened.deleted, exact.deleted);
}

#[test]
#[cfg(target_os = "linux")]
fn test_build_tree_rejects_escaping_symlinks() {
    let (temp_dir, repo) = create_test_repo();

    let symlink_path = temp_dir.path().join("escape");
    let outside_dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside_dir.path(), &symlink_path).unwrap();

    let result = repo.build_tree(temp_dir.path());
    assert!(matches!(result, Err(HeddleError::InvalidSymlinkTarget(_))));
}

#[test]
#[cfg(unix)]
fn test_build_tree_allows_valid_symlinks() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("target.txt"), "target content").unwrap();
    let symlink_path = temp_dir.path().join("link");
    std::os::unix::fs::symlink("target.txt", &symlink_path).unwrap();

    let tree = repo.build_tree(temp_dir.path()).unwrap();
    assert!(tree.get("link").is_some());
}

#[test]
#[cfg(unix)]
fn test_compare_worktree_cached_marks_clean_symlink_index_entry_fresh() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("target.txt"), "target content").unwrap();
    std::os::unix::fs::symlink("target.txt", temp_dir.path().join("link")).unwrap();

    let state = repo.snapshot(Some("initial".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();

    assert!(repo.worktree_is_clean_cached(&tree).unwrap());
    assert!(repo.worktree_is_clean_cached(&tree).unwrap());

    let index_path = temp_dir.path().join(".heddle/state/index.bin");
    let index = WorktreeIndex::load(&index_path).unwrap();
    let metadata = fs::symlink_metadata(temp_dir.path().join("link")).unwrap();

    assert!(index.is_fresh("link", &metadata));
}

#[test]
#[cfg(unix)]
fn test_materialize_tree_creates_symlinks() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("target.txt"), "target").unwrap();
    std::os::unix::fs::symlink("target.txt", temp_dir.path().join("link")).unwrap();

    let tree = repo.build_tree(temp_dir.path()).unwrap();
    assert!(tree.get("link").is_some());

    fs::remove_file(temp_dir.path().join("link")).unwrap();
    fs::remove_file(temp_dir.path().join("target.txt")).unwrap();

    repo.materialize_tree(&tree, temp_dir.path()).unwrap();

    let link_path = temp_dir.path().join("link");
    let target_path = temp_dir.path().join("target.txt");

    // Phase E: tighten this. The pre-Phase-E version used
    // `if link_path.exists() || fs::read_link(...).is_ok()` which silently
    // passed when the symlink was actually a regular file containing the
    // target string. Assert the link is genuinely a symlink.
    let meta =
        fs::symlink_metadata(&link_path).expect("symlink path should exist after materialize");
    assert!(
        meta.file_type().is_symlink(),
        "materialized 'link' must be a real symlink, not a regular file"
    );
    let target = fs::read_link(&link_path).expect("readlink should succeed");
    assert_eq!(target, std::path::Path::new("target.txt"));
    assert!(target_path.exists());
}

#[test]
fn test_materialize_tree_restores_nested_directories() {
    let (temp_dir, repo) = create_test_repo();

    let nested_dir = temp_dir.path().join("src/bin");
    fs::create_dir_all(&nested_dir).unwrap();
    fs::write(
        temp_dir.path().join("Cargo.toml"),
        "[package]\nname='demo'\n",
    )
    .unwrap();
    fs::write(nested_dir.join("app.rs"), "fn main() {}\n").unwrap();

    let tree = repo.build_tree(temp_dir.path()).unwrap();

    fs::remove_dir_all(temp_dir.path().join("src")).unwrap();
    fs::remove_file(temp_dir.path().join("Cargo.toml")).unwrap();

    repo.materialize_tree(&tree, temp_dir.path()).unwrap();

    assert_eq!(
        fs::read_to_string(temp_dir.path().join("Cargo.toml")).unwrap(),
        "[package]\nname='demo'\n"
    );
    assert_eq!(
        fs::read_to_string(temp_dir.path().join("src/bin/app.rs")).unwrap(),
        "fn main() {}\n"
    );
}

#[test]
#[cfg(unix)]
fn test_materialize_tree_restores_executable_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let (temp_dir, repo) = create_test_repo();
    let script_path = temp_dir.path().join("script.sh");

    fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();

    let tree = repo.build_tree(temp_dir.path()).unwrap();
    fs::remove_file(&script_path).unwrap();

    repo.materialize_tree(&tree, temp_dir.path()).unwrap();

    // Two outcomes are correct depending on whether the materializer
    // hardlinks from the canonical loose blob (`0o555`, read-only
    // executable, prevents naive in-place edits from corrupting the
    // shared inode) or copies the bytes via `fs::write` fallback
    // (`0o755`, normal executable). Both preserve the *executable*
    // bit, which is what this test is here to pin. See
    // `repository_materialization::set_hardlink_mode` for the
    // hardlink-mode rationale.
    let restored_mode = fs::metadata(&script_path).unwrap().permissions().mode() & 0o777;
    assert!(
        restored_mode == 0o755 || restored_mode == 0o555,
        "expected 0o755 (fs::write fallback) or 0o555 (hardlinked, read-only-exec), got 0o{:o}",
        restored_mode
    );
    // Whichever mode wired up, the executable bit must be set.
    assert!(
        restored_mode & 0o111 != 0,
        "executable bit must survive materialization, got 0o{:o}",
        restored_mode
    );
}

#[test]
#[cfg(unix)]
fn test_build_tree_rejects_dangling_symlink_escaping_repo() {
    let (temp_dir, repo) = create_test_repo();

    let symlink_path = temp_dir.path().join("escape");
    std::os::unix::fs::symlink("/nonexistent/../../../etc/passwd", &symlink_path).unwrap();

    let result = repo.build_tree(temp_dir.path());
    assert!(
        matches!(result, Err(HeddleError::InvalidSymlinkTarget(_))),
        "Should reject dangling symlink that escapes repo via .. traversal"
    );
}

#[test]
#[cfg(unix)]
fn test_build_tree_allows_dangling_symlink_inside_repo() {
    let (temp_dir, repo) = create_test_repo();

    let symlink_path = temp_dir.path().join("link");
    std::os::unix::fs::symlink("does-not-exist.txt", &symlink_path).unwrap();

    let tree = repo.build_tree(temp_dir.path()).unwrap();
    assert!(
        tree.get("link").is_some(),
        "Should allow dangling symlink inside repo"
    );
}

#[test]
fn test_query_history_filters_by_changed_path() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("src.rs"), "one").unwrap();
    repo.snapshot(Some("add src".to_string()), None).unwrap();

    fs::write(temp_dir.path().join("docs.md"), "docs").unwrap();
    repo.snapshot(Some("add docs".to_string()), None).unwrap();

    fs::write(temp_dir.path().join("src.rs"), "two").unwrap();
    repo.snapshot(Some("update src".to_string()), None).unwrap();

    let query = HistoryQuery::new(repo.head().unwrap())
        .with_limit(10)
        .with_changed_paths(ChangedPathFilters::try_from_paths(["src.rs"]).unwrap());

    let history = repo.query_history(&query).unwrap();
    let intents: Vec<_> = history
        .iter()
        .map(|state| state.intent.as_deref().unwrap_or(""))
        .collect();

    assert_eq!(intents, vec!["update src", "add src"]);
}

#[test]
fn test_query_history_directory_filter_matches_nested_paths() {
    let (temp_dir, repo) = create_test_repo();

    fs::create_dir_all(temp_dir.path().join("src")).unwrap();
    fs::write(temp_dir.path().join("src/lib.rs"), "one").unwrap();
    repo.snapshot(Some("src change".to_string()), None).unwrap();

    fs::write(temp_dir.path().join("top.txt"), "root").unwrap();
    repo.snapshot(Some("root change".to_string()), None)
        .unwrap();

    let query = HistoryQuery::new(repo.head().unwrap())
        .with_limit(10)
        .with_changed_paths(ChangedPathFilters::try_from_paths(["src"]).unwrap());

    let history = repo.query_history(&query).unwrap();

    assert_eq!(history.len(), 1);
    assert_eq!(history[0].intent.as_deref(), Some("src change"));
}

/// Regression: `--since <state> --path <p>` used to silently degrade
/// to "no bound" when `<state>` itself was filtered out by `<p>`.
/// The fix applies `stop_at` BEFORE the path filter, so matches older
/// than the bound never leak into the result. Setup: three snapshots
/// of which the second touches `docs.md` only; bound the walk at the
/// first while filtering for `src.rs`. The first state is filtered
/// out, so under the old behavior its position was None and the bound
/// was a no-op — leaking the older `src.rs` change. With `stop_at`,
/// the walk terminates at the bound regardless of filter, so only
/// post-bound `src.rs` matches survive.
#[test]
fn test_query_history_since_with_path_filter_bounds_walk_first() {
    let (temp_dir, repo) = create_test_repo();

    // Oldest: a `src.rs` change that should NEVER appear once we
    // bound the walk at a later state.
    fs::write(temp_dir.path().join("src.rs"), "v1").unwrap();
    let s1 = repo.snapshot(Some("oldest src".to_string()), None).unwrap();

    // Bound state — touches only docs.md, so it's filtered out by
    // `--path src.rs`. This is the state that historically broke the
    // bound.
    fs::write(temp_dir.path().join("docs.md"), "doc").unwrap();
    let s2 = repo.snapshot(Some("docs only".to_string()), None).unwrap();

    // Post-bound src.rs change — must appear.
    fs::write(temp_dir.path().join("src.rs"), "v2").unwrap();
    repo.snapshot(Some("newer src".to_string()), None).unwrap();

    let query = HistoryQuery::new(repo.head().unwrap())
        .with_limit(10)
        .with_changed_paths(ChangedPathFilters::try_from_paths(["src.rs"]).unwrap())
        .with_stop_at(Some(s2.change_id));

    let history = repo.query_history(&query).unwrap();
    let intents: Vec<_> = history
        .iter()
        .map(|state| state.intent.as_deref().unwrap_or(""))
        .collect();

    // Only the post-bound src.rs change should survive — the oldest
    // src.rs predates the bound and must NOT leak through.
    assert_eq!(intents, vec!["newer src"]);
    // Sanity: confirm the bounded state itself is excluded (it would
    // have been filtered anyway, but the bound is exclusive by
    // contract).
    assert!(!history.iter().any(|s| s.change_id == s2.change_id));
    // And confirm `s1` is excluded — that's the regression.
    assert!(!history.iter().any(|s| s.change_id == s1.change_id));
}

/// Same shape as the path-filter regression but for `--agent`. Bound
/// state's agent doesn't match the filter; matches older than the
/// bound used to leak through. The fix applies `stop_at` before the
/// agent filter.
#[test]
fn test_query_history_since_with_agent_filter_bounds_walk_first() {
    use objects::object::{Agent, Attribution, Principal};
    let (temp_dir, repo) = create_test_repo();

    let claude_attr = Attribution::with_agent(
        Principal::new("Tester", "test@example.com"),
        Agent::new("anthropic", "claude-opus-4"),
    );
    let codex_attr = Attribution::with_agent(
        Principal::new("Tester", "test@example.com"),
        Agent::new("openai", "codex-mini"),
    );

    // Oldest: a Claude capture that must not leak once bounded.
    fs::write(temp_dir.path().join("a.txt"), "1").unwrap();
    let s1 = repo
        .snapshot_with_attribution(Some("old claude".to_string()), None, claude_attr.clone())
        .unwrap();

    // Bound — codex agent, filtered out by `--agent claude`.
    fs::write(temp_dir.path().join("b.txt"), "1").unwrap();
    let s2 = repo
        .snapshot_with_attribution(Some("codex bound".to_string()), None, codex_attr)
        .unwrap();

    // Newer claude capture — must appear.
    fs::write(temp_dir.path().join("c.txt"), "1").unwrap();
    repo.snapshot_with_attribution(Some("new claude".to_string()), None, claude_attr)
        .unwrap();

    let query = HistoryQuery::new(repo.head().unwrap())
        .with_limit(10)
        .with_agent_filter(Some("claude".to_string()))
        .with_stop_at(Some(s2.change_id));

    let history = repo.query_history(&query).unwrap();
    let intents: Vec<_> = history
        .iter()
        .map(|state| state.intent.as_deref().unwrap_or(""))
        .collect();

    assert_eq!(intents, vec!["new claude"]);
    assert!(!history.iter().any(|s| s.change_id == s1.change_id));
}

#[test]
fn test_performance_inspection_reports_repo_shape() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("tracked.txt"), "tracked").unwrap();
    let state = repo.snapshot(Some("tracked".to_string()), None).unwrap();
    let tree = repo.store().get_tree(&state.tree).unwrap().unwrap();
    let _ = repo.compare_worktree_cached(&tree).unwrap();

    let blob_hash = objects::object::ContentHash::compute(b"missing");
    repo.record_missing_blob(blob_hash).unwrap();

    let report = repo.inspect_performance().unwrap();

    assert_eq!(report.ref_counts.threads, 1);
    assert!(report.ref_summary_index.present);
    assert!(report.ref_summary_index.valid);
    assert_eq!(report.ref_summary_index.threads, 1);
    assert!(report.worktree_index.present);
    assert!(report.worktree_index.file_entries >= 1);
    assert_eq!(report.partial_fetch.missing_blob_count, 1);
    assert_eq!(report.pull_planner_cache.status, "absent");
    assert!(!report.pull_planner_cache.present);
    assert!(!report.change_monitor.backend.is_empty());
    assert!(!report.change_monitor.status.is_empty());
}

#[test]
fn test_maintenance_run_builds_commit_graph_and_worktree_index() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("tracked.txt"), "v1").unwrap();
    repo.snapshot(Some("v1".to_string()), None).unwrap();
    fs::write(temp_dir.path().join("tracked.txt"), "v2").unwrap();
    repo.snapshot(Some("v2".to_string()), None).unwrap();

    let graph_path = temp_dir.path().join(".heddle/state/commit-graph.bin");
    let index_path = temp_dir.path().join(".heddle/state/index.bin");
    assert!(!graph_path.exists());
    assert!(!index_path.exists());

    let run = repo.run_maintenance().unwrap();

    assert!(run.rebuilt_commit_graph);
    assert!(run.rebuilt_ref_summary_index);
    assert!(run.rebuilt_worktree_index);
    assert!(graph_path.exists());
    assert!(index_path.exists());
    assert!(run.report.commit_graph.present);
    assert!(run.report.commit_graph.node_count >= 2);
    assert!(run.report.commit_graph.bloom_covered_nodes >= 1);
    assert!(run.report.ref_summary_index.present);
    assert!(run.report.ref_summary_index.valid);
    assert_eq!(run.report.ref_summary_index.threads, 1);
    assert!(run.report.worktree_index.present);
    assert!(run.report.worktree_index.file_entries >= 1);
}

#[test]
fn test_maintenance_run_prunes_and_rebuilds_pull_planner_sidecars() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("README.md"), "alpha").unwrap();
    let state = repo
        .snapshot(Some("alpha".to_string()), None)
        .unwrap()
        .change_id;

    let pull_root = temp_dir
        .path()
        .join(".heddle/state")
        .join("derived-summaries")
        .join("pull");
    let plans_dir = pull_root.join("plans");
    fs::create_dir_all(&plans_dir).unwrap();
    let manifest_path = pull_root.join("cold-clone-manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "generated_at": "2026-01-01T00:00:00Z",
            "repo_path": "org/acme/heddle",
            "head": {
                "kind": "attached",
                "value": "main",
                "head_state": state.to_string_full(),
            },
            "markers": [],
            "threads": [{
                "name": "main",
                "state_id": state.to_string_full(),
            }],
            "thread_entries": [{
                "thread": "main",
                "state_id": state.to_string_full(),
                "planner_key_full": "missing-full.json",
                "planner_key_lazy": "missing-lazy.json",
                "object_count": 0,
                "full_closure_available": true,
            }],
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(plans_dir.join("corrupt-entry.json"), b"corrupt").unwrap();
    let stale_state = ChangeId::generate();
    fs::write(
        plans_dir.join(format!(
            "{}--depth-full--exclude-af1349b9f5f9a1a6--full.json",
            stale_state.to_string_full()
        )),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "generated_at": "2026-01-01T00:00:00Z",
            "repo_path": "org/acme/heddle",
            "remote_state_id": stale_state.to_string_full(),
            "depth": null,
            "exclude_states": [],
            "availability_mode": "Full",
            "object_count": 0,
            "planned_objects": [],
        }))
        .unwrap(),
    )
    .unwrap();

    let before = repo.inspect_performance().unwrap();
    assert!(before.pull_planner_cache.present);
    assert_eq!(before.pull_planner_cache.manifest_count, 1);
    assert_eq!(before.pull_planner_cache.planner_entry_count, 2);

    let run = repo.run_maintenance().unwrap();

    assert!(run.rebuilt_pull_planner_cache);
    assert_eq!(run.pruned_pull_planner_entries, 2);
    assert!(run.report.pull_planner_cache.present);
    assert_eq!(run.report.pull_planner_cache.manifest_count, 1);
    assert_eq!(run.report.pull_planner_cache.planner_entry_count, 2);
    assert!(manifest_path.exists());
    assert_eq!(
        fs::read_dir(plans_dir).unwrap().flatten().count(),
        2,
        "maintenance should leave only the current full and lazy planner entries"
    );
}

/// `Repository::fast_forward_attached` from inside an attached thread must
/// advance the thread's ref AND keep HEAD attached. The low-level
/// `Repository::goto` would silently detach HEAD; this unit test pins the
/// canonical helper's behavior so the merge/rebase/pull regression
/// guarantees stay enforced even if those CLI tests drift.
#[test]
fn test_fast_forward_attached_preserves_head_and_advances_thread() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("base.txt"), "base").unwrap();
    let _state1 = repo.snapshot(Some("Base".to_string()), None).unwrap();

    fs::write(temp_dir.path().join("forward.txt"), "forward").unwrap();
    let state2 = repo.snapshot(Some("Forward".to_string()), None).unwrap();

    // Repo::init_default attaches HEAD to "main"; explicitly rewind the
    // thread ref to state1 so a fast-forward to state2 is meaningful.
    let state1 = repo.head().unwrap().expect("base state should exist");
    repo.refs().set_thread("main", &state1).unwrap();
    repo.refs()
        .write_head(&Head::Attached {
            thread: "main".to_string(),
        })
        .unwrap();

    repo.fast_forward_attached(&state2.change_id).unwrap();

    // Thread ref must advance to the target.
    assert_eq!(
        repo.refs().get_thread("main").unwrap(),
        Some(state2.change_id),
        "main ref must advance to fast-forward target"
    );
    // HEAD must remain attached to "main".
    let head = repo.refs().read_head().unwrap();
    assert!(
        matches!(head, Head::Attached { ref thread } if thread == "main"),
        "HEAD must remain attached to main; got {:?}",
        head
    );
    // If metadata exists for the thread, it must reflect the new state.
    // (`init_default` seeds the ref but not the Thread record; richer
    // CLI-level coverage of the metadata refresh path lives in the merge,
    // rebase, and pull regression integration tests.)
    let manager = ThreadManager::new(repo.heddle_dir());
    if let Some(meta) = manager.find_by_thread("main").unwrap() {
        assert_eq!(
            meta.current_state.as_deref(),
            Some(state2.change_id.short().as_str())
        );
        assert!(matches!(meta.freshness, ThreadFreshness::Current));
    }
}

/// When HEAD is detached, `fast_forward_attached` should advance the
/// worktree to the target and leave HEAD detached (no thread to advance).
#[test]
fn test_fast_forward_attached_when_detached_stays_detached() {
    let (temp_dir, repo) = create_test_repo();

    fs::write(temp_dir.path().join("base.txt"), "base").unwrap();
    let state1 = repo.snapshot(Some("Base".to_string()), None).unwrap();

    fs::write(temp_dir.path().join("forward.txt"), "forward").unwrap();
    let state2 = repo.snapshot(Some("Forward".to_string()), None).unwrap();

    // Detach HEAD at state1.
    repo.goto(&state1.change_id).unwrap();
    assert!(matches!(
        repo.refs().read_head().unwrap(),
        Head::Detached { .. }
    ));

    repo.fast_forward_attached(&state2.change_id).unwrap();

    let head = repo.refs().read_head().unwrap();
    match head {
        Head::Detached { state } => assert_eq!(state, state2.change_id),
        Head::Attached { thread } => panic!(
            "fast_forward_attached must not re-attach a previously-detached HEAD; got Attached({thread})"
        ),
    }
}

/// Regression: `op_scope` must not embed the user's absolute filesystem
/// path. The previous behavior canonicalized HEAD to its absolute path
/// and recorded that on every oplog entry — when an oplog containing
/// those paths shipped in `examples/calculator/.heddle/`, it was a PII
/// leak for anyone who cloned the repo.
///
/// The scope must also distinguish worktrees that share one oplog
/// backend (`undo`/`redo`/`--list` filter by exact-match scope), so the
/// fix must preserve per-worktree uniqueness.
#[test]
fn test_op_scope_is_stable_unique_and_does_not_leak_absolute_path() {
    let (temp_dir, repo) = create_test_repo();
    let scope = repo.op_scope();

    // Stable across calls from the same checkout.
    assert_eq!(scope, repo.op_scope(), "op_scope must be deterministic");

    // No absolute path or home-dir leak.
    let abs_root = temp_dir.path().display().to_string();
    assert!(
        !scope.contains(&abs_root),
        "op_scope leaked absolute path: scope={scope:?} contains repo root {abs_root:?}",
    );
    assert!(
        !scope.contains('/'),
        "op_scope must not contain path separators: {scope:?}",
    );

    // Different worktrees produce different scopes — preserves
    // checkout-local undo/redo when worktrees share an oplog.
    let (other_dir, other_repo) = create_test_repo();
    assert_ne!(
        scope,
        other_repo.op_scope(),
        "different worktrees must have different op_scopes",
    );
    drop(other_dir);
}

/// `op_scope` must be invariant to the directory heddle is invoked
/// from. `Repository::open()` walks upward to find `.heddle/`, so a
/// capture run from `<root>/src/foo/` and one from `<root>` should
/// both write the same scope into the shared oplog. Otherwise
/// subdirectory invocations would record a stranger scope and break
/// undo/redo continuity from the root.
#[test]
fn test_op_scope_is_invariant_to_invocation_cwd() {
    let (temp_dir, repo_from_root) = create_test_repo();
    let nested = temp_dir.path().join("src").join("nested");
    fs::create_dir_all(&nested).unwrap();

    let repo_from_nested = Repository::open(&nested).unwrap();

    assert_eq!(
        repo_from_root.op_scope(),
        repo_from_nested.op_scope(),
        "op_scope must be cwd-invariant; opening from {nested:?} produced a different scope",
    );
}

mod blob_hydrator_callback {
    //! Read-time hydration hook (issue #50).
    //!
    //! When `Repository::require_blob` is called for a hash recorded in
    //! `partial-fetch`, the repo must invoke a registered hydrator,
    //! retry the store read, and clear the missing marker on success.
    //! On failure the underlying error must surface — partial-clone
    //! hydration is not allowed to silently degrade to "blob is just
    //! missing", which would mask network outages.
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use objects::{
        error::Result,
        object::{Blob, ContentHash},
    };

    use super::create_test_repo;
    use crate::{BlobHydrator, HeddleError, Repository};

    /// Test double that records every call and lets the test script the
    /// outcome (success-by-write, hard error, or refuse-to-write).
    struct ScriptedHydrator {
        calls: AtomicUsize,
        seen: Mutex<Vec<ContentHash>>,
        mode: HydratorMode,
    }

    enum HydratorMode {
        /// On hydrate, write `payload` to `repo.store()` so the retry-read finds it.
        WritePayload(Vec<u8>),
        /// Return Err without writing — simulates network failure.
        Fail(String),
        /// Return Ok without writing — caller should still surface MissingObject.
        Lie,
    }

    impl ScriptedHydrator {
        fn new(mode: HydratorMode) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
                mode,
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn hashes_seen(&self) -> Vec<ContentHash> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl BlobHydrator for ScriptedHydrator {
        fn hydrate(&self, repo: &Repository, hash: &ContentHash) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(*hash);
            match &self.mode {
                HydratorMode::WritePayload(payload) => {
                    repo.store().put_blob(&Blob::new(payload.clone()))?;
                    Ok(())
                }
                HydratorMode::Fail(msg) => Err(HeddleError::Config(msg.clone())),
                HydratorMode::Lie => Ok(()),
            }
        }
    }

    #[test]
    fn require_blob_invokes_hydrator_and_clears_marker_on_success() {
        let (_temp, repo) = create_test_repo();
        let payload = b"hydrated bytes".to_vec();
        let hash = Blob::new(payload.clone()).hash();

        repo.record_missing_blob(hash).unwrap();
        assert!(
            repo.is_missing_blob(&hash).unwrap(),
            "precondition: blob must be recorded missing"
        );

        let hydrator = Arc::new(ScriptedHydrator::new(HydratorMode::WritePayload(
            payload.clone(),
        )));
        repo.set_blob_hydrator(hydrator.clone());

        let blob = repo
            .require_blob(&hash)
            .expect("require_blob must hydrate and return the blob");
        assert_eq!(blob.content(), payload.as_slice());
        assert_eq!(hydrator.call_count(), 1, "hydrator must fire exactly once");
        assert_eq!(hydrator.hashes_seen(), vec![hash]);
        assert!(
            !repo.is_missing_blob(&hash).unwrap(),
            "missing marker must be cleared after successful hydration",
        );

        // Subsequent reads must be a cache hit — hydrator stays at 1 call.
        let _ = repo.require_blob(&hash).unwrap();
        assert_eq!(
            hydrator.call_count(),
            1,
            "hydrator must not be re-invoked for a cache hit"
        );
    }

    #[test]
    fn require_blob_surfaces_hydration_error_without_silent_fallback() {
        let (_temp, repo) = create_test_repo();
        let hash = Blob::new(b"will-never-arrive".to_vec()).hash();
        repo.record_missing_blob(hash).unwrap();

        let hydrator = Arc::new(ScriptedHydrator::new(HydratorMode::Fail(
            "upstream offline".to_string(),
        )));
        repo.set_blob_hydrator(hydrator.clone());

        let err = repo
            .require_blob(&hash)
            .expect_err("require_blob must surface the hydrator error");
        let msg = err.to_string();
        assert!(
            msg.contains("upstream offline"),
            "the hydrator's error message must reach the caller verbatim; got: {msg}"
        );
        assert_eq!(hydrator.call_count(), 1);
        assert!(
            repo.is_missing_blob(&hash).unwrap(),
            "marker must remain set when hydration fails so the next attempt also tries to hydrate",
        );
    }

    #[test]
    fn require_blob_returns_missing_object_if_hydrator_lies() {
        // Defensive: if a hydrator returns Ok but doesn't actually write
        // the blob, require_blob must NOT return stale data. It must
        // raise MissingObject so the caller learns the contract was violated.
        let (_temp, repo) = create_test_repo();
        let hash = Blob::new(b"phantom-blob".to_vec()).hash();
        repo.record_missing_blob(hash).unwrap();

        let hydrator = Arc::new(ScriptedHydrator::new(HydratorMode::Lie));
        repo.set_blob_hydrator(hydrator.clone());

        let err = repo
            .require_blob(&hash)
            .expect_err("require_blob must not succeed when the blob is still absent");
        assert!(
            matches!(err, HeddleError::MissingObject { .. }),
            "expected MissingObject, got: {err:?}"
        );
        assert_eq!(hydrator.call_count(), 1);
    }

    #[test]
    fn require_blob_without_hydrator_still_returns_missing_object() {
        // Backwards-compatibility guard: callers that never registered a
        // hydrator (the common path today) must see the same
        // MissingObject error as before #50.
        let (_temp, repo) = create_test_repo();
        let hash = Blob::new(b"no-hydrator".to_vec()).hash();
        repo.record_missing_blob(hash).unwrap();

        let err = repo.require_blob(&hash).expect_err("must error");
        assert!(
            matches!(err, HeddleError::MissingObject { .. }),
            "expected MissingObject, got: {err:?}"
        );
    }

    /// Regression test for the Codex-flagged P1 (PR #53): the lazy-clone
    /// hydrator must survive a `Repository::open` boundary. Without
    /// cross-open reconstruction, `heddle clone --lazy` would work
    /// in-process but every subsequent `heddle <verb>` invocation in a
    /// fresh CLI process would see `MissingObject` because no hydrator
    /// is registered. The factory registry in
    /// [`crate::lazy_hydrator`] closes that gap; this test pins it.
    ///
    /// We register a custom test-only kind so the assertion is
    /// independent of the production git-overlay / hosted factory
    /// implementations.
    #[test]
    fn require_blob_uses_factory_registered_hydrator_after_reopen() {
        use std::path::Path;

        use crate::lazy_hydrator::{
            HydratorSection, LazyHydratorConfig, register_factory,
        };

        // Test-isolated kind name — does not collide with the production
        // "git-overlay" / "hosted" kinds that other tests / CLI startup
        // register.
        const KIND: &str = "test-kind-cross-open-reopen";

        // Build a payload + its blake3 first.
        let payload = b"persisted-and-reopened".to_vec();
        let hash = Blob::new(payload.clone()).hash();

        // Set up a fresh repo and persist the lazy-hydrator metadata
        // pointing at our custom kind. Drop the repo before reopening
        // so we know the hydrator install is happening on the second
        // open, not lingering from the first construction.
        let temp = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let heddle_dir = repo.heddle_dir().to_path_buf();
        repo.record_missing_blob(hash).unwrap();
        // Crucially: do NOT call `set_blob_hydrator` here. The hydrator
        // must come from the factory, not from in-process state.
        let cfg = LazyHydratorConfig {
            hydrator: HydratorSection {
                kind: KIND.to_string(),
                hosted: None,
                git_overlay: None,
            },
        };
        cfg.save(&heddle_dir).unwrap();
        drop(repo);

        // Register the factory that the open path will look up. Done
        // *after* the first repo is dropped to make the order
        // explicit: open → load metadata → consult registry → install.
        let payload_for_factory = payload.clone();
        register_factory(
            KIND,
            Arc::new(
                move |_root: &Path,
                      _section: &HydratorSection|
                      -> Result<Arc<dyn BlobHydrator>> {
                    let bytes = payload_for_factory.clone();
                    let calls = Arc::new(AtomicUsize::new(0));
                    let calls_for_hydrator = Arc::clone(&calls);
                    Ok(Arc::new(InlineHydrator {
                        bytes,
                        calls: calls_for_hydrator,
                    }))
                },
            ),
        );

        // First reopen: the open path should pick up the metadata,
        // consult the registry, and install the factory-built hydrator.
        // require_blob then transparently hydrates.
        let reopened = Repository::open(temp.path()).unwrap();
        let blob = reopened
            .require_blob(&hash)
            .expect("hydrator must be re-installed by Repository::open");
        assert_eq!(blob.content(), payload.as_slice());
        // Marker should now be cleared after the successful hydrate.
        assert!(!reopened.is_missing_blob(&hash).unwrap());
        drop(reopened);

        // Second reopen with a *different* missing blob proves
        // reconstruction isn't a one-shot: each `Repository::open`
        // freshly installs the hydrator from the persisted metadata.
        let payload2 = b"second-reopen".to_vec();
        let hash2 = Blob::new(payload2.clone()).hash();
        // Re-register the factory under the same kind but with the
        // new payload, so the second hydrator delivers `payload2`.
        let payload2_for_factory = payload2.clone();
        register_factory(
            KIND,
            Arc::new(
                move |_root: &Path,
                      _section: &HydratorSection|
                      -> Result<Arc<dyn BlobHydrator>> {
                    let bytes = payload2_for_factory.clone();
                    Ok(Arc::new(InlineHydrator {
                        bytes,
                        calls: Arc::new(AtomicUsize::new(0)),
                    }))
                },
            ),
        );
        let reopened2 = Repository::open(temp.path()).unwrap();
        reopened2.record_missing_blob(hash2).unwrap();
        let blob2 = reopened2
            .require_blob(&hash2)
            .expect("second reopen must also have the hydrator installed");
        assert_eq!(blob2.content(), payload2.as_slice());
    }

    /// Minimal hydrator that writes a fixed payload on each call. Only
    /// used by `require_blob_uses_factory_registered_hydrator_after_reopen`.
    struct InlineHydrator {
        bytes: Vec<u8>,
        calls: Arc<AtomicUsize>,
    }

    impl BlobHydrator for InlineHydrator {
        fn hydrate(&self, repo: &Repository, _hash: &ContentHash) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            repo.store().put_blob(&Blob::new(self.bytes.clone()))?;
            Ok(())
        }
    }
}
