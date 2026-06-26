use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, Write},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use grpc::heddle::v1::{
    GetBlobRequest, GitCheckpointTransfer, GitLaneTransfer, GitPackTransfer, GitRefKind,
    GitRefUpdateTransfer, ListRefsRequest, ObjectAvailabilityStatus, ObjectDescriptor, PackChunk,
    PackStreamKind, PartialFetchStatus, PullMessage, PullRequest, PushMessage, PushRequest,
    RedactionTransfer, StateVisibilityTransfer, ThreadConfidenceSummary, ThreadIntegrationPolicy,
    ThreadMetadata, ThreadVerificationSummary, TransportMode, UpdateRefRequest, WantObjects,
    git_lane_transfer, pull_message, push_message,
};
use objects::{
    object::{ChangeId, ContentHash, MarkerName, ThreadName},
    store::{AnyStore, ObjectStore, PackObjectId},
};
use repo::{
    GitCheckpointRecord, Repository, RepositoryCapability, RevisionAddress, SyncedThreadMetadata,
    ThreadManager,
};
use sley::{
    ObjectId as GitObjectId, RefPrecondition, ReferenceTarget, Repository as SleyRepository,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use wire::{
    ObjectInfo, ObjectType, PlannedObject, ProtocolError, PullComplete, PushComplete, RefEntry,
    RefUpdated,
};

use super::{
    HostedGrpcClient, PullMaterialization,
    helpers::{
        descriptor_id, descriptor_id_from_info, object_descriptor_with_status, object_type_name,
        parse_descriptor_to_info, status_to_protocol_error, to_proto_object_info,
        transport_mode_name,
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

struct GitLanePushPlan {
    local_revision_address: String,
    pack: GitPackPushPlan,
    ref_update: PushMessage,
}

#[derive(Clone)]
struct GitPackPushPlan {
    transfer_id: String,
    pack_id: Vec<u8>,
    pack_size: u64,
    root: GitObjectId,
    git_repo: SleyRepository,
}

const PUSH_FULL_DESCRIPTOR_OBJECT_THRESHOLD: usize = 512;
const PULL_PACK_SPOOL_OBJECT_THRESHOLD: usize = 512;
const NATIVE_PACK_DRAIN_OBJECT_INTERVAL: usize = 32;
const NATIVE_PACK_OBJECT_PREFETCH_LIMIT: usize = 32;
const NATIVE_PACK_OBJECT_LOAD_WORKER_LIMIT: usize = 8;

#[derive(Debug, Clone, Default)]
pub struct PullObjectMix {
    pub blobs: usize,
    pub trees: usize,
    pub states: usize,
    pub actions: usize,
    pub redactions: usize,
    pub state_visibilities: usize,
}

#[derive(Debug, Clone)]
pub struct HostedRefEntry {
    pub name: String,
    pub change_id: ChangeId,
    pub is_thread: bool,
    pub revision_address: String,
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
        Ok(self
            .list_refs_with_revision_addresses(repo_path)
            .await?
            .into_iter()
            .map(|entry| RefEntry {
                name: entry.name,
                change_id: entry.change_id,
                is_thread: entry.is_thread,
            })
            .collect())
    }

    pub async fn list_refs_with_revision_addresses(
        &mut self,
        repo_path: &str,
    ) -> Result<Vec<HostedRefEntry>, ProtocolError> {
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
                Ok(HostedRefEntry {
                    name: entry.name,
                    change_id: ChangeId::try_from_slice(&entry.change_id)
                        .map_err(|err| ProtocolError::InvalidState(err.to_string()))?,
                    is_thread: entry.is_thread,
                    revision_address: entry.revision_address,
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
            old_revision_address: old_value.map(native_revision_address).unwrap_or_default(),
            new_revision_address: native_revision_address(new_value),
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
        self.push_with_revision(
            repo,
            repo_path,
            local_state,
            target_thread,
            force,
            native_revision_address(local_state),
            None,
        )
        .await
    }

    pub async fn push_git_overlay_checkpoint(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        local_state: ChangeId,
        target_thread: &str,
        force: bool,
    ) -> Result<PushComplete, ProtocolError> {
        let git_lane = build_git_lane_push_plan(
            repo,
            local_state,
            target_thread,
            self.transport.chunk_size.max(1),
        )?;
        let local_revision_address = git_lane.local_revision_address.clone();
        self.push_with_revision(
            repo,
            repo_path,
            local_state,
            target_thread,
            force,
            local_revision_address,
            Some(git_lane),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn push_with_revision(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        local_state: ChangeId,
        target_thread: &str,
        force: bool,
        local_revision_address: String,
        mut git_lane: Option<GitLanePushPlan>,
    ) -> Result<PushComplete, ProtocolError> {
        let _ = self.transport.chunk_size;
        let _ = self.transport.resume_attempts;
        let object_plan = wire::enumerate_state_closure_plan(repo.store(), local_state)?;
        let full_objects = if object_plan.len() <= PUSH_FULL_DESCRIPTOR_OBJECT_THRESHOLD {
            Some(wire::enumerate_state_closure(repo.store(), local_state)?)
        } else {
            None
        };
        let object_count = full_objects
            .as_ref()
            .map_or(object_plan.len(), std::vec::Vec::len);
        let transfer_id = push_transfer_id(repo_path, local_state, target_thread);
        let transport_mode = preferred_transport_mode(&self.transport, object_count);
        let thread_metadata = load_thread_metadata(repo, target_thread, local_state)?;
        let request_message = PushMessage {
            body: Some(push_message::Body::Request(PushRequest {
                repo_path: repo_path.to_string(),
                local_state: local_state.to_string_full(),
                target_thread: target_thread.to_string(),
                create_thread: true,
                force,
                objects: full_objects.as_ref().map_or_else(
                    || object_plan.iter().map(to_proto_planned_object).collect(),
                    |objects| objects.iter().map(to_proto_object_info).collect(),
                ),
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
                local_revision_address,
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
        if let Some(git_lane) = git_lane.as_mut() {
            apply_git_ref_expectation(&mut git_lane.ref_update, &ready.remote_revision_address)?;
        }

        let object_index = match full_objects {
            Some(objects) => objects
                .into_iter()
                .map(|info| (descriptor_id_from_info(&info), info))
                .collect::<HashMap<_, _>>(),
            None => object_plan
                .into_iter()
                .map(|object| {
                    (
                        descriptor_id_from_plan(&object),
                        object_info_from_plan(&object),
                    )
                })
                .collect::<HashMap<_, _>>(),
        };

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
            send_native_pack_streaming_messages(
                &tx,
                repo,
                &wanted_packable,
                &transfer_id,
                self.transport.chunk_size.max(1),
                &self.transport,
                ready_transport_mode,
            )
            .await?;
        }

        for info in wanted_sidecars {
            let message = sidecar_push_message(repo, info)?;
            tx.send(message).await.map_err(|_| {
                ProtocolError::InvalidState("push stream closed unexpectedly".to_string())
            })?;
        }

        if let Some(git_lane) = git_lane {
            send_git_pack_streaming_messages(&tx, &git_lane.pack, self.transport.chunk_size.max(1))
                .await?;
            tx.send(git_lane.ref_update).await.map_err(|_| {
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
                target_revision_address: options
                    .target_state
                    .map(native_revision_address)
                    .unwrap_or_default(),
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
        let advertised_object_count = ready.objects_to_fetch.len();
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
        let use_pack_spool = advertised_object_count > PULL_PACK_SPOOL_OBJECT_THRESHOLD;
        let mut pack_state = wire::PackChunkState::default();
        let mut pack_spool = if use_pack_spool {
            Some(wire::PackChunkSpool::new_in(repo.heddle_dir())?)
        } else {
            None
        };
        let mut git_lane_repo = None;
        let mut git_pack_state = wire::GitPackChunkState::default();
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
                    if let Some(pack_spool) = pack_spool.as_mut() {
                        pack_spool.receive_chunk(
                            stream_kind == PackStreamKind::Index,
                            transfer.resume_offset,
                            transfer.chunk_index,
                            transfer.is_complete,
                            &chunk.data,
                            chunk.is_final_chunk,
                        )?;
                    } else {
                        wire::receive_pack_chunk(
                            &mut pack_state,
                            stream_kind == PackStreamKind::Index,
                            transfer.resume_offset,
                            transfer.chunk_index,
                            transfer.is_complete,
                            &chunk.data,
                            chunk.is_final_chunk,
                        )?;
                    }
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
                    wire::check_received_transfer_blob_size(
                        transfer.redactions_blob.len(),
                        wire::MAX_RECEIVED_REDACTIONS_BLOB_SIZE,
                        "redactions",
                    )?;
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
                    wire::check_received_transfer_blob_size(
                        transfer.state_visibility_blob.len(),
                        wire::MAX_RECEIVED_STATE_VISIBILITY_BLOB_SIZE,
                        "state-visibility",
                    )?;
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
                Some(pull_message::Body::GitLane(transfer)) => {
                    let decode_start = Instant::now();
                    profile.bytes_received = profile
                        .bytes_received
                        .saturating_add(git_lane_transfer_size(&transfer));
                    accept_git_lane_pull_transfer(
                        repo,
                        &mut git_lane_repo,
                        &mut git_pack_state,
                        transfer,
                    )?;
                    let decode_elapsed = decode_start.elapsed();
                    profile.store_receive_object += decode_elapsed;
                }
                Some(pull_message::Body::Complete(complete)) => {
                    profile.receive_and_apply = receive_start.elapsed();
                    git_pack_state.ensure_idle()?;
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
                            let store_start = Instant::now();
                            let installed_ids = if let Some(pack_spool) = pack_spool.as_mut() {
                                if !pack_spool.is_complete() {
                                    return Err(ProtocolError::InvalidState(
                                        "pull completed before native pack stream finished"
                                            .to_string(),
                                    ));
                                }
                                pack_spool.install_into(repo.store())?
                            } else {
                                if !pack_state.is_complete() {
                                    return Err(ProtocolError::InvalidState(
                                        "pull completed before native pack stream finished"
                                            .to_string(),
                                    ));
                                }
                                wire::install_received_pack(
                                    repo.store(),
                                    &pack_state.pack_data,
                                    &pack_state.index_data,
                                )?
                            };
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
                            repo.refs()
                                .set_thread(&ThreadName::from(local_thread), &state)?;
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
    info: wire::ObjectInfo,
) -> Result<PushMessage, ProtocolError> {
    let wire::ObjectId::Hash(blob) = info.id else {
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
            redactions_blob: bytes.into(),
        })),
    })
}

fn is_out_of_pack_transfer_object_type(obj_type: ObjectType) -> bool {
    matches!(
        obj_type,
        ObjectType::Redaction | ObjectType::StateVisibility
    )
}

fn native_pack_required_for_pull(want_full_closure: bool, wanted_types: &WantedTypes) -> bool {
    want_full_closure
        || wanted_types
            .values()
            .flatten()
            .copied()
            .any(wire::is_native_packable_object_type)
}

fn object_info_from_plan(object: &PlannedObject) -> ObjectInfo {
    ObjectInfo {
        id: object.id.clone(),
        obj_type: object.obj_type,
        size: 0,
        delta_base: None,
    }
}

fn to_proto_planned_object(object: &PlannedObject) -> ObjectDescriptor {
    object_descriptor_with_status(
        &object_info_from_plan(object),
        ObjectAvailabilityStatus::Present,
        "",
    )
}

fn descriptor_id_from_plan(object: &PlannedObject) -> (String, String) {
    let id = match &object.id {
        wire::ObjectId::Hash(hash) => hash.to_hex(),
        wire::ObjectId::ChangeId(change_id) => change_id.to_string_full(),
    };
    (id, object_type_name(object.obj_type).to_string())
}

fn record_wanted_type(wanted_types: &mut WantedTypes, pack_id: PackObjectId, obj_type: ObjectType) {
    let types = wanted_types.entry(pack_id).or_default();
    if !types.contains(&obj_type) {
        types.push(obj_type);
    }
}

fn wanted_packable_type(wanted_types: &WantedTypes, pack_id: &PackObjectId) -> Option<ObjectType> {
    wanted_types.get(pack_id).and_then(|types| {
        types
            .iter()
            .copied()
            .find(|obj_type| wire::is_native_packable_object_type(*obj_type))
    })
}

fn sidecar_push_message(
    repo: &Repository,
    info: wire::ObjectInfo,
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
    info: wire::ObjectInfo,
) -> Result<PushMessage, ProtocolError> {
    let wire::ObjectId::ChangeId(state) = info.id else {
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
                state_visibility_blob: bytes.into(),
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
            wire::ObjectId::Hash(hash) => PackObjectId::Hash(*hash),
            wire::ObjectId::ChangeId(change_id) => PackObjectId::ChangeId(*change_id),
        };
        let include = if request_full_closure {
            true
        } else {
            let has = wire::has_object(repo.store(), &info)?;
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
    if let Some(discussions_blob) = state.discussions.as_ref()
        && !repo.store().has_blob(discussions_blob)?
    {
        missing.push(*discussions_blob);
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
    if let Some(discussions_blob) = state.discussions.as_ref() {
        missing.push(*discussions_blob);
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

fn native_revision_address(change_id: ChangeId) -> String {
    RevisionAddress::heddle(change_id).to_string()
}

fn git_revision_address(commit_oid: &GitObjectId) -> String {
    RevisionAddress::git_commit(commit_oid.to_hex()).to_string()
}

fn build_git_lane_push_plan(
    repo: &Repository,
    local_state: ChangeId,
    target_thread: &str,
    chunk_size: usize,
) -> Result<GitLanePushPlan, ProtocolError> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Err(ProtocolError::InvalidState(
            "Git lane pushes require a git-overlay repository".to_string(),
        ));
    }
    let checkpoint = repo
        .latest_git_checkpoint_for_change(&local_state)
        .map_err(|err| ProtocolError::InvalidState(err.to_string()))?
        .ok_or_else(|| {
            ProtocolError::InvalidState(format!(
                "state {} has no Git checkpoint; run `heddle checkpoint` before pushing this git-overlay spool",
                local_state.short()
            ))
        })?;
    let git_repo = repo
        .git_overlay_sley_repository()
        .map_err(|err| ProtocolError::InvalidState(err.to_string()))?
        .ok_or_else(|| {
            ProtocolError::InvalidState("git-overlay repository has no Git store".to_string())
        })?;
    let commit_oid = GitObjectId::from_hex(git_repo.object_format(), &checkpoint.git_commit)
        .map_err(|err| {
            ProtocolError::InvalidState(format!(
                "checkpoint {} has invalid Git commit oid: {err}",
                checkpoint.git_commit
            ))
        })?;
    git_repo.read_commit(&commit_oid).map_err(|err| {
        ProtocolError::InvalidState(format!(
            "checkpoint {} is not a readable Git commit: {err}",
            checkpoint.git_commit
        ))
    })?;

    let pack = build_git_lane_pack_plan(&git_repo, commit_oid, chunk_size)?;
    let ref_update = git_ref_update_message(local_state, target_thread, commit_oid, &checkpoint)?;

    Ok(GitLanePushPlan {
        local_revision_address: git_revision_address(&commit_oid),
        pack,
        ref_update,
    })
}

fn build_git_lane_pack_plan(
    git_repo: &SleyRepository,
    root: GitObjectId,
    chunk_size: usize,
) -> Result<GitPackPushPlan, ProtocolError> {
    let objects = git_repo.objects();
    let mut sink = io::sink();
    let pack = sley::plumbing::sley_odb::write_reachable_pack_to_writer(
        objects.as_ref(),
        git_repo.object_format(),
        [root],
        &HashSet::new(),
        &mut sink,
    )
    .map_err(|err| {
        ProtocolError::InvalidState(format!(
            "plan reachable Git pack stream for {}: {err}",
            root.to_hex()
        ))
    })?
    .ok_or_else(|| {
        ProtocolError::InvalidState(format!(
            "checkpoint {} did not produce a reachable Git pack",
            root.to_hex()
        ))
    })?;
    if pack.pack_size > wire::MAX_RECEIVED_GIT_PACK_SIZE {
        return Err(ProtocolError::InvalidState(format!(
            "Git pack exceeds maximum transfer size of {} bytes",
            wire::MAX_RECEIVED_GIT_PACK_SIZE
        )));
    }
    let chunk_size = chunk_size.max(1) as u64;
    let chunk_count = pack.pack_size.div_ceil(chunk_size);
    if chunk_count > u32::MAX as u64 {
        return Err(ProtocolError::InvalidState(
            "Git pack chunk count exceeds u32".to_string(),
        ));
    }
    Ok(GitPackPushPlan {
        transfer_id: format!("git-pack:{}", root.to_hex()),
        pack_id: pack.checksum.as_bytes().to_vec(),
        pack_size: pack.pack_size,
        root,
        git_repo: git_repo.clone(),
    })
}

async fn send_git_pack_streaming_messages(
    tx: &mpsc::Sender<PushMessage>,
    pack: &GitPackPushPlan,
    chunk_size: usize,
) -> Result<(), ProtocolError> {
    let tx = tx.clone();
    let pack = pack.clone();
    tokio::task::spawn_blocking(move || stream_git_pack_messages_blocking(tx, pack, chunk_size))
        .await
        .map_err(|err| {
            ProtocolError::InvalidState(format!("Git pack streaming task failed: {err}"))
        })?
}

fn stream_git_pack_messages_blocking(
    tx: mpsc::Sender<PushMessage>,
    pack: GitPackPushPlan,
    chunk_size: usize,
) -> Result<(), ProtocolError> {
    let objects = pack.git_repo.objects();
    let mut writer = GitPackPushMessageWriter::new(
        tx,
        pack.transfer_id.clone(),
        pack.pack_id.clone(),
        pack.pack_size,
        chunk_size,
    );
    let summary = sley::plumbing::sley_odb::write_reachable_pack_to_writer(
        objects.as_ref(),
        pack.git_repo.object_format(),
        [pack.root],
        &HashSet::new(),
        &mut writer,
    )
    .map_err(|err| {
        ProtocolError::InvalidState(format!(
            "stream reachable Git pack for {}: {err}",
            pack.root.to_hex()
        ))
    })?
    .ok_or_else(|| {
        ProtocolError::InvalidState(format!(
            "checkpoint {} did not produce a reachable Git pack",
            pack.root.to_hex()
        ))
    })?;
    writer.finish()?;
    if summary.pack_size != pack.pack_size || summary.checksum.as_bytes() != pack.pack_id.as_slice()
    {
        return Err(ProtocolError::InvalidState(format!(
            "Git pack stream changed while sending {}; expected {} bytes/{}, got {} bytes/{}",
            pack.root.to_hex(),
            pack.pack_size,
            hex::encode(&pack.pack_id),
            summary.pack_size,
            summary.checksum.to_hex()
        )));
    }
    Ok(())
}

struct GitPackPushMessageWriter {
    tx: mpsc::Sender<PushMessage>,
    transfer_id: String,
    pack_id: Vec<u8>,
    pack_size: u64,
    chunk_size: usize,
    buffer: Vec<u8>,
    offset: u64,
    chunk_index: u32,
}

impl GitPackPushMessageWriter {
    fn new(
        tx: mpsc::Sender<PushMessage>,
        transfer_id: String,
        pack_id: Vec<u8>,
        pack_size: u64,
        chunk_size: usize,
    ) -> Self {
        let chunk_size = chunk_size.max(1);
        Self {
            tx,
            transfer_id,
            pack_id,
            pack_size,
            chunk_size,
            buffer: Vec::with_capacity(chunk_size),
            offset: 0,
            chunk_index: 0,
        }
    }

    fn send_buffer(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::take(&mut self.buffer);
        let next_offset = self.offset.checked_add(chunk.len() as u64).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Git pack offset overflow")
        })?;
        if next_offset > self.pack_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Git pack stream exceeded planned size {}; got at least {}",
                    self.pack_size, next_offset
                ),
            ));
        }
        let is_final_chunk = next_offset == self.pack_size;
        self.tx
            .blocking_send(git_lane_push_message(git_lane_transfer::Body::Pack(
                GitPackTransfer {
                    transfer_id: self.transfer_id.clone(),
                    offset: self.offset,
                    chunk_index: self.chunk_index,
                    is_final_chunk,
                    pack_size: self.pack_size,
                    pack_chunk: chunk.into(),
                    pack_id: self.pack_id.clone().into(),
                },
            )))
            .map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "push stream closed unexpectedly")
            })?;
        self.offset = next_offset;
        self.chunk_index = self.chunk_index.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Git pack chunk index overflow")
        })?;
        Ok(())
    }

    fn finish(mut self) -> Result<(), ProtocolError> {
        self.send_buffer().map_err(|err| {
            ProtocolError::InvalidState(format!("send final Git pack chunk: {err}"))
        })?;
        if self.offset != self.pack_size {
            return Err(ProtocolError::InvalidState(format!(
                "Git pack stream length mismatch: expected {}, got {}",
                self.pack_size, self.offset
            )));
        }
        Ok(())
    }
}

impl Write for GitPackPushMessageWriter {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let written = buf.len();
        while !buf.is_empty() {
            if self.offset == self.pack_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Git pack stream wrote past planned size {}", self.pack_size),
                ));
            }
            let capacity = self
                .chunk_size
                .checked_sub(self.buffer.len())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "Git pack chunk buffer overflow")
                })?;
            let take = capacity.min(buf.len());
            self.buffer.extend_from_slice(&buf[..take]);
            buf = &buf[take..];
            if self.buffer.len() == self.chunk_size {
                self.send_buffer()?;
            }
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn git_ref_update_message(
    local_state: ChangeId,
    target_thread: &str,
    commit_oid: GitObjectId,
    checkpoint: &GitCheckpointRecord,
) -> Result<PushMessage, ProtocolError> {
    let metadata_json = serde_json::json!({
        "source": "heddle-checkpoint",
        "summary": checkpoint.summary,
        "committed_at": checkpoint.committed_at,
    })
    .to_string();
    Ok(git_lane_push_message(git_lane_transfer::Body::RefUpdate(
        GitRefUpdateTransfer {
            name: format!("refs/heads/{target_thread}"),
            kind: GitRefKind::Branch as i32,
            target_oid: commit_oid.as_bytes().to_vec().into(),
            peeled_oid: Vec::new().into(),
            expected_missing: false,
            expected_target_oid: Vec::new().into(),
            checkpoint: Some(GitCheckpointTransfer {
                heddle_change_id: local_state.as_bytes().to_vec().into(),
                git_commit_oid: commit_oid.as_bytes().to_vec().into(),
                thread: target_thread.to_string(),
                metadata_json,
            }),
        },
    )))
}

