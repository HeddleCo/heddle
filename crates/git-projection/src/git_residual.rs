// SPDX-License-Identifier: Apache-2.0
//! Raw Git Object Residual storage.
//!
//! A **Raw Git Object Residual** holds verbatim Git object bytes that Heddle
//! cannot reconstruct byte-for-byte from native state (lossy imports,
//! non-UTF8 identities, and other fidelity gaps). Residuals are durable
//! repository metadata under `.heddle/`, **not** part of source history State
//! hashes.
//!
//! # Layout
//!
//! ```text
//! .heddle/git-residuals/
//!   <object-format>/          # "sha1" or "sha256"
//!     <oid[0..2]>/            # two hex digits
//!       <oid[2..]>            # remaining hex digits of the Git oid
//! ```
//!
//! Each residual file is a small durable record:
//!
//! - magic `b"HR01"` (Heddle Residual v1)
//! - one-byte object type tag (`1=commit`, `2=tree`, `3=blob`, `4=tag`)
//! - canonical Git object **body** (not loose zlib compression, not pack layout)
//!
//! Content identity is the Git object id of `(type, body)` under the recorded
//! object format. Put verifies that the body hashes to the claimed oid.
//!
//! # Bridge Mirror relationship
//!
//! Residuals replace the long-term need for a persistent Bridge Mirror
//! (`.heddle/git`) as a byte warehouse for lossy objects. The Bridge Mirror
//! remains available for migration: callers may copy residual bytes out of an
//! existing mirror, then delete the empty mirror through explicit maintenance
//! cleanup once no mapped object still depends on it. This module does **not**
//! delete mirrors.
//!
//! See `CONTEXT.md` (Raw Git Object Residual, Bridge Mirror, Git Projection
//! Mapping), `docs/adr/0042-retire-persistent-bridge-mirror.md`, and
//! `docs/VERIFICATION_CLEANUP_PLAN.md`.

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use objects::fs_atomic::write_file_atomic;
use serde::{Deserialize, Serialize};
use sley::{
    GitObjectType, ObjectFormat, ObjectId, Repository as SleyRepository,
    plumbing::sley_object::EncodedObject,
};

use crate::git_core::{GitProjectionError, GitProjectionResult, git_err};

/// Directory name under `.heddle/` for Raw Git Object Residuals.
pub const RESIDUALS_DIR_NAME: &str = "git-residuals";

const RESIDUAL_MAGIC: &[u8; 4] = b"HR01";
const TAG_REFS_FILE_NAME: &str = "tag-refs.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidualTagRef {
    pub name: String,
    pub oid: ObjectId,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct TagRefFile {
    refs: BTreeMap<String, String>,
}

/// On-disk Raw Git Object Residual record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidualObject {
    pub oid: ObjectId,
    pub object_format: ObjectFormat,
    pub object_type: GitObjectType,
    /// Canonical Git object body (unframed, uncompressed).
    pub body: Vec<u8>,
}

/// Content-addressed store for Raw Git Object Residuals under a Heddle
/// repository's `.heddle/` directory.
#[derive(Debug, Clone)]
pub struct ResidualStore {
    heddle_dir: PathBuf,
}

impl ResidualStore {
    /// Open (or create on first put) the residual store rooted at `heddle_dir`.
    pub fn open(heddle_dir: impl AsRef<Path>) -> Self {
        Self {
            heddle_dir: heddle_dir.as_ref().to_path_buf(),
        }
    }

    /// Absolute path to `.heddle/git-residuals`.
    pub fn residuals_dir(&self) -> PathBuf {
        self.heddle_dir.join(RESIDUALS_DIR_NAME)
    }

    /// Store a residual, computing its Git oid from `(object_type, body)`.
    ///
    /// Idempotent when the same bytes are written again. Returns the oid.
    pub fn put_residual(
        &self,
        object_format: ObjectFormat,
        object_type: GitObjectType,
        body: impl Into<Vec<u8>>,
    ) -> GitProjectionResult<ObjectId> {
        let body = body.into();
        let encoded = EncodedObject::new(object_type, body.clone());
        let oid = encoded
            .object_id(object_format)
            .map_err(|error| GitProjectionError::Git(error.to_string()))?;
        self.put_residual_verified(oid, object_format, object_type, body)?;
        Ok(oid)
    }

