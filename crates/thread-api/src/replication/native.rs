//! Local SQLite adapter. Blocking work stays off the stream runtime.
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

use crypto::thread_operation::SignedOperation;
use heddle_object_model::object::{
    ContentHash,
    thread_replication::{Admission, ThreadFacet},
};
use objects::store::ObjectStore;
use repo::thread_replication::ThreadReplica;

use super::store::ReplicaStore;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] repo::thread_replication::Error),
    #[error("local replica worker: {0}")]
    Worker(#[from] tokio::task::JoinError),
}
pub struct LocalReplica<S> {
    replica: ThreadReplica,
    objects: Arc<S>,
    authority_home: Option<PathBuf>,
}
impl<S> Clone for LocalReplica<S> {
    fn clone(&self) -> Self {
        Self {
            replica: self.replica.clone(),
            objects: self.objects.clone(),
            authority_home: self.authority_home.clone(),
        }
    }
}
impl<S: ObjectStore + Send + Sync + 'static> LocalReplica<S> {
    pub fn new(replica: ThreadReplica, objects: Arc<S>) -> Self {
        Self {
            replica,
            objects,
            authority_home: None,
        }
    }
    /// Use independently enrolled local account authority for original metadata
    /// authors. Without this binding, new metadata admission fails closed.
    pub fn with_device_authority(mut self, home: PathBuf) -> Self {
        self.authority_home = Some(home);
        self
    }
    async fn execute<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&ThreadReplica, &S) -> repo::thread_replication::Result<T>
        + Send
        + 'static,
    ) -> Result<T, Error> {
        let local = self.clone();
        Ok(
            tokio::task::spawn_blocking(move || operation(&local.replica, &local.objects))
                .await??,
        )
    }
}
impl<S: ObjectStore + Send + Sync + 'static> ReplicaStore for LocalReplica<S> {
    type Error = Error;
    fn thread_id(&self) -> ContentHash {
        self.replica.thread_id()
    }
    async fn generation(&self) -> Result<i64, Error> {
        self.execute(|replica, _| replica.generation()).await
    }
    async fn sharing(&self, destination: [u8; 32]) -> Result<BTreeSet<ThreadFacet>, Error> {
        self.execute(move |replica, _| replica.sharing(&destination).map(|(facets, _)| facets))
            .await
    }
    async fn frontier_page(
        &self,
        facet: ThreadFacet,
        after: Option<ContentHash>,
        limit: usize,
    ) -> Result<Vec<ContentHash>, Error> {
        self.execute(move |replica, _| replica.frontier_page(facet, after, limit))
            .await
    }
    async fn operation(
        &self,
        id: ContentHash,
    ) -> Result<Option<(SignedOperation, Admission)>, Error> {
        self.execute(move |replica, _| replica.operation(&id)).await
    }
    async fn receive(&self, operation: SignedOperation) -> Result<Admission, Error> {
        let authority_home = self.authority_home.clone();
        self.execute(move |replica, objects| {
            replica.receive(&operation, objects, |native| {
                use heddle_object_model::object::thread_replication::ThreadOperationBody;
                if !matches!(native.body, ThreadOperationBody::Metadata(_)) {
                    return Ok(());
                }
                // Durable original-author receipt remains valid while causal
                // parents arrive later. Neither the envelope nor claimed time
                // can synthesize the atomically retained admission marker.
                if replica.control_authority_admitted(&operation)? {
                    return Ok(());
                }
                let home = authority_home.as_ref().ok_or_else(|| {
                    repo::thread_replication::Error::Invalid(
                        "metadata admission requires independently enrolled account authority"
                            .into(),
                    )
                })?;
                let now = chrono::Utc::now().timestamp();
                let authority = repo::device_authority::load(home, now).map_err(authority_error)?;
                let genesis = replica.genesis()?;
                if let ThreadOperationBody::Metadata(bytes) = &native.body {
                    heddle_object_model::object::thread_replication::metadata::ThreadControl::decode(bytes)?.validate_parents(&genesis, &[])?;
                }
                let spool = genesis.spool.parse().map_err(authority_error)?;
                let registered =
                    repo::device_catalog::load(home, spool).map_err(authority_error)?;
                repo::thread_replication::metadata::verify_control_authority(
                    native,
                    &authority,
                    &registered.capability_path,
                    now,
                )
            })
        })
        .await
    }
    async fn remember_peer_heads(
        &self,
        peer: [u8; 32],
        heads: Vec<(ThreadFacet, ContentHash)>,
    ) -> Result<(), Error> {
        self.execute(move |replica, _| replica.remember_peer_heads(peer, &heads))
            .await
    }
    async fn record_peer_receipt(
        &self,
        peer: [u8; 32],
        id: ContentHash,
        admission: Admission,
    ) -> Result<(), Error> {
        self.execute(move |replica, _| replica.record_peer_receipt(peer, id, &admission))
            .await
    }
    async fn settled_peer_heads(
        &self,
        peer: [u8; 32],
        facets: BTreeSet<ThreadFacet>,
        limit: usize,
    ) -> Result<Vec<(ContentHash, Admission)>, Error> {
        self.execute(move |replica, _| replica.settled_peer_heads(peer, &facets, limit))
            .await
    }
    async fn needed_from_peer(
        &self,
        peer: [u8; 32],
        facets: BTreeSet<ThreadFacet>,
        limit: usize,
    ) -> Result<Vec<ContentHash>, Error> {
        self.execute(move |replica, _| replica.needed_from_peer(peer, &facets, limit))
            .await
    }
}

fn authority_error(error: impl std::fmt::Display) -> repo::thread_replication::Error {
    repo::thread_replication::Error::Invalid(error.to_string())
}
