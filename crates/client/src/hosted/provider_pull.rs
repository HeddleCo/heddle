use std::{
    collections::HashSet,
    io,
    time::{SystemTime, UNIX_EPOCH},
};

use api::{
    framing::{MAX_CONTROL_BODY, StreamFrame, decode_stream_frame, encode_stream_message},
    heddle::api::v1alpha1::{
        ProviderPlanChallenge, ProviderPlanResponse, ProviderPullCapabilityContext,
        ProviderPullManifest as ApiProviderPullManifest, ProviderPullResponse,
        ProviderPullResultStatus, ProviderReadCheckpoint, ProviderReadClientFrame,
        ProviderReadComplete, ProviderReadReady, ProviderReadRequest, ProviderReadServerFrame,
        ProviderSource, provider_read_client_frame, provider_read_server_frame,
    },
};
use bytes::Bytes;
use objects::store::PackObjectId;
use prost::Message;
use wire::{
    NativePackBundle, ProtocolError, ProviderPackExtent, ProviderPackIndexEntry,
    ProviderPackManifest, assemble_provider_pack,
};

use super::{HostedClient, helpers::hosted_to_protocol_error};

const PROVIDER_VERSION: u32 = 1;
const PLAN_NONCE_LEN: usize = 16;
const DIGEST_LEN: usize = 32;
const PROVIDER_READ_CHUNK: usize = 1024 * 1024;
const MAX_PROVIDER_ATTEMPTS: usize = 3;

#[derive(Debug, Clone)]
pub(super) struct ProviderPullSession {
    stream_id: String,
    repository: String,
    endpoint_id: String,
    plan_nonce: [u8; PLAN_NONCE_LEN],
    provider_enabled: bool,
}

pub(super) struct CompletedProviderPull {
    pub(super) pack: NativePackBundle,
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
    ) -> Result<CompletedProviderPull, ProtocolError> {
        let (wire_manifest, sources) = convert_manifest(session, challenge, manifest)?;
        let mut bodies = Vec::with_capacity(manifest.extents.len());
        for (extent, source) in manifest.extents.iter().zip(sources) {
            let digest: [u8; DIGEST_LEN] = extent.digest.as_slice().try_into().map_err(|_| {
                ProtocolError::InvalidState(
                    "provider extent digest must be exactly 32 bytes".to_string(),
                )
            })?;
            let downloaded = self
                .download_provider_extent(source, extent.length, digest)
                .await?;
            bodies.push(downloaded.bytes);
        }
        let bundle = assemble_provider_pack(&wire_manifest, &bodies)?;
        Ok(CompletedProviderPull {
            pack: bundle.pack,
            trailer_digest: bundle.trailer_digest,
        })
    }

    async fn download_provider_extent(
        &self,
        source: &ProviderSource,
        expected_length: u64,
        expected_digest: [u8; DIGEST_LEN],
    ) -> Result<DownloadedExtent, ProtocolError> {
        let mut retained = Vec::new();
        let mut previous_generation = None;
        let mut last_retryable = None;

        for _ in 0..MAX_PROVIDER_ATTEMPTS {
            let connection = match self.connection.provider_connection(source).await {
                Ok(connection) => connection,
                Err(error) => {
                    last_retryable = Some(hosted_to_protocol_error(error));
                    continue;
                }
            };
            match download_attempt(
                &connection,
                &source.opaque_ticket,
                expected_length,
                expected_digest,
                &mut retained,
                &mut previous_generation,
            )
            .await
            {
                Ok(_) => return Ok(DownloadedExtent { bytes: retained }),
                Err(AttemptFailure::Retryable { error }) => last_retryable = Some(error),
                Err(AttemptFailure::Fatal(error)) => return Err(error),
            }
        }
        Err(last_retryable.unwrap_or_else(|| {
            ProtocolError::InvalidState("provider transfer attempts exhausted".to_string())
        }))
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
}

