use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use api::{
    framing::{StreamFrame, decode_stream_frame, encode_stream_message},
    heddle::api::v1alpha1::{
        ProviderPlanChallenge, ProviderPlanResponse, ProviderPullCapabilityContext,
        ProviderPullManifest as ApiProviderPullManifest, ProviderPullResponse,
        ProviderPullResultStatus, ProviderReadCheckpoint, ProviderReadClientFrame,
        ProviderReadComplete, ProviderReadReady, ProviderReadRequest, ProviderReadServerFrame,
        ProviderSource, provider_read_client_frame, provider_read_server_frame,
    },
};
use bytes::Bytes;
use futures::{StreamExt as _, stream::FuturesUnordered};
use objects::store::PackObjectId;
use prost::Message;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use wire::{
    CompletedProviderPack, ProtocolError, ProviderPackExtent, ProviderPackIndexEntry,
    ProviderPackManifest, ProviderPackSpool, ProviderPackWriter,
};

use super::{
    HostedClient,
    helpers::{HostedTransportPolicy, hosted_to_protocol_error},
};

const PROVIDER_VERSION: u32 = 1;
const PLAN_NONCE_LEN: usize = 16;
const DIGEST_LEN: usize = 32;
const PROVIDER_READ_CHUNK: usize = 1024 * 1024;
const PROVIDER_CONTROL_CHUNK: usize = 16 * 1024;
const MAX_PROVIDER_CONTROL_BODY: usize = 64 * 1024;
// These are resumptions inside one installed opaque grant. Retrying the signed
// provider plan itself still requires a new pull challenge and nonce.
const MAX_PROVIDER_RESUME_ATTEMPTS: usize = 3;

#[derive(Debug, Clone)]
pub(super) struct ProviderPullSession {
    stream_id: String,
    repository: String,
    endpoint_id: String,
    plan_nonce: [u8; PLAN_NONCE_LEN],
    provider_enabled: bool,
}

pub(super) struct CompletedProviderPull {
    pub(super) pack: CompletedProviderPack,
    pub(super) trailer_digest: [u8; DIGEST_LEN],
}

impl HostedClient {
    pub(super) fn begin_provider_pull(
        &self,
        stream_id: &str,
        repository: &str,
    ) -> Result<(ProviderPullSession, Vec<u8>), ProtocolError> {
        let mut plan_nonce = [0; PLAN_NONCE_LEN];
        rand::fill(&mut plan_nonce);
        let session = ProviderPullSession {
            stream_id: stream_id.to_string(),
            repository: repository.to_string(),
            endpoint_id: self.connection.endpoint_id().to_string(),
            plan_nonce,
            provider_enabled: self.connection.supports_provider_transport(),
        };
        let capability_context = if session.provider_enabled {
            ProviderPullCapabilityContext {
                version: PROVIDER_VERSION,
                client_endpoint_id: session.endpoint_id.clone(),
                plan_nonce: session.plan_nonce.to_vec(),
            }
            .encode_to_vec()
        } else {
            Vec::new()
        };
        Ok((session, capability_context))
    }

    pub(super) fn answer_provider_challenge(
        &self,
        session: &ProviderPullSession,
        challenge: &ProviderPlanChallenge,
    ) -> Result<ProviderPlanResponse, ProtocolError> {
        if !session.provider_enabled {
            return Err(ProtocolError::InvalidState(
                "this client connection has no provider transport".to_string(),
            ));
        }
        validate_challenge(session, challenge)?;
        let signature = self
            .context
            .provider_plan_signature(
                &session.stream_id,
                &session.repository,
                &session.endpoint_id,
                &session.plan_nonce,
                &challenge.grant_batch_digest,
            )
            .map_err(hosted_to_protocol_error)?;
        Ok(ProviderPlanResponse {
            version: PROVIDER_VERSION,
            plan_nonce: session.plan_nonce.to_vec(),
            grant_batch_digest: challenge.grant_batch_digest.clone(),
            signature,
            accepted: true,
        })
    }

    pub(super) fn decline_provider_challenge(
        &self,
        session: &ProviderPullSession,
        challenge: Option<&ProviderPlanChallenge>,
    ) -> ProviderPlanResponse {
        ProviderPlanResponse {
            version: PROVIDER_VERSION,
            plan_nonce: session.plan_nonce.to_vec(),
            grant_batch_digest: challenge
                .map(|challenge| challenge.grant_batch_digest.clone())
                .unwrap_or_default(),
            signature: Vec::new(),
            accepted: false,
        }
    }

    pub(super) async fn download_provider_pull(
        &self,
        session: &ProviderPullSession,
        challenge: &ProviderPlanChallenge,
        manifest: &ApiProviderPullManifest,
        spool_root: &Path,
    ) -> Result<CompletedProviderPull, ProtocolError> {
        let (wire_manifest, sources) = convert_manifest(session, challenge, manifest)?;
        let extents = wire_manifest
            .extents
            .iter()
            .zip(sources)
            .enumerate()
            .map(|(index, (extent, source))| ScheduledProviderExtent {
                index,
                source,
                length: extent.length,
                digest: extent.digest,
            })
            .collect();
        let backend = HostedProviderBackend { client: self };
        let download = download_provider_plan(
            spool_root,
            wire_manifest,
            extents,
            &backend,
            &self.transport,
        )
        .await?;
        tracing::debug!(
            peak_inflight_bytes = download.peak_inflight_bytes,
            configured_bytes = self.transport.provider_max_inflight_bytes,
            "provider pull byte-budget high-water mark"
        );
        Ok(CompletedProviderPull {
            trailer_digest: download.pack.trailer_digest,
            pack: download.pack,
        })
    }
}

impl ProviderPullSession {
    pub(super) fn fallback_response(&self, batch_digest: &[u8]) -> ProviderPullResponse {
        ProviderPullResponse {
            version: PROVIDER_VERSION,
            plan_nonce: self.plan_nonce.to_vec(),
            grant_batch_digest: batch_digest.to_vec(),
            status: ProviderPullResultStatus::Fallback as i32,
            pack_digest: Vec::new(),
        }
    }

    pub(super) fn complete_response(
        &self,
        batch_digest: &[u8],
        pack_digest: [u8; DIGEST_LEN],
    ) -> ProviderPullResponse {
        ProviderPullResponse {
            version: PROVIDER_VERSION,
            plan_nonce: self.plan_nonce.to_vec(),
            grant_batch_digest: batch_digest.to_vec(),
            status: ProviderPullResultStatus::Complete as i32,
            pack_digest: pack_digest.to_vec(),
        }
    }

    pub(super) fn resolve_download(
        &self,
        batch_digest: &[u8],
        result: Result<CompletedProviderPull, ProtocolError>,
    ) -> (ProviderPullResponse, Option<CompletedProviderPull>) {
        match result {
            Ok(completed) => (
                self.complete_response(batch_digest, completed.trailer_digest),
                Some(completed),
            ),
            Err(_) => (self.fallback_response(batch_digest), None),
        }
    }
}

fn validate_challenge(
    session: &ProviderPullSession,
    challenge: &ProviderPlanChallenge,
) -> Result<(), ProtocolError> {
    if challenge.version != PROVIDER_VERSION
        || challenge.plan_nonce != session.plan_nonce
        || challenge.grant_batch_digest.len() != DIGEST_LEN
    {
        return Err(ProtocolError::InvalidState(
            "provider plan challenge does not match the signed opening".to_string(),
        ));
    }
    let summary = challenge.summary.as_ref().ok_or_else(|| {
        ProtocolError::InvalidState("provider plan challenge has no safe summary".to_string())
    })?;
    let repository = summary.repository.as_ref().ok_or_else(|| {
        ProtocolError::InvalidState("provider plan summary has no repository".to_string())
    })?;
    if repository
        != &super::helpers::repository_ref(&session.repository).ok_or_else(|| {
            ProtocolError::InvalidState("provider session repository is invalid".to_string())
        })?
        || summary.extent_count == 0
        || summary.object_count == 0
        || summary.total_bytes == 0
        || summary.expires_at_unix_millis <= now_millis()?
    {
        return Err(ProtocolError::InvalidState(
            "provider plan summary is invalid or expired".to_string(),
        ));
    }
    Ok(())
}

