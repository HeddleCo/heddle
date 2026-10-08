// SPDX-License-Identifier: Apache-2.0
//! Checkout never writes or removes a path through a metadata name
//! (heddle#2028): a `.git` alias at any depth, a `.heddle` alias at the root,
//! or a `.gitmodules` symlink.
//!
//! Stored trees can hold such names: repositories captured before
//! heddle#2028 recorded nested `.git` directories, and decoding stays
//! permissive so they remain readable. Checkout skips those paths with a
//! warning instead of failing, so such a state still checks out and can be
//! left.

use std::{fs, path::Path};

use objects::{
    object::{Blob, Tree, TreeEntry},
    store::ObjectStore,
};
use tempfile::TempDir;

use super::repository_worktree_apply::WorktreeApplyDirtyBehavior;
use crate::{HeddleError, Repository};

const ROOT_HEDDLE_ALIASES: [&str; 8] = [
    ".heddle",
    ".HEDDLE",
    ".heddle.",
    ".heddle ",
    "HEDDLE~1",
    "heddle~4",
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

fn put_symlink(repo: &Repository, name: &str, target: &[u8]) -> TreeEntry {
    let hash = repo.store().put_blob(&Blob::from_slice(target)).unwrap();
    TreeEntry::symlink(name, hash).unwrap()
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

fn apply(repo: &Repository, root: &Path, from: &Tree, to: &Tree) -> crate::Result<()> {
    let plan = repo.plan_worktree_apply(
        Some(from),
        to,
        root,
        true,
        WorktreeApplyDirtyBehavior::RefuseOnDirty,
    )?;
    repo.execute_worktree_apply(&plan, to, root).map(drop)
}

#[test]
fn full_checkout_skips_root_heddle_aliases() {
    for alias in ROOT_HEDDLE_ALIASES {
        let (temp_dir, repo) = test_repo();
        let root = temp_dir.path();
        let config_before = fs::read(root.join(".heddle/config.toml")).unwrap();
        let tree = hostile_tree(&repo, alias);

        let result = repo.materialize_tree(&tree, root);
        assert_metadata_untouched(root, &config_before);
        result.unwrap_or_else(|error| panic!("{alias:?}: checkout failed: {error}"));
        assert!(root.join("README.md").is_file(), "{alias:?}");
        assert!(
            alias == ".heddle" || !root.join(alias).exists(),
            "{alias:?} must not be created"
        );
    }
}

#[test]
fn incremental_checkout_skips_root_heddle_aliases() {
    for alias in ROOT_HEDDLE_ALIASES {
        let (temp_dir, repo) = test_repo();
        let root = temp_dir.path();
        let config_before = fs::read(root.join(".heddle/config.toml")).unwrap();
        let from = Tree::from_entries(vec![put_file(&repo, "base.txt", b"base\n")]);
        repo.materialize_tree(&from, root).unwrap();
        let to = hostile_tree(&repo, alias);

        let result = apply(&repo, root, &from, &to);
        assert_metadata_untouched(root, &config_before);
        result.unwrap_or_else(|error| panic!("{alias:?}: checkout failed: {error}"));
        assert!(root.join("README.md").is_file(), "{alias:?}");

        // Leaving the state must not remove the live `.heddle` either.
        apply(&repo, root, &to, &from).unwrap();
        assert_metadata_untouched(root, &config_before);
        assert!(!root.join("README.md").exists(), "{alias:?}");
    }
}

/// A state captured before heddle#2028 with a vendored clone in it: the
/// nested `.git` was recorded as ordinary content.
fn tree_with_nested_git(repo: &Repository, source: &[u8]) -> Tree {
    let git = put_dir(
        repo,
        ".git",
        vec![
            put_file(repo, "config", b"[core]\n\thooksPath = /tmp/pwn\n"),
            put_dir(
                repo,
                "hooks",
                vec![put_file(repo, "post-checkout", b"#!/bin/sh\necho pwned\n")],
            ),
        ],
    );
    let lib = put_dir(repo, "lib", vec![git, put_file(repo, "src.rs", source)]);
    Tree::from_entries(vec![
        put_file(repo, "README.md", b"readme\n"),
        put_dir(repo, "vendor", vec![lib]),
    ])
}

#[test]
fn a_captured_nested_git_decodes_checks_out_and_can_be_left() {
    let (temp_dir, repo) = test_repo();
    let root = temp_dir.path();
    let base = Tree::from_entries(vec![put_file(&repo, "README.md", b"readme\n")]);
    repo.materialize_tree(&base, root).unwrap();
    // The user's own clone, which owns `vendor/lib/.git`.
    fs::create_dir_all(root.join("vendor/lib/.git")).unwrap();
    fs::write(root.join("vendor/lib/.git/config"), "real\n").unwrap();

    let captured = tree_with_nested_git(&repo, b"fn v1() {}\n");
    let hash = repo.store().put_tree(&captured).unwrap();
    let decoded = repo.store().get_tree(&hash).unwrap().expect("stored tree");
    assert_eq!(decoded, captured, "a stored nested .git still decodes");

    // Moving onto it writes the content but not the nested metadata.
    apply(&repo, root, &base, &captured).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("vendor/lib/src.rs")).unwrap(),
        "fn v1() {}\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("vendor/lib/.git/config")).unwrap(),
        "real\n"
    );
    assert!(!root.join("vendor/lib/.git/hooks/post-checkout").exists());
    let status = repo.compare_worktree_cached_detailed(&captured).unwrap();
    assert!(
        status.is_clean(),
        "a skipped nested .git is not a deletion: {status:?}"
    );

    // Updating within it, and a full rematerialize, behave the same.
    let updated = tree_with_nested_git(&repo, b"fn v2() {}\n");
    apply(&repo, root, &captured, &updated).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("vendor/lib/src.rs")).unwrap(),
        "fn v2() {}\n"
    );
    repo.materialize_tree(&updated, root).unwrap();
    assert_eq!(
        fs::read_to_string(root.join("vendor/lib/.git/config")).unwrap(),
        "real\n"
    );

    // Leaving it removes the tracked content and keeps the clone's metadata.
    apply(&repo, root, &updated, &base).unwrap();
    assert!(!root.join("vendor/lib/src.rs").exists());
    assert_eq!(
        fs::read_to_string(root.join("vendor/lib/.git/config")).unwrap(),
        "real\n",
        "removal must not reach into a nested repository's metadata"
    );
}

