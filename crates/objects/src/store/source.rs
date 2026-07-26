// SPDX-License-Identifier: Apache-2.0
//! Store bridges for read-only object source traits.

use super::{ObjectStore, Result};
use crate::object::{Blob, ContentHash, State, StateId, Tree};

#[cfg(feature = "async-source")]
pub use crate::object::AsyncObjectSource;
pub use crate::object::ObjectSource;

impl<S: ObjectStore + ?Sized> ObjectSource for S {
    #[inline]
    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        ObjectStore::get_tree(self, hash)
    }

    #[inline]
    fn get_state(&self, id: &StateId) -> Result<Option<State>> {
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
