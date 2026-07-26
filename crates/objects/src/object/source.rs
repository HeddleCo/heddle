// SPDX-License-Identifier: Apache-2.0
//! Read-only object source traits for graph walkers.

use crate::error::Result;

use super::{Blob, ContentHash, State, StateId, Tree};

/// Read-only object access needed by object graph walkers.
pub trait ObjectSource {
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>>;
    fn get_state(&self, id: &StateId) -> Result<Option<State>>;
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>>;

    /// Zero-copy variant of `get_blob`.
    fn get_blob_bytes(&self, hash: &ContentHash) -> Result<Option<bytes::Bytes>> {
        Ok(self
            .get_blob(hash)?
            .map(|blob| bytes::Bytes::from(blob.into_content())))
    }
}

#[cfg(feature = "async-source")]
#[allow(async_fn_in_trait)]
pub trait AsyncObjectSource {
    async fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>>;
    async fn get_state(&self, id: &StateId) -> Result<Option<State>>;
    async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>>;
}