fn apply_git_ref_expectation(
    message: &mut PushMessage,
    remote_revision_address: &str,
) -> Result<(), ProtocolError> {
    let expectation = parse_git_ref_expectation(remote_revision_address)?;
    let Some(push_message::Body::GitLane(GitLaneTransfer {
        body: Some(git_lane_transfer::Body::RefUpdate(update)),
    })) = message.body.as_mut()
    else {
        return Err(ProtocolError::InvalidState(
            "Git lane push plan missing ref update message".to_string(),
        ));
    };
    match &expectation {
        GitRefRemoteExpectation::Missing => {
            update.expected_missing = true;
            update.expected_target_oid = Vec::new().into();
        }
        GitRefRemoteExpectation::Value(oid) => {
            update.expected_missing = false;
            update.expected_target_oid = oid.clone().into();
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GitRefRemoteExpectation {
    Missing,
    Value(Vec<u8>),
}

fn parse_git_ref_expectation(
    remote_revision_address: &str,
) -> Result<GitRefRemoteExpectation, ProtocolError> {
    if remote_revision_address.is_empty() || remote_revision_address.starts_with("heddle:") {
        return Ok(GitRefRemoteExpectation::Missing);
    }
    let Some(hex_oid) = remote_revision_address.strip_prefix("git:") else {
        return Err(ProtocolError::InvalidState(format!(
            "server returned unsupported remote_revision_address {remote_revision_address:?}"
        )));
    };
    let oid = hex::decode(hex_oid).map_err(|err| {
        ProtocolError::InvalidState(format!(
            "server returned invalid Git remote_revision_address: {err}"
        ))
    })?;
    match oid.len() {
        20 | 32 => Ok(GitRefRemoteExpectation::Value(oid)),
        len => Err(ProtocolError::InvalidState(format!(
            "server returned Git remote_revision_address with {len} bytes; expected SHA-1 or SHA-256"
        ))),
    }
}

fn git_lane_push_message(body: git_lane_transfer::Body) -> PushMessage {
    PushMessage {
        body: Some(push_message::Body::GitLane(GitLaneTransfer {
            body: Some(body),
        })),
    }
}

fn git_lane_transfer_size(transfer: &GitLaneTransfer) -> usize {
    match transfer.body.as_ref() {
        Some(git_lane_transfer::Body::Pack(pack)) => pack.pack_chunk.len(),
        Some(git_lane_transfer::Body::RefUpdate(update)) => update
            .target_oid
            .len()
            .saturating_add(update.peeled_oid.len())
            .saturating_add(update.expected_target_oid.len())
            .saturating_add(
                update
                    .checkpoint
                    .as_ref()
                    .map(git_checkpoint_transfer_size)
                    .unwrap_or_default(),
            ),
        Some(git_lane_transfer::Body::Checkpoint(checkpoint)) => {
            git_checkpoint_transfer_size(checkpoint)
        }
        None => 0,
    }
}

fn git_checkpoint_transfer_size(checkpoint: &GitCheckpointTransfer) -> usize {
    checkpoint
        .heddle_change_id
        .len()
        .saturating_add(checkpoint.git_commit_oid.len())
        .saturating_add(checkpoint.thread.len())
        .saturating_add(checkpoint.metadata_json.len())
}

fn accept_git_lane_pull_transfer(
    repo: &Repository,
    git_repo: &mut Option<SleyRepository>,
    git_pack_state: &mut wire::GitPackChunkState,
    transfer: GitLaneTransfer,
) -> Result<(), ProtocolError> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Ok(());
    }
    match transfer.body {
        Some(git_lane_transfer::Body::Pack(pack)) => {
            accept_git_lane_pack(repo, git_repo, git_pack_state, pack)
        }
        Some(git_lane_transfer::Body::RefUpdate(update)) => {
            accept_git_lane_ref_update(repo, git_repo, update)
        }
        Some(git_lane_transfer::Body::Checkpoint(checkpoint)) => {
            record_git_lane_checkpoint(repo, git_lane_sley_repository(repo, git_repo)?, checkpoint)
        }
        None => Err(ProtocolError::InvalidState(
            "GitLaneTransfer body is required".to_string(),
        )),
    }
}

fn accept_git_lane_pack(
    repo: &Repository,
    git_repo: &mut Option<SleyRepository>,
    git_pack_state: &mut wire::GitPackChunkState,
    pack: GitPackTransfer,
) -> Result<(), ProtocolError> {
    let Some(pack_bytes) = git_pack_state.receive_chunk(
        &pack.transfer_id,
        pack.offset,
        pack.chunk_index,
        pack.is_final_chunk,
        pack.pack_size,
        &pack.pack_chunk,
    )?
    else {
        return Ok(());
    };
    let git_repo = git_lane_sley_repository(repo, git_repo)?;
    git_repo
        .objects_mut()
        .install_raw_pack(&pack_bytes)
        .map_err(|err| ProtocolError::InvalidState(format!("install Git pack: {err}")))?;
    Ok(())
}

fn accept_git_lane_ref_update(
    repo: &Repository,
    git_repo: &mut Option<SleyRepository>,
    update: GitRefUpdateTransfer,
) -> Result<(), ProtocolError> {
    let git_repo = git_lane_sley_repository(repo, git_repo)?;
    let target = git_oid_from_bytes(
        git_repo,
        "GitRefUpdateTransfer.target_oid",
        &update.target_oid,
    )?;
    git_repo.read_commit(&target).map_err(|err| {
        ProtocolError::InvalidState(format!(
            "Git ref {} target commit {} is not present after pack receive: {err}",
            update.name,
            target.to_hex()
        ))
    })?;
    let refs = git_repo.references();
    let mut tx = refs.transaction();
    tx.update_to(
        update.name.clone(),
        ReferenceTarget::Direct(target),
        RefPrecondition::Any,
        None,
    );
    tx.commit().map_err(|err| {
        ProtocolError::InvalidState(format!("update Git ref {}: {err}", update.name))
    })?;
    if let Some(checkpoint) = update.checkpoint {
        record_git_lane_checkpoint(repo, git_repo, checkpoint)?;
    }
    Ok(())
}

fn record_git_lane_checkpoint(
    repo: &Repository,
    git_repo: &SleyRepository,
    checkpoint: GitCheckpointTransfer,
) -> Result<(), ProtocolError> {
    let state = ChangeId::try_from_slice(&checkpoint.heddle_change_id)
        .map_err(|err| ProtocolError::InvalidState(err.to_string()))?;
    let commit_oid = git_oid_from_bytes(
        git_repo,
        "GitCheckpointTransfer.git_commit_oid",
        &checkpoint.git_commit_oid,
    )?;
    let commit_hex = commit_oid.to_hex();
    if repo
        .latest_git_checkpoint_for_change(&state)
        .map_err(|err| ProtocolError::InvalidState(err.to_string()))?
        .is_some_and(|record| record.git_commit == commit_hex)
    {
        return Ok(());
    }
    let summary = serde_json::from_str::<serde_json::Value>(&checkpoint.metadata_json)
        .ok()
        .and_then(|metadata| {
            metadata
                .get("summary")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "pulled Git checkpoint".to_string());
    repo.record_git_checkpoint(&state, commit_hex, summary)
        .map_err(|err| ProtocolError::InvalidState(err.to_string()))?;
    Ok(())
}

fn git_lane_sley_repository<'a>(
    repo: &Repository,
    git_repo: &'a mut Option<SleyRepository>,
) -> Result<&'a SleyRepository, ProtocolError> {
    if git_repo.is_none() {
        *git_repo = Some(
            repo.git_overlay_sley_repository()
                .map_err(|err| ProtocolError::InvalidState(err.to_string()))?
                .ok_or_else(|| {
                    ProtocolError::InvalidState(
                        "git-overlay repository has no Git store".to_string(),
                    )
                })?,
        );
    }
    match git_repo.as_ref() {
        Some(git_repo) => Ok(git_repo),
        None => Err(ProtocolError::InvalidState(
            "git-overlay repository has no Git store".to_string(),
        )),
    }
}

