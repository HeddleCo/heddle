// SPDX-License-Identifier: Apache-2.0
#![deny(clippy::cast_possible_truncation)]

//! Byte-exact git commit object serialization from a Heddle [`State`] (#566).
//!
//! Reconstructs the exact bytes `git cat-file commit <sha>` prints from the
//! de-lossy fidelity fields #565 captured, so that re-framing (§0) and
//! SHA-1-hashing the result reproduces the *original* commit's object id. This
//! is the consumer that makes #565's fields load-bearing and the step that lets
//! the git mirror be eliminated (#568): a commit can be rebuilt from Heddle
//! state alone — no stored git object.
//!
//! The wire format is specced byte-for-byte in
//! `.heddleco-orchestrator/briefs/spike-566-serializer-format.md`; the `§N`
//! references below point into it. Tag-object reconstruction
//! (`reconstruct_tag_bytes`) is deferred to #575, where annotated tags become
//! first-class content-addressed objects; lightweight tags need no object (just
//! a ref at the commit).

use objects::{
    object::{AnnotatedTag, State},
    store::{ObjectStore, StoreError},
};
use repo::Repository as HeddleRepository;
use sley::{GitObjectType, ObjectFormat, ObjectId, Repository as SleyRepository};

use crate::{
    git_core::{GitProjection, GitProjectionError, GitProjectionResult, SyncMapping, git_err},
    git_export::export_tree,
};

/// Frame an object's content for hashing per spike §0:
/// `<kind> <ascii-decimal-len>\0<content>`. A git object's id is the SHA-1 of
/// THIS buffer — never of the bare content (`git cat-file` strips the framing).
/// `<len>` is the byte length of `content` (after all folding/newlines), with no
/// leading zeros.
pub fn frame_git_object(kind: &str, content: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(kind.len() + 2 + 20 + content.len());
    framed.extend_from_slice(kind.as_bytes());
    framed.push(b' ');
    framed.extend_from_slice(content.len().to_string().as_bytes());
    framed.push(0);
    framed.extend_from_slice(content);
    framed
}

/// The git object id (SHA-1) of a commit whose reconstructed content bytes are
/// `content`: frame per §0, then hash. Equals the original commit SHA exactly
/// when `content` is byte-identical to the original object.
pub fn commit_object_id(content: &[u8]) -> ObjectId {
    sley_core::object_id_for_bytes(ObjectFormat::Sha1, "commit", content)
        .expect("SHA-1 commit object id over in-memory bytes cannot fail")
}

/// Reconstruct the byte-exact git commit object **content** (the bytes
/// `git cat-file commit` prints, WITHOUT the §0 framing) for `state`.
///
/// `repo` is any writable sley repo: the git tree OID is resolved by re-exporting
/// `state.tree` through [`export_tree`] (git trees are content-addressed, so the
/// resulting OID is independent of which repo it is written into — the round-trip
/// fidelity gate proves this path reproduces the original tree SHA). Parent OIDs
/// come from the import `mapping` (`StateId` → original git OID), in
/// `state.parents` order — order is part of a commit's identity (§1.2).
pub fn reconstruct_commit_bytes(
    heddle_repo: &HeddleRepository,
    repo: &SleyRepository,
    mapping: &SyncMapping,
    state: &State,
) -> GitProjectionResult<Vec<u8>> {
    let tree_oid = export_tree(heddle_repo, repo, &state.tree)?;
    let parent_oids = mapped_git_parents(state, mapping)?;
    heddle_git_serialize::serialize_commit(state, &tree_oid, &parent_oids).map_err(
        |heddle_git_serialize::SerializeError::InvalidObject(message)| {
            GitProjectionError::Store(StoreError::InvalidObject(message))
        },
    )
}

/// Native adoption roots Git history at Heddle's synthetic hosted seed. That
/// state has no Git commit, so it is absent from the projected parent list.
pub fn mapped_git_parents(
    state: &State,
    mapping: &SyncMapping,
) -> GitProjectionResult<Vec<ObjectId>> {
    let hosted_seed =
        objects::object::thread_replication::initial_base::synthetic_initial_base()?.id();
    state
        .parents
        .iter()
        .filter(|parent| **parent != hosted_seed)
        .map(|parent| {
            mapping
                .get_git(parent)
                .ok_or(GitProjectionError::StateNotFound(*parent))
        })
        .collect()
}

/// Frame + write a reconstructed commit object's `content` bytes into `repo`'s
/// object database, returning its git OID — the SHA-1 of the framed object (§0),
/// equal to the original commit's id exactly when `content` is byte-identical to
/// the original.
///
/// This is the write side of export-from-state (#567): export regenerates each
/// commit object from Heddle state and writes it here. Idempotent: sley's object
/// writer hashes first and no-ops when the object already exists.
pub fn write_commit_object(repo: &SleyRepository, content: &[u8]) -> GitProjectionResult<ObjectId> {
    repo.write_raw_object(GitObjectType::Commit, content.to_vec())
        .map_err(git_err)
}

