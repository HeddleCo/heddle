// SPDX-License-Identifier: Apache-2.0
//! Object storage interfaces.

use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    pin::Pin,
};

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt, stream};

use crate::{
    error::{HeddleError, StorageErrorKind},
    object::{Action, ActionId, Blob, ChangeId, ContentHash, State, Tree},
};

#[cfg(any(test, feature = "memory-backend"))]
use super::InMemoryStore;
use super::{
    AnyStore, FsStore, Page, PageRequest, Result,
    pack::{self, PackObjectId},
    types::{ObjectBytes, ObjectCollection, ObjectKey, ObjectPresence, ObjectPutOutcome},
};

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

/// Async/cloud-native object storage interface.
#[allow(async_fn_in_trait)]
pub trait ObjectStore: Send + Sync {
    async fn get_blob_bytes_async(&self, hash: &ContentHash) -> Result<Option<Bytes>> {
        collect_optional_stream(self.get_object_stream(&ObjectKey::Blob(*hash)).await?).await
    }

    async fn put_blob_bytes_with_hash_async(
        &self,
        data: Bytes,
        hash: ContentHash,
    ) -> Result<ContentHash> {
        let size = data.len() as u64;
        self.put_object_stream_sized(ObjectKey::Blob(hash), single_chunk_stream(data), Some(size))
            .await?;
        Ok(hash)
    }

    async fn has_blob_async(&self, hash: &ContentHash) -> Result<bool> {
        self.has_object(&ObjectKey::Blob(*hash)).await
    }

    async fn blob_size_async(&self, hash: &ContentHash) -> Result<Option<u64>> {
        Ok(self
            .get_blob_bytes_async(hash)
            .await?
            .map(|bytes| bytes.len() as u64))
    }

    async fn get_object_stream(&self, key: &ObjectKey) -> Result<Option<ByteStream>>;

    async fn put_object_stream(
        &self,
        key: ObjectKey,
        stream: ByteStream,
    ) -> Result<ObjectPutOutcome> {
        self.put_object_stream_sized(key, stream, None).await
    }

    /// Store an object from a stream, preserving the exact byte length when the
    /// caller already knows it.
    ///
    /// Cloud stores can override this to pass a content length to a single PUT
    /// request or choose a multipart strategy without collecting the stream
    /// first.
    async fn put_object_stream_sized(
        &self,
        key: ObjectKey,
        stream: ByteStream,
        size: Option<u64>,
    ) -> Result<ObjectPutOutcome>;

    async fn has_object(&self, key: &ObjectKey) -> Result<bool> {
        Ok(self.get_object_stream(key).await?.is_some())
    }