fn git_oid_from_bytes(
    git_repo: &SleyRepository,
    field: &str,
    bytes: &[u8],
) -> Result<GitObjectId, ProtocolError> {
    GitObjectId::from_raw(git_repo.object_format(), bytes)
        .map_err(|err| ProtocolError::InvalidState(format!("{field}: {err}")))
}

async fn send_native_pack_streaming_messages(
    tx: &mpsc::Sender<PushMessage>,
    repo: &Repository,
    objects: &[ObjectInfo],
    transfer_id: &str,
    chunk_size: usize,
    transport: &super::helpers::HostedTransportPolicy,
    transport_mode: TransportMode,
) -> Result<(), ProtocolError> {
    let object_count = u64::try_from(objects.len()).map_err(|_| {
        ProtocolError::InvalidState("native pack object count exceeds u64".to_string())
    })?;
    let mut writer = wire::NativePackStreamingWriter::new_in(repo.heddle_dir(), object_count)?;
    let mut pack_reader = wire::GrowingPackChunkReader::open(writer.pack_path(), chunk_size)?;
    let (loaded_tx, mut loaded_rx) = mpsc::channel::<(
        usize,
        Result<wire::ObjectData, ProtocolError>,
    )>(NATIVE_PACK_OBJECT_PREFETCH_LIMIT);
    let store = repo.store().clone();
    let object_plan = objects.to_vec();
    let loader = tokio::task::spawn_blocking(move || {
        load_native_pack_objects_parallel(store, object_plan, loaded_tx);
    });

    let mut next_index = 0usize;
    let mut pending = BTreeMap::new();
    while next_index < objects.len() {
        let (index, object) = loaded_rx.recv().await.ok_or_else(|| {
            ProtocolError::InvalidState(
                "native pack object loader stopped before sending all objects".to_string(),
            )
        })?;
        pending.insert(index, object);

        while let Some(object) = pending.remove(&next_index) {
            let object = object?;
            let should_drain = object.data.len() >= chunk_size
                || (next_index + 1) % NATIVE_PACK_DRAIN_OBJECT_INTERVAL == 0;
            writer.add_object_data(object)?;
            if should_drain {
                writer.flush_pack()?;
                drain_growing_native_pack_stream(
                    tx,
                    &mut pack_reader,
                    false,
                    PackStreamKind::Pack,
                    transfer_id,
                    transport,
                    transport_mode,
                )
                .await?;
            }
            next_index += 1;
        }
    }
    loader.await.map_err(|err| {
        ProtocolError::InvalidState(format!("native pack object loader task failed: {err}"))
    })?;

    let bundle = writer.finish()?;
    drain_growing_native_pack_stream(
        tx,
        &mut pack_reader,
        true,
        PackStreamKind::Pack,
        transfer_id,
        transport,
        transport_mode,
    )
    .await?;
    send_native_pack_file_stream(
        tx,
        &bundle.index_path,
        PackStreamKind::Index,
        transfer_id,
        chunk_size,
        transport,
        transport_mode,
    )
    .await
}

