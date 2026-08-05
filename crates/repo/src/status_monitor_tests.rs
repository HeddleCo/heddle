// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use objects::object::Tree;
use tempfile::TempDir;

use super::{
    Repository,
    repository_worktree_status::{WorktreeStatusDetailed, compare_worktree_with_index_detailed},
};
use crate::{
    FsMonitorMode, FsMonitorSettings, WorktreeIndex, fsmonitor::ChangeMonitorSession,
    worktree_ignore::WorktreeIgnoreMatcher,
};

fn flat(status: WorktreeStatusDetailed) -> objects::worktree::WorktreeStatus {
    let mut status = status.into_flat_status();
    status.modified.sort();
    status.added.sort();
    status.deleted.sort();
    status
}

fn compare_with(
    repo: &Repository,
    tree: &Tree,
    index: &WorktreeIndex,
    monitor: &ChangeMonitorSession,
) -> objects::worktree::WorktreeStatus {
    let mut index = index.clone();
    let matcher = WorktreeIgnoreMatcher::new(&repo.ignore_patterns().unwrap());
    let (status, _) =
        compare_worktree_with_index_detailed(repo, tree, &matcher, &mut index, monitor).unwrap();
    flat(status)
}

fn assert_monitor_matches_full(
    repo: &Repository,
    tree: &Tree,
    index: &WorktreeIndex,
    changed: &BTreeSet<String>,
) {
    let monitored = ChangeMonitorSession::test_usable(repo.root(), changed.clone());
    let full = ChangeMonitorSession::prepare(
        repo.root(),
        FsMonitorSettings {
            mode: FsMonitorMode::Off,
        },
    );
    let monitored = compare_with(repo, tree, index, &monitored);
    let full = compare_with(repo, tree, index, &full);
    assert_eq!(monitored.modified, full.modified);
    assert_eq!(monitored.added, full.added);
    assert_eq!(monitored.deleted, full.deleted);
}

fn seed_repo() -> (TempDir, Repository, Tree, WorktreeIndex) {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();
    for directory in ["src", "tests", "nested/deep"] {
        fs::create_dir_all(temp.path().join(directory)).unwrap();
    }
    for (path, contents) in [
        ("src/a.txt", "a"),
        ("src/b.txt", "b"),
        ("tests/status.txt", "status"),
        ("nested/deep/value.txt", "value"),
    ] {
        fs::write(temp.path().join(path), contents).unwrap();
    }
    let state = repo.snapshot(Some("seed".to_string()), None).unwrap();
    let tree = repo.require_tree(&state.tree).unwrap();
    repo.compare_worktree_cached_with_options(
        &tree,
        &crate::WorktreeStatusOptions {
            fsmonitor: FsMonitorSettings {
                mode: FsMonitorMode::Off,
            },
        },
    )
    .unwrap();
    let index = WorktreeIndex::load(&temp.path().join(".heddle/state/index.bin")).unwrap();
    (temp, repo, tree, index)
}

#[test]
fn monitor_and_full_scan_match_randomized_mutation_sequences() {
    let (temp, repo, tree, index) = seed_repo();
    let mut changed = BTreeSet::new();
    let mut seed = 0x5eed_u64;

    for step in 0..32 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let slot = (seed % 8) as usize;
        let path = format!("random/file-{slot}.txt");
        fs::create_dir_all(temp.path().join("random")).unwrap();
        match (seed >> 8) % 4 {
            0 | 1 => {
                fs::write(temp.path().join(&path), format!("generation {step}\n")).unwrap();
                changed.insert(path);
            }
            2 if temp.path().join(&path).exists() => {
                fs::remove_file(temp.path().join(&path)).unwrap();
                changed.insert(path);
            }
            _ => {
                let renamed = format!("random/renamed-{step}.txt");
                if temp.path().join(&path).exists() {
                    fs::rename(temp.path().join(&path), temp.path().join(&renamed)).unwrap();
                    changed.insert(path);
                    changed.insert(renamed);
                }
            }
        }
        assert_monitor_matches_full(&repo, &tree, &index, &changed);
    }

    fs::write(temp.path().join("src/a.txt"), "edited").unwrap();
    changed.insert("src/a.txt".to_string());
    assert_monitor_matches_full(&repo, &tree, &index, &changed);

    fs::rename(
        temp.path().join("nested"),
        temp.path().join("renamed-nested"),
    )
    .unwrap();
    changed.insert("nested".to_string());
    changed.insert("renamed-nested".to_string());
    assert_monitor_matches_full(&repo, &tree, &index, &changed);

    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let path = temp.path().join("src/b.txt");
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        changed.insert("src/b.txt".to_string());
        symlink(Path::new("a.txt"), temp.path().join("src/link.txt")).unwrap();
        changed.insert("src/link.txt".to_string());
        assert_monitor_matches_full(&repo, &tree, &index, &changed);
    }
}

