// SPDX-License-Identifier: Apache-2.0
//! A bounded, transport-neutral causal exchange shared by device and hosted
//! endpoints. The RPC adapter supplies the verified request scope.
use std::collections::BTreeSet;

use crypto::thread_operation::SignedOperation;
use heddle_object_model::object::{
    ContentHash,
    thread_replication::{Admission, OPERATION_FORMAT, ThreadFacet},
};
#[cfg(feature = "native")]
pub mod native;
pub mod opening;
pub mod ownership;
pub mod store;
use store::ReplicaStore;

use crate::contract::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Operation(#[from] heddle_object_model::error::HeddleError),
    #[error(transparent)]
    Signature(#[from] crypto::thread_operation::Error),
    #[error("replication protocol: {0}")]
    Protocol(&'static str),
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError<E: std::error::Error + 'static> {
    #[error("replica store: {0}")]
    Store(#[source] E),
    #[error(transparent)]
    Protocol(#[from] Error),
}
pub type StoreResult<T, E> = std::result::Result<T, StoreError<E>>;

/// Internal frames carry the same typed payloads in both directions.
pub enum Frame {
    Have(ReplicationHave),
    Need(ReplicationNeed),
    Operations(ReplicationOperations),
    Receipt(ReplicationReceipt),
}
/// Validated originals retain shared evidence without re-encoding a wire batch
/// for every committed-prefix unit.
pub(crate) enum InputUnit {
    Frame(Frame),
    Operation(store::ReceivedOperation),
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
pub struct Session<B: ReplicaStore> {
    pub replica: B,
    destination: [u8; 32],
    facets: BTreeSet<ThreadFacet>,
    max_items: usize,
    in_flight: BTreeSet<ContentHash>,
    generation: i64,
    announce_generation: i64,
    announce_facet: usize,
    after: Option<ContentHash>,
    pending_input_bookkeeping: Option<(ThreadFacet, ContentHash)>,
}
impl<B: ReplicaStore> Session<B> {
    /// `facets` is the intersection of authenticated admission scope and the
    /// negotiated opening. Local export still checks current sharing policy.
    pub fn new(
        replica: B,
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
            in_flight: BTreeSet::new(),
            generation: -1,
            announce_generation: -1,
            announce_facet: 0,
            after: None,
            pending_input_bookkeeping: None,
        })
    }
    pub async fn export_facets(&self) -> StoreResult<BTreeSet<ThreadFacet>, B::Error> {
        let sharing = self
            .replica
            .sharing(self.destination)
            .await
            .map_err(StoreError::Store)?;
        Ok(sharing.intersection(&self.facets).copied().collect())
    }
    /// At most one bounded frontier page. Call again until None. A generation
    /// change during a paged announcement starts another round, closing gaps.
    pub async fn announcement(&mut self) -> StoreResult<Option<Frame>, B::Error> {
        let current = self.replica.generation().await.map_err(StoreError::Store)?;
        if self.generation == current && self.announce_generation < 0 {
            return Ok(None);
        }
        if self.announce_generation < 0 {
            self.announce_generation = current;
            self.announce_facet = 0;
            self.after = None;
        }
        let sharing = self.export_facets().await?;
        let facets = ThreadFacet::ALL;
        while self.announce_facet < facets.len() {
            let facet = facets[self.announce_facet];
            if !sharing.contains(&facet) {
                self.announce_facet += 1;
                self.after = None;
                continue;
            }
            let page = self
                .replica
                .frontier_page(facet, self.after, self.max_items)
                .await
                .map_err(StoreError::Store)?;
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
        // A watcher may already have delivered a mutation that happened while
        // this round was being paged. Keep the caller draining until a fresh
        // round covers it; None must mean the announcement is caught up.
        Ok(
            (self.replica.generation().await.map_err(StoreError::Store)? != self.generation)
                .then(|| Frame::Have(ReplicationHave::default())),
        )
    }
    pub async fn handle(&mut self, frame: Frame) -> StoreResult<Vec<Outbound>, B::Error> {
        self.handle_frame(frame, true).await
    }
    /// Validate the complete wire batch before committing any prefix. Each
    /// returned unit can then receive its own durable acknowledgement.
    pub(crate) fn input_units(&self, frame: Frame) -> StoreResult<Vec<InputUnit>, B::Error> {
        let Frame::Operations(batch) = frame else {
            return Ok(vec![InputUnit::Frame(frame)]);
        };
        self.check_count(batch.operations.len())?;
        self.check_count(batch.authority_admissions.len())?;
        self.check_count(batch.boundary_acceptances.len())?;
        let originals = crate::authority_admission::match_batch(&batch)
            .map_err(|_| Error::Protocol("invalid original authority batch"))?;
        let mut units = Vec::with_capacity(originals.len());
        for received in originals {
            let operation = received.original.verify().map_err(Error::from)?;
            if !self.facets.contains(&operation.facet()) {
                return Err(Error::Protocol("operation is outside admission scope").into());
            }
            units.push(InputUnit::Operation(received));
        }
        Ok(units)
    }
    pub(crate) async fn handle_unit(&mut self, unit: InputUnit) -> StoreResult<Vec<Outbound>, B::Error> {
        match unit {
            InputUnit::Frame(frame)=>self.handle_input(frame).await,
            InputUnit::Operation(received)=>Ok(vec![Outbound::Frame(Frame::Receipt(self.commit_original(received,false).await?))]),
        }
    }
    async fn commit_original(&mut self, received: store::ReceivedOperation, maintenance: bool) -> StoreResult<ReplicationReceipt,B::Error> {
        if self.pending_input_bookkeeping.is_some() {return Err(Error::Protocol("prior input bookkeeping not completed").into());}
        let operation=received.original.verify().map_err(Error::from)?;
        if !self.facets.contains(&operation.facet()) {return Err(Error::Protocol("operation is outside admission scope").into());}
        let id=operation.id().map_err(Error::from)?;
        self.in_flight.remove(&id);
        let mut receipt=ReplicationReceipt::default();
        match self.replica.receive(received).await.map_err(StoreError::Store)? {
            Admission::Accepted=>receipt.accepted_operation_ids.push(id.as_bytes().to_vec()),
            Admission::Pending=>{
                receipt.pending_operation_ids.push(id.as_bytes().to_vec());
                if maintenance {self.replica.remember_peer_heads(self.destination,vec![(operation.facet(),id)]).await.map_err(StoreError::Store)?;}
                else {self.pending_input_bookkeeping=Some((operation.facet(),id));}
            },
            Admission::Rejected(message)=>receipt.rejected.push(rejection(id,message)),
        }
        Ok(receipt)
    }
    pub(crate) fn has_input_bookkeeping(&self) -> bool {
        self.pending_input_bookkeeping.is_some()
    }
    pub(crate) async fn finish_input_bookkeeping(&mut self) -> StoreResult<(), B::Error> {
        if let Some(head) = self.pending_input_bookkeeping.take() {
            self.replica
                .remember_peer_heads(self.destination, vec![head])
                .await
                .map_err(StoreError::Store)?;
        }
        Ok(())
    }
    /// Return the committed result before optional peer repair bookkeeping.
    pub(crate) async fn handle_input(
        &mut self,
        frame: Frame,
    ) -> StoreResult<Vec<Outbound>, B::Error> {
        self.handle_frame(frame, false).await
    }
    async fn handle_frame(
        &mut self,
        frame: Frame,
        maintenance: bool,
    ) -> StoreResult<Vec<Outbound>, B::Error> {
        let mut responses = Vec::new();
        match frame {
            Frame::Have(have) => {
                let count: usize = have.frontiers.iter().map(|f| f.heads.len()).sum();
                self.check_count(count)?;
                self.check_count(have.frontiers.len())?;
                let mut heads = Vec::new();
                let mut receipt = ReplicationReceipt::default();
                for frontier in have.frontiers {
                    let facet = native_facet(frontier.facet)?;
                    if !self.facets.contains(&facet) {
                        return Err(Error::Protocol("unnegotiated facet").into());
                    }
                    for bytes in frontier.heads {
                        let id = hash(&bytes)?;
                        if let Some((record, status)) = self
                            .replica
                            .operation(id)
                            .await
                            .map_err(StoreError::Store)?
                        {
                            if record.original.verify().map_err(Error::from)?.facet() != facet {
                                return Err(Error::Protocol(
                                    "advertised frontier has the wrong facet",
                                )
                                .into());
                            }
                            match status {
                                Admission::Accepted => receipt.accepted_operation_ids.push(bytes),
                                Admission::Rejected(message) => {
                                    receipt.rejected.push(rejection(id, message))
                                }
                                Admission::Pending => heads.push((facet, id)),
                            }
                        } else {
                            heads.push((facet, id));
                        }
                    }
                }
                self.replica
                    .remember_peer_heads(self.destination, heads)
                    .await
                    .map_err(StoreError::Store)?;
                if !receipt.accepted_operation_ids.is_empty() || !receipt.rejected.is_empty() {
                    responses.push(Outbound::Frame(Frame::Receipt(receipt)));
                }
            }
            Frame::Need(need) => {
                self.check_count(need.operation_ids.len())?;
                let sharing = self.export_facets().await?;
                for bytes in need.operation_ids {
                    let id = hash(&bytes)?;
                    let Some((record, Admission::Accepted)) = self
                        .replica
                        .operation(id)
                        .await
                        .map_err(StoreError::Store)?
                    else {
                        return Err(Error::Protocol("requested operation unavailable").into());
                    };
                    let operation = record.original.verify().map_err(Error::from)?;
                    if !sharing.contains(&operation.facet()) {
                        return Err(
                            Error::Protocol("operation is outside current sharing policy").into(),
                        );
                    }
                    responses.push(Outbound::Operation(id));
                }
            }
            Frame::Operations(batch) => {
                if self.pending_input_bookkeeping.is_some() {
                    return Err(Error::Protocol("prior input bookkeeping not completed").into());
                }
                self.check_count(batch.operations.len())?;
                self.check_count(batch.authority_admissions.len())?;
        self.check_count(batch.boundary_acceptances.len())?;
                let decoded =
                    crate::authority_admission::match_batch(&batch).map_err(
                        |error| match error {
                            crate::transport::Error::Protocol(message) => Error::Protocol(message),
                            _ => Error::Protocol("invalid original authority batch"),
                        },
                    )?;
                let mut operations = Vec::with_capacity(decoded.len());
                for received in decoded {
                    let operation = received.original.verify().map_err(Error::from)?;
                    let id = operation.id().map_err(Error::from)?;
                    if !self.facets.contains(&operation.facet()) {
                        return Err(Error::Protocol("operation is outside admission scope").into());
                    }
                    operations.push((received, operation, id));
                }
                let mut receipt = ReplicationReceipt::default();
                for (received, _, _) in operations {
                    let next=self.commit_original(received,maintenance).await?;
                    receipt.accepted_operation_ids.extend(next.accepted_operation_ids);
                    receipt.pending_operation_ids.extend(next.pending_operation_ids);
                    receipt.rejected.extend(next.rejected);
                }
                responses.push(Outbound::Frame(Frame::Receipt(receipt)));
            }
            Frame::Receipt(receipt) => {
                self.check_count(
                    receipt.accepted_operation_ids.len()
                        + receipt.pending_operation_ids.len()
                        + receipt.rejected.len(),
                )?;
                for bytes in receipt.accepted_operation_ids {
                    self.replica
                        .record_peer_receipt(self.destination, hash(&bytes)?, Admission::Accepted)
                        .await
                        .map_err(StoreError::Store)?;
                }
                for bytes in receipt.pending_operation_ids {
                    self.replica
                        .record_peer_receipt(self.destination, hash(&bytes)?, Admission::Pending)
                        .await
                        .map_err(StoreError::Store)?;
                }
                for rejected in receipt.rejected {
                    self.replica
                        .record_peer_receipt(
                            self.destination,
                            hash(&rejected.operation_id)?,
                            Admission::Rejected(
                                rejected.failure.map(|f| f.message).unwrap_or_default(),
                            ),
                        )
                        .await
                        .map_err(StoreError::Store)?;
                }
            }
        }
        if maintenance {
            if let Some(repair) = self.control().await? {
                responses.push(Outbound::Frame(repair));
            }
        }
        Ok(responses)
    }

    /// One bounded dependency window, refilled after each received operation.
    /// Peer frontiers are durable; outstanding requests are session-local.
    pub async fn control(&mut self) -> StoreResult<Option<Frame>, B::Error> {
        let settled = self
            .replica
            .settled_peer_heads(self.destination, self.facets.clone(), self.max_items)
            .await
            .map_err(StoreError::Store)?;
        if !settled.is_empty() {
            let mut receipt = ReplicationReceipt::default();
            for (id, admission) in settled {
                self.in_flight.remove(&id);
                match admission {
                    Admission::Accepted => {
                        receipt.accepted_operation_ids.push(id.as_bytes().to_vec())
                    }
                    Admission::Rejected(message) => receipt.rejected.push(rejection(id, message)),
                    Admission::Pending => {}
                }
            }
            return Ok(Some(Frame::Receipt(receipt)));
        }
        let available = self.max_items.saturating_sub(self.in_flight.len());
        if available == 0 {
            return Ok(None);
        }
        let candidates = self
            .replica
            .needed_from_peer(self.destination, self.facets.clone(), self.max_items)
            .await
            .map_err(StoreError::Store)?;
        let ids: Vec<_> = candidates
            .into_iter()
            .filter(|id| !self.in_flight.contains(id))
            .take(available)
            .collect();
        self.in_flight.extend(ids.iter().copied());
        Ok((!ids.is_empty()).then(|| {
            Frame::Need(ReplicationNeed {
                operation_ids: ids.into_iter().map(|id| id.as_bytes().to_vec()).collect(),
            })
        }))
    }

    /// Resolve payload only when the writer is ready. Pending output queues
    /// contain IDs, and policy is checked again immediately before disclosure.
    pub async fn export_operation(&self, id: ContentHash) -> StoreResult<Frame, B::Error> {
        let Some((record, Admission::Accepted)) = self
            .replica
            .operation(id)
            .await
            .map_err(StoreError::Store)?
        else {
            return Err(Error::Protocol("requested operation unavailable").into());
        };
        let operation = record.original.verify().map_err(Error::from)?;
        if !self.export_facets().await?.contains(&operation.facet()) {
            return Err(Error::Protocol("operation is outside current sharing policy").into());
        }
        Ok(Frame::Operations(ReplicationOperations {
            boundary_acceptances: crate::boundary_acceptance::authority_evidence(record.authority_admission.as_ref()).map_err(|_| Error::Protocol("invalid boundary evidence"))?,
 authority_admissions: record
                .authority_admission
                .as_ref()
                .map(crate::authority_admission::encode)
                .transpose()
                .map_err(|_| Error::Protocol("invalid retained authority admission"))?
                .into_iter()
                .collect(),
            operations: vec![SignedRecord {
                format: OPERATION_FORMAT.into(),
                canonical_record: record.original.canonical,
                signatures: vec![RecordSignature {
                    public_key: operation.publisher.to_vec(),
                    signature: record.original.signature,
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

/// Exact completion fields for one durably admitted original operation.
/// Hosts can bind their acknowledgement exception to these canonical fields.
pub fn completion_receipt(id: ContentHash, admission: Admission) -> ReplicationReceipt {
    let mut receipt = ReplicationReceipt::default();
    match admission {
        Admission::Accepted => receipt.accepted_operation_ids.push(id.as_bytes().to_vec()),
        Admission::Pending => receipt.pending_operation_ids.push(id.as_bytes().to_vec()),
        Admission::Rejected(message) => receipt.rejected.push(rejection(id, message)),
    }
    receipt
}

fn rejection(id: ContentHash, message: String) -> ReplicationRejection {
    ReplicationRejection {
        operation_id: id.as_bytes().to_vec(),
        failure: Some(api::heddle::api::v1alpha1::CallFailure {
            code: 9,
            message,
            ..Default::default()
        }),
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
        Ok(SharedFacet::Metadata) => Ok(ThreadFacet::Metadata),
        _ => Err(Error::Protocol("unsupported replication facet")),
    }
}
pub fn wire_facet(facet: ThreadFacet) -> i32 {
    match facet {
        ThreadFacet::Source => SharedFacet::Source as i32,
        ThreadFacet::Discussion => SharedFacet::Collaboration as i32,
        ThreadFacet::Metadata => SharedFacet::Metadata as i32,
    }
}
fn hash(bytes: &[u8]) -> Result<ContentHash> {
    Ok(ContentHash::from_bytes(bytes.try_into().map_err(|_| {
        Error::Protocol("operation ID must be 32 bytes")
    })?))
}

#[cfg(all(test, feature = "native"))]
#[path = "replication_tests.rs"]
mod tests;

#[cfg(all(test, feature = "native"))]
#[path = "replication_admission_tests.rs"]
mod admission_tests;