    async fn has_many(&self, keys: &[ObjectKey]) -> Result<Vec<ObjectPresence>> {
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            out.push(ObjectPresence {
                key: key.clone(),
                present: self.has_object(key).await?,
            });
        }
        Ok(out)
    }

    async fn put_many(&self, objects: Vec<ObjectBytes>) -> Result<Vec<ObjectPutOutcome>> {
        let mut out = Vec::with_capacity(objects.len());
        for object in objects {
            let size = object.bytes.len() as u64;
            out.push(
                self.put_object_stream_sized(
                    object.key,
                    single_chunk_stream(object.bytes),
                    Some(size),
                )
                .await?,
            );
        }
        Ok(out)
    }

    async fn has_redactions_for_blob_async(&self, blob: &ContentHash) -> Result<bool> {
        self.has_object(&ObjectKey::Redactions(*blob)).await
    }

    async fn get_redactions_bytes_for_blob_async(
        &self,
        blob: &ContentHash,
    ) -> Result<Option<Bytes>> {
        collect_optional_stream(
            self.get_object_stream(&ObjectKey::Redactions(*blob))
                .await?,
        )
        .await
    }

    async fn put_redactions_bytes_for_blob_async(
        &self,
        blob: &ContentHash,
        bytes: Bytes,
    ) -> Result<()> {
        let size = bytes.len() as u64;
        self.put_object_stream_sized(
            ObjectKey::Redactions(*blob),
            single_chunk_stream(bytes),
            Some(size),
        )
        .await?;
        Ok(())
    }

    async fn has_state_visibility_for_state_async(&self, state: &ChangeId) -> Result<bool> {
        self.has_object(&ObjectKey::StateVisibility(*state)).await
    }

    async fn get_state_visibility_bytes_for_state_async(
        &self,
        state: &ChangeId,
    ) -> Result<Option<Bytes>> {
        collect_optional_stream(
            self.get_object_stream(&ObjectKey::StateVisibility(*state))
                .await?,
        )
        .await
    }

    async fn put_state_visibility_bytes_for_state_async(
        &self,
        state: &ChangeId,
        bytes: Bytes,
    ) -> Result<()> {
        let size = bytes.len() as u64;
        self.put_object_stream_sized(
            ObjectKey::StateVisibility(*state),
            single_chunk_stream(bytes),
            Some(size),
        )
        .await?;
        Ok(())
    }

    async fn install_pack_stream(
        &self,
        pack_stream: ByteStream,
        index_stream: ByteStream,
    ) -> Result<Vec<PackObjectId>> {
        let _ = (pack_stream, index_stream);
        Err(HeddleError::storage(
            StorageErrorKind::Unsupported,
            "this object store does not support installing native packs",
        ))
    }

    async fn install_pack_from_paths(
        &self,
        pack_path: &Path,
        index_path: &Path,
    ) -> Result<Vec<PackObjectId>> {
        self.install_pack_stream(
            file_byte_stream(pack_path.to_path_buf()),
            file_byte_stream(index_path.to_path_buf()),
        )
        .await
    }

    async fn list_page(
        &self,
        collection: ObjectCollection,
        page: PageRequest,
    ) -> Result<Page<ObjectKey>> {
        let _ = (collection, page);
        Err(HeddleError::storage(
            StorageErrorKind::Unsupported,
            "this object store does not support listing objects",
        ))
    }
}

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

/// Adapter for exposing a synchronous local store through the async
/// [`ObjectStore`] contract without creating downstream coherence conflicts.
#[derive(Debug, Clone, Copy)]
pub struct AsyncFromLocal<T> {
    inner: T,
}

impl<T> AsyncFromLocal<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &T {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: LocalObjectStore> ObjectStore for AsyncFromLocal<T> {
    async fn get_blob_bytes_async(&self, hash: &ContentHash) -> Result<Option<Bytes>> {
        local_get_blob_bytes_async(&self.inner, hash).await
    }

    async fn put_blob_bytes_with_hash_async(
        &self,
        data: Bytes,
        hash: ContentHash,
    ) -> Result<ContentHash> {
        local_put_blob_bytes_with_hash_async(&self.inner, data, hash).await
    }

    async fn has_blob_async(&self, hash: &ContentHash) -> Result<bool> {
        local_has_blob_async(&self.inner, hash).await
    }

    async fn blob_size_async(&self, hash: &ContentHash) -> Result<Option<u64>> {
        local_blob_size_async(&self.inner, hash).await
    }

    async fn get_object_stream(&self, key: &ObjectKey) -> Result<Option<ByteStream>> {
        local_get_object_stream(&self.inner, key).await
    }

    async fn put_object_stream_sized(
        &self,
        key: ObjectKey,
        stream: ByteStream,
        _size: Option<u64>,
    ) -> Result<ObjectPutOutcome> {
        local_put_object_stream(&self.inner, key, stream).await
    }

    async fn has_object(&self, key: &ObjectKey) -> Result<bool> {
        local_has_object(&self.inner, key).await
    }

    async fn has_many(&self, keys: &[ObjectKey]) -> Result<Vec<ObjectPresence>> {
        local_has_many(&self.inner, keys).await
    }

    async fn put_many(&self, objects: Vec<ObjectBytes>) -> Result<Vec<ObjectPutOutcome>> {
        local_put_many(&self.inner, objects).await
    }