struct DownloadedExtent {
    bytes: Vec<u8>,
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

fn convert_manifest<'a>(
    session: &ProviderPullSession,
    challenge: &ProviderPlanChallenge,
    manifest: &'a ApiProviderPullManifest,
) -> Result<(ProviderPackManifest, Vec<&'a ProviderSource>), ProtocolError> {
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
        sources.push(source);
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

#[derive(Debug)]
enum AttemptFailure {
    Retryable { error: ProtocolError },
    Fatal(ProtocolError),
}

async fn download_attempt(
    connection: &iroh::endpoint::Connection,
    opaque_ticket: &str,
    expected_length: u64,
    expected_digest: [u8; DIGEST_LEN],
    retained: &mut Vec<u8>,
    previous_generation: &mut Option<u64>,
) -> Result<u64, AttemptFailure> {
    let (mut send, recv) =
        connection
            .open_bi()
            .await
            .map_err(|error| AttemptFailure::Retryable {
                error: transport_error(error),
            })?;
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
    )
    .await
    .map_err(|error| AttemptFailure::Retryable { error })?;

    let mut reader = ProviderStreamReader::new(recv);
    let ready = read_ready(&mut reader)
        .await
        .map_err(AttemptFailure::Fatal)?;
    if ready.resume_offset > expected_length
        || ready.resume_offset > retained.len() as u64
        || ready.remaining_length != expected_length - ready.resume_offset
        || ready.attempt_generation == 0
        || previous_generation.is_some_and(|previous| previous == ready.attempt_generation)
    {
        abort_provider_stream(&mut send, &mut reader);
        return Err(AttemptFailure::Fatal(ProtocolError::InvalidState(
            "provider returned an invalid resume offset or attempt generation".to_string(),
        )));
    }
    *previous_generation = Some(ready.attempt_generation);
    let resume_offset = usize::try_from(ready.resume_offset).map_err(|_| {
        AttemptFailure::Fatal(ProtocolError::InvalidState(
            "provider resume offset exceeds this platform".to_string(),
        ))
    })?;
    retained.truncate(resume_offset);
    let mut hasher = blake3::Hasher::new();
    hasher.update(retained);
    let prefix_rehashed = ready.resume_offset;

    let raw_length = reader
        .next_raw_body()
        .await
        .map_err(|error| AttemptFailure::Retryable { error })?;
    if raw_length != ready.remaining_length {
        abort_provider_stream(&mut send, &mut reader);
        return Err(AttemptFailure::Fatal(ProtocolError::InvalidState(
            "provider raw body length does not match its resume response".to_string(),
        )));
    }

    while let Some(chunk) = reader
        .read_raw_chunk(PROVIDER_READ_CHUNK)
        .await
        .map_err(|error| AttemptFailure::Retryable { error })?
    {
        hasher.update(&chunk);
        retained.extend_from_slice(&chunk);
        send_provider_frame(
            &mut send,
            &ProviderReadClientFrame {
                frame: Some(provider_read_client_frame::Frame::Checkpoint(
                    ProviderReadCheckpoint {
                        attempt_generation: ready.attempt_generation,
                        acknowledged_length: retained.len() as u64,
                        final_digest: Vec::new(),
                    },
                )),
            },
        )
        .await
        .map_err(|error| AttemptFailure::Retryable { error })?;
    }

    if retained.len() as u64 != expected_length || hasher.finalize().as_bytes() != &expected_digest
    {
        abort_provider_stream(&mut send, &mut reader);
        return Err(AttemptFailure::Fatal(ProtocolError::InvalidState(
            "provider extent digest mismatch".to_string(),
        )));
    }
    send_provider_frame(
        &mut send,
        &ProviderReadClientFrame {
            frame: Some(provider_read_client_frame::Frame::Checkpoint(
                ProviderReadCheckpoint {
                    attempt_generation: ready.attempt_generation,
                    acknowledged_length: expected_length,
                    final_digest: expected_digest.to_vec(),
                },
            )),
        },
    )
    .await
    .map_err(|error| AttemptFailure::Retryable { error })?;
    send.finish().map_err(|error| AttemptFailure::Retryable {
        error: transport_error(error),
    })?;
    let complete = read_complete(&mut reader)
        .await
        .map_err(AttemptFailure::Fatal)?;
    if !complete.success || complete.committed_length != expected_length {
        return Err(AttemptFailure::Fatal(ProtocolError::InvalidState(
            "provider did not commit the exact verified extent".to_string(),
        )));
    }
    Ok(prefix_rehashed)
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
                .read_chunk(MAX_CONTROL_BODY + 5)
                .await
                .map_err(transport_error)?
                .ok_or_else(|| {
                    ProtocolError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "provider stream ended before its next frame",
                    ))
                })?;
            self.buffered.extend_from_slice(&chunk);
        }
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
    use std::{net::Ipv4Addr, time::Duration};

    use api::{
        framing::{encode_stream_message, encode_stream_raw_body},
        heddle::api::v1alpha1::{
            ProviderReadServerFrame, provider_read_client_frame, provider_read_server_frame,
        },
        signing,
    };
    use crypto::{Ed25519Signer, Signer as _};
    use iroh::{Endpoint, RelayMode, endpoint::presets};

    use super::*;
    use crate::hosted::CallContextFactory;

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
        let mut retained = Vec::new();
        let mut generation = None;
        let first = download_attempt(
            &first_client_connection,
            "opaque",
            u64::try_from(complete.len()).unwrap(),
            expected_digest,
            &mut retained,
            &mut generation,
        )
        .await;
        assert!(matches!(first, Err(AttemptFailure::Retryable { .. })));
        assert_eq!(retained, complete[..resume_offset]);
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

        let prefix_rehashed = download_attempt(
            &client_connection,
            "opaque",
            u64::try_from(complete.len()).unwrap(),
            expected_digest,
            &mut retained,
            &mut generation,
        )
        .await
        .unwrap();
        assert_eq!(retained, complete);
        assert_eq!(prefix_rehashed, u64::try_from(resume_offset).unwrap());
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

        let mut retained = Vec::new();
        let mut generation = None;
        let result = download_attempt(
            &client_connection,
            "opaque",
            u64::try_from(expected.len()).unwrap(),
            expected_digest,
            &mut retained,
            &mut generation,
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
    fn fallback_response_carries_no_success_digest() {
        let session = ProviderPullSession {
            stream_id: "pull:one".to_string(),
            repository: "acme/widgets".to_string(),
            endpoint_id: "11".repeat(32),
            plan_nonce: [4; PLAN_NONCE_LEN],
            provider_enabled: true,
        };

        let response = session.fallback_response(&[7; DIGEST_LEN]);

        assert_eq!(response.status, ProviderPullResultStatus::Fallback as i32);
        assert!(response.pack_digest.is_empty());
        println!(
            "fallback provider=unavailable selected=existing-weft caller_protocol=unchanged success_digest_present=false"
        );
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
