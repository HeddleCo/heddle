// SPDX-License-Identifier: Apache-2.0
//! Native source downloads retain original Thread identities and causal proofs.
//! Pack chunks are staging bytes: install only after the verified Complete frame.
use api::v2::client::{ClientError, MessageReader, Messages, RpcTransport};
use prost::Message;

use crate::{Remote, contract::*, replication, rpc, transport};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Client(#[from] ClientError<transport::Error>),
    #[error(transparent)]
    Transport(#[from] transport::Error),
    #[error(transparent)]
    Replication(#[from] replication::Error),
    #[error("invalid source download: {0}")]
    Invalid(&'static str),
}

#[derive(Clone, Copy)]
pub struct Limits {
    pub max_artifact_bytes: u64,
    pub max_total_bytes: u64,
    pub max_operations: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_artifact_bytes: 512 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
            max_operations: 100_000,
        }
    }
}

/// Each item has passed its frame, scope and cryptographic checks. An operation
/// can still have missing causal parents; the durable replica decides admission.
pub enum Item {
    Pack(PackChunk),
    Operation(SignedRecord),
    Sidecar(TransferSidecar),
    Complete(FetchComplete),
}

pub struct Download<R: MessageReader<Error = transport::Error>> {
    messages: Messages<R, FetchServerFrame>,
    state: Validation,
}
impl<R: MessageReader<Error = transport::Error>> Download<R> {
    pub fn ready(&self) -> &TransferReady {
        &self.state.ready
    }
    pub async fn next(&mut self) -> Result<Option<Item>, Error> {
        if self.state.done {
            return Ok(None);
        }
        let frame = self
            .messages
            .next()
            .await?
            .ok_or(Error::Invalid("stream ended before Complete"))?;
        let item = match self.state.accept(frame) {
            Ok(item) => item,
            Err(error) => {
                self.messages.cancel();
                self.state.done = true;
                return Err(error);
            }
        };
        if self.state.done {
            self.messages.cancel();
        }
        Ok(Some(item))
    }
}

impl<T: RpcTransport<Error = transport::Error>> Remote<T> {
    /// A single exact Thread/revision request returns clone admission, immutable
    /// operations, and bounded source artifacts. No reference update is implied.
    pub async fn fetch_content(
        &self,
        open: FetchOpen,
        limits: Limits,
    ) -> Result<Download<T::Reader>, Error> {
        if open.checkpoint.is_some() {
            return Err(Error::Invalid(
                "a fresh download requires an empty transfer checkpoint",
            ));
        }
        let (sender, mut messages) = self
            .api
            .exchange::<rpc::SyncServiceFetch>(&FetchClientFrame {
                body: Some(fetch_client_frame::Body::Open(open.clone())),
            })
            .await?;
        // This direct-hosted transfer needs no provider negotiation. Closing the
        // request half leaves response flow control and cancellation independent.
        sender.finish().await?;
        let frame = messages
            .next()
            .await?
            .ok_or(Error::Invalid("Ready required"))?;
        let Some(fetch_server_frame::Body::Ready(ready)) = frame.body else {
            return Err(Error::Invalid("first response must be Ready"));
        };
        let state = Validation::new(open, ready, self.description.endpoint.as_ref(), limits)?;
        Ok(Download { messages, state })
    }
}

struct Validation {
    ready: TransferReady,
    facets: Vec<i32>,
    frame_bytes: usize,
    limits: Limits,
    artifact: usize,
    offset: u64,
    digest: blake3::Hasher,
    received: u64,
    metadata_bytes: u64,
    operations: usize,
    done: bool,
}
impl Validation {
    fn new(
        open: FetchOpen,
        ready: TransferReady,
        endpoint: Option<&EndpointRef>,
        limits: Limits,
    ) -> Result<Self, Error> {
        let thread = open
            .thread
            .as_ref()
            .ok_or(Error::Invalid("Thread required"))?;
        if ready.endpoint.as_ref() != endpoint
            || endpoint.is_none()
            || ready.thread.as_ref() != Some(thread)
            || ready
                .current
                .as_ref()
                .is_none_or(|r| r.spool != thread.spool)
            || open
                .revision
                .as_ref()
                .is_some_and(|r| Some(r) != ready.current.as_ref())
        {
            return Err(Error::Invalid(
                "admission does not match requested endpoint and revision",
            ));
        }
        let genesis = ready
            .thread_genesis
            .as_ref()
            .ok_or(Error::Invalid("original Thread genesis required"))?;
        replication::opening::verify_genesis(genesis, thread)?;
        let spool = thread
            .spool
            .as_ref()
            .ok_or(Error::Invalid("spool required"))?;
        let id =
            uuid::Uuid::parse_str(&spool.id).map_err(|_| Error::Invalid("invalid spool UUID"))?;
        if ready
            .owner_genesis
            .as_ref()
            .and_then(|g| g.genesis.as_ref())
            .is_none_or(|g| g.spool_uuid != id.as_bytes())
        {
            return Err(Error::Invalid(
                "original owner genesis must bind this spool",
            ));
        }
        if ready.ownership.is_none() {
            return Err(Error::Invalid("portable owner history required"));
        }
        let budget = ready
            .budget
            .ok_or(Error::Invalid("download budget required"))?;
        if !(1024..=512 * 1024).contains(&budget.max_frame_bytes) || limits.max_operations == 0 {
            return Err(Error::Invalid("invalid download limits"));
        }
        if ready.encoded_len() > budget.max_frame_bytes as usize {
            return Err(Error::Invalid("admission exceeds frame budget"));
        }
        if ready.packs.len() != 2
            || ready.packs[0].kind != pack_extent::Kind::NativePack as i32
            || ready.packs[1].kind != pack_extent::Kind::NativeIndex as i32
        {
            return Err(Error::Invalid("ordered native pack and index required"));
        }
        let mut total = 0_u64;
        for extent in &ready.packs {
            let address = extent
                .pack
                .as_ref()
                .ok_or(Error::Invalid("artifact address required"))?;
            if address.algorithm != "blake3"
                || address.digest.len() != 32
                || extent.offset != 0
                || extent.length == 0
                || extent.length > limits.max_artifact_bytes
                || extent.extent_digest.as_ref() != Some(address)
            {
                return Err(Error::Invalid("invalid whole artifact extent"));
            }
            total = total
                .checked_add(extent.length)
                .ok_or(Error::Invalid("artifact size overflow"))?;
        }
        if total > limits.max_total_bytes {
            return Err(Error::Invalid("download exceeds source budget"));
        }
        let checkpoint = ready
            .checkpoint
            .as_ref()
            .ok_or(Error::Invalid("transfer checkpoint required"))?;
        if checkpoint.transfer_id.is_empty()
            || checkpoint.plan_digest.len() != 32
            || checkpoint.committed_bytes != 0
        {
            return Err(Error::Invalid("invalid fresh transfer checkpoint"));
        }
        let facets = open.selection.map(|s| s.facets).unwrap_or_default();
        if !facets.contains(&(SharedFacet::Source as i32))
            || facets.iter().any(|f| {
                !matches!(
                    SharedFacet::try_from(*f),
                    Ok(SharedFacet::Source | SharedFacet::Collaboration)
                )
            })
        {
            return Err(Error::Invalid(
                "explicit supported source/discussion facets required",
            ));
        }
        Ok(Self {
            ready,
            facets,
            frame_bytes: budget.max_frame_bytes as usize,
            limits,
            artifact: 0,
            offset: 0,
            digest: blake3::Hasher::new(),
            received: 0,
            metadata_bytes: 0,
            operations: 0,
            done: false,
        })
    }
    fn accept(&mut self, frame: FetchServerFrame) -> Result<Item, Error> {
        if self.done || frame.encoded_len() > self.frame_bytes {
            return Err(Error::Invalid("frame exceeds active download budget"));
        }
        let body = frame.body.ok_or(Error::Invalid("empty download frame"))?;
        match body {
            fetch_server_frame::Body::Pack(chunk) => {
                let expected = self
                    .ready
                    .packs
                    .get(self.artifact)
                    .ok_or(Error::Invalid("unexpected artifact"))?;
                let extent = chunk
                    .extent
                    .as_ref()
                    .ok_or(Error::Invalid("chunk extent required"))?;
                let digest = ObjectAddress {
                    algorithm: "blake3".into(),
                    digest: blake3::hash(&chunk.data).as_bytes().to_vec(),
                };
                if chunk.data.is_empty()
                    || extent.pack != expected.pack
                    || extent.kind != expected.kind
                    || extent.offset != self.offset
                    || extent.length != chunk.data.len() as u64
                    || extent.length > expected.length.saturating_sub(self.offset)
                    || extent.extent_digest.as_ref() != Some(&digest)
                {
                    return Err(Error::Invalid(
                        "chunk does not match the declared artifact extent",
                    ));
                }
                if extent.length
                    > self
                        .limits
                        .max_total_bytes
                        .saturating_sub(self.received)
                        .saturating_sub(self.metadata_bytes)
                {
                    return Err(Error::Invalid(
                        "source and metadata exceed shared download budget",
                    ));
                }
                self.digest.update(&chunk.data);
                self.offset += extent.length;
                self.received += extent.length;
                if self.offset == expected.length {
                    if expected
                        .pack
                        .as_ref()
                        .is_none_or(|p| p.digest != self.digest.finalize().as_bytes())
                    {
                        return Err(Error::Invalid("whole artifact hash mismatch"));
                    }
                    self.artifact += 1;
                    self.offset = 0;
                    self.digest = blake3::Hasher::new();
                }
                Ok(Item::Pack(chunk))
            }
            fetch_server_frame::Body::Operation(record) => {
                self.operations += 1;
                self.metadata_bytes = self
                    .metadata_bytes
                    .checked_add(record.encoded_len() as u64)
                    .ok_or(Error::Invalid("metadata size overflow"))?;
                if self.operations > self.limits.max_operations
                    || self.metadata_bytes
                        > self.limits.max_total_bytes.saturating_sub(self.received)
                {
                    return Err(Error::Invalid("causal metadata exceeds download budget"));
                }
                let signed = replication::decode_record(record.clone())?;
                let operation = signed
                    .verify()
                    .map_err(|_| Error::Invalid("invalid original operation signature"))?;
                if self
                    .ready
                    .thread
                    .as_ref()
                    .and_then(|t| t.id.as_ref())
                    .is_none_or(|id| id.value != operation.thread.as_bytes())
                    || !self
                        .facets
                        .contains(&replication::wire_facet(operation.facet()))
                {
                    return Err(Error::Invalid("operation crosses Thread or selected facet"));
                }
                Ok(Item::Operation(record))
            }
            fetch_server_frame::Body::Complete(complete) => {
                let original = self
                    .ready
                    .checkpoint
                    .as_ref()
                    .ok_or(Error::Invalid("checkpoint absent"))?;
                let checkpoint = complete
                    .checkpoint
                    .as_ref()
                    .ok_or(Error::Invalid("final checkpoint required"))?;
                if complete.revision != self.ready.current
                    || self.artifact != self.ready.packs.len()
                    || checkpoint.transfer_id != original.transfer_id
                    || checkpoint.plan_digest != original.plan_digest
                    || checkpoint.committed_bytes != self.received
                    || complete.closure != Coverage::Complete as i32
                    || !complete.missing.is_empty()
                {
                    return Err(Error::Invalid(
                        "download is not a complete exact source closure",
                    ));
                }
                self.done = true;
                Ok(Item::Complete(complete))
            }
            fetch_server_frame::Body::Sidecar(_) => Err(Error::Invalid(
                "sidecar facet was not explicitly negotiated",
            )),
            fetch_server_frame::Body::ProviderPlan(_) => Err(Error::Invalid(
                "provider transfer requires explicit client consent",
            )),
            fetch_server_frame::Body::Ready(_) => {
                Err(Error::Invalid("duplicate download admission"))
            }
        }
    }
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