fn convert_manifest(
    session: &ProviderPullSession,
    challenge: &ProviderPlanChallenge,
    manifest: &ApiProviderPullManifest,
) -> Result<(ProviderPackManifest, Vec<ProviderSource>), ProtocolError> {
    validate_challenge(session, challenge)?;
    if manifest.version != PROVIDER_VERSION
        || manifest.plan_nonce != session.plan_nonce
        || manifest.grant_batch_digest != challenge.grant_batch_digest
    {
        return Err(ProtocolError::InvalidState(
            "provider manifest does not match the consented exact plan".to_string(),
        ));
    }
    let header: [u8; 16] = manifest.pack_header.as_slice().try_into().map_err(|_| {
        ProtocolError::InvalidState(
            "provider manifest pack header must be exactly 16 bytes".to_string(),
        )
    })?;
    let summary = challenge.summary.as_ref().ok_or_else(|| {
        ProtocolError::InvalidState("provider plan challenge has no safe summary".to_string())
    })?;
    if u64::try_from(manifest.extents.len()).ok() != Some(summary.extent_count) {
        return Err(ProtocolError::InvalidState(
            "provider manifest extent count differs from the consented summary".to_string(),
        ));
    }
    let now = now_millis()?;
    let mut sources = Vec::with_capacity(manifest.extents.len());
    let mut extents = Vec::with_capacity(manifest.extents.len());
    let mut extent_ids = HashSet::with_capacity(manifest.extents.len());
    let mut object_count = 0_u64;
    let mut total_bytes = 0_u64;
    for extent in &manifest.extents {
        let source = extent.source.as_ref().ok_or_else(|| {
            ProtocolError::InvalidState("provider extent has no source".to_string())
        })?;
        if extent.extent_id.is_empty() || !extent_ids.insert(extent.extent_id.as_str()) {
            return Err(ProtocolError::InvalidState(
                "provider extent identity is empty or duplicated".to_string(),
            ));
        }
        if source.expires_at_unix_millis <= now
            || source.expires_at_unix_millis > summary.expires_at_unix_millis
        {
            return Err(ProtocolError::InvalidState(
                "provider extent source is expired".to_string(),
            ));
        }
        let digest = extent.digest.as_slice().try_into().map_err(|_| {
            ProtocolError::InvalidState(
                "provider extent digest must be exactly 32 bytes".to_string(),
            )
        })?;
        let mut objects = Vec::with_capacity(extent.objects.len());
        object_count = object_count
            .checked_add(u64::try_from(extent.objects.len()).map_err(|_| {
                ProtocolError::InvalidState(
                    "provider manifest object count exceeds u64".to_string(),
                )
            })?)
            .ok_or_else(|| {
                ProtocolError::InvalidState("provider manifest object count overflows".to_string())
            })?;
        total_bytes = total_bytes.checked_add(extent.length).ok_or_else(|| {
            ProtocolError::InvalidState("provider manifest byte count overflows".to_string())
        })?;
        for object in &extent.objects {
            let descriptor = object.object.clone().ok_or_else(|| {
                ProtocolError::InvalidState("provider index entry has no object".to_string())
            })?;
            let info = super::helpers::parse_descriptor_to_info(descriptor)?;
            if !info.obj_type.packable() {
                return Err(ProtocolError::InvalidState(
                    "provider manifest contains a non-packable object".to_string(),
                ));
            }
            let id = match info.id {
                wire::ObjectId::Hash(hash) => PackObjectId::Hash(hash),
                wire::ObjectId::StateId(state) => PackObjectId::StateId(state),
                wire::ObjectId::StateAttachment { id, .. } => PackObjectId::Hash(*id.as_hash()),
            };
            objects.push(ProviderPackIndexEntry {
                id,
                output_offset: object.output_offset,
            });
        }
        extents.push(ProviderPackExtent {
            output_offset: extent.output_offset,
            length: extent.length,
            digest,
            objects,
        });
        sources.push(source.clone());
    }
    if object_count != summary.object_count || total_bytes != summary.total_bytes {
        return Err(ProtocolError::InvalidState(
            "provider manifest differs from the consented safe summary".to_string(),
        ));
    }
    Ok((
        ProviderPackManifest {
            header,
            output_pack_length: manifest.output_pack_length,
            extents,
        },
        sources,
    ))
}

#[derive(Clone, Debug)]
struct ScheduledProviderExtent {
    index: usize,
    source: ProviderSource,
    length: u64,
    digest: [u8; DIGEST_LEN],
}

struct ProviderPlanDownload {
    pack: CompletedProviderPack,
    peak_inflight_bytes: usize,
}

trait ProviderBackend: Sync {
    type Connection: Clone + Send;

    fn connect<'a>(
        &'a self,
        sources: &'a [ProviderSource],
    ) -> impl Future<Output = Result<Self::Connection, ProtocolError>> + Send + 'a;

    fn download(
        &self,
        connection: Self::Connection,
        extent: ScheduledProviderExtent,
        writer: ProviderPackWriter,
        byte_budget: InflightByteBudget,
        stall_timeout: std::time::Duration,
    ) -> impl Future<Output = Result<(), ProtocolError>> + Send;
}

struct HostedProviderBackend<'a> {
    client: &'a HostedClient,
}

impl ProviderBackend for HostedProviderBackend<'_> {
    type Connection = iroh::endpoint::Connection;

    async fn connect(&self, sources: &[ProviderSource]) -> Result<Self::Connection, ProtocolError> {
        let mut last_error = None;
        for source in sources {
            if source.expires_at_unix_millis <= now_millis()? {
                continue;
            }
            match self.client.connection.provider_connection(source).await {
                Ok(connection) => return Ok(connection),
                Err(error) => last_error = Some(hosted_to_protocol_error(error)),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            ProtocolError::InvalidState(
                "provider endpoint has no unexpired connection path".to_string(),
            )
        }))
    }

    async fn download(
        &self,
        connection: Self::Connection,
        extent: ScheduledProviderExtent,
        writer: ProviderPackWriter,
        byte_budget: InflightByteBudget,
        stall_timeout: std::time::Duration,
    ) -> Result<(), ProtocolError> {
        download_provider_extent(&connection, extent, writer, byte_budget, stall_timeout).await
    }
}

#[derive(Clone)]
struct InflightByteBudget {
    inner: Arc<InflightByteBudgetInner>,
}

struct InflightByteBudgetInner {
    semaphore: Arc<Semaphore>,
    limit: usize,
    current: AtomicUsize,
    peak: AtomicUsize,
}

struct InflightByteReservation {
    inner: Arc<InflightByteBudgetInner>,
    reserved_bytes: usize,
    buffered_bytes: usize,
    _permit: OwnedSemaphorePermit,
}

impl InflightByteBudget {
    fn new(limit: usize) -> Self {
        let limit = limit.clamp(1, Semaphore::MAX_PERMITS);
        Self {
            inner: Arc::new(InflightByteBudgetInner {
                semaphore: Arc::new(Semaphore::new(limit)),
                limit,
                current: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            }),
        }
    }

    fn chunk_limit(&self, requested: usize) -> usize {
        requested.min(self.inner.limit).max(1)
    }

    async fn reserve(&self, requested: usize) -> Result<InflightByteReservation, ProtocolError> {
        let bytes = self.chunk_limit(requested);
        let permits = u32::try_from(bytes).map_err(|_| {
            ProtocolError::InvalidState(
                "provider in-flight byte reservation exceeds u32".to_string(),
            )
        })?;
        let permit = Arc::clone(&self.inner.semaphore)
            .acquire_many_owned(permits)
            .await
            .map_err(|_| ProtocolError::InvalidState("provider byte budget closed".to_string()))?;
        Ok(InflightByteReservation {
            inner: Arc::clone(&self.inner),
            reserved_bytes: bytes,
            buffered_bytes: 0,
            _permit: permit,
        })
    }

    fn peak(&self) -> usize {
        self.inner.peak.load(Ordering::Acquire)
    }
}

