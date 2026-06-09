// SPDX-License-Identifier: Apache-2.0
//! Reachable-object copy between repositories (#597 P2).

use std::collections::HashSet;

use sley_core::ObjectFormat;
use sley_core::ObjectId;
use sley_object::{Commit, EncodedObject, ObjectType, Tag, Tree};
use sley_odb::{FileObjectDatabase, ObjectReader, ObjectWriter};
use sley_pack::{PackFile, PackInput, PackWriteOptions};

use crate::repo::GitRepo;
use crate::{empty_blob_sha1, empty_tree_sha1, GitSubstrateError, Result};

const GITLINK_MODE: u32 = 0o160000;

/// Copy all objects reachable from `roots` in `source` into `target`.
///
/// Gitlink (`160000`) tree entries are skipped: they reference commits in
/// submodule repositories that are not stored locally.
pub fn copy_reachable_objects(
    source: &GitRepo,
    target: &GitRepo,
    roots: impl IntoIterator<Item = ObjectId>,
) -> Result<()> {
    if source.object_format() != target.object_format() {
        return Err(GitSubstrateError::Other(format!(
            "object format mismatch: {} vs {}",
            source.object_format().name(),
            target.object_format().name()
        )));
    }
    let format = source.object_format();
    let source_db = source.object_db();
    let mut target_db = FileObjectDatabase::from_git_dir(target.common_dir(), format);
    for object in collect_reachable_objects_ordered(&source_db, format, roots)? {
        target_db
            .write_object(object)
            .map_err(GitSubstrateError::from)?;
    }
    Ok(())
}

/// Ordered object ids reachable from `roots`, including gitlink-safe tree walk.
pub fn collect_reachable_object_ids(
    source: &GitRepo,
    roots: impl IntoIterator<Item = ObjectId>,
) -> Result<Vec<ObjectId>> {
    let format = source.object_format();
    let source_db = source.object_db();
    Ok(collect_reachable_objects_ordered(&source_db, format, roots)?
        .into_iter()
        .map(|object| object.object_id(format))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(GitSubstrateError::from)?)
}

fn collect_reachable_objects_ordered<R: ObjectReader>(
    reader: &R,
    format: ObjectFormat,
    roots: impl IntoIterator<Item = ObjectId>,
) -> Result<Vec<EncodedObject>> {
    let mut stack: Vec<ObjectId> = roots.into_iter().collect();
    let mut seen = HashSet::new();
    let mut ordered = Vec::new();

    while let Some(oid) = stack.pop() {
        if !seen.insert(oid.clone()) {
            continue;
        }
        if is_materialized_canonical_missing(format, reader, &oid)? {
            continue;
        }
        let object = reader
            .read_object(&oid)
            .map_err(GitSubstrateError::from)?;
        enqueue_reachable_children(reader, format, &object, &mut stack)?;
        ordered.push((*object).clone());
    }

    Ok(ordered)
}

fn is_materialized_canonical_missing<R: ObjectReader>(
    format: ObjectFormat,
    reader: &R,
    oid: &ObjectId,
) -> Result<bool> {
    if format != sley_core::ObjectFormat::Sha1 {
        return Ok(false);
    }
    if oid != &empty_tree_sha1() && oid != &empty_blob_sha1() {
        return Ok(false);
    }
    Ok(matches!(
        reader.read_object(oid),
        Err(sley_core::GitError::NotFound(_))
    ))
}

fn enqueue_reachable_children<R: ObjectReader>(
    reader: &R,
    format: ObjectFormat,
    object: &EncodedObject,
    stack: &mut Vec<ObjectId>,
) -> Result<()> {
    match object.object_type {
        ObjectType::Commit => {
            let commit = Commit::parse(format, &object.body).map_err(GitSubstrateError::from)?;
            if !is_materialized_canonical_missing(format, reader, &commit.tree)? {
                stack.push(commit.tree);
            }
            stack.extend(commit.parents);
        }
        ObjectType::Tree => {
            let tree = Tree::parse(format, &object.body).map_err(GitSubstrateError::from)?;
            for entry in tree.entries {
                if entry.mode == GITLINK_MODE {
                    continue;
                }
                stack.push(entry.oid);
            }
        }
        ObjectType::Tag => {
            let tag = Tag::parse(format, &object.body).map_err(GitSubstrateError::from)?;
            stack.push(tag.object);
        }
        ObjectType::Blob => {}
    }
    let _ = reader;
    Ok(())
}

/// Encode reachable objects from `roots` into a v2 pack suitable for push.
pub fn pack_reachable_objects(
    repo: &GitRepo,
    roots: impl IntoIterator<Item = ObjectId>,
) -> Result<Vec<u8>> {
    let format = repo.object_format();
    let oids = collect_reachable_object_ids(repo, roots)?;
    let objects = oids
        .iter()
        .map(|oid| repo.read_object(oid))
        .collect::<Result<Vec<_>>>()?;
    let inputs = oids
        .iter()
        .zip(objects.iter())
        .map(|(oid, object)| PackInput { oid, object })
        .collect::<Vec<_>>();
    let options = PackWriteOptions::new().with_depth(0).with_reorder(false);
    let written =
        PackFile::write_packed_with_known_ids_and_options(&inputs, format, &options)?;
    Ok(written.pack)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::{set_reference, RefConstraint};
    use crate::{empty_tree_sha1, write_simple_commit};
    use tempfile::TempDir;

    fn init_repo(temp: &TempDir) -> GitRepo {
        let root = temp.path();
        let git_dir = root.join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        std::fs::create_dir_all(git_dir.join("refs/heads")).expect("refs");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
        GitRepo::discover(root).expect("discover")
    }

    #[test]
    fn copy_reachable_objects_round_trips_commit_chain() {
        let source_dir = TempDir::new().expect("tempdir");
        let target_dir = TempDir::new().expect("tempdir");
        let source = init_repo(&source_dir);
        let target = init_repo(&target_dir);
        let format = source.object_format();
        let actor = crate::refs::bridge_reflog_committer();
        let root_tree = empty_tree_sha1();
        let root = write_simple_commit(
            source.git_dir(),
            format,
            &root_tree,
            &[],
            &actor,
            &actor,
            b"root",
        )
        .expect("root");
        let tip = write_simple_commit(
            source.git_dir(),
            format,
            &root_tree,
            std::slice::from_ref(&root),
            &actor,
            &actor,
            b"tip",
        )
        .expect("tip");
        set_reference(
            source.git_dir(),
            format,
            "refs/heads/main",
            &tip,
            RefConstraint::Any,
            "test",
        )
        .expect("branch");

        copy_reachable_objects(&source, &target, [tip.clone()]).expect("copy");
        assert!(target.has_object(&root).expect("contains root"));
        assert!(target.has_object(&tip).expect("contains tip"));
    }
}