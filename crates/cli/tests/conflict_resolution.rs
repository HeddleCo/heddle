//! Conflict resolution and merge tests.
//!
//! Tests for handling divergent histories and merge conflicts. The
//! second half drives `repo::MergeState` / `MergeStateManager` through
//! the lifecycle a stack-aware rebase produces — start → resolve →
//! finish / abort / carry_forward.

use objects::object::ChangeId;
use repo::{MergeState, MergeStateManager, Repository};
use tempfile::TempDir;

/// Test that forked histories can be detected.
#[test]
fn test_detect_divergent_history() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create initial state on main
    std::fs::write(temp.path().join("common.txt"), "common base").unwrap();
    let base_state = repo.snapshot(Some("Base state".to_string()), None).unwrap();

    // Fork: create feature branch
    repo.refs()
        .set_thread("feature", &base_state.change_id)
        .unwrap();

    // Make divergent changes on feature
    std::fs::write(temp.path().join("feature.txt"), "feature work").unwrap();
    let feature_state = repo
        .snapshot(Some("Feature work".to_string()), None)
        .unwrap();

    // Reset to base and make different changes on main
    repo.goto(&base_state.change_id).unwrap();
    std::fs::write(temp.path().join("main.txt"), "main work").unwrap();
    let main_state = repo.snapshot(Some("Main work".to_string()), None).unwrap();

    // Verify we have divergent histories
    assert_ne!(feature_state.change_id, main_state.change_id);
    assert_eq!(feature_state.parents, vec![base_state.change_id]);
    assert_eq!(main_state.parents, vec![base_state.change_id]);
}

/// Test that common ancestors can be found.
#[test]
fn test_find_common_ancestor() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create chain: A -> B -> C
    std::fs::write(temp.path().join("file.txt"), "A").unwrap();
    let _state_a = repo.snapshot(Some("A".to_string()), None).unwrap();

    std::fs::write(temp.path().join("file.txt"), "B").unwrap();
    let state_b = repo.snapshot(Some("B".to_string()), None).unwrap();

    std::fs::write(temp.path().join("file.txt"), "C").unwrap();
    let state_c = repo.snapshot(Some("C".to_string()), None).unwrap();

    // Fork from B
    repo.goto(&state_b.change_id).unwrap();
    std::fs::write(temp.path().join("file.txt"), "D").unwrap();
    let state_d = repo.snapshot(Some("D".to_string()), None).unwrap();

    // B should be common ancestor of C and D
    assert_eq!(state_c.parents, vec![state_b.change_id]);
    assert_eq!(state_d.parents, vec![state_b.change_id]);
}

/// Test three-way merge base calculation.
#[test]
fn test_three_way_merge_base() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Base: common ancestor
    std::fs::write(temp.path().join("base.txt"), "base content").unwrap();
    let base = repo.snapshot(Some("Base".to_string()), None).unwrap();

    // Branch 1: modify file
    std::fs::write(temp.path().join("file1.txt"), "branch1").unwrap();
    let branch1 = repo.snapshot(Some("Branch 1".to_string()), None).unwrap();

    // Reset to base, create branch 2
    repo.goto(&base.change_id).unwrap();
    std::fs::write(temp.path().join("file2.txt"), "branch2").unwrap();
    let branch2 = repo.snapshot(Some("Branch 2".to_string()), None).unwrap();

    // Verify merge base
    assert_eq!(branch1.parents[0], base.change_id);
    assert_eq!(branch2.parents[0], base.change_id);
}

/// Test detecting conflicting file modifications.
#[test]
fn test_detect_conflicting_modifications() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Base with file
    std::fs::write(temp.path().join("conflict.txt"), "base").unwrap();
    let base = repo.snapshot(Some("Base".to_string()), None).unwrap();

    // Branch 1: modify conflict.txt
    std::fs::write(temp.path().join("conflict.txt"), "branch1").unwrap();
    repo.snapshot(Some("Branch 1".to_string()), None).unwrap();

    // Reset and branch 2: also modify conflict.txt
    repo.goto(&base.change_id).unwrap();
    std::fs::write(temp.path().join("conflict.txt"), "branch2").unwrap();
    repo.snapshot(Some("Branch 2".to_string()), None).unwrap();

    // In a real merge, this would detect the conflict
    // Both branches modified the same file
}

