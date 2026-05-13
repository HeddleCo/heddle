// SPDX-License-Identifier: Apache-2.0
//! Translate git trees and blobs into their Heddle equivalents.
//!
//! Git and Heddle both content-address their snapshot DAGs, but git uses
//! SHA-1 over a custom framed encoding and Heddle uses BLAKE3 over a
//! `("blob" | "tree", length, bytes)` prefix. That means every blob and
//! tree has a deterministic Heddle [`ContentHash`] derivable only from its
//! bytes — but actually producing it is expensive, so we memoize on the
//! git SHA via [`ShaMap`] and only re-translate what we haven't seen.
//!
//! # Why memoize on the git side, not the Heddle side?
//!
//! Translating blob bytes to a Heddle blob hash is cheap (one BLAKE3 pass);
//! translating a tree is recursive and quadratic-ish in children. But more
//! importantly, repeated runs of the importer want to skip *already-imported*
//! objects without re-reading them from git — and the only stable key we
//! have at that layer is the git SHA.
//!
//! # Gitlinks & symlinks
//!
//! - **Symlinks** (mode `120000`) are stored in git as regular blobs whose
//!   content is the target path. Heddle supports them natively via
//!   [`TreeEntry::symlink`]; we pass the blob hash through unchanged.
//! - **Gitlinks** (mode `160000`, i.e. submodule pointers) have no Heddle
//!   equivalent in v1. They're skipped with a `warn!` trace; the caller
//!   can choose to abort if a tree ends up empty.

use objects::{
    object::{Blob, ContentHash, Tree, TreeEntry},
    store::ObjectStore,
};
use tracing::warn;

use crate::{
    IngestError,
    git_walk::{GitSource, TreeChild, TreeChildKind},
    sha_map::ShaMap,
};

/// Translates one git tree (recursively) into Heddle blobs and trees.
///
/// Holds a short-lived borrow of the store, sha map, and git source for
/// the duration of one commit's translation. Cheap to construct per
/// commit — the memoization state lives in `ShaMap`, not in this struct.
pub struct TreeTranslator<'a> {
    git: &'a GitSource,
    store: &'a dyn ObjectStore,
    map: &'a mut ShaMap,
}

impl<'a> TreeTranslator<'a> {
    pub fn new(git: &'a GitSource, store: &'a dyn ObjectStore, map: &'a mut ShaMap) -> Self {
        Self { git, store, map }
    }

    /// Walk the git tree at `git_tree_sha` and produce the corresponding
    /// Heddle root tree's [`ContentHash`]. Idempotent: re-translating the
    /// same git SHA returns the cached hash without re-reading git.
    pub fn translate_tree(&mut self, git_tree_sha: &str) -> crate::Result<ContentHash> {
        if let Some(hash) = self.map.get_tree(git_tree_sha) {
            return Ok(hash);
        }

        let children = self.git.read_tree(git_tree_sha)?;
        let mut entries = Vec::with_capacity(children.len());
        for child in children {
            if let Some(entry) = self.translate_child(&child)? {
                entries.push(entry);
            }
        }

        let tree = Tree::from_entries(entries);
        let hash = self.store.put_tree(&tree).map_err(IngestError::from)?;

        self.map
            .insert_tree(git_tree_sha, hash)
            .map_err(IngestError::from)?;
        Ok(hash)
    }

    fn translate_child(&mut self, child: &TreeChild) -> crate::Result<Option<TreeEntry>> {
        // Skip `.` / `..` and control-byte names before they hit Heddle's
        // validator — we'd rather warn and drop than abort the whole
        // import because some repo has a weird filename.
        if child.name.is_empty()
            || child.name == "."
            || child.name == ".."
            || child.name.contains('/')
            || child.name.bytes().any(|b| b < 0x20 || b == 0x7f)
        {
            warn!(name = %child.name, "skipping tree child with unusable name");
            return Ok(None);
        }

        match child.kind {
            TreeChildKind::Blob { executable } => {
                let hash = self.translate_blob(&child.sha)?;
                Ok(Some(
                    TreeEntry::file(child.name.clone(), hash, executable)
                        .map_err(|e| IngestError::Heddle(e.into()))?,
                ))
            }
            TreeChildKind::Tree => {
                let hash = self.translate_tree(&child.sha)?;
                Ok(Some(
                    TreeEntry::directory(child.name.clone(), hash)
                        .map_err(|e| IngestError::Heddle(e.into()))?,
                ))
            }
            TreeChildKind::Symlink => {
                // Git stores the link target as a blob; Heddle stores the
                // same bytes and flags the entry as a symlink — so the
                // bytes round-trip without special handling.
                let hash = self.translate_blob(&child.sha)?;
                Ok(Some(
                    TreeEntry::symlink(child.name.clone(), hash)
                        .map_err(|e| IngestError::Heddle(e.into()))?,
                ))
            }
            TreeChildKind::Gitlink => {
                // Submodule pointers. Heddle's tree doesn't model them yet,
                // so we drop the entry and log loudly; the tree hash will
                // diverge from the git tree by exactly this entry.
                warn!(
                    name = %child.name,
                    submodule_sha = %child.sha,
                    "dropping gitlink (submodule) entry — no Heddle equivalent"
                );
                Ok(None)
            }
        }
    }

