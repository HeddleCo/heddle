// SPDX-License-Identifier: Apache-2.0
//! gix interop helpers at repository boundaries while gix APIs remain in use.

use crate::framing::object_id_for_content;
#[cfg(feature = "gix-interop")]
use crate::id::{from_gix, to_gix};
use crate::id::ObjectId;
use crate::kind::ObjectKind;
use crate::{GitRepo, Result};

/// SHA-1 blob object id for `bytes`.
pub fn blob_object_id(bytes: &[u8]) -> Result<ObjectId> {
    object_id_for_content("blob", bytes)
}

/// SHA-1 commit object id for bare commit **content** bytes (no framing).
pub fn commit_object_id(content: &[u8]) -> Result<ObjectId> {
    object_id_for_content("commit", content)
}

/// Returns `true` when `oid` resolves to a commit object.
///
/// Any read error is treated as "not a commit", matching the ingest walker.
pub fn is_commit(repo: &GitRepo, oid: &ObjectId) -> bool {
    matches!(repo.read_object_kind(oid), Some(ObjectKind::Commit))
}

/// Read the object kind for a substrate object id via the sley object database.
pub fn read_object_kind(repo: &GitRepo, oid: &ObjectId) -> Result<Option<ObjectKind>> {
    Ok(repo.read_object_kind(oid))
}

#[cfg(feature = "gix-interop")]
/// Read the object kind for a gix object id via the sley object database.
pub fn read_gix_object_kind(repo: &GitRepo, oid: gix::hash::ObjectId) -> Result<Option<ObjectKind>> {
    read_object_kind(repo, &from_gix(oid)?)
}

#[cfg(feature = "gix-interop")]
/// Returns `true` when `oid` resolves to a commit object (gix id boundary).
pub fn gix_is_commit(repo: &GitRepo, oid: gix::hash::ObjectId) -> bool {
    from_gix(oid)
        .ok()
        .is_some_and(|substrate_oid| is_commit(repo, &substrate_oid))
}

#[cfg(feature = "gix-interop")]
/// SHA-1 blob object id for `bytes`, as a gix [`ObjectId`](gix::hash::ObjectId).
pub fn gix_blob_object_id(bytes: &[u8]) -> Result<gix::hash::ObjectId> {
    Ok(to_gix(&blob_object_id(bytes)?)?)
}

#[cfg(feature = "gix-interop")]
/// SHA-1 commit object id for bare commit content bytes (gix id boundary).
pub fn gix_commit_object_id(content: &[u8]) -> Result<gix::hash::ObjectId> {
    Ok(to_gix(&commit_object_id(content)?)?)
}

#[cfg(feature = "gix-interop")]
/// Map a gix repository's object hash kind to the substrate [`ObjectFormat`].
pub fn gix_object_format(repo: &gix::Repository) -> sley_core::ObjectFormat {
    if repo.object_hash() == gix::hash::Kind::Sha1 {
        sley_core::ObjectFormat::Sha1
    } else {
        sley_core::ObjectFormat::Sha256
    }
}