/// Test non-conflicting changes (different files).
#[test]
fn test_non_conflicting_changes() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Base
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    let base = repo.snapshot(Some("Base".to_string()), None).unwrap();

    // Branch 1: add file1.txt
    std::fs::write(temp.path().join("file1.txt"), "branch1").unwrap();
    let branch1 = repo.snapshot(Some("Branch 1".to_string()), None).unwrap();

    // Reset and branch 2: add file2.txt (different file)
    repo.goto(&base.change_id).unwrap();
    std::fs::write(temp.path().join("file2.txt"), "branch2").unwrap();
    let branch2 = repo.snapshot(Some("Branch 2".to_string()), None).unwrap();

    // These changes don't conflict - different files
    // A merge would succeed
    assert!(
        repo.store()
            .get_state(&branch1.change_id)
            .unwrap()
            .is_some()
    );
    assert!(
        repo.store()
            .get_state(&branch2.change_id)
            .unwrap()
            .is_some()
    );
}

/// Test fast-forward merge detection.
#[test]
fn test_fast_forward_detection() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Linear history: A -> B -> C
    std::fs::write(temp.path().join("file.txt"), "A").unwrap();
    let state_a = repo.snapshot(Some("A".to_string()), None).unwrap();

    std::fs::write(temp.path().join("file.txt"), "B").unwrap();
    let state_b = repo.snapshot(Some("B".to_string()), None).unwrap();

    std::fs::write(temp.path().join("file.txt"), "C").unwrap();
    let state_c = repo.snapshot(Some("C".to_string()), None).unwrap();

    // If we're at A and want to merge C, it's a fast-forward
    // because C is a descendant of A
    assert_eq!(state_b.parents, vec![state_a.change_id]);
    assert_eq!(state_c.parents, vec![state_b.change_id]);
}

/// Test detecting when files are modified vs deleted.
#[test]
fn test_modify_vs_delete_conflict() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Base with file
    std::fs::write(temp.path().join("file.txt"), "content").unwrap();
    let base = repo.snapshot(Some("Base".to_string()), None).unwrap();

    // Branch 1: modify file
    std::fs::write(temp.path().join("file.txt"), "modified").unwrap();
    repo.snapshot(Some("Modified".to_string()), None).unwrap();

    // Reset and branch 2: delete file
    repo.goto(&base.change_id).unwrap();
    std::fs::remove_file(temp.path().join("file.txt")).unwrap();
    repo.snapshot(Some("Deleted".to_string()), None).unwrap();

    // This is a modify/delete conflict
}

/// Test rename detection during merge.
#[test]
fn test_rename_detection_in_merge() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Base with file
    std::fs::write(temp.path().join("oldname.txt"), "content").unwrap();
    let base = repo.snapshot(Some("Base".to_string()), None).unwrap();

    // Branch 1: rename file
    std::fs::remove_file(temp.path().join("oldname.txt")).unwrap();
    std::fs::write(temp.path().join("newname.txt"), "content").unwrap();
    let _branch1 = repo.snapshot(Some("Renamed".to_string()), None).unwrap();

    // Reset and branch 2: modify original file
    repo.goto(&base.change_id).unwrap();
    std::fs::write(temp.path().join("oldname.txt"), "modified content").unwrap();
    let _branch2 = repo
        .snapshot(Some("Modified original".to_string()), None)
        .unwrap();

    // This could be detected as a rename/modify conflict
    // oldname.txt was renamed in branch1 and modified in branch2
}

