use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
use objects::store::{AnyStore, PackObjectId};
use prost::Message;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use wire::{
    ProtocolError, ProviderPackExtent, ProviderPackIndexEntry, ProviderPackManifest,
    ProviderPackSpool, ProviderPackWriter,
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
const PROVIDER_FALLBACK_MARGIN: Duration = Duration::from_secs(1);
const MAX_FALLBACK_REASON_LEN: usize = 96;
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
    pub(super) installed_ids: Vec<PackObjectId>,
    pub(super) trailer_digest: [u8; DIGEST_LEN],
    fallback_deadline: tokio::time::Instant,
    signed_expiry_millis: u64,
    evidence: Arc<ProviderPullEvidence>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderFailureStage {
    Expiry,
    Connect,
    Carrier,
    Stall,
    MalformedResume,
    Digest,
    Spool,
    Install,
    Manifest,
}

impl ProviderFailureStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Expiry => "expiry",
            Self::Connect => "connect",
            Self::Carrier => "carrier",
            Self::Stall => "stall",
            Self::MalformedResume => "malformed_resume",
            Self::Digest => "digest",
            Self::Spool => "spool",
            Self::Install => "install",
            Self::Manifest => "manifest",
        }
    }
}

#[derive(Debug)]
struct ProviderPullEvidence {
    endpoint_count: usize,
    extent_count: usize,
    completed_by_extent: Vec<AtomicU64>,
    reconnect_attempts: AtomicUsize,
    started: Instant,
}

impl ProviderPullEvidence {
    fn new(endpoint_count: usize, extent_count: usize) -> Arc<Self> {
        Arc::new(Self {
            endpoint_count,
            extent_count,
            completed_by_extent: (0..extent_count).map(|_| AtomicU64::new(0)).collect(),
            reconnect_attempts: AtomicUsize::new(0),
            started: Instant::now(),
        })
    }

    fn from_manifest(manifest: &ApiProviderPullManifest) -> Arc<Self> {
        let endpoint_count = manifest
            .extents
            .iter()
            .filter_map(|extent| extent.source.as_ref())
            .map(|source| source.endpoint_id.as_str())
            .collect::<HashSet<_>>()
            .len();
        Self::new(endpoint_count, manifest.extents.len())
    }

    fn set_completed(&self, extent_index: usize, completed: u64) {
        if let Some(value) = self.completed_by_extent.get(extent_index) {
            value.store(completed, Ordering::Release);
        }
    }

    fn completed_bytes(&self) -> u64 {
        self.completed_by_extent
            .iter()
            .map(|value| value.load(Ordering::Acquire))
            .sum()
    }

    fn record_reconnect(&self) {
        self.reconnect_attempts.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
pub(super) struct ProviderPullFailure {
    stage: ProviderFailureStage,
    reason: &'static str,
    error: ProtocolError,
    evidence: Arc<ProviderPullEvidence>,
}

pub(super) fn rejected_provider_manifest(
    manifest: &ApiProviderPullManifest,
    error: ProtocolError,
) -> ProviderPullFailure {
    ProviderPullFailure::new(
        ProviderFailureStage::Manifest,
        "provider_manifest_without_consent",
        error,
        ProviderPullEvidence::from_manifest(manifest),
    )
}

impl ProviderPullFailure {
    fn new(
        stage: ProviderFailureStage,
        reason: &'static str,
        error: ProtocolError,
        evidence: Arc<ProviderPullEvidence>,
    ) -> Self {
        debug_assert!(reason.len() <= MAX_FALLBACK_REASON_LEN);
        Self {
            stage,
            reason,
            error,
            evidence,
        }
    }

    fn emit(&self) {
        tracing::warn!(
            stage = self.stage.as_str(),
            reason = self.reason,
            endpoint_count = self.evidence.endpoint_count,
            extent_count = self.evidence.extent_count,
            completed_bytes = self.evidence.completed_bytes(),
            reconnect_attempts = self.evidence.reconnect_attempts.load(Ordering::Acquire),
            elapsed_ms = self.evidence.started.elapsed().as_millis(),
            "provider pull selected ordinary Weft fallback"
        );
    }
}

impl std::fmt::Display for ProviderPullFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "provider pull failed at {}: {}",
            self.stage.as_str(),
            self.reason
        )
    }
}

