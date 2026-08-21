// SPDX-License-Identifier: Apache-2.0
//! Read-only object source traits for graph walkers.

use crate::error::Result;

use super::{Blob, ContentHash, State, StateId, Tree};

/// Read-only object access needed by object graph walkers.
pub trait ObjectSource {
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>>;
    fn get_state(&self, id: &StateId) -> Result<Option<State>>;
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>>;

    /// Uncompressed byte length without requiring content.
    ///
    /// The default falls back to [`Self::get_blob`]. Stores that can answer
    /// from a header or index should override this so blame can reject an
    /// oversized blob before materializing it.
    fn decoded_blob_len(&self, hash: &ContentHash) -> Result<Option<u64>> {
        Ok(self.get_blob(hash)?.map(|blob| blob.content().len() as u64))
    }

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