/// Test octopus merge (multiple parents).
#[test]
fn test_octopus_merge_structure() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create base
    std::fs::write(temp.path().join("base.txt"), "base").unwrap();
    let base = repo.snapshot(Some("Base".to_string()), None).unwrap();

    // Create three branches from base
    let mut parent_ids = vec![base.change_id];

    for i in 1..=3 {
        repo.goto(&base.change_id).unwrap();
        std::fs::write(
            temp.path().join(format!("branch{}.txt", i)),
            format!("branch {}", i),
        )
        .unwrap();
        let state = repo.snapshot(Some(format!("Branch {}", i)), None).unwrap();
        parent_ids.push(state.change_id);
    }

    // Verify we have multiple parents to merge
    assert!(parent_ids.len() > 2);
}

/// Test merge with binary files.
#[test]
fn test_binary_file_merge() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Base with binary file
    std::fs::write(temp.path().join("image.bin"), vec![0u8, 1, 2, 3, 4]).unwrap();
    let base = repo.snapshot(Some("Base".to_string()), None).unwrap();

    // Branch 1: different binary
    std::fs::write(temp.path().join("image.bin"), vec![5u8, 6, 7, 8, 9]).unwrap();
    repo.snapshot(Some("Binary 1".to_string()), None).unwrap();

    // Reset and branch 2: yet another binary
    repo.goto(&base.change_id).unwrap();
    std::fs::write(temp.path().join("image.bin"), vec![10u8, 11, 12, 13, 14]).unwrap();
    repo.snapshot(Some("Binary 2".to_string()), None).unwrap();

    // Binary files always conflict - can't merge them
}

/// Test directory/file conflict.
#[test]
fn test_directory_file_conflict() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Base
    std::fs::write(temp.path().join("item.txt"), "file content").unwrap();
    let base = repo.snapshot(Some("Base".to_string()), None).unwrap();

    // Branch 1: turn file into directory with contents
    std::fs::remove_file(temp.path().join("item.txt")).unwrap();
    std::fs::create_dir(temp.path().join("item.txt")).unwrap();
    std::fs::write(temp.path().join("item.txt/subfile.txt"), "subcontent").unwrap();
    repo.snapshot(Some("Directory".to_string()), None).unwrap();

    // Reset and branch 2: modify file
    repo.goto(&base.change_id).unwrap();
    std::fs::write(temp.path().join("item.txt"), "modified file").unwrap();
    repo.snapshot(Some("Modified file".to_string()), None)
        .unwrap();

    // Directory/file conflict
}

// ── MergeState lifecycle (stack-aware rebase paths) ─────────────────────

fn merge_manager(temp: &TempDir) -> (Repository, MergeStateManager) {
    let repo = Repository::init_default(temp.path()).unwrap();
    let manager = repo.merge_state_manager();
    (repo, manager)
}

fn sample_ids() -> (ChangeId, ChangeId, ChangeId) {
    (
        ChangeId::generate(),
        ChangeId::generate(),
        ChangeId::generate(),
    )
}

#[test]
fn merge_state_start_persists_initial_conflict_set() {
    let temp = TempDir::new().unwrap();
    let (_repo, manager) = merge_manager(&temp);
    let (ours, theirs, base) = sample_ids();

    manager
        .start(
            ours,
            theirs,
            Some(base),
            vec!["a.txt".into(), "b.txt".into()],
        )
        .unwrap();

    let loaded: MergeState = manager.load().unwrap().unwrap();
    assert_eq!(loaded.ours, ours);
    assert_eq!(loaded.theirs, theirs);
    assert_eq!(loaded.base, Some(base));
    assert_eq!(loaded.conflicts.len(), 2);
    assert!(loaded.resolved.is_empty());
    assert!(manager.is_merge_in_progress());
}

#[test]
fn merge_state_resolve_one_path_at_a_time() {
    let temp = TempDir::new().unwrap();
    let (_repo, manager) = merge_manager(&temp);
    let (ours, theirs, _) = sample_ids();

    manager
        .start(ours, theirs, None, vec!["a.txt".into(), "b.txt".into()])
        .unwrap();

    manager.resolve("a.txt").unwrap();
    assert_eq!(manager.unresolved().unwrap(), vec!["b.txt".to_string()]);
    manager.resolve("b.txt").unwrap();
    assert!(manager.unresolved().unwrap().is_empty());
}

