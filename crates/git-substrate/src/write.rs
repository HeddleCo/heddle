// SPDX-License-Identifier: Apache-2.0
//! P1 write sink — loose-object writes via sley [`FileObjectDatabase`].

use std::path::Path;

use sley_core::{ObjectFormat, ObjectId};
use sley_object::{tree_entry_cmp, Commit, EncodedObject, ObjectType, Tag, Tree, TreeEntry};
use sley_odb::{FileObjectDatabase, ObjectWriter};

use crate::{GitSubstrateError, Result};

/// Git tree entry mode for [`write_tree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeEntryMode {
    Blob,
    BlobExecutable,
    Link,
    Tree,
    GitLink,
}

impl TreeEntryMode {
    /// Octal file mode as stored in a git tree object.
    pub fn as_octal_mode(self) -> u32 {
        match self {
            Self::Blob => 0o100644,
            Self::BlobExecutable => 0o100755,
            Self::Link => 0o120000,
            Self::Tree => 0o040000,
            Self::GitLink => 0o160000,
        }
    }
}

/// One entry in a tree being written.
#[derive(Debug, Clone)]
pub struct TreeEntryInput {
    pub mode: TreeEntryMode,
    pub name: String,
    pub oid: ObjectId,
}

fn write_encoded(git_dir: &Path, format: ObjectFormat, object: EncodedObject) -> Result<ObjectId> {
    let mut odb = FileObjectDatabase::from_git_dir(git_dir, format);
    odb.write_object(object).map_err(GitSubstrateError::from)
}

/// Write blob content into `git_dir`'s object database.
///
/// Idempotent: returns the existing oid when the object is already present.
pub fn write_blob(git_dir: &Path, format: ObjectFormat, content: &[u8]) -> Result<ObjectId> {
    write_encoded(
        git_dir,
        format,
        EncodedObject::new(ObjectType::Blob, content.to_vec()),
    )
}

/// Write commit **content** bytes (unframed — the `git cat-file commit` body).
///
/// Idempotent when the framed object already exists.
pub fn write_commit_content(
    git_dir: &Path,
    format: ObjectFormat,
    content: &[u8],
) -> Result<ObjectId> {
    write_encoded(
        git_dir,
        format,
        EncodedObject::new(ObjectType::Commit, content.to_vec()),
    )
}

/// Write a simple commit (no extra headers) with pre-formatted actor lines.
///
/// `author` and `committer` must be the raw git actor suffix after the label,
/// e.g. `Heddle <heddle@local> 0 +0000`. For fidelity-bearing commits with
/// gpgsig/mergetag, use [`write_commit_content`] with [`build_commit_content`]
/// from the bridge instead.
pub fn write_simple_commit(
    git_dir: &Path,
    format: ObjectFormat,
    tree: &ObjectId,
    parents: &[ObjectId],
    author: &[u8],
    committer: &[u8],
    message: &[u8],
) -> Result<ObjectId> {
    let commit = Commit {
        tree: tree.clone(),
        parents: parents.to_vec(),
        author: author.to_vec(),
        committer: committer.to_vec(),
        encoding: None,
        message: message.to_vec(),
    };
    write_commit_content(git_dir, format, &commit.write())
}

/// Write an annotated tag object into `git_dir`'s object database.
pub fn write_tag(git_dir: &Path, format: ObjectFormat, tag: &Tag) -> Result<ObjectId> {
    write_encoded(
        git_dir,
        format,
        EncodedObject::new(ObjectType::Tag, tag.write()),
    )
}