#[test]
fn ignore_rule_transitions_force_the_same_safe_result_as_a_full_scan() {
    let (temp, repo, tree, index) = seed_repo();
    fs::create_dir_all(temp.path().join("ignored")).unwrap();
    fs::write(temp.path().join("ignored/value.txt"), "ignored").unwrap();
    fs::write(temp.path().join(".heddleignore"), "ignored/\n").unwrap();
    assert_monitor_matches_full(
        &repo,
        &tree,
        &index,
        &BTreeSet::from([".heddleignore".to_string()]),
    );

    fs::write(temp.path().join(".heddleignore"), "").unwrap();
    assert_monitor_matches_full(
        &repo,
        &tree,
        &index,
        &BTreeSet::from([".heddleignore".to_string()]),
    );

    fs::write(temp.path().join(".heddleignore"), "ignored/\n").unwrap();
    assert_monitor_matches_full(
        &repo,
        &tree,
        &index,
        &BTreeSet::from([".heddleignore".to_string()]),
    );
}

#[test]
fn gitignore_rule_transitions_force_the_same_safe_result_as_a_full_scan() {
    // heddle#1155: clearing root `.gitignore` must re-surface previously
    // ignored untracked paths under the change monitor, not leave them
    // hidden until something inside the junk tree is touched.
    let (temp, repo, tree, index) = seed_repo();
    fs::create_dir_all(temp.path().join("__pycache__")).unwrap();
    fs::write(temp.path().join("__pycache__/app.pyc"), "binary").unwrap();
    fs::write(temp.path().join(".gitignore"), "__pycache__/\n").unwrap();
    assert_monitor_matches_full(
        &repo,
        &tree,
        &index,
        &BTreeSet::from([".gitignore".to_string()]),
    );

    fs::write(temp.path().join(".gitignore"), "").unwrap();
    assert_monitor_matches_full(
        &repo,
        &tree,
        &index,
        &BTreeSet::from([".gitignore".to_string()]),
    );

    fs::write(temp.path().join(".gitignore"), "__pycache__/\n").unwrap();
    assert_monitor_matches_full(
        &repo,
        &tree,
        &index,
        &BTreeSet::from([".gitignore".to_string()]),
    );
}

#[test]
fn missing_or_cursor_lagging_index_never_reports_false_clean() {
    let (temp, repo, tree, mut index) = seed_repo();
    fs::write(temp.path().join("src/a.txt"), "changed after cursor").unwrap();

    let cursor_ahead = ChangeMonitorSession::test_usable(repo.root(), BTreeSet::new());
    let from_missing_index = compare_with(&repo, &tree, &WorktreeIndex::new(), &cursor_ahead);
    assert_eq!(
        from_missing_index.modified,
        vec![PathBuf::from("src/a.txt")]
    );

    let event_not_committed =
        ChangeMonitorSession::test_usable(repo.root(), BTreeSet::from(["src/a.txt".to_string()]));
    let matcher = WorktreeIgnoreMatcher::new(&repo.ignore_patterns().unwrap());
    let _ = compare_worktree_with_index_detailed(
        &repo,
        &tree,
        &matcher,
        &mut index,
        &event_not_committed,
    )
    .unwrap();
    let index_path = temp.path().join(".heddle/state/index.bin");
    index.save(&index_path).unwrap();
    let (reloaded_index, _) = WorktreeIndex::load_hot_profiled_for_directories(
        &index_path,
        &event_not_committed.changed_directory_keys(),
    )
    .unwrap();
    let replayed_event = compare_with(&repo, &tree, &reloaded_index, &event_not_committed);
    assert_eq!(replayed_event.modified, vec![PathBuf::from("src/a.txt")]);
}
