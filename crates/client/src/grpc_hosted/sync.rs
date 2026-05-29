use objects::store::ObjectStore;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use grpc::heddle::v1::{
    GetBlobRequest, ListRefsRequest, ObjectAvailabilityStatus, ObjectDescriptor, PackChunk,
    PackStreamKind, PartialFetchStatus, PullMessage, PullRequest, PushMessage, PushRequest,
    RedactionTransfer, ThreadConfidenceSummary, ThreadIntegrationPolicy, ThreadMetadata,
    ThreadVerificationSummary, TransportMode, UpdateRefRequest, WantObjects, pull_message,
    push_message,
};
use objects::{
    object::{ChangeId, ContentHash, MarkerName, ThreadName},
    store::PackObjectId,
};
use proto::{ObjectType, ProtocolError, PullComplete, PushComplete, RefEntry, RefUpdated};
use repo::{Repository, SyncedThreadMetadata, ThreadManager};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;

use super::{
    HostedGrpcClient, PullMaterialization,
    helpers::{
        descriptor_id, object_descriptor_with_status, parse_descriptor_to_info,
        status_to_protocol_error, to_proto_object_info, transport_mode_name,
    },
};

#[derive(Clone, Copy)]
struct PullOptions<'a> {
    local_thread: Option<&'a str>,
    depth: Option<u32>,
    target_state: Option<ChangeId>,
    materialization: PullMaterialization,
}

