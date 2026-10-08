// SPDX-License-Identifier: Apache-2.0
//! Read-through objects for a Git-overlay repository.
//!
//! The SQLite map and state descriptors are identity metadata only. Blob and
//! tree contents remain authoritative in `.git` and are translated on demand.

use std::{
    fs,
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

use objects::{
    error::{HeddleError, Result},
    object::{
        Blob, ContentHash, EntryType, State, StateId, Tree, TreeEntry, parse_git_tree,
        reserved_tree_entry_name,
    },
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
    mapping: Mutex<Option<Connection>>,
    git: Mutex<Option<SleyRepository>>,
}

impl GitOverlayObjectSource {
    pub(crate) fn new(root: PathBuf, heddle_dir: PathBuf) -> Self {
        Self {
            root,
            heddle_dir,
            mapping: Mutex::new(None),
            git: Mutex::new(None),
        }
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
        let mut mapping = self.mapping()?;
        let Some(connection) = self.open_mapping(&mut mapping)? else {
            return Ok(None);
        };
        connection
            .query_row(
                "SELECT git_sha FROM sha_map WHERE heddle_repr = ? AND kind = ?",
                params![value, kind],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)
    }

    /// The identity map's format (SQLite `user_version`); 0 before heddle#2018.
    fn map_format(&self) -> Result<i32> {
        let mut mapping = self.mapping()?;
        let Some(connection) = self.open_mapping(&mut mapping)? else {
            return Ok(crate::GIT_IMPORT_MAP_FORMAT);
        };
        connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(db_error)
    }

    fn heddle_for_git(&self, oid: &ObjectId, kind: i64) -> Result<Option<String>> {
        let mut mapping = self.mapping()?;
        let Some(connection) = self.open_mapping(&mut mapping)? else {
            return Ok(None);
        };
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
        let mut git = self.git.lock().map_err(|_| {
            HeddleError::Config("Git-overlay repository cache lock was poisoned".to_string())
        })?;
        if let Some(repo) = git.as_ref() {
            return Ok(repo.clone());
        }
        let repo = crate::open_git_import_source(&self.root).map_err(|error| {
            HeddleError::Config(format!(
                "open authoritative Git object database at {}: {error}",
                self.root.display()
            ))
        })?;
        *git = Some(repo.clone());
        Ok(repo)
    }

    fn refresh_git(&self) -> Result<SleyRepository> {
        let repo = crate::open_git_import_source(&self.root).map_err(|error| {
            HeddleError::Config(format!(
                "refresh authoritative Git object database at {}: {error}",
                self.root.display()
            ))
        })?;
        let mut git = self.git.lock().map_err(|_| {
            HeddleError::Config("Git-overlay repository cache lock was poisoned".to_string())
        })?;
        *git = Some(repo.clone());
        Ok(repo)
    }

    fn mapping(&self) -> Result<MutexGuard<'_, Option<Connection>>> {
        self.mapping.lock().map_err(|_| {
            HeddleError::Config("Git-overlay identity mapping cache lock was poisoned".to_string())
        })
    }

    fn open_mapping<'a>(
        &self,
        mapping: &'a mut Option<Connection>,
    ) -> Result<Option<&'a Connection>> {
        if mapping.is_none() {
            let path = self.map_path();
            if !path.exists() {
                return Ok(None);
            }
            *mapping = Some(Connection::open(path).map_err(db_error)?);
        }
        Ok(mapping.as_ref())
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
        let object = match git.read_object(&oid) {
            Ok(object) => object,
            Err(sley::GitError::NotFound(_)) => match self.refresh_git()?.read_object(&oid) {
                Ok(object) => object,
                Err(sley::GitError::NotFound(_)) => {
                    return Err(mapped_object_missing("blob", &git_sha));
                }
                Err(error) => return Err(git_read_error(error)),
            },
            Err(error) => return Err(git_read_error(error)),
        };
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
        let object = if oid == ObjectId::empty_tree(git.object_format()) {
            None
        } else {
            let object = match git.read_object(&oid) {
                Ok(object) => object,
                Err(sley::GitError::NotFound(_)) => match self.refresh_git()?.read_object(&oid) {
                    Ok(object) => object,
                    Err(sley::GitError::NotFound(_)) => {
                        return Err(mapped_object_missing("tree", &git_sha));
                    }
                    Err(error) => return Err(git_read_error(error)),
                },
                Err(error) => return Err(git_read_error(error)),
            };
            if object.object_type != sley::GitObjectType::Tree {
                return Err(HeddleError::InvalidObject(format!(
                    "mapped Git tree {git_sha} is a {}",
                    object.object_type.as_str()
                )));
            }
            Some(object)
        };
        let body = object
            .as_ref()
            .map_or(&[][..], |object| object.body.as_slice());
        // Read the raw tree so a non-canonical source (odd modes, source
        // order) translates to the same native tree the importer stored.
        let children = parse_git_tree(git.object_format(), body)
            .map_err(|error| HeddleError::InvalidObject(format!("Git tree {git_sha}: {error}")))?;
        let mut entries = Vec::with_capacity(children.len());
        for child in children {
            // `.git` aliases are refused at every depth (heddle#2028). This
            // source does not know whether the tree is a root, so a root
            // `.heddle` is left to checkout, which refuses it.
            if let Some(reason) = reserved_tree_entry_name(child.name, false) {
                return Err(HeddleError::InvalidObject(format!(
                    "Git tree {git_sha} entry '{}' {reason}",
                    String::from_utf8_lossy(child.name)
                )));
            }
            let name = String::from_utf8(child.name.to_vec()).map_err(|_| {
                HeddleError::Config(format!(
                    "Git tree {git_sha} has a non-UTF-8 entry; run `heddle import local --lossy` to import it explicitly"
                ))
            })?;
            let entry = match child.mode.entry_type() {
                Some(EntryType::Tree) => {
                    let mapped = self
                        .heddle_for_git(&child.oid, KIND_TREE)?
                        .ok_or_else(|| missing_mapping("tree", &child.oid, &git_sha))?;
                    TreeEntry::directory(name, parse_hash(&mapped)?)
                }
                Some(EntryType::Blob) => {
                    let mapped = self
                        .heddle_for_git(&child.oid, KIND_BLOB)?
                        .ok_or_else(|| missing_mapping("blob", &child.oid, &git_sha))?;
                    TreeEntry::file(name, parse_hash(&mapped)?, child.mode.is_executable())
                }
                Some(EntryType::Symlink) => {
                    let mapped = self
                        .heddle_for_git(&child.oid, KIND_BLOB)?
                        .ok_or_else(|| missing_mapping("symlink blob", &child.oid, &git_sha))?;
                    TreeEntry::symlink(name, parse_hash(&mapped)?)
                }
                Some(EntryType::Gitlink) => TreeEntry::gitlink(name, child.oid),
                Some(EntryType::Spoollink) | None => {
                    return Err(HeddleError::Config(format!(
                        "Git tree {git_sha} entry has unsupported mode {:o}",
                        child.mode.value()
                    )));
                }
            }
            .and_then(|entry| entry.with_raw_git_mode(child.mode))
            .map_err(|error| HeddleError::InvalidObject(error.to_string()))?;
            entries.push(entry);
        }
        let tree = Tree::from_git_entries(entries)
            .map_err(|error| HeddleError::InvalidObject(format!("Git tree {git_sha}: {error}")))?;
        // A map from before heddle#2018 stored a normalised id for exactly
        // the trees that record a Git layout. Its rows cannot be trusted for
        // them; the identity map has to be rebuilt.
        if tree.has_git_layout() && self.map_format()? < crate::GIT_IMPORT_MAP_FORMAT {
            return Err(HeddleError::Config(format!(
                "Git tree {git_sha} has a non-canonical mode or entry order, and this \
                 repository's Git import map predates exact Git tree layouts (heddle#2018); \
                 re-import required: recreate the repository or re-adopt its Git history \
                 with this Heddle version"
            )));
        }
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
        let state = State::decode_current_msgpack(&bytes)?;
        let actual = state.id();
        if actual != *id {
            return Err(HeddleError::InvalidObject(format!(
                "Git-overlay state descriptor {} hashes to {}",
                id.to_string_full(),
                actual.to_string_full()
            )));
        }
        Ok(Some(state))
    }

    fn list_states(&self) -> Result<Vec<StateId>> {
        let dir = self.heddle_dir.join("ingest").join("overlay-states");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut states = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "state")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && let Ok(id) = StateId::parse(stem)
                && self
                    .git_for_heddle(&id.to_string_full(), KIND_COMMIT)?
                    .is_some()
            {
                states.push(id);
            }
        }
        Ok(states)
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