impl InflightByteReservation {
    fn record_buffered(&mut self, bytes: usize) -> Result<(), ProtocolError> {
        if self.buffered_bytes != 0 || bytes > self.reserved_bytes {
            return Err(ProtocolError::InvalidState(
                "provider body buffer exceeds its in-flight reservation".to_string(),
            ));
        }
        self.buffered_bytes = bytes;
        let current = self.inner.current.fetch_add(bytes, Ordering::AcqRel) + bytes;
        self.inner.peak.fetch_max(current, Ordering::AcqRel);
        Ok(())
    }
}

impl Drop for InflightByteReservation {
    fn drop(&mut self) {
        self.inner
            .current
            .fetch_sub(self.buffered_bytes, Ordering::AcqRel);
    }
}

async fn download_provider_plan<B: ProviderBackend>(
    spool_root: &Path,
    manifest: ProviderPackManifest,
    extents: Vec<ScheduledProviderExtent>,
    backend: &B,
    policy: &HostedTransportPolicy,
) -> Result<ProviderPlanDownload, ProtocolError> {
    let spool = ProviderPackSpool::new_in(spool_root, manifest)?;
    let writer = spool.writer();
    let byte_budget = InflightByteBudget::new(policy.provider_max_inflight_bytes);
    let connection_limit = Arc::new(Semaphore::new(policy.provider_global_concurrency));
    let stream_limit = Arc::new(Semaphore::new(policy.provider_global_concurrency));
    let mut groups = BTreeMap::<String, Vec<ScheduledProviderExtent>>::new();
    for extent in extents {
        groups
            .entry(extent.source.endpoint_id.clone())
            .or_default()
            .push(extent);
    }

    let mut pending = FuturesUnordered::new();
    for extents in groups.into_values() {
        pending.push(download_provider_group(
            backend,
            extents,
            writer.clone(),
            Arc::clone(&connection_limit),
            Arc::clone(&stream_limit),
            byte_budget.clone(),
            policy,
        ));
    }
    while let Some(result) = pending.next().await {
        result?;
    }
    drop(writer);

    let peak_inflight_bytes = byte_budget.peak();
    Ok(ProviderPlanDownload {
        pack: spool.finish()?,
        peak_inflight_bytes,
    })
}

async fn download_provider_group<B: ProviderBackend>(
    backend: &B,
    extents: Vec<ScheduledProviderExtent>,
    writer: ProviderPackWriter,
    connection_limit: Arc<Semaphore>,
    stream_limit: Arc<Semaphore>,
    byte_budget: InflightByteBudget,
    policy: &HostedTransportPolicy,
) -> Result<(), ProtocolError> {
    let _connection_permit = connection_limit.acquire_owned().await.map_err(|_| {
        ProtocolError::InvalidState("provider connection scheduler closed".to_string())
    })?;
    for extent in &extents {
        ensure_grant_unexpired(&extent.source)?;
    }
    let sources = extents
        .iter()
        .map(|extent| extent.source.clone())
        .collect::<Vec<_>>();
    let connection = tokio::time::timeout(policy.provider_stall_timeout, backend.connect(&sources))
        .await
        .map_err(|_| provider_stall_error())??;

    let endpoint_limit = Arc::new(Semaphore::new(policy.provider_per_endpoint_concurrency));
    let mut pending = FuturesUnordered::new();
    for extent in extents {
        let endpoint_limit = Arc::clone(&endpoint_limit);
        let stream_limit = Arc::clone(&stream_limit);
        let connection = connection.clone();
        let writer = writer.clone();
        let byte_budget = byte_budget.clone();
        pending.push(async move {
            let _endpoint_permit = endpoint_limit.acquire_owned().await.map_err(|_| {
                ProtocolError::InvalidState("provider endpoint scheduler closed".to_string())
            })?;
            let _stream_permit = stream_limit.acquire_owned().await.map_err(|_| {
                ProtocolError::InvalidState("provider stream scheduler closed".to_string())
            })?;
            ensure_grant_unexpired(&extent.source)?;
            backend
                .download(
                    connection,
                    extent,
                    writer,
                    byte_budget,
                    policy.provider_stall_timeout,
                )
                .await
        });
    }
    while let Some(result) = pending.next().await {
        result?;
    }
    Ok(())
}

fn ensure_grant_unexpired(source: &ProviderSource) -> Result<(), ProtocolError> {
    if source.expires_at_unix_millis <= now_millis()? {
        return Err(ProtocolError::InvalidState(
            "provider grant expired before its extent started".to_string(),
        ));
    }
    Ok(())
}

fn provider_stall_error() -> ProtocolError {
    ProtocolError::InvalidState(
        "provider extent exceeded the configured fallback threshold".to_string(),
    )
}

async fn download_provider_extent(
    connection: &iroh::endpoint::Connection,
    extent: ScheduledProviderExtent,
    writer: ProviderPackWriter,
    byte_budget: InflightByteBudget,
    stall_timeout: std::time::Duration,
) -> Result<(), ProtocolError> {
    let mut retained_length = 0_u64;
    let mut previous_generation = None;
    let mut last_retryable = None;

    for _ in 0..MAX_PROVIDER_RESUME_ATTEMPTS {
        let mut attempt = ExtentAttemptState {
            expected_length: extent.length,
            expected_digest: extent.digest,
            writer: &writer,
            extent_index: extent.index,
            retained_length: &mut retained_length,
            previous_generation: &mut previous_generation,
        };
        match download_attempt(
            connection,
            &extent.source.opaque_ticket,
            &mut attempt,
            &byte_budget,
            stall_timeout,
        )
        .await
        {
            Ok(_) => {
                writer.mark_verified(extent.index)?;
                return Ok(());
            }
            Err(AttemptFailure::Retryable { error }) => last_retryable = Some(error),
            Err(AttemptFailure::Fatal(error)) => return Err(error),
        }
    }
    Err(last_retryable.unwrap_or_else(|| {
        ProtocolError::InvalidState("provider transfer attempts exhausted".to_string())
    }))
}

#[derive(Debug)]
enum AttemptFailure {
    Retryable { error: ProtocolError },
    Fatal(ProtocolError),
}

struct ExtentAttemptState<'a> {
    expected_length: u64,
    expected_digest: [u8; DIGEST_LEN],
    writer: &'a ProviderPackWriter,
    extent_index: usize,
    retained_length: &'a mut u64,
    previous_generation: &'a mut Option<u64>,
}

