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

/// Envelope headroom added on top of the largest legitimate sidecar blob when
/// sizing the pull-stream gRPC decode limit. Covers the protobuf fields that
/// wrap a max-size sidecar blob in a `PullMessage` — the oneof tag, the
/// `blob_hash`/`state_id` string, and the transfer checkpoint — none of which
/// approach a MiB. Kept deliberately tight (not generously round): the decode
/// limit is a per-*message* bound, so the unavoidable slop above the precise
/// per-blob cap equals this headroom. Minimizing it keeps the worst-case
/// attacker-forced allocation within ~1 MiB of the 64 MiB blob cap; the exact
/// per-blob cap for that residual window is enforced by the post-decode
/// `check_received_transfer_blob_size` defense-in-depth check.
const PULL_DECODE_ENVELOPE_HEADROOM: u64 = 1024 * 1024;

const fn max_u64(a: u64, b: u64) -> u64 {
    if a > b { a } else { b }
}

/// Inbound gRPC decode limit for the pull stream (tonic's
/// `max_decoding_message_size`).
///
/// This is the *load-bearing* bound on the single-shot, server-controlled
/// sidecar allocation. tonic refuses to decode an inbound `PullMessage` larger
/// than this, so an oversized `redactions_blob` / `state_visibility_blob` is
/// rejected at the decode boundary *before* its `Vec<u8>` is materialized.
/// [`check_received_transfer_blob_size`] is retained as a cheap post-decode
/// defense-in-depth check, but the allocation itself is bounded here.
///
/// Sized to the largest legitimate single message — a sidecar transfer carrying
/// a max-size blob ([`MAX_RECEIVED_REDACTIONS_BLOB_SIZE`] /
/// [`MAX_RECEIVED_STATE_VISIBILITY_BLOB_SIZE`], 64 MiB) — plus
/// [`PULL_DECODE_ENVELOPE_HEADROOM`]. Native pack chunks share this stream but
/// are bounded far below this by the negotiated chunk size, so they are
/// unaffected.
pub const MAX_PULL_DECODE_MESSAGE_SIZE: usize = (max_u64(
    MAX_RECEIVED_REDACTIONS_BLOB_SIZE,
    MAX_RECEIVED_STATE_VISIBILITY_BLOB_SIZE,
) + PULL_DECODE_ENVELOPE_HEADROOM) as usize;

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
        ObjectId::StateId(state_id) => {
            let state = store
                .get_state(state_id)?
                .ok_or_else(|| ProtocolError::ObjectNotFound(state_id.to_string()))?;
            (ObjectType::State, rmp_serde::to_vec_named(&state)?)
        }
        ObjectId::StateAttachment { state, id } => {
            let attachment = store
                .get_state_attachment(state, id)?
                .ok_or_else(|| ProtocolError::ObjectNotFound(id.to_string()))?;
            (
                ObjectType::StateAttachment,
                rmp_serde::to_vec_named(&attachment)?,
            )
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
        (ObjectId::StateId(state_id), ObjectType::State) => {
            let state = store
                .get_state(state_id)?
                .ok_or_else(|| ProtocolError::ObjectNotFound(state_id.to_string()))?;
            rmp_serde::to_vec_named(&state)?
        }
        (ObjectId::Hash(hash), ObjectType::Redaction) => store
            .get_redactions_bytes_for_blob(hash)?
            .ok_or_else(|| ProtocolError::ObjectNotFound(hash.to_hex()))?,
        (ObjectId::StateId(state_id), ObjectType::StateVisibility) => store
            .get_state_visibility_bytes_for_state(state_id)?
            .ok_or_else(|| ProtocolError::ObjectNotFound(state_id.to_string_full()))?,
        (ObjectId::StateAttachment { state, id }, ObjectType::StateAttachment) => {
            let attachment = store
                .get_state_attachment(state, id)?
                .ok_or_else(|| ProtocolError::ObjectNotFound(id.to_string()))?;
            rmp_serde::to_vec_named(&attachment)?
        }
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
        (ObjectId::StateId(state_id), ObjectType::State) => {
            let state: State = rmp_serde::from_slice(&data.data)?;
            if state.id() != *state_id {
                return Err(ProtocolError::InvalidState(format!(
                    "StateId mismatch: expected {state_id}, computed {}",
                    state.id()
                )));
            }
            store.put_state_serialized(&data.data, *state_id)?;
        }
        (ObjectId::StateAttachment { state, id }, ObjectType::StateAttachment) => {
            let attachment: objects::object::StateAttachment = rmp_serde::from_slice(&data.data)?;
            if attachment.state_id != *state || attachment.id() != *id {
                return Err(ProtocolError::InvalidState(
                    "state attachment id mismatch".to_string(),
                ));
            }
            store.put_state_attachment(&attachment)?;
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
    use objects::{
        object::{Attribution, Blob, ContentHash, Principal, State, Tree, TreeEntry},
        store::{FsStore, ObjectStore},
    };
    use tempfile::TempDir;

    use super::*;

    fn create_test_store() -> (TempDir, FsStore) {
        let temp = TempDir::new().unwrap();
        let store = FsStore::new(temp.path().join(".heddle"));
        store.init().unwrap();
        (temp, store)
    }

    fn test_attribution() -> Attribution {
        Attribution::human(Principal::new("Wire Tester", "wire@example.com"))
    }

    #[test]
    fn primary_objects_roundtrip_through_wire_data() {
        let (_source_temp, source) = create_test_store();
        let (_dest_temp, dest) = create_test_store();

        let blob = Blob::from("wire transfer blob\n");
        let blob_hash = source.put_blob(&blob).unwrap();
        let tree = Tree::from_entries(vec![TreeEntry::file("lib.rs", blob_hash, false).unwrap()]);
        let tree_hash = source.put_tree(&tree).unwrap();
        let state = State::new(tree_hash, Vec::new(), test_attribution())
            .with_intent("exercise wire transfer");
        source.put_state(&state).unwrap();

        let blob_data = load_requested_object(
            &source,
            &ObjectRequest {
                id: ObjectId::Hash(blob_hash),
                have_base: None,
            },
        )
        .unwrap();
        assert_eq!(blob_data.obj_type, ObjectType::Blob);
        assert_eq!(blob_data.data, blob.content());
        store_received_object(&dest, &blob_data).unwrap();
        assert_eq!(
            dest.get_blob(&blob_hash).unwrap().unwrap().content(),
            blob.content()
        );

        let tree_data = load_requested_object(
            &source,
            &ObjectRequest {
                id: ObjectId::Hash(tree_hash),
                have_base: None,
            },
        )
        .unwrap();
        assert_eq!(tree_data.obj_type, ObjectType::Tree);
        assert_eq!(
            rmp_serde::from_slice::<Tree>(&tree_data.data).unwrap(),
            tree
        );
        store_received_object(&dest, &tree_data).unwrap();
        assert_eq!(dest.get_tree(&tree_hash).unwrap().unwrap(), tree);

        let state_data = load_requested_object(
            &source,
            &ObjectRequest {
                id: ObjectId::StateId(state.state_id),
                have_base: None,
            },
        )
        .unwrap();
        assert_eq!(state_data.obj_type, ObjectType::State);
        assert_eq!(
            rmp_serde::from_slice::<State>(&state_data.data).unwrap(),
            state
        );
        store_received_object(&dest, &state_data).unwrap();
        assert_eq!(
            dest.get_state(&state.state_id).unwrap().unwrap().state_id,
            state.state_id
        );
    }

    #[test]
    fn load_object_data_reports_missing_and_id_type_mismatch_errors() {
        let (_temp, store) = create_test_store();
        let missing_hash = ContentHash::from_bytes([7; 32]);
        let missing_state = objects::object::StateId::from_bytes([9; 32]);

        let missing = load_requested_object(
            &store,
            &ObjectRequest {
                id: ObjectId::Hash(missing_hash),
                have_base: None,
            },
        )
        .unwrap_err();
        assert!(
            matches!(missing, ProtocolError::ObjectNotFound(id) if id == missing_hash.to_hex())
        );

        let missing = load_requested_object(
            &store,
            &ObjectRequest {
                id: ObjectId::StateId(missing_state),
                have_base: None,
            },
        )
        .unwrap_err();
        assert!(
            matches!(missing, ProtocolError::ObjectNotFound(id) if id == missing_state.to_string())
        );

        let mismatch =
            load_object_data(&store, &ObjectId::Hash(missing_hash), ObjectType::State).unwrap_err();
        assert!(
            matches!(mismatch, ProtocolError::InvalidState(message) if message == "object id/type mismatch")
        );

        let mismatch =
            load_object_data(&store, &ObjectId::StateId(missing_state), ObjectType::Blob)
                .unwrap_err();
        assert!(
            matches!(mismatch, ProtocolError::InvalidState(message) if message == "object id/type mismatch")
        );
    }

    #[test]
    fn store_received_object_rejects_mismatched_object_identity() {
        let (_temp, store) = create_test_store();
        let blob = Blob::from("tree leaf");
        let blob_hash = store.put_blob(&blob).unwrap();
        let tree = Tree::from_entries(vec![TreeEntry::file("leaf.txt", blob_hash, false).unwrap()]);
        let tree_bytes = rmp_serde::to_vec_named(&tree).unwrap();
        let wrong_hash = ContentHash::from_bytes([4; 32]);

        let error = store_received_object(
            &store,
            &ObjectData {
                id: ObjectId::Hash(wrong_hash),
                obj_type: ObjectType::Tree,
                data: tree_bytes,
                is_delta: false,
            },
        )
        .unwrap_err();
        assert!(
            matches!(error, ProtocolError::InvalidState(message) if message == "tree hash mismatch")
        );

        let state = State::new(tree.hash(), Vec::new(), test_attribution());
        let wrong_state_id = objects::object::StateId::from_bytes([5; 32]);
        let error = store_received_object(
            &store,
            &ObjectData {
                id: ObjectId::StateId(wrong_state_id),
                obj_type: ObjectType::State,
                data: rmp_serde::to_vec_named(&state).unwrap(),
                is_delta: false,
            },
        )
        .unwrap_err();
        assert!(
            matches!(error, ProtocolError::InvalidState(message) if message.contains("StateId mismatch"))
        );
    }

    #[test]
    fn store_received_object_rejects_raw_sidecar_objects() {
        let (_temp, store) = create_test_store();
        let blob_hash = ContentHash::from_bytes([1; 32]);
        let state_id = objects::object::StateId::from_bytes([2; 32]);

        let redaction_error = store_received_object(
            &store,
            &ObjectData {
                id: ObjectId::Hash(blob_hash),
                obj_type: ObjectType::Redaction,
                data: b"unsigned redaction bytes".to_vec(),
                is_delta: false,
            },
        )
        .unwrap_err();
        assert!(
            matches!(redaction_error, ProtocolError::InvalidState(message) if message.contains("signature verification is required"))
        );

        let visibility_error = store_received_object(
            &store,
            &ObjectData {
                id: ObjectId::StateId(state_id),
                obj_type: ObjectType::StateVisibility,
                data: b"raw visibility bytes".to_vec(),
                is_delta: false,
            },
        )
        .unwrap_err();
        assert!(
            matches!(visibility_error, ProtocolError::InvalidState(message) if message.contains("sidecar validation is required"))
        );
    }

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