#[test]
fn gitmodules_symlinks_are_not_checked_out() {
    let (temp_dir, repo) = test_repo();
    let root = temp_dir.path();
    let tree = Tree::from_entries(vec![
        put_symlink(&repo, ".gitmodules", b"/etc/passwd"),
        put_dir(
            &repo,
            "sub",
            vec![put_symlink(&repo, "GITMOD~1", b"../../outside")],
        ),
        put_file(&repo, "README.md", b"readme\n"),
    ]);
    repo.materialize_tree(&tree, root).unwrap();
    assert!(fs::symlink_metadata(root.join(".gitmodules")).is_err());
    assert!(fs::symlink_metadata(root.join("sub/GITMOD~1")).is_err());
    assert!(root.join("README.md").is_file());

    // A regular `.gitmodules` file is ordinary content.
    let regular = Tree::from_entries(vec![put_file(&repo, ".gitmodules", b"[submodule]\n")]);
    repo.materialize_tree(&regular, root).unwrap();
    assert!(root.join(".gitmodules").is_file());
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
fn single_path_writes_refuse_reserved_paths() {
    for path in [
        ".heddle/config.toml",
        ".HEDDLE/hooks/pre-capture",
        "a/.git/hooks/x",
        "a/GIT~1/config",
    ] {
        assert!(
            matches!(
                objects::worktree::check_worktree_write_path(Path::new(path), false),
                Err(HeddleError::ReservedWorktreePath { .. })
            ),
            "{path:?}"
        );
    }
    assert!(
        objects::worktree::check_worktree_write_path(Path::new("sub/.gitmodules"), true).is_err()
    );
}
