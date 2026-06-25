// SPDX-License-Identifier: Apache-2.0
//! Read-only object source traits for graph walkers.

use super::Result;
use crate::{
    object::{Blob, ChangeId, ContentHash, State, Tree},
    store::ObjectStore,
};

/// Read-only subset of [`ObjectStore`] needed by object graph walkers.
pub trait ObjectSource {
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>>;
    fn get_state(&self, id: &ChangeId) -> Result<Option<State>>;
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>>;

    /// Zero-copy variant of `get_blob`.
    fn get_blob_bytes(&self, hash: &ContentHash) -> Result<Option<bytes::Bytes>> {
        Ok(self
            .get_blob(hash)?
            .map(|blob| bytes::Bytes::from(blob.into_content())))
    }
}

impl<S: ObjectStore + ?Sized> ObjectSource for S {
    #[inline]
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        ObjectStore::get_tree(self, hash)
    }

    #[inline]
    fn get_state(&self, id: &ChangeId) -> Result<Option<State>> {
        ObjectStore::get_state(self, id)
    }

    #[inline]
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        ObjectStore::get_blob(self, hash)
    }

    #[inline]
    fn get_blob_bytes(&self, hash: &ContentHash) -> Result<Option<bytes::Bytes>> {
        ObjectStore::get_blob_bytes(self, hash)
    }
}

#[cfg(feature = "async-source")]
#[allow(async_fn_in_trait)]
pub trait AsyncObjectSource {
    async fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>>;
    async fn get_state(&self, id: &ChangeId) -> Result<Option<State>>;
    async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>>;
}
