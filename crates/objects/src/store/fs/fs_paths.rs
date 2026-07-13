// SPDX-License-Identifier: Apache-2.0
//! Path helpers for FsStore.

use std::path::{Path, PathBuf};

use crate::object::{ActionId, ContentHash, StateAttachmentId, StateId};

pub(super) fn objects_dir(root: &Path) -> PathBuf {
    root.join("objects")
}

pub(super) fn blobs_dir(root: &Path) -> PathBuf {
    objects_dir(root).join("blobs")
}

pub(super) fn trees_dir(root: &Path) -> PathBuf {
    objects_dir(root).join("trees")
}

pub(super) fn states_dir(root: &Path) -> PathBuf {
    objects_dir(root).join("states")
}

pub(super) fn actions_dir(root: &Path) -> PathBuf {
    root.join("actions")
}

pub(super) fn hash_path(base_dir: &Path, hash: &ContentHash) -> PathBuf {
    let hex = hash.to_hex();
    let (prefix, rest) = hex.split_at(2);
    base_dir.join(prefix).join(rest)
}

pub(super) fn state_path(root: &Path, id: &StateId) -> PathBuf {
    states_dir(root).join(format!("{}.state", id.to_string_full()))
}

pub(super) fn state_attachments_dir(root: &Path, state: &StateId) -> PathBuf {
    objects_dir(root)
        .join("state-attachments")
        .join(state.to_string_full())
}

pub(super) fn state_attachment_path(
    root: &Path,
    state: &StateId,
    id: &StateAttachmentId,
) -> PathBuf {
    state_attachments_dir(root, state).join(format!("{}.attachment", id.as_hash().to_hex()))
}

pub(super) fn state_attachment_index_path(root: &Path, state: &StateId) -> PathBuf {
    objects_dir(root)
        .join("state-attachment-index")
        .join(format!("{}.msgpack", state.to_string_full()))
}

pub(super) fn action_path(root: &Path, id: &ActionId) -> PathBuf {
    actions_dir(root).join(format!("{}.action", id.as_hash().to_hex()))
}

pub(super) fn packs_dir(root: &Path) -> PathBuf {
    root.join("packs")
}

pub(super) fn redactions_dir(root: &Path) -> PathBuf {
    root.join("redactions")
}

pub(super) fn redaction_path(root: &Path, blob: &ContentHash) -> PathBuf {
    redactions_dir(root).join(format!("{}.bin", blob.to_hex()))
}

pub(super) fn state_visibility_dir(root: &Path) -> PathBuf {
    root.join("visibility")
}

pub(super) fn state_visibility_path(root: &Path, state: &StateId) -> PathBuf {
    state_visibility_dir(root).join(format!("{}.bin", state.to_string_full()))
}
