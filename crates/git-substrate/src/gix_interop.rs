// SPDX-License-Identifier: Apache-2.0
//! Object-id and kind helpers backed by the sley object database.

use crate::framing::object_id_for_content;
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