/// Reconstruct the exact unframed bytes of a stored annotated Git tag.
pub fn reconstruct_tag_bytes(tag: &AnnotatedTag) -> Vec<u8> {
    tag.body().to_vec()
}

/// Frame, hash, and write a reconstructed annotated tag through sley's raw sink.
pub fn write_tag_object(
    repo: &SleyRepository,
    tag: &AnnotatedTag,
) -> GitProjectionResult<ObjectId> {
    if tag.git_format().map_err(|error| {
        GitProjectionError::Git(format!("invalid stored annotated tag: {error}"))
    })? != repo.object_format()
    {
        return Err(GitProjectionError::Git(
            "stored annotated tag uses a different Git object format".to_string(),
        ));
    }
    let oid = repo
        .write_raw_object(GitObjectType::Tag, reconstruct_tag_bytes(tag))
        .map_err(git_err)?;
    let expected = tag
        .git_oid()
        .map_err(|error| GitProjectionError::Git(error.to_string()))?;
    if oid != expected {
        return Err(GitProjectionError::Git(format!(
            "reconstructed annotated tag wrote {oid}, expected {expected}"
        )));
    }
    Ok(oid)
}

impl GitProjection<'_> {
    /// Open an ephemeral writable sley repo for tree-OID resolution.
    pub fn reconstruction_repo(&mut self) -> GitProjectionResult<SleyRepository> {
        self.init_scratch_repo()?;
        self.open_git_repo()
    }

    /// Reconstruct the byte-exact commit content for `state` against `repo` (see
    /// [`reconstruct_commit_bytes`]), using the import-built mapping for
    /// parent OIDs.
    pub fn reconstruct_commit_bytes(
        &self,
        repo: &SleyRepository,
        state: &State,
    ) -> GitProjectionResult<Vec<u8>> {
        reconstruct_commit_bytes(self.heddle_repo, repo, &self.mapping, state)
    }

    /// Reconstruct `state`'s commit object from Heddle state and WRITE it into
    /// `repo`'s object database, returning its git OID (see [`write_commit_object`]).
    /// The export's commit-minting step (#567): the object is regenerated from
    /// state, so it lands at the original SHA without a second object warehouse.
    pub fn reconstruct_and_write_commit(
        &self,
        repo: &SleyRepository,
        state: &State,
    ) -> GitProjectionResult<ObjectId> {
        let content = self.reconstruct_commit_bytes(repo, state)?;
        write_commit_object(repo, &content)
    }

    /// Reconstruct the commit currently mapped to the git object `sha` (40-hex),
    /// or `None` if no Heddle state maps to it. Convenience for callers keyed by
    /// the original git OID — e.g. the #566 conformance gate, which compares the
    /// reconstruction of each original commit against its captured golden bytes.
    pub fn reconstruct_commit_for_git_sha(
        &self,
        repo: &SleyRepository,
        sha: &str,
    ) -> GitProjectionResult<Option<Vec<u8>>> {
        let oid = ObjectId::from_hex(ObjectFormat::Sha1, sha).map_err(git_err)?;
        let Some(state_id) = self.mapping.get_heddle(oid) else {
            return Ok(None);
        };
        let Some(state) = self.heddle_repo.store().get_state(&state_id)? else {
            return Ok(None);
        };
        Ok(Some(reconstruct_commit_bytes(
            self.heddle_repo,
            repo,
            &self.mapping,
            &state,
        )?))
    }

    /// Reconstruct the commit mapped to git object `sha` and WRITE it into `repo`,
    /// returning the written OID (or `None` if no Heddle state maps to `sha`).
    /// Combines [`Self::reconstruct_commit_for_git_sha`] with the odb write so the
    /// #567 export-from-state path is exercisable against an arbitrary repo —
    /// notably a FRESH one that never received the verbatim imported bytes, which
    /// is how the export gate proves the object is regenerated from state, not
    /// copied from retained Git storage.
    pub fn reconstruct_and_write_commit_for_git_sha(
        &self,
        repo: &SleyRepository,
        sha: &str,
    ) -> GitProjectionResult<Option<ObjectId>> {
        let oid = ObjectId::from_hex(ObjectFormat::Sha1, sha).map_err(git_err)?;
        let Some(state_id) = self.mapping.get_heddle(oid) else {
            return Ok(None);
        };
        let Some(state) = self.heddle_repo.store().get_state(&state_id)? else {
            return Ok(None);
        };
        Ok(Some(self.reconstruct_and_write_commit(repo, &state)?))
    }
}

#[cfg(test)]
mod tests {
    use super::frame_git_object;

    #[test]
    fn frame_prepends_kind_len_nul() {
        assert_eq!(frame_git_object("commit", b"abc"), b"commit 3\0abc");
        assert_eq!(frame_git_object("commit", b""), b"commit 0\0");
    }
}
