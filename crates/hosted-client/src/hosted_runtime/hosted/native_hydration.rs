//! Exact-revision hydration uses the same bounded source stream as Tapestry.
use api::heddle::api::{v1alpha1::StateId as ProtoStateId, v2alpha1 as contract};
use objects::{
    object::{Blob, ContentHash, StateId},
    store::ObjectStore,
};
use repo::Repository;
use thread_api::content::BlobSource;
use wire::ProtocolError;

use super::HostedClient;

impl HostedClient {
    /// Hydrate a path in an explicitly selected immutable revision.
    pub async fn hydrate_blob_at_path(
        &self,
        repo: &Repository,
        spool_address: &str,
        revision: StateId,
        path: &str,
    ) -> Result<Blob, ProtocolError> {
        let blob = self
            .read_native_blob(spool_address, revision, BlobSource::Path(path.into()))
            .await?;
        repo.store().put_blob(&blob)?;
        repo.clear_missing_blob(&blob.hash())?;
        Ok(blob)
    }

    /// Fetch one missing object without resolving a mutable Thread or filename.
    /// Store bytes and clear the missing marker only after the complete selection
    /// and the native content hash have both been verified.
    pub async fn hydrate_blob(
        &self,
        repo: &Repository,
        spool_address: &str,
        revision: StateId,
        hash: ContentHash,
    ) -> Result<usize, ProtocolError> {
        if repo.store().has_blob(&hash)? {
            repo.clear_missing_blob(&hash)?;
            return Ok(0);
        }
        let blob = self
            .read_native_blob(
                spool_address,
                revision,
                BlobSource::ObjectHash(hash.as_bytes().to_vec()),
            )
            .await?;
        repo.store().put_blob(&blob)?;
        repo.clear_missing_blob(&hash)?;
        Ok(1)
    }

    async fn read_native_blob(
        &self,
        spool_address: &str,
        revision: StateId,
        source: BlobSource,
    ) -> Result<Blob, ProtocolError> {
        let spool = self.resolve_spool_ref(spool_address).await?;
        let remote = self.native().await.map_err(invalid)?;
        let mut blobs = remote
            .read_blobs(
                contract::RevisionRef {
                    spool: Some(spool),
                    revision: Some(contract::revision_ref::Revision::State(ProtoStateId {
                        value: revision.as_bytes().to_vec(),
                    })),
                },
                vec![source],
            )
            .await
            .map_err(invalid)?;
        if blobs.len() != 1 {
            return Err(invalid("content response differs from requested selection"));
        }
        let selected = blobs
            .pop()
            .ok_or_else(|| invalid("content selection absent"))?;
        let blob = Blob::new(selected.bytes);
        if blob.hash().as_bytes().as_slice() != selected.object_hash {
            return Err(invalid(
                "content bytes do not match the selected native blob hash",
            ));
        }
        Ok(blob)
    }
}

fn invalid(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::InvalidState(error.to_string())
}
