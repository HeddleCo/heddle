// SPDX-License-Identifier: Apache-2.0
//! Local object storage interface.

use std::path::Path;

use bytes::Bytes;

use super::{
    Page, PageRequest, Result,
    pack::{self, PackObjectId},
};
use crate::{
    error::{HeddleError, StorageErrorKind},
    object::{Action, ActionId, Blob, ChangeId, ContentHash, State, Tree},
};

/// Local synchronous object-store capability.
pub trait LocalObjectStore: Send + Sync {
    fn get_blob(&self, _hash: &ContentHash) -> Result<Option<Blob>> {
        unsupported_local_method("get_blob")
    }

    fn put_blob(&self, _blob: &Blob) -> Result<ContentHash> {
        unsupported_local_method("put_blob")
    }

    fn get_blob_bytes(&self, hash: &ContentHash) -> Result<Option<Bytes>> {
        Ok(self
            .get_blob(hash)?
            .map(|blob| Bytes::from(blob.into_content())))
    }

    fn blob_size(&self, hash: &ContentHash) -> Result<Option<u64>> {
        Ok(self.get_blob(hash)?.map(|blob| blob.content().len() as u64))
    }

    fn put_blob_with_hash(&self, blob: &Blob, hash: ContentHash) -> Result<ContentHash> {
        if blob.hash() != hash {
            return Err(HeddleError::storage(
                StorageErrorKind::CasMismatch,
                "blob hash mismatch",
            ));
        }
        self.put_blob(blob)
    }

    fn put_blob_bytes_with_hash(&self, data: &[u8], hash: ContentHash) -> Result<ContentHash> {
        self.put_blob_with_hash(&Blob::from_slice(data), hash)
    }

    fn has_blob(&self, _hash: &ContentHash) -> Result<bool> {
        unsupported_local_method("has_blob")
    }

    fn get_tree(&self, _hash: &ContentHash) -> Result<Option<Tree>> {
        unsupported_local_method("get_tree")
    }

    fn put_tree(&self, _tree: &Tree) -> Result<ContentHash> {
        unsupported_local_method("put_tree")
    }

    fn has_tree(&self, _hash: &ContentHash) -> Result<bool> {
        unsupported_local_method("has_tree")
    }

    fn get_state(&self, _id: &ChangeId) -> Result<Option<State>> {
        unsupported_local_method("get_state")
    }

    fn put_state(&self, _state: &State) -> Result<()> {
        unsupported_local_method("put_state")
    }

    fn has_state(&self, _id: &ChangeId) -> Result<bool> {
        unsupported_local_method("has_state")
    }

    fn list_states(&self) -> Result<Vec<ChangeId>> {
        unsupported_local_method("list_states")
    }

    fn list_states_page(&self, page: PageRequest) -> Result<Page<ChangeId>> {
        Page::from_local_items(self.list_states()?, page)
    }

    fn get_action(&self, _id: &ActionId) -> Result<Option<Action>> {
        unsupported_local_method("get_action")
    }

    fn put_action(&self, _action: &mut Action) -> Result<ActionId> {
        unsupported_local_method("put_action")
    }

    fn list_actions(&self) -> Result<Vec<ActionId>> {
        unsupported_local_method("list_actions")
    }

    fn list_actions_page(&self, page: PageRequest) -> Result<Page<ActionId>> {
        Page::from_local_items(self.list_actions()?, page)
    }

    fn list_blobs(&self) -> Result<Vec<ContentHash>> {
        unsupported_local_method("list_blobs")
    }

    fn list_blobs_page(&self, page: PageRequest) -> Result<Page<ContentHash>> {
        Page::from_local_items(self.list_blobs()?, page)
    }

    fn list_trees(&self) -> Result<Vec<ContentHash>> {
        unsupported_local_method("list_trees")
    }

    fn list_trees_page(&self, page: PageRequest) -> Result<Page<ContentHash>> {
        Page::from_local_items(self.list_trees()?, page)
    }

    fn put_tree_serialized(&self, data: &[u8], hash: ContentHash) -> Result<ContentHash> {
        let tree: Tree = rmp_serde::from_slice(data)?;
        tree.validate()?;
        if tree.hash() != hash {
            return Err(HeddleError::Corruption {
                expected: hash,
                found: tree.hash(),
            });
        }
        self.put_tree(&tree)
    }

    fn put_state_serialized(&self, data: &[u8], id: ChangeId) -> Result<()> {
        let state: State = rmp_serde::from_slice(data)?;
        if state.change_id != id {
            return Err(HeddleError::InvalidObject(format!(
                "state change_id mismatch: expected {}, found {}",
                id, state.change_id
            )));
        }
        self.put_state(&state)
    }

