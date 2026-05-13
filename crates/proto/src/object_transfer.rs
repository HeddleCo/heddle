// SPDX-License-Identifier: Apache-2.0
use objects::{
    object::{State, Tree},
    store::ObjectStore,
};

use crate::{ObjectData, ObjectId, ObjectRequest, ObjectType, ProtocolError, Result};

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

pub fn load_requested_object(store: &dyn ObjectStore, req: &ObjectRequest) -> Result<ObjectData> {
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
    store: &dyn ObjectStore,
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

pub fn store_received_object(store: &dyn ObjectStore, data: &ObjectData) -> Result<()> {
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
}