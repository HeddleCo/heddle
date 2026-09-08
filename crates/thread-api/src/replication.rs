// SPDX-License-Identifier: Apache-2.0
//! A bounded, transport-neutral causal exchange shared by device and hosted
//! endpoints. The RPC adapter supplies the verified request scope.
use std::collections::BTreeSet;

use objects::{
    object::{
        ContentHash,
        thread_replication::{OPERATION_FORMAT, ThreadFacet},
    },
    store::ObjectStore,
};
use repo::thread_replication::{Admission, SignedOperation, ThreadReplica};

use crate::contract::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] repo::thread_replication::Error),
    #[error("replication protocol: {0}")]
    Protocol(&'static str),
}
pub type Result<T> = std::result::Result<T, Error>;

/// Internal frames carry the same typed payloads in both directions.
pub enum Frame {
    Have(ReplicationHave),
    Need(ReplicationNeed),
    Operations(ReplicationOperations),
    Receipt(ReplicationReceipt),
}
impl Frame {
    pub fn request(self) -> ReplicateThreadRequest {
        use replicate_thread_request::Body;
        ReplicateThreadRequest {
            body: Some(match self {
                Self::Have(v) => Body::Have(v),
                Self::Need(v) => Body::Need(v),
                Self::Operations(v) => Body::Operations(v),
                Self::Receipt(v) => Body::Receipt(v),
            }),
        }
    }
    pub fn response(self) -> ReplicateThreadResponse {
        use replicate_thread_response::Body;
        ReplicateThreadResponse {
            body: Some(match self {
                Self::Have(v) => Body::Have(v),
                Self::Need(v) => Body::Need(v),
                Self::Operations(v) => Body::Operations(v),
                Self::Receipt(v) => Body::Receipt(v),
            }),
        }
    }
    pub fn from_request(request: ReplicateThreadRequest) -> Result<Self> {
        use replicate_thread_request::Body;
        Ok(match request.body {
            Some(Body::Have(v)) => Self::Have(v),
            Some(Body::Need(v)) => Self::Need(v),
            Some(Body::Operations(v)) => Self::Operations(v),
            Some(Body::Receipt(v)) => Self::Receipt(v),
            _ => return Err(Error::Protocol("unexpected replication opening")),
        })
    }
    pub fn from_response(response: ReplicateThreadResponse) -> Result<Self> {
        use replicate_thread_response::Body;
        Ok(match response.body {
            Some(Body::Have(v)) => Self::Have(v),
            Some(Body::Need(v)) => Self::Need(v),
            Some(Body::Operations(v)) => Self::Operations(v),
            Some(Body::Receipt(v)) => Self::Receipt(v),
            _ => return Err(Error::Protocol("unexpected replication ready")),
        })
    }
}

pub enum Outbound {
    Frame(Frame),
    Operation(ContentHash),
}