/// Write a tree from `entries`, sorting entries in Git's canonical tree order.
pub fn write_tree(
    git_dir: &Path,
    format: ObjectFormat,
    entries: &mut [TreeEntryInput],
) -> Result<ObjectId> {
    entries.sort_by(|left, right| {
        tree_entry_cmp(
            left.name.as_bytes(),
            left.mode.as_octal_mode(),
            right.name.as_bytes(),
            right.mode.as_octal_mode(),
        )
    });
    let tree_entries: Vec<TreeEntry> = entries
        .iter()
        .map(|entry| TreeEntry {
            mode: entry.mode.as_octal_mode(),
            name: entry.name.as_bytes().into(),
            oid: entry.oid.clone(),
        })
        .collect();
    let body = Tree {
        entries: tree_entries,
    }
    .write();
    write_encoded(
        git_dir,
        format,
        EncodedObject::new(ObjectType::Tree, body),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{empty_tree_sha1, parse_sha1_hex};
    use sley_odb::ObjectReader;
    use tempfile::TempDir;

    #[test]
    fn write_blob_matches_known_sha() {
        let tmp = TempDir::new().expect("temp");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects dir");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("head");

        let oid =
            write_blob(&git_dir, ObjectFormat::Sha1, b"hello\n").expect("write blob");
        assert_eq!(
            oid.to_hex(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        assert!(git_dir.join("objects/ce/013625030ba8dba906f756967f9e9ca394464a").is_file());
    }

    #[test]
    fn write_tree_empty_matches_canonical_empty_tree() {
        let tmp = TempDir::new().expect("temp");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects dir");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("head");

        let mut entries = Vec::new();
        let oid = write_tree(&git_dir, ObjectFormat::Sha1, &mut entries).expect("write tree");
        assert_eq!(oid.to_hex(), empty_tree_sha1().to_hex());
    }

    #[test]
    fn write_tree_sorts_subtree_after_same_prefix_blob() {
        let tmp = TempDir::new().expect("temp");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects dir");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("head");

        let blob_foo_txt =
            write_blob(&git_dir, ObjectFormat::Sha1, b"txt\n").expect("blob foo.txt");
        let blob_in_foo =
            write_blob(&git_dir, ObjectFormat::Sha1, b"inner\n").expect("blob in foo");
        let mut foo_tree_entries = vec![TreeEntryInput {
            mode: TreeEntryMode::Blob,
            name: "inner".into(),
            oid: blob_in_foo,
        }];
        let foo_tree_oid =
            write_tree(&git_dir, ObjectFormat::Sha1, &mut foo_tree_entries).expect("foo tree");

        // Deliberately out of canonical order: subtree `foo` before blob `foo.txt`.
        let mut entries = vec![
            TreeEntryInput {
                mode: TreeEntryMode::Tree,
                name: "foo".into(),
                oid: foo_tree_oid,
            },
            TreeEntryInput {
                mode: TreeEntryMode::Blob,
                name: "foo.txt".into(),
                oid: blob_foo_txt,
            },
        ];
        let oid = write_tree(&git_dir, ObjectFormat::Sha1, &mut entries).expect("write tree");

        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = odb.read_object(&oid).expect("read tree");
        let tree = Tree::parse(ObjectFormat::Sha1, &object.body).expect("parse tree");
        assert_eq!(tree.entries.len(), 2);
        assert_eq!(tree.entries[0].name, b"foo.txt");
        assert_eq!(tree.entries[1].name, b"foo");
        assert_eq!(tree.entries[1].mode, 0o040000);
    }

    #[test]
    fn write_tree_sorts_entries_by_name() {
        let tmp = TempDir::new().expect("temp");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects dir");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("head");

        let blob_b = write_blob(&git_dir, ObjectFormat::Sha1, b"b\n").expect("blob b");
        let blob_a = write_blob(&git_dir, ObjectFormat::Sha1, b"a\n").expect("blob a");

        // Deliberately out of order — write_tree must sort before serializing.
        let mut entries = vec![
            TreeEntryInput {
                mode: TreeEntryMode::Blob,
                name: "z.txt".into(),
                oid: blob_b.clone(),
            },
            TreeEntryInput {
                mode: TreeEntryMode::Blob,
                name: "a.txt".into(),
                oid: blob_a,
            },
        ];
        let oid = write_tree(&git_dir, ObjectFormat::Sha1, &mut entries).expect("write tree");

        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let object = odb.read_object(&oid).expect("read tree");
        let tree = Tree::parse(ObjectFormat::Sha1, &object.body).expect("parse tree");
        assert_eq!(tree.entries.len(), 2);
        assert_eq!(tree.entries[0].name, b"a.txt");
        assert_eq!(tree.entries[1].name, b"z.txt");
    }

    #[test]
    fn write_commit_content_round_trips_known_empty_tree_commit_body() {
        let tmp = TempDir::new().expect("temp");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects dir");
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").expect("head");

        let empty_tree = empty_tree_sha1();
        let body = format!(
            "tree {}\nauthor T <t@e> 0 +0000\ncommitter T <t@e> 0 +0000\n\n",
            empty_tree.to_hex()
        );
        let oid =
            write_commit_content(&git_dir, ObjectFormat::Sha1, body.as_bytes()).expect("write");
        assert!(git_dir.join(format!("objects/{}/{}", &oid.to_hex()[..2], &oid.to_hex()[2..])).is_file());

        let submodule = parse_sha1_hex("0303030303030303030303030303030303030303").expect("oid");
        let mut entries = vec![TreeEntryInput {
            mode: TreeEntryMode::GitLink,
            name: "vendor".into(),
            oid: submodule,
        }];
        let tree_oid = write_tree(&git_dir, ObjectFormat::Sha1, &mut entries).expect("gitlink tree");
        let odb = FileObjectDatabase::from_git_dir(&git_dir, ObjectFormat::Sha1);
        let parsed = Tree::parse(ObjectFormat::Sha1, &odb.read_object(&tree_oid).expect("read").body)
            .expect("parse");
        assert_eq!(parsed.entries[0].mode, 0o160000);
    }
}