struct PullWantPlan {
    wants: Vec<ObjectDescriptor>,
    wanted_types: HashMap<PackObjectId, ObjectType>,
    want_full_closure: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PullObjectMix {
    pub blobs: usize,
    pub trees: usize,
    pub states: usize,
    pub actions: usize,
    pub redactions: usize,
}

impl PullObjectMix {
    fn record(&mut self, obj_type: ObjectType) {
        match obj_type {
            ObjectType::Blob => self.blobs += 1,
            ObjectType::Tree => self.trees += 1,
            ObjectType::State => self.states += 1,
            ObjectType::Action => self.actions += 1,
            ObjectType::Redaction => self.redactions += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.blobs + self.trees + self.states + self.actions + self.redactions
    }
}

#[derive(Debug, Clone, Default)]
pub struct PullProfile {
    pub ready_wait: Duration,
    pub receive_and_apply: Duration,
    pub decode: Duration,
    pub store_receive_object: Duration,
    pub metadata_sync: Duration,
    pub pack_decode_apply: Duration,
    pub raw_decode_apply: Duration,
    pub pack_decode: Duration,
    pub raw_decode: Duration,
    pub bytes_received: usize,
    pub pack_bytes_received: usize,
    pub raw_bytes_received: usize,
    pub objects_received: usize,
    pub object_mix: PullObjectMix,
}

impl HostedGrpcClient {
    pub async fn list_refs(&mut self, repo_path: &str) -> Result<Vec<RefEntry>, ProtocolError> {
        let mut request = Request::new(ListRefsRequest {
            repo_path: repo_path.to_string(),
        });
        self.apply_auth(&mut request)?;
        let response = self
            .inner
            .list_refs(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        response
            .refs
            .into_iter()
            .map(|entry| {
                Ok(RefEntry {
                    name: entry.name,
                    change_id: ChangeId::try_from_slice(&entry.change_id)
                        .map_err(|err| ProtocolError::InvalidState(err.to_string()))?,
                    is_thread: entry.is_thread,
                })
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_ref(
        &mut self,
        repo_path: &str,
        name: &str,
        is_thread: bool,
        old_value: Option<ChangeId>,
        new_value: ChangeId,
        force: bool,
        thread_metadata: Option<&SyncedThreadMetadata>,
    ) -> Result<RefUpdated, ProtocolError> {
        let mut request = Request::new(UpdateRefRequest {
            repo_path: repo_path.to_string(),
            name: name.to_string(),
            is_thread,
            force,
            old_value: old_value
                .map(|value| value.to_string_full())
                .unwrap_or_default(),
            new_value: new_value.to_string_full(),
            thread_metadata: thread_metadata.map(to_proto_thread_metadata),
            client_operation_id: String::new(),
        });
        self.apply_auth(&mut request)?;
        let response = self
            .inner
            .update_ref(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();
        Ok(RefUpdated {
            success: response.success,
            old_value: if response.old_value.is_empty() {
                None
            } else {
                Some(
                    ChangeId::parse(&response.old_value)
                        .map_err(|err| ProtocolError::InvalidState(err.to_string()))?,
                )
            },
            error: (!response.error.is_empty()).then_some(response.error),
        })
    }

    pub async fn push(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        local_state: ChangeId,
        target_thread: &str,
        force: bool,
    ) -> Result<PushComplete, ProtocolError> {
        let _ = self.transport.chunk_size;
        let _ = self.transport.resume_attempts;
        let _ = self.transport.negotiated.chunk_size();
        let objects = proto::enumerate_state_closure(repo.store(), local_state)?;
        let transfer_id = push_transfer_id(repo_path, local_state, target_thread);
        let transport_mode = preferred_transport_mode(&self.transport, objects.len());
        let thread_metadata = load_thread_metadata(repo, target_thread, local_state)?;
        let request_message = PushMessage {
            body: Some(push_message::Body::Request(PushRequest {
                repo_path: repo_path.to_string(),
                local_state: local_state.to_string_full(),
                target_thread: target_thread.to_string(),
                create_thread: true,
                force,
                objects: objects.iter().map(to_proto_object_info).collect(),
                transfer: Some(self.transport.transfer_checkpoint_with_mode(
                    transfer_id.clone(),
                    transport_mode,
                    0,
                    0,
                    false,
                )),
                partial_fetch_status: partial_fetch_status_for_repo(repo),
                allow_partial_fetch: true,
                thread_metadata: thread_metadata
                    .map(|metadata| to_proto_thread_metadata(&metadata)),
                client_operation_id: String::new(),
            })),
        };

        let (tx, rx) = mpsc::channel(self.transport.max_inflight_objects.max(4));
        tx.send(request_message).await.map_err(|_| {
            ProtocolError::InvalidState("failed to initialize push stream".to_string())
        })?;
        let mut request = Request::new(ReceiverStream::new(rx));
        self.apply_auth(&mut request)?;
        let mut response = self
            .inner
            .push(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();

        let ready = match response.message().await.map_err(status_to_protocol_error)? {
            Some(PushMessage {
                body: Some(push_message::Body::Ready(ready)),
            }) => ready,
            _ => {
                return Err(ProtocolError::InvalidState(
                    "expected PushReady from gRPC server".to_string(),
                ));
            }
        };

        let object_index = objects
            .into_iter()
            .map(|info| (descriptor_id(&to_proto_object_info(&info)), info))
            .collect::<HashMap<_, _>>();

        let ready_transport_mode = ready
            .transfer
            .as_ref()
            .and_then(|transfer| TransportMode::try_from(transfer.transport_mode).ok())
            .unwrap_or(transport_mode);
        let wanted_infos = ready
            .want_objects
            .into_iter()
            .map(|want| {
                object_index
                    .get(&descriptor_id(&want))
                    .cloned()
                    .ok_or_else(|| {
                        ProtocolError::InvalidState("server requested unknown object".to_string())
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Split want_objects: blob/tree/state/action → native pack;
        // redactions → out-of-pack `RedactionTransfer` channel (the
        // sidecar lives outside `.heddle/objects/` so GC can't reach
        // it and it can't ride the pack — `build_native_pack` already
        // skips Redaction entries on the sender side).
        let (wanted_redactions, wanted_packable): (Vec<_>, Vec<_>) = wanted_infos
            .into_iter()
            .partition(|info| info.obj_type == proto::ObjectType::Redaction);

        if !wanted_packable.is_empty() {
            let bundle = proto::build_native_pack(repo.store(), &wanted_packable)?;
            for message in encode_native_pack_messages(
                &bundle,
                &transfer_id,
                self.transport.chunk_size.max(1),
                &self.transport,
                ready_transport_mode,
            )? {
                tx.send(message).await.map_err(|_| {
                    ProtocolError::InvalidState("push stream closed unexpectedly".to_string())
                })?;
            }
        }

        for info in wanted_redactions {
            let proto::ObjectId::Hash(blob) = info.id else {
                return Err(ProtocolError::InvalidState(
                    "wanted Redaction must be keyed by ObjectId::Hash(content_hash)".to_string(),
                ));
            };
            // Sender-side: load the byte-identical sidecar payload
            // that `Repository::put_redaction` wrote to disk. The
            // receiver verifies the signature + trust list and then
            // persists these bytes verbatim.
            let bytes = repo
                .store()
                .get_redactions_bytes_for_blob(&blob)
                .map_err(|err| {
                    ProtocolError::InvalidState(format!(
                        "load redactions sidecar for {}: {err}",
                        blob.to_hex()
                    ))
                })?
                .ok_or_else(|| {
                    ProtocolError::InvalidState(format!(
                        "server wants redaction for blob {} but sender has no sidecar",
                        blob.to_hex()
                    ))
                })?;
            let message = PushMessage {
                body: Some(push_message::Body::Redaction(RedactionTransfer {
                    blob_hash: blob.to_hex(),
                    redactions_blob: bytes,
                })),
            };
            tx.send(message).await.map_err(|_| {
                ProtocolError::InvalidState("push stream closed unexpectedly".to_string())
            })?;
        }
        drop(tx);

        let result = match response.message().await.map_err(status_to_protocol_error)? {
            Some(PushMessage {
                body: Some(push_message::Body::Complete(complete)),
            }) => PushComplete {
                success: complete.success,
                new_state: if complete.new_state.is_empty() {
                    None
                } else {
                    Some(
                        ChangeId::parse(&complete.new_state)
                            .map_err(|err| ProtocolError::InvalidState(err.to_string()))?,
                    )
                },
                error: (!complete.error.is_empty()).then_some(complete.error),
                transfer_id: complete
                    .transfer
                    .as_ref()
                    .map(|transfer| transfer.transfer_id.clone())
                    .unwrap_or_default(),
                transport_mode: complete
                    .transfer
                    .as_ref()
                    .map(|transfer| transport_mode_name(transfer.transport_mode))
                    .unwrap_or("raw-objects")
                    .to_string(),
                resume_offset: complete
                    .transfer
                    .as_ref()
                    .map(|transfer| transfer.resume_offset)
                    .unwrap_or_default(),
                chunk_index: complete
                    .transfer
                    .as_ref()
                    .map(|transfer| transfer.chunk_index)
                    .unwrap_or_default(),
                checkpoint: complete
                    .transfer
                    .as_ref()
                    .map(|transfer| transfer.checkpoint.clone())
                    .unwrap_or_default(),
                is_complete: complete
                    .transfer
                    .as_ref()
                    .map(|transfer| transfer.is_complete)
                    .unwrap_or(false),
            },
            _ => {
                return Err(ProtocolError::InvalidState(
                    "expected PushComplete from gRPC server".to_string(),
                ));
            }
        };

        if result.success {
            self.sync_remote_markers(repo, repo_path, local_state)
                .await?;
        }
        Ok(result)
    }

    pub async fn pull(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        local_thread: Option<&str>,
    ) -> Result<PullComplete, ProtocolError> {
        self.pull_with_options(
            repo,
            repo_path,
            remote_thread,
            PullOptions {
                local_thread,
                depth: None,
                target_state: None,
                materialization: PullMaterialization::Full,
            },
        )
        .await
    }

    pub async fn pull_profiled(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        local_thread: Option<&str>,
    ) -> Result<(PullComplete, PullProfile), ProtocolError> {
        self.pull_exchange(
            repo,
            repo_path,
            remote_thread,
            PullOptions {
                local_thread,
                depth: None,
                target_state: None,
                materialization: PullMaterialization::Full,
            },
        )
        .await
        .map(|exchange| (exchange.result, exchange.profile))
    }

    pub async fn pull_partial(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        local_thread: Option<&str>,
    ) -> Result<PullComplete, ProtocolError> {
        self.pull_with_options(
            repo,
            repo_path,
            remote_thread,
            PullOptions {
                local_thread,
                depth: None,
                target_state: None,
                materialization: PullMaterialization::Lazy,
            },
        )
        .await
    }

    pub async fn pull_with_depth_and_materialization(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        local_thread: Option<&str>,
        depth: Option<u32>,
        materialization: PullMaterialization,
    ) -> Result<PullComplete, ProtocolError> {
        self.pull_with_options(
            repo,
            repo_path,
            remote_thread,
            PullOptions {
                local_thread,
                depth,
                target_state: None,
                materialization,
            },
        )
        .await
    }

    pub async fn pull_with_depth(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        local_thread: Option<&str>,
        depth: Option<u32>,
    ) -> Result<PullComplete, ProtocolError> {
        self.pull_with_depth_and_materialization(
            repo,
            repo_path,
            remote_thread,
            local_thread,
            depth,
            PullMaterialization::Full,
        )
        .await
    }

    pub async fn fetch_state(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        target_state: ChangeId,
    ) -> Result<usize, ProtocolError> {
        self.pull_exchange(
            repo,
            repo_path,
            remote_thread,
            PullOptions {
                local_thread: None,
                depth: None,
                target_state: Some(target_state),
                materialization: PullMaterialization::Full,
            },
        )
        .await
        .map(|exchange| exchange.object_count)
    }

    pub async fn fetch_state_partial(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        target_state: ChangeId,
    ) -> Result<usize, ProtocolError> {
        self.pull_exchange(
            repo,
            repo_path,
            remote_thread,
            PullOptions {
                local_thread: None,
                depth: None,
                target_state: Some(target_state),
                materialization: PullMaterialization::Lazy,
            },
        )
        .await
        .map(|exchange| exchange.object_count)
    }

    pub async fn hydrate_blob_at_path(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        reference: &str,
        path: &str,
    ) -> Result<objects::object::Blob, ProtocolError> {
        let mut request = Request::new(GetBlobRequest {
            repo_path: repo_path.to_string(),
            r#ref: reference.to_string(),
            path: path.to_string(),
        });
        self.apply_auth(&mut request)?;
        let response = self
            .content
            .get_blob(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();

        let content = super::helpers::decode_blob_content(response.content, response.is_binary)?;
        let blob = objects::object::Blob::new(content);
        repo.store().put_blob(&blob)?;
        repo.clear_missing_blob(&blob.hash())?;
        Ok(blob)
    }

    pub async fn hydrate_missing_blobs_for_state(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        target_state: ChangeId,
    ) -> Result<usize, ProtocolError> {
        let exchange = self
            .pull_exchange(
                repo,
                repo_path,
                remote_thread,
                PullOptions {
                    local_thread: None,
                    depth: None,
                    target_state: Some(target_state),
                    materialization: PullMaterialization::Full,
                },
            )
            .await?;
        clear_missing_blobs_for_state(repo, target_state)?;
        Ok(exchange.object_count)
    }

    async fn pull_with_options(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        options: PullOptions<'_>,
    ) -> Result<PullComplete, ProtocolError> {
        self.pull_exchange(repo, repo_path, remote_thread, options)
            .await
            .map(|exchange| exchange.result)
    }

    async fn pull_exchange(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        options: PullOptions<'_>,
    ) -> Result<PullExchange, ProtocolError> {
        let exchange_start = Instant::now();
        let mut exclude_states = Vec::new();
        if let Some(local_thread) = options.local_thread
            && let Some(head) = repo.refs().get_thread(&ThreadName::from(local_thread))?
        {
            exclude_states.push(head);
        }
        let allow_partial_fetch = options.materialization.allows_partial_fetch();
        let fresh_full_pull =
            supports_compact_full_pull(repo, allow_partial_fetch, &exclude_states)?;

        let transfer_id = pull_transfer_id(
            repo_path,
            remote_thread,
            options.local_thread,
            options.depth,
            options.target_state,
        );
        let request_message = PullMessage {
            body: Some(pull_message::Body::Request(PullRequest {
                repo_path: repo_path.to_string(),
                remote_thread: remote_thread.to_string(),
                local_thread: options.local_thread.unwrap_or_default().to_string(),
                target_state: options
                    .target_state
                    .map(|value| value.to_string_full())
                    .unwrap_or_default(),
                depth: options.depth.unwrap_or_default(),
                exclude_states: exclude_states
                    .iter()
                    .map(ChangeId::to_string_full)
                    .collect(),
                transfer: Some(self.transport.transfer_checkpoint_with_mode(
                    transfer_id.clone(),
                    TransportMode::NativePack,
                    0,
                    0,
                    false,
                )),
                partial_fetch_status: partial_fetch_status_for_repo(repo),
                allow_partial_fetch,
                fresh_full_pull,
                client_operation_id: String::new(),
            })),
        };

        let (tx, rx) = mpsc::channel(self.transport.max_inflight_objects.max(4));
        tx.send(request_message).await.map_err(|_| {
            ProtocolError::InvalidState("failed to initialize pull stream".to_string())
        })?;
        let mut request = Request::new(ReceiverStream::new(rx));
        self.apply_auth(&mut request)?;
        let mut response = self
            .inner
            .pull(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();

        let ready = match response.message().await.map_err(status_to_protocol_error)? {
            Some(PullMessage {
                body: Some(pull_message::Body::Ready(ready)),
            }) => ready,
            _ => {
                return Err(ProtocolError::InvalidState(
                    "expected PullReady from gRPC server".to_string(),
                ));
            }
        };
        let mut profile = PullProfile {
            ready_wait: exchange_start.elapsed(),
            ..PullProfile::default()
        };
        let remote_state = ChangeId::parse(&ready.remote_state)
            .map_err(|err| ProtocolError::InvalidState(err.to_string()))?;
        let PullWantPlan {
            wants,
            wanted_types,
            want_full_closure,
        } = plan_pull_wants(
            repo,
            &remote_state,
            ready.full_closure_available,
            ready.objects_to_fetch,
            allow_partial_fetch,
        )?;

        tx.send(PullMessage {
            body: Some(pull_message::Body::Want(WantObjects {
                objects: wants.clone(),
                want_full_closure,
                transfer: Some(self.transport.transfer_checkpoint_with_mode(
                    transfer_id.clone(),
                    TransportMode::NativePack,
                    0,
                    0,
                    false,
                )),
            })),
        })
        .await
        .map_err(|_| ProtocolError::InvalidState("pull stream closed unexpectedly".to_string()))?;
        drop(tx);

        let receive_start = Instant::now();
        let mut pack_state = proto::PackChunkState::default();
        let mut received = 0usize;
        while let Some(message) = response.message().await.map_err(status_to_protocol_error)? {
            match message.body {
                Some(pull_message::Body::Pack(chunk)) => {
                    profile.bytes_received =
                        profile.bytes_received.saturating_add(chunk.data.len());
                    profile.pack_bytes_received =
                        profile.pack_bytes_received.saturating_add(chunk.data.len());
                    let transfer = chunk.transfer.as_ref().ok_or_else(|| {
                        ProtocolError::InvalidState(
                            "native pack chunk missing transfer checkpoint".to_string(),
                        )
                    })?;
                    let stream_kind = PackStreamKind::try_from(chunk.stream_kind)
                        .unwrap_or(PackStreamKind::Unspecified);
                    if stream_kind == PackStreamKind::Unspecified {
                        return Err(ProtocolError::InvalidState(
                            "native pack chunk missing stream kind".to_string(),
                        ));
                    }
                    let decode_start = Instant::now();
                    proto::receive_pack_chunk(
                        &mut pack_state,
                        stream_kind == PackStreamKind::Index,
                        transfer.resume_offset,
                        transfer.chunk_index,
                        transfer.is_complete,
                        &chunk.data,
                        chunk.is_final_chunk,
                    )?;
                    let decode_elapsed = decode_start.elapsed();
                    profile.pack_decode += decode_elapsed;
                    profile.pack_decode_apply += decode_elapsed;
                    profile.decode += decode_elapsed;
                }
                Some(pull_message::Body::Redaction(transfer)) => {
                    // Out-of-pack channel: receive a redaction sidecar
                    // and route through `Repository::accept_wire_redactions`
                    // for signature + trust-list verification. The
                    // server emitted these only for blobs in our want
                    // set that carry an active redaction.
                    profile.bytes_received = profile
                        .bytes_received
                        .saturating_add(transfer.redactions_blob.len());
                    profile.object_mix.record(ObjectType::Redaction);
                    let blob = ContentHash::from_hex(&transfer.blob_hash).map_err(|err| {
                        ProtocolError::InvalidState(format!(
                            "RedactionTransfer.blob_hash is not a valid content hash: {err}"
                        ))
                    })?;
                    let decode_start = Instant::now();
                    repo.accept_wire_redactions(blob, &transfer.redactions_blob)
                        .map_err(|err| {
                            ProtocolError::InvalidState(format!(
                                "accept_wire_redactions for blob {}: {err}",
                                transfer.blob_hash
                            ))
                        })?;
                    let decode_elapsed = decode_start.elapsed();
                    profile.store_receive_object += decode_elapsed;
                }
                Some(pull_message::Body::Complete(complete)) => {
                    profile.receive_and_apply = receive_start.elapsed();
                    let final_state = if complete.new_state.is_empty() {
                        None
                    } else {
                        Some(
                            ChangeId::parse(&complete.new_state)
                                .map_err(|err| ProtocolError::InvalidState(err.to_string()))?,
                        )
                    };

                    if complete.success {
                        if want_full_closure || !wants.is_empty() {
                            if !pack_state.is_complete() {
                                return Err(ProtocolError::InvalidState(
                                    "pull completed before native pack stream finished".to_string(),
                                ));
                            }
                            let store_start = Instant::now();
                            let installed_ids = proto::install_received_pack(
                                repo.store(),
                                &pack_state.pack_data,
                                &pack_state.index_data,
                            )?;
                            profile.store_receive_object += store_start.elapsed();
                            received = installed_ids.len();
                            for id in installed_ids {
                                match (id, wanted_types.get(&id).copied()) {
                                    (PackObjectId::Hash(hash), Some(ObjectType::Blob)) => {
                                        profile.object_mix.record(ObjectType::Blob);
                                        repo.clear_missing_blob(&hash)?;
                                    }
                                    (_, Some(obj_type)) => {
                                        profile.object_mix.record(obj_type);
                                    }
                                    (PackObjectId::ChangeId(_), None) => {
                                        profile.object_mix.record(ObjectType::State);
                                    }
                                    (PackObjectId::Hash(hash), None) => {
                                        let inferred =
                                            infer_installed_hash_object_type(repo, &hash)?;
                                        profile.object_mix.record(inferred);
                                    }
                                }
                            }
                        }

                        let metadata_start = Instant::now();
                        if let Some(local_thread) = options.local_thread
                            && let Some(state) = final_state
                        {
                            repo.refs().set_thread(&ThreadName::from(local_thread), &state)?;
                        }
                        if let Some(state) = final_state
                            && allow_partial_fetch
                        {
                            mark_missing_blobs_for_state(repo, state)?;
                        } else if final_state.is_some() {
                            let _ = repo.clear_all_missing_blobs()?;
                        }
                        let synced_markers = complete
                            .transfer
                            .as_ref()
                            .map(|transfer| apply_marker_snapshot(repo, &transfer.checkpoint))
                            .transpose()?
                            .unwrap_or(false);
                        if !synced_markers {
                            self.sync_local_markers(repo, repo_path).await?;
                        }
                        profile.metadata_sync = metadata_start.elapsed();
                        profile.objects_received = received;
                        return Ok(PullExchange {
                            result: PullComplete {
                                success: true,
                                final_state,
                                error: None,
                                transfer_id: complete
                                    .transfer
                                    .as_ref()
                                    .map(|transfer| transfer.transfer_id.clone())
                                    .unwrap_or_default(),
                                transport_mode: complete
                                    .transfer
                                    .as_ref()
                                    .map(|transfer| transport_mode_name(transfer.transport_mode))
                                    .unwrap_or("native-pack")
                                    .to_string(),
                                resume_offset: complete
                                    .transfer
                                    .as_ref()
                                    .map(|transfer| transfer.resume_offset)
                                    .unwrap_or_default(),
                                chunk_index: complete
                                    .transfer
                                    .as_ref()
                                    .map(|transfer| transfer.chunk_index)
                                    .unwrap_or_default(),
                                checkpoint: complete
                                    .transfer
                                    .as_ref()
                                    .map(|transfer| transfer.checkpoint.clone())
                                    .unwrap_or_default(),
                                is_complete: complete
                                    .transfer
                                    .as_ref()
                                    .map(|transfer| transfer.is_complete)
                                    .unwrap_or(false),
                            },
                            object_count: received,
                            profile,
                        });
                    }

                    profile.objects_received = received;
                    return Ok(PullExchange {
                        result: PullComplete {
                            success: false,
                            final_state,
                            error: (!complete.error.is_empty()).then_some(complete.error),
                            transfer_id: complete
                                .transfer
                                .as_ref()
                                .map(|transfer| transfer.transfer_id.clone())
                                .unwrap_or_default(),
                            transport_mode: complete
                                .transfer
                                .as_ref()
                                .map(|transfer| transport_mode_name(transfer.transport_mode))
                                .unwrap_or("native-pack")
                                .to_string(),
                            resume_offset: complete
                                .transfer
                                .as_ref()
                                .map(|transfer| transfer.resume_offset)
                                .unwrap_or_default(),
                            chunk_index: complete
                                .transfer
                                .as_ref()
                                .map(|transfer| transfer.chunk_index)
                                .unwrap_or_default(),
                            checkpoint: complete
                                .transfer
                                .as_ref()
                                .map(|transfer| transfer.checkpoint.clone())
                                .unwrap_or_default(),
                            is_complete: complete
                                .transfer
                                .as_ref()
                                .map(|transfer| transfer.is_complete)
                                .unwrap_or(false),
                        },
                        object_count: received,
                        profile,
                    });
                }
                _ => {}
            }
        }

        Err(ProtocolError::InvalidState(format!(
            "pull stream ended unexpectedly after receiving {received} packed objects"
        )))
    }
}

fn load_thread_metadata(
    repo: &Repository,
    target_thread: &str,
    local_state: ChangeId,
) -> Result<Option<SyncedThreadMetadata>, ProtocolError> {
    let thread_manager = ThreadManager::new(repo.heddle_dir());
    Ok(thread_manager.find_synced_record_by_thread(repo, target_thread, Some(local_state))?)
}

fn plan_pull_wants(
    repo: &Repository,
    remote_state: &ChangeId,
    full_closure_available: bool,
    objects_to_fetch: Vec<ObjectDescriptor>,
    allow_partial_fetch: bool,
) -> Result<PullWantPlan, ProtocolError> {
    if full_closure_available {
        return Ok(PullWantPlan {
            wants: Vec::new(),
            wanted_types: HashMap::new(),
            want_full_closure: true,
        });
    }
    let request_full_closure =
        should_request_full_closure(repo, remote_state, allow_partial_fetch)?;
    let mut wants = Vec::with_capacity(objects_to_fetch.len());
    let mut wanted_types = HashMap::with_capacity(objects_to_fetch.len());

    for descriptor in objects_to_fetch {
        let info = parse_descriptor_to_info(descriptor)?;
        let pack_id = match &info.id {
            proto::ObjectId::Hash(hash) => PackObjectId::Hash(*hash),
            proto::ObjectId::ChangeId(change_id) => PackObjectId::ChangeId(*change_id),
        };
        let include = if request_full_closure {
            true
        } else {
            let has = proto::has_object(repo.store(), &info)?;
            !(has || (allow_partial_fetch && matches!(info.obj_type, ObjectType::Blob)))
        };

        if include {
            wanted_types.insert(pack_id, info.obj_type);
            wants.push(object_descriptor_with_status(
                &info,
                ObjectAvailabilityStatus::Missing,
                "requested by client",
            ));
        }
    }

    Ok(PullWantPlan {
        wants,
        wanted_types,
        want_full_closure: false,
    })
}

fn supports_compact_full_pull(
    repo: &Repository,
    allow_partial_fetch: bool,
    exclude_states: &[ChangeId],
) -> Result<bool, ProtocolError> {
    if allow_partial_fetch || !exclude_states.is_empty() {
        return Ok(false);
    }
    repo_looks_fresh(repo)
}

fn should_request_full_closure(
    repo: &Repository,
    remote_state: &ChangeId,
    allow_partial_fetch: bool,
) -> Result<bool, ProtocolError> {
    if allow_partial_fetch || repo.store().has_state(remote_state)? {
        return Ok(false);
    }
    repo_looks_fresh(repo)
}

fn repo_looks_fresh(repo: &Repository) -> Result<bool, ProtocolError> {
    if repo.head()?.is_some() {
        return Ok(false);
    }
    if !repo.refs().list_threads()?.is_empty() || !repo.refs().list_markers()?.is_empty() {
        return Ok(false);
    }
    Ok(repo.missing_blobs()?.is_empty())
}

fn infer_installed_hash_object_type(
    repo: &Repository,
    hash: &ContentHash,
) -> Result<ObjectType, ProtocolError> {
    let store = repo.store();
    if store.get_tree(hash)?.is_some() {
        return Ok(ObjectType::Tree);
    }
    if store
        .get_action(&objects::object::ActionId::from_hash(*hash))?
        .is_some()
    {
        return Ok(ObjectType::Action);
    }
    Ok(ObjectType::Blob)
}

fn apply_marker_snapshot(repo: &Repository, checkpoint: &[u8]) -> Result<bool, ProtocolError> {
    const HEADER: &str = "heddle-markers-v1\n";
    if checkpoint.is_empty() {
        return Ok(false);
    }
    let payload = std::str::from_utf8(checkpoint)
        .map_err(|err| ProtocolError::InvalidState(err.to_string()))?;
    let Some(lines) = payload.strip_prefix(HEADER) else {
        return Ok(false);
    };

    for line in lines.lines() {
        if line.is_empty() {
            continue;
        }
        let Some((name, change_id)) = line.split_once('\t') else {
            return Err(ProtocolError::InvalidState(
                "invalid marker snapshot line".to_string(),
            ));
        };
        let change_id = ChangeId::parse(change_id)
            .map_err(|err| ProtocolError::InvalidState(err.to_string()))?;
        if !repo.store().has_state(&change_id)? {
            continue;
        }
        let name = MarkerName::from(name);
        match repo.refs().get_marker(&name)? {
            Some(existing) if existing == change_id => {}
            Some(existing) => repo.refs().set_marker_cas(
                &name,
                refs::RefExpectation::Value(existing),
                &change_id,
            )?,
            None => repo.refs().create_marker(&name, &change_id)?,
        }
    }

    Ok(true)
}

fn change_id_string_to_bytes(s: &str) -> Vec<u8> {
    if s.is_empty() {
        return Vec::new();
    }
    objects::object::ChangeId::parse(s)
        .map(|id| id.as_bytes().to_vec())
        .unwrap_or_default()
}

fn to_proto_thread_metadata(metadata: &SyncedThreadMetadata) -> ThreadMetadata {
    ThreadMetadata {
        name: metadata.thread.clone(),
        target_thread: metadata.target_thread.clone(),
        parent_thread: metadata.parent_thread.clone(),
        task: metadata.task.clone(),
        thread_mode: metadata.mode.to_string(),
        thread_state: metadata.state.to_string(),
        freshness: metadata.freshness.to_string(),
        base_state: change_id_string_to_bytes(&metadata.base_state),
        base_root: change_id_string_to_bytes(&metadata.base_root),
        current_state: metadata
            .current_state
            .as_deref()
            .map(change_id_string_to_bytes),
        merged_state: metadata
            .merged_state
            .as_deref()
            .map(change_id_string_to_bytes),
        changed_paths: metadata.changed_paths.clone(),
        impact_categories: metadata
            .impact_categories
            .iter()
            .map(ToString::to_string)
            .collect(),
        heavy_impact_paths: metadata.heavy_impact_paths.clone(),
        promotion_suggested: metadata.promotion_suggested,
        verification_summary: Some(ThreadVerificationSummary {
            tests_passed: metadata.verification_summary.tests_passed,
            tests_failed: metadata
                .verification_summary
                .tests_failed
                .unwrap_or_default(),
            coverage_pct: metadata.verification_summary.coverage_pct,
            lint_warnings: metadata.verification_summary.lint_warnings,
        }),
        confidence_summary: Some(ThreadConfidenceSummary {
            value: metadata.confidence_summary.value,
            band: metadata
                .confidence_summary
                .band
                .as_ref()
                .map(ToString::to_string),
        }),
        integration_policy_result: Some(ThreadIntegrationPolicy {
            status: metadata
                .integration_policy_result
                .status
                .clone()
                .unwrap_or_default(),
            reason: metadata
                .integration_policy_result
                .reason
                .clone()
                .unwrap_or_default(),
        }),
        created_at: Some(prost_types::Timestamp {
            seconds: metadata.created_at.timestamp(),
            nanos: metadata.created_at.timestamp_subsec_nanos() as i32,
        }),
        updated_at: Some(prost_types::Timestamp {
            seconds: metadata.updated_at.timestamp(),
            nanos: metadata.updated_at.timestamp_subsec_nanos() as i32,
        }),
    }
}

struct PullExchange {
    result: PullComplete,
    object_count: usize,
    profile: PullProfile,
}

fn mark_missing_blobs_for_state(
    repo: &Repository,
    state_id: ChangeId,
) -> Result<(), ProtocolError> {
    let state = repo
        .store()
        .get_state(&state_id)?
        .ok_or_else(|| ProtocolError::ObjectNotFound(state_id.to_string_full()))?;
    let mut missing = collect_missing_blobs(repo, &state.tree);
    if let Some(context_root) = state.context.as_ref() {
        missing.extend(collect_missing_blobs(repo, context_root));
    }
    missing
        .into_iter()
        .try_for_each(|hash| repo.record_missing_blob(hash).map_err(ProtocolError::from))
}

fn clear_missing_blobs_for_state(
    repo: &Repository,
    state_id: ChangeId,
) -> Result<(), ProtocolError> {
    let state = repo
        .store()
        .get_state(&state_id)?
        .ok_or_else(|| ProtocolError::ObjectNotFound(state_id.to_string_full()))?;
    let mut missing = collect_missing_blobs(repo, &state.tree);
    if let Some(context_root) = state.context.as_ref() {
        missing.extend(collect_missing_blobs(repo, context_root));
    }
    missing
        .into_iter()
        .try_for_each(|hash| repo.clear_missing_blob(&hash).map_err(ProtocolError::from))
}

fn collect_missing_blobs(repo: &Repository, tree_hash: &ContentHash) -> Vec<ContentHash> {
    let mut missing = Vec::new();
    collect_missing_blobs_recursive(repo, tree_hash, &mut missing);
    missing
}

fn collect_missing_blobs_recursive(
    repo: &Repository,
    tree_hash: &ContentHash,
    missing: &mut Vec<ContentHash>,
) {
    let Some(tree) = repo.store().get_tree(tree_hash).ok().flatten() else {
        return;
    };
    for entry in tree.entries() {
        match entry.entry_type {
            objects::object::EntryType::Blob | objects::object::EntryType::Symlink => {
                if !repo.store().has_blob(&entry.hash).unwrap_or(true) {
                    missing.push(entry.hash);
                }
            }
            objects::object::EntryType::Tree => {
                collect_missing_blobs_recursive(repo, &entry.hash, missing);
            }
        }
    }
}

fn partial_fetch_status_for_repo(repo: &Repository) -> i32 {
    match repo.missing_blobs() {
        Ok(missing) if !missing.is_empty() => PartialFetchStatus::Enabled as i32,
        Ok(_) => PartialFetchStatus::Disabled as i32,
        Err(_) => PartialFetchStatus::Unspecified as i32,
    }
}

fn pull_transfer_id(
    repo_path: &str,
    remote_thread: &str,
    local_thread: Option<&str>,
    depth: Option<u32>,
    target_state: Option<ChangeId>,
) -> String {
    format!(
        "pull:{repo_path}:{remote_thread}:{}:{depth:?}:{}",
        local_thread.unwrap_or_default(),
        target_state
            .map(|value| value.to_string_full())
            .unwrap_or_default()
    )
}

fn push_transfer_id(repo_path: &str, local_state: ChangeId, target_thread: &str) -> String {
    format!(
        "push:{repo_path}:{}:{target_thread}",
        local_state.to_string_full()
    )
}

fn encode_native_pack_messages(
    bundle: &proto::NativePackBundle,
    transfer_id: &str,
    chunk_size: usize,
    transport: &super::helpers::HostedTransportPolicy,
    transport_mode: TransportMode,
) -> Result<Vec<PushMessage>, ProtocolError> {
    let mut messages = Vec::new();
    let chunk_size = chunk_size.max(1);

    let pack_total_chunks = proto::chunk_count(bundle.pack_data.len(), chunk_size);
    for chunk_index in 0..pack_total_chunks.max(1) {
        let Some((start, len)) =
            proto::chunk_bounds(bundle.pack_data.len(), chunk_size, chunk_index)
        else {
            break;
        };
        messages.push(PushMessage {
            body: Some(push_message::Body::Pack(PackChunk {
                stream_kind: PackStreamKind::Pack as i32,
                data: bundle.pack_data[start..start + len].to_vec(),
                transfer: Some(transport.transfer_checkpoint_with_mode(
                    transfer_id,
                    transport_mode,
                    chunk_index as u32,
                    start as u64,
                    chunk_index + 1 == pack_total_chunks,
                )),
                chunk_length: len as u32,
                is_final_chunk: chunk_index + 1 == pack_total_chunks,
            })),
        });
    }

    let index_total_chunks = proto::chunk_count(bundle.index_data.len(), chunk_size);
    for chunk_index in 0..index_total_chunks.max(1) {
        let Some((start, len)) =
            proto::chunk_bounds(bundle.index_data.len(), chunk_size, chunk_index)
        else {
            break;
        };
        messages.push(PushMessage {
            body: Some(push_message::Body::Pack(PackChunk {
                stream_kind: PackStreamKind::Index as i32,
                data: bundle.index_data[start..start + len].to_vec(),
                transfer: Some(transport.transfer_checkpoint_with_mode(
                    transfer_id,
                    transport_mode,
                    chunk_index as u32,
                    start as u64,
                    chunk_index + 1 == index_total_chunks,
                )),
                chunk_length: len as u32,
                is_final_chunk: chunk_index + 1 == index_total_chunks,
            })),
        });
    }
    Ok(messages)
}

fn preferred_transport_mode(
    transport: &super::helpers::HostedTransportPolicy,
    object_count: usize,
) -> TransportMode {
    let _ = transport;
    let _ = object_count;
    TransportMode::NativePack
}
