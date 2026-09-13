//! Capability-free candidate admission followed by exact signed consent.
//! The caller's verified credential implementation owns its terminal PoP key;
//! this module never accepts an arbitrary identity string or private key per
//! Fetch invocation.

use std::{io::Read, path::Path};

use api::{
    heddle::api::v2alpha1::{
        EndpointRef, FetchClientFrame, FetchOpen, FetchServerFrame, ProviderConsent, ProviderOffer,
        ProviderPlan, ProviderPlanChallenge, ReadProviderExtentRequest, RecordSignature,
        SignedRecord, fetch_client_frame, fetch_open, fetch_server_frame, provider_assembly_record,
        provider_extent_event,
    },
    provider_v2::{
        PROVIDER_CONSENT_FORMAT, provider_consent_signing_bytes, validate_plan_for_offer,
        validate_provider_offer,
    },
    v2::client::{MessageReader, MessageWriter, Messages, RpcTransport, Sender},
};
use heddle_object_model::object::{ContentHash, StateId};
use heddle_pack::store::pack::PackObjectId;
use wire::{ProviderPackExtent, ProviderPackIndexEntry, ProviderPackManifest, ProviderPackSpool};

use super::{Download, Error, Item, Limits, StagedSource, Validation};
use crate::{Remote, contract::TransferReady, rpc, transport};

/// Implemented by the same credential that signed the Fetch opening. The
/// server verifies this identity against its authenticated Biscuit subject
/// and requires the signature key to equal the credential's terminal cnf key.
pub trait ProviderConsentSigner {
    fn verified_subject(&self) -> Result<String, Error>;
    fn client_endpoint(&self) -> Result<EndpointRef, Error>;
    fn public_key(&self) -> &[u8];
    fn sign(&self, canonical: &[u8]) -> Result<Vec<u8>, Error>;
}

/// An authenticated Fetch exchange whose request half remains open for exact
/// consent and the final verified result. Dropping it aborts both halves.
pub struct ProviderDownload<
    W: MessageWriter<Error = transport::Error>,
    R: MessageReader<Error = transport::Error>,
> {
    sender: Sender<W, FetchClientFrame>,
    messages: Messages<R, FetchServerFrame>,
    state: Validation,
    open: FetchOpen,
    issuer: EndpointRef,
}

/// The issuer may explicitly select ordinary direct source delivery when a
/// preferred provider transfer cannot be offered. Only the admitted Ready
/// decides the branch; an arbitrary stream error never triggers a retry.
pub enum ProviderFetch<
    W: MessageWriter<Error = transport::Error>,
    R: MessageReader<Error = transport::Error>,
> {
    Direct(Download<R>),
    Provider(ProviderDownload<W, R>),
}

/// Only an issued plan matching the signed candidate can reach this stage.
pub struct ProviderPlanSession<
    W: MessageWriter<Error = transport::Error>,
    R: MessageReader<Error = transport::Error>,
> {
    plan: ProviderPlan,
    ready: TransferReady,
    originals: Vec<Item>,
    sender: Sender<W, FetchClientFrame>,
    messages: Messages<R, FetchServerFrame>,
    state: Validation,
    spool: Option<ProviderPackSpool>,
}

