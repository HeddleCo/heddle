// SPDX-License-Identifier: Apache-2.0
use objects::{
    object::{State, Tree},
    store::ObjectStore,
};

use crate::{ObjectData, ObjectId, ObjectRequest, ObjectType, ProtocolError, Result};

/// Maximum redaction sidecar blob accepted from the pull stream, per blob.
///
/// Redaction sidecars are signed range lists for a single blob — orders of
/// magnitude smaller than the blob payload they describe. 64 MiB bounds the
/// server-controlled receive buffer on the pull stream (the same
/// unbounded-allocation OOM class #366 closed for the native pack/index
/// buffers) while leaving generous headroom for any legitimate record.
pub const MAX_RECEIVED_REDACTIONS_BLOB_SIZE: u64 = 64 * 1024 * 1024;

/// Maximum state-visibility sidecar blob accepted from the pull stream, per
/// state.
///
/// State-visibility sidecars are per-state tier records, not object payloads.
/// 64 MiB bounds this second server-controlled pull-stream buffer with the
/// same receive-side cap.
pub const MAX_RECEIVED_STATE_VISIBILITY_BLOB_SIZE: u64 = 64 * 1024 * 1024;

/// Reject a received per-object transfer sidecar blob whose length exceeds
/// `max_bytes`, before it is handed to the repository accept path.
///
/// Sidecar blobs (redaction, state-visibility) arrive as single
/// server-controlled buffers on the pull stream. This is the single-shot
/// analogue of [`crate::receive_pack_chunk`]'s running-total check: it bounds
/// the in-memory allocation a hostile or buggy server can drive on the receive
/// side. `kind` names the blob in the error (e.g. `"redactions"`).
pub fn check_received_transfer_blob_size(
    blob_len: usize,
    max_bytes: u64,
    kind: &str,
) -> Result<()> {
    let len = u64::try_from(blob_len).map_err(|_| {
        ProtocolError::InvalidState(format!("{kind} blob length does not fit in u64"))
    })?;
    if len > max_bytes {
        return Err(ProtocolError::InvalidState(format!(
            "{kind} blob exceeds receive size limit: {len} bytes (max {max_bytes})"
        )));
    }
    Ok(())
}

#[allow(dead_code)]
pub fn chunk_count(object_size: usize, chunk_size: usize) -> usize {
    if object_size == 0 || chunk_size == 0 {
        return 0;
    }
    object_size.div_ceil(chunk_size)
}

#[allow(dead_code)]
pub fn chunk_bounds(
    object_size: usize,
    chunk_size: usize,
    chunk_index: usize,
) -> Option<(usize, usize)> {
    if chunk_size == 0 {
        return None;
    }

    let start = chunk_index.checked_mul(chunk_size)?;
    if start >= object_size {
        return None;
    }
    let end = (start + chunk_size).min(object_size);
    Some((start, end - start))
}

#[allow(dead_code)]
pub fn chunk_offset(chunk_index: usize, chunk_size: usize) -> Option<usize> {
    chunk_index.checked_mul(chunk_size)
}

pub fn load_requested_object(store: &impl ObjectStore, req: &ObjectRequest) -> Result<ObjectData> {
    // Note on sidecar objects: redactions and state visibility are keyed by
    // ids that also identify primary objects. `load_requested_object`
    // resolves blob-vs-tree or state by id shape/probe; it cannot
    // disambiguate a sidecar request by ObjectId alone. Callers that need to
    // fetch a sidecar must use `load_object_data` with an explicit object
    // type.
    let (obj_type, data) = match &req.id {
        ObjectId::Hash(hash) => {
            if let Some(blob) = store.get_blob(hash)? {
                (ObjectType::Blob, blob.content().to_vec())
            } else if let Some(tree) = store.get_tree(hash)? {
                (ObjectType::Tree, rmp_serde::to_vec_named(&tree)?)
            } else {
                return Err(ProtocolError::ObjectNotFound(hash.to_hex()));
            }
        }
        ObjectId::ChangeId(change_id) => {
            let state = store
                .get_state(change_id)?
                .ok_or_else(|| ProtocolError::ObjectNotFound(change_id.to_string()))?;
            (ObjectType::State, rmp_serde::to_vec_named(&state)?)
        }
    };

    Ok(ObjectData {
        id: req.id.clone(),
        obj_type,
        data,
        is_delta: false,
    })
}

