// SPDX-License-Identifier: Apache-2.0
//! Async/cloud-native object storage interface.

use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    pin::Pin,
};

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt, stream};

use super::{
    Page, PageRequest, Result,
    pack::PackObjectId,
    types::{ObjectBytes, ObjectCollection, ObjectKey, ObjectPresence, ObjectPutOutcome},
};
use crate::{
    error::{HeddleError, StorageErrorKind},
    object::{ChangeId, ContentHash},
};

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

/// Async object-store capability for downstream hosted/cloud backends.
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

pub fn single_chunk_stream(bytes: Bytes) -> ByteStream {
    Box::pin(stream::once(async move { Ok(bytes) }))
}

pub fn file_byte_stream(path: PathBuf) -> ByteStream {
    Box::pin(stream::once(async move {
        let mut file = File::open(path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        Ok(Bytes::from(buf))
    }))
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
        Some(stream) => collect_stream(stream).await.map(Some),
        None => Ok(None),
    }
}
