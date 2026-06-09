// SPDX-License-Identifier: Apache-2.0
//! Index and checkout helpers via sley `git-worktree` (#597 P2).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use sley_core::{BString, ObjectFormat, ObjectId};
use sley_index::{Index, IndexEntry};
use sley_object::{Commit, ObjectType, Tree};
use sley_odb::{repository_common_dir, FileObjectDatabase, ObjectReader};
use sley_worktree::{read_repository_index, repository_index_path};

use crate::id::{empty_blob_sha1, empty_tree_sha1};
use crate::index::{INDEX_FLAG_EXTENDED, INDEX_FLAG_INTENT_TO_ADD};
use crate::{GitSubstrateError, Result};

const INDEX_NAME_MASK: u16 = 0x0FFF;

/// Path to the repository index (honours `GIT_INDEX_FILE`).
pub fn index_path(git_dir: &Path) -> PathBuf {
    repository_index_path(git_dir)
}

/// Whether `index.lock` exists under `git_dir`.
pub fn index_lock_exists(git_dir: &Path) -> bool {
    index_path(git_dir).with_extension("lock").exists()
}

/// Rebuild the index from `commit_oid`'s tree without touching worktree files.
pub fn write_index_from_commit(
    _worktree_root: &Path,
    git_dir: &Path,
    format: ObjectFormat,
    commit_oid: &ObjectId,
) -> Result<()> {
    let common_dir = repository_common_dir(git_dir);
    let db = FileObjectDatabase::from_git_dir(&common_dir, format);
    let object = db.read_object(commit_oid).map_err(GitSubstrateError::from)?;
    let commit = Commit::parse(format, &object.body).map_err(GitSubstrateError::from)?;
    write_index_from_tree(git_dir, format, &commit.tree)
}

/// Build an in-memory index from `tree_oid` (index-only; no worktree writes).
pub fn read_index_from_tree(
    git_dir: &Path,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<Index> {
    index_from_tree(git_dir, format, tree_oid, 2)
}

/// Rebuild the on-disk index from `tree_oid` without touching worktree files.
pub fn write_index_from_tree(
    git_dir: &Path,
    format: ObjectFormat,
    tree_oid: &ObjectId,
) -> Result<()> {
    let index = read_index_from_tree(git_dir, format, tree_oid)?;
    write_index(git_dir, format, &index)
}

/// File mode for an intent-to-add placeholder entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentToAddMode {
    Normal,
    Executable,
    Symlink,
}

impl IntentToAddMode {
    fn git_mode(self) -> u32 {
        match self {
            Self::Normal => 0o100644,
            Self::Executable => 0o100755,
            Self::Symlink => 0o120000,
        }
    }
}

/// Reconcile intent-to-add placeholders against `captured` paths.
///
/// Returns `true` when the index was modified.
pub fn reconcile_intent_to_add(
    git_dir: &Path,
    format: ObjectFormat,
    captured: &[(String, IntentToAddMode)],
) -> Result<bool> {
    let mut index = read_repository_index(git_dir, format)
        .map_err(GitSubstrateError::from)?
        .unwrap_or_else(|| empty_index(2));

    let mut real_tracked: HashSet<String> = HashSet::new();
    let mut existing_ita: HashSet<String> = HashSet::new();
    for entry in &index.entries {
        let path = String::from_utf8_lossy(&entry.path).into_owned();
        if entry.flags_extended & INDEX_FLAG_INTENT_TO_ADD != 0 {
            existing_ita.insert(path);
        } else {
            real_tracked.insert(path);
        }
    }

    let captured_paths: HashSet<&str> = captured.iter().map(|(path, _)| path.as_str()).collect();
    let before_len = index.entries.len();
    index.entries.retain(|entry| {
        let path = String::from_utf8_lossy(&entry.path);
        !(entry.flags_extended & INDEX_FLAG_INTENT_TO_ADD != 0
            && !captured_paths.contains(path.as_ref()))
    });
    let mut changed = index.entries.len() != before_len;

    let empty_blob = empty_blob_sha1();
    for (path, mode) in captured {
        if real_tracked.contains(path) || existing_ita.contains(path) {
            continue;
        }
        if real_tracked
            .iter()
            .any(|tracked| path_prefix_conflict(path, tracked))
        {
            continue;
        }
        index.entries.push(intent_to_add_entry(
            path.as_bytes(),
            mode.git_mode(),
            &empty_blob,
        ));
        changed = true;
    }

    if !changed {
        return Ok(false);
    }

    if index
        .entries
        .iter()
        .any(|entry| entry.flags_extended & INDEX_FLAG_INTENT_TO_ADD != 0)
        && index.version < 3
    {
        index.version = 3;
    }
    index.entries.sort_by(|left, right| left.path.cmp(&right.path));
    write_index(git_dir, format, &index)?;
    Ok(true)
}

