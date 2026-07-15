// SPDX-License-Identifier: Apache-2.0
//! Read-through objects for a Git-overlay repository.
//!
//! The SQLite map and state descriptors are identity metadata only. Blob and
//! tree contents remain authoritative in `.git` and are translated on demand.

use std::{fs, path::PathBuf};

use objects::{
    error::{HeddleError, Result},
    object::{Blob, ContentHash, State, StateId, Tree, TreeEntry},
    store::ExternalObjectSource,
};
use rusqlite::{Connection, OptionalExtension, params};
use sley::{ObjectId, Repository as SleyRepository};

const KIND_COMMIT: i64 = 0;
const KIND_TREE: i64 = 1;
const KIND_BLOB: i64 = 2;

pub(crate) struct GitOverlayObjectSource {
    root: PathBuf,
    heddle_dir: PathBuf,
}

impl GitOverlayObjectSource {
    pub(crate) fn new(root: PathBuf, heddle_dir: PathBuf) -> Self {
        Self { root, heddle_dir }
    }

    fn map_path(&self) -> PathBuf {
        self.heddle_dir.join("ingest").join("sha_map.sqlite")
    }

    fn state_path(&self, id: &StateId) -> PathBuf {
        self.heddle_dir
            .join("ingest")
            .join("overlay-states")
            .join(format!("{}.state", id.to_string_full()))
    }

    fn git_for_heddle(&self, value: &str, kind: i64) -> Result<Option<String>> {
        let path = self.map_path();
        if !path.exists() {
            return Ok(None);
        }
        let connection = Connection::open(&path).map_err(db_error)?;
        connection
            .query_row(
                "SELECT git_sha FROM sha_map WHERE heddle_repr = ? AND kind = ?",
                params![value, kind],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)
    }

    fn heddle_for_git(&self, oid: &ObjectId, kind: i64) -> Result<Option<String>> {
        let path = self.map_path();
        if !path.exists() {
            return Ok(None);
        }
        let connection = Connection::open(&path).map_err(db_error)?;
        connection
            .query_row(
                "SELECT heddle_repr FROM sha_map WHERE git_sha = ? AND kind = ?",
                params![oid.to_string(), kind],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)
    }

    fn git(&self) -> Result<SleyRepository> {
        SleyRepository::discover(&self.root).map_err(|error| {
            HeddleError::Config(format!(
                "open authoritative Git object database at {}: {error}",
                self.root.display()
            ))
        })
    }
}

impl ExternalObjectSource for GitOverlayObjectSource {
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        let Some(git_sha) = self.git_for_heddle(&hash.to_hex(), KIND_BLOB)? else {
            return Ok(None);
        };
        let git = self.git()?;
        let oid = ObjectId::from_hex(git.object_format(), &git_sha).map_err(|error| {
            HeddleError::Config(format!("parse mapped Git blob {git_sha}: {error}"))
        })?;
        let object = git.read_object(&oid).map_err(git_read_error)?;
        let blob = Blob::from_slice(&object.body);
        if blob.hash() != *hash {
            return Err(HeddleError::Corruption {
                expected: *hash,
                found: blob.hash(),
            });
        }
        Ok(Some(blob))
    }

    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        let Some(git_sha) = self.git_for_heddle(&hash.to_hex(), KIND_TREE)? else {
            return Ok(None);
        };
        let git = self.git()?;
        let oid = ObjectId::from_hex(git.object_format(), &git_sha).map_err(|error| {
            HeddleError::Config(format!("parse mapped Git tree {git_sha}: {error}"))
        })?;
        let children = if oid == ObjectId::empty_tree(git.object_format()) {
            Vec::new()
        } else {
            git.read_tree(&oid).map_err(git_read_error)?.entries
        };
        let mut entries = Vec::with_capacity(children.len());
        for child in children {
            let name = String::from_utf8(child.name.as_bytes().to_vec()).map_err(|_| {
                HeddleError::Config(format!(
                    "Git tree {git_sha} has a non-UTF-8 entry; run `heddle adopt --lossy` to import it explicitly"
                ))
            })?;
            let entry = match child.mode {
                0o040000 => {
                    let mapped = self
                        .heddle_for_git(&child.oid, KIND_TREE)?
                        .ok_or_else(|| missing_mapping("tree", &child.oid, &git_sha))?;
                    TreeEntry::directory(name, parse_hash(&mapped)?)
                }
                0o100644 | 0o100755 => {
                    let mapped = self
                        .heddle_for_git(&child.oid, KIND_BLOB)?
                        .ok_or_else(|| missing_mapping("blob", &child.oid, &git_sha))?;
                    TreeEntry::file(name, parse_hash(&mapped)?, child.mode == 0o100755)
                }
                0o120000 => {
                    let mapped = self
                        .heddle_for_git(&child.oid, KIND_BLOB)?
                        .ok_or_else(|| missing_mapping("symlink blob", &child.oid, &git_sha))?;
                    TreeEntry::symlink(name, parse_hash(&mapped)?)
                }
                0o160000 => TreeEntry::gitlink(name, child.oid),
                mode => {
                    return Err(HeddleError::Config(format!(
                        "Git tree {git_sha} entry has unsupported mode {mode:o}"
                    )));
                }
            }
            .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
            entries.push(entry);
        }
        let tree = Tree::from_entries(entries);
        if tree.hash() != *hash {
            return Err(HeddleError::Corruption {
                expected: *hash,
                found: tree.hash(),
            });
        }
        Ok(Some(tree))
    }

    fn get_state(&self, id: &StateId) -> Result<Option<State>> {
        // A state descriptor is durable identity/commit metadata, not a copy
        // of Git source content. Its tree and blobs are resolved above.
        if self
            .git_for_heddle(&id.to_string_full(), KIND_COMMIT)?
            .is_none()
        {
            return Ok(None);
        }
        let path = self.state_path(id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut state: State = rmp_serde::from_slice(&bytes)?;
        let actual = state.id();
        if actual != *id {
            return Err(HeddleError::InvalidObject(format!(
                "Git-overlay state descriptor {} hashes to {}",
                id.to_string_full(),
                actual.to_string_full()
            )));
        }
        state.state_id = actual;
        Ok(Some(state))
    }
}

fn parse_hash(value: &str) -> Result<ContentHash> {
    ContentHash::from_hex(value).map_err(|error| {
        HeddleError::InvalidObject(format!("invalid mapped content hash: {error}"))
    })
}

fn missing_mapping(kind: &str, oid: &ObjectId, parent: &str) -> HeddleError {
    HeddleError::Config(format!(
        "Git-overlay {kind} {oid} referenced by tree {parent} has no identity mapping"
    ))
}

fn db_error(error: rusqlite::Error) -> HeddleError {
    HeddleError::Config(format!("read Git-overlay identity mapping: {error}"))
}

fn git_read_error(error: impl std::fmt::Display) -> HeddleError {
    HeddleError::Config(format!("read authoritative Git object: {error}"))
}