async fn download_attempt(
    connection: &iroh::endpoint::Connection,
    opaque_ticket: &str,
    state: &mut ExtentAttemptState<'_>,
    byte_budget: &InflightByteBudget,
    stall_timeout: std::time::Duration,
) -> Result<u64, AttemptFailure> {
    let (mut send, recv) = await_provider_progress(
        stall_timeout,
        async { connection.open_bi().await.map_err(transport_error) },
        true,
    )
    .await?;
    await_provider_progress(
        stall_timeout,
        send_provider_frame(
            &mut send,
            &ProviderReadClientFrame {
                frame: Some(provider_read_client_frame::Frame::Request(
                    ProviderReadRequest {
                        version: PROVIDER_VERSION,
                        opaque_ticket: opaque_ticket.to_string(),
                    },
                )),
            },
        ),
        true,
    )
    .await?;

    let mut reader = ProviderStreamReader::new(recv);
    let ready = await_provider_progress(stall_timeout, read_ready(&mut reader), false).await?;
    if ready.resume_offset > state.expected_length
        || ready.resume_offset > *state.retained_length
        || ready.remaining_length != state.expected_length - ready.resume_offset
        || ready.attempt_generation == 0
        || state
            .previous_generation
            .is_some_and(|previous| previous == ready.attempt_generation)
    {
        abort_provider_stream(&mut send, &mut reader);
        return Err(AttemptFailure::Fatal(ProtocolError::InvalidState(
            "provider returned an invalid resume offset or attempt generation".to_string(),
        )));
    }
    *state.previous_generation = Some(ready.attempt_generation);
    *state.retained_length = ready.resume_offset;
    let mut hasher = blake3::Hasher::new();
    state
        .writer
        .hash_extent_prefix(state.extent_index, ready.resume_offset, &mut hasher)
        .map_err(AttemptFailure::Fatal)?;
    let prefix_rehashed = ready.resume_offset;

    let raw_length = await_provider_progress(stall_timeout, reader.next_raw_body(), true).await?;
    if raw_length != ready.remaining_length {
        abort_provider_stream(&mut send, &mut reader);
        return Err(AttemptFailure::Fatal(ProtocolError::InvalidState(
            "provider raw body length does not match its resume response".to_string(),
        )));
    }

    while reader.raw_remaining() != 0 {
        let requested = usize::try_from(reader.raw_remaining().min(PROVIDER_READ_CHUNK as u64))
            .unwrap_or(PROVIDER_READ_CHUNK);
        let mut reservation = byte_budget
            .reserve(requested)
            .await
            .map_err(AttemptFailure::Fatal)?;
        let Some(chunk) = await_provider_progress(
            stall_timeout,
            reader.read_raw_chunk(reservation.reserved_bytes),
            true,
        )
        .await?
        else {
            break;
        };
        reservation
            .record_buffered(chunk.len())
            .map_err(AttemptFailure::Fatal)?;
        hasher.update(&chunk);
        state
            .writer
            .write_extent_chunk(state.extent_index, *state.retained_length, &chunk)
            .map_err(AttemptFailure::Fatal)?;
        *state.retained_length = state
            .retained_length
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| {
                AttemptFailure::Fatal(ProtocolError::InvalidState(
                    "provider retained length overflows".to_string(),
                ))
            })?;
        await_provider_progress(
            stall_timeout,
            send_provider_frame(
                &mut send,
                &ProviderReadClientFrame {
                    frame: Some(provider_read_client_frame::Frame::Checkpoint(
                        ProviderReadCheckpoint {
                            attempt_generation: ready.attempt_generation,
                            acknowledged_length: *state.retained_length,
                            final_digest: Vec::new(),
                        },
                    )),
                },
            ),
            true,
        )
        .await?;
    }

    if *state.retained_length != state.expected_length
        || hasher.finalize().as_bytes() != &state.expected_digest
    {
        abort_provider_stream(&mut send, &mut reader);
        return Err(AttemptFailure::Fatal(ProtocolError::InvalidState(
            "provider extent digest mismatch".to_string(),
        )));
    }
    await_provider_progress(
        stall_timeout,
        send_provider_frame(
            &mut send,
            &ProviderReadClientFrame {
                frame: Some(provider_read_client_frame::Frame::Checkpoint(
                    ProviderReadCheckpoint {
                        attempt_generation: ready.attempt_generation,
                        acknowledged_length: state.expected_length,
                        final_digest: state.expected_digest.to_vec(),
                    },
                )),
            },
        ),
        true,
    )
    .await?;
    send.finish().map_err(|error| AttemptFailure::Retryable {
        error: transport_error(error),
    })?;
    let complete =
        await_provider_progress(stall_timeout, read_complete(&mut reader), false).await?;
    if !complete.success || complete.committed_length != state.expected_length {
        return Err(AttemptFailure::Fatal(ProtocolError::InvalidState(
            "provider did not commit the exact verified extent".to_string(),
        )));
    }
    Ok(prefix_rehashed)
}

async fn await_provider_progress<T>(
    stall_timeout: std::time::Duration,
    future: impl Future<Output = Result<T, ProtocolError>>,
    retryable: bool,
) -> Result<T, AttemptFailure> {
    match tokio::time::timeout(stall_timeout, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) if retryable => Err(AttemptFailure::Retryable { error }),
        Ok(Err(error)) => Err(AttemptFailure::Fatal(error)),
        Err(_) => Err(AttemptFailure::Fatal(provider_stall_error())),
    }
}

async fn send_provider_frame(
    send: &mut iroh::endpoint::SendStream,
    frame: &ProviderReadClientFrame,
) -> Result<(), ProtocolError> {
    let encoded = encode_stream_message(&frame.encode_to_vec())
        .map_err(|error| ProtocolError::Serialization(error.to_string()))?;
    send.write_all(&encoded).await.map_err(transport_error)
}

async fn read_ready(reader: &mut ProviderStreamReader) -> Result<ProviderReadReady, ProtocolError> {
    let frame = reader.next_message().await?;
    let frame = ProviderReadServerFrame::decode(frame.as_slice())
        .map_err(|error| ProtocolError::Serialization(error.to_string()))?;
    match frame.frame {
        Some(provider_read_server_frame::Frame::Ready(ready)) => Ok(ready),
        _ => Err(ProtocolError::InvalidState(
            "provider did not begin with ProviderReadReady".to_string(),
        )),
    }
}

async fn read_complete(
    reader: &mut ProviderStreamReader,
) -> Result<ProviderReadComplete, ProtocolError> {
    let frame = reader.next_message().await?;
    let frame = ProviderReadServerFrame::decode(frame.as_slice())
        .map_err(|error| ProtocolError::Serialization(error.to_string()))?;
    match frame.frame {
        Some(provider_read_server_frame::Frame::Complete(complete)) => Ok(complete),
        _ => Err(ProtocolError::InvalidState(
            "provider did not finish with ProviderReadComplete".to_string(),
        )),
    }
}

struct ProviderStreamReader {
    recv: iroh::endpoint::RecvStream,
    buffered: Vec<u8>,
    raw_remaining: u64,
}

impl ProviderStreamReader {
    fn new(recv: iroh::endpoint::RecvStream) -> Self {
        Self {
            recv,
            buffered: Vec::new(),
            raw_remaining: 0,
        }
    }

    async fn next_message(&mut self) -> Result<Vec<u8>, ProtocolError> {
        if self.raw_remaining != 0 {
            return Err(ProtocolError::InvalidState(
                "provider control frame arrived within a raw body".to_string(),
            ));
        }
        match self.next_frame().await? {
            OwnedProviderFrame::Message(message) => Ok(message),
            OwnedProviderFrame::Failure(failure) => Err(ProtocolError::Remote(failure.message)),
            OwnedProviderFrame::RawBody(_) => Err(ProtocolError::InvalidState(
                "provider sent a raw body where control was required".to_string(),
            )),
        }
    }

    async fn next_raw_body(&mut self) -> Result<u64, ProtocolError> {
        match self.next_frame().await? {
            OwnedProviderFrame::RawBody(length) => {
                self.raw_remaining = length;
                Ok(length)
            }
            _ => Err(ProtocolError::InvalidState(
                "provider did not send the declared extent body".to_string(),
            )),
        }
    }

    async fn next_frame(&mut self) -> Result<OwnedProviderFrame, ProtocolError> {
        loop {
            if let Some((frame, consumed)) = decode_stream_frame(&self.buffered)
                .map_err(|error| ProtocolError::Serialization(error.to_string()))?
            {
                let owned = match frame {
                    StreamFrame::Message(message) => OwnedProviderFrame::Message(message.to_vec()),
                    StreamFrame::Failure(failure) => OwnedProviderFrame::Failure(failure),
                    StreamFrame::RawBody { length } => OwnedProviderFrame::RawBody(length),
                };
                self.buffered.drain(..consumed);
                return Ok(owned);
            }
            let chunk = self
                .recv
                .read_chunk(PROVIDER_CONTROL_CHUNK)
                .await
                .map_err(transport_error)?
                .ok_or_else(|| {
                    ProtocolError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "provider stream ended before its next frame",
                    ))
                })?;
            self.buffered.extend_from_slice(&chunk);
            if self.buffered.len() > MAX_PROVIDER_CONTROL_BODY + 5 {
                return Err(ProtocolError::InvalidState(
                    "provider control frame exceeds its bounded buffer".to_string(),
                ));
            }
        }
    }

    fn raw_remaining(&self) -> u64 {
        self.raw_remaining
    }

    async fn read_raw_chunk(&mut self, maximum: usize) -> Result<Option<Bytes>, ProtocolError> {
        if self.raw_remaining == 0 {
            return Ok(None);
        }
        if !self.buffered.is_empty() {
            let length = self
                .buffered
                .len()
                .min(maximum.max(1))
                .min(usize::try_from(self.raw_remaining).unwrap_or(usize::MAX));
            let chunk = Bytes::copy_from_slice(&self.buffered[..length]);
            self.buffered.drain(..length);
            self.raw_remaining -= length as u64;
            return Ok(Some(chunk));
        }
        let chunk = self
            .recv
            .read_chunk(maximum.max(1))
            .await
            .map_err(transport_error)?
            .ok_or_else(|| {
                ProtocolError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "provider stream ended within its declared body",
                ))
            })?;
        let accepted = chunk
            .len()
            .min(usize::try_from(self.raw_remaining).unwrap_or(usize::MAX));
        if accepted < chunk.len() {
            self.buffered.extend_from_slice(&chunk[accepted..]);
        }
        self.raw_remaining -= accepted as u64;
        Ok(Some(chunk.slice(..accepted)))
    }
}

