// SPDX-License-Identifier: Apache-2.0
//! Async/cloud-native object storage interface.

use std::pin::Pin;

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt, stream};

use crate::{
    error::{HeddleError, StorageErrorKind},
    object::ContentHash,
};

use super::{
    BlockingObjectStore, Page, PageRequest, Result,
    types::{ObjectBytes, ObjectCollection, ObjectKey, ObjectPresence, ObjectPutOutcome},
};

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

/// Async storage interface for hosted/cloud backends.
#[allow(async_fn_in_trait)]
pub trait ObjectStore: Send + Sync {
    async fn get_blob_bytes(&self, hash: &ContentHash) -> Result<Option<Bytes>>;

    async fn put_blob_bytes_with_hash(&self, data: Bytes, hash: ContentHash)
    -> Result<ContentHash>;

    async fn has_blob(&self, hash: &ContentHash) -> Result<bool>;

    async fn get_object_stream(&self, key: &ObjectKey) -> Result<Option<ByteStream>>;

    async fn put_object_stream(
        &self,
        key: ObjectKey,
        stream: ByteStream,
    ) -> Result<ObjectPutOutcome>;

    async fn has_many(&self, keys: &[ObjectKey]) -> Result<Vec<ObjectPresence>>;

    async fn put_many(&self, objects: Vec<ObjectBytes>) -> Result<Vec<ObjectPutOutcome>>;

    async fn list_page(
        &self,
        collection: ObjectCollection,
        page: PageRequest,
    ) -> Result<Page<ObjectKey>>;
}

impl<T> ObjectStore for T
where
    T: BlockingObjectStore + Send + Sync,
{
    async fn get_blob_bytes(&self, hash: &ContentHash) -> Result<Option<Bytes>> {
        BlockingObjectStore::get_blob_bytes(self, hash)
    }

    async fn put_blob_bytes_with_hash(
        &self,
        data: Bytes,
        hash: ContentHash,
    ) -> Result<ContentHash> {
        BlockingObjectStore::put_blob_bytes_with_hash(self, &data, hash)
    }

    async fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        BlockingObjectStore::has_blob(self, hash)
    }

    async fn get_object_stream(&self, key: &ObjectKey) -> Result<Option<ByteStream>> {
        let bytes = match key {
            ObjectKey::Blob(hash) => BlockingObjectStore::get_blob_bytes(self, hash)?,
            ObjectKey::Tree(hash) => {
                BlockingObjectStore::get_pack_object(self, &super::pack::PackObjectId::Hash(*hash))?
                    .map(|(_, bytes)| Bytes::from(bytes))
            }
            ObjectKey::State(id) => BlockingObjectStore::get_pack_object(
                self,
                &super::pack::PackObjectId::ChangeId(*id),
            )?
            .map(|(_, bytes)| Bytes::from(bytes)),
            ObjectKey::Action(id) => BlockingObjectStore::get_pack_object(
                self,
                &super::pack::PackObjectId::Hash(*id.as_hash()),
            )?
            .map(|(_, bytes)| Bytes::from(bytes)),
            ObjectKey::Redactions(hash) => {
                BlockingObjectStore::get_redactions_bytes_for_blob(self, hash)?.map(Bytes::from)
            }
            ObjectKey::StateVisibility(id) => {
                BlockingObjectStore::get_state_visibility_bytes_for_state(self, id)?
                    .map(Bytes::from)
            }
            ObjectKey::PackObject(id) => {
                BlockingObjectStore::get_pack_object(self, id)?.map(|(_, bytes)| Bytes::from(bytes))
            }
        };
        Ok(bytes.map(single_chunk_stream))
    }

    async fn put_object_stream(
        &self,
        key: ObjectKey,
        stream: ByteStream,
    ) -> Result<ObjectPutOutcome> {
        let bytes = collect_stream(stream).await?;
        put_object_bytes(self, &key, &bytes)?;
        Ok(ObjectPutOutcome { key, written: true })
    }

    async fn has_many(&self, keys: &[ObjectKey]) -> Result<Vec<ObjectPresence>> {
        keys.iter()
            .map(|key| {
                Ok(ObjectPresence {
                    key: key.clone(),
                    present: has_key(self, key)?,
                })
            })
            .collect()
    }

    async fn put_many(&self, objects: Vec<ObjectBytes>) -> Result<Vec<ObjectPutOutcome>> {
        objects
            .into_iter()
            .map(|object| {
                put_object_bytes(self, &object.key, &object.bytes)?;
                Ok(ObjectPutOutcome {
                    key: object.key,
                    written: true,
                })
            })
            .collect()
    }

    async fn list_page(
        &self,
        collection: ObjectCollection,
        page: PageRequest,
    ) -> Result<Page<ObjectKey>> {
        match collection {
            ObjectCollection::Blobs => map_page(
                BlockingObjectStore::list_blobs_page(self, page)?,
                ObjectKey::Blob,
            ),
            ObjectCollection::Trees => map_page(
                BlockingObjectStore::list_trees_page(self, page)?,
                ObjectKey::Tree,
            ),
            ObjectCollection::States => map_page(
                BlockingObjectStore::list_states_page(self, page)?,
                ObjectKey::State,
            ),
            ObjectCollection::Actions => map_page(
                BlockingObjectStore::list_actions_page(self, page)?,
                ObjectKey::Action,
            ),
            ObjectCollection::Redactions => map_page(
                BlockingObjectStore::list_blobs_with_redactions_page(self, page)?,
                ObjectKey::Redactions,
            ),
            ObjectCollection::StateVisibility => map_page(
                BlockingObjectStore::list_states_with_visibility_page(self, page)?,
                ObjectKey::StateVisibility,
            ),
        }
    }
}