    async fn has_redactions_for_blob_async(&self, blob: &ContentHash) -> Result<bool> {
        local_has_redactions_for_blob_async(&self.inner, blob).await
    }

    async fn get_redactions_bytes_for_blob_async(
        &self,
        blob: &ContentHash,
    ) -> Result<Option<Bytes>> {
        local_get_redactions_bytes_for_blob_async(&self.inner, blob).await
    }

    async fn put_redactions_bytes_for_blob_async(
        &self,
        blob: &ContentHash,
        bytes: Bytes,
    ) -> Result<()> {
        local_put_redactions_bytes_for_blob_async(&self.inner, blob, bytes).await
    }

    async fn has_state_visibility_for_state_async(&self, state: &ChangeId) -> Result<bool> {
        local_has_state_visibility_for_state_async(&self.inner, state).await
    }

    async fn get_state_visibility_bytes_for_state_async(
        &self,
        state: &ChangeId,
    ) -> Result<Option<Bytes>> {
        local_get_state_visibility_bytes_for_state_async(&self.inner, state).await
    }

    async fn put_state_visibility_bytes_for_state_async(
        &self,
        state: &ChangeId,
        bytes: Bytes,
    ) -> Result<()> {
        local_put_state_visibility_bytes_for_state_async(&self.inner, state, bytes).await
    }

    async fn install_pack_stream(
        &self,
        pack_stream: ByteStream,
        index_stream: ByteStream,
    ) -> Result<Vec<PackObjectId>> {
        local_install_pack_stream(&self.inner, pack_stream, index_stream).await
    }

    async fn install_pack_from_paths(
        &self,
        pack_path: &Path,
        index_path: &Path,
    ) -> Result<Vec<PackObjectId>> {
        local_install_pack_from_paths(&self.inner, pack_path, index_path).await
    }

    async fn list_page(
        &self,
        collection: ObjectCollection,
        page: PageRequest,
    ) -> Result<Page<ObjectKey>> {
        local_list_page(&self.inner, collection, page).await
    }
}

/// Borrowed variant of [`AsyncFromLocal`] for tests and helpers that
/// should not take ownership of the local store.
#[derive(Debug, Clone, Copy)]
pub struct AsyncFromLocalRef<'a, T: ?Sized> {
    inner: &'a T,
}

impl<'a, T: ?Sized> AsyncFromLocalRef<'a, T> {
    pub fn new(inner: &'a T) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &'a T {
        self.inner
    }
}

impl<'a, T: LocalObjectStore + ?Sized> ObjectStore for AsyncFromLocalRef<'a, T> {
    async fn get_blob_bytes_async(&self, hash: &ContentHash) -> Result<Option<Bytes>> {
        local_get_blob_bytes_async(self.inner, hash).await
    }

    async fn put_blob_bytes_with_hash_async(
        &self,
        data: Bytes,
        hash: ContentHash,
    ) -> Result<ContentHash> {
        local_put_blob_bytes_with_hash_async(self.inner, data, hash).await
    }

    async fn has_blob_async(&self, hash: &ContentHash) -> Result<bool> {
        local_has_blob_async(self.inner, hash).await
    }

    async fn blob_size_async(&self, hash: &ContentHash) -> Result<Option<u64>> {
        local_blob_size_async(self.inner, hash).await
    }

    async fn get_object_stream(&self, key: &ObjectKey) -> Result<Option<ByteStream>> {
        local_get_object_stream(self.inner, key).await
    }

    async fn put_object_stream_sized(
        &self,
        key: ObjectKey,
        stream: ByteStream,
        _size: Option<u64>,
    ) -> Result<ObjectPutOutcome> {
        local_put_object_stream(self.inner, key, stream).await
    }

    async fn has_object(&self, key: &ObjectKey) -> Result<bool> {
        local_has_object(self.inner, key).await
    }