#[derive(Clone)]
pub struct Session {
    pub replica: ThreadReplica,
    destination: [u8; 32],
    facets: BTreeSet<ThreadFacet>,
    max_items: usize,
    generation: i64,
    announce_generation: i64,
    announce_facet: usize,
    after: Option<ContentHash>,
}
impl Session {
    /// `facets` is the intersection of authenticated admission scope and the
    /// negotiated opening. Local export still checks current sharing policy.
    pub fn new(
        replica: ThreadReplica,
        destination: [u8; 32],
        facets: BTreeSet<ThreadFacet>,
        max_items: usize,
    ) -> Result<Self> {
        if max_items == 0 || max_items > 64 {
            return Err(Error::Protocol("replication batch must be 1..64"));
        }
        Ok(Self {
            replica,
            destination,
            facets,
            max_items,
            generation: -1,
            announce_generation: -1,
            announce_facet: 0,
            after: None,
        })
    }
    pub fn export_facets(&self) -> Result<BTreeSet<ThreadFacet>> {
        let (sharing, _) = self.replica.sharing(&self.destination)?;
        Ok(sharing.intersection(&self.facets).copied().collect())
    }
    /// At most one bounded frontier page. Call again until None. A generation
    /// change during a paged announcement starts another round, closing gaps.
    pub fn announcement(&mut self) -> Result<Option<Frame>> {
        let current = self.replica.generation()?;
        if self.generation == current && self.announce_generation < 0 {
            return Ok(None);
        }
        if self.announce_generation < 0 {
            self.announce_generation = current;
            self.announce_facet = 0;
            self.after = None;
        }
        let sharing = self.export_facets()?;
        let facets = [ThreadFacet::Source, ThreadFacet::Discussion];
        while self.announce_facet < facets.len() {
            let facet = facets[self.announce_facet];
            if !sharing.contains(&facet) {
                self.announce_facet += 1;
                self.after = None;
                continue;
            }
            let page = self
                .replica
                .frontier_page(facet, self.after, self.max_items)?;
            if page.is_empty() {
                self.announce_facet += 1;
                self.after = None;
                continue;
            }
            self.after = page.last().copied();
            return Ok(Some(Frame::Have(ReplicationHave {
                frontiers: vec![CausalFrontier {
                    facet: wire_facet(facet),
                    heads: page.into_iter().map(|id| id.as_bytes().to_vec()).collect(),
                }],
            })));
        }
        self.generation = self.announce_generation;
        self.announce_generation = -1;
        Ok(None)
    }
    pub fn handle(&mut self, frame: Frame, store: &impl ObjectStore) -> Result<Vec<Outbound>> {
        let mut responses = Vec::new();
        match frame {
            Frame::Have(have) => {
                let count: usize = have.frontiers.iter().map(|f| f.heads.len()).sum();
                self.check_count(count)?;
                let mut need = BTreeSet::new();
                for frontier in have.frontiers {
                    let facet = native_facet(frontier.facet)?;
                    if !self.facets.contains(&facet) {
                        return Err(Error::Protocol("unnegotiated facet"));
                    }
                    for bytes in frontier.heads {
                        let id = hash(&bytes)?;
                        match self.replica.operation(&id)? {
                            Some((_, Admission::Pending)) => {
                                need.extend(self.replica.missing_ancestors(id, self.max_items)?);
                            }
                            Some((_, Admission::Accepted | Admission::Rejected(_))) => {}
                            None => {
                                need.insert(id);
                            }
                        }
                    }
                }
                for ids in need.into_iter().collect::<Vec<_>>().chunks(self.max_items) {
                    responses.push(Outbound::Frame(Frame::Need(ReplicationNeed {
                        operation_ids: ids.iter().map(|id| id.as_bytes().to_vec()).collect(),
                    })));
                }
            }
            Frame::Need(need) => {
                self.check_count(need.operation_ids.len())?;
                let sharing = self.export_facets()?;
                for bytes in need.operation_ids {
                    let id = hash(&bytes)?;
                    let Some((record, Admission::Accepted)) = self.replica.operation(&id)? else {
                        return Err(Error::Protocol("requested operation unavailable"));
                    };
                    let operation = record.verify()?;
                    if !sharing.contains(&operation.facet()) {
                        return Err(Error::Protocol(
                            "operation is outside current sharing policy",
                        ));
                    }
                    responses.push(Outbound::Operation(id));
                }
            }
            Frame::Operations(batch) => {
                self.check_count(batch.operations.len())?;
                let mut receipt = ReplicationReceipt::default();
                for record in batch.operations {
                    let signed = decode_record(record)?;
                    let operation = signed.verify()?;
                    let id = operation
                        .id()
                        .map_err(repo::thread_replication::Error::from)?;
                    if !self.facets.contains(&operation.facet()) {
                        return Err(Error::Protocol("operation is outside admission scope"));
                    }
                    match self.replica.receive(&signed, store, |_| Ok(()))? {
                        Admission::Accepted => {
                            receipt.accepted_operation_ids.push(id.as_bytes().to_vec())
                        }
                        Admission::Pending => {
                            receipt.pending_operation_ids.push(id.as_bytes().to_vec());
                            let missing = self.replica.missing_ancestors(id, self.max_items)?;
                            for ids in missing.chunks(self.max_items) {
                                responses.push(Outbound::Frame(Frame::Need(ReplicationNeed {
                                    operation_ids: ids
                                        .iter()
                                        .map(|id| id.as_bytes().to_vec())
                                        .collect(),
                                })));
                            }
                        }
                        Admission::Rejected(message) => {
                            receipt.rejected.push(ReplicationRejection {
                                operation_id: id.as_bytes().to_vec(),
                                failure: Some(api::heddle::api::v1alpha1::CallFailure {
                                    code: 9,
                                    message,
                                    ..Default::default()
                                }),
                            })
                        }
                    }
                }
                responses.push(Outbound::Frame(Frame::Receipt(receipt)));
            }
            Frame::Receipt(receipt) => {
                self.check_count(
                    receipt.accepted_operation_ids.len()
                        + receipt.pending_operation_ids.len()
                        + receipt.rejected.len(),
                )?;
                // A peer receipt is a destination assertion, never local
                // acceptance or proof that source blobs are available.
                for bytes in receipt.accepted_operation_ids {
                    let id = hash(&bytes)?;
                    if !matches!(self.replica.operation(&id)?, Some((_, Admission::Accepted))) {
                        return Err(Error::Protocol("receipt names an unknown local operation"));
                    }
                }
            }
        }
        Ok(responses)
    }
    /// Resolve payload only when the writer is ready. Pending output queues
    /// contain IDs, and policy is checked again immediately before disclosure.
    pub fn export_operation(&self, id: ContentHash) -> Result<Frame> {
        let Some((record, Admission::Accepted)) = self.replica.operation(&id)? else {
            return Err(Error::Protocol("requested operation unavailable"));
        };
        let operation = record.verify()?;
        if !self.export_facets()?.contains(&operation.facet()) {
            return Err(Error::Protocol(
                "operation is outside current sharing policy",
            ));
        }
        Ok(Frame::Operations(ReplicationOperations {
            operations: vec![SignedRecord {
                format: OPERATION_FORMAT.into(),
                canonical_record: record.canonical,
                signatures: vec![RecordSignature {
                    public_key: operation.publisher.to_vec(),
                    signature: record.signature,
                }],
            }],
        }))
    }