pub fn load_object_data(
    store: &impl ObjectStore,
    id: &ObjectId,
    obj_type: ObjectType,
) -> Result<ObjectData> {
    let data = match (id, obj_type) {
        (ObjectId::Hash(hash), ObjectType::Blob) => store
            .get_blob(hash)?
            .ok_or_else(|| ProtocolError::ObjectNotFound(hash.to_hex()))?
            .content()
            .to_vec(),
        (ObjectId::Hash(hash), ObjectType::Tree) => {
            let tree = store
                .get_tree(hash)?
                .ok_or_else(|| ProtocolError::ObjectNotFound(hash.to_hex()))?;
            rmp_serde::to_vec_named(&tree)?
        }
        (ObjectId::ChangeId(change_id), ObjectType::State) => {
            let state = store
                .get_state(change_id)?
                .ok_or_else(|| ProtocolError::ObjectNotFound(change_id.to_string()))?;
            rmp_serde::to_vec_named(&state)?
        }
        (ObjectId::Hash(hash), ObjectType::Redaction) => store
            .get_redactions_bytes_for_blob(hash)?
            .ok_or_else(|| ProtocolError::ObjectNotFound(hash.to_hex()))?,
        (ObjectId::ChangeId(change_id), ObjectType::StateVisibility) => store
            .get_state_visibility_bytes_for_state(change_id)?
            .ok_or_else(|| ProtocolError::ObjectNotFound(change_id.to_string_full()))?,
        _ => {
            return Err(ProtocolError::InvalidState(
                "object id/type mismatch".to_string(),
            ));
        }
    };

    Ok(ObjectData {
        id: id.clone(),
        obj_type,
        data,
        is_delta: false,
    })
}

pub fn store_received_object(store: &impl ObjectStore, data: &ObjectData) -> Result<()> {
    match (&data.id, data.obj_type) {
        (ObjectId::Hash(hash), ObjectType::Blob) => {
            store.put_blob_bytes_with_hash(&data.data, *hash)?;
        }
        (ObjectId::Hash(hash), ObjectType::Tree) => {
            let tree: Tree = rmp_serde::from_slice(&data.data)?;
            tree.validate().map_err(|error| {
                ProtocolError::InvalidState(format!("invalid tree object: {error}"))
            })?;
            if &tree.hash() != hash {
                return Err(ProtocolError::InvalidState(
                    "tree hash mismatch".to_string(),
                ));
            }
            store.put_tree_serialized(&data.data, *hash)?;
        }
        (ObjectId::ChangeId(change_id), ObjectType::State) => {
            let state: State = rmp_serde::from_slice(&data.data)?;
            if state.change_id != *change_id {
                return Err(ProtocolError::InvalidState(format!(
                    "ChangeId mismatch: expected {}, got {}",
                    change_id, state.change_id
                )));
            }
            store.put_state_serialized(&data.data, *change_id)?;
        }
        (_, ObjectType::Redaction) => {
            // Redactions ship signed and need verification before any
            // bytes hit the sidecar. Refuse here so callers route via
            // `Repository::accept_wire_redactions` instead of silently
            // landing an unverified record.
            return Err(ProtocolError::InvalidState(
                "Redaction objects must be persisted via Repository::accept_wire_redactions, \
                 not store_received_object — signature verification is required"
                    .to_string(),
            ));
        }
        (_, ObjectType::StateVisibility) => {
            // State visibility must be validated and normalized at the
            // Repository boundary (`put_state_visibility` enforces
            // public-by-absence). Refuse raw sidecar writes here.
            return Err(ProtocolError::InvalidState(
                "StateVisibility objects must be persisted via Repository::accept_wire_state_visibility, \
                 not store_received_object — sidecar validation is required"
                    .to_string(),
            ));
        }
        _ => {
            return Err(ProtocolError::InvalidState(
                "object id/type mismatch".to_string(),
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_count_rounds_up() {
        assert_eq!(chunk_count(0, 64), 0);
        assert_eq!(chunk_count(1, 64), 1);
        assert_eq!(chunk_count(64, 64), 1);
        assert_eq!(chunk_count(65, 64), 2);
    }

    #[test]
    fn test_chunk_bounds_returns_ranges() {
        assert_eq!(chunk_bounds(100, 32, 0), Some((0, 32)));
        assert_eq!(chunk_bounds(100, 32, 2), Some((64, 32)));
        assert_eq!(chunk_bounds(100, 32, 3), Some((96, 4)));
        assert_eq!(chunk_bounds(100, 32, 4), None);
        assert_eq!(chunk_bounds(100, 0, 0), None);
    }

    #[test]
    fn test_chunk_offset_returns_position() {
        assert_eq!(chunk_offset(0, 64), Some(0));
        assert_eq!(chunk_offset(3, 64), Some(192));
        assert_eq!(chunk_offset(usize::MAX, 2), None);
    }

    #[test]
    fn received_transfer_blob_at_limit_is_accepted() {
        check_received_transfer_blob_size(8, 8, "redactions").unwrap();
    }

    #[test]
    fn received_transfer_blob_over_limit_is_rejected() {
        let error = check_received_transfer_blob_size(9, 8, "redactions").unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("redactions blob exceeds receive size limit"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("9 bytes (max 8)"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn received_transfer_blob_caps_are_enforced_against_production_limits() {
        check_received_transfer_blob_size(
            MAX_RECEIVED_REDACTIONS_BLOB_SIZE as usize,
            MAX_RECEIVED_REDACTIONS_BLOB_SIZE,
            "redactions",
        )
        .unwrap();
        check_received_transfer_blob_size(
            MAX_RECEIVED_STATE_VISIBILITY_BLOB_SIZE as usize,
            MAX_RECEIVED_STATE_VISIBILITY_BLOB_SIZE,
            "state-visibility",
        )
        .unwrap();
    }
}
