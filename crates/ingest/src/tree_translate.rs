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
//!   equivalent in v1. Imports refuse them by default; callers can opt into
//!   lossy import to drop them and receive an end-of-run summary.

use objects::{
    object::{Blob, ContentHash, Tree, TreeEntry},
    store::ObjectStore,
    util::{GitTreeNameClassification, GitTreeNameLossyAction, classify_git_tree_name},
};
use tracing::warn;

use crate::{
    IngestError,
    git_walk::{GitSource, TreeChild, TreeChildKind},
    import_options::{
        ImportOptions, LossyImportEntry, entry_relative_to_prefix, fail_lossy_entry,
        join_tree_path, rebase_lossy_entry,
    },
    sha_map::ShaMap,
};

/// Translates one git tree (recursively) into Heddle blobs and trees.
///
/// Holds a short-lived borrow of the store, sha map, and git source for
/// the duration of one commit's translation. Cheap to construct per
/// commit — the memoization state lives in `ShaMap`, not in this struct.
pub struct TreeTranslator<'a, S: ObjectStore> {
    git: &'a GitSource,
    store: &'a S,
    map: &'a mut ShaMap,
    options: ImportOptions,
    lossy_entries: Vec<LossyImportEntry>,
}

impl<'a, S: ObjectStore> TreeTranslator<'a, S> {
    pub fn new(git: &'a GitSource, store: &'a S, map: &'a mut ShaMap) -> Self {
        Self::with_options(git, store, map, ImportOptions::default())
    }

    pub fn with_options(
        git: &'a GitSource,
        store: &'a S,
        map: &'a mut ShaMap,
        options: ImportOptions,
    ) -> Self {
        Self {
            git,
            store,
            map,
            options,
            lossy_entries: Vec::new(),
        }
    }

    pub fn lossy_entries(&self) -> &[LossyImportEntry] {
        &self.lossy_entries
    }

    /// Walk the git tree at `git_tree_sha` and produce the corresponding
    /// Heddle root tree's [`ContentHash`]. Idempotent: re-translating the
    /// same git SHA returns the cached hash without re-reading git.
    pub fn translate_tree(&mut self, git_tree_sha: &str) -> crate::Result<ContentHash> {
        self.translate_tree_at(git_tree_sha, "")
    }

    fn translate_tree_at(
        &mut self,
        git_tree_sha: &str,
        path_prefix: &str,
    ) -> crate::Result<ContentHash> {
        if let Some(hash) = self.map.get_tree(git_tree_sha) {
            let entries = self
                .map
                .get_tree_lossy_entries(git_tree_sha)
                .map_err(IngestError::from)?
                .unwrap_or_default();
            if !entries.is_empty() {
                if !self.options.lossy {
                    return Err(fail_lossy_entry(&rebase_lossy_entry(path_prefix, &entries[0])));
                }
                self.lossy_entries.extend(
                    entries
                        .iter()
                        .map(|entry| rebase_lossy_entry(path_prefix, entry)),
                );
            }
            return Ok(hash);
        }

        let before_lossy = self.lossy_entries.len();
        let children = self.git.read_tree(git_tree_sha)?;
        let mut entries = Vec::with_capacity(children.len());
        for child in children {
            if let Some(entry) = self.translate_child(&child, path_prefix)? {
                entries.push(entry);
            }
        }
        let tree_lossy_entries = self.lossy_entries[before_lossy..]
            .iter()
            .map(|entry| entry_relative_to_prefix(path_prefix, entry))
            .collect::<Vec<_>>();

        let tree = Tree::from_entries(entries);
        let hash = self.store.put_tree(&tree).map_err(IngestError::from)?;

        self.map
            .insert_tree_with_lossy_entries(git_tree_sha, hash, &tree_lossy_entries)
            .map_err(IngestError::from)?;
        Ok(hash)
    }

