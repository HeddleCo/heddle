// SPDX-License-Identifier: Apache-2.0
//! Ref updates via sley [`FileRefStore`] (#597 P2).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use sley_core::{GitError, ObjectFormat, ObjectId};
use sley_refs::{branch_ref_name, validate_ref_name, FileRefStore, RefTarget, RefUpdate, ReflogEntry};

use crate::framing::actor_suffix_bytes;
use crate::{GitSubstrateError, Result};

/// Whether `name` is valid as a Git branch shorthand (`git check-ref-format --branch`).
pub fn branch_name_is_valid(name: &str) -> bool {
    if name == "HEAD" || name == "@" || name.starts_with('-') {
        return false;
    }
    branch_ref_name(name).is_ok()
}

/// Whether `name` is a syntactically valid full ref name.
pub fn ref_name_is_valid(name: &str) -> bool {
    validate_ref_name(name).is_ok()
}

/// Constraint on an existing ref before an update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefConstraint {
    /// No expectation check; create or force-update.
    Any,
    /// Ref must not exist (create-only).
    MustNotExist,
    /// Ref must exist and point at `oid`.
    MustExistAndMatch(ObjectId),
}

/// Constraint on an existing direct ref before a delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefDeleteConstraint {
    /// Ref must exist (any value).
    MustExist,
    /// Ref must exist and point at `oid`.
    MustExistAndMatch(ObjectId),
}

/// Reflog committer line for bridge-owned ref writes.
pub fn bridge_reflog_committer() -> Vec<u8> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    actor_suffix_bytes(b"Heddle", b"heddle@local", seconds, 0)
}

/// Update `name` to `target`, optionally checking `constraint` and appending reflog.
pub fn set_reference(
    git_dir: &Path,
    format: ObjectFormat,
    name: &str,
    target: &ObjectId,
    constraint: RefConstraint,
    log_message: &str,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let current = store.read_ref(name).map_err(GitSubstrateError::from)?;

    let expected = match constraint {
        RefConstraint::Any => None,
        RefConstraint::MustNotExist => {
            if current.is_some() {
                return Err(GitSubstrateError::Git(GitError::Transaction(format!(
                    "ref {name} already exists"
                ))));
            }
            None
        }
        RefConstraint::MustExistAndMatch(oid) => Some(RefTarget::Direct(oid)),
    };

    let old_oid = current
        .and_then(|target| match target {
            RefTarget::Direct(oid) => Some(oid),
            RefTarget::Symbolic(_) => None,
        })
        .unwrap_or_else(|| zero_oid(format));

    let mut tx = store.transaction();
    tx.update(RefUpdate {
        name: name.to_string(),
        expected,
        new: RefTarget::Direct(target.clone()),
        reflog: Some(ReflogEntry {
            old_oid,
            new_oid: target.clone(),
            committer: bridge_reflog_committer(),
            message: log_message.as_bytes().to_vec(),
        }),
    });
    tx.commit().map_err(GitSubstrateError::from)
}

/// Delete `name` when present; missing ref is a no-op.
pub fn delete_reference_if_present(git_dir: &Path, format: ObjectFormat, name: &str) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    match store.delete_ref(name) {
        Ok(_) => Ok(()),
        Err(GitError::NotFound(_)) => Ok(()),
        Err(err) => Err(GitSubstrateError::from(err)),
    }
}

/// Delete `name` when `constraint` is satisfied; appends reflog on success.
pub fn delete_reference_matching(
    git_dir: &Path,
    format: ObjectFormat,
    name: &str,
    constraint: RefDeleteConstraint,
    log_message: &str,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let current = store.read_ref(name).map_err(GitSubstrateError::from)?;
    match (&constraint, &current) {
        (RefDeleteConstraint::MustExist, None) => {
            return Err(GitSubstrateError::Git(GitError::Transaction(format!(
                "expected ref {name} to exist"
            ))));
        }
        (RefDeleteConstraint::MustExistAndMatch(expected), Some(RefTarget::Direct(oid)))
            if oid == expected => {}
        (RefDeleteConstraint::MustExistAndMatch(_), _) => {
            return Err(GitSubstrateError::Git(GitError::Transaction(format!(
                "expected ref {name} to match"
            ))));
        }
        (RefDeleteConstraint::MustExist, Some(_)) => {}
    };
    let Some(RefTarget::Direct(old_oid)) = current else {
        return Err(GitSubstrateError::Git(GitError::Transaction(format!(
            "expected ref {name} to exist"
        ))));
    };
    store
        .append_reflog(
            name,
            &ReflogEntry {
                old_oid: old_oid.clone(),
                new_oid: zero_oid(format),
                committer: bridge_reflog_committer(),
                message: log_message.as_bytes().to_vec(),
            },
        )
        .map_err(GitSubstrateError::from)?;
    store.delete_ref(name).map_err(GitSubstrateError::from)?;
    Ok(())
}

/// Update the ref `HEAD` points at (symbolic checkout) to `target`.
pub fn update_head_target_ref(
    git_dir: &Path,
    format: ObjectFormat,
    target: &ObjectId,
    constraint: RefConstraint,
    log_message: &str,
) -> Result<()> {
    let store = FileRefStore::new(git_dir, format);
    let head = store
        .read_ref("HEAD")
        .map_err(GitSubstrateError::from)?
        .ok_or_else(|| GitSubstrateError::Other("HEAD is missing".into()))?;
    let RefTarget::Symbolic(name) = head else {
        return Err(GitSubstrateError::Other(
            "HEAD is detached; expected symbolic ref".into(),
        ));
    };
    set_reference(git_dir, format, &name, target, constraint, log_message)
}

