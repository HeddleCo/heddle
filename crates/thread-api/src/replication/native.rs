//! Local SQLite adapter. Blocking work stays off the stream runtime.
use std::{collections::BTreeSet, path::PathBuf, sync::Arc};

use heddle_object_model::object::{
    ContentHash,
    thread_replication::{Admission, ThreadFacet},
};
use objects::store::ObjectStore;
use repo::thread_replication::ThreadReplica;

use super::store::{ReceivedOperation, ReplicaStore};

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
    ) -> Result<Option<(ReceivedOperation, Admission)>, Error> {
        self.execute(move |replica, _| {
            Ok(replica
                .operation_with_authority_admission(&id)?
                .map(|stored| {
                    (
                        ReceivedOperation {
                            original: stored.original,
                            authority_admission: stored.authority_admission,
                        },
                        stored.status,
                    )
                }))
        })
        .await
    }
    async fn receive(&self, received: ReceivedOperation) -> Result<Admission, Error> {
        let authority_home = self.authority_home.clone();
        self.execute(move |replica, objects| {
            let operation = received.original;
            if let Some(receipt) = received.authority_admission {
                return replica.receive_with_authority_admission(&operation, &receipt, objects, |_| Ok(()));
            }
            replica.receive(&operation, objects, |native| {
                use heddle_object_model::object::thread_replication::ThreadOperationBody;
                if native.local_integration()?.is_some() && native.publisher != replica.genesis()?.creator {
                    return Err(repo::thread_replication::Error::Invalid("fresh local integration requires independently admitted author authority".into()));
                }
                if !matches!(native.body, ThreadOperationBody::Metadata(_) | ThreadOperationBody::Capture(_)) {
                    return Ok(());
                }
                // Durable original-author receipt remains valid while causal
                // parents arrive later. Neither the envelope nor claimed time
                // can synthesize the atomically retained admission marker.
                if replica.original_authority_admitted(&operation)? {
                    return Ok(());
                }
                let genesis = replica.genesis()?;
                if matches!(&native.body, ThreadOperationBody::Capture(capture) if matches!(capture.author, heddle_object_model::object::thread_replication::SourceAuthor::LocalKey)) {
                    return repo::thread_replication::source_authority::verify_local_source_owner(native, &genesis);
                }
                let home = authority_home.as_ref().ok_or_else(|| {
                    repo::thread_replication::Error::Invalid(
                        "original operation requires independently enrolled account authority"
                            .into(),
                    )
                })?;
                let now = chrono::Utc::now().timestamp();
                let authority = repo::device_authority::load(home, now).map_err(authority_error)?;
                if let ThreadOperationBody::Metadata(bytes) = &native.body {
                    heddle_object_model::object::thread_replication::metadata::ThreadControl::decode(bytes)?.validate_parents(&genesis, &[])?;
                }
                let spool = genesis.spool.parse().map_err(authority_error)?;
                let registered =
                    repo::device_catalog::load(home, spool).map_err(authority_error)?;
                if matches!(native.body, ThreadOperationBody::Capture(_)) {
                    repo::thread_replication::source_authority::verify_source_authority(native, &genesis, &authority, &registered.capability_path, now)
                } else {
                    repo::thread_replication::metadata::verify_control_authority(native, &authority, &registered.capability_path, now)
                }

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