fn empty_index(version: u32) -> Index {
    Index {
        version,
        entries: Vec::new(),
        extensions: Vec::new(),
        checksum: None,
    }
}

fn write_index(git_dir: &Path, format: ObjectFormat, index: &Index) -> Result<()> {
    let path = repository_index_path(git_dir);
    let bytes = index.write(format).map_err(GitSubstrateError::from)?;
    let lock_path = path.with_extension("lock");
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
            .map_err(|err| GitSubstrateError::Other(err.to_string()))?;
        use std::io::Write;
        file.write_all(&bytes)
            .map_err(|err| GitSubstrateError::Other(err.to_string()))?;
        file.sync_all()
            .map_err(|err| GitSubstrateError::Other(err.to_string()))?;
    }
    match fs::rename(&lock_path, &path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(lock_path);
            Err(GitSubstrateError::Other(err.to_string()))
        }
    }
}

fn index_from_tree(
    git_dir: &Path,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    version: u32,
) -> Result<Index> {
    if tree_oid == &empty_tree_sha1() {
        return Ok(Index {
            version,
            entries: Vec::new(),
            extensions: Vec::new(),
            checksum: None,
        });
    }
    let common_dir = repository_common_dir(git_dir);
    let db = FileObjectDatabase::from_git_dir(&common_dir, format);
    let mut blobs = Vec::new();
    collect_tree_blob_entries(&db, format, tree_oid, "", &mut blobs)?;
    blobs.sort_by(|left, right| left.0.cmp(&right.0));
    let entries = blobs
        .into_iter()
        .map(|(path, mode, oid)| tree_index_entry(path, mode, oid))
        .collect();
    Ok(Index {
        version,
        entries,
        extensions: Vec::new(),
        checksum: None,
    })
}

fn collect_tree_blob_entries(
    db: &FileObjectDatabase,
    format: ObjectFormat,
    tree_oid: &ObjectId,
    prefix: &str,
    out: &mut Vec<(Vec<u8>, u32, ObjectId)>,
) -> Result<()> {
    if tree_oid == &empty_tree_sha1() {
        return Ok(());
    }
    let object = db.read_object(tree_oid).map_err(GitSubstrateError::from)?;
    if object.object_type != ObjectType::Tree {
        return Err(GitSubstrateError::Git(sley_core::GitError::InvalidObject(
            format!("expected tree {tree_oid}, found {}", object.object_type.as_str()),
        )));
    }
    let tree = Tree::parse(format, &object.body).map_err(GitSubstrateError::from)?;
    for entry in tree.entries {
        let name = String::from_utf8_lossy(&entry.name);
        validate_tree_entry_name(name.as_ref())?;
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode == 0o040000 {
            collect_tree_blob_entries(db, format, &entry.oid, &path, out)?;
        } else {
            out.push((path.into_bytes(), entry.mode, entry.oid));
        }
    }
    Ok(())
}

fn tree_index_entry(path: impl Into<BString>, mode: u32, oid: ObjectId) -> IndexEntry {
    let path = path.into();
    let name_len = (path.len() as u16).min(INDEX_NAME_MASK);
    IndexEntry {
        ctime_seconds: 0,
        ctime_nanoseconds: 0,
        mtime_seconds: 0,
        mtime_nanoseconds: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: name_len,
        flags_extended: 0,
        path,
    }
}