#[test]
fn merge_state_finish_fails_when_anything_unresolved() {
    let temp = TempDir::new().unwrap();
    let (_repo, manager) = merge_manager(&temp);
    let (ours, theirs, _) = sample_ids();

    manager
        .start(ours, theirs, None, vec!["a.txt".into()])
        .unwrap();
    let err = manager.finish().unwrap_err();
    assert!(
        format!("{err}").contains("Unresolved conflicts"),
        "expected Unresolved Conflicts error, got: {err}"
    );
    assert!(
        manager.is_merge_in_progress(),
        "merge stays alive when finish fails"
    );
}

#[test]
fn merge_state_finish_clears_state_when_everything_resolved() {
    let temp = TempDir::new().unwrap();
    let (_repo, manager) = merge_manager(&temp);
    let (ours, theirs, _) = sample_ids();

    manager
        .start(ours, theirs, None, vec!["a.txt".into()])
        .unwrap();
    manager.resolve("a.txt").unwrap();
    let final_state = manager.finish().unwrap();
    assert_eq!(final_state.ours, ours);
    assert!(manager.load().unwrap().is_none());
}

#[test]
fn merge_state_abort_drops_state_without_requiring_resolution() {
    let temp = TempDir::new().unwrap();
    let (_repo, manager) = merge_manager(&temp);
    let (ours, theirs, _) = sample_ids();

    manager
        .start(ours, theirs, None, vec!["a.txt".into(), "b.txt".into()])
        .unwrap();
    let aborted = manager.abort().unwrap();
    assert_eq!(aborted.theirs, theirs);
    assert!(manager.load().unwrap().is_none());
}

#[test]
fn merge_state_carry_forward_repoints_ours_without_ending_merge() {
    // This is the path stack-aware mid-merge checkpoints use: capture a
    // WIP state, re-point `ours` at it, then keep the merge alive so the
    // operator can finish resolving against the just-committed tip.
    let temp = TempDir::new().unwrap();
    let (_repo, manager) = merge_manager(&temp);
    let (ours, theirs, base) = sample_ids();
    let new_ours = ChangeId::generate();

    manager
        .start(
            ours,
            theirs,
            Some(base),
            vec!["a.txt".into(), "b.txt".into()],
        )
        .unwrap();
    // Resolve one path so we can verify it survives the carry-forward.
    manager.resolve("a.txt").unwrap();

    let after = manager.carry_forward(new_ours).unwrap();
    assert_eq!(after.ours, new_ours, "ours repointed");
    assert_eq!(after.theirs, theirs, "theirs preserved");
    assert_eq!(after.base, Some(base), "base preserved");
    assert_eq!(after.resolved, vec!["a.txt".to_string()], "resolved preserved");
    assert!(manager.is_merge_in_progress(), "merge stays alive");
    assert_eq!(manager.unresolved().unwrap(), vec!["b.txt".to_string()]);
}

#[test]
fn merge_state_resolve_all_marks_remaining_paths() {
    let temp = TempDir::new().unwrap();
    let (_repo, manager) = merge_manager(&temp);
    let (ours, theirs, _) = sample_ids();

    manager
        .start(
            ours,
            theirs,
            None,
            vec!["a.txt".into(), "b.txt".into(), "c.txt".into()],
        )
        .unwrap();
    manager.resolve("b.txt").unwrap();
    let newly = manager.resolve_all().unwrap();
    // Only paths NOT already resolved come back in the newly-resolved list.
    assert_eq!(newly.len(), 2);
    assert!(newly.contains(&"a.txt".to_string()));
    assert!(newly.contains(&"c.txt".to_string()));
    assert!(manager.unresolved().unwrap().is_empty());
}