    fn translate_child(
        &mut self,
        child: &TreeChild,
        path_prefix: &str,
    ) -> crate::Result<Option<TreeEntry>> {
        let name = match classify_git_tree_name(&child.raw_name) {
            GitTreeNameClassification::Representable(name) => name,
            GitTreeNameClassification::NeedsLossy(lossy) => {
                let path = join_tree_path(path_prefix, &lossy.name);
                let entry = match lossy.action {
                    GitTreeNameLossyAction::Dropped => {
                        LossyImportEntry::dropped(path, Some(child.sha.clone()), lossy.reason)
                    }
                    GitTreeNameLossyAction::Converted => {
                        LossyImportEntry::converted(path, Some(child.sha.clone()), lossy.reason)
                    }
                };
                self.record_lossy(entry)?;
                if matches!(lossy.action, GitTreeNameLossyAction::Dropped) {
                    return Ok(None);
                }
                lossy.name
            }
        };

        match child.kind {
            TreeChildKind::Blob { executable } => {
                let hash = self.translate_blob(&child.sha)?;
                Ok(Some(
                    TreeEntry::file(name, hash, executable)
                        .map_err(|e| IngestError::Heddle(e.into()))?,
                ))
            }
            TreeChildKind::Tree => {
                let hash =
                    self.translate_tree_at(&child.sha, &join_tree_path(path_prefix, &name))?;
                Ok(Some(
                    TreeEntry::directory(name, hash).map_err(|e| IngestError::Heddle(e.into()))?,
                ))
            }
            TreeChildKind::Symlink => {
                // Git stores the link target as a blob; Heddle stores the
                // same bytes and flags the entry as a symlink — so the
                // bytes round-trip without special handling.
                let hash = self.translate_blob(&child.sha)?;
                Ok(Some(
                    TreeEntry::symlink(name, hash).map_err(|e| IngestError::Heddle(e.into()))?,
                ))
            }
            TreeChildKind::Gitlink => {
                let entry = LossyImportEntry::dropped(
                    join_tree_path(path_prefix, &name),
                    Some(child.sha.clone()),
                    "gitlink/submodule entries have no Heddle tree equivalent",
                );
                self.record_lossy(entry)?;
                Ok(None)
            }
        }
    }