impl<W: MessageWriter<Error = transport::Error>, R: MessageReader<Error = transport::Error>>
    ProviderPlanSession<W, R>
{
    /// The exact ticketed layout admitted after client consent.
    pub fn plan(&self) -> &ProviderPlan {
        &self.plan
    }

    /// The exact issuer admission that preceded the unsigned offer.
    pub fn ready(&self) -> &TransferReady {
        &self.ready
    }

    /// Reserve a bounded virtual pack and receive only the issuer's inline
    /// records. Every chunk must name its exact record and next offset; the
    /// wire framing cannot allocate or write outside the canonical layout.
    /// The session retains the spool for separately authorized provider ranges.
    pub async fn receive_inline(&mut self, scratch: &Path) -> Result<(), Error> {
        if self.spool.is_some() {
            return Err(Error::Invalid("provider inline delivery already started"));
        }
        if self.plan.output_pack_length > 256 * 1024 * 1024
            || self.plan.output_pack_length > self.state.limits.max_artifact_bytes
            || self.plan.output_pack_length
                > self
                    .state
                    .limits
                    .max_total_bytes
                    .saturating_sub(self.state.metadata_bytes)
        {
            return Err(Error::Invalid(
                "provider source pack exceeds staging budget",
            ));
        }
        let manifest = pack_manifest(&self.plan)?;
        let scratch = scratch.to_path_buf();
        let spool =
            tokio::task::spawn_blocking(move || ProviderPackSpool::new_in(&scratch, manifest))
                .await
                .map_err(|error| Error::Preparation(error.to_string()))?
                .map_err(|error| Error::Preparation(error.to_string()))?;
        let writer = spool.writer();
        let mut offsets = vec![0_u64; self.plan.records.len()];
        let inline_count = self
            .plan
            .records
            .iter()
            .filter(|record| {
                matches!(
                    record.source,
                    Some(provider_assembly_record::Source::Inline(_))
                )
            })
            .count();
        let mut complete = 0_usize;
        while complete < inline_count {
            let frame = self.messages.next().await?.ok_or(Error::Invalid(
                "provider inline records ended before complete coverage",
            ))?;
            let Some(fetch_server_frame::Body::ProviderInline(chunk)) = frame.body else {
                return Err(Error::Invalid(
                    "unexpected frame during provider inline delivery",
                ));
            };
            let index = usize::try_from(chunk.record_index)
                .map_err(|_| Error::Invalid("provider inline record index"))?;
            let record = self
                .plan
                .records
                .get(index)
                .ok_or(Error::Invalid("provider inline record absent"))?;
            if !matches!(
                record.source,
                Some(provider_assembly_record::Source::Inline(_))
            ) || chunk.assembly_digest != self.plan.assembly_digest
                || chunk.data.is_empty()
                || chunk.data.len() > 1024 * 1024
                || chunk.offset != offsets[index]
            {
                return Err(Error::Invalid("provider inline chunk differs from plan"));
            }
            let next = chunk
                .offset
                .checked_add(chunk.data.len() as u64)
                .ok_or(Error::Invalid("provider inline offset overflow"))?;
            if next > record.encoded_length {
                return Err(Error::Invalid("provider inline chunk exceeds record"));
            }
            let finished = next == record.encoded_length;
            write_chunk(
                writer.clone(),
                index,
                chunk.offset,
                chunk.data,
                record.clone(),
                finished,
            )
            .await?;
            offsets[index] = next;
            if finished {
                complete += 1;
            }
        }
        self.spool = Some(spool);
        Ok(())
    }

    /// Read each ticketed physical range from its exact provider endpoint.
    /// The provider cannot change virtual placement: every byte is written
    /// only into its precommitted record, then independently rehashed.
    pub async fn receive_provider_ranges<T: RpcTransport<Error = transport::Error>>(
        &mut self,
        providers: &[Remote<T>],
    ) -> Result<(), Error> {
        let spool = self
            .spool
            .as_ref()
            .ok_or(Error::Invalid("provider inline stage required"))?;
        let writer = spool.writer();
        let mut grouped = vec![Vec::new(); self.plan.extents.len()];
        for (index, record) in self.plan.records.iter().enumerate() {
            if let Some(provider_assembly_record::Source::Provider(source)) = &record.source {
                grouped
                    .get_mut(source.extent_index as usize)
                    .ok_or(Error::Invalid("provider record extent absent"))?
                    .push((source.source_offset, index, record));
            }
        }
        for records in &mut grouped {
            records.sort_by_key(|(offset, _, _)| *offset);
        }
        for (extent_index, extent) in self.plan.extents.iter().enumerate() {
            let endpoint = extent
                .provider
                .as_ref()
                .ok_or(Error::Invalid("provider endpoint absent"))?;
            let remote = providers
                .iter()
                .find(|remote| remote.description.endpoint.as_ref() == Some(endpoint))
                .ok_or(Error::Invalid("selected provider endpoint unavailable"))?;
            let range = extent
                .range
                .as_ref()
                .ok_or(Error::Invalid("provider range absent"))?;
            let ticket = extent
                .ticket
                .as_ref()
                .ok_or(Error::Invalid("provider ticket absent"))?;
            let records = grouped
                .get(extent_index)
                .ok_or(Error::Invalid("provider extent group absent"))?;
            if records.is_empty() || records[0].0 != 0 {
                return Err(Error::Invalid("provider range has no tiled records"));
            }
            let mut messages = remote
                .api
                .observe::<rpc::SyncServiceReadProviderExtent>(&ReadProviderExtentRequest {
                    ticket: Some(ticket.clone()),
                    extent_set_digest: self.plan.extent_set_digest.clone(),
                    range: Some(range.clone()),
                })
                .await?;
            let first = messages
                .next()
                .await?
                .ok_or(Error::Invalid("provider extent Ready absent"))?;
            if !matches!(first.body, Some(provider_extent_event::Body::Ready(ref ready)) if ready == range)
            {
                return Err(Error::Invalid("provider extent Ready differs from ticket"));
            }
            let mut offset = 0_u64;
            let mut record_pos = 0_usize;
            loop {
                let event = messages
                    .next()
                    .await?
                    .ok_or(Error::Invalid("provider extent ended without Complete"))?;
                match event.body {
                    Some(provider_extent_event::Body::Chunk(chunk)) => {
                        if chunk.offset != offset
                            || chunk.data.is_empty()
                            || chunk.data.len() > 1024 * 1024
                        {
                            return Err(Error::Invalid("provider extent chunk offset or size"));
                        }
                        let end = offset
                            .checked_add(chunk.data.len() as u64)
                            .ok_or(Error::Invalid("provider extent offset overflow"))?;
                        if end > range.length {
                            return Err(Error::Invalid("provider extent exceeds ticket"));
                        }
                        let mut used = 0_usize;
                        while used < chunk.data.len() {
                            let (start, index, record) = records
                                .get(record_pos)
                                .ok_or(Error::Invalid("provider extra bytes after records"))?;
                            let relative = offset
                                .checked_sub(*start)
                                .ok_or(Error::Invalid("provider record gap"))?;
                            let remaining = record
                                .encoded_length
                                .checked_sub(relative)
                                .ok_or(Error::Invalid("provider record overflow"))?;
                            let take =
                                usize::try_from(remaining.min((chunk.data.len() - used) as u64))
                                    .map_err(|_| Error::Invalid("provider record size"))?;
                            let finished = relative + take as u64 == record.encoded_length;
                            write_chunk(
                                writer.clone(),
                                *index,
                                relative,
                                chunk.data[used..used + take].to_vec(),
                                (*record).clone(),
                                finished,
                            )
                            .await?;
                            offset += take as u64;
                            used += take;
                            if finished {
                                record_pos += 1;
                            }
                        }
                    }
                    Some(provider_extent_event::Body::Complete(checkpoint)) => {
                        if offset != range.length
                            || record_pos != records.len()
                            || checkpoint.committed_bytes != range.length
                        {
                            return Err(Error::Invalid("provider extent incomplete"));
                        }
                        break;
                    }
                    _ => return Err(Error::Invalid("unexpected provider extent frame")),
                }
            }
        }
        Ok(())
    }

    /// Finalize exact pack integrity and signed source closure before sending
    /// the provider result. A terminal Complete must acknowledge the same
    /// transfer, plan and verified output length before staging is returned.
    pub async fn complete(mut self, scratch: &Path) -> Result<StagedSource, Error> {
        let spool = self
            .spool
            .take()
            .ok_or(Error::Invalid("provider pack stage required"))?;
        let completed = tokio::task::spawn_blocking(move || spool.finish())
            .await
            .map_err(|error| Error::Preparation(error.to_string()))?
            .map_err(|error| Error::Preparation(error.to_string()))?;
        let (pack, index) = completed.artifact_paths();
        let directory = tempfile::Builder::new()
            .prefix("provider-download-")
            .tempdir_in(scratch)?;
        std::fs::hard_link(pack, directory.path().join("source.pack"))?;
        std::fs::hard_link(index, directory.path().join("source.idx"))?;
        let mut operations = Vec::new();
        let mut receipt_records = Vec::new();
        let mut dependencies = Vec::new();
        for item in self.originals {
            match item {
                Item::Operations(batch) => {
                    for received in crate::authority_admission::match_batch(&batch)? {
                        operations.push(received.original);
                        receipt_records.extend(received.authority_admission);
                    }
                }
                Item::ThreadGenesis(record) => dependencies.push(record),
                _ => {
                    return Err(Error::Invalid(
                        "provider source originals differ from Ready",
                    ));
                }
            }
        }
        let ready = self.ready.clone();
        let staged = tokio::task::spawn_blocking(move || {
            super::staging::validate_with_receipts(
                directory,
                ready,
                operations,
                dependencies,
                receipt_records,
            )
        })
        .await
        .map_err(|error| Error::Preparation(error.to_string()))??;
        let pack_path = pack.to_path_buf();
        let (bytes, digest) = tokio::task::spawn_blocking(move || -> Result<_, Error> {
            let mut file = std::fs::File::open(pack_path)?;
            let mut hasher = blake3::Hasher::new();
            let mut bytes = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                bytes = bytes
                    .checked_add(read as u64)
                    .ok_or(Error::Invalid("provider output length overflow"))?;
                hasher.update(&buffer[..read]);
            }
            Ok((bytes, hasher.finalize()))
        })
        .await
        .map_err(|error| Error::Preparation(error.to_string()))??;
        if bytes != self.plan.output_pack_length {
            return Err(Error::Invalid("provider output length differs from plan"));
        }
        let result = api::heddle::api::v2alpha1::ProviderResult {
            extent_set_digest: self.plan.extent_set_digest.clone(),
            assembly_digest: self.plan.assembly_digest.clone(),
            verified_range_commitments: self
                .plan
                .extents
                .iter()
                .map(|extent| {
                    extent
                        .range
                        .as_ref()
                        .map(|range| range.record_set_commitment.clone())
                        .ok_or(Error::Invalid("provider range absent"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            assembled_pack: Some(api::heddle::api::v2alpha1::ObjectAddress {
                algorithm: "blake3".into(),
                digest: digest.as_bytes().to_vec(),
            }),
        };
        self.sender
            .send(&FetchClientFrame {
                body: Some(fetch_client_frame::Body::ProviderResult(result)),
            })
            .await?;
        self.sender.finish().await?;
        let frame = self
            .messages
            .next()
            .await?
            .ok_or(Error::Invalid("provider terminal Complete absent"))?;
        let Some(fetch_server_frame::Body::Complete(complete)) = frame.body else {
            return Err(Error::Invalid("provider terminal Complete required"));
        };
        let initial = self
            .state
            .ready
            .checkpoint
            .as_ref()
            .ok_or(Error::Invalid("provider initial checkpoint absent"))?;
        let final_checkpoint = complete
            .checkpoint
            .as_ref()
            .ok_or(Error::Invalid("provider final checkpoint absent"))?;
        if complete.revision != self.state.ready.current
            || complete.closure != api::heddle::api::v2alpha1::Coverage::Complete as i32
            || !complete.missing.is_empty()
            || final_checkpoint.transfer_id != initial.transfer_id
            || final_checkpoint.plan_digest != initial.plan_digest
            || final_checkpoint.committed_bytes != bytes
        {
            return Err(Error::Invalid(
                "provider completion differs from verified source",
            ));
        }
        self.messages.cancel();
        Ok(staged)
    }
}

async fn write_chunk(
    writer: wire::ProviderPackWriter,
    index: usize,
    offset: u64,
    data: Vec<u8>,
    record: api::heddle::api::v2alpha1::ProviderAssemblyRecord,
    finished: bool,
) -> Result<(), Error> {
    tokio::task::spawn_blocking(move || {
        writer
            .write_extent_chunk(index, offset, &data)
            .map_err(|error| Error::Preparation(error.to_string()))?;
        if finished {
            verify_record(&writer, index, &record)?;
        }
        Ok(())
    })
    .await
    .map_err(|error| Error::Preparation(error.to_string()))?
}

fn verify_record(
    writer: &wire::ProviderPackWriter,
    index: usize,
    record: &api::heddle::api::v2alpha1::ProviderAssemblyRecord,
) -> Result<(), Error> {
    let mut hasher = blake3::Hasher::new();
    writer
        .hash_extent_prefix(index, record.encoded_length, &mut hasher)
        .map_err(|error| Error::Preparation(error.to_string()))?;
    let digest = record
        .encoded_digest
        .as_ref()
        .ok_or(Error::Invalid("provider record digest absent"))?;
    if hasher.finalize().as_bytes() != digest.digest.as_slice() {
        return Err(Error::Invalid("provider encoded record digest differs"));
    }
    writer
        .mark_verified(index)
        .map_err(|error| Error::Preparation(error.to_string()))
}

impl<T: RpcTransport<Error = transport::Error>> Remote<T> {
    pub async fn begin_provider_fetch(
        &self,
        open: FetchOpen,
        limits: Limits,
    ) -> Result<ProviderFetch<T::Writer, T::Reader>, Error> {
        if open.delivery != fetch_open::Delivery::ProviderPreferred as i32
            || open.checkpoint.is_some()
        {
            return Err(Error::Invalid("fresh preferred provider Fetch required"));
        }
        let issuer = self
            .description
            .endpoint
            .clone()
            .ok_or(Error::Invalid("issuer endpoint required"))?;
        let (sender, mut messages) = self
            .api
            .exchange::<rpc::SyncServiceFetch>(&FetchClientFrame {
                body: Some(fetch_client_frame::Body::Open(open.clone())),
            })
            .await?;
        let frame = messages
            .next()
            .await?
            .ok_or(Error::Invalid("provider Ready required"))?;
        let Some(fetch_server_frame::Body::Ready(ready)) = frame.body else {
            return Err(Error::Invalid("first provider response must be Ready"));
        };
        if !ready.packs.is_empty() {
            let mut direct = open.clone();
            direct.delivery = fetch_open::Delivery::Direct as i32;
            let state = Validation::new(direct, ready, Some(&issuer), limits)?;
            sender.finish().await?;
            return Ok(ProviderFetch::Direct(Download { messages, state }));
        }
        let state = Validation::new(open.clone(), ready, Some(&issuer), limits)?;
        Ok(ProviderFetch::Provider(ProviderDownload {
            sender,
            messages,
            state,
            open,
            issuer,
        }))
    }
}

impl<W: MessageWriter<Error = transport::Error>, R: MessageReader<Error = transport::Error>>
    ProviderDownload<W, R>
{
    pub async fn negotiate(
        mut self,
        signer: &impl ProviderConsentSigner,
    ) -> Result<ProviderPlanSession<W, R>, Error> {
        let mut originals = Vec::new();
        let offer = loop {
            let frame = self
                .messages
                .next()
                .await?
                .ok_or(Error::Invalid("provider Offer required"))?;
            match frame.body {
                Some(fetch_server_frame::Body::Operations(_))
                | Some(fetch_server_frame::Body::ThreadGenesis(_)) => {
                    originals.push(self.state.accept(frame)?);
                }
                Some(fetch_server_frame::Body::ProviderOffer(offer)) => break offer,
                _ => return Err(Error::Invalid("unexpected frame before provider Offer")),
            }
        };
        if self
            .state
            .ready
            .checkpoint
            .as_ref()
            .is_none_or(|checkpoint| checkpoint.plan_digest != offer.assembly_digest)
        {
            return Err(Error::Invalid(
                "provider Offer differs from Ready checkpoint",
            ));
        }
        let candidate =
            Candidate::new(&self.open, &self.issuer, &signer.client_endpoint()?, offer)?;
        if candidate.challenge()?.revision.as_ref() != self.state.ready.current.as_ref() {
            return Err(Error::Invalid(
                "provider offer differs from admitted revision",
            ));
        }
        self.sender
            .send(&FetchClientFrame {
                body: Some(fetch_client_frame::Body::Consent(
                    candidate.consent(signer)?,
                )),
            })
            .await?;
        let issued = self
            .messages
            .next()
            .await?
            .ok_or(Error::Invalid("issued provider Plan required"))?;
        let Some(fetch_server_frame::Body::ProviderPlan(plan)) = issued.body else {
            return Err(Error::Invalid(
                "first frame after consent must be provider Plan",
            ));
        };
        candidate.admit(&plan)?;
        Ok(ProviderPlanSession {
            ready: self.state.ready.clone(),
            plan,
            originals,
            sender: self.sender,
            messages: self.messages,
            state: self.state,
            spool: None,
        })
    }
}

/// Convert only a canonical, ticketed plan to the positional spool layout.
/// Each encoded source record is verified separately before the spool may
/// finalize, including records that arrive inline rather than from a provider.
fn pack_manifest(plan: &ProviderPlan) -> Result<ProviderPackManifest, Error> {
    api::provider_v2::validate_provider_plan(plan)
        .map_err(|_| Error::Invalid("invalid issued provider plan"))?;
    let header: [u8; 16] = plan
        .pack_header
        .as_slice()
        .try_into()
        .map_err(|_| Error::Invalid("invalid provider pack header"))?;
    let extents = plan
        .records
        .iter()
        .map(|record| {
            let object = record
                .object
                .as_ref()
                .ok_or(Error::Invalid("provider object absent"))?;
            let address = object
                .address
                .as_ref()
                .ok_or(Error::Invalid("provider object address absent"))?;
            let digest = record
                .encoded_digest
                .as_ref()
                .ok_or(Error::Invalid("provider encoded digest absent"))?;
            let object_hash: [u8; 32] = address
                .digest
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invalid("provider object digest length"))?;
            let encoded_hash: [u8; 32] = digest
                .digest
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invalid("provider encoded digest length"))?;
            let id = match object.kind.as_str() {
                "blob" | "tree" => PackObjectId::Hash(ContentHash::from_bytes(object_hash)),
                "state" => PackObjectId::StateId(StateId::from_bytes(object_hash)),
                _ => return Err(Error::Invalid("provider object kind")),
            };
            Ok(ProviderPackExtent {
                output_offset: record.output_offset,
                length: record.encoded_length,
                digest: encoded_hash,
                objects: vec![ProviderPackIndexEntry {
                    id,
                    output_offset: record.output_offset,
                }],
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(ProviderPackManifest {
        header,
        output_pack_length: plan.output_pack_length,
        extents,
    })
}

/// An unsigned offer bound to the authenticated issuer, client, and exact
/// selected source. It grants no provider read until final ticket admission.
pub struct Candidate {
    offer: ProviderOffer,
}

impl Candidate {
    pub fn new(
        open: &FetchOpen,
        issuer: &EndpointRef,
        client: &EndpointRef,
        offer: ProviderOffer,
    ) -> Result<Self, Error> {
        if open.delivery != fetch_open::Delivery::ProviderPreferred as i32 {
            return Err(Error::Invalid("provider offer requires preferred delivery"));
        }
        validate_provider_offer(&offer).map_err(|_| Error::Invalid("invalid provider offer"))?;
        let challenge = offer
            .challenge
            .as_ref()
            .ok_or(Error::Invalid("provider challenge required"))?;
        if challenge.thread != open.thread
            || open
                .revision
                .as_ref()
                .is_some_and(|revision| challenge.revision.as_ref() != Some(revision))
            || challenge.issuer.as_ref() != Some(issuer)
            || challenge.client.as_ref() != Some(client)
        {
            return Err(Error::Invalid(
                "provider offer differs from selected source or peer",
            ));
        }
        let expiry = challenge
            .expires_at
            .as_ref()
            .ok_or(Error::Invalid("provider expiry required"))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Invalid("provider clock unavailable"))?;
        if expiry.seconds
            <= i64::try_from(now.as_secs())
                .map_err(|_| Error::Invalid("provider clock overflow"))?
        {
            return Err(Error::Invalid("provider offer expired"));
        }
        Ok(Self { offer })
    }

    pub fn challenge(&self) -> Result<&ProviderPlanChallenge, Error> {
        self.offer
            .challenge
            .as_ref()
            .ok_or(Error::Invalid("validated provider challenge absent"))
    }

    pub fn consent(&self, signer: &impl ProviderConsentSigner) -> Result<ProviderConsent, Error> {
        let identity = format!("principal:{}", signer.verified_subject()?);
        let canonical = provider_consent_signing_bytes(self.challenge()?, &identity)
            .map_err(|_| Error::Invalid("invalid provider consent challenge"))?;
        let key = signer.public_key();
        if key.len() != 32 {
            return Err(Error::Invalid("provider consent key must be Ed25519"));
        }
        let signature = signer.sign(&canonical)?;
        if signature.len() != 64 {
            return Err(Error::Invalid("provider consent signature length"));
        }
        Ok(ProviderConsent {
            extent_set_digest: self.offer.extent_set_digest.clone(),
            exact_plan_consent: Some(SignedRecord {
                format: PROVIDER_CONSENT_FORMAT.into(),
                canonical_record: canonical,
                signatures: vec![RecordSignature {
                    public_key: key.to_vec(),
                    signature,
                }],
            }),
            assembly_digest: self.offer.assembly_digest.clone(),
        })
    }

    pub fn admit(&self, plan: &ProviderPlan) -> Result<(), Error> {
        validate_plan_for_offer(&self.offer, plan)
            .map_err(|_| Error::Invalid("issued provider plan differs from consented offer"))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use api::{
        heddle::api::v2alpha1::{
            Coverage, DescribeEndpointResponse, FetchComplete, ObjectAddress, PackChunk,
            ProviderAssemblyRecord,
        },
        v2::{
            MethodDescriptor,
            client::{Client, Rpc, RpcTransport},
        },
    };
    use prost::Message;

    use super::*;

    struct Reader(VecDeque<Vec<u8>>);
    impl MessageReader for Reader {
        type Error = transport::Error;
        async fn next(&mut self) -> Result<Option<Vec<u8>>, Self::Error> {
            Ok(self.0.pop_front())
        }
        fn cancel(&mut self) {
            self.0.clear();
        }
    }
    struct Writer(Arc<AtomicBool>);
    impl MessageWriter for Writer {
        type Error = transport::Error;
        async fn send(&mut self, _: Vec<u8>) -> Result<(), Self::Error> {
            Err(transport::Error::Protocol("direct fallback sent consent"))
        }
        async fn finish(&mut self) -> Result<(), Self::Error> {
            self.0.store(true, Ordering::Release);
            Ok(())
        }
        fn abort(&mut self) {}
    }
    struct Peer {
        frames: Vec<Vec<u8>>,
        finished: Arc<AtomicBool>,
    }
    impl RpcTransport for Peer {
        type Error = transport::Error;
        type Reader = Reader;
        type Writer = Writer;
        async fn unary(
            &self,
            _: &'static MethodDescriptor,
            _: Vec<u8>,
        ) -> Result<Vec<u8>, Self::Error> {
            Err(transport::Error::Protocol("unused"))
        }
        async fn observe(
            &self,
            _: &'static MethodDescriptor,
            _: Vec<u8>,
        ) -> Result<Reader, Self::Error> {
            Err(transport::Error::Protocol("unused"))
        }
        async fn exchange(
            &self,
            _: &'static MethodDescriptor,
            opening: Vec<u8>,
        ) -> Result<(Writer, Reader), Self::Error> {
            let frame = FetchClientFrame::decode(opening.as_slice())
                .map_err(|_| transport::Error::Protocol("bad opening"))?;
            assert!(
                matches!(frame.body, Some(fetch_client_frame::Body::Open(FetchOpen {delivery, ..}))
                if delivery == fetch_open::Delivery::ProviderPreferred as i32)
            );
            Ok((
                Writer(Arc::clone(&self.finished)),
                Reader(self.frames.clone().into()),
            ))
        }
    }

    #[tokio::test]
    async fn preferred_ready_with_packs_is_direct_and_terminal_is_checked() {
        for wrong_terminal in [false, true] {
            let (mut open, ready, endpoint, artifacts) = super::super::tests::fixture();
            open.delivery = fetch_open::Delivery::ProviderPreferred as i32;
            let mut frames = vec![
                FetchServerFrame {
                    body: Some(fetch_server_frame::Body::Ready(ready.clone())),
                }
                .encode_to_vec(),
            ];
            for (index, bytes) in artifacts.iter().enumerate() {
                frames.push(
                    FetchServerFrame {
                        body: Some(fetch_server_frame::Body::Pack(PackChunk {
                            extent: Some(ready.packs[index].clone()),
                            data: bytes.clone(),
                        })),
                    }
                    .encode_to_vec(),
                );
            }
            let mut checkpoint = ready.checkpoint.clone().expect("fixture checkpoint");
            checkpoint.committed_bytes = if wrong_terminal {
                0
            } else {
                artifacts.iter().map(|bytes| bytes.len() as u64).sum()
            };
            frames.push(
                FetchServerFrame {
                    body: Some(fetch_server_frame::Body::Complete(FetchComplete {
                        revision: ready.current.clone(),
                        checkpoint: Some(checkpoint),
                        closure: Coverage::Complete as i32,
                        missing: vec![],
                    })),
                }
                .encode_to_vec(),
            );
            let finished = Arc::new(AtomicBool::new(false));
            let remote = Remote {
                api: Client::new(
                    Peer {
                        frames,
                        finished: Arc::clone(&finished),
                    },
                    [rpc::SyncServiceFetch::METHOD.path.into()],
                ),
                description: DescribeEndpointResponse {
                    endpoint: Some(endpoint),
                    ..Default::default()
                },
            };
            let ProviderFetch::Direct(mut download) = remote
                .begin_provider_fetch(open, Limits::default())
                .await
                .expect("direct fallback")
            else {
                panic!("expected direct branch")
            };
            assert!(
                finished.load(Ordering::Acquire),
                "direct path closes request half"
            );
            for _ in 0..2 {
                assert!(matches!(
                    download.next().await.expect("pack"),
                    Some(Item::Pack(_))
                ));
            }
            if wrong_terminal {
                assert!(matches!(
                    download.next().await,
                    Err(Error::Invalid(
                        "download does not match its exact declared source coverage"
                    ))
                ));
            } else {
                assert!(matches!(
                    download.next().await.expect("Complete"),
                    Some(Item::Complete(_))
                ));
            }
        }
    }

    #[test]
    fn provider_ready_has_no_direct_pack_or_partial_fallback() {
        let (mut open, mut ready, endpoint, _) = super::super::tests::fixture();
        open.delivery = fetch_open::Delivery::ProviderPreferred as i32;
        assert!(
            Validation::new(
                open.clone(),
                ready.clone(),
                Some(&endpoint),
                Limits::default()
            )
            .is_err(),
            "provider mode cannot silently receive direct source packs"
        );
        ready.packs.clear();
        Validation::new(
            open.clone(),
            ready.clone(),
            Some(&endpoint),
            Limits::default(),
        )
        .expect("complete provider Ready with no direct artifact");
        ready.full_closure_available = false;
        assert!(
            Validation::new(open, ready, Some(&endpoint), Limits::default()).is_err(),
            "provider offer cannot downgrade whole-source disclosure"
        );
    }

    #[test]
    fn encoded_record_digest_is_checked_before_completion() {
        let scratch = tempfile::tempdir().expect("test scratch");
        let body = b"one encoded record";
        let mut header = [0_u8; 16];
        header[..4].copy_from_slice(b"LMPK");
        header[4..8].copy_from_slice(&4_u32.to_be_bytes());
        header[8..].copy_from_slice(&1_u64.to_be_bytes());
        let spool = ProviderPackSpool::new_in(
            scratch.path(),
            ProviderPackManifest {
                header,
                output_pack_length: 16 + body.len() as u64 + 32,
                extents: vec![ProviderPackExtent {
                    output_offset: 16,
                    length: body.len() as u64,
                    digest: *blake3::hash(body).as_bytes(),
                    objects: vec![ProviderPackIndexEntry {
                        id: PackObjectId::Hash(ContentHash::from_bytes([7; 32])),
                        output_offset: 16,
                    }],
                }],
            },
        )
        .expect("bounded spool fixture");
        let writer = spool.writer();
        writer
            .write_extent_chunk(0, 0, body)
            .expect("fixture bytes");
        let mut record = ProviderAssemblyRecord {
            encoded_length: body.len() as u64,
            encoded_digest: Some(ObjectAddress {
                algorithm: "blake3".into(),
                digest: vec![0; 32],
            }),
            ..Default::default()
        };
        assert!(
            verify_record(&writer, 0, &record).is_err(),
            "a fully received but wrong record cannot become verified"
        );
        record
            .encoded_digest
            .as_mut()
            .expect("fixture digest")
            .digest = blake3::hash(body).as_bytes().to_vec();
        verify_record(&writer, 0, &record).expect("exact record becomes verified");
    }
}