fn load_native_pack_objects_parallel(
    store: AnyStore,
    objects: Vec<ObjectInfo>,
    tx: mpsc::Sender<(usize, Result<wire::ObjectData, ProtocolError>)>,
) {
    if objects.is_empty() {
        return;
    }
    let worker_count = native_pack_object_load_worker_count(objects.len());
    let objects = Arc::new(objects);
    let next_index = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let store = store.clone();
            let objects = Arc::clone(&objects);
            let next_index = Arc::clone(&next_index);
            let tx = tx.clone();
            scope.spawn(move || {
                loop {
                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    let Some(info) = objects.get(index) else {
                        break;
                    };
                    let object = wire::load_object_data(&store, &info.id, info.obj_type);
                    if tx.blocking_send((index, object)).is_err() {
                        break;
                    }
                }
            });
        }
    });
}

fn native_pack_object_load_worker_count(object_count: usize) -> usize {
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    object_count
        .min(available)
        .min(NATIVE_PACK_OBJECT_LOAD_WORKER_LIMIT)
        .max(1)
}

async fn drain_growing_native_pack_stream(
    tx: &mpsc::Sender<PushMessage>,
    reader: &mut wire::GrowingPackChunkReader,
    final_stream: bool,
    stream_kind: PackStreamKind,
    transfer_id: &str,
    transport: &super::helpers::HostedTransportPolicy,
    transport_mode: TransportMode,
) -> Result<(), ProtocolError> {
    while let Some((offset, chunk_index, data, is_final_chunk)) =
        reader.next_available_chunk(final_stream)?
    {
        send_pack_chunk(
            tx,
            stream_kind,
            data,
            transfer_id,
            transport,
            transport_mode,
            chunk_index,
            offset,
            is_final_chunk,
        )
        .await?;
    }
    Ok(())
}