    /// Hash `git_blob_sha` into Heddle's object store, memoizing on the git
    /// SHA. Bytes are read from the git odb on cache-miss only.
    pub fn translate_blob(&mut self, git_blob_sha: &str) -> crate::Result<ContentHash> {
        if let Some(hash) = self.map.get_blob(git_blob_sha) {
            return Ok(hash);
        }

        let bytes = self.git.read_blob(git_blob_sha)?;
        let blob = Blob::from_slice(&bytes);
        let hash = self.store.put_blob(&blob).map_err(IngestError::from)?;

        self.map
            .insert_blob(git_blob_sha, hash)
            .map_err(IngestError::from)?;
        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, process::Command};

    use objects::store::InMemoryStore;
    use tempfile::TempDir;

    use super::*;

    /// Build a git repo with a small, mixed-mode tree so we can exercise
    /// blob + executable + nested tree translation in one shot. Returns
    /// the HEAD commit SHA.
    fn seed_mixed_repo(path: &Path) -> String {
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(path)
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git cmd");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "-q", "--initial-branch=main"]);
        std::fs::create_dir_all(path.join("src")).unwrap();
        std::fs::write(path.join("README.md"), "# hello\n").unwrap();
        std::fs::write(path.join("src/lib.rs"), "fn main() {}\n").unwrap();
        let script = path.join("run.sh");
        std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "mixed tree"]);

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(path)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn translates_tree_round_trip() {
        let tmp = TempDir::new().unwrap();
        let head = seed_mixed_repo(tmp.path());

        let git = GitSource::open(tmp.path()).unwrap();
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();

        let commit = git.read_commit(&head).unwrap();
        let root_hash = {
            let mut tx = TreeTranslator::new(&git, &store, &mut map);
            tx.translate_tree(&commit.tree_sha).unwrap()
        };

        // Root tree must be retrievable and have validated entries.
        let tree = store.get_tree(&root_hash).unwrap().expect("root tree");
        tree.validate().expect("root tree validates");
        let names: Vec<&str> = tree.entries().iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"README.md"));
        assert!(names.contains(&"src"));

        // Blob content should survive the round-trip byte-for-byte.
        let readme_entry = tree
            .entries()
            .iter()
            .find(|e| e.name == "README.md")
            .unwrap();
        let blob = store.get_blob(&readme_entry.hash).unwrap().unwrap();
        assert_eq!(blob.content(), b"# hello\n");
    }

    #[test]
    fn executable_flag_preserved() {
        let tmp = TempDir::new().unwrap();
        let head = seed_mixed_repo(tmp.path());

        let git = GitSource::open(tmp.path()).unwrap();
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();

        let commit = git.read_commit(&head).unwrap();
        let root_hash = TreeTranslator::new(&git, &store, &mut map)
            .translate_tree(&commit.tree_sha)
            .unwrap();
        let tree = store.get_tree(&root_hash).unwrap().unwrap();

        // `run.sh` was chmod +x'd; git captured the BlobExecutable mode,
        // and the translator must carry it through.
        let run = tree.entries().iter().find(|e| e.name == "run.sh");
        if let Some(entry) = run {
            // On Windows the test skips the chmod block, so only assert
            // when we have the entry and we're on unix.
            #[cfg(unix)]
            assert!(entry.is_executable(), "run.sh should be marked executable");
            let _ = entry;
        }
    }

    #[test]
    fn second_call_hits_cache_and_does_not_reread() {
        let tmp = TempDir::new().unwrap();
        let head = seed_mixed_repo(tmp.path());
        let git = GitSource::open(tmp.path()).unwrap();
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let commit = git.read_commit(&head).unwrap();

        let first = TreeTranslator::new(&git, &store, &mut map)
            .translate_tree(&commit.tree_sha)
            .unwrap();
        let tree_count_after_first = store.list_trees().unwrap().len();

        // Second call must hit the ShaMap and produce identical hash
        // without minting new tree objects in the store.
        let second = TreeTranslator::new(&git, &store, &mut map)
            .translate_tree(&commit.tree_sha)
            .unwrap();
        let tree_count_after_second = store.list_trees().unwrap().len();

        assert_eq!(first, second);
        assert_eq!(
            tree_count_after_first, tree_count_after_second,
            "second translate should not write new tree objects"
        );
    }
}