    async fn has_many(&self, keys: &[ObjectKey]) -> Result<Vec<ObjectPresence>> {
        local_has_many(self.inner, keys).await
    }

    async fn put_many(&self, objects: Vec<ObjectBytes>) -> Result<Vec<ObjectPutOutcome>> {
        local_put_many(self.inner, objects).await
    }

    async fn has_redactions_for_blob_async(&self, blob: &ContentHash) -> Result<bool> {
        local_has_redactions_for_blob_async(self.inner, blob).await
    }

    async fn get_redactions_bytes_for_blob_async(
        &self,
        blob: &ContentHash,
    ) -> Result<Option<Bytes>> {
        local_get_redactions_bytes_for_blob_async(self.inner, blob).await
    }

    async fn put_redactions_bytes_for_blob_async(
        &self,
        blob: &ContentHash,
        bytes: Bytes,
    ) -> Result<()> {
        local_put_redactions_bytes_for_blob_async(self.inner, blob, bytes).await
    }

    async fn has_state_visibility_for_state_async(&self, state: &ChangeId) -> Result<bool> {
        local_has_state_visibility_for_state_async(self.inner, state).await
    }

    async fn get_state_visibility_bytes_for_state_async(
        &self,
        state: &ChangeId,
    ) -> Result<Option<Bytes>> {
        local_get_state_visibility_bytes_for_state_async(self.inner, state).await
    }

    async fn put_state_visibility_bytes_for_state_async(
        &self,
        state: &ChangeId,
        bytes: Bytes,
    ) -> Result<()> {
        local_put_state_visibility_bytes_for_state_async(self.inner, state, bytes).await
    }

    async fn install_pack_stream(
        &self,
        pack_stream: ByteStream,
        index_stream: ByteStream,
    ) -> Result<Vec<PackObjectId>> {
        local_install_pack_stream(self.inner, pack_stream, index_stream).await
    }

    async fn install_pack_from_paths(
        &self,
        pack_path: &Path,
        index_path: &Path,
    ) -> Result<Vec<PackObjectId>> {
        local_install_pack_from_paths(self.inner, pack_path, index_path).await
    }

