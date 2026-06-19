// SPDX-License-Identifier: Apache-2.0
//! In-memory object store — reference implementation and test utility.
//!
//! Enable with the `memory-backend` Cargo feature, or use it automatically in
//! `#[cfg(test)]` contexts (no feature flag needed for tests).

use std::{collections::HashMap, sync::RwLock};

use crate::{
    object::{Action, ActionId, Blob, ChangeId, ContentHash, State, Tree},
    store::{BlockingObjectStore, HeddleError, PackMaintenanceStoreExt, Result},
};

/// A non-persistent, in-memory implementation of [`BlockingObjectStore`].
///
/// Useful for testing and as a reference implementation for custom backends.
/// All data is lost when the store is dropped.
///
/// # Example
///
/// ```ignore
/// use cli::store::InMemoryStore;
/// use cli::{BlockingObjectStore, Blob};
///
/// let store = InMemoryStore::new();
/// let blob = Blob::from("hello world");
/// let hash = store.put_blob(&blob).unwrap();
/// let retrieved = store.get_blob(&hash).unwrap().unwrap();
/// assert_eq!(retrieved.content(), b"hello world");
/// ```
#[derive(Default)]
pub struct InMemoryStore {
    blobs: RwLock<HashMap<ContentHash, Vec<u8>>>,
    trees: RwLock<HashMap<ContentHash, Vec<u8>>>,
    states: RwLock<HashMap<ChangeId, Vec<u8>>>,
    actions: RwLock<HashMap<ActionId, Vec<u8>>>,
    redactions: RwLock<HashMap<ContentHash, Vec<u8>>>,
    state_visibility: RwLock<HashMap<ChangeId, Vec<u8>>>,
}

impl InMemoryStore {
    /// Create a new, empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl BlockingObjectStore for InMemoryStore {
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        Ok(self
            .blobs
            .read()
            .unwrap()
            .get(hash)
            .map(|v| Blob::new(v.clone())))
    }

    fn put_blob(&self, blob: &Blob) -> Result<ContentHash> {
        let hash = blob.hash();
        self.blobs
            .write()
            .unwrap()
            .insert(hash, blob.content().to_vec());
        Ok(hash)
    }

    fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        Ok(self.blobs.read().unwrap().contains_key(hash))
    }

    fn blob_size(&self, hash: &ContentHash) -> Result<Option<u64>> {
        // InMemoryStore keeps raw uncompressed bytes — the length of
        // the stored buffer is the blob size, no header parsing needed.
        Ok(self.blobs.read().unwrap().get(hash).map(|v| v.len() as u64))
    }

    fn list_blobs(&self) -> Result<Vec<ContentHash>> {
        Ok(self.blobs.read().unwrap().keys().copied().collect())
    }

    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        match self.trees.read().unwrap().get(hash) {
            Some(bytes) => Ok(Some(rmp_serde::from_slice(bytes)?)),
            None => Ok(None),
        }
    }

    fn put_tree(&self, tree: &Tree) -> Result<ContentHash> {
        let hash = tree.hash();
        self.trees
            .write()
            .unwrap()
            .insert(hash, rmp_serde::to_vec(tree)?);
        Ok(hash)
    }

    fn has_tree(&self, hash: &ContentHash) -> Result<bool> {
        Ok(self.trees.read().unwrap().contains_key(hash))
    }

    fn list_trees(&self) -> Result<Vec<ContentHash>> {
        Ok(self.trees.read().unwrap().keys().copied().collect())
    }

    fn get_state(&self, id: &ChangeId) -> Result<Option<State>> {
        match self.states.read().unwrap().get(id) {
            Some(bytes) => Ok(Some(rmp_serde::from_slice(bytes)?)),
            None => Ok(None),
        }
    }

    fn put_state(&self, state: &State) -> Result<()> {
        self.states
            .write()
            .unwrap()
            .insert(state.change_id, rmp_serde::to_vec(state)?);
        Ok(())
    }

    fn has_state(&self, id: &ChangeId) -> Result<bool> {
        Ok(self.states.read().unwrap().contains_key(id))
    }

    fn list_states(&self) -> Result<Vec<ChangeId>> {
        Ok(self.states.read().unwrap().keys().copied().collect())
    }

    fn get_action(&self, id: &ActionId) -> Result<Option<Action>> {
        match self.actions.read().unwrap().get(id) {
            Some(bytes) => {
                let action: Action = rmp_serde::from_slice(bytes)?;
                let found_id = action.compute_id();
                if found_id != *id {
                    return Err(HeddleError::InvalidObject(format!(
                        "action id mismatch: requested {}, found {}",
                        id, found_id
                    )));
                }
                Ok(Some(action))
            }
            None => Ok(None),
        }
    }

    fn put_action(&self, action: &mut Action) -> Result<ActionId> {
        let id = action.id();
        self.actions
            .write()
            .unwrap()
            .insert(id, rmp_serde::to_vec(action)?);
        Ok(id)
    }

    fn list_actions(&self) -> Result<Vec<ActionId>> {
        Ok(self.actions.read().unwrap().keys().copied().collect())
    }

    fn has_redactions_for_blob(&self, blob: &ContentHash) -> Result<bool> {
        Ok(self.redactions.read().unwrap().contains_key(blob))
    }

    fn get_redactions_bytes_for_blob(&self, blob: &ContentHash) -> Result<Option<Vec<u8>>> {
        Ok(self.redactions.read().unwrap().get(blob).cloned())
    }

    fn put_redactions_bytes_for_blob(&self, blob: &ContentHash, bytes: &[u8]) -> Result<()> {
        self.redactions
            .write()
            .unwrap()
            .insert(*blob, bytes.to_vec());
        Ok(())
    }

    fn list_blobs_with_redactions(&self) -> Result<Vec<ContentHash>> {
        Ok(self.redactions.read().unwrap().keys().copied().collect())
    }

    fn has_state_visibility_for_state(&self, state: &ChangeId) -> Result<bool> {
        Ok(self.state_visibility.read().unwrap().contains_key(state))
    }

    fn get_state_visibility_bytes_for_state(&self, state: &ChangeId) -> Result<Option<Vec<u8>>> {
        Ok(self.state_visibility.read().unwrap().get(state).cloned())
    }

    fn put_state_visibility_bytes_for_state(&self, state: &ChangeId, bytes: &[u8]) -> Result<()> {
        self.state_visibility
            .write()
            .unwrap()
            .insert(*state, bytes.to_vec());
        Ok(())
    }

    fn list_states_with_visibility(&self) -> Result<Vec<ChangeId>> {
        Ok(self
            .state_visibility
            .read()
            .unwrap()
            .keys()
            .copied()
            .collect())
    }
}

impl PackMaintenanceStoreExt for InMemoryStore {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify InMemoryStore satisfies the full BlockingObjectStore compliance contract.
    #[test]
    fn test_compliance() {
        let store = InMemoryStore::new();
        crate::store::store_compliance::run_compliance_tests(&store);
    }

    /// Verify that a second put of the same blob is idempotent.
    #[test]
    fn test_blob_put_idempotent() {
        let store = InMemoryStore::new();
        let blob = Blob::from("idempotent");
        let h1 = store.put_blob(&blob).unwrap();
        let h2 = store.put_blob(&blob).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(store.list_blobs().unwrap().len(), 1);
    }

    /// Verify has_blob returns false for a hash that was never stored.
    #[test]
    fn test_has_blob_unknown() {
        let store = InMemoryStore::new();
        let hash = ContentHash::compute(b"never-stored");
        assert!(!store.has_blob(&hash).unwrap());
    }
}
