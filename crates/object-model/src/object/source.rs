// SPDX-License-Identifier: Apache-2.0
//! Read-only object source traits for graph walkers.

use super::{Blob, ContentHash, State, StateId, Tree};
use crate::error::{HeddleError, Result};

/// Read-only object access needed by object graph walkers.
pub trait ObjectSource {
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>>;
    fn get_state(&self, id: &StateId) -> Result<Option<State>>;
    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>>;

    /// Resolve a recorded tree hash; absence is never an empty tree.
    fn require_tree(&self, hash: &ContentHash) -> Result<Tree> {
        self.get_tree(hash)?
            .ok_or_else(|| HeddleError::MissingObject {
                object_type: "tree".to_string(),
                id: hash.to_hex(),
            })
    }

    /// Resolve a recorded blob hash; absence is never empty content.
    fn require_blob(&self, hash: &ContentHash) -> Result<Blob> {
        self.get_blob(hash)?
            .ok_or_else(|| HeddleError::MissingObject {
                object_type: "blob".to_string(),
                id: hash.to_hex(),
            })
    }

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
    async fn require_tree(&self, hash: &ContentHash) -> Result<Tree> {
        self.get_tree(hash)
            .await?
            .ok_or_else(|| HeddleError::MissingObject {
                object_type: "tree".to_string(),
                id: hash.to_hex(),
            })
    }

    async fn require_blob(&self, hash: &ContentHash) -> Result<Blob> {
        self.get_blob(hash)
            .await?
            .ok_or_else(|| HeddleError::MissingObject {
                object_type: "blob".to_string(),
                id: hash.to_hex(),
            })
    }

    async fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>>;
    async fn get_state(&self, id: &StateId) -> Result<Option<State>>;
    async fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>>;
}
