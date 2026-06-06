// SPDX-License-Identifier: Apache-2.0
//! ObjectStore trait implementation for S3 storage.
//!
//! Every method dispatches its async `aws_sdk_s3` work through
//! [`S3Store::bridge`], the worker-thread bridge defined in `s3_store.rs`.
//! Routing through the bridge means the sync `ObjectStore` surface is safe
//! to call from inside a caller's Tokio runtime — the previous design
//! (`Handle::try_current().block_on(...)`) panicked with "Cannot start a
//! runtime from within a runtime" the moment any `#[tokio::main]`,
//! `#[tokio::test]`, or daemon worker exercised this code (issue #60).

mod helpers;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use aws_sdk_s3::primitives::ByteStream;

use self::helpers::{
    should_retry_store_error, validate_loaded_action, validate_loaded_state, validate_loaded_tree,
};
use crate::{
    object::{Action, ActionId, Blob, ChangeId, ContentHash, State, Tree},
    store::{ObjectStore, Result, StoreError, s3::S3Store},
    util::{RetryPolicy, retry_with},
};

impl ObjectStore for S3Store {
    // ── Blob ─────────────────────────────────────────────────────────────────

    fn get_blob(&self, hash: &ContentHash) -> Result<Option<Blob>> {
        let key = self.blob_key(hash);
        let hash = *hash;
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    match client.get_object().bucket(&bucket).key(&key).send().await {
                        Ok(response) => {
                            let data = response.body.collect().await.map_err(|e| {
                                StoreError::Io(std::io::Error::other(format!(
                                    "Failed to read S3 object body: {}",
                                    e
                                )))
                            })?;
                            let bytes = data.into_bytes();
                            let content = if crate::store::compression::is_compressed(&bytes) {
                                crate::store::compression::decompress(&bytes)?
                            } else {
                                bytes.to_vec()
                            };
                            let blob = Blob::new(content);
                            if blob.hash() != hash {
                                return Err(StoreError::Corruption {
                                    expected: hash,
                                    found: blob.hash(),
                                });
                            }
                            Ok(Some(blob))
                        }
                        Err(e) => {
                            if e.as_service_error()
                                .map(|e| e.is_no_such_key())
                                .unwrap_or(false)
                            {
                                Ok(None)
                            } else {
                                Err(StoreError::Io(std::io::Error::other(format!(
                                    "S3 get_object failed: {}",
                                    e
                                ))))
                            }
                        }
                    }
                },
            )
            .await
        })
    }

    fn put_blob(&self, blob: &Blob) -> Result<ContentHash> {
        let hash = blob.hash();
        let key = self.blob_key(&hash);
        let content = blob.content().to_vec();
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    match client.head_object().bucket(&bucket).key(&key).send().await {
                        Ok(_) => return Ok(hash),
                        Err(e) => {
                            if !e
                                .as_service_error()
                                .map(|e| e.is_not_found())
                                .unwrap_or(false)
                            {
                                return Err(StoreError::Io(std::io::Error::other(format!(
                                    "S3 head_object failed: {}",
                                    e
                                ))));
                            }
                        }
                    }
                    let compression_config = crate::store::CompressionConfig::default();
                    let data = crate::store::compression::compress(&content, &compression_config)?
                        .unwrap_or(content.clone());
                    client
                        .put_object()
                        .bucket(&bucket)
                        .key(&key)
                        .body(ByteStream::from(data))
                        .send()
                        .await
                        .map_err(|e| {
                            StoreError::Io(std::io::Error::other(format!(
                                "S3 put_object failed: {}",
                                e
                            )))
                        })?;
                    Ok(hash)
                },
            )
            .await
        })
    }

    fn has_blob(&self, hash: &ContentHash) -> Result<bool> {
        let key = self.blob_key(hash);
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    match client.head_object().bucket(&bucket).key(&key).send().await {
                        Ok(_) => Ok(true),
                        Err(e) => {
                            if e.as_service_error()
                                .map(|e| e.is_not_found())
                                .unwrap_or(false)
                            {
                                Ok(false)
                            } else {
                                Err(StoreError::Io(std::io::Error::other(format!(
                                    "S3 head_object failed: {}",
                                    e
                                ))))
                            }
                        }
                    }
                },
            )
            .await
        })
    }

    fn list_blobs(&self) -> Result<Vec<ContentHash>> {
        let store = self.clone();
        let keys = self.block(async move {
            retry_with(RetryPolicy::S3_DEFAULT, should_retry_store_error, || {
                store.list_with_prefix("blobs/")
            })
            .await
        })?;
        let mut hashes = Vec::new();
        for key in keys {
            if let Some(stem) = key
                .strip_prefix("blobs/")
                .and_then(|k| k.strip_suffix(".bin"))
                && let Ok(hash) = ContentHash::from_hex(stem)
            {
                hashes.push(hash);
            }
        }
        Ok(hashes)
    }

    // ── Tree ──────────────────────────────────────────────────────────────────

    fn get_tree(&self, hash: &ContentHash) -> Result<Option<Tree>> {
        let key = self.tree_key(hash);
        let hash = *hash;
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    match client.get_object().bucket(&bucket).key(&key).send().await {
                        Ok(response) => {
                            let data = response.body.collect().await.map_err(|e| {
                                StoreError::Io(std::io::Error::other(format!(
                                    "Failed to read S3 object body: {}",
                                    e
                                )))
                            })?;
                            let bytes = data.into_bytes();
                            let decoded = if crate::store::compression::is_compressed(&bytes) {
                                crate::store::compression::decompress(&bytes)?
                            } else {
                                bytes.to_vec()
                            };
                            let tree = validate_loaded_tree(rmp_serde::from_slice(&decoded)?)?;
                            if tree.hash() != hash {
                                return Err(StoreError::Corruption {
                                    expected: hash,
                                    found: tree.hash(),
                                });
                            }
                            Ok(Some(tree))
                        }
                        Err(e) => {
                            if e.as_service_error()
                                .map(|e| e.is_no_such_key())
                                .unwrap_or(false)
                            {
                                Ok(None)
                            } else {
                                Err(StoreError::Io(std::io::Error::other(format!(
                                    "S3 get_object failed: {}",
                                    e
                                ))))
                            }
                        }
                    }
                },
            )
            .await
        })
    }

    fn put_tree(&self, tree: &Tree) -> Result<ContentHash> {
        let hash = tree.hash();
        let key = self.tree_key(&hash);
        let serialized = rmp_serde::to_vec(tree)?;
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    match client.head_object().bucket(&bucket).key(&key).send().await {
                        Ok(_) => return Ok(hash),
                        Err(e) => {
                            if !e
                                .as_service_error()
                                .map(|e| e.is_not_found())
                                .unwrap_or(false)
                            {
                                return Err(StoreError::Io(std::io::Error::other(format!(
                                    "S3 head_object failed: {}",
                                    e
                                ))));
                            }
                        }
                    }
                    let compression_config = crate::store::CompressionConfig::default();
                    let data =
                        crate::store::compression::compress(&serialized, &compression_config)?
                            .unwrap_or(serialized.clone());
                    client
                        .put_object()
                        .bucket(&bucket)
                        .key(&key)
                        .body(ByteStream::from(data))
                        .send()
                        .await
                        .map_err(|e| {
                            StoreError::Io(std::io::Error::other(format!(
                                "S3 put_object failed: {}",
                                e
                            )))
                        })?;
                    Ok(hash)
                },
            )
            .await
        })
    }

    fn has_tree(&self, hash: &ContentHash) -> Result<bool> {
        let key = self.tree_key(hash);
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    match client.head_object().bucket(&bucket).key(&key).send().await {
                        Ok(_) => Ok(true),
                        Err(e) => {
                            if e.as_service_error()
                                .map(|e| e.is_not_found())
                                .unwrap_or(false)
                            {
                                Ok(false)
                            } else {
                                Err(StoreError::Io(std::io::Error::other(format!(
                                    "S3 head_object failed: {}",
                                    e
                                ))))
                            }
                        }
                    }
                },
            )
            .await
        })
    }

    fn list_trees(&self) -> Result<Vec<ContentHash>> {
        let store = self.clone();
        let keys = self.block(async move {
            retry_with(RetryPolicy::S3_DEFAULT, should_retry_store_error, || {
                store.list_with_prefix("trees/")
            })
            .await
        })?;
        let mut hashes = Vec::new();
        for key in keys {
            if let Some(stem) = key
                .strip_prefix("trees/")
                .and_then(|k| k.strip_suffix(".bin"))
                && let Ok(hash) = ContentHash::from_hex(stem)
            {
                hashes.push(hash);
            }
        }
        Ok(hashes)
    }

    // ── State ────────────────────────────────────────────────────────────────

    fn get_state(&self, id: &ChangeId) -> Result<Option<State>> {
        let key = self.state_key(id);
        let id = *id;
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    match client.get_object().bucket(&bucket).key(&key).send().await {
                        Ok(response) => {
                            let data = response.body.collect().await.map_err(|e| {
                                StoreError::Io(std::io::Error::other(format!(
                                    "Failed to read S3 object body: {}",
                                    e
                                )))
                            })?;
                            let bytes = data.into_bytes();
                            let decoded = if crate::store::compression::is_compressed(&bytes) {
                                crate::store::compression::decompress(&bytes)?
                            } else {
                                bytes.to_vec()
                            };
                            let state =
                                validate_loaded_state(&id, rmp_serde::from_slice(&decoded)?)?;
                            Ok(Some(state))
                        }
                        Err(e) => {
                            if e.as_service_error()
                                .map(|e| e.is_no_such_key())
                                .unwrap_or(false)
                            {
                                Ok(None)
                            } else {
                                Err(StoreError::Io(std::io::Error::other(format!(
                                    "S3 get_object failed: {}",
                                    e
                                ))))
                            }
                        }
                    }
                },
            )
            .await
        })
    }

    fn put_state(&self, state: &State) -> Result<()> {
        let key = self.state_key(&state.change_id);
        let serialized = rmp_serde::to_vec(state)?;
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    let compression_config = crate::store::CompressionConfig::default();
                    let data =
                        crate::store::compression::compress(&serialized, &compression_config)?
                            .unwrap_or(serialized.clone());
                    client
                        .put_object()
                        .bucket(&bucket)
                        .key(&key)
                        .body(ByteStream::from(data))
                        .send()
                        .await
                        .map_err(|e| {
                            StoreError::Io(std::io::Error::other(format!(
                                "S3 put_object failed: {}",
                                e
                            )))
                        })?;
                    Ok(())
                },
            )
            .await
        })
    }

    fn has_state(&self, id: &ChangeId) -> Result<bool> {
        let key = self.state_key(id);
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    match client.head_object().bucket(&bucket).key(&key).send().await {
                        Ok(_) => Ok(true),
                        Err(e) => {
                            if e.as_service_error()
                                .map(|e| e.is_not_found())
                                .unwrap_or(false)
                            {
                                Ok(false)
                            } else {
                                Err(StoreError::Io(std::io::Error::other(format!(
                                    "S3 head_object failed: {}",
                                    e
                                ))))
                            }
                        }
                    }
                },
            )
            .await
        })
    }

    fn list_states(&self) -> Result<Vec<ChangeId>> {
        let store = self.clone();
        let keys = self.block(async move {
            retry_with(RetryPolicy::S3_DEFAULT, should_retry_store_error, || {
                store.list_with_prefix("states/")
            })
            .await
        })?;
        let mut ids = Vec::new();
        for key in keys {
            if let Some(stem) = key
                .strip_prefix("states/")
                .and_then(|k| k.strip_suffix(".bin"))
                && let Ok(id) = ChangeId::parse(stem)
            {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    // ── Action ───────────────────────────────────────────────────────────────

    fn get_action(&self, id: &ActionId) -> Result<Option<Action>> {
        let key = self.action_key(id);
        let id = *id;
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    match client.get_object().bucket(&bucket).key(&key).send().await {
                        Ok(response) => {
                            let data = response.body.collect().await.map_err(|e| {
                                StoreError::Io(std::io::Error::other(format!(
                                    "Failed to read S3 object body: {}",
                                    e
                                )))
                            })?;
                            let bytes = data.into_bytes();
                            let decoded = if crate::store::compression::is_compressed(&bytes) {
                                crate::store::compression::decompress(&bytes)?
                            } else {
                                bytes.to_vec()
                            };
                            let action =
                                validate_loaded_action(&id, rmp_serde::from_slice(&decoded)?)?;
                            Ok(Some(action))
                        }
                        Err(e) => {
                            if e.as_service_error()
                                .map(|e| e.is_no_such_key())
                                .unwrap_or(false)
                            {
                                Ok(None)
                            } else {
                                Err(StoreError::Io(std::io::Error::other(format!(
                                    "S3 get_object failed: {}",
                                    e
                                ))))
                            }
                        }
                    }
                },
            )
            .await
        })
    }

    fn put_action(&self, action: &mut Action) -> Result<ActionId> {
        let id = action.id();
        let key = self.action_key(&id);
        let serialized = rmp_serde::to_vec(action)?;
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    let compression_config = crate::store::CompressionConfig::default();
                    let data =
                        crate::store::compression::compress(&serialized, &compression_config)?
                            .unwrap_or(serialized.clone());
                    client
                        .put_object()
                        .bucket(&bucket)
                        .key(&key)
                        .body(ByteStream::from(data))
                        .send()
                        .await
                        .map_err(|e| {
                            StoreError::Io(std::io::Error::other(format!(
                                "S3 put_object failed: {}",
                                e
                            )))
                        })?;
                    Ok(id)
                },
            )
            .await
        })
    }

    fn list_actions(&self) -> Result<Vec<ActionId>> {
        let store = self.clone();
        let keys = self.block(async move {
            retry_with(RetryPolicy::S3_DEFAULT, should_retry_store_error, || {
                store.list_with_prefix("actions/")
            })
            .await
        })?;
        let mut ids = Vec::new();
        for key in keys {
            if let Some(stem) = key
                .strip_prefix("actions/")
                .and_then(|k| k.strip_suffix(".bin"))
                && let Ok(hash) = ContentHash::from_hex(stem)
            {
                let id = ActionId::from_hash(hash);
                ids.push(id);
            }
        }
        Ok(ids)
    }

    // ── Annotated tag sidecar ──────────────────────────────────────────────
    //
    // Per-marker sidecar holding an annotated git tag object's metadata (#564
    // step 1, #565), keyed by hex(marker name) under `marker-tags/` to match
    // the fs backend. The bridge import writes this UNCONDITIONALLY for every
    // annotated tag, so without these overrides an S3-backed repo would hit the
    // trait's "unsupported" default and fail every annotated-tag import.

    fn has_annotated_tag_for_marker(&self, marker: &str) -> Result<bool> {
        let key = self.marker_tag_key(marker);
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    match client.head_object().bucket(&bucket).key(&key).send().await {
                        Ok(_) => Ok(true),
                        Err(e) => {
                            if e.as_service_error()
                                .map(|e| e.is_not_found())
                                .unwrap_or(false)
                            {
                                Ok(false)
                            } else {
                                Err(StoreError::Io(std::io::Error::other(format!(
                                    "S3 head_object failed: {}",
                                    e
                                ))))
                            }
                        }
                    }
                },
            )
            .await
        })
    }

    fn get_annotated_tag_bytes_for_marker(&self, marker: &str) -> Result<Option<Vec<u8>>> {
        let key = self.marker_tag_key(marker);
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    match client.get_object().bucket(&bucket).key(&key).send().await {
                        Ok(response) => {
                            let data = response.body.collect().await.map_err(|e| {
                                StoreError::Io(std::io::Error::other(format!(
                                    "Failed to read S3 object body: {}",
                                    e
                                )))
                            })?;
                            Ok(Some(data.into_bytes().to_vec()))
                        }
                        Err(e) => {
                            if e.as_service_error()
                                .map(|e| e.is_no_such_key())
                                .unwrap_or(false)
                            {
                                Ok(None)
                            } else {
                                Err(StoreError::Io(std::io::Error::other(format!(
                                    "S3 get_object failed: {}",
                                    e
                                ))))
                            }
                        }
                    }
                },
            )
            .await
        })
    }

    fn put_annotated_tag_bytes_for_marker(&self, marker: &str, bytes: &[u8]) -> Result<()> {
        let key = self.marker_tag_key(marker);
        let body = bytes.to_vec();
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    client
                        .put_object()
                        .bucket(&bucket)
                        .key(&key)
                        .body(ByteStream::from(body.clone()))
                        .send()
                        .await
                        .map_err(|e| {
                            StoreError::Io(std::io::Error::other(format!(
                                "S3 put_object failed: {}",
                                e
                            )))
                        })?;
                    Ok(())
                },
            )
            .await
        })
    }

    fn delete_annotated_tag_for_marker(&self, marker: &str) -> Result<()> {
        // S3 delete is idempotent — removing a missing key returns success, so
        // this is a no-op for lightweight tags, matching the fs backend.
        let key = self.marker_tag_key(marker);
        let client = Arc::clone(&self.client);
        let bucket = self.bucket.clone();
        self.block(async move {
            retry_with(
                RetryPolicy::S3_DEFAULT,
                should_retry_store_error,
                || async {
                    client
                        .delete_object()
                        .bucket(&bucket)
                        .key(&key)
                        .send()
                        .await
                        .map_err(|e| {
                            StoreError::Io(std::io::Error::other(format!(
                                "S3 delete_object failed: {}",
                                e
                            )))
                        })?;
                    Ok(())
                },
            )
            .await
        })
    }

    fn list_markers_with_annotated_tag(&self) -> Result<Vec<String>> {
        let store = self.clone();
        let keys = self.block(async move {
            retry_with(RetryPolicy::S3_DEFAULT, should_retry_store_error, || {
                store.list_with_prefix("marker-tags/")
            })
            .await
        })?;
        let mut names = Vec::new();
        for key in keys {
            // Filenames are hex(marker_name_utf8); recover the name.
            if let Some(stem) = key
                .strip_prefix("marker-tags/")
                .and_then(|k| k.strip_suffix(".bin"))
                && let Ok(bytes) = hex::decode(stem)
                && let Ok(name) = String::from_utf8(bytes)
            {
                names.push(name);
            }
        }
        Ok(names)
    }
}