enum OwnedProviderFrame {
    Message(Vec<u8>),
    Failure(api::heddle::api::v1alpha1::CallFailure),
    RawBody(u64),
}

fn abort_provider_stream(send: &mut iroh::endpoint::SendStream, reader: &mut ProviderStreamReader) {
    let _ = send.reset(1_u32.into());
    let _ = reader.recv.stop(1_u32.into());
}

fn transport_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::Io(io::Error::other(error.to_string()))
}

fn now_millis() -> Result<u64, ProtocolError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?
        .as_millis();
    u64::try_from(millis)
        .map_err(|_| ProtocolError::InvalidState("system time exceeds u64".to_string()))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        fs,
        net::Ipv4Addr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use api::{
        framing::{encode_stream_message, encode_stream_raw_body},
        heddle::api::v1alpha1::{
            ProviderReadServerFrame, provider_read_client_frame, provider_read_server_frame,
        },
        signing,
    };
    use crypto::{Ed25519Signer, Signer as _};
    use iroh::{Endpoint, RelayMode, endpoint::presets};
    use objects::{
        object::Blob,
        store::{
            CompressionConfig, FsStore,
            pack::{ObjectType, PackBuilder, PackIndex, PackObjectId},
        },
    };

    use super::*;
    use crate::hosted::CallContextFactory;

    #[derive(Clone)]
    struct FakeProviderConnection {
        endpoint_id: String,
    }

    struct FakeProviderBackend {
        bodies: HashMap<String, Arc<Vec<u8>>>,
        delays: HashMap<String, Duration>,
        stalled: HashSet<String>,
        connections: Mutex<HashMap<String, usize>>,
        completion_order: Mutex<Vec<String>>,
        active_streams: Arc<AtomicUsize>,
        cancelled_streams: Arc<AtomicUsize>,
        chunk_size: usize,
    }

    struct FakeStreamGuard {
        active: Arc<AtomicUsize>,
        cancelled: Arc<AtomicUsize>,
        completed: bool,
    }

    impl FakeProviderBackend {
        fn new(bodies: HashMap<String, Arc<Vec<u8>>>) -> Self {
            Self {
                bodies,
                delays: HashMap::new(),
                stalled: HashSet::new(),
                connections: Mutex::new(HashMap::new()),
                completion_order: Mutex::new(Vec::new()),
                active_streams: Arc::new(AtomicUsize::new(0)),
                cancelled_streams: Arc::new(AtomicUsize::new(0)),
                chunk_size: 64 * 1024,
            }
        }

        fn connection_count(&self, endpoint_id: &str) -> usize {
            self.connections
                .lock()
                .unwrap()
                .get(endpoint_id)
                .copied()
                .unwrap_or(0)
        }
    }

    impl ProviderBackend for FakeProviderBackend {
        type Connection = FakeProviderConnection;

        async fn connect(
            &self,
            sources: &[ProviderSource],
        ) -> Result<Self::Connection, ProtocolError> {
            let endpoint_id = sources
                .first()
                .ok_or_else(|| {
                    ProtocolError::InvalidState("fake provider group is empty".to_string())
                })?
                .endpoint_id
                .clone();
            *self
                .connections
                .lock()
                .unwrap()
                .entry(endpoint_id.clone())
                .or_default() += 1;
            Ok(FakeProviderConnection { endpoint_id })
        }

        async fn download(
            &self,
            connection: Self::Connection,
            extent: ScheduledProviderExtent,
            writer: ProviderPackWriter,
            byte_budget: InflightByteBudget,
            stall_timeout: Duration,
        ) -> Result<(), ProtocolError> {
            if connection.endpoint_id != extent.source.endpoint_id {
                return Err(ProtocolError::InvalidState(
                    "fake provider connection crossed endpoint groups".to_string(),
                ));
            }
            let ticket = extent.source.opaque_ticket.clone();
            let mut guard = FakeStreamGuard {
                active: Arc::clone(&self.active_streams),
                cancelled: Arc::clone(&self.cancelled_streams),
                completed: false,
            };
            guard.active.fetch_add(1, Ordering::AcqRel);
            if self.stalled.contains(&ticket) {
                tokio::time::timeout(stall_timeout, std::future::pending::<()>())
                    .await
                    .map_err(|_| provider_stall_error())?;
            }
            if let Some(delay) = self.delays.get(&ticket) {
                tokio::time::sleep(*delay).await;
            }
            let body = Arc::clone(self.bodies.get(&ticket).ok_or_else(|| {
                ProtocolError::InvalidState("fake provider body is missing".to_string())
            })?);
            let mut hasher = blake3::Hasher::new();
            let mut offset = 0_usize;
            while offset < body.len() {
                let length = (body.len() - offset).min(self.chunk_size);
                let mut reservation = byte_budget.reserve(length).await?;
                let chunk = Bytes::copy_from_slice(&body[offset..offset + length]);
                reservation.record_buffered(chunk.len())?;
                tokio::task::yield_now().await;
                hasher.update(&chunk);
                writer.write_extent_chunk(extent.index, offset as u64, &chunk)?;
                offset += length;
                drop(reservation);
            }
            if offset as u64 != extent.length || hasher.finalize().as_bytes() != &extent.digest {
                return Err(ProtocolError::InvalidState(
                    "fake provider extent length or digest mismatch".to_string(),
                ));
            }
            writer.mark_verified(extent.index)?;
            self.completion_order.lock().unwrap().push(ticket);
            guard.completed = true;
            Ok(())
        }
    }

    impl Drop for FakeStreamGuard {
        fn drop(&mut self) {
            self.active.fetch_sub(1, Ordering::AcqRel);
            if !self.completed {
                self.cancelled.fetch_add(1, Ordering::AcqRel);
            }
        }
    }

    struct ProviderFixture {
        manifest: ProviderPackManifest,
        extents: Vec<ScheduledProviderExtent>,
        bodies: HashMap<String, Arc<Vec<u8>>>,
        source_pack: Vec<u8>,
        source_index: Vec<u8>,
    }

    fn provider_fixture(payload_sizes: &[usize], endpoints: &[&str]) -> ProviderFixture {
        assert_eq!(payload_sizes.len(), endpoints.len());
        let mut builder = PackBuilder::new(CompressionConfig::disabled());
        let mut ids = Vec::new();
        for (index, size) in payload_sizes.iter().copied().enumerate() {
            let data = (0..size)
                .map(|offset| ((offset.wrapping_mul(31) + index * 17) % 251) as u8)
                .collect::<Vec<_>>();
            let blob = Blob::new(data.clone());
            let id = PackObjectId::Hash(blob.hash());
            builder.add_id(id, ObjectType::Blob, data);
            ids.push(id);
        }
        let (source_pack, source_index, _) = builder.build().unwrap();
        let parsed_index = PackIndex::from_bytes(&source_index).unwrap();
        let mut ordered = ids
            .iter()
            .copied()
            .enumerate()
            .map(|(index, id)| (parsed_index.find(&id).unwrap(), index, id))
            .collect::<Vec<_>>();
        ordered.sort_unstable_by_key(|(offset, _, _)| *offset);

        let body_end = source_pack.len() - DIGEST_LEN;
        let expires_at_unix_millis = now_millis().unwrap() + 60_000;
        let mut extents = Vec::new();
        let mut scheduled = Vec::new();
        let mut bodies = HashMap::new();
        for (position, (output_offset, source_index, id)) in ordered.iter().copied().enumerate() {
            let end = ordered
                .get(position + 1)
                .map(|(offset, _, _)| *offset as usize)
                .unwrap_or(body_end);
            let body = Arc::new(source_pack[output_offset as usize..end].to_vec());
            let ticket = format!("extent-{source_index}");
            let digest = *blake3::hash(&body).as_bytes();
            let extent = ProviderPackExtent {
                output_offset,
                length: body.len() as u64,
                digest,
                objects: vec![ProviderPackIndexEntry { id, output_offset }],
            };
            let source = ProviderSource {
                provider_id: format!("provider-{}", endpoints[source_index]),
                endpoint_id: endpoints[source_index].to_string(),
                direct_url: format!(
                    "wss://{}.invalid/direct?provider=provider-{}&ticket={ticket}",
                    endpoints[source_index], endpoints[source_index]
                ),
                opaque_ticket: ticket.clone(),
                expires_at_unix_millis,
            };
            let index = extents.len();
            scheduled.push(ScheduledProviderExtent {
                index,
                source,
                length: extent.length,
                digest,
            });
            extents.push(extent);
            bodies.insert(ticket, body);
        }
        ProviderFixture {
            manifest: ProviderPackManifest {
                header: source_pack[..16].try_into().unwrap(),
                output_pack_length: source_pack.len() as u64,
                extents,
            },
            extents: scheduled,
            bodies,
            source_pack,
            source_index,
        }
    }

    fn provider_policy() -> HostedTransportPolicy {
        HostedTransportPolicy {
            chunk_size: 64 * 1024,
            max_inflight_objects: 4,
            resume_attempts: 2,
            provider_global_concurrency: 4,
            provider_per_endpoint_concurrency: 2,
            provider_max_inflight_bytes: 512 * 1024,
            provider_stall_timeout: Duration::from_millis(100),
        }
    }

    fn provider_session() -> ProviderPullSession {
        ProviderPullSession {
            stream_id: "pull:one".to_string(),
            repository: "acme/widgets".to_string(),
            endpoint_id: "11".repeat(32),
            plan_nonce: [4; PLAN_NONCE_LEN],
            provider_enabled: true,
        }
    }

    fn assert_download_falls_back(result: Result<ProviderPlanDownload, ProtocolError>) {
        let result = result.map(|download| CompletedProviderPull {
            trailer_digest: download.pack.trailer_digest,
            pack: download.pack,
        });
        let (response, completed) = provider_session().resolve_download(&[7; DIGEST_LEN], result);
        assert_eq!(response.status, ProviderPullResultStatus::Fallback as i32);
        assert!(response.pack_digest.is_empty());
        assert!(completed.is_none());
    }

    #[test]
    fn captured_exact_plan_signature_does_not_verify_for_another_plan() {
        let signer = Ed25519Signer::generate().unwrap();
        let context = CallContextFactory::default()
            .with_signing_key_pem(&signer.to_pem().unwrap(), "principal:alice")
            .unwrap();
        let first_digest = [7; DIGEST_LEN];
        let second_digest = [8; DIGEST_LEN];
        let capability_context = ProviderPullCapabilityContext {
            version: PROVIDER_VERSION,
            client_endpoint_id: "11".repeat(32),
            plan_nonce: vec![4; PLAN_NONCE_LEN],
        }
        .encode_to_vec();
        let opening_signature = signer
            .sign(&signing::stream_open_bytes(
                "principal:alice",
                "pull:one",
                "/heddle.api.v1alpha1.RepoSyncService/Pull",
                "acme/widgets",
                "",
                &capability_context,
            ))
            .unwrap();
        let signature = context
            .provider_plan_signature(
                "pull:one",
                "acme/widgets",
                &"11".repeat(32),
                &[4; PLAN_NONCE_LEN],
                &first_digest,
            )
            .unwrap();
        let replayed_bytes = signing::provider_plan_bytes(
            "principal:alice",
            "pull:one",
            "acme/widgets",
            &"11".repeat(32),
            &[4; PLAN_NONCE_LEN],
            &second_digest,
        );

        assert!(
            Ed25519Signer::verify_with_public_key(
                &replayed_bytes,
                signer.public_key(),
                &opening_signature,
            )
            .is_err(),
            "a captured opening signature must not authorize any exact plan"
        );
        assert!(
            Ed25519Signer::verify_with_public_key(
                &replayed_bytes,
                signer.public_key(),
                &signature,
            )
            .is_err(),
            "one exact-plan signature must not authorize a different batch digest"
        );
        println!(
            "replay_resistance opening_as_plan=rejected captured_plan={} replayed_plan={} exact_plan_replay=rejected",
            hex::encode(first_digest),
            hex::encode(second_digest),
        );
    }

    #[tokio::test]
    async fn distinct_endpoints_complete_out_of_order_into_a_byte_identical_pack() {
        let fixture = provider_fixture(&[128 * 1024, 128 * 1024], &["endpoint-a", "endpoint-b"]);
        let mut backend = FakeProviderBackend::new(fixture.bodies);
        backend
            .delays
            .insert("extent-0".to_string(), Duration::from_millis(40));
        let root = tempfile::tempdir().unwrap();

        let mut download = download_provider_plan(
            root.path(),
            fixture.manifest,
            fixture.extents,
            &backend,
            &provider_policy(),
        )
        .await
        .unwrap();

        assert_eq!(
            backend.completion_order.lock().unwrap().as_slice(),
            ["extent-1", "extent-0"]
        );
        assert_eq!(
            download.pack.trailer_digest.as_slice(),
            &fixture.source_pack[fixture.source_pack.len() - DIGEST_LEN..]
        );
        let store_root = tempfile::tempdir().unwrap();
        let store = FsStore::new(store_root.path().join(".heddle"));
        let installed = download.pack.install_into(&store).unwrap();
        assert_eq!(
            installed.len(),
            PackIndex::from_bytes(&fixture.source_index)
                .unwrap()
                .ids()
                .len()
        );
        println!(
            "provider_out_of_order completion=endpoint-b,endpoint-a pack_bytes={} byte_identical=true",
            fixture.source_pack.len()
        );
    }

    #[tokio::test]
    async fn one_endpoint_reuses_one_connection_for_multiple_grants() {
        let fixture = provider_fixture(
            &[64 * 1024, 64 * 1024, 64 * 1024],
            &["endpoint-a", "endpoint-a", "endpoint-a"],
        );
        let backend = FakeProviderBackend::new(fixture.bodies);
        let root = tempfile::tempdir().unwrap();

        download_provider_plan(
            root.path(),
            fixture.manifest,
            fixture.extents,
            &backend,
            &provider_policy(),
        )
        .await
        .unwrap();

        assert_eq!(backend.connection_count("endpoint-a"), 1);
        println!("provider_group grants=3 endpoint=endpoint-a connections=1 reused=true");
    }

    #[tokio::test]
    async fn peak_provider_body_memory_is_constant_as_pack_size_grows() {
        async fn measure(payload_size: usize) -> (usize, usize) {
            let quarter = payload_size / 4;
            let fixture = provider_fixture(
                &[quarter, quarter, quarter, quarter],
                &["endpoint-a", "endpoint-b", "endpoint-c", "endpoint-d"],
            );
            let pack_size = fixture.source_pack.len();
            let mut backend = FakeProviderBackend::new(fixture.bodies);
            backend.chunk_size = 256 * 1024;
            let root = tempfile::tempdir().unwrap();
            let download = download_provider_plan(
                root.path(),
                fixture.manifest,
                fixture.extents,
                &backend,
                &provider_policy(),
            )
            .await
            .unwrap();
            (pack_size, download.peak_inflight_bytes)
        }

        let (small_pack, small_peak) = measure(1024 * 1024).await;
        let (large_pack, large_peak) = measure(16 * 1024 * 1024).await;

        assert!(large_pack >= small_pack * 10);
        assert!(small_peak <= 512 * 1024);
        assert!(large_peak <= 512 * 1024);
        assert_eq!(small_peak, large_peak);
        println!(
            "provider_memory method=production-live-body-buffer-high-water small_pack={small_pack} small_peak={small_peak} large_pack={large_pack} large_peak={large_peak} bounded=true"
        );
    }

    #[tokio::test]
    async fn stalled_lane_does_not_block_healthy_lane_before_fallback_threshold() {
        let fixture = provider_fixture(&[128 * 1024, 128 * 1024], &["endpoint-a", "endpoint-b"]);
        let mut backend = FakeProviderBackend::new(fixture.bodies);
        backend.stalled.insert("extent-0".to_string());
        let root = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();

        let result = download_provider_plan(
            root.path(),
            fixture.manifest,
            fixture.extents,
            &backend,
            &provider_policy(),
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(
            backend.completion_order.lock().unwrap().as_slice(),
            ["extent-1"]
        );
        assert!(elapsed >= Duration::from_millis(90));
        assert!(elapsed < Duration::from_millis(500));
        assert_download_falls_back(result);
        println!(
            "provider_slow_lane healthy_completed=true fallback_after_ms={} threshold_ms=100",
            elapsed.as_millis()
        );
    }

    #[tokio::test]
    async fn invalid_provider_extents_all_select_existing_weft_fallback() {
        let fixture = provider_fixture(&[64 * 1024, 64 * 1024], &["a", "b"]);
        let mut overlapping = fixture.manifest.clone();
        overlapping.extents[1].output_offset -= 1;
        let root = tempfile::tempdir().unwrap();
        assert_download_falls_back(
            download_provider_plan(
                root.path(),
                overlapping,
                fixture.extents.clone(),
                &FakeProviderBackend::new(fixture.bodies.clone()),
                &provider_policy(),
            )
            .await,
        );

        let mut gapped = fixture.manifest.clone();
        gapped.extents[1].output_offset += 1;
        assert_download_falls_back(
            download_provider_plan(
                root.path(),
                gapped,
                fixture.extents.clone(),
                &FakeProviderBackend::new(fixture.bodies.clone()),
                &provider_policy(),
            )
            .await,
        );

        let mut oversized_pack = fixture.manifest.clone();
        oversized_pack.output_pack_length = wire::MAX_RECEIVED_PACK_SIZE + 1;
        assert_download_falls_back(
            download_provider_plan(
                root.path(),
                oversized_pack,
                fixture.extents.clone(),
                &FakeProviderBackend::new(fixture.bodies.clone()),
                &provider_policy(),
            )
            .await,
        );

        let mut expired_extents = fixture.extents.clone();
        for extent in &mut expired_extents {
            extent.source.expires_at_unix_millis = 0;
        }
        assert_download_falls_back(
            download_provider_plan(
                root.path(),
                fixture.manifest.clone(),
                expired_extents,
                &FakeProviderBackend::new(fixture.bodies.clone()),
                &provider_policy(),
            )
            .await,
        );

        for mutation in ["truncated", "oversized", "digest-mismatched"] {
            let fixture = provider_fixture(&[64 * 1024], &["endpoint-a"]);
            let mut bodies = fixture.bodies.clone();
            let mut body = bodies["extent-0"].as_ref().clone();
            match mutation {
                "truncated" => {
                    body.pop();
                }
                "oversized" => body.push(0),
                "digest-mismatched" => body[0] ^= 0xff,
                _ => unreachable!(),
            }
            bodies.insert("extent-0".to_string(), Arc::new(body));
            assert_download_falls_back(
                download_provider_plan(
                    root.path(),
                    fixture.manifest,
                    fixture.extents,
                    &FakeProviderBackend::new(bodies),
                    &provider_policy(),
                )
                .await,
            );
        }
        println!(
            "provider_fail_closed cases=expired,overlap,gap,truncated,oversized,digest-mismatch fallback=existing-weft"
        );
    }

    #[tokio::test]
    async fn cancellation_drops_streams_and_partial_spool_state() {
        let fixture = provider_fixture(
            &[64 * 1024, 64 * 1024, 64 * 1024],
            &["endpoint-a", "endpoint-b", "endpoint-c"],
        );
        let mut bodies = fixture.bodies;
        let mut corrupt = bodies["extent-2"].as_ref().clone();
        corrupt[0] ^= 0xff;
        bodies.insert("extent-2".to_string(), Arc::new(corrupt));
        let mut backend = FakeProviderBackend::new(bodies);
        backend.stalled.insert("extent-0".to_string());
        backend.stalled.insert("extent-1".to_string());
        backend
            .delays
            .insert("extent-2".to_string(), Duration::from_millis(30));
        let root = tempfile::tempdir().unwrap();
        let mut policy = provider_policy();
        policy.provider_stall_timeout = Duration::from_millis(200);

        let result = download_provider_plan(
            root.path(),
            fixture.manifest,
            fixture.extents,
            &backend,
            &policy,
        )
        .await;

        assert_download_falls_back(result);
        assert_eq!(backend.active_streams.load(Ordering::Acquire), 0);
        assert!(backend.cancelled_streams.load(Ordering::Acquire) >= 2);
        let spool_entries = fs::read_dir(root.path().join("transfer-spool"))
            .unwrap()
            .count();
        assert_eq!(spool_entries, 0);
        println!(
            "provider_cancellation in_flight=2 streams_dropped={} spool_entries=0",
            backend.cancelled_streams.load(Ordering::Acquire)
        );
    }

    #[tokio::test]
    async fn interrupted_transfer_resumes_and_rehashes_the_retained_prefix() {
        let complete = b"retained verified prefix and resumed suffix".to_vec();
        let resume_offset = 24_usize;
        let expected_digest = *blake3::hash(&complete).as_bytes();
        let (
            first_client_endpoint,
            first_client_connection,
            first_server_endpoint,
            server_connection,
        ) = provider_connection_pair().await;
        let first_server_connection_guard = server_connection.clone();
        let server_bytes = complete.clone();
        let first_server_task = tokio::spawn(async move {
            let (mut first_send, first_recv) = server_connection.accept_bi().await.unwrap();
            let _first_reader = expect_read_request(first_recv).await;
            send_ready(
                &mut first_send,
                0,
                1,
                u64::try_from(server_bytes.len()).unwrap(),
            )
            .await;
            first_send
                .write_all(
                    &encode_stream_raw_body(u64::try_from(server_bytes.len()).unwrap()).unwrap(),
                )
                .await
                .unwrap();
            first_send
                .write_all(&server_bytes[..resume_offset])
                .await
                .unwrap();
            first_send.finish().unwrap();
        });
        let spool_root = tempfile::tempdir().unwrap();
        let (spool, writer) = attempt_spool(
            spool_root.path(),
            u64::try_from(complete.len()).unwrap(),
            expected_digest,
        );
        let budget = InflightByteBudget::new(PROVIDER_READ_CHUNK);
        let mut retained_length = 0;
        let mut generation = None;
        let mut attempt = ExtentAttemptState {
            expected_length: complete.len() as u64,
            expected_digest,
            writer: &writer,
            extent_index: 0,
            retained_length: &mut retained_length,
            previous_generation: &mut generation,
        };
        let first = download_attempt(
            &first_client_connection,
            "opaque",
            &mut attempt,
            &budget,
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(first, Err(AttemptFailure::Retryable { .. })));
        assert_eq!(retained_length, resume_offset as u64);
        drop(first_server_connection_guard);
        tokio::time::timeout(Duration::from_secs(2), first_server_task)
            .await
            .unwrap()
            .unwrap();
        first_client_endpoint.close().await;
        first_server_endpoint.close().await;

        let (client_endpoint, client_connection, server_endpoint, server_connection) =
            provider_connection_pair().await;
        let server_connection_guard = server_connection.clone();
        let server_bytes = complete.clone();
        let server_task = tokio::spawn(async move {
            let (mut second_send, second_recv) = server_connection.accept_bi().await.unwrap();
            let mut second_reader = expect_read_request(second_recv).await;
            send_ready(
                &mut second_send,
                u64::try_from(resume_offset).unwrap(),
                2,
                u64::try_from(server_bytes.len() - resume_offset).unwrap(),
            )
            .await;
            second_send
                .write_all(
                    &encode_stream_raw_body(
                        u64::try_from(server_bytes.len() - resume_offset).unwrap(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            second_send
                .write_all(&server_bytes[resume_offset..])
                .await
                .unwrap();
            expect_final_checkpoint(&mut second_reader, expected_digest).await;
            send_complete(
                &mut second_send,
                true,
                u64::try_from(server_bytes.len()).unwrap(),
            )
            .await;
            second_send.finish().unwrap();
        });

        let mut attempt = ExtentAttemptState {
            expected_length: complete.len() as u64,
            expected_digest,
            writer: &writer,
            extent_index: 0,
            retained_length: &mut retained_length,
            previous_generation: &mut generation,
        };
        let prefix_rehashed = download_attempt(
            &client_connection,
            "opaque",
            &mut attempt,
            &budget,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(retained_length, complete.len() as u64);
        assert_eq!(prefix_rehashed, u64::try_from(resume_offset).unwrap());
        writer.mark_verified(0).unwrap();
        spool.finish().unwrap();
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .unwrap()
            .unwrap();
        drop(server_connection_guard);
        println!(
            "resume_correctness interrupted_at={resume_offset} resume_offset={resume_offset} prefix_rehashed={prefix_rehashed} bytes_identical=true"
        );
        client_endpoint.close().await;
        server_endpoint.close().await;
    }

    #[tokio::test]
    async fn digest_mismatch_never_sends_a_final_ack_or_advances_committed_offset() {
        let expected = b"authorized provider bytes".to_vec();
        let corrupt = b"tampered---provider bytes".to_vec();
        assert_eq!(expected.len(), corrupt.len());
        let expected_digest = *blake3::hash(&expected).as_bytes();
        let (client_endpoint, client_connection, server_endpoint, server_connection) =
            provider_connection_pair().await;
        let server_task = tokio::spawn(async move {
            let (mut send, recv) = server_connection.accept_bi().await.unwrap();
            let mut reader = expect_read_request(recv).await;
            send_ready(&mut send, 0, 1, u64::try_from(corrupt.len()).unwrap()).await;
            send.write_all(&encode_stream_raw_body(u64::try_from(corrupt.len()).unwrap()).unwrap())
                .await
                .unwrap();
            send.write_all(&corrupt).await.unwrap();
            send.finish().unwrap();

            let mut committed_offset = 0_u64;
            while let Ok(frame) = reader.next_message().await {
                let frame = ProviderReadClientFrame::decode(frame.as_slice()).unwrap();
                if let Some(provider_read_client_frame::Frame::Checkpoint(checkpoint)) = frame.frame
                    && !checkpoint.final_digest.is_empty()
                    && checkpoint.final_digest == expected_digest
                {
                    committed_offset = checkpoint.acknowledged_length;
                }
            }
            committed_offset
        });

        let spool_root = tempfile::tempdir().unwrap();
        let (_spool, writer) = attempt_spool(
            spool_root.path(),
            u64::try_from(expected.len()).unwrap(),
            expected_digest,
        );
        let budget = InflightByteBudget::new(PROVIDER_READ_CHUNK);
        let mut retained_length = 0;
        let mut generation = None;
        let mut attempt = ExtentAttemptState {
            expected_length: expected.len() as u64,
            expected_digest,
            writer: &writer,
            extent_index: 0,
            retained_length: &mut retained_length,
            previous_generation: &mut generation,
        };
        let result = download_attempt(
            &client_connection,
            "opaque",
            &mut attempt,
            &budget,
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(
            result,
            Err(AttemptFailure::Fatal(ProtocolError::InvalidState(message)))
                if message == "provider extent digest mismatch"
        ));
        let committed_offset = tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(committed_offset, 0);
        println!(
            "digest_mismatch result=rejected committed_offset={committed_offset} final_ack_sent=false"
        );
        client_endpoint.close().await;
        server_endpoint.close().await;
    }

    #[test]
    fn fallback_stays_on_the_same_logical_pull_and_carries_no_success_digest() {
        let session = provider_session();

        let response = session.fallback_response(&[7; DIGEST_LEN]);

        assert_eq!(response.status, ProviderPullResultStatus::Fallback as i32);
        assert_eq!(response.plan_nonce, session.plan_nonce);
        assert_eq!(response.grant_batch_digest, [7; DIGEST_LEN]);
        assert!(response.pack_digest.is_empty());
        println!(
            "fallback provider=unavailable selected=existing-weft logical_pulls=1 same_nonce=true caller_protocol=unchanged success_digest_present=false"
        );
    }

    fn attempt_spool(
        root: &Path,
        extent_length: u64,
        digest: [u8; DIGEST_LEN],
    ) -> (ProviderPackSpool, ProviderPackWriter) {
        let mut header = [0_u8; 16];
        header[..4].copy_from_slice(b"LMPK");
        header[4..8].copy_from_slice(&3_u32.to_be_bytes());
        let spool = ProviderPackSpool::new_in(
            root,
            ProviderPackManifest {
                header,
                output_pack_length: 16 + extent_length + 32,
                extents: vec![ProviderPackExtent {
                    output_offset: 16,
                    length: extent_length,
                    digest,
                    objects: Vec::new(),
                }],
            },
        )
        .unwrap();
        let writer = spool.writer();
        (spool, writer)
    }

    async fn provider_connection_pair() -> (
        Endpoint,
        iroh::endpoint::Connection,
        Endpoint,
        iroh::endpoint::Connection,
    ) {
        let server = Endpoint::builder(presets::Minimal)
            .alpns(vec![api::PROVIDER_ALPN_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let server_addr = server.addr();
        let client = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let (client_connection, server_connection) =
            tokio::join!(client.connect(server_addr, api::PROVIDER_ALPN_V1), async {
                server
                    .accept()
                    .await
                    .expect("incoming provider connection")
                    .await
            },);
        (
            client,
            client_connection.unwrap(),
            server,
            server_connection.unwrap(),
        )
    }

    async fn expect_read_request(recv: iroh::endpoint::RecvStream) -> ProviderStreamReader {
        let mut reader = ProviderStreamReader::new(recv);
        let frame = reader.next_message().await.unwrap();
        let frame = ProviderReadClientFrame::decode(frame.as_slice()).unwrap();
        assert!(matches!(
            frame.frame,
            Some(provider_read_client_frame::Frame::Request(
                ProviderReadRequest {
                    version: PROVIDER_VERSION,
                    ..
                }
            ))
        ));
        reader
    }

    async fn expect_final_checkpoint(
        reader: &mut ProviderStreamReader,
        expected_digest: [u8; DIGEST_LEN],
    ) {
        loop {
            let frame = reader.next_message().await.unwrap();
            let frame = ProviderReadClientFrame::decode(frame.as_slice()).unwrap();
            if let Some(provider_read_client_frame::Frame::Checkpoint(checkpoint)) = frame.frame
                && !checkpoint.final_digest.is_empty()
            {
                assert_eq!(checkpoint.final_digest, expected_digest);
                return;
            }
        }
    }

    async fn send_ready(
        send: &mut iroh::endpoint::SendStream,
        resume_offset: u64,
        attempt_generation: u64,
        remaining_length: u64,
    ) {
        send_server_message(
            send,
            ProviderReadServerFrame {
                frame: Some(provider_read_server_frame::Frame::Ready(
                    ProviderReadReady {
                        resume_offset,
                        attempt_generation,
                        remaining_length,
                    },
                )),
            },
        )
        .await;
    }

    async fn send_complete(
        send: &mut iroh::endpoint::SendStream,
        success: bool,
        committed_length: u64,
    ) {
        send_server_message(
            send,
            ProviderReadServerFrame {
                frame: Some(provider_read_server_frame::Frame::Complete(
                    ProviderReadComplete {
                        success,
                        committed_length,
                        error: String::new(),
                    },
                )),
            },
        )
        .await;
    }

    async fn send_server_message(
        send: &mut iroh::endpoint::SendStream,
        frame: ProviderReadServerFrame,
    ) {
        send.write_all(&encode_stream_message(&frame.encode_to_vec()).unwrap())
            .await
            .unwrap();
    }
}