    /// Store a residual at a known oid, verifying the body hashes to that oid.
    pub fn put_residual_verified(
        &self,
        oid: ObjectId,
        object_format: ObjectFormat,
        object_type: GitObjectType,
        body: impl Into<Vec<u8>>,
    ) -> GitProjectionResult<()> {
        if oid.format() != object_format {
            return Err(GitProjectionError::Git(format!(
                "Raw Git Object Residual format mismatch: oid is {}, store tag is {}",
                oid.format().name(),
                object_format.name()
            )));
        }
        let body = body.into();
        let encoded = EncodedObject::new(object_type, body.clone());
        let computed = encoded
            .object_id(object_format)
            .map_err(|error| GitProjectionError::Git(error.to_string()))?;
        if computed != oid {
            return Err(GitProjectionError::Git(format!(
                "Raw Git Object Residual oid mismatch: body hashes to {computed}, expected {oid}"
            )));
        }

        let path = self.residual_path(object_format, &oid);
        if path.is_file() {
            // Content-addressed: identical path implies identical oid. Re-read
            // is optional integrity; overwrite only when bytes differ.
            if let Ok(existing) = fs::read(&path)
                && existing == encode_residual_file(object_type, &body)
            {
                return Ok(());
            }
        }
        write_file_atomic(&path, &encode_residual_file(object_type, &body))?;
        Ok(())
    }

    /// Load a residual by format + oid.
    pub fn get_residual(
        &self,
        object_format: ObjectFormat,
        oid: &ObjectId,
    ) -> GitProjectionResult<Option<ResidualObject>> {
        let path = self.residual_path(object_format, oid);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let (object_type, body) = decode_residual_file(&bytes)?;
        // Defensive re-hash: refuse a residual whose body no longer matches the
        // path oid (bitrot / partial write survivors).
        let encoded = EncodedObject::new(object_type, body.clone());
        let computed = encoded
            .object_id(object_format)
            .map_err(|error| GitProjectionError::Git(error.to_string()))?;
        if &computed != oid {
            return Err(GitProjectionError::Git(format!(
                "corrupt Raw Git Object Residual at {}: body hashes to {computed}, path claims {oid}",
                path.display()
            )));
        }
        Ok(Some(ResidualObject {
            oid: *oid,
            object_format,
            object_type,
            body,
        }))
    }

    /// True when a residual file exists for `oid` under `object_format`.
    pub fn has_residual(
        &self,
        object_format: ObjectFormat,
        oid: &ObjectId,
    ) -> GitProjectionResult<bool> {
        Ok(self.residual_path(object_format, oid).is_file())
    }

    /// List residual oids present for `object_format` (unsorted).
    pub fn list_residual_oids(
        &self,
        object_format: ObjectFormat,
    ) -> GitProjectionResult<Vec<ObjectId>> {
        let format_dir = self.residuals_dir().join(object_format.name());
        if !format_dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut oids = Vec::new();
        for prefix_entry in fs::read_dir(&format_dir)? {
            let prefix_entry = prefix_entry?;
            let prefix_path = prefix_entry.path();
            if !prefix_path.is_dir() {
                continue;
            }
            let prefix = match prefix_entry.file_name().into_string() {
                Ok(name) if name.len() == 2 => name,
                _ => continue,
            };
            for object_entry in fs::read_dir(&prefix_path)? {
                let object_entry = object_entry?;
                if !object_entry.path().is_file() {
                    continue;
                }
                let Ok(rest) = object_entry.file_name().into_string() else {
                    continue;
                };
                let hex = format!("{prefix}{rest}");
                match ObjectId::from_hex(object_format, &hex) {
                    Ok(oid) => oids.push(oid),
                    Err(_) => continue,
                }
            }
        }
        Ok(oids)
    }

    /// Copy one object from a Bridge Mirror (or any Sley repo) into residual
    /// storage when missing. Returns `true` when a residual is now present.
    ///
    /// This is the lazy migration helper for existing `.heddle/git` mirrors:
    /// verification, fsck, export, and write-through can call it as needed.
    /// It never deletes the mirror.
    pub fn migrate_object_from_git_repo(
        &self,
        source: &SleyRepository,
        oid: &ObjectId,
    ) -> GitProjectionResult<bool> {
        let format = source.object_format();
        if self.has_residual(format, oid)? {
            return Ok(true);
        }
        let object = match source.read_object(oid) {
            Ok(object) => object,
            Err(_) => return Ok(false),
        };
        self.put_residual_verified(*oid, format, object.object_type, object.body.clone())?;
        Ok(true)
    }

