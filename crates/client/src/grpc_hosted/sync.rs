use objects::store::ObjectStore;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use grpc::heddle::v1::{
    GetBlobRequest, ListRefsRequest, ObjectAvailabilityStatus, ObjectDescriptor, PackChunk,
    PackStreamKind, PartialFetchStatus, PullMessage, PullRequest, PushMessage, PushRequest,
    RedactionTransfer, StateVisibilityTransfer, ThreadConfidenceSummary, ThreadIntegrationPolicy,
    ThreadMetadata, ThreadVerificationSummary, TransportMode, UpdateRefRequest, WantObjects,
    pull_message, push_message,
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
    wanted_types: WantedTypes,
    want_full_closure: bool,
}

type WantedTypes = HashMap<PackObjectId, Vec<ObjectType>>;

#[derive(Debug, Clone, Default)]
pub struct PullObjectMix {
    pub blobs: usize,
    pub trees: usize,
    pub states: usize,
    pub actions: usize,
    pub redactions: usize,
    pub state_visibilities: usize,
}

impl PullObjectMix {
    fn record(&mut self, obj_type: ObjectType) {
        match obj_type {
            ObjectType::Blob => self.blobs += 1,
            ObjectType::Tree => self.trees += 1,
            ObjectType::State => self.states += 1,
            ObjectType::Action => self.actions += 1,
            ObjectType::Redaction => self.redactions += 1,
            ObjectType::StateVisibility => self.state_visibilities += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.blobs
            + self.trees
            + self.states
            + self.actions
            + self.redactions
            + self.state_visibilities
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
        // sidecars → out-of-pack transfer channels. Sidecars live outside
        // `.heddle/objects/` so GC can't reach them and they can't ride the
        // pack — `build_native_pack` skips the same object-type set.
        let (wanted_sidecars, wanted_packable): (Vec<_>, Vec<_>) = wanted_infos
            .into_iter()
            .partition(|info| is_out_of_pack_transfer_object_type(info.obj_type));

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

        for info in wanted_sidecars {
            let message = sidecar_push_message(repo, info)?;
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
        let native_pack_required = native_pack_required_for_pull(want_full_closure, &wanted_types);

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
                Some(pull_message::Body::StateVisibility(transfer)) => {
                    profile.bytes_received = profile
                        .bytes_received
                        .saturating_add(transfer.state_visibility_blob.len());
                    profile.object_mix.record(ObjectType::StateVisibility);
                    let state = ChangeId::parse(&transfer.state_id).map_err(|err| {
                        ProtocolError::InvalidState(format!(
                            "StateVisibilityTransfer.state_id is not a valid ChangeId: {err}"
                        ))
                    })?;
                    let decode_start = Instant::now();
                    repo.accept_wire_state_visibility(state, &transfer.state_visibility_blob)
                        .map_err(|err| {
                            ProtocolError::InvalidState(format!(
                                "accept_wire_state_visibility for state {}: {err}",
                                transfer.state_id
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
                        if native_pack_required {
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
                                match (id, wanted_packable_type(&wanted_types, &id)) {
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

fn redaction_push_message(
    repo: &Repository,
    info: proto::ObjectInfo,
) -> Result<PushMessage, ProtocolError> {
    let proto::ObjectId::Hash(blob) = info.id else {
        return Err(ProtocolError::InvalidState(
            "wanted Redaction must be keyed by ObjectId::Hash(content_hash)".to_string(),
        ));
    };
    let hex = blob.to_hex();
    // Sender-side: load the byte-identical sidecar payload
    // that `Repository::put_redaction` wrote to disk. The
    // receiver verifies the signature + trust list and then
    // persists these bytes verbatim.
    let bytes = repo
        .store()
        .get_redactions_bytes_for_blob(&blob)
        .map_err(|err| {
            ProtocolError::InvalidState(format!("load redactions sidecar for {}: {err}", hex))
        })?
        .ok_or_else(|| {
            ProtocolError::InvalidState(format!(
                "server wants redaction for blob {} but sender has no sidecar",
                hex
            ))
        })?;
    Ok(PushMessage {
        body: Some(push_message::Body::Redaction(RedactionTransfer {
            blob_hash: hex,
            redactions_blob: bytes,
        })),
    })
}

fn is_out_of_pack_transfer_object_type(obj_type: ObjectType) -> bool {
    matches!(obj_type, ObjectType::Redaction | ObjectType::StateVisibility)
}

fn native_pack_required_for_pull(
    want_full_closure: bool,
    wanted_types: &WantedTypes,
) -> bool {
    want_full_closure
        || wanted_types
            .values()
            .flatten()
            .copied()
            .any(proto::is_native_packable_object_type)
}

fn record_wanted_type(
    wanted_types: &mut WantedTypes,
    pack_id: PackObjectId,
    obj_type: ObjectType,
) {
    let types = wanted_types.entry(pack_id).or_default();
    if !types.contains(&obj_type) {
        types.push(obj_type);
    }
}

fn wanted_packable_type(
    wanted_types: &WantedTypes,
    pack_id: &PackObjectId,
) -> Option<ObjectType> {
    wanted_types
        .get(pack_id)
        .and_then(|types| {
            types
                .iter()
                .copied()
                .find(|obj_type| proto::is_native_packable_object_type(*obj_type))
        })
}

fn sidecar_push_message(
    repo: &Repository,
    info: proto::ObjectInfo,
) -> Result<PushMessage, ProtocolError> {
    match info.obj_type {
        ObjectType::Redaction => redaction_push_message(repo, info),
        ObjectType::StateVisibility => state_visibility_push_message(repo, info),
        obj_type => Err(ProtocolError::InvalidState(format!(
            "{obj_type:?} is not an out-of-pack sidecar object"
        ))),
    }
}

fn state_visibility_push_message(
    repo: &Repository,
    info: proto::ObjectInfo,
) -> Result<PushMessage, ProtocolError> {
    let proto::ObjectId::ChangeId(state) = info.id else {
        return Err(ProtocolError::InvalidState(
            "wanted StateVisibility must be keyed by ObjectId::ChangeId(state)".to_string(),
        ));
    };
    let state_id = state.to_string_full();
    let bytes = repo
        .get_state_visibility_bytes_for_state(&state)
        .map_err(|err| {
            ProtocolError::InvalidState(format!(
                "load state-visibility sidecar for {}: {err}",
                state_id
            ))
        })?
        .ok_or_else(|| {
            ProtocolError::InvalidState(format!(
                "server wants state visibility for state {} but sender has no sidecar",
                state_id
            ))
        })?;
    Ok(PushMessage {
        body: Some(push_message::Body::StateVisibility(
            StateVisibilityTransfer {
                state_id,
                state_visibility_blob: bytes,
            },
        )),
    })
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
            record_wanted_type(&mut wanted_types, pack_id, info.obj_type);
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
    let mut missing = collect_missing_blobs(repo, &state.tree)?;
    if let Some(context_root) = state.context.as_ref() {
        missing.extend(collect_missing_blobs(repo, context_root)?);
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
    let mut missing = collect_missing_blobs(repo, &state.tree)?;
    if let Some(context_root) = state.context.as_ref() {
        missing.extend(collect_missing_blobs(repo, context_root)?);
    }
    missing
        .into_iter()
        .try_for_each(|hash| repo.clear_missing_blob(&hash).map_err(ProtocolError::from))
}

fn collect_missing_blobs(
    repo: &Repository,
    tree_hash: &ContentHash,
) -> Result<Vec<ContentHash>, ProtocolError> {
    let mut missing = Vec::new();
    collect_missing_blobs_recursive(repo, tree_hash, &mut missing)?;
    Ok(missing)
}

fn collect_missing_blobs_recursive(
    repo: &Repository,
    tree_hash: &ContentHash,
    missing: &mut Vec<ContentHash>,
) -> Result<(), ProtocolError> {
    let Some(tree) = repo.store().get_tree(tree_hash).map_err(|err| {
        ProtocolError::InvalidState(format!(
            "load tree {} while collecting lazy hydration missing blobs: {err}",
            tree_hash.to_hex()
        ))
    })?
    else {
        return Ok(());
    };
    for entry in tree.entries() {
        match entry.entry_type {
            objects::object::EntryType::Blob | objects::object::EntryType::Symlink => {
                if !repo.store().has_blob(&entry.hash).map_err(|err| {
                    ProtocolError::InvalidState(format!(
                        "check blob {} while collecting lazy hydration missing blobs: {err}",
                        entry.hash.to_hex()
                    ))
                })? {
                    missing.push(entry.hash);
                }
            }
            objects::object::EntryType::Tree => {
                collect_missing_blobs_recursive(repo, &entry.hash, missing)?;
            }
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use cli_shared::ClientConfig;
    use grpc::heddle::v1::{
        ListRefsRequest, ListRefsResponse, PullComplete as GrpcPullComplete, PullReady,
        TransferCheckpoint, UpdateRefRequest, UpdateRefResponse,
        repo_sync_service_server::{RepoSyncService, RepoSyncServiceServer},
        push_message,
    };
    use objects::{
        object::{
            Attribution, Blob, ChangeId, ContentHash, Principal, Redaction, State,
            StateVisibility, StateVisibilityBlob, Tree, TreeEntry, VisibilityTier,
        },
        store::ObjectStore,
    };
    use proto::{ObjectId, ObjectInfo};
    use tempfile::TempDir;
    use tonic::{Response, Status, transport::Server};

    use super::*;

    fn temp_repo() -> (TempDir, Repository) {
        let dir = TempDir::new().expect("tempdir");
        let repo = Repository::init_default(dir.path()).expect("init repo");
        (dir, repo)
    }

    fn redaction_info(blob: ContentHash) -> ObjectInfo {
        ObjectInfo {
            id: ObjectId::Hash(blob),
            obj_type: ObjectType::Redaction,
            size: 0,
            delta_base: None,
        }
    }

    fn state_info(state: ChangeId) -> ObjectInfo {
        ObjectInfo {
            id: ObjectId::ChangeId(state),
            obj_type: ObjectType::State,
            size: 0,
            delta_base: None,
        }
    }

    fn state_visibility_info(state: ChangeId) -> ObjectInfo {
        ObjectInfo {
            id: ObjectId::ChangeId(state),
            obj_type: ObjectType::StateVisibility,
            size: 0,
            delta_base: None,
        }
    }

    fn sample_blob() -> ContentHash {
        ContentHash::from_bytes([7u8; 32])
    }

    fn loose_tree_path(repo: &Repository, hash: &ContentHash) -> std::path::PathBuf {
        let hex = hash.to_hex();
        let (prefix, rest) = hex.split_at(2);
        repo.heddle_dir()
            .join("objects")
            .join("trees")
            .join(prefix)
            .join(rest)
    }

    fn sample_redaction(blob: ContentHash) -> Redaction {
        Redaction {
            redacted_blob: blob,
            state: ChangeId::from_bytes([1u8; 16]),
            path: "config/secrets.toml".into(),
            reason: "leaked credential".into(),
            redactor: Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            },
            redacted_at: Utc.with_ymd_and_hms(2026, 5, 10, 14, 33, 0).unwrap(),
            signature: None,
            purged_at: None,
            supersedes: None,
        }
    }

    fn sample_state_visibility(state: ChangeId) -> StateVisibility {
        StateVisibility {
            state,
            tier: VisibilityTier::Restricted {
                scope_label: "security-embargo".into(),
            },
            embargo_until: None,
            declarer: Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            },
            declared_at: Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap(),
            signature: None,
            supersedes: None,
        }
    }

    #[test]
    fn non_packable_object_types_are_in_out_of_pack_transfer_partition() {
        for obj_type in proto::native_pack_excluded_object_types() {
            assert!(
                is_out_of_pack_transfer_object_type(*obj_type),
                "{obj_type:?} is excluded from native packs but missing from the out-of-pack transfer partition"
            );
        }
    }

    #[test]
    fn native_pack_required_tracks_packable_pull_wants() {
        let blob = sample_blob();
        let state = ChangeId::from_bytes([9u8; 16]);

        let sidecar_only = HashMap::from([(
            PackObjectId::ChangeId(state),
            vec![ObjectType::StateVisibility],
        )]);
        assert!(!native_pack_required_for_pull(false, &sidecar_only));

        let redaction_only = HashMap::from([(PackObjectId::Hash(blob), vec![ObjectType::Redaction])]);
        assert!(!native_pack_required_for_pull(false, &redaction_only));

        let packable = HashMap::from([(PackObjectId::Hash(blob), vec![ObjectType::Blob])]);
        assert!(native_pack_required_for_pull(false, &packable));

        let state_with_sidecar = HashMap::from([(
            PackObjectId::ChangeId(state),
            vec![ObjectType::State, ObjectType::StateVisibility],
        )]);
        assert!(native_pack_required_for_pull(false, &state_with_sidecar));
        assert!(native_pack_required_for_pull(true, &HashMap::new()));
    }

    #[test]
    fn plan_pull_wants_accumulates_state_and_visibility_for_same_change_id() {
        let (_dir, repo) = temp_repo();
        let state = ChangeId::from_bytes([9u8; 16]);
        let plan = plan_pull_wants(
            &repo,
            &state,
            false,
            vec![
                object_descriptor_with_status(
                    &state_info(state),
                    ObjectAvailabilityStatus::Missing,
                    "missing state",
                ),
                object_descriptor_with_status(
                    &state_visibility_info(state),
                    ObjectAvailabilityStatus::Missing,
                    "missing state visibility",
                ),
            ],
            false,
        )
        .expect("plan pull wants");

        let wanted = plan
            .wanted_types
            .get(&PackObjectId::ChangeId(state))
            .expect("same ChangeId want entry");
        assert_eq!(wanted.as_slice(), &[ObjectType::State, ObjectType::StateVisibility]);
        assert!(native_pack_required_for_pull(
            plan.want_full_closure,
            &plan.wanted_types
        ));
    }

    #[derive(Clone)]
    struct SidecarOnlyPullService {
        state: ChangeId,
        state_visibility_blob: Vec<u8>,
    }

    #[tonic::async_trait]
    impl RepoSyncService for SidecarOnlyPullService {
        async fn list_refs(
            &self,
            _request: tonic::Request<ListRefsRequest>,
        ) -> Result<Response<ListRefsResponse>, Status> {
            Ok(Response::new(ListRefsResponse::default()))
        }

        async fn update_ref(
            &self,
            _request: tonic::Request<UpdateRefRequest>,
        ) -> Result<Response<UpdateRefResponse>, Status> {
            Ok(Response::new(UpdateRefResponse::default()))
        }

        type PushStream = tokio_stream::wrappers::ReceiverStream<Result<PushMessage, Status>>;

        async fn push(
            &self,
            _request: tonic::Request<tonic::Streaming<PushMessage>>,
        ) -> Result<Response<Self::PushStream>, Status> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
        }

        type PullStream = tokio_stream::wrappers::ReceiverStream<Result<PullMessage, Status>>;

        async fn pull(
            &self,
            request: tonic::Request<tonic::Streaming<PullMessage>>,
        ) -> Result<Response<Self::PullStream>, Status> {
            let state = self.state;
            let state_visibility_blob = self.state_visibility_blob.clone();
            let (tx, rx) = mpsc::channel(4);

            tokio::spawn(async move {
                let mut inbound = request.into_inner();
                match inbound.message().await {
                    Ok(Some(PullMessage {
                        body: Some(pull_message::Body::Request(_)),
                    })) => {}
                    other => {
                        let _ = tx
                            .send(Err(Status::invalid_argument(format!(
                                "expected pull request, got {other:?}"
                            ))))
                            .await;
                        return;
                    }
                }

                let descriptor = object_descriptor_with_status(
                    &state_visibility_info(state),
                    ObjectAvailabilityStatus::Missing,
                    "missing state visibility",
                );
                let ready = PullMessage {
                    body: Some(pull_message::Body::Ready(PullReady {
                        remote_state: state.to_string_full(),
                        objects_to_fetch: vec![descriptor],
                        transfer: None,
                        partial_fetch_status: PartialFetchStatus::Disabled as i32,
                        missing_objects: Vec::new(),
                        full_closure_available: false,
                        object_count: 1,
                    })),
                };
                if tx.send(Ok(ready)).await.is_err() {
                    return;
                }

                match inbound.message().await {
                    Ok(Some(PullMessage {
                        body: Some(pull_message::Body::Want(want)),
                    })) if !want.want_full_closure
                        && want.objects.len() == 1
                        && want.objects[0].object_type == "state_visibility" => {}
                    other => {
                        let _ = tx
                            .send(Err(Status::invalid_argument(format!(
                                "expected sidecar-only want, got {other:?}"
                            ))))
                            .await;
                        return;
                    }
                }

                let transfer = PullMessage {
                    body: Some(pull_message::Body::StateVisibility(
                        StateVisibilityTransfer {
                            state_id: state.to_string_full(),
                            state_visibility_blob,
                        },
                    )),
                };
                if tx.send(Ok(transfer)).await.is_err() {
                    return;
                }

                let complete = PullMessage {
                    body: Some(pull_message::Body::Complete(GrpcPullComplete {
                        success: true,
                        new_state: state.to_string_full(),
                        error: String::new(),
                        transfer: Some(TransferCheckpoint {
                            transfer_id: "sidecar-only-test".to_string(),
                            transport_mode: TransportMode::NativePack as i32,
                            resume_offset: 0,
                            chunk_index: 0,
                            checkpoint: b"heddle-markers-v1\n".to_vec(),
                            is_complete: true,
                        }),
                    })),
                };
                let _ = tx.send(Ok(complete)).await;
            });

            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
        }
    }

    async fn connect_sidecar_only_service(
        service: SidecarOnlyPullService,
    ) -> (HostedGrpcClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let incoming = futures::stream::unfold(listener, |listener| async {
            match listener.accept().await {
                Ok((stream, _addr)) => Some((Ok::<_, std::io::Error>(stream), listener)),
                Err(err) => Some((Err(err), listener)),
            }
        });

        let handle = tokio::spawn(async move {
            Server::builder()
                .add_service(RepoSyncServiceServer::new(service))
                .serve_with_incoming(incoming)
                .await
                .expect("serve sidecar-only test service");
        });

        let client = HostedGrpcClient::connect(addr, &ClientConfig::default())
            .await
            .expect("connect client");
        (client, handle)
    }

    #[tokio::test]
    async fn state_visibility_sidecar_only_pull_completes_without_native_pack() {
        let (_dir, repo) = temp_repo();
        let tree_hash = repo.store().put_tree(&Tree::new()).expect("put tree");
        let state = State::new_snapshot(
            tree_hash,
            vec![],
            Attribution::human(Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            }),
        );
        let state_id = state.change_id;
        repo.store().put_state(&state).expect("put state");
        assert!(
            repo.get_state_visibility_bytes_for_state(&state_id)
                .expect("load local sidecar")
                .is_none(),
            "test starts with state present and StateVisibility sidecar absent"
        );

        let state_visibility_blob =
            StateVisibilityBlob::new(vec![sample_state_visibility(state_id)])
                .encode()
                .expect("encode state visibility blob");
        let (mut client, server) = connect_sidecar_only_service(SidecarOnlyPullService {
            state: state_id,
            state_visibility_blob,
        })
        .await;

        let exchange = tokio::time::timeout(
            Duration::from_secs(5),
            client.pull_exchange(
                &repo,
                "owner/repo",
                "main",
                PullOptions {
                    local_thread: None,
                    depth: None,
                    target_state: Some(state_id),
                    materialization: PullMaterialization::Full,
                },
            ),
        )
        .await
        .expect("sidecar-only pull must not hang waiting for native pack")
        .expect("sidecar-only pull succeeds");
        server.abort();

        assert!(exchange.result.success);
        assert_eq!(exchange.object_count, 0);
        assert_eq!(exchange.profile.pack_bytes_received, 0);
        assert_eq!(exchange.profile.object_mix.state_visibilities, 1);
        assert!(
            repo.get_state_visibility_for_state(&state_id)
                .expect("load accepted sidecar")
                .has_record(),
            "pull must accept the out-of-pack StateVisibility sidecar"
        );
    }

    #[derive(Clone)]
    struct StateAndVisibilityPullService {
        state: ChangeId,
        pack_bundle: proto::NativePackBundle,
        state_visibility_blob: Vec<u8>,
    }

    #[tonic::async_trait]
    impl RepoSyncService for StateAndVisibilityPullService {
        async fn list_refs(
            &self,
            _request: tonic::Request<ListRefsRequest>,
        ) -> Result<Response<ListRefsResponse>, Status> {
            Ok(Response::new(ListRefsResponse::default()))
        }

        async fn update_ref(
            &self,
            _request: tonic::Request<UpdateRefRequest>,
        ) -> Result<Response<UpdateRefResponse>, Status> {
            Ok(Response::new(UpdateRefResponse::default()))
        }

        type PushStream = tokio_stream::wrappers::ReceiverStream<Result<PushMessage, Status>>;

        async fn push(
            &self,
            _request: tonic::Request<tonic::Streaming<PushMessage>>,
        ) -> Result<Response<Self::PushStream>, Status> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
        }

        type PullStream = tokio_stream::wrappers::ReceiverStream<Result<PullMessage, Status>>;

        async fn pull(
            &self,
            request: tonic::Request<tonic::Streaming<PullMessage>>,
        ) -> Result<Response<Self::PullStream>, Status> {
            let state = self.state;
            let pack_bundle = self.pack_bundle.clone();
            let state_visibility_blob = self.state_visibility_blob.clone();
            let (tx, rx) = mpsc::channel(8);

            tokio::spawn(async move {
                let mut inbound = request.into_inner();
                match inbound.message().await {
                    Ok(Some(PullMessage {
                        body: Some(pull_message::Body::Request(_)),
                    })) => {}
                    other => {
                        let _ = tx
                            .send(Err(Status::invalid_argument(format!(
                                "expected pull request, got {other:?}"
                            ))))
                            .await;
                        return;
                    }
                }

                let ready = PullMessage {
                    body: Some(pull_message::Body::Ready(PullReady {
                        remote_state: state.to_string_full(),
                        objects_to_fetch: vec![
                            object_descriptor_with_status(
                                &state_info(state),
                                ObjectAvailabilityStatus::Missing,
                                "missing state",
                            ),
                            object_descriptor_with_status(
                                &state_visibility_info(state),
                                ObjectAvailabilityStatus::Missing,
                                "missing state visibility",
                            ),
                        ],
                        transfer: None,
                        partial_fetch_status: PartialFetchStatus::Disabled as i32,
                        missing_objects: Vec::new(),
                        full_closure_available: false,
                        object_count: 2,
                    })),
                };
                if tx.send(Ok(ready)).await.is_err() {
                    return;
                }

                match inbound.message().await {
                    Ok(Some(PullMessage {
                        body: Some(pull_message::Body::Want(want)),
                    })) if !want.want_full_closure
                        && want.objects.len() == 2
                        && want.objects.iter().any(|object| object.object_type == "state")
                        && want
                            .objects
                            .iter()
                            .any(|object| object.object_type == "state_visibility") => {}
                    other => {
                        let _ = tx
                            .send(Err(Status::invalid_argument(format!(
                                "expected state + sidecar wants, got {other:?}"
                            ))))
                            .await;
                        return;
                    }
                }

                for message in encode_pull_native_pack_messages(
                    &pack_bundle,
                    "state-and-visibility-test",
                    16,
                ) {
                    if tx.send(Ok(message)).await.is_err() {
                        return;
                    }
                }

                let transfer = PullMessage {
                    body: Some(pull_message::Body::StateVisibility(
                        StateVisibilityTransfer {
                            state_id: state.to_string_full(),
                            state_visibility_blob,
                        },
                    )),
                };
                if tx.send(Ok(transfer)).await.is_err() {
                    return;
                }

                let complete = PullMessage {
                    body: Some(pull_message::Body::Complete(GrpcPullComplete {
                        success: true,
                        new_state: state.to_string_full(),
                        error: String::new(),
                        transfer: Some(TransferCheckpoint {
                            transfer_id: "state-and-visibility-test".to_string(),
                            transport_mode: TransportMode::NativePack as i32,
                            resume_offset: 0,
                            chunk_index: 0,
                            checkpoint: b"heddle-markers-v1\n".to_vec(),
                            is_complete: true,
                        }),
                    })),
                };
                let _ = tx.send(Ok(complete)).await;
            });

            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
        }
    }

    fn encode_pull_native_pack_messages(
        bundle: &proto::NativePackBundle,
        transfer_id: &str,
        chunk_size: usize,
    ) -> Vec<PullMessage> {
        let mut messages = Vec::new();
        let chunk_size = chunk_size.max(1);

        let pack_total_chunks = proto::chunk_count(bundle.pack_data.len(), chunk_size);
        for chunk_index in 0..pack_total_chunks.max(1) {
            let Some((start, len)) =
                proto::chunk_bounds(bundle.pack_data.len(), chunk_size, chunk_index)
            else {
                break;
            };
            messages.push(PullMessage {
                body: Some(pull_message::Body::Pack(PackChunk {
                    stream_kind: PackStreamKind::Pack as i32,
                    data: bundle.pack_data[start..start + len].to_vec(),
                    transfer: Some(TransferCheckpoint {
                        transfer_id: transfer_id.to_string(),
                        transport_mode: TransportMode::NativePack as i32,
                        resume_offset: start as u64,
                        chunk_index: chunk_index as u32,
                        checkpoint: Vec::new(),
                        is_complete: chunk_index + 1 == pack_total_chunks,
                    }),
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
            messages.push(PullMessage {
                body: Some(pull_message::Body::Pack(PackChunk {
                    stream_kind: PackStreamKind::Index as i32,
                    data: bundle.index_data[start..start + len].to_vec(),
                    transfer: Some(TransferCheckpoint {
                        transfer_id: transfer_id.to_string(),
                        transport_mode: TransportMode::NativePack as i32,
                        resume_offset: start as u64,
                        chunk_index: chunk_index as u32,
                        checkpoint: Vec::new(),
                        is_complete: chunk_index + 1 == index_total_chunks,
                    }),
                    chunk_length: len as u32,
                    is_final_chunk: chunk_index + 1 == index_total_chunks,
                })),
            });
        }

        messages
    }

    async fn connect_state_and_visibility_service(
        service: StateAndVisibilityPullService,
    ) -> (HostedGrpcClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let incoming = futures::stream::unfold(listener, |listener| async {
            match listener.accept().await {
                Ok((stream, _addr)) => Some((Ok::<_, std::io::Error>(stream), listener)),
                Err(err) => Some((Err(err), listener)),
            }
        });

        let handle = tokio::spawn(async move {
            Server::builder()
                .add_service(RepoSyncServiceServer::new(service))
                .serve_with_incoming(incoming)
                .await
                .expect("serve state-and-visibility test service");
        });

        let client = HostedGrpcClient::connect(addr, &ClientConfig::default())
            .await
            .expect("connect client");
        (client, handle)
    }

    #[tokio::test]
    async fn state_and_visibility_same_change_id_pull_requests_pack_and_sidecar() {
        let (_source_dir, source_repo) = temp_repo();
        let (_target_dir, target_repo) = temp_repo();
        let tree_hash = source_repo.store().put_tree(&Tree::new()).expect("put tree");
        let state = State::new_snapshot(
            tree_hash,
            vec![],
            Attribution::human(Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            }),
        );
        let state_id = state.change_id;
        source_repo.store().put_state(&state).expect("put source state");
        let state_visibility_blob =
            StateVisibilityBlob::new(vec![sample_state_visibility(state_id)])
                .encode()
                .expect("encode state visibility blob");
        source_repo
            .accept_wire_state_visibility(state_id, &state_visibility_blob)
            .expect("put source state visibility");
        let pack_bundle = proto::build_native_pack(
            source_repo.store(),
            &[state_info(state_id)],
        )
        .expect("build state pack");

        assert!(
            target_repo
                .store()
                .get_state(&state_id)
                .expect("load target state")
                .is_none(),
            "test starts with state absent"
        );
        assert!(
            target_repo
                .get_state_visibility_bytes_for_state(&state_id)
                .expect("load target sidecar")
                .is_none(),
            "test starts with StateVisibility sidecar absent"
        );

        let (mut client, server) =
            connect_state_and_visibility_service(StateAndVisibilityPullService {
                state: state_id,
                pack_bundle,
                state_visibility_blob,
            })
            .await;

        let exchange = tokio::time::timeout(
            Duration::from_secs(5),
            client.pull_exchange(
                &target_repo,
                "owner/repo",
                "main",
                PullOptions {
                    local_thread: None,
                    depth: None,
                    target_state: Some(state_id),
                    materialization: PullMaterialization::Full,
                },
            ),
        )
        .await
        .expect("state + sidecar pull must not hang waiting for native pack")
        .expect("state + sidecar pull succeeds");
        server.abort();

        assert!(exchange.result.success);
        assert_eq!(exchange.object_count, 1);
        assert!(exchange.profile.pack_bytes_received > 0);
        assert_eq!(exchange.profile.object_mix.states, 1);
        assert_eq!(exchange.profile.object_mix.state_visibilities, 1);
        assert!(
            target_repo
                .store()
                .get_state(&state_id)
                .expect("load installed state")
                .is_some(),
            "native pack must install the State"
        );
        assert!(
            target_repo
                .get_state_visibility_for_state(&state_id)
                .expect("load accepted sidecar")
                .has_record(),
            "pull must accept the out-of-pack StateVisibility sidecar"
        );
    }

    #[test]
    fn collect_missing_blobs_treats_absent_tree_as_empty() {
        let (_dir, repo) = temp_repo();
        let absent_tree = ContentHash::from_bytes([99u8; 32]);

        let missing =
            collect_missing_blobs(&repo, &absent_tree).expect("absent tree is not an error");

        assert!(missing.is_empty());
    }

    #[test]
    fn collect_missing_blobs_reports_only_genuinely_missing_blobs() {
        let (_dir, repo) = temp_repo();
        let present_blob = Blob::from("already local");
        let present_hash = repo.store().put_blob(&present_blob).expect("put blob");
        let missing_hash = ContentHash::from_bytes([42u8; 32]);
        let tree = Tree::from_entries(vec![
            TreeEntry::file("local.txt", present_hash, false).expect("present entry"),
            TreeEntry::file("remote.txt", missing_hash, false).expect("missing entry"),
        ]);
        let tree_hash = repo.store().put_tree(&tree).expect("put tree");

        let missing = collect_missing_blobs(&repo, &tree_hash).expect("collect missing blobs");

        assert_eq!(missing, vec![missing_hash]);
    }

    #[test]
    fn collect_missing_blobs_reports_corrupt_tree_read() {
        let (_dir, repo) = temp_repo();
        let tree_hash = repo.store().put_tree(&Tree::new()).expect("put tree");
        std::fs::write(loose_tree_path(&repo, &tree_hash), [0xc1]).expect("corrupt tree");
        repo.store().clear_recent_caches();

        let err = collect_missing_blobs(&repo, &tree_hash).expect_err("corrupt tree must fail");

        assert!(matches!(err, ProtocolError::InvalidState(_)));
        assert!(
            err.to_string().contains(&format!(
                "load tree {} while collecting lazy hydration missing blobs",
                tree_hash.to_hex()
            )),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn redaction_push_message_uses_hex_keyed_sidecar_payload() {
        let (_dir, repo) = temp_repo();
        let blob = sample_blob();
        repo.put_redaction(sample_redaction(blob))
            .expect("put redaction");
        let expected_bytes = repo
            .store()
            .get_redactions_bytes_for_blob(&blob)
            .expect("load sidecar")
            .expect("sidecar exists");

        let message = redaction_push_message(&repo, redaction_info(blob)).expect("message");

        let Some(push_message::Body::Redaction(transfer)) = message.body else {
            panic!("expected redaction transfer");
        };
        assert_eq!(transfer.blob_hash, blob.to_hex());
        assert_eq!(transfer.redactions_blob, expected_bytes);
    }

    #[test]
    fn redaction_push_message_reports_missing_sidecar_with_blob_hex() {
        let (_dir, repo) = temp_repo();
        let blob = sample_blob();

        let err = redaction_push_message(&repo, redaction_info(blob)).expect_err("missing sidecar");

        assert!(matches!(err, ProtocolError::InvalidState(_)));
        assert!(
            err.to_string().contains(&format!(
                "server wants redaction for blob {} but sender has no sidecar",
                blob.to_hex()
            )),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn redaction_push_message_reports_sidecar_load_error_with_blob_hex() {
        let (_dir, repo) = temp_repo();
        let blob = sample_blob();
        let redaction_path = repo
            .heddle_dir()
            .join("redactions")
            .join(format!("{}.bin", blob.to_hex()));
        std::fs::create_dir_all(&redaction_path).expect("directory at redaction path");

        let err = redaction_push_message(&repo, redaction_info(blob)).expect_err("load error");

        assert!(matches!(err, ProtocolError::InvalidState(_)));
        assert!(
            err.to_string()
                .contains(&format!("load redactions sidecar for {}:", blob.to_hex())),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn state_visibility_push_message_uses_state_keyed_sidecar_payload() {
        let (_dir, repo) = temp_repo();
        let state = ChangeId::from_bytes([17u8; 16]);
        repo.put_state_visibility(sample_state_visibility(state))
            .expect("put state visibility");
        let expected_bytes = repo
            .get_state_visibility_bytes_for_state(&state)
            .expect("load sidecar")
            .expect("sidecar exists");

        let message =
            state_visibility_push_message(&repo, state_visibility_info(state)).expect("message");

        let Some(push_message::Body::StateVisibility(transfer)) = message.body else {
            panic!("expected state visibility transfer");
        };
        assert_eq!(transfer.state_id, state.to_string_full());
        assert_eq!(transfer.state_visibility_blob, expected_bytes);
    }
}