fn mapped_object_missing(kind: &str, git_sha: &str) -> HeddleError {
    HeddleError::NotFound(format!(
        "Git-overlay identity map references missing authoritative Git {kind} {git_sha}"
    ))
}

fn db_error(error: rusqlite::Error) -> HeddleError {
    HeddleError::Config(format!("read Git-overlay identity mapping: {error}"))
}

fn git_read_error(error: impl std::fmt::Display) -> HeddleError {
    HeddleError::Config(format!("read authoritative Git object: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping_table(heddle_dir: &std::path::Path) -> Connection {
        let ingest_dir = heddle_dir.join("ingest");
        fs::create_dir_all(&ingest_dir).unwrap();
        let connection = Connection::open(ingest_dir.join("sha_map.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sha_map (
                    git_sha TEXT PRIMARY KEY NOT NULL,
                    kind INTEGER NOT NULL,
                    heddle_repr TEXT NOT NULL,
                    lossy_entries TEXT
                );",
            )
            .unwrap();
        connection
    }

    /// A source tree with a `100664` entry out of Git's order, its blobs, and
    /// the native tree the current importer maps it to.
    struct LayoutFixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        heddle_dir: PathBuf,
        tree_oid: ObjectId,
        rows: Vec<(String, i64, String)>,
        expected: Tree,
        normalised: Tree,
    }

    fn layout_fixture() -> LayoutFixture {
        let temp = tempfile::TempDir::new().unwrap();
        let git = SleyRepository::init(temp.path()).unwrap();
        let a = git.write_blob(b"a\n").unwrap();
        let b = git.write_blob(b"b\n").unwrap();
        let mut body = Vec::new();
        for (mode, name, oid) in [("100664", "b.txt", b), ("100644", "a.txt", a)] {
            body.extend_from_slice(mode.as_bytes());
            body.push(b' ');
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(oid.as_bytes());
        }
        let tree_oid = git
            .write_raw_object(sley::GitObjectType::Tree, body)
            .unwrap();
        let a_hash = Blob::from_slice(b"a\n").hash();
        let b_hash = Blob::from_slice(b"b\n").hash();
        let expected = Tree::from_git_entries(vec![
            TreeEntry::file("b.txt", b_hash, false)
                .unwrap()
                .with_raw_git_mode(objects::object::RawGitMode::parse(b"100664").unwrap())
                .unwrap(),
            TreeEntry::file("a.txt", a_hash, false).unwrap(),
        ])
        .unwrap();
        // What a pre-#2018 importer stored: modes and order normalised.
        let normalised = Tree::from_entries(vec![
            TreeEntry::file("b.txt", b_hash, false).unwrap(),
            TreeEntry::file("a.txt", a_hash, false).unwrap(),
        ]);
        LayoutFixture {
            root: temp.path().to_path_buf(),
            heddle_dir: temp.path().join(".heddle"),
            _temp: temp,
            tree_oid,
            rows: vec![
                (a.to_string(), KIND_BLOB, a_hash.to_hex()),
                (b.to_string(), KIND_BLOB, b_hash.to_hex()),
            ],
            expected,
            normalised,
        }
    }

    fn write_map(fixture: &LayoutFixture, tree: &Tree, format: i32) {
        let connection = mapping_table(&fixture.heddle_dir);
        connection
            .pragma_update(None, "user_version", format)
            .unwrap();
        let tree_row = (
            fixture.tree_oid.to_string(),
            KIND_TREE,
            tree.hash().to_hex(),
        );
        for (git_sha, kind, heddle_repr) in fixture.rows.iter().chain([&tree_row]) {
            connection
                .execute(
                    "INSERT INTO sha_map (git_sha, kind, heddle_repr) VALUES (?, ?, ?)",
                    params![git_sha, kind, heddle_repr],
                )
                .unwrap();
        }
    }

    /// heddle#2018: a non-canonical source tree read through the overlay must
    /// translate to the same native tree the importer stored — raw modes and
    /// source order included — or the id check rejects it as corruption.
    #[test]
    fn noncanonical_git_tree_reads_through_with_its_layout() {
        let fixture = layout_fixture();
        assert!(fixture.expected.has_git_layout());
        write_map(&fixture, &fixture.expected, crate::GIT_IMPORT_MAP_FORMAT);
        let source = GitOverlayObjectSource::new(fixture.root.clone(), fixture.heddle_dir.clone());
        let tree = source
            .get_tree(&fixture.expected.hash())
            .expect("layout tree reads through")
            .expect("mapped tree");
        assert_eq!(tree, fixture.expected);
    }

    /// A map written before heddle#2018 holds the normalised id of a
    /// non-canonical tree. Reading it must ask for a re-import, not report
    /// corruption and not serve the normalised tree.
    #[test]
    fn pre_layout_map_asks_for_a_reimport() {
        let fixture = layout_fixture();
        write_map(&fixture, &fixture.normalised, 0);
        let source = GitOverlayObjectSource::new(fixture.root.clone(), fixture.heddle_dir.clone());
        let error = source
            .get_tree(&fixture.normalised.hash())
            .expect_err("pre-#2018 map");
        assert!(
            !matches!(error, HeddleError::Corruption { .. }),
            "must not be reported as corruption: {error:?}"
        );
        let message = error.to_string();
        assert!(message.contains("re-import"), "{message}");
        assert!(message.contains(&fixture.tree_oid.to_string()), "{message}");
    }

    #[test]
    fn mapped_missing_git_objects_are_errors_after_refresh() {
        let temp = tempfile::TempDir::new().unwrap();
        SleyRepository::init(temp.path()).unwrap();
        let heddle_dir = temp.path().join(".heddle");
        let ingest_dir = heddle_dir.join("ingest");
        fs::create_dir_all(&ingest_dir).unwrap();
        let connection = Connection::open(ingest_dir.join("sha_map.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE sha_map (
                    git_sha TEXT PRIMARY KEY NOT NULL,
                    kind INTEGER NOT NULL,
                    heddle_repr TEXT NOT NULL,
                    lossy_entries TEXT
                );",
            )
            .unwrap();
        let blob_hash = ContentHash::compute_typed("blob", b"mapped missing blob");
        let tree_hash = ContentHash::compute_typed("tree", b"mapped missing tree");
        let missing_blob_oid = "1111111111111111111111111111111111111111";
        let missing_tree_oid = "2222222222222222222222222222222222222222";
        for (git_sha, kind, heddle_repr) in [
            (missing_blob_oid, KIND_BLOB, blob_hash.to_hex()),
            (missing_tree_oid, KIND_TREE, tree_hash.to_hex()),
        ] {
            connection
                .execute(
                    "INSERT INTO sha_map (git_sha, kind, heddle_repr) VALUES (?, ?, ?)",
                    params![git_sha, kind, heddle_repr],
                )
                .unwrap();
        }
        drop(connection);

        let source = GitOverlayObjectSource::new(temp.path().to_path_buf(), heddle_dir);
        for (kind, result) in [
            ("blob", source.get_blob(&blob_hash).map(|_| ())),
            ("tree", source.get_tree(&tree_hash).map(|_| ())),
        ] {
            let error = result.expect_err("mapped Git object absence must be visible");
            assert!(
                matches!(error, HeddleError::NotFound(ref message) if message.contains(kind) && message.contains("identity map")),
                "{error}"
            );
        }
    }
}