fn intent_to_add_entry(path: &[u8], mode: u32, oid: &ObjectId) -> IndexEntry {
    let name_len = (path.len() as u16).min(INDEX_NAME_MASK);
    IndexEntry {
        ctime_seconds: 0,
        ctime_nanoseconds: 0,
        mtime_seconds: 0,
        mtime_nanoseconds: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid: oid.clone(),
        flags: INDEX_FLAG_EXTENDED | name_len,
        flags_extended: INDEX_FLAG_INTENT_TO_ADD,
        path: path.into(),
    }
}

fn validate_tree_entry_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return Err(GitSubstrateError::Other(format!(
            "invalid tree entry name: {name:?}"
        )));
    }
    Ok(())
}

fn path_prefix_conflict(a: &str, b: &str) -> bool {
    let child_of = |parent: &str, child: &str| {
        child
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    child_of(a, b) || child_of(b, a)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write_simple_commit;
    use tempfile::TempDir;

    fn init_repo(temp: &TempDir) -> (PathBuf, ObjectFormat) {
        let git_dir = temp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        std::fs::create_dir_all(git_dir.join("refs/heads")).expect("refs");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        (git_dir, ObjectFormat::Sha1)
    }

    #[test]
    fn read_index_from_tree_handles_canonical_empty_tree_without_object() {
        let temp = TempDir::new().expect("tempdir");
        let (git_dir, format) = init_repo(&temp);
        let index = read_index_from_tree(&git_dir, format, &empty_tree_sha1()).expect("empty tree");
        assert!(index.entries.is_empty());
    }

    #[test]
    fn write_index_from_tree_round_trips_entries() {
        let temp = TempDir::new().expect("tempdir");
        let (git_dir, format) = init_repo(&temp);
        let tree = crate::write_tree(&git_dir, format, &mut []).expect("empty tree");
        let actor = crate::refs::bridge_reflog_committer();
        let commit = write_simple_commit(
            &git_dir,
            format,
            &tree,
            &[],
            &actor,
            &actor,
            b"seed",
        )
        .expect("commit");
        write_index_from_tree(&git_dir, format, &tree).expect("write index");
        let index = read_repository_index(&git_dir, format)
            .expect("read")
            .expect("index exists");
        assert!(index.entries.is_empty());
        write_index_from_commit(temp.path(), &git_dir, format, &commit).expect("from commit");
        let index = read_repository_index(&git_dir, format)
            .expect("read")
            .expect("index exists");
        assert!(index.entries.is_empty());
    }

    #[test]
    fn collect_tree_blob_entries_rejects_parent_segment_name() {
        let temp = TempDir::new().expect("tempdir");
        let (git_dir, format) = init_repo(&temp);
        let blob = crate::write_blob(&git_dir, format, b"x\n").expect("blob");
        let mut parent_entries = vec![crate::TreeEntryInput {
            mode: crate::TreeEntryMode::Blob,
            name: "..".into(),
            oid: blob,
        }];
        let parent_tree =
            crate::write_tree(&git_dir, format, &mut parent_entries).expect("parent tree");
        let err = read_index_from_tree(&git_dir, format, &parent_tree).expect_err("reject ..");
        assert!(
            err.to_string().contains("invalid tree entry name"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn reconcile_intent_to_add_adds_and_prunes() {
        let temp = TempDir::new().expect("tempdir");
        let (git_dir, format) = init_repo(&temp);
        write_index(&git_dir, format, &empty_index(3)).expect("seed empty index");
        let changed = reconcile_intent_to_add(
            &git_dir,
            format,
            &[("new.txt".into(), IntentToAddMode::Normal)],
        )
        .expect("reconcile");
        assert!(changed);
        let index = read_repository_index(&git_dir, format)
            .expect("read")
            .expect("index");
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].flags_extended & INDEX_FLAG_INTENT_TO_ADD, INDEX_FLAG_INTENT_TO_ADD);

        let changed = reconcile_intent_to_add(&git_dir, format, &[]).expect("prune");
        assert!(changed);
        let index = read_repository_index(&git_dir, format)
            .expect("read")
            .expect("index");
        assert!(index.entries.is_empty());
    }
}