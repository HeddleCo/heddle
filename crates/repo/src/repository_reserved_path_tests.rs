// SPDX-License-Identifier: Apache-2.0
//! Checkout refuses tree entries that alias a metadata directory
//! (heddle#2028).
//!
//! A root `.heddle` entry survives tree validation, because a tree does not
//! know whether it is a root, so checkout is where it must be stopped.
//! `.git` aliases cannot be built or decoded at all (see
//! `object::reserved_name` tests); checkout refuses them too.

use std::{fs, path::Path};

use objects::{
    object::{Blob, Tree, TreeEntry},
    store::ObjectStore,
};
use tempfile::TempDir;

use super::repository_worktree_apply::WorktreeApplyDirtyBehavior;
use crate::{HeddleError, Repository};

const ROOT_HEDDLE_ALIASES: [&str; 7] = [
    ".heddle",
    ".HEDDLE",
    ".heddle.",
    ".heddle ",
    "HEDDLE~1",
    ".heddle::$INDEX_ALLOCATION",
    ".hed\u{200c}dle",
];

fn test_repo() -> (TempDir, Repository) {
    let temp_dir = TempDir::new().unwrap();
    let repo = crate::init_test_repository(temp_dir.path()).unwrap();
    (temp_dir, repo)
}

fn put_file(repo: &Repository, name: &str, content: &[u8]) -> TreeEntry {
    let hash = repo.store().put_blob(&Blob::from_slice(content)).unwrap();
    TreeEntry::file(name, hash, false).unwrap()
}

fn put_dir(repo: &Repository, name: &str, entries: Vec<TreeEntry>) -> TreeEntry {
    let hash = repo.store().put_tree(&Tree::from_entries(entries)).unwrap();
    TreeEntry::directory(name, hash).unwrap()
}

/// `README.md` plus `<alias>/config.toml` and `<alias>/hooks/pre-capture`:
/// checked out under the alias `.heddle`, these replace the repository's
/// config and plant an executable hook.
fn hostile_tree(repo: &Repository, alias: &str) -> Tree {
    let hooks = put_dir(
        repo,
        "hooks",
        vec![put_file(repo, "pre-capture", b"#!/bin/sh\necho pwned\n")],
    );
    let metadata = put_dir(
        repo,
        alias,
        vec![put_file(repo, "config.toml", b"pwned = true\n"), hooks],
    );
    Tree::from_entries(vec![put_file(repo, "README.md", b"readme\n"), metadata])
}

fn assert_refused(result: crate::Result<()>, alias: &str) {
    match result {
        Err(HeddleError::ReservedWorktreePath { path, reason }) => {
            assert_eq!(reason.component, alias, "{path:?}");
            assert!(
                reason.to_string().contains(".heddle metadata directory"),
                "{reason}"
            );
        }
        other => panic!("{alias:?}: expected ReservedWorktreePath, got {other:?}"),
    }
}

fn assert_metadata_untouched(root: &Path, config_before: &[u8]) {
    assert_eq!(
        String::from_utf8_lossy(&fs::read(root.join(".heddle/config.toml")).unwrap()),
        String::from_utf8_lossy(config_before),
        "checkout must not rewrite .heddle/config.toml"
    );
    assert!(
        !root.join(".heddle/hooks/pre-capture").exists(),
        "checkout must not plant a hook"
    );
}

#[test]
fn full_checkout_refuses_root_heddle_aliases() {
    for alias in ROOT_HEDDLE_ALIASES {
        let (temp_dir, repo) = test_repo();
        let root = temp_dir.path();
        let config_before = fs::read(root.join(".heddle/config.toml")).unwrap();
        let tree = hostile_tree(&repo, alias);

        let result = repo.materialize_tree(&tree, root);
        assert_metadata_untouched(root, &config_before);
        assert_refused(result, alias);
        assert!(
            !root.join(alias).exists() || alias == ".heddle",
            "{alias:?} must not be created"
        );
    }
}

#[test]
fn incremental_checkout_refuses_root_heddle_aliases() {
    for alias in ROOT_HEDDLE_ALIASES {
        let (temp_dir, repo) = test_repo();
        let root = temp_dir.path();
        let config_before = fs::read(root.join(".heddle/config.toml")).unwrap();
        let from = Tree::from_entries(vec![put_file(&repo, "README.md", b"readme\n")]);
        repo.materialize_tree(&from, root).unwrap();
        let to = hostile_tree(&repo, alias);

        let result = repo
            .plan_worktree_apply(
                Some(&from),
                &to,
                root,
                true,
                WorktreeApplyDirtyBehavior::RefuseOnDirty,
            )
            .and_then(|plan| repo.execute_worktree_apply(&plan, &to, root))
            .map(drop);
        assert_metadata_untouched(root, &config_before);
        assert_refused(result, alias);
    }
}

#[test]
fn nested_heddle_fixtures_and_ordinary_dotfiles_still_check_out() {
    let (temp_dir, repo) = test_repo();
    let root = temp_dir.path();
    let fixture = put_dir(
        &repo,
        ".heddle",
        vec![put_file(&repo, "HEAD", b"fixture\n")],
    );
    let tree = Tree::from_entries(vec![
        put_dir(
            &repo,
            ".github",
            vec![put_file(&repo, "ci.yml", b"on: push\n")],
        ),
        put_file(&repo, ".gitignore", b"target\n"),
        put_file(&repo, ".heddleignore", b"build\n"),
        put_dir(
            &repo,
            "examples",
            vec![put_dir(&repo, "calculator", vec![fixture])],
        ),
    ]);

    repo.materialize_tree(&tree, root).unwrap();

    assert!(root.join(".github/ci.yml").is_file());
    assert!(root.join(".gitignore").is_file());
    assert!(root.join(".heddleignore").is_file());
    assert_eq!(
        fs::read(root.join("examples/calculator/.heddle/HEAD")).unwrap(),
        b"fixture\n"
    );
}

#[test]
fn git_aliases_cannot_reach_checkout() {
    let hash = objects::object::ContentHash::compute(b"hook");
    for alias in [
        ".git",
        ".GIT",
        ".git.",
        ".git ",
        "GIT~1",
        ".git::$INDEX_ALLOCATION",
        ".g\u{200c}it",
    ] {
        assert!(TreeEntry::file(alias, hash, false).is_err(), "{alias:?}");
        assert!(TreeEntry::directory(alias, hash).is_err(), "{alias:?}");
        for path in [alias.to_string(), format!("a/{alias}/hooks/x")] {
            assert!(
                matches!(
                    objects::worktree::check_worktree_write_path(Path::new(&path)),
                    Err(HeddleError::ReservedWorktreePath { .. })
                ),
                "{path:?}"
            );
        }
    }
}