    fn put_action_serialized(&self, data: &[u8], id: ActionId) -> Result<()> {
        let mut action: Action = rmp_serde::from_slice(data)?;
        let found_id = action.compute_id();
        if found_id != id {
            return Err(HeddleError::InvalidObject(format!(
                "action id mismatch: expected {}, found {}",
                id, found_id
            )));
        }
        let stored_id = self.put_action(&mut action)?;
        if stored_id != id {
            return Err(HeddleError::InvalidObject(format!(
                "action id mismatch after write: expected {}, found {}",
                id, stored_id
            )));
        }
        Ok(())
    }

    fn get_pack_object(&self, id: &PackObjectId) -> Result<Option<(pack::ObjectType, Vec<u8>)>> {
        match id {
            PackObjectId::Hash(hash) => {
                if let Some(blob) = self.get_blob(hash)? {
                    return Ok(Some((pack::ObjectType::Blob, blob.content().to_vec())));
                }
                if let Some(tree) = self.get_tree(hash)? {
                    return Ok(Some((
                        pack::ObjectType::Tree,
                        rmp_serde::to_vec_named(&tree)?,
                    )));
                }
                if let Some(action) = self.get_action(&ActionId::from_hash(*hash))? {
                    return Ok(Some((
                        pack::ObjectType::Action,
                        rmp_serde::to_vec_named(&action)?,
                    )));
                }
                Ok(None)
            }
            PackObjectId::ChangeId(change_id) => {
                if let Some(state) = self.get_state(change_id)? {
                    Ok(Some((
                        pack::ObjectType::State,
                        rmp_serde::to_vec_named(&state)?,
                    )))
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn put_blobs_packed(&self, blobs: Vec<(ContentHash, Vec<u8>)>) -> Result<()> {
        for (hash, data) in blobs {
            if !self.has_blob(&hash)? {
                self.put_blob_bytes_with_hash(&data, hash)?;
            }
        }
        Ok(())
    }

    fn install_pack(&self, pack_data: &[u8], index_data: &[u8]) -> Result<Vec<PackObjectId>> {
        let reader = pack::PackReader::from_slice(pack_data, index_data)?;
        let ids = reader.list_ids();
        for id in &ids {
            let Some((obj_type, data)) = reader.get_object(id)? else {
                continue;
            };
            match (id, obj_type) {
                (PackObjectId::Hash(hash), pack::ObjectType::Blob) => {
                    self.put_blob_bytes_with_hash(&data, *hash)?;
                }
                (PackObjectId::Hash(hash), pack::ObjectType::Tree) => {
                    self.put_tree_serialized(&data, *hash)?;
                }
                (PackObjectId::Hash(hash), pack::ObjectType::Action) => {
                    self.put_action_serialized(&data, ActionId::from_hash(*hash))?;
                }
                (PackObjectId::ChangeId(change_id), pack::ObjectType::State) => {
                    self.put_state_serialized(&data, *change_id)?;
                }
                _ => {
                    return Err(HeddleError::InvalidObject(format!(
                        "unsupported native pack object: {:?} {:?}",
                        id, obj_type
                    )));
                }
            }
        }
        Ok(ids)
    }

    fn install_pack_from_paths(
        &self,
        pack_path: &Path,
        index_path: &Path,
    ) -> Result<Vec<PackObjectId>> {
        let pack_data = std::fs::read(pack_path)?;
        let index_data = std::fs::read(index_path)?;
        self.install_pack(&pack_data, &index_data)
    }

    fn has_redactions_for_blob(&self, _blob: &ContentHash) -> Result<bool> {
        Ok(false)
    }

    fn get_redactions_bytes_for_blob(&self, _blob: &ContentHash) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn put_redactions_bytes_for_blob(&self, _blob: &ContentHash, _bytes: &[u8]) -> Result<()> {
        Err(HeddleError::storage(
            StorageErrorKind::Unsupported,
            "this object store does not support persisting redactions",
        ))
    }

    fn list_blobs_with_redactions(&self) -> Result<Vec<ContentHash>> {
        Ok(Vec::new())
    }

    fn list_blobs_with_redactions_page(&self, page: PageRequest) -> Result<Page<ContentHash>> {
        Page::from_local_items(self.list_blobs_with_redactions()?, page)
    }

    fn has_state_visibility_for_state(&self, _state: &ChangeId) -> Result<bool> {
        Ok(false)
    }

    fn get_state_visibility_bytes_for_state(&self, _state: &ChangeId) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn put_state_visibility_bytes_for_state(&self, _state: &ChangeId, _bytes: &[u8]) -> Result<()> {
        Err(HeddleError::storage(
            StorageErrorKind::Unsupported,
            "this object store does not support persisting state visibility",
        ))
    }

    fn list_states_with_visibility(&self) -> Result<Vec<ChangeId>> {
        Ok(Vec::new())
    }

    fn list_states_with_visibility_page(&self, page: PageRequest) -> Result<Page<ChangeId>> {
        Page::from_local_items(self.list_states_with_visibility()?, page)
    }
}

fn unsupported_local_method<T>(name: &str) -> Result<T> {
    Err(HeddleError::storage(
        StorageErrorKind::Unsupported,
        format!("object store does not support local method {name}"),
    ))
}