impl std::error::Error for ProviderPullFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
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
        store: AnyStore,
    ) -> Result<CompletedProviderPull, ProviderPullFailure> {
        let evidence = ProviderPullEvidence::from_manifest(manifest);
        let signed_expiry = minimum_signed_expiry(challenge, manifest).map_err(|error| {
            ProviderPullFailure::new(
                ProviderFailureStage::Manifest,
                "signed_expiry_missing",
                error,
                Arc::clone(&evidence),
            )
        })?;
        let deadline = ProviderPlanDeadline::new(signed_expiry, Arc::clone(&evidence))?;
        let (wire_manifest, sources) =
            convert_manifest(session, challenge, manifest).map_err(|error| {
                ProviderPullFailure::new(
                    ProviderFailureStage::Manifest,
                    "provider_manifest_rejected",
                    error,
                    Arc::clone(&evidence),
                )
            })?;
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
        let operation = async {
            let download = download_provider_plan_with_evidence(
                spool_root,
                wire_manifest,
                extents,
                &backend,
                &self.transport,
                Arc::clone(&evidence),
            )
            .await?;
            finish_and_install_provider_pack(download, store, Arc::clone(&evidence)).await
        };
        let completed = tokio::time::timeout_at(deadline.fallback_deadline, operation)
            .await
            .map_err(|_| {
                ProviderPullFailure::new(
                    ProviderFailureStage::Expiry,
                    "signed_deadline_margin_elapsed",
                    ProtocolError::InvalidState(
                        "provider signed deadline margin elapsed".to_string(),
                    ),
                    Arc::clone(&evidence),
                )
            })??;
        tracing::debug!(
            peak_inflight_bytes = completed.peak_inflight_bytes,
            configured_bytes = self.transport.provider_max_inflight_bytes,
            "provider pull byte-budget high-water mark"
        );
        Ok(CompletedProviderPull {
            installed_ids: completed.installed_ids,
            trailer_digest: completed.trailer_digest,
            fallback_deadline: deadline.fallback_deadline,
            signed_expiry_millis: deadline.signed_expiry_millis,
            evidence,
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
        result: Result<CompletedProviderPull, ProviderPullFailure>,
    ) -> (ProviderPullResponse, Option<CompletedProviderPull>) {
        match result {
            Ok(completed)
                if tokio::time::Instant::now() < completed.fallback_deadline
                    && now_millis().is_ok_and(|now| now < completed.signed_expiry_millis) =>
            {
                let response = self.complete_response(batch_digest, completed.trailer_digest);
                (response, Some(completed))
            }
            Ok(completed) => {
                ProviderPullFailure::new(
                    ProviderFailureStage::Expiry,
                    "complete_missed_signed_deadline_margin",
                    ProtocolError::InvalidState(
                        "provider completion missed signed deadline margin".to_string(),
                    ),
                    Arc::clone(&completed.evidence),
                )
                .emit();
                (self.fallback_response(batch_digest), None)
            }
            Err(error) => {
                error.emit();
                (self.fallback_response(batch_digest), None)
            }
        }
    }
}

#[derive(Clone, Copy)]
struct ProviderPlanDeadline {
    fallback_deadline: tokio::time::Instant,
    signed_expiry_millis: u64,
}

impl ProviderPlanDeadline {
    fn new(
        signed_expiry_millis: u64,
        evidence: Arc<ProviderPullEvidence>,
    ) -> Result<Self, ProviderPullFailure> {
        let now = now_millis().map_err(|error| {
            ProviderPullFailure::new(
                ProviderFailureStage::Expiry,
                "system_deadline_unavailable",
                error,
                Arc::clone(&evidence),
            )
        })?;
        let remaining = signed_expiry_millis.checked_sub(now).ok_or_else(|| {
            ProviderPullFailure::new(
                ProviderFailureStage::Expiry,
                "signed_deadline_expired",
                ProtocolError::InvalidState("provider signed deadline expired".to_string()),
                Arc::clone(&evidence),
            )
        })?;
        let margin_millis = u64::try_from(PROVIDER_FALLBACK_MARGIN.as_millis()).unwrap_or(u64::MAX);
        let provider_window = remaining.checked_sub(margin_millis).ok_or_else(|| {
            ProviderPullFailure::new(
                ProviderFailureStage::Expiry,
                "insufficient_fallback_margin",
                ProtocolError::InvalidState(
                    "provider signed deadline leaves no fallback margin".to_string(),
                ),
                evidence,
            )
        })?;
        Ok(Self {
            fallback_deadline: tokio::time::Instant::now() + Duration::from_millis(provider_window),
            signed_expiry_millis,
        })
    }
}

fn minimum_signed_expiry(
    challenge: &ProviderPlanChallenge,
    manifest: &ApiProviderPullManifest,
) -> Result<u64, ProtocolError> {
    let summary = challenge.summary.as_ref().ok_or_else(|| {
        ProtocolError::InvalidState("provider plan challenge has no safe summary".to_string())
    })?;
    manifest
        .extents
        .iter()
        .try_fold(summary.expires_at_unix_millis, |minimum, extent| {
            let source = extent.source.as_ref().ok_or_else(|| {
                ProtocolError::InvalidState("provider extent has no source".to_string())
            })?;
            Ok(minimum.min(source.expires_at_unix_millis))
        })
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

#[derive(Debug)]
struct ProviderPlanDownload {
    spool: ProviderPackSpool,
    peak_inflight_bytes: usize,
}

struct InstalledProviderPlan {
    installed_ids: Vec<PackObjectId>,
    trailer_digest: [u8; DIGEST_LEN],
    peak_inflight_bytes: usize,
}

#[derive(Clone)]
struct ProviderGroupRuntime {
    writer: ProviderPackWriter,
    connection_limit: Arc<Semaphore>,
    stream_limit: Arc<Semaphore>,
    byte_budget: InflightByteBudget,
    policy: HostedTransportPolicy,
    evidence: Arc<ProviderPullEvidence>,
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
        evidence: Arc<ProviderPullEvidence>,
    ) -> impl Future<Output = Result<(), ProviderPullFailure>> + Send;
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
        evidence: Arc<ProviderPullEvidence>,
    ) -> Result<(), ProviderPullFailure> {
        download_provider_extent(
            self,
            connection,
            extent,
            writer,
            byte_budget,
            stall_timeout,
            evidence,
        )
        .await
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

#[cfg(test)]
async fn download_provider_plan<B: ProviderBackend>(
    spool_root: &Path,
    manifest: ProviderPackManifest,
    extents: Vec<ScheduledProviderExtent>,
    backend: &B,
    policy: &HostedTransportPolicy,
) -> Result<ProviderPlanDownload, ProviderPullFailure> {
    let endpoint_count = extents
        .iter()
        .map(|extent| extent.source.endpoint_id.as_str())
        .collect::<HashSet<_>>()
        .len();
    let evidence = ProviderPullEvidence::new(endpoint_count, extents.len());
    download_provider_plan_with_evidence(spool_root, manifest, extents, backend, policy, evidence)
        .await
}

async fn download_provider_plan_with_evidence<B: ProviderBackend>(
    spool_root: &Path,
    manifest: ProviderPackManifest,
    extents: Vec<ScheduledProviderExtent>,
    backend: &B,
    policy: &HostedTransportPolicy,
    evidence: Arc<ProviderPullEvidence>,
) -> Result<ProviderPlanDownload, ProviderPullFailure> {
    let spool = ProviderPackSpool::new_in(spool_root, manifest).map_err(|error| {
        ProviderPullFailure::new(
            ProviderFailureStage::Spool,
            "spool_create_failed",
            error,
            Arc::clone(&evidence),
        )
    })?;
    let writer = spool.writer();
    let byte_budget = InflightByteBudget::new(policy.provider_max_inflight_bytes);
    let connection_limit = Arc::new(Semaphore::new(policy.provider_global_concurrency));
    let stream_limit = Arc::new(Semaphore::new(policy.provider_global_concurrency));
    let runtime = ProviderGroupRuntime {
        writer: writer.clone(),
        connection_limit,
        stream_limit,
        byte_budget: byte_budget.clone(),
        policy: policy.clone(),
        evidence,
    };
    let mut groups = BTreeMap::<String, Vec<ScheduledProviderExtent>>::new();
    for extent in extents {
        groups
            .entry(extent.source.endpoint_id.clone())
            .or_default()
            .push(extent);
    }

    let mut pending = FuturesUnordered::new();
    for extents in groups.into_values() {
        pending.push(download_provider_group(backend, extents, runtime.clone()));
    }
    while let Some(result) = pending.next().await {
        result?;
    }
    drop(writer);

    let peak_inflight_bytes = byte_budget.peak();
    Ok(ProviderPlanDownload {
        spool,
        peak_inflight_bytes,
    })
}

async fn finish_and_install_provider_pack(
    download: ProviderPlanDownload,
    store: AnyStore,
    evidence: Arc<ProviderPullEvidence>,
) -> Result<InstalledProviderPlan, ProviderPullFailure> {
    let peak_inflight_bytes = download.peak_inflight_bytes;
    let task_evidence = Arc::clone(&evidence);
    tokio::task::spawn_blocking(move || {
        let mut pack = download.spool.finish().map_err(|error| {
            ProviderPullFailure::new(
                ProviderFailureStage::Spool,
                "spool_finalize_failed",
                error,
                Arc::clone(&task_evidence),
            )
        })?;
        let trailer_digest = pack.trailer_digest;
        let installed_ids = pack.install_into(&store).map_err(|error| {
            ProviderPullFailure::new(
                ProviderFailureStage::Install,
                "pack_install_failed",
                error,
                Arc::clone(&task_evidence),
            )
        })?;
        Ok(InstalledProviderPlan {
            installed_ids,
            trailer_digest,
            peak_inflight_bytes,
        })
    })
    .await
    .map_err(|error| {
        ProviderPullFailure::new(
            ProviderFailureStage::Install,
            "pack_install_task_failed",
            ProtocolError::InvalidState(format!("provider install task failed: {error}")),
            evidence,
        )
    })?
}

async fn download_provider_group<B: ProviderBackend>(
    backend: &B,
    extents: Vec<ScheduledProviderExtent>,
    runtime: ProviderGroupRuntime,
) -> Result<(), ProviderPullFailure> {
    let evidence = Arc::clone(&runtime.evidence);
    let _connection_permit = runtime
        .connection_limit
        .acquire_owned()
        .await
        .map_err(|_| {
            ProviderPullFailure::new(
                ProviderFailureStage::Connect,
                "connection_scheduler_closed",
                ProtocolError::InvalidState("provider connection scheduler closed".to_string()),
                Arc::clone(&evidence),
            )
        })?;
    for extent in &extents {
        ensure_grant_unexpired(&extent.source).map_err(|error| {
            ProviderPullFailure::new(
                ProviderFailureStage::Expiry,
                "source_grant_expired",
                error,
                Arc::clone(&evidence),
            )
        })?;
    }
    let sources = extents
        .iter()
        .map(|extent| extent.source.clone())
        .collect::<Vec<_>>();
    let connection = tokio::time::timeout(
        runtime.policy.provider_stall_timeout,
        backend.connect(&sources),
    )
    .await
    .map_err(|_| {
        ProviderPullFailure::new(
            ProviderFailureStage::Stall,
            "provider_connect_stalled",
            provider_stall_error(),
            Arc::clone(&evidence),
        )
    })?
    .map_err(|error| {
        ProviderPullFailure::new(
            ProviderFailureStage::Connect,
            "provider_connect_failed",
            error,
            Arc::clone(&evidence),
        )
    })?;

    let endpoint_limit = Arc::new(Semaphore::new(
        runtime.policy.provider_per_endpoint_concurrency,
    ));
    let mut pending = FuturesUnordered::new();
    for extent in extents {
        let endpoint_limit = Arc::clone(&endpoint_limit);
        let stream_limit = Arc::clone(&runtime.stream_limit);
        let connection = connection.clone();
        let writer = runtime.writer.clone();
        let byte_budget = runtime.byte_budget.clone();
        let evidence = Arc::clone(&evidence);
        let stall_timeout = runtime.policy.provider_stall_timeout;
        pending.push(async move {
            let _endpoint_permit = endpoint_limit.acquire_owned().await.map_err(|_| {
                ProviderPullFailure::new(
                    ProviderFailureStage::Connect,
                    "endpoint_scheduler_closed",
                    ProtocolError::InvalidState("provider endpoint scheduler closed".to_string()),
                    Arc::clone(&evidence),
                )
            })?;
            let _stream_permit = stream_limit.acquire_owned().await.map_err(|_| {
                ProviderPullFailure::new(
                    ProviderFailureStage::Connect,
                    "stream_scheduler_closed",
                    ProtocolError::InvalidState("provider stream scheduler closed".to_string()),
                    Arc::clone(&evidence),
                )
            })?;
            ensure_grant_unexpired(&extent.source).map_err(|error| {
                ProviderPullFailure::new(
                    ProviderFailureStage::Expiry,
                    "source_grant_expired",
                    error,
                    Arc::clone(&evidence),
                )
            })?;
            backend
                .download(
                    connection,
                    extent,
                    writer,
                    byte_budget,
                    stall_timeout,
                    evidence,
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

async fn download_provider_extent<B>(
    backend: &B,
    mut connection: iroh::endpoint::Connection,
    extent: ScheduledProviderExtent,
    writer: ProviderPackWriter,
    byte_budget: InflightByteBudget,
    stall_timeout: std::time::Duration,
    evidence: Arc<ProviderPullEvidence>,
) -> Result<(), ProviderPullFailure>
where
    B: ProviderBackend<Connection = iroh::endpoint::Connection>,
{
    let mut retained_length = 0_u64;
    let mut previous_generation = None;
    let mut last_retryable = None;

    for attempt_index in 0..MAX_PROVIDER_RESUME_ATTEMPTS {
        let mut attempt = ExtentAttemptState {
            expected_length: extent.length,
            expected_digest: extent.digest,
            writer: &writer,
            extent_index: extent.index,
            retained_length: &mut retained_length,
            previous_generation: &mut previous_generation,
            evidence: Arc::clone(&evidence),
        };
        match download_attempt(
            &connection,
            &extent.source.opaque_ticket,
            &mut attempt,
            &byte_budget,
            stall_timeout,
        )
        .await
        {
            Ok(_) => {
                writer.mark_verified(extent.index).map_err(|error| {
                    ProviderPullFailure::new(
                        ProviderFailureStage::Spool,
                        "spool_verify_failed",
                        error,
                        Arc::clone(&evidence),
                    )
                })?;
                return Ok(());
            }
            Err(AttemptFailure::Retryable { error }) => {
                last_retryable = Some(error);
                if attempt_index + 1 == MAX_PROVIDER_RESUME_ATTEMPTS {
                    break;
                }
                evidence.record_reconnect();
                connection = tokio::time::timeout(
                    stall_timeout,
                    backend.connect(std::slice::from_ref(&extent.source)),
                )
                .await
                .map_err(|_| {
                    ProviderPullFailure::new(
                        ProviderFailureStage::Stall,
                        "provider_reconnect_stalled",
                        provider_stall_error(),
                        Arc::clone(&evidence),
                    )
                })?
                .map_err(|error| {
                    ProviderPullFailure::new(
                        ProviderFailureStage::Carrier,
                        "provider_reconnect_failed",
                        error,
                        Arc::clone(&evidence),
                    )
                })?;
            }
            Err(AttemptFailure::Stall(error)) => {
                return Err(ProviderPullFailure::new(
                    ProviderFailureStage::Stall,
                    "provider_progress_stalled",
                    error,
                    evidence,
                ));
            }
            Err(AttemptFailure::Fatal {
                stage,
                reason,
                error,
            }) => {
                return Err(ProviderPullFailure::new(stage, reason, error, evidence));
            }
        }
    }
    Err(ProviderPullFailure::new(
        ProviderFailureStage::Carrier,
        "carrier_resume_attempts_exhausted",
        last_retryable.unwrap_or_else(|| {
            ProtocolError::InvalidState("provider transfer attempts exhausted".to_string())
        }),
        evidence,
    ))
}

#[derive(Debug)]
enum AttemptFailure {
    Retryable {
        error: ProtocolError,
    },
    Stall(ProtocolError),
    Fatal {
        stage: ProviderFailureStage,
        reason: &'static str,
        error: ProtocolError,
    },
}

struct ExtentAttemptState<'a> {
    expected_length: u64,
    expected_digest: [u8; DIGEST_LEN],
    writer: &'a ProviderPackWriter,
    extent_index: usize,
    retained_length: &'a mut u64,
    previous_generation: &'a mut Option<u64>,
    evidence: Arc<ProviderPullEvidence>,
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
        ProviderFailureStage::Carrier,
        "provider_stream_open_failed",
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
        ProviderFailureStage::Carrier,
        "provider_request_write_failed",
    )
    .await?;

    let mut reader = ProviderStreamReader::new(recv);
    let ready = await_provider_progress(
        stall_timeout,
        read_ready(&mut reader),
        ProviderFailureStage::MalformedResume,
        "provider_ready_malformed",
    )
    .await?;
    if ready.resume_offset > state.expected_length
        || ready.resume_offset > *state.retained_length
        || ready.remaining_length != state.expected_length - ready.resume_offset
        || ready.attempt_generation == 0
        || state
            .previous_generation
            .is_some_and(|previous| previous == ready.attempt_generation)
    {
        abort_provider_stream(&mut send, &mut reader);
        return Err(fatal_attempt(
            ProviderFailureStage::MalformedResume,
            "resume_offset_or_generation_invalid",
            ProtocolError::InvalidState(
                "provider returned an invalid resume offset or attempt generation".to_string(),
            ),
        ));
    }
    *state.previous_generation = Some(ready.attempt_generation);
    *state.retained_length = ready.resume_offset;
    let mut hasher = blake3::Hasher::new();
    state
        .writer
        .hash_extent_prefix(state.extent_index, ready.resume_offset, &mut hasher)
        .map_err(|error| {
            fatal_attempt(
                ProviderFailureStage::Spool,
                "spool_prefix_read_failed",
                error,
            )
        })?;
    state
        .evidence
        .set_completed(state.extent_index, ready.resume_offset);
    let prefix_rehashed = ready.resume_offset;

    let raw_length = await_provider_progress(
        stall_timeout,
        reader.next_raw_body(),
        ProviderFailureStage::MalformedResume,
        "provider_body_header_malformed",
    )
    .await?;
    if raw_length != ready.remaining_length {
        abort_provider_stream(&mut send, &mut reader);
        return Err(fatal_attempt(
            ProviderFailureStage::MalformedResume,
            "resume_body_length_invalid",
            ProtocolError::InvalidState(
                "provider raw body length does not match its resume response".to_string(),
            ),
        ));
    }

    while reader.raw_remaining() != 0 {
        let requested = usize::try_from(reader.raw_remaining().min(PROVIDER_READ_CHUNK as u64))
            .unwrap_or(PROVIDER_READ_CHUNK);
        let mut reservation = byte_budget.reserve(requested).await.map_err(|error| {
            fatal_attempt(ProviderFailureStage::Spool, "inflight_budget_failed", error)
        })?;
        let Some(chunk) = await_provider_progress(
            stall_timeout,
            reader.read_raw_chunk(reservation.reserved_bytes),
            ProviderFailureStage::MalformedResume,
            "provider_body_frame_malformed",
        )
        .await?
        else {
            break;
        };
        reservation.record_buffered(chunk.len()).map_err(|error| {
            fatal_attempt(
                ProviderFailureStage::Spool,
                "inflight_accounting_failed",
                error,
            )
        })?;
        hasher.update(&chunk);
        state
            .writer
            .write_extent_chunk(state.extent_index, *state.retained_length, &chunk)
            .map_err(|error| {
                fatal_attempt(ProviderFailureStage::Spool, "spool_write_failed", error)
            })?;
        *state.retained_length = state
            .retained_length
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| {
                fatal_attempt(
                    ProviderFailureStage::Spool,
                    "completed_byte_count_overflow",
                    ProtocolError::InvalidState("provider retained length overflows".to_string()),
                )
            })?;
        state
            .evidence
            .set_completed(state.extent_index, *state.retained_length);
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
            ProviderFailureStage::Carrier,
            "provider_checkpoint_write_failed",
        )
        .await?;
    }

    if *state.retained_length != state.expected_length
        || hasher.finalize().as_bytes() != &state.expected_digest
    {
        abort_provider_stream(&mut send, &mut reader);
        return Err(fatal_attempt(
            ProviderFailureStage::Digest,
            "extent_digest_mismatch",
            ProtocolError::InvalidState("provider extent digest mismatch".to_string()),
        ));
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
        ProviderFailureStage::Carrier,
        "provider_final_checkpoint_write_failed",
    )
    .await?;
    send.finish().map_err(|error| AttemptFailure::Retryable {
        error: transport_error(error),
    })?;
    let complete = await_provider_progress(
        stall_timeout,
        read_complete(&mut reader),
        ProviderFailureStage::MalformedResume,
        "provider_complete_malformed",
    )
    .await?;
    if !complete.success || complete.committed_length != state.expected_length {
        return Err(fatal_attempt(
            ProviderFailureStage::MalformedResume,
            "provider_commit_invalid",
            ProtocolError::InvalidState(
                "provider did not commit the exact verified extent".to_string(),
            ),
        ));
    }
    Ok(prefix_rehashed)
}

async fn await_provider_progress<T>(
    stall_timeout: std::time::Duration,
    future: impl Future<Output = Result<T, ProtocolError>>,
    stage: ProviderFailureStage,
    reason: &'static str,
) -> Result<T, AttemptFailure> {
    match tokio::time::timeout(stall_timeout, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error @ ProtocolError::Io(_))) => Err(AttemptFailure::Retryable { error }),
        Ok(Err(error)) => Err(fatal_attempt(stage, reason, error)),
        Err(_) => Err(AttemptFailure::Stall(provider_stall_error())),
    }
}

fn fatal_attempt(
    stage: ProviderFailureStage,
    reason: &'static str,
    error: ProtocolError,
) -> AttemptFailure {
    AttemptFailure::Fatal {
        stage,
        reason,
        error,
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
    use crate::hosted_runtime::hosted::CallContextFactory;

    #[derive(Clone)]
    struct FakeProviderConnection {
        endpoint_id: String,
    }

    struct ReconnectingIrohBackend {
        endpoint: Endpoint,
        provider_addr: iroh::EndpointAddr,
        connects: AtomicUsize,
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
        chunk_delay: Option<Duration>,
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
                chunk_delay: None,
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
            evidence: Arc<ProviderPullEvidence>,
        ) -> Result<(), ProviderPullFailure> {
            if connection.endpoint_id != extent.source.endpoint_id {
                return Err(ProviderPullFailure::new(
                    ProviderFailureStage::Connect,
                    "fake_connection_crossed_group",
                    ProtocolError::InvalidState(
                        "fake provider connection crossed endpoint groups".to_string(),
                    ),
                    evidence,
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
                    .map_err(|_| {
                        ProviderPullFailure::new(
                            ProviderFailureStage::Stall,
                            "provider_progress_stalled",
                            provider_stall_error(),
                            Arc::clone(&evidence),
                        )
                    })?;
            }
            if let Some(delay) = self.delays.get(&ticket) {
                tokio::time::sleep(*delay).await;
            }
            let body = Arc::clone(self.bodies.get(&ticket).ok_or_else(|| {
                ProviderPullFailure::new(
                    ProviderFailureStage::Manifest,
                    "fake_provider_body_missing",
                    ProtocolError::InvalidState("fake provider body is missing".to_string()),
                    Arc::clone(&evidence),
                )
            })?);
            let mut hasher = blake3::Hasher::new();
            let mut offset = 0_usize;
            while offset < body.len() {
                let length = (body.len() - offset).min(self.chunk_size);
                let mut reservation = byte_budget.reserve(length).await.map_err(|error| {
                    ProviderPullFailure::new(
                        ProviderFailureStage::Spool,
                        "inflight_budget_failed",
                        error,
                        Arc::clone(&evidence),
                    )
                })?;
                let chunk = Bytes::copy_from_slice(&body[offset..offset + length]);
                reservation.record_buffered(chunk.len()).map_err(|error| {
                    ProviderPullFailure::new(
                        ProviderFailureStage::Spool,
                        "inflight_accounting_failed",
                        error,
                        Arc::clone(&evidence),
                    )
                })?;
                if let Some(delay) = self.chunk_delay {
                    tokio::time::sleep(delay).await;
                } else {
                    tokio::task::yield_now().await;
                }
                hasher.update(&chunk);
                writer
                    .write_extent_chunk(extent.index, offset as u64, &chunk)
                    .map_err(|error| {
                        ProviderPullFailure::new(
                            ProviderFailureStage::Spool,
                            "spool_write_failed",
                            error,
                            Arc::clone(&evidence),
                        )
                    })?;
                offset += length;
                evidence.set_completed(extent.index, offset as u64);
                drop(reservation);
            }
            if offset as u64 != extent.length || hasher.finalize().as_bytes() != &extent.digest {
                return Err(ProviderPullFailure::new(
                    ProviderFailureStage::Digest,
                    "extent_digest_mismatch",
                    ProtocolError::InvalidState(
                        "fake provider extent length or digest mismatch".to_string(),
                    ),
                    evidence,
                ));
            }
            writer.mark_verified(extent.index).map_err(|error| {
                ProviderPullFailure::new(
                    ProviderFailureStage::Spool,
                    "spool_verify_failed",
                    error,
                    Arc::clone(&evidence),
                )
            })?;
            self.completion_order.lock().unwrap().push(ticket);
            guard.completed = true;
            Ok(())
        }
    }

    impl ProviderBackend for ReconnectingIrohBackend {
        type Connection = iroh::endpoint::Connection;

        async fn connect(
            &self,
            sources: &[ProviderSource],
        ) -> Result<Self::Connection, ProtocolError> {
            let source = sources.first().ok_or_else(|| {
                ProtocolError::InvalidState("provider reconnect source is missing".to_string())
            })?;
            if source.endpoint_id != self.provider_addr.id.to_string() {
                return Err(ProtocolError::InvalidState(
                    "provider reconnect changed cryptographic endpoint identity".to_string(),
                ));
            }
            self.connects.fetch_add(1, Ordering::AcqRel);
            self.endpoint
                .connect(self.provider_addr.clone(), api::PROVIDER_ALPN_V1)
                .await
                .map_err(transport_error)
        }

        async fn download(
            &self,
            connection: Self::Connection,
            extent: ScheduledProviderExtent,
            writer: ProviderPackWriter,
            byte_budget: InflightByteBudget,
            stall_timeout: Duration,
            evidence: Arc<ProviderPullEvidence>,
        ) -> Result<(), ProviderPullFailure> {
            download_provider_extent(
                self,
                connection,
                extent,
                writer,
                byte_budget,
                stall_timeout,
                evidence,
            )
            .await
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
            .map(|(index, id)| {
                (
                    parsed_index.find(&id).unwrap().expect("fixture id indexed"),
                    index,
                    id,
                )
            })
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

    fn assert_download_falls_back(result: Result<ProviderPlanDownload, ProviderPullFailure>) {
        let error = result.expect_err("provider download must fail");
        let (response, completed) =
            provider_session().resolve_download(&[7; DIGEST_LEN], Err(error));
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

        let download = download_provider_plan(
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
        let mut pack = download.spool.finish().unwrap();
        assert_eq!(
            pack.trailer_digest.as_slice(),
            &fixture.source_pack[fixture.source_pack.len() - DIGEST_LEN..]
        );
        let store_root = tempfile::tempdir().unwrap();
        let store = FsStore::new(store_root.path().join(".heddle"));
        let installed = pack.install_into(&store).unwrap();
        assert_eq!(
            installed.len(),
            PackIndex::from_bytes(&fixture.source_index)
                .unwrap()
                .ids()
                .unwrap()
                .len()
        );
        println!(
            "provider_out_of_order completion=endpoint-b,endpoint-a pack_bytes={} byte_identical=true",
            fixture.source_pack.len()
        );
    }

    #[tokio::test]
    async fn install_failure_sends_fallback_and_never_complete() {
        let mut fixture = provider_fixture(&[64 * 1024], &["endpoint-a"]);
        fixture.manifest.extents[0].objects[0].id =
            PackObjectId::Hash(Blob::new(b"wrong object identity".to_vec()).hash());
        let backend = FakeProviderBackend::new(fixture.bodies);
        let root = tempfile::tempdir().unwrap();
        let evidence = ProviderPullEvidence::new(1, 1);
        let download = download_provider_plan_with_evidence(
            root.path(),
            fixture.manifest,
            fixture.extents,
            &backend,
            &provider_policy(),
            Arc::clone(&evidence),
        )
        .await
        .unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let result = finish_and_install_provider_pack(
            download,
            AnyStore::Fs(FsStore::new(store_root.path().join(".heddle"))),
            evidence,
        )
        .await;
        assert!(matches!(
            &result,
            Err(ProviderPullFailure {
                stage: ProviderFailureStage::Install,
                ..
            })
        ));
        let (response, completed) = provider_session().resolve_download(
            &[7; DIGEST_LEN],
            result.map(|installed| CompletedProviderPull {
                installed_ids: installed.installed_ids,
                trailer_digest: installed.trailer_digest,
                fallback_deadline: tokio::time::Instant::now() + Duration::from_secs(1),
                signed_expiry_millis: now_millis().unwrap() + 2_000,
                evidence: ProviderPullEvidence::new(1, 1),
            }),
        );

        assert_eq!(response.status, ProviderPullResultStatus::Fallback as i32);
        assert!(response.pack_digest.is_empty());
        assert!(completed.is_none());
        println!(
            "provider_install_failure install=forced-error fallback=existing-weft complete_sent=false"
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
        async fn measure(payload_size: usize, byte_budget: usize) -> (usize, usize) {
            let quarter = payload_size / 4;
            let fixture = provider_fixture(
                &[quarter, quarter, quarter, quarter],
                &["endpoint-a", "endpoint-b", "endpoint-c", "endpoint-d"],
            );
            let pack_size = fixture.source_pack.len();
            let mut backend = FakeProviderBackend::new(fixture.bodies);
            backend.chunk_size = 256 * 1024;
            let root = tempfile::tempdir().unwrap();
            let mut policy = provider_policy();
            policy.provider_max_inflight_bytes = byte_budget;
            let download = download_provider_plan(
                root.path(),
                fixture.manifest,
                fixture.extents,
                &backend,
                &policy,
            )
            .await
            .unwrap();
            (pack_size, download.peak_inflight_bytes)
        }

        let configured_budget = 512 * 1024;
        let (small_pack, small_peak) = measure(1024 * 1024, configured_budget).await;
        let (large_pack, large_peak) = measure(16 * 1024 * 1024, configured_budget).await;
        let (_, unbounded_peak) = measure(16 * 1024 * 1024, 64 * 1024 * 1024).await;

        assert!(large_pack >= small_pack * 10);
        assert!(small_peak <= 512 * 1024);
        assert!(large_peak <= 512 * 1024);
        assert_eq!(small_peak, large_peak);
        assert!(
            unbounded_peak > configured_budget,
            "negative control must detect concurrent buffering above the production budget"
        );
        println!(
            "provider_memory method=production-live-body-buffer-high-water small_pack={small_pack} small_peak={small_peak} large_pack={large_pack} large_peak={large_peak} bounded=true negative_control_peak={unbounded_peak} unbounded_detected=true"
        );
    }

    #[tokio::test]
    async fn stalled_lane_does_not_block_healthy_lane_before_fallback_threshold() {
        let fixture = provider_fixture(&[128 * 1024, 128 * 1024], &["endpoint-a", "endpoint-b"]);
        let mut backend = FakeProviderBackend::new(fixture.bodies);
        backend.stalled.insert("extent-0".to_string());
        let root = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();

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
        let elapsed = started.elapsed();

        assert_eq!(
            backend.completion_order.lock().unwrap().as_slice(),
            ["extent-1"]
        );
        assert!(elapsed >= Duration::from_millis(175));
        assert!(elapsed < Duration::from_secs(2));
        assert_download_falls_back(result);
        println!(
            "provider_slow_lane healthy_completed=true fallback_after_ms={} threshold_ms=200",
            elapsed.as_millis()
        );
    }

    #[tokio::test]
    async fn continuous_sub_stall_progress_falls_back_before_signed_expiry() {
        let mut fixture = provider_fixture(&[512 * 1024], &["endpoint-a"]);
        let signed_expiry = now_millis().unwrap() + 1_600;
        fixture.extents[0].source.expires_at_unix_millis = signed_expiry;
        let mut backend = FakeProviderBackend::new(fixture.bodies);
        backend.chunk_size = 4 * 1024;
        backend.chunk_delay = Some(Duration::from_millis(20));
        let root = tempfile::tempdir().unwrap();
        let evidence = ProviderPullEvidence::new(1, 1);
        let deadline = ProviderPlanDeadline::new(signed_expiry, Arc::clone(&evidence)).unwrap();
        let started = Instant::now();
        let result = tokio::time::timeout_at(
            deadline.fallback_deadline,
            download_provider_plan_with_evidence(
                root.path(),
                fixture.manifest,
                fixture.extents,
                &backend,
                &provider_policy(),
                Arc::clone(&evidence),
            ),
        )
        .await
        .map_err(|_| {
            ProviderPullFailure::new(
                ProviderFailureStage::Expiry,
                "signed_deadline_margin_elapsed",
                ProtocolError::InvalidState("provider signed deadline margin elapsed".to_string()),
                Arc::clone(&evidence),
            )
        })
        .and_then(|result| result);
        let elapsed = started.elapsed();
        let session = provider_session();
        let (response, completed) = session.resolve_download(
            &[7; DIGEST_LEN],
            result.map(|download| {
                let pack = download.spool.finish().unwrap();
                CompletedProviderPull {
                    installed_ids: Vec::new(),
                    trailer_digest: pack.trailer_digest,
                    fallback_deadline: deadline.fallback_deadline,
                    signed_expiry_millis: deadline.signed_expiry_millis,
                    evidence: Arc::clone(&evidence),
                }
            }),
        );

        assert_eq!(response.status, ProviderPullResultStatus::Fallback as i32);
        assert!(response.pack_digest.is_empty());
        assert_eq!(response.plan_nonce, session.plan_nonce);
        assert!(completed.is_none());
        assert!(now_millis().unwrap() < signed_expiry);
        assert!(evidence.completed_bytes() > 0);
        assert_eq!(backend.active_streams.load(Ordering::Acquire), 0);
        assert!(
            fs::read_dir(root.path().join("transfer-spool"))
                .unwrap()
                .next()
                .is_none()
        );
        println!(
            "provider_absolute_deadline continuous_progress=true chunk_ms=20 stall_ms=100 fallback_after_ms={} before_expiry=true complete_sent=false logical_pulls=1",
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
    async fn carrier_loss_reconnects_same_endpoint_and_resumes_without_reauthorization() {
        let server = Endpoint::builder(presets::Minimal)
            .alpns(vec![api::PROVIDER_ALPN_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let provider_addr = server.addr();
        let provider_id = server.id().to_string();
        let client = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let mut fixture = provider_fixture(&[128 * 1024], &["placeholder"]);
        fixture.extents[0].source.endpoint_id = provider_id.clone();
        let extent_body = Arc::clone(&fixture.bodies["extent-0"]);
        let resume_offset = 32 * 1024;
        let expected_digest = fixture.extents[0].digest;
        let expected_pack = fixture.source_pack.clone();
        let (server_release, wait_for_release) = tokio::sync::oneshot::channel();
        let server_task = tokio::spawn(async move {
            let first_connection = server.accept().await.unwrap().await.unwrap();
            assert_eq!(first_connection.remote_id().to_string().len(), 64);
            let (mut first_send, first_recv) = first_connection.accept_bi().await.unwrap();
            let mut first_reader = expect_read_request(first_recv).await;
            send_ready(
                &mut first_send,
                0,
                1,
                u64::try_from(extent_body.len()).unwrap(),
            )
            .await;
            first_send
                .write_all(
                    &encode_stream_raw_body(u64::try_from(extent_body.len()).unwrap()).unwrap(),
                )
                .await
                .unwrap();
            first_send
                .write_all(&extent_body[..resume_offset])
                .await
                .unwrap();
            loop {
                let first_checkpoint = first_reader.next_message().await.unwrap();
                let first_checkpoint =
                    ProviderReadClientFrame::decode(first_checkpoint.as_slice()).unwrap();
                if matches!(
                    first_checkpoint.frame,
                    Some(provider_read_client_frame::Frame::Checkpoint(
                        ProviderReadCheckpoint {
                            acknowledged_length,
                            ..
                        }
                    )) if acknowledged_length == resume_offset as u64
                ) {
                    break;
                }
            }
            first_connection.close(1_u32.into(), b"simulated carrier migration");
            first_connection.closed().await;

            let second_connection = server.accept().await.unwrap().await.unwrap();
            let (mut second_send, second_recv) = second_connection.accept_bi().await.unwrap();
            let mut second_reader = expect_read_request(second_recv).await;
            send_ready(
                &mut second_send,
                resume_offset as u64,
                2,
                u64::try_from(extent_body.len() - resume_offset).unwrap(),
            )
            .await;
            second_send
                .write_all(
                    &encode_stream_raw_body(
                        u64::try_from(extent_body.len() - resume_offset).unwrap(),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            second_send
                .write_all(&extent_body[resume_offset..])
                .await
                .unwrap();
            expect_final_checkpoint(&mut second_reader, expected_digest).await;
            send_complete(
                &mut second_send,
                true,
                u64::try_from(extent_body.len()).unwrap(),
            )
            .await;
            second_send.finish().unwrap();
            let _ = wait_for_release.await;
            drop(second_connection);
            server.close().await;
            resume_offset
        });
        let backend = ReconnectingIrohBackend {
            endpoint: client.clone(),
            provider_addr,
            connects: AtomicUsize::new(0),
        };
        let root = tempfile::tempdir().unwrap();
        let evidence = ProviderPullEvidence::new(1, 1);
        let mut policy = provider_policy();
        policy.provider_stall_timeout = Duration::from_secs(1);
        let download = download_provider_plan_with_evidence(
            root.path(),
            fixture.manifest,
            fixture.extents,
            &backend,
            &policy,
            Arc::clone(&evidence),
        )
        .await
        .unwrap();
        let pack = download.spool.finish().unwrap();

        assert_eq!(
            pack.trailer_digest.as_slice(),
            &expected_pack[expected_pack.len() - DIGEST_LEN..]
        );
        assert_eq!(backend.connects.load(Ordering::Acquire), 2);
        assert_eq!(evidence.reconnect_attempts.load(Ordering::Acquire), 1);
        server_release.send(()).unwrap();
        let committed_offset = tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(committed_offset, resume_offset);
        client.close().await;
        println!(
            "provider_carrier_resume endpoint_id={} carrier_connections=2 reconnect_attempts=1 resume_offset={} generation=2 bytes_identical=true authorization_signatures=1",
            provider_id, resume_offset
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
            evidence: ProviderPullEvidence::new(1, 1),
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
            evidence: ProviderPullEvidence::new(1, 1),
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
            evidence: ProviderPullEvidence::new(1, 1),
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
            Err(AttemptFailure::Fatal {
                stage: ProviderFailureStage::Digest,
                error: ProtocolError::InvalidState(message),
                ..
            })
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

    #[test]
    fn fallback_evidence_is_structured_bounded_and_never_exposes_provider_input() {
        let evidence = ProviderPullEvidence::new(2, 3);
        evidence.set_completed(0, 4096);
        evidence.set_completed(1, 8192);
        evidence.record_reconnect();
        let hostile = format!(
            "opaque-ticket=super-secret\nforged-field={} ",
            "x".repeat(8 * 1024)
        );
        let failure = ProviderPullFailure::new(
            ProviderFailureStage::Carrier,
            "carrier_resume_attempts_exhausted",
            ProtocolError::Remote(hostile.clone()),
            Arc::clone(&evidence),
        );
        let rendered = failure.to_string();

        assert_eq!(failure.stage.as_str(), "carrier");
        assert!(failure.reason.len() <= MAX_FALLBACK_REASON_LEN);
        assert!(!rendered.contains("super-secret"));
        assert!(!rendered.contains('\n'));
        assert_eq!(evidence.endpoint_count, 2);
        assert_eq!(evidence.extent_count, 3);
        assert_eq!(evidence.completed_bytes(), 12_288);
        assert_eq!(evidence.reconnect_attempts.load(Ordering::Acquire), 1);
        failure.emit();
        println!(
            "provider_fallback_evidence stage=carrier reason_len={} endpoints=2 extents=3 completed_bytes=12288 reconnect_attempts=1 secret_present=false bounded=true",
            failure.reason.len()
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
