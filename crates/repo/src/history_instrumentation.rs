// SPDX-License-Identifier: Apache-2.0
//! Object-source instrumentation shared by bounded history readers.

use objects::{
    object::{Blob, ContentHash, State, StateId, Tree},
    store::ObjectSource,
};

pub(crate) struct HistoryObjectSource<'source, S: ?Sized> {
    source: &'source S,
}

impl<'source, S: ?Sized> HistoryObjectSource<'source, S> {
    pub(crate) fn new(source: &'source S) -> Self {
        Self { source }
    }
}

impl<S> ObjectSource for HistoryObjectSource<'_, S>
where
    S: ObjectSource + ?Sized,
{
    fn get_tree(&self, hash: &ContentHash) -> objects::error::Result<Option<Tree>> {
        record_decoded(self.source.get_tree(hash)?)
    }

    fn get_state(&self, id: &StateId) -> objects::error::Result<Option<State>> {
        record_decoded(self.source.get_state(id)?)
    }

    fn get_blob(&self, hash: &ContentHash) -> objects::error::Result<Option<Blob>> {
        record_decoded(self.source.get_blob(hash)?)
    }

    fn decoded_blob_len(&self, hash: &ContentHash) -> objects::error::Result<Option<u64>> {
        self.source.decoded_blob_len(hash)
    }
}

fn record_decoded<T>(object: Option<T>) -> objects::error::Result<Option<T>> {
    if object.is_some() {
        heddle_perf_contract::record_history_object_decode();
    }
    Ok(object)
}

#[cfg(feature = "async-source")]
pub(crate) struct AsyncHistoryObjectSource<'source, S: ?Sized> {
    source: &'source S,
}

#[cfg(feature = "async-source")]
impl<'source, S: ?Sized> AsyncHistoryObjectSource<'source, S> {
    pub(crate) fn new(source: &'source S) -> Self {
        Self { source }
    }
}

#[cfg(feature = "async-source")]
impl<S> objects::store::AsyncObjectSource for AsyncHistoryObjectSource<'_, S>
where
    S: objects::store::AsyncObjectSource + ?Sized,
{
    async fn get_tree(&self, hash: &ContentHash) -> objects::error::Result<Option<Tree>> {
        record_decoded(self.source.get_tree(hash).await?)
    }

    async fn get_state(&self, id: &StateId) -> objects::error::Result<Option<State>> {
        record_decoded(self.source.get_state(id).await?)
    }

    async fn get_blob(&self, hash: &ContentHash) -> objects::error::Result<Option<Blob>> {
        record_decoded(self.source.get_blob(hash).await?)
    }
}