    async fn list_page(
        &self,
        collection: ObjectCollection,
        page: PageRequest,
    ) -> Result<Page<ObjectKey>> {
        local_list_page(self.inner, collection, page).await
    }
}

mod sealed {
    pub trait HeddleLocalAsyncOptIn {}
}

impl sealed::HeddleLocalAsyncOptIn for FsStore {}
#[cfg(any(test, feature = "memory-backend"))]
impl sealed::HeddleLocalAsyncOptIn for InMemoryStore {}
impl sealed::HeddleLocalAsyncOptIn for AnyStore {}

impl<T> ObjectStore for T
where
    T: LocalObjectStore + sealed::HeddleLocalAsyncOptIn,
{
    async fn get_blob_bytes_async(&self, hash: &ContentHash) -> Result<Option<Bytes>> {
        local_get_blob_bytes_async(self, hash).await
    }

    async fn put_blob_bytes_with_hash_async(
        &self,
        data: Bytes,
        hash: ContentHash,
    ) -> Result<ContentHash> {
        local_put_blob_bytes_with_hash_async(self, data, hash).await
    }

    async fn has_blob_async(&self, hash: &ContentHash) -> Result<bool> {
        local_has_blob_async(self, hash).await
    }

    async fn blob_size_async(&self, hash: &ContentHash) -> Result<Option<u64>> {
        local_blob_size_async(self, hash).await
    }

    async fn get_object_stream(&self, key: &ObjectKey) -> Result<Option<ByteStream>> {
        local_get_object_stream(self, key).await
    }

    async fn put_object_stream_sized(
        &self,
        key: ObjectKey,
        stream: ByteStream,
        _size: Option<u64>,
    ) -> Result<ObjectPutOutcome> {
        local_put_object_stream(self, key, stream).await
    }

    async fn has_object(&self, key: &ObjectKey) -> Result<bool> {
        local_has_object(self, key).await
    }

    async fn has_many(&self, keys: &[ObjectKey]) -> Result<Vec<ObjectPresence>> {
        local_has_many(self, keys).await
    }

    async fn put_many(&self, objects: Vec<ObjectBytes>) -> Result<Vec<ObjectPutOutcome>> {
        local_put_many(self, objects).await
    }

    async fn has_redactions_for_blob_async(&self, blob: &ContentHash) -> Result<bool> {
        local_has_redactions_for_blob_async(self, blob).await
    }

    async fn get_redactions_bytes_for_blob_async(
        &self,
        blob: &ContentHash,
    ) -> Result<Option<Bytes>> {
        local_get_redactions_bytes_for_blob_async(self, blob).await
    }

    async fn put_redactions_bytes_for_blob_async(
        &self,
        blob: &ContentHash,
        bytes: Bytes,
    ) -> Result<()> {
        local_put_redactions_bytes_for_blob_async(self, blob, bytes).await
    }

    async fn has_state_visibility_for_state_async(&self, state: &ChangeId) -> Result<bool> {
        local_has_state_visibility_for_state_async(self, state).await
    }

    async fn get_state_visibility_bytes_for_state_async(
        &self,
        state: &ChangeId,
    ) -> Result<Option<Bytes>> {
        local_get_state_visibility_bytes_for_state_async(self, state).await
    }

    async fn put_state_visibility_bytes_for_state_async(
        &self,
        state: &ChangeId,
        bytes: Bytes,
    ) -> Result<()> {
        local_put_state_visibility_bytes_for_state_async(self, state, bytes).await
    }

    async fn install_pack_stream(
        &self,
        pack_stream: ByteStream,
        index_stream: ByteStream,
    ) -> Result<Vec<PackObjectId>> {
        local_install_pack_stream(self, pack_stream, index_stream).await
    }

    async fn install_pack_from_paths(
        &self,
        pack_path: &Path,
        index_path: &Path,
    ) -> Result<Vec<PackObjectId>> {
        local_install_pack_from_paths(self, pack_path, index_path).await
    }

    async fn list_page(
        &self,
        collection: ObjectCollection,
        page: PageRequest,
    ) -> Result<Page<ObjectKey>> {
        local_list_page(self, collection, page).await
    }
}

async fn local_get_blob_bytes_async(
    store: &(impl LocalObjectStore + ?Sized),
    hash: &ContentHash,
) -> Result<Option<Bytes>> {
    collect_optional_stream(local_get_object_stream(store, &ObjectKey::Blob(*hash)).await?).await
}

async fn local_put_blob_bytes_with_hash_async(
    store: &(impl LocalObjectStore + ?Sized),
    data: Bytes,
    hash: ContentHash,
) -> Result<ContentHash> {
    local_put_object_stream(store, ObjectKey::Blob(hash), single_chunk_stream(data)).await?;
    Ok(hash)
}

async fn local_has_blob_async(
    store: &(impl LocalObjectStore + ?Sized),
    hash: &ContentHash,
) -> Result<bool> {
    local_has_object(store, &ObjectKey::Blob(*hash)).await
}

async fn local_blob_size_async(
    store: &(impl LocalObjectStore + ?Sized),
    hash: &ContentHash,
) -> Result<Option<u64>> {
    store.blob_size(hash)
}

async fn local_get_object_stream(
    store: &(impl LocalObjectStore + ?Sized),
    key: &ObjectKey,
) -> Result<Option<ByteStream>> {
    let bytes = match key {
        ObjectKey::Blob(hash) => store.get_blob_bytes(hash)?,
        ObjectKey::Tree(hash) => store
            .get_pack_object(&PackObjectId::Hash(*hash))?
            .map(|(_, bytes)| Bytes::from(bytes)),
        ObjectKey::State(id) => store
            .get_pack_object(&PackObjectId::ChangeId(*id))?
            .map(|(_, bytes)| Bytes::from(bytes)),
        ObjectKey::Action(id) => store
            .get_pack_object(&PackObjectId::Hash(*id.as_hash()))?
            .map(|(_, bytes)| Bytes::from(bytes)),
        ObjectKey::Redactions(hash) => store.get_redactions_bytes_for_blob(hash)?.map(Bytes::from),
        ObjectKey::StateVisibility(id) => store
            .get_state_visibility_bytes_for_state(id)?
            .map(Bytes::from),
        ObjectKey::PackObject(id) => store
            .get_pack_object(id)?
            .map(|(_, bytes)| Bytes::from(bytes)),
    };
    Ok(bytes.map(single_chunk_stream))
}

async fn local_put_object_stream(
    store: &(impl LocalObjectStore + ?Sized),
    key: ObjectKey,
    stream: ByteStream,
) -> Result<ObjectPutOutcome> {
    let bytes = collect_stream(stream).await?;
    put_object_bytes(store, &key, &bytes)?;
    Ok(ObjectPutOutcome { key, written: true })
}

async fn local_has_object(
    store: &(impl LocalObjectStore + ?Sized),
    key: &ObjectKey,
) -> Result<bool> {
    has_key(store, key)
}

async fn local_has_many(
    store: &(impl LocalObjectStore + ?Sized),
    keys: &[ObjectKey],
) -> Result<Vec<ObjectPresence>> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        out.push(ObjectPresence {
            key: key.clone(),
            present: local_has_object(store, key).await?,
        });
    }
    Ok(out)
}