    fn check_count(&self, count: usize) -> Result<()> {
        if count > self.max_items {
            Err(Error::Protocol("replication item budget exceeded"))
        } else {
            Ok(())
        }
    }
}

pub fn decode_record(record: SignedRecord) -> Result<SignedOperation> {
    if record.format != OPERATION_FORMAT || record.signatures.len() != 1 {
        return Err(Error::Protocol("unsupported signed operation format"));
    }
    let signature = &record.signatures[0];
    let signed = SignedOperation {
        canonical: record.canonical_record,
        signature: signature.signature.clone(),
    };
    if signed.verify()?.publisher.as_slice() != signature.public_key {
        return Err(Error::Protocol(
            "record signature key differs from publisher",
        ));
    }
    Ok(signed)
}
pub fn native_facet(facet: i32) -> Result<ThreadFacet> {
    match SharedFacet::try_from(facet) {
        Ok(SharedFacet::Source) => Ok(ThreadFacet::Source),
        Ok(SharedFacet::Collaboration) => Ok(ThreadFacet::Discussion),
        _ => Err(Error::Protocol("unsupported replication facet")),
    }
}
pub fn wire_facet(facet: ThreadFacet) -> i32 {
    match facet {
        ThreadFacet::Source => SharedFacet::Source as i32,
        ThreadFacet::Discussion => SharedFacet::Collaboration as i32,
    }
}
fn hash(bytes: &[u8]) -> Result<ContentHash> {
    Ok(ContentHash::from_bytes(bytes.try_into().map_err(|_| {
        Error::Protocol("operation ID must be 32 bytes")
    })?))
}

#[cfg(test)]
#[path = "replication_tests.rs"]
mod tests;