async fn send_native_pack_file_stream(
    tx: &mpsc::Sender<PushMessage>,
    path: &std::path::Path,
    stream_kind: PackStreamKind,
    transfer_id: &str,
    chunk_size: usize,
    transport: &super::helpers::HostedTransportPolicy,
    transport_mode: TransportMode,
) -> Result<(), ProtocolError> {
    let mut reader = wire::PackFileChunkReader::open(path, chunk_size)?;
    while let Some((offset, chunk_index, data, is_final_chunk)) = reader.next_chunk()? {
        send_pack_chunk(
            tx,
            stream_kind,
            data,
            transfer_id,
            transport,
            transport_mode,
            chunk_index,
            offset,
            is_final_chunk,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn send_pack_chunk(
    tx: &mpsc::Sender<PushMessage>,
    stream_kind: PackStreamKind,
    data: Vec<u8>,
    transfer_id: &str,
    transport: &super::helpers::HostedTransportPolicy,
    transport_mode: TransportMode,
    chunk_index: u32,
    offset: u64,
    is_final_chunk: bool,
) -> Result<(), ProtocolError> {
    let chunk_length = data.len().min(u32::MAX as usize) as u32;
    tx.send(PushMessage {
        body: Some(push_message::Body::Pack(PackChunk {
            stream_kind: stream_kind as i32,
            data: data.into(),
            transfer: Some(transport.transfer_checkpoint_with_mode(
                transfer_id,
                transport_mode,
                chunk_index,
                offset,
                is_final_chunk,
            )),
            chunk_length,
            is_final_chunk,
        })),
    })
    .await
    .map_err(|_| ProtocolError::InvalidState("push stream closed unexpectedly".to_string()))
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
        TransferCheckpoint, UpdateRefRequest, UpdateRefResponse, push_message,
        repo_sync_service_server::{RepoSyncService, RepoSyncServiceServer},
    };
    use objects::{
        object::{
            Attribution, Blob, ChangeId, ContentHash, Principal, Redaction, State, StateVisibility,
            StateVisibilityBlob, Tree, TreeEntry, VisibilityTier,
        },
        store::ObjectStore,
    };
    use tempfile::TempDir;
    use tonic::{Response, Status, transport::Server};
    use wire::{ObjectId, ObjectInfo};

    use super::*;
    use crate::grpc_hosted::helpers::{descriptor_id_from_info, to_proto_object_info};

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

    #[test]
    fn descriptor_id_from_info_matches_proto_encode_path() {
        let infos = [
            redaction_info(sample_blob()),
            state_info(ChangeId::from_bytes([3u8; 16])),
            state_visibility_info(ChangeId::from_bytes([9u8; 16])),
        ];
        for info in infos {
            assert_eq!(
                descriptor_id_from_info(&info),
                descriptor_id(&to_proto_object_info(&info)),
                "keying path must stay byte-identical to the throwaway-encode path",
            );
        }
    }

    #[test]
    fn git_lane_messages_pack_reachable_commit_graph_and_attach_checkpoint() {
        let dir = TempDir::new().expect("tempdir");
        let git = sley::Repository::init(dir.path()).expect("init git");
        let blob_oid = git.write_blob(b"hello\n").expect("write blob");
        let tree = sley::TreeObject {
            entries: vec![sley::plumbing::sley_object::TreeEntry {
                mode: 0o100644,
                name: sley::BString::from(b"hello.txt"),
                oid: blob_oid,
            }],
        };
        let tree_oid = git
            .write_raw_object(sley::GitObjectType::Tree, tree.write())
            .expect("write tree");
        let commit = sley::CommitObject {
            tree: tree_oid,
            parents: Vec::new(),
            author: b"Tester <test@example.com> 1700000000 +0000".to_vec(),
            committer: b"Tester <test@example.com> 1700000000 +0000".to_vec(),
            encoding: None,
            message: b"checkpoint\n".to_vec(),
        };
        let commit_oid = git
            .write_raw_object(sley::GitObjectType::Commit, commit.write())
            .expect("write commit");

        let pack = build_git_lane_pack_plan(&git, commit_oid, 64 * 1024)
            .expect("build git lane pack plan");
        assert_eq!(pack.pack_id.len(), git.object_format().raw_len());
        let (tx, mut rx) = mpsc::channel(8);
        stream_git_pack_messages_blocking(tx, pack.clone(), 64 * 1024)
            .expect("stream git lane pack");
        let mut pack_bytes = Vec::new();
        let mut chunks = Vec::new();
        while let Some(chunk) = rx.blocking_recv() {
            let Some(push_message::Body::GitLane(GitLaneTransfer {
                body: Some(git_lane_transfer::Body::Pack(pack)),
            })) = chunk.body
            else {
                panic!("expected git pack chunk");
            };
            pack_bytes.extend_from_slice(&pack.pack_chunk);
            chunks.push(pack);
        }
        assert!(!chunks.is_empty());
        assert!(chunks.last().expect("pack chunk").is_final_chunk);
        assert_eq!(pack_bytes.len() as u64, pack.pack_size);
        let indexed = sley::plumbing::sley_odb::index_raw_pack(&pack_bytes, git.object_format())
            .expect("generated pack should index");
        assert_eq!(indexed.pack_id.as_bytes(), pack.pack_id.as_slice());
        assert_eq!(indexed.objects.len(), 3);

        let state = ChangeId::from_bytes([9u8; 16]);
        let checkpoint = GitCheckpointRecord {
            change_id: state.to_string_full(),
            git_commit: commit_oid.to_hex(),
            summary: "checkpoint".to_string(),
            committed_at: "2026-06-25T00:00:00Z".to_string(),
        };
        let ref_message =
            git_ref_update_message(state, "main", commit_oid, &checkpoint).expect("ref update");
        let Some(push_message::Body::GitLane(GitLaneTransfer {
            body: Some(git_lane_transfer::Body::RefUpdate(update)),
        })) = ref_message.body
        else {
            panic!("expected git ref update message");
        };
        assert_eq!(update.name, "refs/heads/main");
        assert_eq!(update.kind, GitRefKind::Branch as i32);
        assert_eq!(update.target_oid.as_ref(), commit_oid.as_bytes());
        let checkpoint = update.checkpoint.expect("checkpoint");
        assert_eq!(checkpoint.heddle_change_id.as_ref(), state.as_bytes());
        assert_eq!(checkpoint.git_commit_oid.as_ref(), commit_oid.as_bytes());
        assert_eq!(checkpoint.thread, "main");
    }

    #[test]
    fn git_ref_expectation_marks_missing_when_remote_has_no_git_revision() {
        let state = ChangeId::from_bytes([9u8; 16]);
        let commit_oid = GitObjectId::from_hex(
            sley::ObjectFormat::Sha1,
            "0123456789abcdef0123456789abcdef01234567",
        )
        .expect("oid");
        let checkpoint = GitCheckpointRecord {
            change_id: state.to_string_full(),
            git_commit: commit_oid.to_hex(),
            summary: "checkpoint".to_string(),
            committed_at: "2026-06-25T00:00:00Z".to_string(),
        };
        let mut message =
            git_ref_update_message(state, "main", commit_oid, &checkpoint).expect("ref update");

        apply_git_ref_expectation(&mut message, "").expect("missing expectation");
        let update = git_ref_update_from_message(&message);
        assert!(update.expected_missing);
        assert!(update.expected_target_oid.is_empty());
    }

    #[test]
    fn git_ref_expectation_uses_remote_git_revision_oid() {
        let state = ChangeId::from_bytes([9u8; 16]);
        let commit_oid = GitObjectId::from_hex(
            sley::ObjectFormat::Sha1,
            "0123456789abcdef0123456789abcdef01234567",
        )
        .expect("oid");
        let remote_oid = "89abcdef012345670123456789abcdef01234567";
        let checkpoint = GitCheckpointRecord {
            change_id: state.to_string_full(),
            git_commit: commit_oid.to_hex(),
            summary: "checkpoint".to_string(),
            committed_at: "2026-06-25T00:00:00Z".to_string(),
        };
        let mut message =
            git_ref_update_message(state, "main", commit_oid, &checkpoint).expect("ref update");

        apply_git_ref_expectation(&mut message, &format!("git:{remote_oid}"))
            .expect("git expectation");
        let update = git_ref_update_from_message(&message);
        assert!(!update.expected_missing);
        assert_eq!(hex::encode(update.expected_target_oid.as_ref()), remote_oid);
    }

    #[test]
    fn git_pack_stream_writer_emits_ordered_chunks() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut writer =
            GitPackPushMessageWriter::new(tx, "git-pack:test".to_string(), vec![0x42; 20], 10, 4);
        writer.write_all(b"abcdefghij").expect("write pack bytes");
        writer.finish().expect("finish pack stream");

        let mut chunks = Vec::new();
        while let Some(message) = rx.blocking_recv() {
            let Some(push_message::Body::GitLane(GitLaneTransfer {
                body: Some(git_lane_transfer::Body::Pack(pack)),
            })) = message.body
            else {
                panic!("expected git pack chunk");
            };
            chunks.push(pack);
        }

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].offset, 0);
        assert_eq!(chunks[0].chunk_index, 0);
        assert!(!chunks[0].is_final_chunk);
        assert_eq!(chunks[0].pack_chunk.as_ref(), b"abcd");
        assert_eq!(chunks[1].offset, 4);
        assert_eq!(chunks[1].chunk_index, 1);
        assert!(!chunks[1].is_final_chunk);
        assert_eq!(chunks[1].pack_chunk.as_ref(), b"efgh");
        assert_eq!(chunks[2].offset, 8);
        assert_eq!(chunks[2].chunk_index, 2);
        assert!(chunks[2].is_final_chunk);
        assert_eq!(chunks[2].pack_chunk.as_ref(), b"ij");
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.pack_id.as_ref() == &[0x42; 20][..])
        );
    }

    fn git_ref_update_from_message(message: &PushMessage) -> &GitRefUpdateTransfer {
        let Some(push_message::Body::GitLane(GitLaneTransfer {
            body: Some(git_lane_transfer::Body::RefUpdate(update)),
        })) = message.body.as_ref()
        else {
            panic!("expected git ref update message");
        };
        update
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
        for obj_type in wire::native_pack_excluded_object_types() {
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

        let redaction_only =
            HashMap::from([(PackObjectId::Hash(blob), vec![ObjectType::Redaction])]);
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
        assert_eq!(
            wanted.as_slice(),
            &[ObjectType::State, ObjectType::StateVisibility]
        );
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
            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )))
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
                        remote_revision_address: native_revision_address(state),
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
                            state_visibility_blob: state_visibility_blob.into(),
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
                        new_revision_address: native_revision_address(state),
                    })),
                };
                let _ = tx.send(Ok(complete)).await;
            });

            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )))
        }
    }

    async fn connect_sidecar_only_service(
        service: SidecarOnlyPullService,
    ) -> Option<(HostedGrpcClient, tokio::task::JoinHandle<()>)> {
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(listener) => listener,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping hosted sync local gRPC test: TCP bind denied: {err}");
                return None;
            }
            Err(err) => panic!("bind test server: {err}"),
        };
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
        Some((client, handle))
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
        let Some((mut client, server)) = connect_sidecar_only_service(SidecarOnlyPullService {
            state: state_id,
            state_visibility_blob,
        })
        .await
        else {
            return;
        };

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

    // A sidecar blob larger than tonic's 4 MiB default decode limit but well
    // under the 64 MiB receive cap must still decode + install: the raised
    // `max_decoding_message_size` is the bound, and it isn't set too tight.
    #[tokio::test]
    async fn legitimate_large_sidecar_blob_decodes_and_installs() {
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

        // ~8 MiB blob: above tonic's 4 MiB default (which would reject at
        // decode without the raised limit), below the 64 MiB sidecar cap.
        let mut record = sample_state_visibility(state_id);
        record.tier = VisibilityTier::Restricted {
            scope_label: "x".repeat(8 * 1024 * 1024),
        };
        let state_visibility_blob = StateVisibilityBlob::new(vec![record])
            .encode()
            .expect("encode large state visibility blob");
        assert!(
            state_visibility_blob.len() > 4 * 1024 * 1024,
            "blob must exceed tonic's 4 MiB default to exercise the raised decode limit"
        );
        assert!(
            (state_visibility_blob.len() as u64) <= wire::MAX_RECEIVED_STATE_VISIBILITY_BLOB_SIZE,
            "blob must stay within the legitimate sidecar receive cap"
        );

        let Some((mut client, server)) = connect_sidecar_only_service(SidecarOnlyPullService {
            state: state_id,
            state_visibility_blob,
        })
        .await
        else {
            return;
        };

        let exchange = tokio::time::timeout(
            Duration::from_secs(30),
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
        .expect("large-sidecar pull must not hang")
        .expect("large but legitimate sidecar pull succeeds");
        server.abort();

        assert!(exchange.result.success);
        assert!(
            repo.get_state_visibility_for_state(&state_id)
                .expect("load accepted sidecar")
                .has_record(),
            "pull must accept a legitimately-large StateVisibility sidecar"
        );
    }

    // A sidecar blob beyond the pull-stream decode limit must be rejected at
    // the gRPC decode boundary — before its `Vec<u8>` is materialized — not by
    // the cheaper post-decode `check_received_transfer_blob_size` guard.
    #[tokio::test]
    async fn oversized_sidecar_blob_rejected_at_grpc_decode_boundary() {
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

        // One byte past the decode limit. Content is irrelevant: decode is
        // refused before the blob is ever handed to the accept path.
        let oversized = vec![0u8; wire::MAX_PULL_DECODE_MESSAGE_SIZE + 1];
        let Some((mut client, server)) = connect_sidecar_only_service(SidecarOnlyPullService {
            state: state_id,
            state_visibility_blob: oversized,
        })
        .await
        else {
            return;
        };

        let result = tokio::time::timeout(
            Duration::from_secs(30),
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
        .expect("oversized-sidecar pull must not hang");
        server.abort();

        let err = match result {
            Err(err) => err,
            Ok(_) => panic!("oversized sidecar PullMessage must be rejected at decode"),
        };
        let message = err.to_string();
        assert!(
            !message.contains("exceeds receive size limit"),
            "rejection must come from the decode-size limit, before the post-decode check: {message}"
        );
        assert!(
            repo.get_state_visibility_for_state(&state_id)
                .expect("load sidecar")
                .latest()
                .expect("resolve visibility")
                .is_none(),
            "an oversized sidecar must never be installed"
        );
    }

    #[derive(Clone)]
    struct StateAndVisibilityPullService {
        state: ChangeId,
        pack_bundle: wire::NativePackBundle,
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
            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )))
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
                        remote_revision_address: native_revision_address(state),
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
                        && want
                            .objects
                            .iter()
                            .any(|object| object.object_type == "state")
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

                for message in
                    encode_pull_native_pack_messages(&pack_bundle, "state-and-visibility-test", 16)
                {
                    if tx.send(Ok(message)).await.is_err() {
                        return;
                    }
                }

                let transfer = PullMessage {
                    body: Some(pull_message::Body::StateVisibility(
                        StateVisibilityTransfer {
                            state_id: state.to_string_full(),
                            state_visibility_blob: state_visibility_blob.into(),
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
                        new_revision_address: native_revision_address(state),
                    })),
                };
                let _ = tx.send(Ok(complete)).await;
            });

            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )))
        }
    }

    fn encode_pull_native_pack_messages(
        bundle: &wire::NativePackBundle,
        transfer_id: &str,
        chunk_size: usize,
    ) -> Vec<PullMessage> {
        let mut messages = Vec::new();
        let chunk_size = chunk_size.max(1);

        let pack_total_chunks = wire::chunk_count(bundle.pack_data.len(), chunk_size);
        for chunk_index in 0..pack_total_chunks.max(1) {
            let Some((start, len)) =
                wire::chunk_bounds(bundle.pack_data.len(), chunk_size, chunk_index)
            else {
                break;
            };
            messages.push(PullMessage {
                body: Some(pull_message::Body::Pack(PackChunk {
                    stream_kind: PackStreamKind::Pack as i32,
                    data: bundle.pack_data[start..start + len].to_vec().into(),
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

        let index_total_chunks = wire::chunk_count(bundle.index_data.len(), chunk_size);
        for chunk_index in 0..index_total_chunks.max(1) {
            let Some((start, len)) =
                wire::chunk_bounds(bundle.index_data.len(), chunk_size, chunk_index)
            else {
                break;
            };
            messages.push(PullMessage {
                body: Some(pull_message::Body::Pack(PackChunk {
                    stream_kind: PackStreamKind::Index as i32,
                    data: bundle.index_data[start..start + len].to_vec().into(),
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
    ) -> Option<(HostedGrpcClient, tokio::task::JoinHandle<()>)> {
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
            Ok(listener) => listener,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("skipping hosted sync local gRPC test: TCP bind denied: {err}");
                return None;
            }
            Err(err) => panic!("bind test server: {err}"),
        };
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
        Some((client, handle))
    }

    #[tokio::test]
    async fn state_and_visibility_same_change_id_pull_requests_pack_and_sidecar() {
        let (_source_dir, source_repo) = temp_repo();
        let (_target_dir, target_repo) = temp_repo();
        let tree_hash = source_repo
            .store()
            .put_tree(&Tree::new())
            .expect("put tree");
        let state = State::new_snapshot(
            tree_hash,
            vec![],
            Attribution::human(Principal {
                name: "Grace Hopper".into(),
                email: "grace@example.com".into(),
            }),
        );
        let state_id = state.change_id;
        source_repo
            .store()
            .put_state(&state)
            .expect("put source state");
        let state_visibility_blob =
            StateVisibilityBlob::new(vec![sample_state_visibility(state_id)])
                .encode()
                .expect("encode state visibility blob");
        source_repo
            .accept_wire_state_visibility(state_id, &state_visibility_blob)
            .expect("put source state visibility");
        let pack_bundle = wire::build_native_pack(source_repo.store(), &[state_info(state_id)])
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

        let Some((mut client, server)) =
            connect_state_and_visibility_service(StateAndVisibilityPullService {
                state: state_id,
                pack_bundle,
                state_visibility_blob,
            })
            .await
        else {
            return;
        };

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