fn zero_oid(format: ObjectFormat) -> ObjectId {
    ObjectId::from_raw(format, &vec![0; format.raw_len()]).expect("zero oid is valid for format")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{empty_tree_sha1, write_commit_content, write_simple_commit};
    use sley_refs::write_loose_ref;
    use tempfile::TempDir;

    fn init_bare_repo(temp: &TempDir) -> (PathBuf, ObjectFormat) {
        let git_dir = temp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects dir");
        std::fs::create_dir_all(git_dir.join("refs/heads")).expect("refs dir");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        (git_dir, ObjectFormat::Sha1)
    }

    fn seed_commit(git_dir: &Path, format: ObjectFormat) -> ObjectId {
        let tree = empty_tree_sha1();
        let actor = bridge_reflog_committer();
        write_simple_commit(
            git_dir,
            format,
            &tree,
            &[],
            &actor,
            &actor,
            b"seed",
        )
        .expect("seed commit")
    }

    fn second_commit(git_dir: &Path, format: ObjectFormat) -> ObjectId {
        let tree = empty_tree_sha1();
        let body = format!(
            "tree {}\nauthor T <t@e> 2 +0000\ncommitter T <t@e> 2 +0000\n\nm2\n",
            tree
        );
        write_commit_content(git_dir, format, body.as_bytes()).expect("second commit")
    }

    #[test]
    fn set_reference_creates_branch() {
        let temp = TempDir::new().expect("tempdir");
        let (git_dir, format) = init_bare_repo(&temp);
        let oid = seed_commit(&git_dir, format);
        set_reference(
            &git_dir,
            format,
            "refs/heads/feature",
            &oid,
            RefConstraint::MustNotExist,
            "test: create",
        )
        .expect("create branch");
        let store = FileRefStore::new(&git_dir, format);
        assert_eq!(
            store.read_ref("refs/heads/feature").expect("read"),
            Some(RefTarget::Direct(oid))
        );
    }

    #[test]
    fn set_reference_enforces_must_exist_and_match() {
        let temp = TempDir::new().expect("tempdir");
        let (git_dir, format) = init_bare_repo(&temp);
        let first = seed_commit(&git_dir, format);
        let second = second_commit(&git_dir, format);
        set_reference(
            &git_dir,
            format,
            "refs/heads/feature",
            &first,
            RefConstraint::MustNotExist,
            "create",
        )
        .expect("create");
        let mismatch = set_reference(
            &git_dir,
            format,
            "refs/heads/feature",
            &second,
            RefConstraint::MustExistAndMatch(second.clone()),
            "wrong expected",
        );
        assert!(mismatch.is_err());
        set_reference(
            &git_dir,
            format,
            "refs/heads/feature",
            &second,
            RefConstraint::MustExistAndMatch(first),
            "advance",
        )
        .expect("advance with matching expected");
    }

    #[test]
    fn delete_reference_matching_enforces_compare_and_swap() {
        let temp = TempDir::new().expect("tempdir");
        let (git_dir, format) = init_bare_repo(&temp);
        let first = seed_commit(&git_dir, format);
        let second = second_commit(&git_dir, format);
        set_reference(
            &git_dir,
            format,
            "refs/heads/feature",
            &first,
            RefConstraint::Any,
            "create",
        )
        .expect("create");
        let mismatch = delete_reference_matching(
            &git_dir,
            format,
            "refs/heads/feature",
            RefDeleteConstraint::MustExistAndMatch(second),
            "wrong expected",
        );
        assert!(mismatch.is_err());
        delete_reference_matching(
            &git_dir,
            format,
            "refs/heads/feature",
            RefDeleteConstraint::MustExistAndMatch(first),
            "delete",
        )
        .expect("delete with matching expected");
        let store = FileRefStore::new(&git_dir, format);
        assert!(store.read_ref("refs/heads/feature").expect("read").is_none());
    }

    #[test]
    fn delete_reference_if_present_is_idempotent() {
        let temp = TempDir::new().expect("tempdir");
        let (git_dir, format) = init_bare_repo(&temp);
        let oid = seed_commit(&git_dir, format);
        set_reference(
            &git_dir,
            format,
            "refs/heads/feature",
            &oid,
            RefConstraint::Any,
            "create",
        )
        .expect("create");
        delete_reference_if_present(&git_dir, format, "refs/heads/feature").expect("delete");
        delete_reference_if_present(&git_dir, format, "refs/heads/feature").expect("delete again");
        let store = FileRefStore::new(&git_dir, format);
        assert!(store.read_ref("refs/heads/feature").expect("read").is_none());
    }

    #[test]
    fn update_head_target_ref_follows_symbolic_head() {
        let temp = TempDir::new().expect("tempdir");
        let (git_dir, format) = init_bare_repo(&temp);
        let oid = seed_commit(&git_dir, format);
        std::fs::write(
            git_dir.join("refs/heads/main"),
            write_loose_ref(&sley_refs::Ref {
                name: "refs/heads/main".into(),
                target: RefTarget::Direct(oid.clone()),
            }),
        )
        .expect("seed main");
        update_head_target_ref(
            &git_dir,
            format,
            &oid,
            RefConstraint::MustExistAndMatch(oid.clone()),
            "touch main",
        )
        .expect("update via HEAD");
    }
}