fn map_page<T>(page: Page<T>, map: impl Fn(T) -> ObjectKey) -> Result<Page<ObjectKey>> {
    Ok(Page::new(
        page.items.into_iter().map(map).collect(),
        page.next_token,
    ))
}

fn single_chunk_stream(bytes: Bytes) -> ByteStream {
    Box::pin(stream::once(async move { Ok(bytes) }))
}

async fn collect_stream(mut source: ByteStream) -> Result<Bytes> {
    let mut out = BytesMut::new();
    while let Some(chunk) = source.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out.freeze())
}

fn has_key(store: &(impl BlockingObjectStore + ?Sized), key: &ObjectKey) -> Result<bool> {
    match key {
        ObjectKey::Blob(hash) => store.has_blob(hash),
        ObjectKey::Tree(hash) => store.has_tree(hash),
        ObjectKey::State(id) => store.has_state(id),
        ObjectKey::Action(id) => Ok(store.get_action(id)?.is_some()),
        ObjectKey::Redactions(hash) => store.has_redactions_for_blob(hash),
        ObjectKey::StateVisibility(id) => store.has_state_visibility_for_state(id),
        ObjectKey::PackObject(id) => Ok(store.get_pack_object(id)?.is_some()),
    }
}

fn put_object_bytes(
    store: &(impl BlockingObjectStore + ?Sized),
    key: &ObjectKey,
    bytes: &[u8],
) -> Result<()> {
    match key {
        ObjectKey::Blob(hash) => {
            store.put_blob_bytes_with_hash(bytes, *hash)?;
        }
        ObjectKey::Tree(hash) => {
            store.put_tree_serialized(bytes, *hash)?;
        }
        ObjectKey::State(id) => {
            store.put_state_serialized(bytes, *id)?;
        }
        ObjectKey::Action(id) => {
            store.put_action_serialized(bytes, *id)?;
        }
        ObjectKey::Redactions(hash) => {
            store.put_redactions_bytes_for_blob(hash, bytes)?;
        }
        ObjectKey::StateVisibility(id) => {
            store.put_state_visibility_bytes_for_state(id, bytes)?;
        }
        ObjectKey::PackObject(_) => {
            return Err(HeddleError::storage(
                StorageErrorKind::Unsupported,
                "writing an individual pack object is not supported; write typed objects or install a full pack",
            ));
        }
    }
    Ok(())
}
