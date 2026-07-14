use std::{
    collections::{BTreeMap, HashMap},
    io::{self, Seek, SeekFrom, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use grpc::heddle::api::v1alpha1::{
    GetBlobRequest, GitCheckpointTransfer, GitLaneTransfer, GitObjectAlgorithm,
    GitObjectId as ProtoGitObjectId, GitPackTransfer, GitRefKind as GrpcGitRefKind,
    GitRefUpdateTransfer, ListRefsRequest, ObjectAvailabilityStatus, ObjectDescriptor, PackChunk,
    PackStreamKind, PartialFetchStatus, PullClientFrame, PullRequest, PullServerFrame,
    PushClientFrame, PushRequest, PushServerFrame, RedactionTransfer, StateVisibilityTransfer,
    StreamOpeningProof, ThreadConfidenceSummary, ThreadIntegrationPolicy, ThreadMetadata,
    ThreadVerificationSummary, TransportMode, UpdateRefRequest, WantObjects, git_lane_transfer,
    pull_client_frame, pull_server_frame, push_client_frame, push_server_frame,
};
use objects::{
    Progress,
    object::{ContentHash, MarkerName, StateId, ThreadName},
    store::{AnyStore, ObjectStore, PackObjectId},
};
use repo::{
    GitRefKind as ClassifiedGitRefKind, GitRefName, Repository, RepositoryCapability,
    RevisionAddress, SyncedThreadMetadata, ThreadManager,
};
use sley::{
    ObjectId as GitObjectId, RefPrecondition, ReferenceTarget, Repository as SleyRepository,
};
use tempfile::NamedTempFile;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use wire::{
    GitLaneTransferIntent, ObjectInfo, ObjectType, ObjectTypeBucket, PlannedObject, ProtocolError,
    PullComplete, PushComplete, RefEntry, RefUpdated, RepositoryTransferPlan,
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
    target_state: Option<StateId>,
    materialization: PullMaterialization,
}

struct PullWantPlan {
    wants: Vec<ObjectDescriptor>,
    transfer_plan: RepositoryTransferPlan<ObjectInfo>,
    wanted_types: WantedTypes,
    want_full_closure: bool,
}

type WantedTypes = HashMap<PackObjectId, Vec<ObjectType>>;

struct GitLanePushPlan {
    local_revision_address: String,
    /// `None` when want-only packing found nothing new to send: every object
    /// reachable from the pushed refs is already present on the server (the
    /// server's ref tips, learned from `ListRefs`, cover the full closure).
    /// This is the pure ref-move case (e.g. pushing `pr/N` right after `main`
    /// when it points at an already-present commit) — the client streams only
    /// the ref updates below, no pack.
    pack: Option<GitPackPushPlan>,
    /// The N ref updates streamed after the single multi-root pack, one per
    /// direct git-overlay ref (Branch/Tag/Note/Other). Every entry carries
    /// `checkpoint: None` — the discriminator the weft server uses to admit
    /// checkpoint-less multi-ref pushes. Per-ref compare-and-set expectations
    /// are pre-applied from the server `ListRefs` response.
    ref_updates: Vec<PushClientFrame>,
}

#[derive(Clone)]
struct GitPackPushPlan {
    transfer_id: String,
    pack_id: Vec<u8>,
    pack_size: u64,
    /// Root oids the pack is built from. The native path has exactly one
    /// root (the checkpoint commit); the mirror path has one per resolved
    /// ref target. The reachable-pack plan packs the transitive closure of
    /// every root into a single pack.
    #[cfg_attr(not(test), allow(dead_code))]
    roots: Vec<GitObjectId>,
    /// Reachable pack bytes written once during planning; streamed on the wire
    /// without a second ODB traversal. Auto-deleted when the last clone drops.
    pack_file: Arc<Mutex<NamedTempFile>>,
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
    pub state_attachments: usize,
}

#[derive(Debug, Clone)]
pub struct HostedRefEntry {
    pub name: String,
    pub state_id: StateId,
    pub is_thread: bool,
    pub revision_address: String,
}

impl PullObjectMix {
    fn record(&mut self, obj_type: ObjectType) {
        match obj_type.bucket() {
            ObjectTypeBucket::Blob => self.blobs += 1,
            ObjectTypeBucket::Tree => self.trees += 1,
            ObjectTypeBucket::State => self.states += 1,
            ObjectTypeBucket::Action => self.actions += 1,
            ObjectTypeBucket::Redaction => self.redactions += 1,
            ObjectTypeBucket::StateVisibility => self.state_visibilities += 1,
            ObjectTypeBucket::StateAttachment => self.state_attachments += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.blobs
            + self.trees
            + self.states
            + self.actions
            + self.redactions
            + self.state_visibilities
            + self.state_attachments
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
    fn stream_opening_proof(
        &self,
        stream_id: &str,
        route: &str,
        repository: &str,
        resume_cursor: &str,
    ) -> Result<StreamOpeningProof, ProtocolError> {
        use crypto::Signer as _;

        let signer = self.device_signer()?.ok_or_else(|| {
            ProtocolError::AuthenticationFailed(
                "hosted sync requires a stable device signing identity".to_string(),
            )
        })?;
        let identity = format!("ed25519:{}", hex::encode(signer.public_key()));
        let capability_context = Vec::new();
        let canonical = grpc::signing::stream_open_bytes(
            &identity,
            stream_id,
            route,
            repository,
            resume_cursor,
            &capability_context,
        );
        let signature = signer
            .sign(&canonical)
            .map_err(|err| ProtocolError::AuthenticationFailed(err.to_string()))?;
        Ok(StreamOpeningProof {
            stream_id: stream_id.to_string(),
            route: route.to_string(),
            repository: super::helpers::repository_ref(repository),
            resume_cursor: resume_cursor.to_string(),
            capability_context,
            nonce: Vec::new(),
            signature,
        })
    }

    pub async fn list_refs(&mut self, repo_path: &str) -> Result<Vec<RefEntry>, ProtocolError> {
        Ok(self
            .list_refs_with_revision_addresses(repo_path)
            .await?
            .into_iter()
            .map(|entry| RefEntry {
                name: entry.name,
                state_id: entry.state_id,
                is_thread: entry.is_thread,
            })
            .collect())
    }

    pub async fn list_refs_with_revision_addresses(
        &mut self,
        repo_path: &str,
    ) -> Result<Vec<HostedRefEntry>, ProtocolError> {
        let mut request = Request::new(ListRefsRequest {
            repo_path: super::helpers::repository_ref(repo_path),
        });
        self.apply_signed_auth(
            &mut request,
            "/heddle.api.v1alpha1.RepoSyncService/ListRefs",
        )?;
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
                    state_id: super::helpers::parse_proto_state_id(entry.state_id)?.ok_or_else(
                        || ProtocolError::InvalidState("ref is missing its state ID".to_string()),
                    )?,
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
        old_value: Option<StateId>,
        new_value: StateId,
        force: bool,
        thread_metadata: Option<&SyncedThreadMetadata>,
    ) -> Result<RefUpdated, ProtocolError> {
        let mut request = Request::new(UpdateRefRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            name: name.to_string(),
            is_thread,
            force,
            old_value: old_value
                .map(|value| value.to_string_full())
                .unwrap_or_default(),
            new_value: new_value.to_string_full(),
            thread_metadata: thread_metadata.map(to_proto_thread_metadata),
            old_revision_address: old_value
                .map(|value| RevisionAddress::heddle(value).to_string())
                .unwrap_or_default(),
            new_revision_address: RevisionAddress::heddle(new_value).to_string(),
            client_operation_id: String::new(),
        });
        self.apply_signed_auth(
            &mut request,
            "/heddle.api.v1alpha1.RepoSyncService/UpdateRef",
        )?;
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
                    StateId::parse(&response.old_value)
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
        local_state: StateId,
        target_thread: &str,
        force: bool,
    ) -> Result<PushComplete, ProtocolError> {
        self.push_with_revision(
            repo,
            repo_path,
            local_state,
            target_thread,
            force,
            RevisionAddress::heddle(local_state).to_string(),
            None,
            &Progress::null(),
        )
        .await
    }

    /// Push ALL git-overlay refs (every branch, tag, note, and other ref)
    /// in one shot: a single multi-root pack followed by N checkpoint-less
    /// ref updates (git-mirror mode). This is the DEFAULT hosted git-overlay
    /// push path (#846) — the git format is shipped straight through weft's
    /// git lane with no native conversion. Native heddle conversion stays
    /// opt-in via `heddle adopt`.
    ///
    /// Per-ref remote expectations are read from the server `ListRefs`
    /// response so each ref update carries the compare-and-set precondition
    /// the server currently holds.
    ///
    /// `progress` drives the live push line (packing → uploading bytes →
    /// writing N refs); pass [`Progress::null`] for machine-readable / non-TTY
    /// callers.
    pub async fn push_git_overlay_mirror(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        local_state: StateId,
        target_thread: &str,
        force: bool,
        progress: &Progress,
    ) -> Result<PushComplete, ProtocolError> {
        progress.set_phase("packing refs");
        let remote_ref_expectations = self.git_mirror_ref_expectations(repo_path).await?;
        let git_lane = build_git_mirror_push_plan(
            repo,
            self.transport.chunk_size.max(1),
            &remote_ref_expectations,
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
            progress,
        )
        .await
    }

    /// Fetch the server's current ref → git-revision-address map so the
    /// mirror plan can attach per-ref compare-and-set expectations. Refs the
    /// server does not know about are treated as expected-missing (create).
    async fn git_mirror_ref_expectations(
        &mut self,
        repo_path: &str,
    ) -> Result<HashMap<String, GitRefRemoteExpectation>, ProtocolError> {
        let remote_refs = self.list_refs_with_revision_addresses(repo_path).await?;
        let mut expectations = HashMap::with_capacity(remote_refs.len());
        for entry in remote_refs {
            let expectation = parse_git_ref_expectation(&entry.revision_address)?;
            expectations.insert(entry.name, expectation);
        }
        Ok(expectations)
    }

    #[allow(clippy::too_many_arguments)]
    async fn push_with_revision(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        local_state: StateId,
        target_thread: &str,
        force: bool,
        local_revision_address: String,
        git_lane: Option<GitLanePushPlan>,
        progress: &Progress,
    ) -> Result<PushComplete, ProtocolError> {
        let _ = self.transport.chunk_size;
        let _ = self.transport.resume_attempts;
        // TODO: Gate hosted Git-lane transfer planning on the Sley reachable-pack
        // facade. Keep this as ExistingImplementation until Sley can return the
        // exact pack identity and stream from one boundary; do not add a
        // Heddle-local reachable-pack planner or wire variant here.
        let git_lane_intent = if git_lane.is_some() {
            GitLaneTransferIntent::ExistingImplementation
        } else {
            GitLaneTransferIntent::HeddleObjectsOnly
        };
        let closure = wire::enumerate_state_closure_transfer_with_options(
            repo.store(),
            local_state,
            wire::StateClosureOptions::default(),
            PUSH_FULL_DESCRIPTOR_OBJECT_THRESHOLD,
        )?;
        let object_plan =
            RepositoryTransferPlan::from_planned_objects(closure.planned_objects, git_lane_intent);
        let full_objects = closure.full_objects;
        let object_count = full_objects
            .as_ref()
            .map_or(object_plan.stats.total_objects, std::vec::Vec::len);
        let transfer_id = push_transfer_id(repo_path, local_state, target_thread);
        let transport_mode = preferred_transport_mode(&self.transport, object_count);
        let thread_metadata = load_thread_metadata(repo, target_thread, local_state)?;
        let request_message = PushClientFrame {
            frame: Some(push_client_frame::Frame::Request(Box::new(PushRequest {
                repo_path: super::helpers::repository_ref(repo_path),
                local_state: super::helpers::proto_state_id(local_state),
                target_thread: target_thread.to_string(),
                create_thread: true,
                force,
                objects: full_objects.as_ref().map_or_else(
                    || {
                        object_plan
                            .partitions
                            .iter()
                            .map(to_proto_planned_object)
                            .collect()
                    },
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
            }))),
            client_operation_id: transfer_id.clone(),
        };

        let (tx, rx) = mpsc::channel(self.transport.max_inflight_objects.max(4));
        tx.send(PushClientFrame {
            frame: Some(push_client_frame::Frame::Open(self.stream_opening_proof(
                &transfer_id,
                "/heddle.api.v1alpha1.RepoSyncService/Push",
                repo_path,
                "",
            )?)),
            client_operation_id: transfer_id.clone(),
        })
        .await
        .map_err(|_| ProtocolError::InvalidState("failed to open push stream".to_string()))?;
        tx.send(request_message).await.map_err(|_| {
            ProtocolError::InvalidState("failed to initialize push stream".to_string())
        })?;
        let mut request = Request::new(ReceiverStream::new(rx));
        self.apply_auth(&mut request, "/heddle.api.v1alpha1.RepoSyncService/Push")?;
        let mut response = self
            .inner
            .push(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();

        let ready = match response.message().await.map_err(status_to_protocol_error)? {
            Some(PushServerFrame {
                frame: Some(push_server_frame::Frame::Ready(ready)),
            }) => ready,
            _ => {
                return Err(ProtocolError::InvalidState(
                    "expected PushReady from gRPC server".to_string(),
                ));
            }
        };
        let object_index = match full_objects {
            Some(objects) => objects
                .into_iter()
                .map(|info| (descriptor_id_from_info(&info), info))
                .collect::<HashMap<_, _>>(),
            None => object_plan
                .partitions
                .iter()
                .map(|object| {
                    (
                        descriptor_id_from_plan(object),
                        object_info_from_plan(object),
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

        let wanted_plan = RepositoryTransferPlan::from_object_infos(wanted_infos, git_lane_intent);

        if !wanted_plan.partitions.packable_objects.is_empty() {
            send_native_pack_streaming_messages(
                &tx,
                repo,
                &wanted_plan.partitions.packable_objects,
                &transfer_id,
                self.transport.chunk_size.max(1),
                &self.transport,
                ready_transport_mode,
            )
            .await?;
        }

        for info in wanted_plan.partitions.sidecar_objects {
            let message = sidecar_push_message(repo, info)?;
            tx.send(message).await.map_err(|_| {
                ProtocolError::InvalidState("push stream closed unexpectedly".to_string())
            })?;
        }

        if let Some(git_lane) = git_lane {
            // One multi-root pack (live "uploading" progress), then N
            // checkpoint-less ref updates (git-mirror mode). When want-only
            // packing found nothing new (the server already holds every pushed
            // object), `pack` is `None`: skip the pack stream and send only the
            // ref updates — the near-empty ref-move fast path (heddle#968).
            if let Some(pack) = git_lane.pack.as_ref() {
                send_git_pack_streaming_messages(
                    &tx,
                    pack,
                    self.transport.chunk_size.max(1),
                    progress,
                )
                .await?;
            } else {
                progress.set_phase("no new objects to pack");
            }
            progress.set_phase(format!("writing {} refs", git_lane.ref_updates.len()));
            for ref_update in git_lane.ref_updates {
                tx.send(ref_update).await.map_err(|_| {
                    ProtocolError::InvalidState("push stream closed unexpectedly".to_string())
                })?;
            }
        }
        drop(tx);

        let result = match response.message().await.map_err(status_to_protocol_error)? {
            Some(PushServerFrame {
                frame: Some(push_server_frame::Frame::Complete(complete)),
            }) => PushComplete {
                success: complete.success,
                new_state: super::helpers::parse_proto_state_id(complete.new_state)?,
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
        target_state: StateId,
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
        target_state: StateId,
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
            repo_path: super::helpers::repository_ref(repo_path),
            r#ref: reference.to_string(),
            path: path.to_string(),
        });
        self.apply_signed_auth(
            &mut request,
            "/heddle.api.v1alpha1.RepositoryService/GetBlob",
        )?;
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
        target_state: StateId,
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
        // Whether the head comes from an explicit `--local-thread` or is
        // inferred from the bare remote thread, it is advertised as
        // `exclude_states` ONLY when its full object closure is provably
        // present locally. The server trusts `exclude_states` blindly and
        // prunes the advertised closure — so advertising a head whose closure
        // we lack (a partial/lazy clone, an interrupted prior pull) would make
        // the server omit those objects and silently leave us with an
        // incomplete repo. Both branches therefore share the same completeness
        // gate; when it refuses, we fall back to the correct (slower)
        // empty-exclude full pull.
        let advertised_head = if let Some(local_thread) = options.local_thread {
            locally_complete_local_thread_head(repo, local_thread, options.target_state)?
        } else {
            locally_complete_pull_head(repo, remote_thread, options.target_state)?
        };
        if let Some(head) = advertised_head {
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
        let request_message = PullClientFrame {
            frame: Some(pull_client_frame::Frame::Request(PullRequest {
                repo_path: super::helpers::repository_ref(repo_path),
                remote_thread: remote_thread.to_string(),
                local_thread: options.local_thread.unwrap_or_default().to_string(),
                target_state: options
                    .target_state
                    .and_then(super::helpers::proto_state_id),
                depth: options.depth.unwrap_or_default(),
                exclude_states: exclude_states
                    .iter()
                    .copied()
                    .filter_map(super::helpers::proto_state_id)
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
                    .map(|state| RevisionAddress::heddle(state).to_string())
                    .unwrap_or_default(),
            })),
        };

        let (tx, rx) = mpsc::channel(self.transport.max_inflight_objects.max(4));
        tx.send(PullClientFrame {
            frame: Some(pull_client_frame::Frame::Open(self.stream_opening_proof(
                &transfer_id,
                "/heddle.api.v1alpha1.RepoSyncService/Pull",
                repo_path,
                "",
            )?)),
        })
        .await
        .map_err(|_| ProtocolError::InvalidState("failed to open pull stream".to_string()))?;
        tx.send(request_message).await.map_err(|_| {
            ProtocolError::InvalidState("failed to initialize pull stream".to_string())
        })?;
        let mut request = Request::new(ReceiverStream::new(rx));
        self.apply_auth(&mut request, "/heddle.api.v1alpha1.RepoSyncService/Pull")?;
        let mut response = self
            .inner
            .pull(request)
            .await
            .map_err(status_to_protocol_error)?
            .into_inner();

        let ready = match response.message().await.map_err(status_to_protocol_error)? {
            Some(PullServerFrame {
                frame: Some(pull_server_frame::Frame::Ready(ready)),
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
        let remote_state = super::helpers::parse_proto_state_id(ready.remote_state)?
            .ok_or_else(|| ProtocolError::InvalidState("missing remote state".to_string()))?;
        let advertised_object_count = ready.objects_to_fetch.len();
        let PullWantPlan {
            wants,
            transfer_plan,
            wanted_types,
            want_full_closure,
        } = plan_pull_wants(
            repo,
            &remote_state,
            ready.full_closure_available,
            ready.objects_to_fetch,
            allow_partial_fetch,
        )?;
        let native_pack_required = native_pack_required_for_pull(want_full_closure, &transfer_plan);

        tx.send(PullClientFrame {
            frame: Some(pull_client_frame::Frame::Want(WantObjects {
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
        let mut git_pack_state = GitPackPullInstallState::default();
        let mut received = 0usize;
        while let Some(message) = response.message().await.map_err(status_to_protocol_error)? {
            match message.frame {
                Some(pull_server_frame::Frame::Pack(chunk)) => {
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
                Some(pull_server_frame::Frame::Redaction(transfer)) => {
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
                Some(pull_server_frame::Frame::StateVisibility(transfer)) => {
                    wire::check_received_transfer_blob_size(
                        transfer.state_visibility_blob.len(),
                        wire::MAX_RECEIVED_STATE_VISIBILITY_BLOB_SIZE,
                        "state-visibility",
                    )?;
                    profile.bytes_received = profile
                        .bytes_received
                        .saturating_add(transfer.state_visibility_blob.len());
                    profile.object_mix.record(ObjectType::StateVisibility);
                    let state = transfer
                        .state_id
                        .as_ref()
                        .ok_or_else(|| {
                            ProtocolError::InvalidState(
                                "StateVisibilityTransfer.state_id is required".to_string(),
                            )
                        })
                        .and_then(|state| {
                            StateId::try_from_slice(&state.value).map_err(|err| {
                                ProtocolError::InvalidState(format!(
                                    "StateVisibilityTransfer.state_id is not a valid StateId: {err}"
                                ))
                            })
                        })?;
                    let decode_start = Instant::now();
                    repo.accept_wire_state_visibility(state, &transfer.state_visibility_blob)
                        .map_err(|err| {
                            ProtocolError::InvalidState(format!(
                                "accept_wire_state_visibility for state {}: {err}",
                                state
                            ))
                        })?;
                    let decode_elapsed = decode_start.elapsed();
                    profile.store_receive_object += decode_elapsed;
                }
                Some(pull_server_frame::Frame::GitLane(transfer)) => {
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
                Some(pull_server_frame::Frame::Complete(complete)) => {
                    profile.receive_and_apply = receive_start.elapsed();
                    git_pack_state.ensure_idle()?;
                    let final_state = super::helpers::parse_proto_state_id(complete.new_state)?;

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
                                    (PackObjectId::StateId(_), None) => {
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
) -> Result<PushClientFrame, ProtocolError> {
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
    Ok(PushClientFrame {
        frame: Some(push_client_frame::Frame::Redaction(RedactionTransfer {
            blob_hash: hex,
            redactions_blob: bytes,
        })),
        client_operation_id: String::new(),
    })
}

fn native_pack_required_for_pull(
    want_full_closure: bool,
    transfer_plan: &RepositoryTransferPlan<ObjectInfo>,
) -> bool {
    transfer_plan.requires_native_pack(want_full_closure)
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
        wire::ObjectId::StateId(state_id) => state_id.to_string_full(),
        wire::ObjectId::StateAttachment { state, id } => {
            format!("{}:{}", state.to_string_full(), id.as_hash().to_hex())
        }
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
    wanted_types
        .get(pack_id)
        .and_then(|types| types.iter().copied().find(|obj_type| obj_type.packable()))
}

fn sidecar_push_message(
    repo: &Repository,
    info: wire::ObjectInfo,
) -> Result<PushClientFrame, ProtocolError> {
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
) -> Result<PushClientFrame, ProtocolError> {
    let wire::ObjectId::StateId(state) = info.id else {
        return Err(ProtocolError::InvalidState(
            "wanted StateVisibility must be keyed by ObjectId::StateId(state)".to_string(),
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
    Ok(PushClientFrame {
        frame: Some(push_client_frame::Frame::StateVisibility(
            StateVisibilityTransfer {
                state_id: super::helpers::proto_state_id(state),
                state_visibility_blob: bytes,
            },
        )),
        client_operation_id: String::new(),
    })
}

fn load_thread_metadata(
    repo: &Repository,
    target_thread: &str,
    local_state: StateId,
) -> Result<Option<SyncedThreadMetadata>, ProtocolError> {
    let thread_manager = ThreadManager::new(repo.heddle_dir());
    Ok(thread_manager.find_synced_record_by_thread(repo, target_thread, Some(local_state))?)
}

fn plan_pull_wants(
    repo: &Repository,
    remote_state: &StateId,
    full_closure_available: bool,
    objects_to_fetch: Vec<ObjectDescriptor>,
    allow_partial_fetch: bool,
) -> Result<PullWantPlan, ProtocolError> {
    if full_closure_available {
        return Ok(PullWantPlan {
            wants: Vec::new(),
            transfer_plan: RepositoryTransferPlan::from_object_infos(
                Vec::<ObjectInfo>::new(),
                GitLaneTransferIntent::HeddleObjectsOnly,
            ),
            wanted_types: HashMap::new(),
            want_full_closure: true,
        });
    }
    let request_full_closure =
        should_request_full_closure(repo, remote_state, allow_partial_fetch)?;
    let mut wants = Vec::with_capacity(objects_to_fetch.len());
    let mut wanted_infos = Vec::with_capacity(objects_to_fetch.len());
    let mut wanted_types = HashMap::with_capacity(objects_to_fetch.len());

    for descriptor in objects_to_fetch {
        let info = parse_descriptor_to_info(descriptor)?;
        let pack_id = match &info.id {
            wire::ObjectId::Hash(hash) => PackObjectId::Hash(*hash),
            wire::ObjectId::StateId(state_id) => PackObjectId::StateId(*state_id),
            wire::ObjectId::StateAttachment { id, .. } => PackObjectId::Hash(*id.as_hash()),
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
            wanted_infos.push(info);
        }
    }

    Ok(PullWantPlan {
        wants,
        transfer_plan: RepositoryTransferPlan::from_object_infos(
            wanted_infos,
            GitLaneTransferIntent::HeddleObjectsOnly,
        ),
        wanted_types,
        want_full_closure: false,
    })
}

fn supports_compact_full_pull(
    repo: &Repository,
    allow_partial_fetch: bool,
    exclude_states: &[StateId],
) -> Result<bool, ProtocolError> {
    if allow_partial_fetch || !exclude_states.is_empty() {
        return Ok(false);
    }
    repo_looks_fresh(repo)
}

/// For a bare `pull` (no explicit `--local-thread`), determine which locally
/// held head — if any — is safe to advertise to the server as an
/// `exclude_states` entry so the server prunes its closure to the delta.
///
/// Advertising state S asserts "I already hold S's FULL object closure
/// locally." If we advertise a head whose closure we do NOT fully have, the
/// server omits those objects and we silently end up with an incomplete repo.
/// The server trusts this assertion blindly, so the entire correctness burden
/// is here. We therefore advertise a head ONLY when:
///
/// 1. A target-state override is not in play (the override drives the want
///    plan directly; advertising the thread head would be unrelated).
/// 2. The local repo holds no recorded missing blobs (a partial/lazy clone
///    can hold a state's metadata while its blobs were never fetched — never
///    advertise such a head).
/// 3. The thread we're about to update (`remote_thread`) resolves to a local
///    head whose ENTIRE object closure is present locally — proven by walking
///    it with `enumerate_state_closure`, which errors `ObjectNotFound` on the
///    first absent state/tree/blob.
///
/// When any check fails we return `None` and the caller falls back to the
/// (correct, just slower) empty-exclude full pull. Correctness > speed.
fn locally_complete_pull_head(
    repo: &Repository,
    remote_thread: &str,
    target_state: Option<StateId>,
) -> Result<Option<StateId>, ProtocolError> {
    locally_complete_thread_head(repo, remote_thread, target_state)
}

/// Same completeness gate for an explicit `--local-thread`: the user named the
/// local thread whose head should be advertised as already-held. The cardinal
/// risk is identical to the bare path — advertising a head whose closure we do
/// NOT fully hold makes the server prune objects we lack and silently leaves us
/// with an incomplete repo. A `--local-thread` pointed at a partial/lazy clone
/// or an interrupted prior pull is exactly that hazard, so it must clear the
/// same checks (no target-state override, no recorded missing blobs, full
/// closure present) before it may be advertised.
fn locally_complete_local_thread_head(
    repo: &Repository,
    local_thread: &str,
    target_state: Option<StateId>,
) -> Result<Option<StateId>, ProtocolError> {
    locally_complete_thread_head(repo, local_thread, target_state)
}

/// Shared completeness gate: given the name of a thread whose head we are about
/// to advertise as an `exclude_states` entry, return that head ONLY when its
/// full object closure is provably present locally; otherwise `None` (caller
/// falls back to the correct, slower empty-exclude full pull).
///
/// Advertising state S asserts "I already hold S's FULL object closure
/// locally." If we advertise a head whose closure we do NOT fully have, the
/// server omits those objects and we silently end up with an incomplete repo.
/// The server trusts this assertion blindly, so the entire correctness burden
/// is here. We therefore advertise a head ONLY when:
///
/// 1. A target-state override is not in play (the override drives the want
///    plan directly; advertising the thread head would be unrelated).
/// 2. The local repo holds no recorded missing blobs (a partial/lazy clone
///    can hold a state's metadata while its blobs were never fetched — never
///    advertise such a head).
/// 3. The named thread resolves to a local head whose ENTIRE object closure is
///    present locally — proven by walking it with `enumerate_state_closure`,
///    which errors `ObjectNotFound` on the first absent state/tree/blob.
fn locally_complete_thread_head(
    repo: &Repository,
    thread: &str,
    target_state: Option<StateId>,
) -> Result<Option<StateId>, ProtocolError> {
    // A target-state override pulls a specific state, not the thread tip;
    // advertising the thread head here would not match what's being fetched.
    if target_state.is_some() {
        return Ok(None);
    }
    // A repo carrying known-missing blobs is partial/lazy: it may hold a
    // state's metadata while lacking its blob content. Never advertise.
    if !repo.missing_blobs()?.is_empty() {
        return Ok(None);
    }
    let Some(head) = repo.refs().get_thread(&ThreadName::from(thread))? else {
        // Fresh local repo (no local head for this thread) — nothing to
        // advertise; the server sends the full closure as before.
        return Ok(None);
    };
    // Prove the head's closure is fully present locally. `enumerate_state_closure`
    // loads every state, tree, and blob in the closure and errors `ObjectNotFound`
    // on the first absent object. A clean `Ok` is the completeness guarantee that
    // makes advertising this head safe.
    match wire::enumerate_state_closure(repo.store(), head) {
        Ok(_) => Ok(Some(head)),
        Err(ProtocolError::ObjectNotFound(_)) => Ok(None),
        Err(err) => Err(err),
    }
}

fn should_request_full_closure(
    repo: &Repository,
    remote_state: &StateId,
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
        let Some((name, state_id)) = line.split_once('\t') else {
            return Err(ProtocolError::InvalidState(
                "invalid marker snapshot line".to_string(),
            ));
        };
        let state_id =
            StateId::parse(state_id).map_err(|err| ProtocolError::InvalidState(err.to_string()))?;
        if !repo.store().has_state(&state_id)? {
            continue;
        }
        let name = MarkerName::from(name);
        match repo.refs().get_marker(&name)? {
            Some(existing) if existing == state_id => {}
            Some(existing) => repo.refs().set_marker_cas(
                &name,
                refs::RefExpectation::Value(existing),
                &state_id,
            )?,
            None => repo.refs().create_marker(&name, &state_id)?,
        }
    }

    Ok(true)
}

fn state_id_string_to_bytes(s: &str) -> Vec<u8> {
    if s.is_empty() {
        return Vec::new();
    }
    objects::object::StateId::parse(s)
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
        base_state: StateId::parse(&metadata.base_state)
            .ok()
            .and_then(super::helpers::proto_state_id),
        base_root: state_id_string_to_bytes(&metadata.base_root),
        current_state: metadata.current_state.as_deref().and_then(|state| {
            StateId::parse(state)
                .ok()
                .and_then(super::helpers::proto_state_id)
        }),
        merged_state: metadata.merged_state.as_deref().and_then(|state| {
            StateId::parse(state)
                .ok()
                .and_then(super::helpers::proto_state_id)
        }),
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

fn mark_missing_blobs_for_state(repo: &Repository, state_id: StateId) -> Result<(), ProtocolError> {
    let state = repo
        .store()
        .get_state(&state_id)?
        .ok_or_else(|| ProtocolError::ObjectNotFound(state_id.to_string_full()))?;
    let mut missing = wire::missing_blobs_in_tree(repo.store(), state.tree)?;
    for attachment in repo.list_state_attachments(&state_id)? {
        match attachment.body {
            objects::object::StateAttachmentBody::Context(root) => {
                missing.extend(wire::missing_blobs_in_tree(repo.store(), root)?);
            }
            objects::object::StateAttachmentBody::RiskSignals(hash)
            | objects::object::StateAttachmentBody::ReviewSignatures(hash)
            | objects::object::StateAttachmentBody::Discussions(hash)
            | objects::object::StateAttachmentBody::StructuredConflicts(hash)
                if !repo.store().has_blob(&hash)? =>
            {
                missing.push(hash)
            }
            _ => {}
        }
    }
    missing
        .into_iter()
        .try_for_each(|hash| repo.record_missing_blob(hash).map_err(ProtocolError::from))
}

fn clear_missing_blobs_for_state(
    repo: &Repository,
    state_id: StateId,
) -> Result<(), ProtocolError> {
    let state = repo
        .store()
        .get_state(&state_id)?
        .ok_or_else(|| ProtocolError::ObjectNotFound(state_id.to_string_full()))?;
    let mut missing = wire::missing_blobs_in_tree(repo.store(), state.tree)?;
    for attachment in repo.list_state_attachments(&state_id)? {
        match attachment.body {
            objects::object::StateAttachmentBody::Context(root) => {
                missing.extend(wire::missing_blobs_in_tree(repo.store(), root)?);
            }
            objects::object::StateAttachmentBody::RiskSignals(hash)
            | objects::object::StateAttachmentBody::ReviewSignatures(hash)
            | objects::object::StateAttachmentBody::Discussions(hash)
            | objects::object::StateAttachmentBody::StructuredConflicts(hash) => missing.push(hash),
            _ => {}
        }
    }
    missing
        .into_iter()
        .try_for_each(|hash| repo.clear_missing_blob(&hash).map_err(ProtocolError::from))
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
    target_state: Option<StateId>,
) -> String {
    format!(
        "pull:{repo_path}:{remote_thread}:{}:{depth:?}:{}",
        local_thread.unwrap_or_default(),
        target_state
            .map(|value| value.to_string_full())
            .unwrap_or_default()
    )
}

fn push_transfer_id(repo_path: &str, local_state: StateId, target_thread: &str) -> String {
    format!(
        "push:{repo_path}:{}:{target_thread}",
        local_state.to_string_full()
    )
}

/// Build the git-mirror push plan: read ALL refs from the git ODB, resolve
/// each to its object oid, build ONE multi-root pack over the resolved
/// targets, and emit N checkpoint-less `GitRefUpdateTransfer` messages.
///
/// `remote_ref_expectations` maps each server-side ref name to the git
/// revision address the server currently holds for it (from `ListRefs`);
/// unlisted refs are treated as expected-missing (create). Callers fetch
/// this map before building the plan — the builder itself is synchronous so
/// it can be exercised without a live server.
///
/// The signal for mirror mode is `checkpoint: None` on every ref update
/// (plan §B.4) — do NOT change this discriminator without the matching weft
/// server change (`feat/git-mirror-ref-scope`).
fn build_git_mirror_push_plan(
    repo: &Repository,
    chunk_size: usize,
    remote_ref_expectations: &HashMap<String, GitRefRemoteExpectation>,
) -> Result<GitLanePushPlan, ProtocolError> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Err(ProtocolError::InvalidState(
            "Git remote mirror pushes require a git-overlay repository".to_string(),
        ));
    }
    let git_repo = repo
        .git_overlay_sley_repository()
        .map_err(|err| ProtocolError::InvalidState(err.to_string()))?
        .ok_or_else(|| {
            ProtocolError::InvalidState("git-overlay repository has no Git store".to_string())
        })?;
    build_git_mirror_plan_from_sley(&git_repo, chunk_size, remote_ref_expectations)
}

/// Core of the mirror plan builder, operating directly on a sley repository
/// so it is unit-testable without a hosted `Repository` façade.
fn build_git_mirror_plan_from_sley(
    git_repo: &SleyRepository,
    chunk_size: usize,
    remote_ref_expectations: &HashMap<String, GitRefRemoteExpectation>,
) -> Result<GitLanePushPlan, ProtocolError> {
    let refs = git_repo
        .references()
        .list_refs()
        .map_err(|err| ProtocolError::InvalidState(format!("list git-overlay refs: {err}")))?;

    let mut roots: Vec<GitObjectId> = Vec::new();
    let mut ref_updates: Vec<PushClientFrame> = Vec::new();
    let mut newest_root: Option<GitObjectId> = None;

    for reference in refs {
        // Local-only bookkeeping refs must NOT ship to the hosted server now
        // that the mirror path is the DEFAULT `heddle push` (#846). These four
        // namespaces are purely local git machinery, never content:
        //   - refs/stash       : the stash reflog stack (local WIP)
        //   - refs/remotes/*    : this clone's remote-tracking refs (the
        //                         server has its own view of remotes)
        //   - refs/original/*   : filter-branch/-repo backups (local undo)
        //   - refs/replace/*    : local object replacements (grafts)
        // Excluding them BEFORE the readability check below also means a
        // single dangling/unreadable ref in one of these namespaces (e.g. a
        // stale `refs/original/*` backup) no longer fails the whole push.
        // Content refs — refs/heads/*, refs/tags/*, refs/notes/* (incl.
        // heddle's `refs/notes/heddle` state metadata) — are kept.
        if GitRefName::new(&reference.name).is_local_only() {
            continue;
        }

        // Only direct refs are pushable ref updates. Symbolic refs (e.g.
        // `HEAD`) name another ref that is itself pushed separately; sending
        // a ref update for the symbolic name would push the pointed-at oid
        // under the wrong name.
        let target_oid = match reference.target {
            ReferenceTarget::Direct(oid) => oid,
            ReferenceTarget::Symbolic(_) => continue,
        };

        // Verify the target object is present in the ODB and learn whether
        // it is a tag (so we can populate `peeled_oid`). A dangling ref
        // whose target object is missing cannot be packed — surface it.
        let object = git_repo.read_object(&target_oid).map_err(|err| {
            ProtocolError::InvalidState(format!(
                "git-overlay ref {} target {} is not readable: {err}",
                reference.name,
                target_oid.to_hex()
            ))
        })?;

        let peeled_oid = if object.object_type == sley::GitObjectType::Tag {
            let peeled = git_repo.peel_to_object_oid(target_oid).map_err(|err| {
                ProtocolError::InvalidState(format!(
                    "peel git-overlay tag ref {}: {err}",
                    reference.name
                ))
            })?;
            Some(peeled)
        } else {
            None
        };

        // The pack root is the ref's direct target: packing a tag oid pulls
        // the tag object AND its referent closure; packing a commit pulls
        // that commit's closure.
        roots.push(target_oid);
        newest_root.get_or_insert(target_oid);

        let expectation = remote_ref_expectations
            .get(&reference.name)
            .cloned()
            .unwrap_or(GitRefRemoteExpectation::Missing);

        let kind = grpc_git_ref_kind(GitRefName::new(&reference.name).wire_kind());
        let mut message =
            git_ref_update_message(&reference.name, kind, target_oid, peeled_oid, None);
        apply_git_ref_expectation_value(&mut message, &expectation)?;
        ref_updates.push(message);
    }

    if roots.is_empty() {
        return Err(ProtocolError::InvalidState(
            "git-overlay repository has no direct refs to mirror".to_string(),
        ));
    }

    // Want-only packing (heddle#968). The server told us, per ref, the git oid
    // it currently holds (`GitRefRemoteExpectation::Value` == `git:<oid>` from
    // `ListRefs`). Those oids are a "have" boundary: the server already holds
    // each one and — because git history is a content-addressed DAG — its
    // entire closure (ancestor commits, trees, blobs). Feeding them to the
    // reachable-pack walk as a stop-set packs ONLY the objects reachable from
    // the new roots but not from anything the server already has, instead of
    // re-packing (and re-uploading) the full closure on every push. A pure
    // ref-move whose target the server already holds collapses to an empty
    // pack — see the `None` short-circuit in `build_git_lane_multi_root_pack_plan`.
    let format = git_repo.object_format();
    let mut have_boundary: Vec<GitObjectId> = Vec::new();
    for expectation in remote_ref_expectations.values() {
        if let GitRefRemoteExpectation::Value(oid_bytes) = expectation
            && let Ok(oid) = GitObjectId::from_raw(format, oid_bytes)
        {
            // A stop-set entry the local walk never reaches is simply inert, so
            // it is safe to feed every server-held tip: only the ones that are
            // genuinely ancestors of a pushed root will actually prune the pack.
            have_boundary.push(oid);
        }
    }

    let pack = build_git_lane_multi_root_pack_plan(git_repo, roots, have_boundary, chunk_size)?;

    // `local_revision_address` is advisory in mirror mode (per-ref
    // expectations are already applied); use the first resolved target.
    let local_revision_address = newest_root
        .map(|oid| RevisionAddress::git_commit(oid.to_hex()).to_string())
        .unwrap_or_default();

    Ok(GitLanePushPlan {
        local_revision_address,
        pack,
        ref_updates,
    })
}

fn grpc_git_ref_kind(kind: ClassifiedGitRefKind) -> GrpcGitRefKind {
    match kind {
        ClassifiedGitRefKind::Branch => GrpcGitRefKind::Branch,
        ClassifiedGitRefKind::Tag => GrpcGitRefKind::Tag,
        ClassifiedGitRefKind::Note => GrpcGitRefKind::Note,
        ClassifiedGitRefKind::Other => GrpcGitRefKind::Other,
    }
}

/// Plan a single pack over `roots` (N for the git-mirror path), excluding every
/// object reachable from `excluded` (the server's existing ref tips — the "have"
/// boundary). Returns `Ok(None)` when the exclusions cover the entire reachable
/// set, i.e. the server already holds every pushed object: a pure ref-move that
/// needs no pack, only the ref updates (heddle#968 want-only short-circuit).
fn build_git_lane_multi_root_pack_plan(
    git_repo: &SleyRepository,
    roots: Vec<GitObjectId>,
    excluded: Vec<GitObjectId>,
    chunk_size: usize,
) -> Result<Option<GitPackPushPlan>, ProtocolError> {
    if roots.is_empty() {
        return Err(ProtocolError::InvalidState(
            "cannot plan a Git pack with no roots".to_string(),
        ));
    }
    let Some(plan) = git_repo
        .reachable_pack_plan()
        .roots(roots.iter().copied())
        .exclusions(excluded)
        .build()
        .map_err(|err| {
            ProtocolError::InvalidState(format!("plan reachable Git pack stream: {err}"))
        })?
    else {
        // Empty reachable set: every object the refs reach is already on the
        // server. Skip the pack entirely; only the ref updates ship.
        return Ok(None);
    };
    let pack_file = NamedTempFile::new()
        .map_err(|err| ProtocolError::InvalidState(format!("create Git pack tempfile: {err}")))?;
    let prepared = plan.prepare_to_file(pack_file.path()).map_err(|err| {
        ProtocolError::InvalidState(format!("write reachable Git pack tempfile: {err}"))
    })?;
    let pack_size = prepared.summary.pack_size;
    let checksum = prepared.summary.checksum;
    if pack_size > wire::MAX_RECEIVED_GIT_PACK_SIZE {
        return Err(ProtocolError::InvalidState(format!(
            "Git pack exceeds maximum transfer size of {} bytes; multi-pack split for repos over this size is a follow-up (plan §B.2)",
            wire::MAX_RECEIVED_GIT_PACK_SIZE
        )));
    }
    let chunk_size = chunk_size.max(1) as u64;
    let chunk_count = pack_size.div_ceil(chunk_size);
    if chunk_count > u32::MAX as u64 {
        return Err(ProtocolError::InvalidState(
            "Git pack chunk count exceeds u32".to_string(),
        ));
    }
    let transfer_id = format!("git-pack:{}", checksum.to_hex());
    Ok(Some(GitPackPushPlan {
        transfer_id,
        pack_id: checksum.as_bytes().to_vec(),
        pack_size,
        roots,
        pack_file: Arc::new(Mutex::new(pack_file)),
    }))
}

async fn send_git_pack_streaming_messages(
    tx: &mpsc::Sender<PushClientFrame>,
    pack: &GitPackPushPlan,
    chunk_size: usize,
    progress: &Progress,
) -> Result<(), ProtocolError> {
    let tx = tx.clone();
    let pack = pack.clone();
    let progress = progress.clone();
    tokio::task::spawn_blocking(move || {
        stream_git_pack_messages_blocking(tx, pack, chunk_size, progress)
    })
    .await
    .map_err(|err| ProtocolError::InvalidState(format!("Git pack streaming task failed: {err}")))?
}

fn stream_git_pack_messages_blocking(
    tx: mpsc::Sender<PushClientFrame>,
    pack: GitPackPushPlan,
    chunk_size: usize,
    progress: Progress,
) -> Result<(), ProtocolError> {
    let mut writer = GitPackPushMessageWriter::new(
        tx,
        pack.transfer_id.clone(),
        pack.pack_id.clone(),
        pack.pack_size,
        chunk_size,
        progress,
    );
    let mut pack_file = pack
        .pack_file
        .lock()
        .map_err(|err| ProtocolError::InvalidState(format!("lock Git pack tempfile: {err}")))?;
    pack_file
        .seek(SeekFrom::Start(0))
        .map_err(|err| ProtocolError::InvalidState(format!("rewind Git pack tempfile: {err}")))?;
    let streamed = io::copy(&mut pack_file.as_file_mut(), &mut writer)
        .map_err(|err| ProtocolError::InvalidState(format!("stream Git pack tempfile: {err}")))?;
    if streamed != pack.pack_size {
        return Err(ProtocolError::InvalidState(format!(
            "Git pack stream changed while sending; expected {} bytes/{}, streamed {} bytes",
            pack.pack_size,
            hex::encode(&pack.pack_id),
            streamed
        )));
    }
    writer.finish()?;
    Ok(())
}

struct GitPackPushMessageWriter {
    tx: mpsc::Sender<PushClientFrame>,
    transfer_id: String,
    pack_id: Vec<u8>,
    pack_size: u64,
    chunk_size: usize,
    buffer: Vec<u8>,
    offset: u64,
    chunk_index: u32,
    /// Live "uploading N/M bytes" progress, driven per flushed chunk. A null
    /// handle (`--output json` / non-TTY) makes every update a no-op.
    progress: Progress,
    /// Last integer percent painted, so the "uploading" phase line repaints at
    /// most ~101 times regardless of chunk count. `u64::MAX` forces the first
    /// chunk to paint.
    last_progress_pct: u64,
}

impl GitPackPushMessageWriter {
    fn new(
        tx: mpsc::Sender<PushClientFrame>,
        transfer_id: String,
        pack_id: Vec<u8>,
        pack_size: u64,
        chunk_size: usize,
        progress: Progress,
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
            progress,
            last_progress_pct: u64::MAX,
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
                    pack_chunk: chunk,
                    pack_id: self.pack_id.clone(),
                },
            )))
            .map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "push stream closed unexpectedly")
            })?;
        self.offset = next_offset;
        self.chunk_index = self.chunk_index.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Git pack chunk index overflow")
        })?;
        self.report_upload_progress();
        Ok(())
    }

    /// Paint the live "uploading N/M bytes" line for the bytes flushed so far,
    /// throttled to one repaint per integer percent so a large pack does not
    /// spend its time formatting. A null (`--output json` / non-TTY) handle
    /// short-circuits before any formatting.
    fn report_upload_progress(&mut self) {
        if !self.progress.is_active() {
            return;
        }
        let pct = self.offset.saturating_mul(100) / self.pack_size.max(1);
        if pct == self.last_progress_pct {
            return;
        }
        self.last_progress_pct = pct;
        self.progress.set_phase(format!(
            "uploading {}/{} bytes ({pct}%)",
            self.offset, self.pack_size
        ));
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

/// Build a single `GitRefUpdateTransfer` message.
///
/// `checkpoint` is `Some(..)` for the native checkpoint path and `None` for
/// the git-mirror path — the `None` case is the wire signal the weft server
/// uses to admit checkpoint-less multi-ref pushes (plan §B.4). `peeled_oid`
/// is set for annotated-tag refs (the underlying object the tag names).
fn git_ref_update_message(
    name: &str,
    kind: GrpcGitRefKind,
    target_oid: GitObjectId,
    peeled_oid: Option<GitObjectId>,
    checkpoint: Option<GitCheckpointTransfer>,
) -> PushClientFrame {
    git_lane_push_message(git_lane_transfer::Body::RefUpdate(GitRefUpdateTransfer {
        name: name.to_string(),
        kind: kind as i32,
        target_oid: proto_git_oid(&target_oid),
        peeled_oid: peeled_oid.as_ref().and_then(proto_git_oid),
        expected_missing: false,
        expected_target_oid: None,
        checkpoint,
    }))
}

fn apply_git_ref_expectation_value(
    message: &mut PushClientFrame,
    expectation: &GitRefRemoteExpectation,
) -> Result<(), ProtocolError> {
    let Some(push_client_frame::Frame::GitLane(GitLaneTransfer {
        body: Some(git_lane_transfer::Body::RefUpdate(update)),
    })) = message.frame.as_mut()
    else {
        return Err(ProtocolError::InvalidState(
            "Git lane push plan missing ref update message".to_string(),
        ));
    };
    match expectation {
        GitRefRemoteExpectation::Missing => {
            update.expected_missing = true;
            update.expected_target_oid = None;
        }
        GitRefRemoteExpectation::Value(oid) => {
            update.expected_missing = false;
            update.expected_target_oid = proto_git_oid_bytes(oid);
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
    if remote_revision_address.is_empty() {
        return Ok(GitRefRemoteExpectation::Missing);
    }

    match remote_revision_address.parse::<RevisionAddress>() {
        Ok(RevisionAddress::Heddle(_)) => Ok(GitRefRemoteExpectation::Missing),
        Ok(RevisionAddress::GitCommit(oid)) => hex::decode(&oid)
            .map(GitRefRemoteExpectation::Value)
            .map_err(|err| {
                ProtocolError::InvalidState(format!(
                    "server returned invalid Git remote_revision_address: {err}"
                ))
            }),
        Err(err) => Err(ProtocolError::InvalidState(format!(
            "server returned invalid remote_revision_address {remote_revision_address:?}: {err}"
        ))),
    }
}

fn git_lane_push_message(body: git_lane_transfer::Body) -> PushClientFrame {
    PushClientFrame {
        frame: Some(push_client_frame::Frame::GitLane(GitLaneTransfer {
            body: Some(body),
        })),
        client_operation_id: String::new(),
    }
}

fn git_lane_transfer_size(transfer: &GitLaneTransfer) -> usize {
    match transfer.body.as_ref() {
        Some(git_lane_transfer::Body::Pack(pack)) => pack.pack_chunk.len(),
        Some(git_lane_transfer::Body::RefUpdate(update)) => update
            .target_oid
            .as_ref()
            .map_or(0, |oid| oid.digest.len())
            .saturating_add(update.peeled_oid.as_ref().map_or(0, |oid| oid.digest.len()))
            .saturating_add(
                update
                    .expected_target_oid
                    .as_ref()
                    .map_or(0, |oid| oid.digest.len()),
            )
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
        .heddle_state_id
        .as_ref()
        .map_or(0, |state| state.value.len())
        .saturating_add(
            checkpoint
                .git_commit_oid
                .as_ref()
                .map_or(0, |oid| oid.digest.len()),
        )
        .saturating_add(checkpoint.thread.len())
        .saturating_add(checkpoint.metadata_json.len())
}

#[derive(Default)]
struct GitPackPullInstallState {
    active: Option<GitPackPullInstall>,
}

impl GitPackPullInstallState {
    fn ensure_idle(&self) -> Result<(), ProtocolError> {
        if self.active.is_none() {
            Ok(())
        } else {
            Err(ProtocolError::InvalidState(
                "Git pack transfer ended before final chunk".to_string(),
            ))
        }
    }

    fn receive_chunk(
        &mut self,
        git_repo: &SleyRepository,
        pack: GitPackTransfer,
    ) -> Result<(), ProtocolError> {
        if pack.transfer_id.is_empty() {
            return Err(ProtocolError::InvalidState(
                "Git pack transfer_id is required".to_string(),
            ));
        }
        if pack.pack_size > wire::MAX_RECEIVED_GIT_PACK_SIZE {
            return Err(ProtocolError::InvalidState(format!(
                "Git pack exceeds maximum transfer size of {} bytes",
                wire::MAX_RECEIVED_GIT_PACK_SIZE
            )));
        }
        if pack.pack_chunk.is_empty() {
            return Err(ProtocolError::InvalidState(
                "Git pack chunk must not be empty".to_string(),
            ));
        }
        let pack_id =
            GitObjectId::from_raw(git_repo.object_format(), &pack.pack_id).map_err(|err| {
                ProtocolError::InvalidState(format!("GitPackTransfer.pack_id: {err}"))
            })?;
        if self.active.is_none() {
            if pack.offset != 0 {
                return Err(ProtocolError::InvalidState(format!(
                    "Git pack offset mismatch: expected 0, got {}",
                    pack.offset
                )));
            }
            if pack.chunk_index != 0 {
                return Err(ProtocolError::InvalidState(format!(
                    "Git pack chunk index mismatch: expected 0, got {}",
                    pack.chunk_index
                )));
            }
            let writer = git_repo
                .objects()
                .begin_raw_pack_install(pack_id, pack.pack_size)
                .map_err(|err| {
                    ProtocolError::InvalidState(format!("begin Git pack install: {err}"))
                })?;
            self.active = Some(GitPackPullInstall {
                transfer_id: pack.transfer_id.clone(),
                pack_id,
                pack_size: pack.pack_size,
                next_offset: 0,
                next_chunk_index: 0,
                writer,
            });
        }

        let active = self.active.as_mut().ok_or_else(|| {
            ProtocolError::InvalidState("Git pack install not active".to_string())
        })?;
        active.receive_chunk(&pack, pack_id)?;
        if pack.is_final_chunk {
            let active = self.active.take().ok_or_else(|| {
                ProtocolError::InvalidState("Git pack install not active".to_string())
            })?;
            active.finish()?;
            git_repo.refresh_objects();
        }
        Ok(())
    }
}

struct GitPackPullInstall {
    transfer_id: String,
    pack_id: GitObjectId,
    pack_size: u64,
    next_offset: u64,
    next_chunk_index: u32,
    writer: sley::plumbing::sley_odb::RawPackStreamingInstall,
}

impl GitPackPullInstall {
    fn receive_chunk(
        &mut self,
        pack: &GitPackTransfer,
        pack_id: GitObjectId,
    ) -> Result<(), ProtocolError> {
        if self.transfer_id != pack.transfer_id {
            return Err(ProtocolError::InvalidState(format!(
                "Git pack transfer id changed from {:?} to {:?}",
                self.transfer_id, pack.transfer_id
            )));
        }
        if self.pack_id != pack_id {
            return Err(ProtocolError::InvalidState(
                "Git pack id changed during transfer".to_string(),
            ));
        }
        if self.pack_size != pack.pack_size {
            return Err(ProtocolError::InvalidState(
                "Git pack size changed during transfer".to_string(),
            ));
        }
        if pack.offset != self.next_offset {
            return Err(ProtocolError::InvalidState(format!(
                "Git pack offset mismatch: expected {}, got {}",
                self.next_offset, pack.offset
            )));
        }
        if pack.chunk_index != self.next_chunk_index {
            return Err(ProtocolError::InvalidState(format!(
                "Git pack chunk index mismatch: expected {}, got {}",
                self.next_chunk_index, pack.chunk_index
            )));
        }
        let chunk_len = u64::try_from(pack.pack_chunk.len()).map_err(|_| {
            ProtocolError::InvalidState("Git pack chunk length exceeds u64".to_string())
        })?;
        let next_offset = self
            .next_offset
            .checked_add(chunk_len)
            .ok_or_else(|| ProtocolError::InvalidState("Git pack offset overflow".to_string()))?;
        if next_offset > self.pack_size {
            return Err(ProtocolError::InvalidState(
                "Git pack chunk exceeds declared pack size".to_string(),
            ));
        }
        self.writer.write_all(&pack.pack_chunk).map_err(|err| {
            ProtocolError::InvalidState(format!("write streamed Git pack chunk: {err}"))
        })?;
        self.next_offset = next_offset;
        self.next_chunk_index = self.next_chunk_index.checked_add(1).ok_or_else(|| {
            ProtocolError::InvalidState("Git pack chunk index overflow".to_string())
        })?;
        if pack.is_final_chunk {
            if self.next_offset != self.pack_size {
                return Err(ProtocolError::InvalidState(format!(
                    "Git pack final size mismatch: declared {}, received {}",
                    self.pack_size, self.next_offset
                )));
            }
        } else if self.next_offset == self.pack_size {
            return Err(ProtocolError::InvalidState(
                "Git pack reached declared size without final chunk marker".to_string(),
            ));
        }
        Ok(())
    }

    fn finish(self) -> Result<(), ProtocolError> {
        self.writer
            .finish()
            .map_err(|err| ProtocolError::InvalidState(format!("install Git pack: {err}")))?;
        Ok(())
    }
}

fn accept_git_lane_pull_transfer(
    repo: &Repository,
    git_repo: &mut Option<SleyRepository>,
    git_pack_state: &mut GitPackPullInstallState,
    transfer: GitLaneTransfer,
) -> Result<(), ProtocolError> {
    if repo.capability() != RepositoryCapability::GitOverlay {
        return Err(ProtocolError::InvalidState(format!(
            "received git-lane pull transfer for non-GitOverlay repository (capability {:?})",
            repo.capability()
        )));
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
    git_pack_state: &mut GitPackPullInstallState,
    pack: GitPackTransfer,
) -> Result<(), ProtocolError> {
    let git_repo = git_lane_sley_repository(repo, git_repo)?;
    git_pack_state.receive_chunk(git_repo, pack)?;
    Ok(())
}

/// Apply a server-originated Git ref update on the pull stream.
///
/// Pull-side ref application is unconditional: we commit the local Git ref with
/// [`RefPrecondition::Any`] and do not compare against a prior target oid. That
/// is deliberate and **not** symmetric with push-side compare-and-set, where the
/// client transmits `expected_target_oid` / `expected_missing` from `ListRefs`.
/// The pull stream is single-threaded and server-trusted — the client applies
/// ref updates in the order the server sends them after installing the
/// accompanying pack, so there is no concurrent local writer racing this path.
fn accept_git_lane_ref_update(
    repo: &Repository,
    git_repo: &mut Option<SleyRepository>,
    update: GitRefUpdateTransfer,
) -> Result<(), ProtocolError> {
    let git_repo = git_lane_sley_repository(repo, git_repo)?;
    let target = git_oid_from_proto(
        git_repo,
        "GitRefUpdateTransfer.target_oid",
        update.target_oid.as_ref(),
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
    let state = checkpoint
        .heddle_state_id
        .as_ref()
        .ok_or_else(|| {
            ProtocolError::InvalidState(
                "GitCheckpointTransfer.heddle_state_id is required".to_string(),
            )
        })
        .and_then(|state| {
            StateId::try_from_slice(&state.value)
                .map_err(|err| ProtocolError::InvalidState(err.to_string()))
        })?;
    let commit_oid = git_oid_from_proto(
        git_repo,
        "GitCheckpointTransfer.git_commit_oid",
        checkpoint.git_commit_oid.as_ref(),
    )?;
    let commit_hex = commit_oid.to_hex();
    if repo
        .latest_git_checkpoint_for_state(&state)
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

fn git_oid_from_proto(
    git_repo: &SleyRepository,
    field: &str,
    oid: Option<&ProtoGitObjectId>,
) -> Result<GitObjectId, ProtocolError> {
    let oid = oid.ok_or_else(|| ProtocolError::InvalidState(format!("{field} is required")))?;
    git_oid_from_bytes(git_repo, field, &oid.digest)
}

fn proto_git_oid(oid: &GitObjectId) -> Option<ProtoGitObjectId> {
    proto_git_oid_bytes(oid.as_bytes())
}

fn proto_git_oid_bytes(bytes: &[u8]) -> Option<ProtoGitObjectId> {
    let algorithm = match bytes.len() {
        20 => GitObjectAlgorithm::Sha1,
        32 => GitObjectAlgorithm::Sha256,
        _ => return None,
    };
    Some(ProtoGitObjectId {
        algorithm: algorithm as i32,
        digest: bytes.to_vec(),
    })
}

async fn send_native_pack_streaming_messages(
    tx: &mpsc::Sender<PushClientFrame>,
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
                || (next_index + 1).is_multiple_of(NATIVE_PACK_DRAIN_OBJECT_INTERVAL);
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
        .clamp(1, NATIVE_PACK_OBJECT_LOAD_WORKER_LIMIT)
}

async fn drain_growing_native_pack_stream(
    tx: &mpsc::Sender<PushClientFrame>,
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
    tx: &mpsc::Sender<PushClientFrame>,
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
    tx: &mpsc::Sender<PushClientFrame>,
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
    tx.send(PushClientFrame {
        frame: Some(push_client_frame::Frame::Pack(PackChunk {
            stream_kind: stream_kind as i32,
            data,
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
        client_operation_id: String::new(),
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
    use std::collections::HashSet;

    use chrono::{TimeZone, Utc};
    use cli_shared::ClientConfig;
    use grpc::heddle::api::v1alpha1::{
        ListRefsRequest, ListRefsResponse, PullComplete as GrpcPullComplete, PullReady,
        PullServerFrame, PushServerFrame, StateId as ProtoStateId, TransferCheckpoint,
        UpdateRefRequest, UpdateRefResponse, pull_server_frame, push_client_frame,
        repo_sync_service_server::{RepoSyncService, RepoSyncServiceServer},
    };
    use objects::{
        object::{
            Attribution, Blob, ContentHash, Principal, Redaction, State, StateId, StateVisibility,
            StateVisibilityBlob, Tree, TreeEntry, VisibilityTier,
        },
        store::ObjectStore,
    };
    use tempfile::TempDir;
    use tonic::{Response, Status, transport::Server};
    use wire::{ObjectId, ObjectInfo};

    use super::*;
    use crate::grpc_hosted::helpers::{
        descriptor_id_from_info, proto_state_id, to_proto_object_info,
    };

    fn temp_repo() -> (TempDir, Repository) {
        let dir = TempDir::new().expect("tempdir");
        let repo = Repository::init_default(dir.path()).expect("init repo");
        (dir, repo)
    }

    fn proto_oid_bytes(oid: &Option<ProtoGitObjectId>) -> Option<&[u8]> {
        oid.as_ref().map(|oid| oid.digest.as_slice())
    }

    fn signed_test_config() -> ClientConfig {
        let signer = crypto::Ed25519Signer::generate().expect("generate test signing identity");
        ClientConfig::default().with_auth_proof_key_pem(signer.to_pem().expect("export test key"))
    }

    /// An unseeded repo with no thread heads — the shape `heddle clone`
    /// creates locally before its first pull (`Repository::init`, not
    /// `init_default`).
    fn temp_repo_unseeded() -> (TempDir, Repository) {
        let dir = TempDir::new().expect("tempdir");
        let repo = Repository::init(dir.path()).expect("init repo");
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

    fn state_info(state: StateId) -> ObjectInfo {
        ObjectInfo {
            id: ObjectId::StateId(state),
            obj_type: ObjectType::State,
            size: 0,
            delta_base: None,
        }
    }

    fn state_visibility_info(state: StateId) -> ObjectInfo {
        ObjectInfo {
            id: ObjectId::StateId(state),
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
            state_info(StateId::from_bytes([3u8; 32])),
            state_visibility_info(StateId::from_bytes([9u8; 32])),
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

        let pack =
            build_git_lane_multi_root_pack_plan(&git, vec![commit_oid], Vec::new(), 64 * 1024)
                .expect("build git lane pack plan")
                .expect("non-empty pack plan");
        assert_eq!(pack.pack_id.len(), git.object_format().raw_len());
        let (tx, mut rx) = mpsc::channel(8);
        stream_git_pack_messages_blocking(tx, pack.clone(), 64 * 1024, Progress::null())
            .expect("stream git lane pack");
        let mut pack_bytes = Vec::new();
        let mut chunks = Vec::new();
        while let Some(chunk) = rx.blocking_recv() {
            let Some(push_client_frame::Frame::GitLane(GitLaneTransfer {
                body: Some(git_lane_transfer::Body::Pack(pack)),
            })) = chunk.frame
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

        let state = StateId::from_bytes([9u8; 32]);
        let ref_message = git_ref_update_message(
            "refs/heads/main",
            GrpcGitRefKind::Branch,
            commit_oid,
            None,
            Some(GitCheckpointTransfer {
                heddle_state_id: proto_state_id(state),
                git_commit_oid: proto_git_oid(&commit_oid),
                thread: "main".to_string(),
                metadata_json: String::new(),
            }),
        );
        let Some(push_client_frame::Frame::GitLane(GitLaneTransfer {
            body: Some(git_lane_transfer::Body::RefUpdate(update)),
        })) = ref_message.frame
        else {
            panic!("expected git ref update message");
        };
        assert_eq!(update.name, "refs/heads/main");
        assert_eq!(update.kind, GrpcGitRefKind::Branch as i32);
        assert_eq!(
            update.target_oid.as_ref().map(|oid| oid.digest.as_slice()),
            Some(commit_oid.as_bytes())
        );
        let checkpoint = update.checkpoint.expect("checkpoint");
        assert_eq!(
            checkpoint
                .heddle_state_id
                .as_ref()
                .map(|state| state.value.as_slice()),
            Some(state.as_bytes().as_slice())
        );
        assert_eq!(
            checkpoint
                .git_commit_oid
                .as_ref()
                .map(|oid| oid.digest.as_slice()),
            Some(commit_oid.as_bytes())
        );
        assert_eq!(checkpoint.thread, "main");
    }

    fn sample_ref_update_message(commit_oid: GitObjectId) -> PushClientFrame {
        git_ref_update_message(
            "refs/heads/main",
            GrpcGitRefKind::Branch,
            commit_oid,
            None,
            Some(GitCheckpointTransfer {
                heddle_state_id: proto_state_id(StateId::from_bytes([9u8; 32])),
                git_commit_oid: proto_git_oid(&commit_oid),
                thread: "main".to_string(),
                metadata_json: String::new(),
            }),
        )
    }

    /// Write a distinct single-commit graph into `git` and return the commit
    /// oid. `seed` differentiates the tree content so each commit is unique.
    fn write_commit(git: &sley::Repository, seed: &str) -> GitObjectId {
        let blob_oid = git
            .write_blob(format!("content-{seed}\n").as_bytes())
            .expect("write blob");
        let tree = sley::TreeObject {
            entries: vec![sley::plumbing::sley_object::TreeEntry {
                mode: 0o100644,
                name: sley::BString::from(format!("{seed}.txt").into_bytes()),
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
            message: format!("commit {seed}\n").into_bytes(),
        };
        git.write_raw_object(sley::GitObjectType::Commit, commit.write())
            .expect("write commit")
    }

    fn set_ref(git: &mut sley::Repository, name: &str, target: GitObjectId) {
        let refs = git.references();
        let mut tx = refs.transaction();
        tx.update_to(
            name.to_string(),
            ReferenceTarget::Direct(target),
            RefPrecondition::Any,
            None,
        );
        tx.commit().expect("commit ref");
    }

    fn mirror_ref_updates(plan: &GitLanePushPlan) -> Vec<GitRefUpdateTransfer> {
        plan.ref_updates
            .iter()
            .map(git_ref_update_from_message)
            .cloned()
            .collect()
    }

    /// A commit that reuses `parent`'s tree/blob closure and only adds one new
    /// blob + tree + commit on top, so a descendant push shares nearly all of
    /// the parent's objects.
    fn write_child_commit(git: &sley::Repository, parent: GitObjectId, seed: &str) -> GitObjectId {
        let blob_oid = git
            .write_blob(format!("content-{seed}\n").as_bytes())
            .expect("write blob");
        let tree = sley::TreeObject {
            entries: vec![sley::plumbing::sley_object::TreeEntry {
                mode: 0o100644,
                name: sley::BString::from(format!("{seed}.txt").into_bytes()),
                oid: blob_oid,
            }],
        };
        let tree_oid = git
            .write_raw_object(sley::GitObjectType::Tree, tree.write())
            .expect("write tree");
        let commit = sley::CommitObject {
            tree: tree_oid,
            parents: vec![parent],
            author: b"Tester <test@example.com> 1700000000 +0000".to_vec(),
            committer: b"Tester <test@example.com> 1700000000 +0000".to_vec(),
            encoding: None,
            message: format!("commit {seed}\n").into_bytes(),
        };
        git.write_raw_object(sley::GitObjectType::Commit, commit.write())
            .expect("write commit")
    }

    fn value_expectation(oid: GitObjectId) -> GitRefRemoteExpectation {
        GitRefRemoteExpectation::Value(oid.as_bytes().to_vec())
    }

    /// heddle#968: the mirror plan must honor the server's have-boundary
    /// (`ListRefs` ref tips) and pack ONLY the objects the server is missing,
    /// instead of re-packing the full closure on every push.
    ///
    /// Demonstrates all three regimes on one repo:
    ///   1. fresh server (no expectations)        → full pack (baseline)
    ///   2. server already has the base commit     → tiny delta pack
    ///   3. server already has the exact target    → NO pack (pure ref-move)
    #[test]
    fn git_mirror_plan_packs_only_wanted_objects_against_have_boundary() {
        let dir = TempDir::new().expect("tempdir");
        let mut git = sley::Repository::init(dir.path()).expect("init git");

        // A base branch with a chunky closure, then a descendant that adds a
        // single new object on top and shares everything else.
        let base = write_commit(&git, "base-with-a-reasonably-large-payload");
        let mut tip = base;
        for i in 0..8 {
            tip = write_child_commit(&git, tip, &format!("layer-{i}"));
        }
        let descendant = write_child_commit(&git, tip, "descendant-adds-one-object");
        set_ref(&mut git, "refs/heads/main", descendant);

        // (1) Fresh server: nothing is on the far side, so the full closure
        //     packs — this is the status-quo "re-pack everything" cost.
        let full =
            build_git_mirror_plan_from_sley(&git, 64 * 1024, &HashMap::new()).expect("full plan");
        let full_pack = full.pack.as_ref().expect("fresh server produces a pack");
        let full_size = full_pack.pack_size;
        assert!(full_size > 0, "baseline full pack must be non-empty");

        // (2) Server already holds `tip` (e.g. `main` was pushed first). Only
        //     `descendant`'s single new blob/tree/commit is missing, so the
        //     want-only pack is DRAMATICALLY smaller than the full closure.
        let mut have_tip = HashMap::new();
        have_tip.insert("refs/heads/main".to_string(), value_expectation(tip));
        let delta =
            build_git_mirror_plan_from_sley(&git, 64 * 1024, &have_tip).expect("delta plan");
        let delta_pack = delta
            .pack
            .as_ref()
            .expect("descendant still has one new object to pack");
        let delta_size = delta_pack.pack_size;
        assert!(
            delta_size * 4 < full_size,
            "want-only packing must be dramatically smaller than a full re-pack \
             (delta {delta_size} bytes vs full {full_size} bytes)",
        );

        // (3) Pure ref-move: the server already holds the EXACT ref target, so
        //     there is nothing new to pack. The plan short-circuits to `None`
        //     (only the ref update ships) — the `pr/N`-after-`main` fast path.
        let mut have_target = HashMap::new();
        have_target.insert("refs/heads/main".to_string(), value_expectation(descendant));
        let ref_only =
            build_git_mirror_plan_from_sley(&git, 64 * 1024, &have_target).expect("ref-only plan");
        assert!(
            ref_only.pack.is_none(),
            "pushing a ref the server already has must send NO pack",
        );
        // The ref update itself is still present — correctness preserved.
        assert_eq!(
            ref_only.ref_updates.len(),
            1,
            "the ref update still ships even with an empty want set",
        );
    }

    /// `ReachablePackPlan::prepare_to_file` must emit the same pack bytes as the
    /// legacy `write_reachable_pack_to_writer` path (wire checksum + size).
    #[test]
    fn git_lane_reachable_pack_plan_matches_legacy_writer_checksum() {
        let dir = TempDir::new().expect("tempdir");
        let git = sley::Repository::init(dir.path()).expect("init git");
        let commit_oid = write_commit(&git, "byte-identity");

        let pack_plan =
            build_git_lane_multi_root_pack_plan(&git, vec![commit_oid], Vec::new(), 64 * 1024)
                .expect("build pack plan")
                .expect("non-empty pack plan");

        let mut legacy_pack = Vec::new();
        let legacy = sley::plumbing::sley_odb::write_reachable_pack_to_writer(
            git.objects().as_ref(),
            git.object_format(),
            std::iter::once(commit_oid),
            &HashSet::new(),
            &mut legacy_pack,
        )
        .expect("legacy reachable pack")
        .expect("legacy pack summary");

        assert_eq!(pack_plan.pack_id, legacy.checksum.as_bytes().to_vec());
        assert_eq!(pack_plan.pack_size, legacy.pack_size);
        assert_eq!(pack_plan.pack_size, legacy_pack.len() as u64);

        let mut planned_pack = Vec::new();
        pack_plan
            .pack_file
            .lock()
            .expect("lock pack tempfile")
            .seek(SeekFrom::Start(0))
            .expect("rewind pack tempfile");
        io::copy(
            &mut pack_plan
                .pack_file
                .lock()
                .expect("lock pack tempfile")
                .as_file_mut(),
            &mut planned_pack,
        )
        .expect("read planned pack");
        assert_eq!(planned_pack, legacy_pack);
    }

    /// The mirror plan builder reads ALL direct refs (N branches + a tag),
    /// emits exactly N checkpoint-less ref updates with the correct kind, and
    /// builds ONE pack whose root set equals the resolved ref targets.
    #[test]
    fn git_mirror_plan_builds_one_pack_and_checkpointless_updates_for_all_refs() {
        let dir = TempDir::new().expect("tempdir");
        let mut git = sley::Repository::init(dir.path()).expect("init git");

        let main_commit = write_commit(&git, "main");
        let feature_commit = write_commit(&git, "feature");
        let tagged_commit = write_commit(&git, "tagged");

        set_ref(&mut git, "refs/heads/main", main_commit);
        set_ref(&mut git, "refs/heads/feature", feature_commit);

        // Annotated tag pointing at `tagged_commit`.
        let tag = sley::TagObject {
            object: tagged_commit,
            object_type: sley::GitObjectType::Commit,
            name: b"v1".to_vec(),
            tagger: Some(b"Tester <test@example.com> 1700000000 +0000".to_vec()),
            message: b"release v1\n".to_vec(),
            raw_body: None,
        };
        let tag_oid = git
            .write_raw_object(sley::GitObjectType::Tag, tag.write())
            .expect("write tag");
        set_ref(&mut git, "refs/tags/v1", tag_oid);

        let plan = build_git_mirror_plan_from_sley(&git, 64 * 1024, &HashMap::new())
            .expect("build mirror plan");

        let updates = mirror_ref_updates(&plan);
        assert_eq!(updates.len(), 3, "one ref update per direct ref");

        // Every mirror-mode ref update MUST have `checkpoint: None` — the
        // discriminator the weft server keys on (plan §B.4).
        assert!(
            updates.iter().all(|u| u.checkpoint.is_none()),
            "mirror mode signals via checkpoint: None on every ref update",
        );

        let by_name: HashMap<&str, &GitRefUpdateTransfer> =
            updates.iter().map(|u| (u.name.as_str(), u)).collect();

        let main = by_name["refs/heads/main"];
        assert_eq!(main.kind, GrpcGitRefKind::Branch as i32);
        assert_eq!(
            proto_oid_bytes(&main.target_oid),
            Some(main_commit.as_bytes())
        );
        assert!(main.peeled_oid.is_none(), "commit refs are not peeled");

        let feature = by_name["refs/heads/feature"];
        assert_eq!(feature.kind, GrpcGitRefKind::Branch as i32);
        assert_eq!(
            proto_oid_bytes(&feature.target_oid),
            Some(feature_commit.as_bytes())
        );

        let tag_update = by_name["refs/tags/v1"];
        assert_eq!(tag_update.kind, GrpcGitRefKind::Tag as i32);
        assert_eq!(
            proto_oid_bytes(&tag_update.target_oid),
            Some(tag_oid.as_bytes())
        );
        assert_eq!(
            proto_oid_bytes(&tag_update.peeled_oid),
            Some(tagged_commit.as_bytes()),
            "annotated tag ref is peeled to its underlying commit",
        );

        // ONE pack, whose root set equals the resolved ref targets. With no
        // remote expectations every object is new, so the want-only walk still
        // produces a full pack.
        let plan_pack = plan.pack.as_ref().expect("full pack for fresh server");
        let mut pack_roots: Vec<GitObjectId> = plan_pack.roots.clone();
        pack_roots.sort_by_key(|oid| oid.to_hex());
        let mut expected_roots = vec![main_commit, feature_commit, tag_oid];
        expected_roots.sort_by_key(|oid| oid.to_hex());
        assert_eq!(
            pack_roots, expected_roots,
            "pack roots == resolved ref targets",
        );

        // The single pack must actually contain the whole closure: main,
        // feature, tag object + tagged commit, plus their trees/blobs.
        let (tx, mut rx) = mpsc::channel(64);
        stream_git_pack_messages_blocking(tx, plan_pack.clone(), 64 * 1024, Progress::null())
            .expect("stream mirror pack");
        let mut pack_bytes = Vec::new();
        while let Some(message) = rx.blocking_recv() {
            if let Some(push_client_frame::Frame::GitLane(GitLaneTransfer {
                body: Some(git_lane_transfer::Body::Pack(pack)),
            })) = message.frame
            {
                pack_bytes.extend_from_slice(&pack.pack_chunk);
            }
        }
        assert_eq!(
            pack_bytes.len() as u64,
            plan_pack.pack_size,
            "streamed pack size must match plan",
        );
        let indexed = sley::plumbing::sley_odb::index_raw_pack(&pack_bytes, git.object_format())
            .expect("mirror pack indexes");
        let packed: HashSet<Vec<u8>> = indexed
            .objects
            .iter()
            .map(|obj| obj.oid.as_bytes().to_vec())
            .collect();
        for oid in [main_commit, feature_commit, tagged_commit, tag_oid] {
            assert!(
                packed.contains(oid.as_bytes()),
                "pack must contain {}",
                oid.to_hex()
            );
        }
    }

    /// Symbolic refs (e.g. HEAD) are not emitted as ref updates — only their
    /// direct target ref is pushed.
    #[test]
    fn git_mirror_plan_skips_symbolic_refs() {
        let dir = TempDir::new().expect("tempdir");
        let mut git = sley::Repository::init(dir.path()).expect("init git");
        let main_commit = write_commit(&git, "main");
        set_ref(&mut git, "refs/heads/main", main_commit);
        // HEAD is symbolic → refs/heads/main; it must not become its own
        // ref update.
        {
            let refs = git.references();
            let mut tx = refs.transaction();
            tx.update_to(
                "HEAD".to_string(),
                ReferenceTarget::Symbolic("refs/heads/main".to_string()),
                RefPrecondition::Any,
                None,
            );
            tx.commit().expect("set HEAD");
        }

        let plan = build_git_mirror_plan_from_sley(&git, 64 * 1024, &HashMap::new())
            .expect("build mirror plan");
        let updates = mirror_ref_updates(&plan);
        assert_eq!(updates.len(), 1, "only the direct ref is pushed");
        assert_eq!(updates[0].name, "refs/heads/main");
    }

    /// Per-ref remote expectations from `ListRefs` are applied to each ref
    /// update (compare-and-set); unlisted refs default to expected-missing.
    #[test]
    fn git_mirror_plan_applies_per_ref_remote_expectations() {
        let dir = TempDir::new().expect("tempdir");
        let mut git = sley::Repository::init(dir.path()).expect("init git");
        let main_commit = write_commit(&git, "main");
        let feature_commit = write_commit(&git, "feature");
        set_ref(&mut git, "refs/heads/main", main_commit);
        set_ref(&mut git, "refs/heads/feature", feature_commit);

        let remote_oid = "89abcdef012345670123456789abcdef01234567";
        let mut expectations = HashMap::new();
        expectations.insert(
            "refs/heads/main".to_string(),
            GitRefRemoteExpectation::Value(hex::decode(remote_oid).expect("hex")),
        );
        // refs/heads/feature intentionally absent → expected-missing.

        let plan = build_git_mirror_plan_from_sley(&git, 64 * 1024, &expectations)
            .expect("build mirror plan");
        let updates = mirror_ref_updates(&plan);
        let by_name: HashMap<&str, &GitRefUpdateTransfer> =
            updates.iter().map(|u| (u.name.as_str(), u)).collect();

        let main = by_name["refs/heads/main"];
        assert!(!main.expected_missing);
        assert_eq!(
            hex::encode(proto_oid_bytes(&main.expected_target_oid).expect("expected oid")),
            remote_oid
        );

        let feature = by_name["refs/heads/feature"];
        assert!(
            feature.expected_missing,
            "ref absent from ListRefs is expected-missing (create)",
        );
    }

    /// GitHub-style `refs/pull/*` ref names must pass through the mirror plan
    /// unchanged: classified as `Other` (not branch/tag/note), packed with the
    /// correct target, and checkpoint-less. Confirms the #846 ref-name caveat.
    #[test]
    fn git_mirror_plan_includes_pull_request_refs() {
        let dir = TempDir::new().expect("tempdir");
        let mut git = sley::Repository::init(dir.path()).expect("init git");
        let pr_commit = write_commit(&git, "pr");
        set_ref(&mut git, "refs/pull/42/head", pr_commit);

        let plan = build_git_mirror_plan_from_sley(&git, 64 * 1024, &HashMap::new())
            .expect("build mirror plan");
        let updates = mirror_ref_updates(&plan);
        let pull = updates
            .iter()
            .find(|update| update.name == "refs/pull/42/head")
            .expect("refs/pull/* ref must be mirrored");
        assert_eq!(
            pull.kind,
            GrpcGitRefKind::Other as i32,
            "refs/pull/* classifies as Other",
        );
        assert_eq!(
            proto_oid_bytes(&pull.target_oid),
            Some(pr_commit.as_bytes())
        );
        assert!(
            pull.checkpoint.is_none(),
            "mirror ref updates are checkpoint-less",
        );
        assert!(pull.peeled_oid.is_none(), "a commit ref is not peeled");
    }

    /// The default mirror push must ship CONTENT refs (heads, tags, and
    /// heddle's `refs/notes/heddle` state metadata) but EXCLUDE local-only
    /// bookkeeping namespaces (#846): `refs/stash`, `refs/remotes/*`,
    /// `refs/original/*`, `refs/replace/*`.
    #[test]
    fn git_mirror_plan_excludes_local_only_refs_and_keeps_content() {
        let dir = TempDir::new().expect("tempdir");
        let mut git = sley::Repository::init(dir.path()).expect("init git");

        let main_commit = write_commit(&git, "main");
        let notes_commit = write_commit(&git, "notes");
        let stash_commit = write_commit(&git, "stash");
        let remote_commit = write_commit(&git, "remote");
        let original_commit = write_commit(&git, "original");
        let replace_commit = write_commit(&git, "replace");

        // Content refs — these MUST be mirrored.
        set_ref(&mut git, "refs/heads/main", main_commit);
        set_ref(&mut git, "refs/notes/heddle", notes_commit);

        // Local-only bookkeeping — these MUST be excluded.
        set_ref(&mut git, "refs/stash", stash_commit);
        set_ref(&mut git, "refs/remotes/origin/main", remote_commit);
        set_ref(&mut git, "refs/original/refs/heads/main", original_commit);
        set_ref(&mut git, "refs/replace/deadbeef", replace_commit);

        let plan = build_git_mirror_plan_from_sley(&git, 64 * 1024, &HashMap::new())
            .expect("build mirror plan");
        let names: Vec<&str> = plan
            .ref_updates
            .iter()
            .map(|m| git_ref_update_from_message(m).name.as_str())
            .collect();

        assert!(
            names.contains(&"refs/heads/main"),
            "content branch is mirrored: {names:?}",
        );
        assert!(
            names.contains(&"refs/notes/heddle"),
            "heddle state-note ref is mirrored: {names:?}",
        );
        for excluded in [
            "refs/stash",
            "refs/remotes/origin/main",
            "refs/original/refs/heads/main",
            "refs/replace/deadbeef",
        ] {
            assert!(
                !names.contains(&excluded),
                "local-only ref {excluded} must NOT be mirrored: {names:?}",
            );
        }
        assert_eq!(
            names.len(),
            2,
            "exactly the two content refs ship: {names:?}"
        );
    }

    /// A dangling ref in an EXCLUDED namespace (e.g. a stale
    /// `refs/original/*` filter-branch backup whose target object is gone)
    /// must not fail the whole push — it is filtered before the readability
    /// check. Content refs still ship.
    #[test]
    fn git_mirror_plan_ignores_dangling_local_only_ref() {
        let dir = TempDir::new().expect("tempdir");
        let mut git = sley::Repository::init(dir.path()).expect("init git");

        let main_commit = write_commit(&git, "main");
        set_ref(&mut git, "refs/heads/main", main_commit);

        // Point a local-only ref at a non-existent object oid.
        let missing = GitObjectId::from_hex(
            sley::ObjectFormat::Sha1,
            "0123456789abcdef0123456789abcdef01234567",
        )
        .expect("oid");
        set_ref(&mut git, "refs/original/refs/heads/main", missing);

        let plan = build_git_mirror_plan_from_sley(&git, 64 * 1024, &HashMap::new())
            .expect("dangling local-only ref must not fail the mirror plan");
        let names: Vec<&str> = plan
            .ref_updates
            .iter()
            .map(|m| git_ref_update_from_message(m).name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["refs/heads/main"],
            "only content ships: {names:?}"
        );
    }

    #[test]
    fn git_ref_expectation_marks_missing_when_remote_has_no_git_revision() {
        let commit_oid = GitObjectId::from_hex(
            sley::ObjectFormat::Sha1,
            "0123456789abcdef0123456789abcdef01234567",
        )
        .expect("oid");
        let mut message = sample_ref_update_message(commit_oid);

        let expectation = parse_git_ref_expectation("").expect("missing expectation");
        apply_git_ref_expectation_value(&mut message, &expectation).expect("apply expectation");
        let update = git_ref_update_from_message(&message);
        assert!(update.expected_missing);
        assert!(update.expected_target_oid.is_none());
    }

    #[test]
    fn git_ref_expectation_uses_remote_git_revision_oid() {
        let commit_oid = GitObjectId::from_hex(
            sley::ObjectFormat::Sha1,
            "0123456789abcdef0123456789abcdef01234567",
        )
        .expect("oid");
        let remote_oid = "89abcdef012345670123456789abcdef01234567";
        let mut message = sample_ref_update_message(commit_oid);

        let expectation =
            parse_git_ref_expectation(&format!("git:{remote_oid}")).expect("git expectation");
        apply_git_ref_expectation_value(&mut message, &expectation).expect("apply expectation");
        let update = git_ref_update_from_message(&message);
        assert!(!update.expected_missing);
        assert_eq!(
            hex::encode(proto_oid_bytes(&update.expected_target_oid).expect("expected oid")),
            remote_oid
        );
    }

    #[test]
    fn git_pack_stream_writer_emits_ordered_chunks() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut writer = GitPackPushMessageWriter::new(
            tx,
            "git-pack:test".to_string(),
            vec![0x42; 20],
            10,
            4,
            Progress::null(),
        );
        writer.write_all(b"abcdefghij").expect("write pack bytes");
        writer.finish().expect("finish pack stream");

        let mut chunks = Vec::new();
        while let Some(message) = rx.blocking_recv() {
            let Some(push_client_frame::Frame::GitLane(GitLaneTransfer {
                body: Some(git_lane_transfer::Body::Pack(pack)),
            })) = message.frame
            else {
                panic!("expected git pack chunk");
            };
            chunks.push(pack);
        }

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].offset, 0);
        assert_eq!(chunks[0].chunk_index, 0);
        assert!(!chunks[0].is_final_chunk);
        assert_eq!(chunks[0].pack_chunk.as_slice(), b"abcd");
        assert_eq!(chunks[1].offset, 4);
        assert_eq!(chunks[1].chunk_index, 1);
        assert!(!chunks[1].is_final_chunk);
        assert_eq!(chunks[1].pack_chunk.as_slice(), b"efgh");
        assert_eq!(chunks[2].offset, 8);
        assert_eq!(chunks[2].chunk_index, 2);
        assert!(chunks[2].is_final_chunk);
        assert_eq!(chunks[2].pack_chunk.as_slice(), b"ij");
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.pack_id.as_slice() == &[0x42; 20][..])
        );
    }

    /// A `Sink` that records the phase label of every rendered snapshot, so a
    /// test can assert on the human progress line the push seam drives.
    #[derive(Default)]
    struct PhaseCapturingSink {
        phases: std::sync::Mutex<Vec<String>>,
    }

    impl PhaseCapturingSink {
        fn phases(&self) -> Vec<String> {
            self.phases.lock().unwrap().clone()
        }
    }

    impl objects::Sink for PhaseCapturingSink {
        fn render(&self, snap: objects::ProgressSnapshot) {
            self.phases.lock().unwrap().push(snap.phase);
        }
    }

    /// Streaming the Git pack must drive the generic progress substrate with a
    /// live "uploading N/M bytes" phase, ending at the full pack size. This is
    /// the DoD "live progress line" for the default Git Projection pack push.
    #[test]
    fn git_pack_stream_reports_upload_progress() {
        let dir = TempDir::new().expect("tempdir");
        let git = sley::Repository::init(dir.path()).expect("init git");
        let commit_oid = write_commit(&git, "progress");
        // Small chunk size so the pack streams over several chunks.
        let pack = build_git_lane_multi_root_pack_plan(&git, vec![commit_oid], Vec::new(), 64)
            .expect("build pack plan")
            .expect("non-empty pack plan");

        let sink = std::sync::Arc::new(PhaseCapturingSink::default());
        struct Forward(std::sync::Arc<PhaseCapturingSink>);
        impl objects::Sink for Forward {
            fn render(&self, snap: objects::ProgressSnapshot) {
                self.0.render(snap);
            }
        }
        let progress = Progress::with_sink(Box::new(Forward(std::sync::Arc::clone(&sink))));

        let (tx, mut rx) = mpsc::channel(1024);
        stream_git_pack_messages_blocking(tx, pack.clone(), 64, progress.clone())
            .expect("stream pack");
        while rx.blocking_recv().is_some() {}

        let phases = sink.phases();
        assert!(
            phases.iter().any(|phase| phase.contains("uploading")),
            "pack streamer must drive an 'uploading' progress phase; saw {phases:?}",
        );
        let last_uploading = phases
            .iter()
            .rev()
            .find(|phase| phase.contains("uploading"))
            .cloned();
        assert!(
            last_uploading
                .as_deref()
                .is_some_and(|phase| phase.contains(&pack.pack_size.to_string())),
            "final uploading phase must show the full pack size ({} bytes); saw {last_uploading:?}",
            pack.pack_size,
        );
    }

    #[test]
    fn git_pack_pull_install_state_streams_pack_into_sley_store() {
        let source_dir = TempDir::new().expect("source tempdir");
        let source = sley::Repository::init(source_dir.path()).expect("init source git");
        let blob_oid = source.write_blob(b"streamed pull pack\n").expect("blob");
        let pack = sley::plumbing::sley_odb::build_reachable_pack(
            source.objects().as_ref(),
            source.object_format(),
            [blob_oid],
            &HashSet::new(),
        )
        .expect("build pack")
        .expect("reachable pack");

        let dest_dir = TempDir::new().expect("dest tempdir");
        let dest = sley::Repository::init(dest_dir.path()).expect("init dest git");
        let mut state = GitPackPullInstallState::default();
        let chunk_size = 7usize;
        let mut offset = 0u64;
        for (chunk_index, chunk) in pack.pack.chunks(chunk_size).enumerate() {
            let next_offset = offset + chunk.len() as u64;
            state
                .receive_chunk(
                    &dest,
                    GitPackTransfer {
                        transfer_id: "git-pack:test".to_string(),
                        offset,
                        chunk_index: chunk_index as u32,
                        is_final_chunk: next_offset == pack.pack.len() as u64,
                        pack_size: pack.pack.len() as u64,
                        pack_chunk: chunk.to_vec(),
                        pack_id: pack.checksum.as_bytes().to_vec(),
                    },
                )
                .expect("receive chunk");
            offset = next_offset;
        }

        state.ensure_idle().expect("stream should finish");
        let object = dest.read_object(&blob_oid).expect("read installed object");
        assert_eq!(object.body.as_slice(), b"streamed pull pack\n");
    }

    #[test]
    fn accept_git_lane_pull_transfer_errors_for_non_overlay_repo() {
        let (_dir, repo) = temp_repo();
        assert_ne!(
            repo.capability(),
            RepositoryCapability::GitOverlay,
            "init_default repo must be non-GitOverlay for this guard test",
        );
        let mut git_repo = None;
        let mut state = GitPackPullInstallState::default();
        let transfer = GitLaneTransfer {
            body: Some(git_lane_transfer::Body::Pack(GitPackTransfer {
                transfer_id: "git-pack:test".to_string(),
                offset: 0,
                chunk_index: 0,
                is_final_chunk: true,
                pack_size: 0,
                pack_chunk: Vec::new(),
                pack_id: Vec::new(),
            })),
        };
        let err = accept_git_lane_pull_transfer(&repo, &mut git_repo, &mut state, transfer)
            .expect_err("git-lane pull to a non-GitOverlay repo must fail loud, not silently drop");
        assert!(
            matches!(err, ProtocolError::InvalidState(_)),
            "expected InvalidState protocol error, got {err:?}",
        );
    }

    fn git_ref_update_from_message(message: &PushClientFrame) -> &GitRefUpdateTransfer {
        let Some(push_client_frame::Frame::GitLane(GitLaneTransfer {
            body: Some(git_lane_transfer::Body::RefUpdate(update)),
        })) = message.frame.as_ref()
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
            state: StateId::from_bytes([1u8; 32]),
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

    fn sample_state_visibility(state: StateId) -> StateVisibility {
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
                wire::TransferPartitions::<ObjectInfo>::is_sidecar_object_type(*obj_type),
                "{obj_type:?} is excluded from native packs but missing from the out-of-pack transfer partition"
            );
        }
    }

    #[test]
    fn native_pack_required_tracks_packable_pull_wants() {
        let blob = sample_blob();
        let state = StateId::from_bytes([9u8; 32]);

        let sidecar_only = RepositoryTransferPlan::from_object_infos(
            vec![state_visibility_info(state)],
            GitLaneTransferIntent::HeddleObjectsOnly,
        );
        assert!(!native_pack_required_for_pull(false, &sidecar_only));

        let redaction_only = RepositoryTransferPlan::from_object_infos(
            vec![redaction_info(blob)],
            GitLaneTransferIntent::HeddleObjectsOnly,
        );
        assert!(!native_pack_required_for_pull(false, &redaction_only));

        let packable = RepositoryTransferPlan::from_object_infos(
            vec![ObjectInfo {
                id: ObjectId::Hash(blob),
                obj_type: ObjectType::Blob,
                size: 0,
                delta_base: None,
            }],
            GitLaneTransferIntent::HeddleObjectsOnly,
        );
        assert!(native_pack_required_for_pull(false, &packable));

        let state_with_sidecar = RepositoryTransferPlan::from_object_infos(
            vec![state_info(state), state_visibility_info(state)],
            GitLaneTransferIntent::HeddleObjectsOnly,
        );
        assert!(native_pack_required_for_pull(false, &state_with_sidecar));
        let empty = RepositoryTransferPlan::from_object_infos(
            Vec::<ObjectInfo>::new(),
            GitLaneTransferIntent::HeddleObjectsOnly,
        );
        assert!(native_pack_required_for_pull(true, &empty));
    }

    #[test]
    fn plan_pull_wants_accumulates_state_and_visibility_for_same_state_id() {
        let (_dir, repo) = temp_repo();
        let state = StateId::from_bytes([9u8; 32]);
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
            .get(&PackObjectId::StateId(state))
            .expect("same StateId want entry");
        assert_eq!(
            wanted.as_slice(),
            &[ObjectType::State, ObjectType::StateVisibility]
        );
        assert!(native_pack_required_for_pull(
            plan.want_full_closure,
            &plan.transfer_plan
        ));
    }

    #[derive(Clone)]
    struct SidecarOnlyPullService {
        state: StateId,
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

        type PushStream = tokio_stream::wrappers::ReceiverStream<Result<PushServerFrame, Status>>;

        async fn push(
            &self,
            _request: tonic::Request<tonic::Streaming<PushClientFrame>>,
        ) -> Result<Response<Self::PushStream>, Status> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )))
        }

        type PullStream = tokio_stream::wrappers::ReceiverStream<Result<PullServerFrame, Status>>;

        async fn pull(
            &self,
            request: tonic::Request<tonic::Streaming<PullClientFrame>>,
        ) -> Result<Response<Self::PullStream>, Status> {
            let state = self.state;
            let state_visibility_blob = self.state_visibility_blob.clone();
            let (tx, rx) = mpsc::channel(4);

            tokio::spawn(async move {
                let mut inbound = request.into_inner();
                if !matches!(
                    inbound.message().await,
                    Ok(Some(PullClientFrame {
                        frame: Some(pull_client_frame::Frame::Open(_)),
                    }))
                ) {
                    let _ = tx
                        .send(Err(Status::invalid_argument("expected signed pull open")))
                        .await;
                    return;
                }
                match inbound.message().await {
                    Ok(Some(PullClientFrame {
                        frame: Some(pull_client_frame::Frame::Request(_)),
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
                let ready = PullServerFrame {
                    frame: Some(pull_server_frame::Frame::Ready(PullReady {
                        remote_state: proto_state_id(state),
                        objects_to_fetch: vec![descriptor],
                        transfer: None,
                        partial_fetch_status: PartialFetchStatus::Disabled as i32,
                        missing_objects: Vec::new(),
                        full_closure_available: false,
                        object_count: 1,
                        remote_revision_address: RevisionAddress::heddle(state).to_string(),
                    })),
                };
                if tx.send(Ok(ready)).await.is_err() {
                    return;
                }

                match inbound.message().await {
                    Ok(Some(PullClientFrame {
                        frame: Some(pull_client_frame::Frame::Want(want)),
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

                let transfer = PullServerFrame {
                    frame: Some(pull_server_frame::Frame::StateVisibility(
                        StateVisibilityTransfer {
                            state_id: proto_state_id(state),
                            state_visibility_blob,
                        },
                    )),
                };
                if tx.send(Ok(transfer)).await.is_err() {
                    return;
                }

                let complete = PullServerFrame {
                    frame: Some(pull_server_frame::Frame::Complete(GrpcPullComplete {
                        success: true,
                        new_state: proto_state_id(state),
                        error: String::new(),
                        transfer: Some(TransferCheckpoint {
                            transfer_id: "sidecar-only-test".to_string(),
                            transport_mode: TransportMode::NativePack as i32,
                            resume_offset: 0,
                            chunk_index: 0,
                            checkpoint: b"heddle-markers-v1\n".to_vec(),
                            is_complete: true,
                        }),
                        new_revision_address: RevisionAddress::heddle(state).to_string(),
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

        let client = HostedGrpcClient::connect(addr, &signed_test_config())
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
        let state_id = state.state_id;
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
        let state_id = state.state_id;
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
        let state_id = state.state_id;
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
            Ok(_) => panic!("oversized sidecar PullClientFrame must be rejected at decode"),
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
        state: StateId,
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

        type PushStream = tokio_stream::wrappers::ReceiverStream<Result<PushServerFrame, Status>>;

        async fn push(
            &self,
            _request: tonic::Request<tonic::Streaming<PushClientFrame>>,
        ) -> Result<Response<Self::PushStream>, Status> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )))
        }

        type PullStream = tokio_stream::wrappers::ReceiverStream<Result<PullServerFrame, Status>>;

        async fn pull(
            &self,
            request: tonic::Request<tonic::Streaming<PullClientFrame>>,
        ) -> Result<Response<Self::PullStream>, Status> {
            let state = self.state;
            let pack_bundle = self.pack_bundle.clone();
            let state_visibility_blob = self.state_visibility_blob.clone();
            let (tx, rx) = mpsc::channel(8);

            tokio::spawn(async move {
                let mut inbound = request.into_inner();
                if !matches!(
                    inbound.message().await,
                    Ok(Some(PullClientFrame {
                        frame: Some(pull_client_frame::Frame::Open(_)),
                    }))
                ) {
                    let _ = tx
                        .send(Err(Status::invalid_argument("expected signed pull open")))
                        .await;
                    return;
                }
                match inbound.message().await {
                    Ok(Some(PullClientFrame {
                        frame: Some(pull_client_frame::Frame::Request(_)),
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

                let ready = PullServerFrame {
                    frame: Some(pull_server_frame::Frame::Ready(PullReady {
                        remote_state: proto_state_id(state),
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
                        remote_revision_address: RevisionAddress::heddle(state).to_string(),
                    })),
                };
                if tx.send(Ok(ready)).await.is_err() {
                    return;
                }

                match inbound.message().await {
                    Ok(Some(PullClientFrame {
                        frame: Some(pull_client_frame::Frame::Want(want)),
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

                let transfer = PullServerFrame {
                    frame: Some(pull_server_frame::Frame::StateVisibility(
                        StateVisibilityTransfer {
                            state_id: proto_state_id(state),
                            state_visibility_blob,
                        },
                    )),
                };
                if tx.send(Ok(transfer)).await.is_err() {
                    return;
                }

                let complete = PullServerFrame {
                    frame: Some(pull_server_frame::Frame::Complete(GrpcPullComplete {
                        success: true,
                        new_state: proto_state_id(state),
                        error: String::new(),
                        transfer: Some(TransferCheckpoint {
                            transfer_id: "state-and-visibility-test".to_string(),
                            transport_mode: TransportMode::NativePack as i32,
                            resume_offset: 0,
                            chunk_index: 0,
                            checkpoint: b"heddle-markers-v1\n".to_vec(),
                            is_complete: true,
                        }),
                        new_revision_address: RevisionAddress::heddle(state).to_string(),
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
    ) -> Vec<PullServerFrame> {
        let mut messages = Vec::new();
        let chunk_size = chunk_size.max(1);

        let pack_total_chunks = wire::chunk_count(bundle.pack_data.len(), chunk_size);
        for chunk_index in 0..pack_total_chunks.max(1) {
            let Some((start, len)) =
                wire::chunk_bounds(bundle.pack_data.len(), chunk_size, chunk_index)
            else {
                break;
            };
            messages.push(PullServerFrame {
                frame: Some(pull_server_frame::Frame::Pack(PackChunk {
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

        let index_total_chunks = wire::chunk_count(bundle.index_data.len(), chunk_size);
        for chunk_index in 0..index_total_chunks.max(1) {
            let Some((start, len)) =
                wire::chunk_bounds(bundle.index_data.len(), chunk_size, chunk_index)
            else {
                break;
            };
            messages.push(PullServerFrame {
                frame: Some(pull_server_frame::Frame::Pack(PackChunk {
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

        let client = HostedGrpcClient::connect(addr, &signed_test_config())
            .await
            .expect("connect client");
        Some((client, handle))
    }

    #[tokio::test]
    async fn state_and_visibility_same_state_id_pull_requests_pack_and_sidecar() {
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
        let state_id = state.state_id;
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
    fn missing_blobs_in_tree_treats_absent_tree_as_empty() {
        let (_dir, repo) = temp_repo();
        let absent_tree = ContentHash::from_bytes([99u8; 32]);

        let missing = wire::missing_blobs_in_tree(repo.store(), absent_tree)
            .expect("absent tree is not an error");

        assert!(missing.is_empty());
    }

    #[test]
    fn missing_blobs_in_tree_reports_only_genuinely_missing_blobs() {
        let (_dir, repo) = temp_repo();
        let present_blob = Blob::from("already local");
        let present_hash = repo.store().put_blob(&present_blob).expect("put blob");
        let missing_hash = ContentHash::from_bytes([42u8; 32]);
        let tree = Tree::from_entries(vec![
            TreeEntry::file("local.txt", present_hash, false).expect("present entry"),
            TreeEntry::file("remote.txt", missing_hash, false).expect("missing entry"),
        ]);
        let tree_hash = repo.store().put_tree(&tree).expect("put tree");

        let missing =
            wire::missing_blobs_in_tree(repo.store(), tree_hash).expect("collect missing blobs");

        assert_eq!(missing, vec![missing_hash]);
    }

    #[test]
    fn missing_blobs_in_tree_reports_corrupt_tree_read() {
        let (_dir, repo) = temp_repo();
        let tree_hash = repo.store().put_tree(&Tree::new()).expect("put tree");
        std::fs::write(loose_tree_path(&repo, &tree_hash), [0xc1]).expect("corrupt tree");
        repo.store().clear_recent_caches();

        let err = wire::missing_blobs_in_tree(repo.store(), tree_hash)
            .expect_err("corrupt tree must fail");

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

        let Some(push_client_frame::Frame::Redaction(transfer)) = message.frame else {
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
        let state = StateId::from_bytes([17u8; 32]);
        repo.put_state_visibility(sample_state_visibility(state))
            .expect("put state visibility");
        let expected_bytes = repo
            .get_state_visibility_bytes_for_state(&state)
            .expect("load sidecar")
            .expect("sidecar exists");

        let message =
            state_visibility_push_message(&repo, state_visibility_info(state)).expect("message");

        let Some(push_client_frame::Frame::StateVisibility(transfer)) = message.frame else {
            panic!("expected state visibility transfer");
        };
        assert_eq!(
            transfer
                .state_id
                .as_ref()
                .map(|state| state.value.as_slice()),
            Some(state.as_bytes().as_slice())
        );
        assert_eq!(transfer.state_visibility_blob, expected_bytes);
    }

    // ---- bare-pull head advertisement (delta sync) ----

    fn sample_attribution() -> Attribution {
        Attribution::human(Principal {
            name: "Grace Hopper".into(),
            email: "grace@example.com".into(),
        })
    }

    /// Put a state whose tree holds one blob into `repo`'s store. Returns the
    /// state id. With `parents`, builds a child on top of an existing state.
    fn put_state_with_blob(repo: &Repository, contents: &str, parents: Vec<StateId>) -> StateId {
        let blob = Blob::from(contents);
        let blob_hash = repo.store().put_blob(&blob).expect("put blob");
        let tree = Tree::from_entries(vec![
            TreeEntry::file(format!("{contents}.txt"), blob_hash, false).expect("tree entry"),
        ]);
        let tree_hash = repo.store().put_tree(&tree).expect("put tree");
        let state = State::new_snapshot(tree_hash, parents, sample_attribution());
        let state_id = state.state_id;
        repo.store().put_state(&state).expect("put state");
        state_id
    }

    #[test]
    fn locally_complete_pull_head_advertises_fully_present_thread_head() {
        let (_dir, repo) = temp_repo();
        let head = put_state_with_blob(&repo, "alpha", vec![]);
        repo.refs()
            .set_thread(&ThreadName::from("main"), &head)
            .expect("set thread");

        let advertised =
            locally_complete_pull_head(&repo, "main", None).expect("completeness check");
        assert_eq!(
            advertised,
            Some(head),
            "a thread head whose full closure is present locally must be advertised"
        );
    }

    #[test]
    fn locally_complete_pull_head_skips_unknown_thread() {
        // Unseeded local repo: the bare-pull thread has no local head, so
        // there is nothing to advertise and the pull falls back to a full
        // closure.
        let (_dir, repo) = temp_repo_unseeded();
        let advertised =
            locally_complete_pull_head(&repo, "nonexistent", None).expect("completeness check");
        assert_eq!(advertised, None);
    }

    #[test]
    fn locally_complete_pull_head_skips_when_target_state_override_in_play() {
        let (_dir, repo) = temp_repo();
        let head = put_state_with_blob(&repo, "alpha", vec![]);
        repo.refs()
            .set_thread(&ThreadName::from("main"), &head)
            .expect("set thread");

        // A fetch_state-style override drives the want plan directly; the
        // thread head is unrelated to what's being fetched.
        let advertised =
            locally_complete_pull_head(&repo, "main", Some(head)).expect("completeness check");
        assert_eq!(advertised, None);
    }

    #[test]
    fn locally_complete_pull_head_skips_repo_with_missing_blobs() {
        // A partial/lazy clone records known-missing blobs: it may hold a
        // state's metadata while lacking blob content. Advertising such a
        // head would make the server omit objects and silently leave us with
        // an incomplete repo — so the completeness gate must refuse.
        let (_dir, repo) = temp_repo();
        let head = put_state_with_blob(&repo, "alpha", vec![]);
        repo.refs()
            .set_thread(&ThreadName::from("main"), &head)
            .expect("set thread");
        repo.record_missing_blob(ContentHash::from_bytes([88u8; 32]))
            .expect("record missing blob");

        let advertised =
            locally_complete_pull_head(&repo, "main", None).expect("completeness check");
        assert_eq!(
            advertised, None,
            "a repo carrying missing blobs must never advertise a head"
        );
    }

    #[test]
    fn locally_complete_pull_head_skips_head_with_incomplete_closure() {
        // The thread head's *state* is present, but a parent state in its
        // closure is absent. Walking the closure surfaces `ObjectNotFound`,
        // so the head must NOT be advertised. This is the dangerous case the
        // cardinal correctness constraint guards against.
        let (_dir, repo) = temp_repo();
        let absent_parent = StateId::from_bytes([21; 32]);
        let blob = Blob::from("orphan");
        let blob_hash = repo.store().put_blob(&blob).expect("put blob");
        let tree = Tree::from_entries(vec![
            TreeEntry::file("orphan.txt", blob_hash, false).expect("tree entry"),
        ]);
        let tree_hash = repo.store().put_tree(&tree).expect("put tree");
        // Child references a parent state that is NOT in the store.
        let child = State::new_snapshot(tree_hash, vec![absent_parent], sample_attribution());
        let child_id = child.state_id;
        repo.store().put_state(&child).expect("put child state");
        repo.refs()
            .set_thread(&ThreadName::from("main"), &child_id)
            .expect("set thread");

        let advertised =
            locally_complete_pull_head(&repo, "main", None).expect("completeness check");
        assert_eq!(
            advertised, None,
            "a head whose closure has an absent parent state must not be advertised"
        );
    }

    #[test]
    fn locally_complete_local_thread_head_advertises_fully_present_thread_head() {
        // The explicit `--local-thread` happy path: the named thread's head
        // closure is fully local, so it may be advertised (fast delta path).
        let (_dir, repo) = temp_repo();
        let head = put_state_with_blob(&repo, "alpha", vec![]);
        repo.refs()
            .set_thread(&ThreadName::from("feature"), &head)
            .expect("set thread");

        let advertised =
            locally_complete_local_thread_head(&repo, "feature", None).expect("completeness check");
        assert_eq!(
            advertised,
            Some(head),
            "an explicit local-thread head whose full closure is present must be advertised"
        );
    }

    #[test]
    fn locally_complete_local_thread_head_skips_unknown_thread() {
        // `--local-thread` naming a thread with no local head: nothing to
        // advertise, falls back to a full closure.
        let (_dir, repo) = temp_repo_unseeded();
        let advertised = locally_complete_local_thread_head(&repo, "nonexistent", None)
            .expect("completeness check");
        assert_eq!(advertised, None);
    }

    #[test]
    fn locally_complete_local_thread_head_skips_when_target_state_override_in_play() {
        let (_dir, repo) = temp_repo();
        let head = put_state_with_blob(&repo, "alpha", vec![]);
        repo.refs()
            .set_thread(&ThreadName::from("feature"), &head)
            .expect("set thread");

        // A target-state override drives the want plan directly; the explicit
        // thread head is unrelated and must not be advertised.
        let advertised = locally_complete_local_thread_head(&repo, "feature", Some(head))
            .expect("completeness check");
        assert_eq!(advertised, None);
    }

    #[test]
    fn locally_complete_local_thread_head_skips_repo_with_missing_blobs() {
        // A partial/lazy clone named via `--local-thread`: holds metadata but
        // records known-missing blobs. The gate must refuse.
        let (_dir, repo) = temp_repo();
        let head = put_state_with_blob(&repo, "alpha", vec![]);
        repo.refs()
            .set_thread(&ThreadName::from("feature"), &head)
            .expect("set thread");
        repo.record_missing_blob(ContentHash::from_bytes([88u8; 32]))
            .expect("record missing blob");

        let advertised =
            locally_complete_local_thread_head(&repo, "feature", None).expect("completeness check");
        assert_eq!(
            advertised, None,
            "a repo carrying missing blobs must never advertise an explicit local-thread head"
        );
    }

    #[test]
    fn locally_complete_local_thread_head_skips_head_with_incomplete_closure() {
        // The cardinal case for the explicit branch: the named thread's head
        // state is present, but a parent state in its closure is absent — an
        // interrupted prior pull or partial clone. Advertising this head would
        // make the server prune objects we lack. The gate must refuse.
        let (_dir, repo) = temp_repo();
        let absent_parent = StateId::from_bytes([22; 32]);
        let blob = Blob::from("orphan");
        let blob_hash = repo.store().put_blob(&blob).expect("put blob");
        let tree = Tree::from_entries(vec![
            TreeEntry::file("orphan.txt", blob_hash, false).expect("tree entry"),
        ]);
        let tree_hash = repo.store().put_tree(&tree).expect("put tree");
        let child = State::new_snapshot(tree_hash, vec![absent_parent], sample_attribution());
        let child_id = child.state_id;
        repo.store().put_state(&child).expect("put child state");
        repo.refs()
            .set_thread(&ThreadName::from("feature"), &child_id)
            .expect("set thread");

        let advertised =
            locally_complete_local_thread_head(&repo, "feature", None).expect("completeness check");
        assert_eq!(
            advertised, None,
            "an explicit local-thread head whose closure has an absent parent must not be advertised"
        );
    }

    /// Mock pull service that mimics the weft server's `exclude_states`
    /// contract: it captures the inbound `exclude_states`, fires the
    /// zero-delta short-circuit when the remote tip is advertised, and
    /// otherwise sends only the objects NOT covered by an advertised
    /// (parent) closure.
    #[derive(Clone)]
    struct DeltaAwarePullService {
        remote_state: StateId,
        /// Object descriptors keyed by the parent state that an advertised
        /// `exclude_states` entry would cover. If `exclude_states` contains
        /// `remote_state`, the delta is empty (short-circuit). If it contains
        /// a known parent, only the child-specific objects are sent. If it
        /// contains neither, the full closure is sent.
        full_closure: Vec<ObjectInfo>,
        delta_objects: Vec<ObjectInfo>,
        known_parent: StateId,
        full_pack: wire::NativePackBundle,
        delta_pack: wire::NativePackBundle,
        captured_exclude: std::sync::Arc<std::sync::Mutex<Option<Vec<ProtoStateId>>>>,
    }

    #[tonic::async_trait]
    impl RepoSyncService for DeltaAwarePullService {
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

        type PushStream = tokio_stream::wrappers::ReceiverStream<Result<PushServerFrame, Status>>;

        async fn push(
            &self,
            _request: tonic::Request<tonic::Streaming<PushClientFrame>>,
        ) -> Result<Response<Self::PushStream>, Status> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )))
        }

        type PullStream = tokio_stream::wrappers::ReceiverStream<Result<PullServerFrame, Status>>;

        async fn pull(
            &self,
            request: tonic::Request<tonic::Streaming<PullClientFrame>>,
        ) -> Result<Response<Self::PullStream>, Status> {
            let svc = self.clone();
            let (tx, rx) = mpsc::channel(16);

            tokio::spawn(async move {
                let mut inbound = request.into_inner();
                if !matches!(
                    inbound.message().await,
                    Ok(Some(PullClientFrame {
                        frame: Some(pull_client_frame::Frame::Open(_)),
                    }))
                ) {
                    let _ = tx
                        .send(Err(Status::invalid_argument("expected signed pull open")))
                        .await;
                    return;
                }
                let exclude = match inbound.message().await {
                    Ok(Some(PullClientFrame {
                        frame: Some(pull_client_frame::Frame::Request(req)),
                    })) => req.exclude_states,
                    other => {
                        let _ = tx
                            .send(Err(Status::invalid_argument(format!(
                                "expected pull request, got {other:?}"
                            ))))
                            .await;
                        return;
                    }
                };
                *svc.captured_exclude.lock().unwrap() = Some(exclude.clone());

                let remote_proto = proto_state_id(svc.remote_state).expect("valid remote state");
                let parent_proto = proto_state_id(svc.known_parent).expect("valid parent state");

                // Mirror the server contract: subtract advertised closures.
                let (objects, pack, short_circuit) = if exclude.contains(&remote_proto) {
                    // weft#215 zero-delta short-circuit: client is at the tip.
                    (Vec::new(), None, true)
                } else if exclude.contains(&parent_proto) {
                    // Client is behind at `known_parent`; send only the delta.
                    (
                        svc.delta_objects.clone(),
                        Some(svc.delta_pack.clone()),
                        false,
                    )
                } else {
                    // No usable advertisement; send the full closure.
                    (svc.full_closure.clone(), Some(svc.full_pack.clone()), false)
                };

                let descriptors: Vec<_> = objects
                    .iter()
                    .map(|info| {
                        object_descriptor_with_status(
                            info,
                            ObjectAvailabilityStatus::Missing,
                            "requested by client",
                        )
                    })
                    .collect();

                let ready = PullServerFrame {
                    frame: Some(pull_server_frame::Frame::Ready(PullReady {
                        remote_state: proto_state_id(svc.remote_state),
                        objects_to_fetch: descriptors,
                        transfer: None,
                        partial_fetch_status: PartialFetchStatus::Disabled as i32,
                        missing_objects: Vec::new(),
                        full_closure_available: false,
                        object_count: objects.len() as u32,
                        remote_revision_address: RevisionAddress::heddle(svc.remote_state)
                            .to_string(),
                    })),
                };
                if tx.send(Ok(ready)).await.is_err() {
                    return;
                }

                // Expect the client's Want.
                match inbound.message().await {
                    Ok(Some(PullClientFrame {
                        frame: Some(pull_client_frame::Frame::Want(_)),
                    })) => {}
                    other => {
                        let _ = tx
                            .send(Err(Status::invalid_argument(format!(
                                "expected want, got {other:?}"
                            ))))
                            .await;
                        return;
                    }
                }

                if let Some(pack) = pack
                    && !short_circuit
                {
                    for message in encode_pull_native_pack_messages(&pack, "delta-aware-test", 64) {
                        if tx.send(Ok(message)).await.is_err() {
                            return;
                        }
                    }
                }

                let complete = PullServerFrame {
                    frame: Some(pull_server_frame::Frame::Complete(GrpcPullComplete {
                        success: true,
                        new_state: proto_state_id(svc.remote_state),
                        error: String::new(),
                        transfer: Some(TransferCheckpoint {
                            transfer_id: "delta-aware-test".to_string(),
                            transport_mode: TransportMode::NativePack as i32,
                            resume_offset: 0,
                            chunk_index: 0,
                            checkpoint: b"heddle-markers-v1\n".to_vec(),
                            is_complete: true,
                        }),
                        new_revision_address: RevisionAddress::heddle(svc.remote_state).to_string(),
                    })),
                };
                let _ = tx.send(Ok(complete)).await;
            });

            Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
                rx,
            )))
        }
    }

    async fn connect_delta_aware_service(
        service: DeltaAwarePullService,
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
                .expect("serve delta-aware test service");
        });
        let client = HostedGrpcClient::connect(addr, &signed_test_config())
            .await
            .expect("connect client");
        Some((client, handle))
    }

    #[tokio::test]
    async fn warm_bare_pull_advertises_head_and_fires_zero_delta_short_circuit() {
        // Client is exactly at the remote tip. A bare pull must advertise that
        // tip as exclude_states; the server then returns an empty delta
        // (weft#215 short-circuit) and the client transfers nothing.
        let (_dir, repo) = temp_repo();
        let head = put_state_with_blob(&repo, "tip", vec![]);
        repo.refs()
            .set_thread(&ThreadName::from("main"), &head)
            .expect("set thread");

        let full_closure =
            wire::enumerate_state_closure(repo.store(), head).expect("enumerate closure");
        let full_pack =
            wire::build_native_pack(repo.store(), &full_closure).expect("build full pack");
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));

        let Some((mut client, server)) = connect_delta_aware_service(DeltaAwarePullService {
            remote_state: head,
            full_closure,
            delta_objects: Vec::new(),
            known_parent: StateId::from_bytes([23; 32]),
            full_pack: full_pack.clone(),
            delta_pack: full_pack,
            captured_exclude: captured.clone(),
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
                    target_state: None,
                    materialization: PullMaterialization::Full,
                },
            ),
        )
        .await
        .expect("warm bare pull must not hang")
        .expect("warm bare pull succeeds");
        server.abort();

        let advertised = captured.lock().unwrap().clone().expect("request captured");
        assert_eq!(
            advertised,
            vec![proto_state_id(head).expect("valid state")],
            "a bare pull at the tip must advertise that tip in exclude_states"
        );
        assert!(exchange.result.success);
        assert_eq!(
            exchange.object_count, 0,
            "the zero-delta short-circuit must transfer no objects, not the full closure"
        );
    }

    #[tokio::test]
    async fn behind_client_bare_pull_receives_exactly_the_missing_delta() {
        // The dangerous direction: client sits at the PARENT state, remote is
        // at the CHILD. A bare pull advertises the parent (whose closure the
        // client fully holds); the server sends only the child-specific
        // objects, and the client must end up with a COMPLETE repo (no object
        // it lacked is dropped).
        // Build parent + child in the SOURCE repo (the server's view).
        let (_src_dir, src_repo) = temp_repo_unseeded();
        let parent = put_state_with_blob(&src_repo, "base", vec![]);
        let child = put_state_with_blob(&src_repo, "feature", vec![parent]);
        let child_closure =
            wire::enumerate_state_closure(src_repo.store(), child).expect("child closure");

        // The CLIENT holds exactly the parent closure (the dangerous "behind"
        // setup): copy the parent's objects into a fresh client store and
        // track `main` at the parent.
        let (_dir, repo) = temp_repo_unseeded();
        let parent_closure =
            wire::enumerate_state_closure(src_repo.store(), parent).expect("parent closure");
        let parent_pack =
            wire::build_native_pack(src_repo.store(), &parent_closure).expect("parent pack");
        wire::install_received_pack(
            repo.store(),
            &parent_pack.pack_data,
            &parent_pack.index_data,
        )
        .expect("install parent closure into client");
        repo.refs()
            .set_thread(&ThreadName::from("main"), &parent)
            .expect("set thread to parent");
        // Sanity: the client provably holds the parent's full closure...
        wire::enumerate_state_closure(repo.store(), parent).expect("client holds parent closure");
        // ...but NOT the child yet.
        assert!(
            repo.store()
                .get_state(&child)
                .expect("probe child")
                .is_none(),
            "client must start without the child state"
        );
        let parent_clone = parent;
        let full_pack =
            wire::build_native_pack(src_repo.store(), &child_closure).expect("full pack");
        // Delta = child closure minus the parent closure (what the server
        // would send when the parent is advertised).
        let delta_objects = wire::enumerate_state_closure_with_options(
            src_repo.store(),
            child,
            wire::StateClosureOptions {
                depth: None,
                exclude_states: vec![parent_clone],
            },
        )
        .expect("delta closure");
        assert!(
            !delta_objects.is_empty() && delta_objects.len() < child_closure.len(),
            "delta must be a strict, non-empty subset of the full closure"
        );
        let delta_pack =
            wire::build_native_pack(src_repo.store(), &delta_objects).expect("delta pack");

        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let Some((mut client, server)) = connect_delta_aware_service(DeltaAwarePullService {
            remote_state: child,
            full_closure: child_closure.clone(),
            delta_objects: delta_objects.clone(),
            known_parent: parent_clone,
            full_pack,
            delta_pack,
            captured_exclude: captured.clone(),
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
                    target_state: None,
                    materialization: PullMaterialization::Full,
                },
            ),
        )
        .await
        .expect("behind-client pull must not hang")
        .expect("behind-client pull succeeds");
        server.abort();

        let advertised = captured.lock().unwrap().clone().expect("request captured");
        assert_eq!(
            advertised,
            vec![proto_state_id(parent).expect("valid state")],
            "a behind client must advertise the parent head it holds"
        );
        assert!(exchange.result.success);
        // The client must now hold the COMPLETE child closure — every object
        // in the remote's full closure must be present locally, proving the
        // server-side parent-pruning dropped nothing the client lacked.
        let reassembled = wire::enumerate_state_closure(repo.store(), child)
            .expect("the client must hold the complete child closure after a delta pull");
        assert_eq!(
            reassembled.len(),
            child_closure.len(),
            "delta pull must leave the client with the full child closure, no gaps"
        );
    }

    #[tokio::test]
    async fn fresh_bare_pull_advertises_nothing_and_gets_full_closure() {
        // Unseeded local repo (the `heddle clone` shape): no local head for
        // the thread, so nothing is advertised and the server sends the full
        // closure (today's behavior).
        let (_dir, repo) = temp_repo_unseeded();
        let (_src_dir, src_repo) = temp_repo_unseeded();
        let remote = put_state_with_blob(&src_repo, "seed", vec![]);
        let full_closure =
            wire::enumerate_state_closure(src_repo.store(), remote).expect("closure");
        let full_pack =
            wire::build_native_pack(src_repo.store(), &full_closure).expect("full pack");

        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let Some((mut client, server)) = connect_delta_aware_service(DeltaAwarePullService {
            remote_state: remote,
            full_closure: full_closure.clone(),
            delta_objects: Vec::new(),
            known_parent: StateId::from_bytes([24; 32]),
            full_pack: full_pack.clone(),
            delta_pack: full_pack,
            captured_exclude: captured.clone(),
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
                    target_state: None,
                    materialization: PullMaterialization::Full,
                },
            ),
        )
        .await
        .expect("fresh bare pull must not hang")
        .expect("fresh bare pull succeeds");
        server.abort();

        let advertised = captured.lock().unwrap().clone().expect("request captured");
        assert!(
            advertised.is_empty(),
            "a fresh repo must advertise no exclude_states"
        );
        assert!(exchange.result.success);
        assert_eq!(
            exchange.object_count,
            full_closure.len(),
            "a fresh pull must receive the full closure, nothing wrongly excluded"
        );
        assert!(
            wire::enumerate_state_closure(repo.store(), remote).is_ok(),
            "fresh pull must install the complete closure"
        );
    }

    #[tokio::test]
    async fn explicit_local_thread_incomplete_closure_does_not_advertise_and_repairs() {
        // The footgun this PR closes: a user passes `--local-thread` against a
        // repo whose named thread head has an INCOMPLETE closure (an absent
        // parent state — a partial clone or interrupted prior pull). The
        // explicit branch must NOT advertise it (that would make the server
        // prune objects the client lacks and silently leave it corrupt). The
        // completeness gate refuses, the pull falls back to the full closure,
        // and the client ends COMPLETE.
        //
        // Build parent + child in the SOURCE repo (the server's view).
        let (_src_dir, src_repo) = temp_repo_unseeded();
        let parent = put_state_with_blob(&src_repo, "base", vec![]);
        let child = put_state_with_blob(&src_repo, "feature", vec![parent]);
        let child_closure =
            wire::enumerate_state_closure(src_repo.store(), child).expect("child closure");
        let full_pack =
            wire::build_native_pack(src_repo.store(), &child_closure).expect("full pack");

        // The CLIENT's `feature` thread points at `child`, but the client only
        // holds child's own metadata, NOT the parent closure — an incomplete
        // local closure. Copy just the child-specific objects (closure minus
        // parent) so the parent state is genuinely absent.
        let (_dir, repo) = temp_repo_unseeded();
        let child_only = wire::enumerate_state_closure_with_options(
            src_repo.store(),
            child,
            wire::StateClosureOptions {
                depth: None,
                exclude_states: vec![parent],
            },
        )
        .expect("child-only objects");
        let child_only_pack =
            wire::build_native_pack(src_repo.store(), &child_only).expect("child-only pack");
        wire::install_received_pack(
            repo.store(),
            &child_only_pack.pack_data,
            &child_only_pack.index_data,
        )
        .expect("install child-only objects");
        repo.refs()
            .set_thread(&ThreadName::from("feature"), &child)
            .expect("set local thread to child");
        // Sanity: the named thread's closure is INCOMPLETE locally (parent
        // state absent), exactly the dangerous over-advertise setup.
        assert!(
            matches!(
                wire::enumerate_state_closure(repo.store(), child),
                Err(ProtocolError::ObjectNotFound(_))
            ),
            "the explicit thread head's closure must start incomplete"
        );

        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let Some((mut client, server)) = connect_delta_aware_service(DeltaAwarePullService {
            remote_state: child,
            full_closure: child_closure.clone(),
            delta_objects: Vec::new(),
            known_parent: parent,
            full_pack: full_pack.clone(),
            delta_pack: full_pack,
            captured_exclude: captured.clone(),
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
                    local_thread: Some("feature"),
                    depth: None,
                    target_state: None,
                    materialization: PullMaterialization::Full,
                },
            ),
        )
        .await
        .expect("explicit-thread pull must not hang")
        .expect("explicit-thread pull succeeds");
        server.abort();

        let advertised = captured.lock().unwrap().clone().expect("request captured");
        assert!(
            advertised.is_empty(),
            "an explicit --local-thread with an incomplete closure must advertise nothing, \
             falling back to a full pull (got {advertised:?})"
        );
        assert!(exchange.result.success);
        // The repair: the client must now hold the COMPLETE child closure.
        let reassembled = wire::enumerate_state_closure(repo.store(), child)
            .expect("the client must hold the complete child closure after the fallback full pull");
        assert_eq!(
            reassembled.len(),
            child_closure.len(),
            "the full-pull fallback must leave the client with the complete closure, no gaps"
        );
    }

    #[tokio::test]
    async fn explicit_local_thread_complete_closure_advertises_and_fires_short_circuit() {
        // Fast path preserved: an explicit `--local-thread` whose head closure
        // IS fully local must still advertise that head, so the server can
        // prune to the delta (here the zero-delta short-circuit, since the
        // client is at the remote tip).
        let (_dir, repo) = temp_repo();
        let head = put_state_with_blob(&repo, "tip", vec![]);
        repo.refs()
            .set_thread(&ThreadName::from("feature"), &head)
            .expect("set local thread");
        // Sanity: the named thread's closure is fully present locally.
        wire::enumerate_state_closure(repo.store(), head)
            .expect("client holds the complete thread closure");

        let full_closure =
            wire::enumerate_state_closure(repo.store(), head).expect("enumerate closure");
        let full_pack =
            wire::build_native_pack(repo.store(), &full_closure).expect("build full pack");
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));

        let Some((mut client, server)) = connect_delta_aware_service(DeltaAwarePullService {
            remote_state: head,
            full_closure,
            delta_objects: Vec::new(),
            known_parent: StateId::from_bytes([25; 32]),
            full_pack: full_pack.clone(),
            delta_pack: full_pack,
            captured_exclude: captured.clone(),
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
                    local_thread: Some("feature"),
                    depth: None,
                    target_state: None,
                    materialization: PullMaterialization::Full,
                },
            ),
        )
        .await
        .expect("explicit-thread pull must not hang")
        .expect("explicit-thread pull succeeds");
        server.abort();

        let advertised = captured.lock().unwrap().clone().expect("request captured");
        assert_eq!(
            advertised,
            vec![proto_state_id(head).expect("valid state")],
            "an explicit --local-thread with a complete closure must advertise its head"
        );
        assert!(exchange.result.success);
        assert_eq!(
            exchange.object_count, 0,
            "advertising the complete head must fire the zero-delta short-circuit, not a full pull"
        );
    }
}
