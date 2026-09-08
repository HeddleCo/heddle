//! Async durable-store boundary shared by device and hosted replication.
use std::{collections::BTreeSet, future::Future};

use crypto::thread_operation::SignedOperation;
use heddle_object_model::object::{
    ContentHash,
    thread_replication::{Admission, ThreadFacet},
};

/// Implementations commit before returning receipts. A store is bound to one
/// authorized Thread; operations and peer metadata may never escape that scope.
/// Notification delivery is separate from this durable contract.
pub trait ReplicaStore: Clone + Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;
    fn thread_id(&self) -> ContentHash;
    fn generation(&self) -> impl Future<Output = Result<i64, Self::Error>> + Send;
    fn sharing(
        &self,
        destination: [u8; 32],
    ) -> impl Future<Output = Result<BTreeSet<ThreadFacet>, Self::Error>> + Send;
    fn frontier_page(
        &self,
        facet: ThreadFacet,
        after: Option<ContentHash>,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<ContentHash>, Self::Error>> + Send;
    fn operation(
        &self,
        id: ContentHash,
    ) -> impl Future<Output = Result<Option<(SignedOperation, Admission)>, Self::Error>> + Send;
    fn receive(
        &self,
        operation: SignedOperation,
    ) -> impl Future<Output = Result<Admission, Self::Error>> + Send;
    fn remember_peer_heads(
        &self,
        peer: [u8; 32],
        heads: Vec<(ThreadFacet, ContentHash)>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn record_peer_receipt(
        &self,
        peer: [u8; 32],
        id: ContentHash,
        admission: Admission,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
    fn settled_peer_heads(
        &self,
        peer: [u8; 32],
        facets: BTreeSet<ThreadFacet>,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<(ContentHash, Admission)>, Self::Error>> + Send;
    fn needed_from_peer(
        &self,
        peer: [u8; 32],
        facets: BTreeSet<ThreadFacet>,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<ContentHash>, Self::Error>> + Send;
}