    /// Capture a non-reconstructable commit and its complete file closure.
    ///
    /// Parent commits are deliberately not traversed: each imported commit is
    /// classified independently, so reconstructable parents remain backed by
    /// native state while non-reconstructable parents are captured as their own
    /// roots. Trees and blobs are captured recursively because the verbatim
    /// commit points at the original tree, not the lossy native translation.
    pub fn capture_commit_closure_from_git_repo(
        &self,
        source: &SleyRepository,
        commit_oid: &ObjectId,
    ) -> GitProjectionResult<()> {
        let format = source.object_format();
        let mut stack = vec![*commit_oid];
        let mut seen = HashSet::new();
        while let Some(oid) = stack.pop() {
            if !seen.insert(oid) {
                continue;
            }
            let object = source.read_object(&oid).map_err(git_err)?;
            self.put_residual_verified(oid, format, object.object_type, object.body.clone())?;
            match object.object_type {
                GitObjectType::Commit => {
                    if oid != *commit_oid {
                        return Err(GitProjectionError::Git(format!(
                            "unexpected commit {oid} inside tree closure for {commit_oid}"
                        )));
                    }
                    let commit =
                        sley::CommitObject::parse_ref(format, &object.body).map_err(git_err)?;
                    stack.push(commit.tree);
                }
                GitObjectType::Tree => {
                    let tree = sley::TreeObject::parse(format, &object.body).map_err(git_err)?;
                    stack.extend(
                        tree.entries
                            .into_iter()
                            .filter(|entry| !entry.is_gitlink())
                            .map(|entry| entry.oid),
                    );
                }
                GitObjectType::Blob => {}
                GitObjectType::Tag => {
                    return Err(GitProjectionError::Git(format!(
                        "unexpected tag {oid} inside tree closure for {commit_oid}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Capture an annotated tag wrapper and any tag-of-tag wrappers beneath it.
    ///
    /// The eventual commit/tree/blob target is captured by its own import
    /// classification. This method preserves only the tag objects that native
    /// markers cannot reconstruct.
    pub fn capture_tag_chain_from_git_repo(
        &self,
        source: &SleyRepository,
        tag_oid: &ObjectId,
    ) -> GitProjectionResult<()> {
        let format = source.object_format();
        let mut oid = *tag_oid;
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(oid) {
                return Err(GitProjectionError::Git(format!(
                    "annotated tag cycle while capturing {tag_oid}"
                )));
            }
            let object = source.read_object(&oid).map_err(git_err)?;
            if object.object_type != GitObjectType::Tag {
                return Ok(());
            }
            self.put_residual_verified(oid, format, GitObjectType::Tag, object.body.clone())?;
            oid = sley::TagObject::parse(format, &object.body)
                .map_err(git_err)?
                .object;
        }
    }

    /// Persist the raw annotated-tag object targeted by an imported tag ref.
    /// Native markers retain the peeled state identity; this sidecar retains the
    /// wrapper identity needed to recreate an annotated ref in a fresh projection.
    pub fn record_tag_ref(&self, name: &str, oid: ObjectId) -> GitProjectionResult<()> {
        let path = self.residuals_dir().join(TAG_REFS_FILE_NAME);
        let mut file = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<TagRefFile>(&bytes).map_err(|error| {
                GitProjectionError::Git(format!(
                    "invalid Raw Git Object Residual tag-ref mapping: {error}"
                ))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => TagRefFile::default(),
            Err(error) => return Err(error.into()),
        };
        file.refs.insert(name.to_string(), oid.to_string());
        let bytes = serde_json::to_vec_pretty(&file).map_err(|error| {
            GitProjectionError::Git(format!(
                "serialize Raw Git Object Residual tag-ref mapping: {error}"
            ))
        })?;
        write_file_atomic(&path, &bytes)?;
        Ok(())
    }

    /// Read the durable imported annotated-tag ref identities.
    pub fn list_tag_refs(&self, format: ObjectFormat) -> GitProjectionResult<Vec<ResidualTagRef>> {
        let path = self.residuals_dir().join(TAG_REFS_FILE_NAME);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let file = serde_json::from_slice::<TagRefFile>(&bytes).map_err(|error| {
            GitProjectionError::Git(format!(
                "invalid Raw Git Object Residual tag-ref mapping: {error}"
            ))
        })?;
        file.refs
            .into_iter()
            .map(|(name, oid)| {
                ObjectId::from_hex(format, &oid)
                    .map(|oid| ResidualTagRef { name, oid })
                    .map_err(|error| {
                        GitProjectionError::Git(format!(
                            "invalid annotated-tag residual oid {oid}: {error}"
                        ))
                    })
            })
            .collect()
    }

    /// Install a residual into a target Git object database.
    ///
    /// Prefer this over reading residual bytes and re-framing in call sites.
    pub fn install_into(
        &self,
        target: &SleyRepository,
        oid: &ObjectId,
    ) -> GitProjectionResult<bool> {
        let format = target.object_format();
        let Some(residual) = self.get_residual(format, oid)? else {
            return Ok(false);
        };
        let written = target
            .write_object(EncodedObject::new(residual.object_type, residual.body))
            .map_err(git_err)?;
        if written != *oid {
            return Err(GitProjectionError::Git(format!(
                "installing Raw Git Object Residual wrote {written}, expected {oid}"
            )));
        }
        Ok(true)
    }

    /// Install a captured non-reconstructable commit and its tree/blob closure.
    /// Returns `false` only when the root residual is absent; a missing dependent
    /// is a hard fidelity error.
    pub fn install_commit_closure_into(
        &self,
        target: &SleyRepository,
        commit_oid: &ObjectId,
    ) -> GitProjectionResult<bool> {
        let format = target.object_format();
        if self.get_residual(format, commit_oid)?.is_none() {
            return Ok(false);
        }
        let mut stack = vec![*commit_oid];
        let mut seen = HashSet::new();
        while let Some(oid) = stack.pop() {
            if !seen.insert(oid) {
                continue;
            }
            let residual = self.get_residual(format, &oid)?.ok_or_else(|| {
                GitProjectionError::Git(format!(
                    "Raw Git Object Residual closure for {commit_oid} is missing reachable object {oid}"
                ))
            })?;
            let written = target
                .write_object(EncodedObject::new(
                    residual.object_type,
                    residual.body.clone(),
                ))
                .map_err(git_err)?;
            if written != oid {
                return Err(GitProjectionError::Git(format!(
                    "installing Raw Git Object Residual wrote {written}, expected {oid}"
                )));
            }
            match residual.object_type {
                GitObjectType::Commit => {
                    if oid != *commit_oid {
                        return Err(GitProjectionError::Git(format!(
                            "unexpected commit {oid} inside residual closure for {commit_oid}"
                        )));
                    }
                    let commit =
                        sley::CommitObject::parse_ref(format, &residual.body).map_err(git_err)?;
                    stack.push(commit.tree);
                }
                GitObjectType::Tree => {
                    let tree = sley::TreeObject::parse(format, &residual.body).map_err(git_err)?;
                    stack.extend(
                        tree.entries
                            .into_iter()
                            .filter(|entry| !entry.is_gitlink())
                            .map(|entry| entry.oid),
                    );
                }
                GitObjectType::Blob => {}
                GitObjectType::Tag => {
                    return Err(GitProjectionError::Git(format!(
                        "unexpected tag {oid} inside residual closure for {commit_oid}"
                    )));
                }
            }
        }
        Ok(true)
    }

    /// Validate a captured commit/tree/blob closure without materializing it.
    /// Returns `false` only when the mapped root residual is absent. Missing or
    /// corrupt dependents are reported as integrity errors.
    pub fn validate_commit_closure(
        &self,
        format: ObjectFormat,
        commit_oid: &ObjectId,
    ) -> GitProjectionResult<bool> {
        if self.get_residual(format, commit_oid)?.is_none() {
            return Ok(false);
        }
        let mut stack = vec![*commit_oid];
        let mut seen = HashSet::new();
        while let Some(oid) = stack.pop() {
            if !seen.insert(oid) {
                continue;
            }
            let residual = self.get_residual(format, &oid)?.ok_or_else(|| {
                GitProjectionError::Git(format!(
                    "Raw Git Object Residual closure for {commit_oid} is missing reachable object {oid}"
                ))
            })?;
            match residual.object_type {
                GitObjectType::Commit => {
                    if oid != *commit_oid {
                        return Err(GitProjectionError::Git(format!(
                            "unexpected commit {oid} inside residual closure for {commit_oid}"
                        )));
                    }
                    let commit =
                        sley::CommitObject::parse_ref(format, &residual.body).map_err(git_err)?;
                    stack.push(commit.tree);
                }
                GitObjectType::Tree => {
                    let tree = sley::TreeObject::parse(format, &residual.body).map_err(git_err)?;
                    stack.extend(
                        tree.entries
                            .into_iter()
                            .filter(|entry| !entry.is_gitlink())
                            .map(|entry| entry.oid),
                    );
                }
                GitObjectType::Blob => {}
                GitObjectType::Tag => {
                    return Err(GitProjectionError::Git(format!(
                        "unexpected tag {oid} inside residual closure for {commit_oid}"
                    )));
                }
            }
        }
        Ok(true)
    }

    /// Install every captured annotated-tag object into a Git object database.
    /// Ref reconciliation remains name/ownership based; this only replaces the
    /// Bridge Mirror's former role as the tag byte warehouse.
    pub fn install_tag_residuals_into(
        &self,
        target: &SleyRepository,
    ) -> GitProjectionResult<usize> {
        let format = target.object_format();
        let mut installed = 0;
        for oid in self.list_residual_oids(format)? {
            let Some(residual) = self.get_residual(format, &oid)? else {
                continue;
            };
            if residual.object_type != GitObjectType::Tag {
                continue;
            }
            let written = target
                .write_object(EncodedObject::new(residual.object_type, residual.body))
                .map_err(git_err)?;
            if written != oid {
                return Err(GitProjectionError::Git(format!(
                    "installing annotated-tag residual wrote {written}, expected {oid}"
                )));
            }
            installed += 1;
        }
        Ok(installed)
    }

    fn residual_path(&self, object_format: ObjectFormat, oid: &ObjectId) -> PathBuf {
        let hex = oid.to_string();
        let (prefix, rest) = hex.split_at(2);
        self.residuals_dir()
            .join(object_format.name())
            .join(prefix)
            .join(rest)
    }
}

/// Maintenance-oriented report about whether a Bridge Mirror looks removable
/// after residual migration.
///
/// Deletion is intentionally **not** performed here. Callers (future
/// `maintenance` / `fsck --repair git`) should only remove `.heddle/git` when
/// this report says the mirror is empty of needed residuals and the operator
/// opts in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeMirrorRetirementStatus {
    /// Path to `.heddle/git` if present.
    pub mirror_path: PathBuf,
    pub mirror_exists: bool,
    /// Residual store root (`.heddle/git-residuals`).
    pub residuals_dir: PathBuf,
    pub residual_count: usize,
    /// True when the mirror directory is absent — retirement already complete
    /// for this checkout.
    pub mirror_already_absent: bool,
    /// Human-oriented note for diagnostics. Not a public CLI contract.
    pub note: String,
}

/// Inspect residual + Bridge Mirror state for future maintenance cleanup.
///
/// Does not delete anything. Full "mirror is empty of needed residuals"
/// reachability analysis remains follow-on work once Git Projection Mapping
/// records residual-vs-reconstructable backing for every mapped oid.
pub fn bridge_mirror_retirement_status(
    heddle_dir: impl AsRef<Path>,
) -> GitProjectionResult<BridgeMirrorRetirementStatus> {
    let heddle_dir = heddle_dir.as_ref();
    let mirror_path = heddle_dir.join("git");
    let store = ResidualStore::open(heddle_dir);
    let residual_count = store.list_residual_oids(ObjectFormat::Sha1)?.len()
        + store.list_residual_oids(ObjectFormat::Sha256)?.len();
    let mirror_exists = mirror_path.exists();
    let note = if !mirror_exists {
        "Bridge Mirror absent; no residual migration cleanup required for .heddle/git.".to_string()
    } else if residual_count == 0 {
        "Bridge Mirror still present and no Raw Git Object Residuals stored yet; \
         migrate lossy objects into residuals before considering mirror deletion."
            .to_string()
    } else {
        format!(
            "Bridge Mirror still present with {residual_count} residual object(s) stored. \
             After all mapped non-reconstructable oids have residuals and no live path \
             requires the mirror, delete .heddle/git via explicit maintenance cleanup."
        )
    };
    Ok(BridgeMirrorRetirementStatus {
        mirror_path: mirror_path.clone(),
        mirror_exists,
        residuals_dir: store.residuals_dir(),
        residual_count,
        mirror_already_absent: !mirror_exists,
        note,
    })
}

/// Resolve lossy Git object bytes for materialization/export:
/// prefer residual, else optional Bridge Mirror, else hard fail.
///
/// When `migrate_from_mirror` is true and the residual was missing, a
/// successful mirror read also writes a residual (lazy migration).
pub fn resolve_lossy_object(
    residual_store: &ResidualStore,
    mirror_repo: Option<&SleyRepository>,
    target_format: ObjectFormat,
    oid: &ObjectId,
    migrate_from_mirror: bool,
) -> GitProjectionResult<ResidualObject> {
    if let Some(residual) = residual_store.get_residual(target_format, oid)? {
        return Ok(residual);
    }

    if let Some(mirror) = mirror_repo
        && mirror.object_format() == target_format
        && let Ok(object) = mirror.read_object(oid)
    {
        if migrate_from_mirror {
            residual_store.put_residual_verified(
                *oid,
                target_format,
                object.object_type,
                object.body.clone(),
            )?;
        }
        return Ok(ResidualObject {
            oid: *oid,
            object_format: target_format,
            object_type: object.object_type,
            body: object.body.clone(),
        });
    }

    Err(GitProjectionError::Git(format!(
        "mapped Git object {oid} is not reconstructable from Heddle state, has no Raw Git Object Residual, \
         and is unavailable from the Bridge Mirror; hard fidelity failure (import with residual capture \
         or restore residual bytes before export/write-through)"
    )))
}

fn encode_residual_file(object_type: GitObjectType, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 1 + body.len());
    out.extend_from_slice(RESIDUAL_MAGIC);
    out.push(object_type_tag(object_type));
    out.extend_from_slice(body);
    out
}

fn decode_residual_file(bytes: &[u8]) -> GitProjectionResult<(GitObjectType, Vec<u8>)> {
    if bytes.len() < 5 || &bytes[..4] != RESIDUAL_MAGIC {
        return Err(GitProjectionError::Git(
            "invalid Raw Git Object Residual file: bad magic or truncated header".into(),
        ));
    }
    let object_type = object_type_from_tag(bytes[4])?;
    Ok((object_type, bytes[5..].to_vec()))
}

fn object_type_tag(object_type: GitObjectType) -> u8 {
    match object_type {
        GitObjectType::Commit => 1,
        GitObjectType::Tree => 2,
        GitObjectType::Blob => 3,
        GitObjectType::Tag => 4,
    }
}

fn object_type_from_tag(tag: u8) -> GitProjectionResult<GitObjectType> {
    match tag {
        1 => Ok(GitObjectType::Commit),
        2 => Ok(GitObjectType::Tree),
        3 => Ok(GitObjectType::Blob),
        4 => Ok(GitObjectType::Tag),
        other => Err(GitProjectionError::Git(format!(
            "invalid Raw Git Object Residual type tag {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use sley::Repository as SleyRepository;

    use super::*;

    #[test]
    fn put_get_round_trip_and_hash_identity() {
        let dir = tempfile::tempdir().unwrap();
        let heddle = dir.path().join(".heddle");
        fs::create_dir_all(&heddle).unwrap();
        let store = ResidualStore::open(&heddle);

        let body = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
author A <a@e> 1 +0000\n\
committer A <a@e> 1 +0000\n\
\nmsg\n";
        let oid = store
            .put_residual(ObjectFormat::Sha1, GitObjectType::Commit, body.to_vec())
            .unwrap();

        assert!(store.has_residual(ObjectFormat::Sha1, &oid).unwrap());
        let got = store
            .get_residual(ObjectFormat::Sha1, &oid)
            .unwrap()
            .expect("residual present");
        assert_eq!(got.oid, oid);
        assert_eq!(got.object_format, ObjectFormat::Sha1);
        assert_eq!(got.object_type, GitObjectType::Commit);
        assert_eq!(got.body, body);

        // Re-put is idempotent.
        let oid2 = store
            .put_residual(ObjectFormat::Sha1, GitObjectType::Commit, body.to_vec())
            .unwrap();
        assert_eq!(oid, oid2);
    }

    #[test]
    fn put_verified_rejects_oid_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let heddle = dir.path().join(".heddle");
        fs::create_dir_all(&heddle).unwrap();
        let store = ResidualStore::open(&heddle);
        let wrong = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        let err = store
            .put_residual_verified(
                wrong,
                ObjectFormat::Sha1,
                GitObjectType::Blob,
                b"hello".to_vec(),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("oid mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn list_residual_oids_finds_stored_objects() {
        let dir = tempfile::tempdir().unwrap();
        let heddle = dir.path().join(".heddle");
        fs::create_dir_all(&heddle).unwrap();
        let store = ResidualStore::open(&heddle);
        let oid = store
            .put_residual(ObjectFormat::Sha1, GitObjectType::Blob, b"x")
            .unwrap();
        let listed = store.list_residual_oids(ObjectFormat::Sha1).unwrap();
        assert_eq!(listed, vec![oid]);
    }

    #[test]
    fn migrate_from_git_repo_copies_object() {
        let dir = tempfile::tempdir().unwrap();
        let mirror_path = dir.path().join("mirror");
        let heddle = dir.path().join(".heddle");
        fs::create_dir_all(&heddle).unwrap();
        let mirror = SleyRepository::init_bare(&mirror_path).unwrap();
        let oid = mirror.write_blob(b"migrated-bytes\n").unwrap();

        let store = ResidualStore::open(&heddle);
        assert!(!store.has_residual(ObjectFormat::Sha1, &oid).unwrap());
        assert!(store.migrate_object_from_git_repo(&mirror, &oid).unwrap());
        assert!(store.has_residual(ObjectFormat::Sha1, &oid).unwrap());
        let residual = store
            .get_residual(ObjectFormat::Sha1, &oid)
            .unwrap()
            .unwrap();
        assert_eq!(residual.body, b"migrated-bytes\n");
    }

    #[test]
    fn install_into_target_repo() {
        let dir = tempfile::tempdir().unwrap();
        let heddle = dir.path().join(".heddle");
        let target_path = dir.path().join("target");
        fs::create_dir_all(&heddle).unwrap();
        let store = ResidualStore::open(&heddle);
        let oid = store
            .put_residual(ObjectFormat::Sha1, GitObjectType::Blob, b"install-me")
            .unwrap();
        let target = SleyRepository::init_bare(&target_path).unwrap();
        assert!(target.read_object(&oid).is_err());
        assert!(store.install_into(&target, &oid).unwrap());
        let object = target.read_object(&oid).unwrap();
        assert_eq!(object.body, b"install-me");
    }

    #[test]
    fn resolve_lossy_prefers_residual_over_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let heddle = dir.path().join(".heddle");
        let mirror_path = dir.path().join("mirror");
        fs::create_dir_all(&heddle).unwrap();
        let store = ResidualStore::open(&heddle);
        let residual_oid = store
            .put_residual(ObjectFormat::Sha1, GitObjectType::Blob, b"from-residual")
            .unwrap();
        let mirror = SleyRepository::init_bare(&mirror_path).unwrap();
        // Different body under a different oid so mirror is not the source.
        let _ = mirror.write_blob(b"from-mirror").unwrap();

        let resolved = resolve_lossy_object(
            &store,
            Some(&mirror),
            ObjectFormat::Sha1,
            &residual_oid,
            false,
        )
        .unwrap();
        assert_eq!(resolved.body, b"from-residual");
    }

    #[test]
    fn resolve_lossy_hard_fails_when_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let heddle = dir.path().join(".heddle");
        fs::create_dir_all(&heddle).unwrap();
        let store = ResidualStore::open(&heddle);
        let missing = ObjectId::from_hex(
            ObjectFormat::Sha1,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        )
        .unwrap();
        let err =
            resolve_lossy_object(&store, None, ObjectFormat::Sha1, &missing, false).unwrap_err();
        assert!(
            err.to_string().contains("hard fidelity failure"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn bridge_mirror_retirement_status_reports_absence() {
        let dir = tempfile::tempdir().unwrap();
        let heddle = dir.path().join(".heddle");
        fs::create_dir_all(&heddle).unwrap();
        let status = bridge_mirror_retirement_status(&heddle).unwrap();
        assert!(status.mirror_already_absent);
        assert!(!status.mirror_exists);
        assert_eq!(status.residual_count, 0);
    }
}