async fn local_put_many(
    store: &(impl LocalObjectStore + ?Sized),
    objects: Vec<ObjectBytes>,
) -> Result<Vec<ObjectPutOutcome>> {
    let mut out = Vec::with_capacity(objects.len());
    for object in objects {
        out.push(
            local_put_object_stream(store, object.key, single_chunk_stream(object.bytes)).await?,
        );
    }
    Ok(out)
}

async fn local_has_redactions_for_blob_async(
    store: &(impl LocalObjectStore + ?Sized),
    blob: &ContentHash,
) -> Result<bool> {
    store.has_redactions_for_blob(blob)
}

async fn local_get_redactions_bytes_for_blob_async(
    store: &(impl LocalObjectStore + ?Sized),
    blob: &ContentHash,
) -> Result<Option<Bytes>> {
    Ok(store.get_redactions_bytes_for_blob(blob)?.map(Bytes::from))
}

async fn local_put_redactions_bytes_for_blob_async(
    store: &(impl LocalObjectStore + ?Sized),
    blob: &ContentHash,
    bytes: Bytes,
) -> Result<()> {
    store.put_redactions_bytes_for_blob(blob, &bytes)
}

async fn local_has_state_visibility_for_state_async(
    store: &(impl LocalObjectStore + ?Sized),
    state: &ChangeId,
) -> Result<bool> {
    store.has_state_visibility_for_state(state)
}

async fn local_get_state_visibility_bytes_for_state_async(
    store: &(impl LocalObjectStore + ?Sized),
    state: &ChangeId,
) -> Result<Option<Bytes>> {
    Ok(store
        .get_state_visibility_bytes_for_state(state)?
        .map(Bytes::from))
}

async fn local_put_state_visibility_bytes_for_state_async(
    store: &(impl LocalObjectStore + ?Sized),
    state: &ChangeId,
    bytes: Bytes,
) -> Result<()> {
    store.put_state_visibility_bytes_for_state(state, &bytes)
}

async fn local_install_pack_stream(
    store: &(impl LocalObjectStore + ?Sized),
    pack_stream: ByteStream,
    index_stream: ByteStream,
) -> Result<Vec<PackObjectId>> {
    let pack_data = collect_stream(pack_stream).await?;
    let index_data = collect_stream(index_stream).await?;
    store.install_pack(&pack_data, &index_data)
}