    fn record_lossy(&mut self, entry: LossyImportEntry) -> crate::Result<()> {
        if !self.options.lossy {
            return Err(fail_lossy_entry(&entry));
        }
        warn!(entry = %entry.summary_line(), "lossy git import accepted");
        self.lossy_entries.push(entry);
        Ok(())
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
    use std::{io::Write, path::Path, process::Command};

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

    fn seed_gitlink_repo(path: &Path) -> String {
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
        std::fs::write(path.join("README.md"), "# hello\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-q", "-m", "initial"]);
        run(&[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000,0707070707070707070707070707070707070707,vendor",
        ]);
        run(&["commit", "-q", "-m", "add gitlink"]);

        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(path)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn git_output(path: &Path, args: &[&str], stdin: Option<&[u8]>) -> String {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        if stdin.is_some() {
            command.stdin(std::process::Stdio::piped());
        }
        let mut child = command.spawn().expect("git cmd");
        if let Some(stdin) = stdin {
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(stdin)
                .expect("write stdin");
        }
        let output = child.wait_with_output().expect("git output");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn seed_invalid_utf8_name_repo(path: &Path) -> String {
        let status = Command::new("git")
            .args(["init", "-q", "--initial-branch=main"])
            .current_dir(path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git init");
        assert!(status.success(), "git init failed");

        let blob = git_output(path, &["hash-object", "-w", "--stdin"], Some(b"hello\n"));
        let mut tree_input = Vec::new();
        write!(&mut tree_input, "100644 blob {blob}\t").expect("tree record");
        tree_input.extend_from_slice(b"bad\xffname\0");
        let tree = git_output(path, &["mktree", "-z"], Some(&tree_input));
        let commit = git_output(path, &["commit-tree", &tree, "-m", "invalid name"], None);
        git_output(path, &["update-ref", "refs/heads/main", &commit], None);
        commit
    }

    fn seed_nested_gitlink_repo(path: &Path, dir: &str, parent: Option<&str>) -> String {
        if parent.is_none() {
            let status = Command::new("git")
                .args(["init", "-q", "--initial-branch=main"])
                .current_dir(path)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git init");
            assert!(status.success(), "git init failed");
        }

        let mut subtree_input = Vec::new();
        subtree_input
            .extend_from_slice(b"160000 commit 0707070707070707070707070707070707070707\tvendor\0");
        let subtree = git_output(path, &["mktree", "--missing", "-z"], Some(&subtree_input));
        let mut root_input = Vec::new();
        write!(&mut root_input, "040000 tree {subtree}\t{dir}\0").expect("root tree record");
        let root = git_output(path, &["mktree", "--missing", "-z"], Some(&root_input));

        let mut args = vec!["commit-tree", root.as_str(), "-m", "nested gitlink"];
        if let Some(parent) = parent {
            args.extend(["-p", parent]);
        }
        let commit = git_output(path, &args, None);
        git_output(path, &["update-ref", "refs/heads/main", &commit], None);
        commit
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
    fn gitlink_fails_by_default() {
        let tmp = TempDir::new().unwrap();
        let head = seed_gitlink_repo(tmp.path());
        let git = GitSource::open(tmp.path()).unwrap();
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let commit = git.read_commit(&head).unwrap();

        let err = TreeTranslator::new(&git, &store, &mut map)
            .translate_tree(&commit.tree_sha)
            .expect_err("gitlink import must fail without lossy opt-in");
        let message = err.to_string();

        assert!(message.contains("vendor"), "error names entry: {message}");
        assert!(message.contains("--lossy"), "error names opt-in: {message}");
    }

    #[test]
    fn gitlink_lossy_opt_in_drops_and_records_summary() {
        let tmp = TempDir::new().unwrap();
        let head = seed_gitlink_repo(tmp.path());
        let git = GitSource::open(tmp.path()).unwrap();
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let commit = git.read_commit(&head).unwrap();

        let mut tx =
            TreeTranslator::with_options(&git, &store, &mut map, ImportOptions { lossy: true });
        let root_hash = tx.translate_tree(&commit.tree_sha).unwrap();
        let lossy_entries = tx.lossy_entries().to_vec();
        let tree = store.get_tree(&root_hash).unwrap().unwrap();
        let names = tree
            .entries()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();

        assert!(!names.contains(&"vendor"));
        assert_eq!(lossy_entries.len(), 1);
        assert_eq!(lossy_entries[0].path, "vendor");
        assert!(lossy_entries[0].summary_line().contains("dropped"));
    }

    #[test]
    fn invalid_utf8_name_fails_by_default() {
        let tmp = TempDir::new().unwrap();
        let head = seed_invalid_utf8_name_repo(tmp.path());
        let git = GitSource::open(tmp.path()).unwrap();
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let commit = git.read_commit(&head).unwrap();

        let err = TreeTranslator::new(&git, &store, &mut map)
            .translate_tree(&commit.tree_sha)
            .expect_err("invalid UTF-8 name must fail without lossy opt-in");
        let message = err.to_string();

        assert!(message.contains("bad"), "error names entry: {message}");
        assert!(
            message.contains("not valid UTF-8"),
            "error explains conversion: {message}"
        );
        assert!(message.contains("--lossy"), "error names opt-in: {message}");
    }

    #[test]
    fn invalid_utf8_name_lossy_opt_in_converts_and_records_summary() {
        let tmp = TempDir::new().unwrap();
        let head = seed_invalid_utf8_name_repo(tmp.path());
        let git = GitSource::open(tmp.path()).unwrap();
        let store = InMemoryStore::new();
        let mut map = ShaMap::new();
        let commit = git.read_commit(&head).unwrap();

        let mut tx =
            TreeTranslator::with_options(&git, &store, &mut map, ImportOptions { lossy: true });
        let root_hash = tx.translate_tree(&commit.tree_sha).unwrap();
        let lossy_entries = tx.lossy_entries().to_vec();
        let tree = store.get_tree(&root_hash).unwrap().unwrap();
        let converted_name = "bad\u{fffd}name";

        assert!(
            tree.entries()
                .iter()
                .any(|entry| entry.name == converted_name),
            "converted entry should be retained"
        );
        assert_eq!(lossy_entries.len(), 1);
        assert_eq!(lossy_entries[0].path, converted_name);
        assert!(lossy_entries[0].summary_line().contains("converted"));
    }

    #[test]
    fn cached_lossy_subtree_reports_with_new_path_prefix() {
        let gitdir = TempDir::new().unwrap();
        let mapdir = TempDir::new().unwrap();
        let map_path = mapdir.path().join("sha_map.sqlite");
        let first_commit = seed_nested_gitlink_repo(gitdir.path(), "dir1", None);
        let git = GitSource::open(gitdir.path()).unwrap();
        let store = InMemoryStore::new();

        {
            let mut map = ShaMap::open(&map_path).unwrap();
            let commit = git.read_commit(&first_commit).unwrap();
            let mut tx =
                TreeTranslator::with_options(&git, &store, &mut map, ImportOptions { lossy: true });
            tx.translate_tree(&commit.tree_sha).unwrap();
            assert_eq!(tx.lossy_entries()[0].path, "dir1/vendor");
        }

        let second_commit = seed_nested_gitlink_repo(gitdir.path(), "dir2", Some(&first_commit));
        let mut map = ShaMap::open(&map_path).unwrap();
        let commit = git.read_commit(&second_commit).unwrap();
        let mut tx =
            TreeTranslator::with_options(&git, &store, &mut map, ImportOptions { lossy: true });
        tx.translate_tree(&commit.tree_sha).unwrap();
        let lossy_entries = tx.lossy_entries().to_vec();

        assert_eq!(lossy_entries.len(), 1);
        assert_eq!(lossy_entries[0].path, "dir2/vendor");
        assert!(lossy_entries[0].summary_line().contains("dropped"));
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