async fn local_install_pack_from_paths(
    store: &(impl LocalObjectStore + ?Sized),
    pack_path: &Path,
    index_path: &Path,
) -> Result<Vec<PackObjectId>> {
    store.install_pack_from_paths(pack_path, index_path)
}

async fn local_list_page(
    store: &(impl LocalObjectStore + ?Sized),
    collection: ObjectCollection,
    page: PageRequest,
) -> Result<Page<ObjectKey>> {
    match collection {
        ObjectCollection::Blobs => map_page(store.list_blobs_page(page)?, ObjectKey::Blob),
        ObjectCollection::Trees => map_page(store.list_trees_page(page)?, ObjectKey::Tree),
        ObjectCollection::States => map_page(store.list_states_page(page)?, ObjectKey::State),
        ObjectCollection::Actions => map_page(store.list_actions_page(page)?, ObjectKey::Action),
        ObjectCollection::Redactions => map_page(
            store.list_blobs_with_redactions_page(page)?,
            ObjectKey::Redactions,
        ),
        ObjectCollection::StateVisibility => map_page(
            store.list_states_with_visibility_page(page)?,
            ObjectKey::StateVisibility,
        ),
    }
}

fn unsupported_local_method<T>(name: &str) -> Result<T> {
    Err(HeddleError::storage(
        StorageErrorKind::Unsupported,
        format!("object store does not support local method {name}"),
    ))
}

fn map_page<T>(page: Page<T>, map: impl Fn(T) -> ObjectKey) -> Result<Page<ObjectKey>> {
    Ok(Page::new(
        page.items.into_iter().map(map).collect(),
        page.next_token,
    ))
}

pub fn single_chunk_stream(bytes: Bytes) -> ByteStream {
    Box::pin(stream::once(async move { Ok(bytes) }))
}

pub fn file_byte_stream(path: PathBuf) -> ByteStream {
    const CHUNK_SIZE: usize = 64 * 1024;

    Box::pin(stream::unfold(
        Some((path, None::<File>)),
        |state| async move {
            let Some((path, file)) = state else {
                return None;
            };
            let mut file = match file {
                Some(file) => file,
                None => match File::open(&path) {
                    Ok(file) => file,
                    Err(error) => return Some((Err(HeddleError::from(error)), None)),
                },
            };
            let mut buf = vec![0; CHUNK_SIZE];
            match file.read(&mut buf) {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(Bytes::from(buf)), Some((path, Some(file)))))
                }
                Err(error) => Some((Err(HeddleError::from(error)), None)),
            }
        },
    ))
}

pub async fn collect_stream(mut source: ByteStream) -> Result<Bytes> {
    let mut out = BytesMut::new();
    while let Some(chunk) = source.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out.freeze())
}

pub async fn collect_optional_stream(source: Option<ByteStream>) -> Result<Option<Bytes>> {
    match source {
        Some(stream) => Ok(Some(collect_stream(stream).await?)),
        None => Ok(None),
    }
}

fn has_key(store: &(impl LocalObjectStore + ?Sized), key: &ObjectKey) -> Result<bool> {
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
    store: &(impl LocalObjectStore + ?Sized),
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

#[cfg(test)]
mod tests {
    use super::*;

    struct DualCapabilityStore;

    impl LocalObjectStore for DualCapabilityStore {}

    impl ObjectStore for DualCapabilityStore {
        async fn get_object_stream(&self, _key: &ObjectKey) -> Result<Option<ByteStream>> {
            Ok(None)
        }

        async fn put_object_stream_sized(
            &self,
            key: ObjectKey,
            _stream: ByteStream,
            _size: Option<u64>,
        ) -> Result<ObjectPutOutcome> {
            Ok(ObjectPutOutcome {
                key,
                written: false,
            })
        }
    }

    #[test]
    fn local_store_can_provide_explicit_async_impl() {
        fn assert_local<T: LocalObjectStore>() {}
        fn assert_async<T: ObjectStore>() {}

        assert_local::<DualCapabilityStore>();
        assert_async::<DualCapabilityStore>();
    }
}
