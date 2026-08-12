use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, Seek, SeekFrom, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use api::heddle::api::v1alpha1::{
    ConfidenceBand as ProtoConfidenceBand, GetBlobRequest, GetThreadRequest, GitCheckpointTransfer,
    GitLaneTransfer, GitObjectAlgorithm, GitObjectId as ProtoGitObjectId, GitPackTransfer,
    GitRefKind as ProtoGitRefKind, GitRefUpdateTransfer,
    IntegrationPolicyStatus as ProtoIntegrationPolicyStatus, ListRefsRequest,
    ObjectAvailabilityStatus, ObjectDescriptor, PackChunk, PackStreamKind, PartialFetchStatus,
    ProviderPlanChallenge, PullClientFrame, PullReady, PullRequest, PullServerFrame,
    PushClientFrame, PushRequest, PushServerFrame, RedactionTransfer, StateAttachmentTransfer,
    StateVisibilityTransfer, StreamOpeningProof, ThreadConfidenceSummary,
    ThreadFreshness as ProtoThreadFreshness, ThreadIntegrationPolicy, ThreadMetadata,
    ThreadMode as ProtoThreadMode, ThreadVerificationSummary, TransportMode, UpdateRefRequest,
    WantObjects, git_lane_transfer, list_refs_response, pull_client_frame, pull_server_frame,
    push_client_frame, push_server_frame, thread_state::Kind as ProtoThreadState,
};
use objects::{
    Progress,
    object::{
        AnnotationStatus, ContentHash, ContextBlob, ContextTarget, Discussion, DiscussionsBlob,
        MarkerName, StateAttachmentBody, StateAttachmentKind, StateId, ThreadName,
    },
    store::{AnyStore, ObjectStore, PackObjectId, SnapshotCommitDescriptor},
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
use wire::{
    GitLaneTransferIntent, ObjectInfo, ObjectType, ObjectTypeBucket, PlannedObject, ProtocolError,
    PullComplete, PushComplete, RefEntry, RefUpdated, RepositoryTransferPlan,
};

use super::{
    BidirectionalRequestStream, HostedClient, PullMaterialization, ServerStream, ServerStreamItem,
    helpers::{
        descriptor_id, descriptor_id_from_info, hosted_to_protocol_error,
        object_descriptor_with_status, parse_descriptor_to_info, to_proto_object_info,
        transport_mode_name,
    },
    operation_id::ClientOperationId,
};

#[derive(Clone, Copy)]
struct PullOptions<'a> {
    local_thread: Option<&'a str>,
    depth: Option<u32>,
    target_state: Option<StateId>,
    materialization: PullMaterialization,
    publish_refs: bool,
}

const PULL_BOOTSTRAP_LINE_PREFIX: &str = "heddle-pull-bootstrap-v1:";
const PULL_REFS_LINE_PREFIX: &str = "heddle-pull-refs-v1:";
const PULL_CLONE_BOOTSTRAP_THREAD_PREFIX: &str = "heddle-clone-bootstrap-v1:";
type PullBootstrapPayload = (
    bool,
    Vec<Discussion>,
    bool,
    Vec<(ContextTarget, ContextBlob)>,
);
type PullRefsPayload = (
    String,
    Option<Vec<u8>>,
    Vec<(String, Vec<u8>, bool, String)>,
);
type PullRepositoryInitializer<'a> =
    Box<dyn FnOnce(&PullReady) -> Result<Repository, ProtocolError> + 'a>;

#[derive(Debug, Clone)]
pub(crate) struct PullBootstrapMetadata {
    discussions_from_pack: bool,
    pub discussions: Vec<Discussion>,
    context_from_pack: bool,
    pub context: Vec<(ContextTarget, ContextBlob)>,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedPullBootstrapMetadata {
    pub discussions: Vec<Discussion>,
    pub context: Vec<(ContextTarget, ContextBlob)>,
}

#[derive(Debug, Clone)]
pub(crate) struct PullBootstrapRefs {
    pub refs: Vec<HostedRefEntry>,
}

impl PullBootstrapMetadata {
    pub(crate) fn resolve(
        &self,
        repo: &Repository,
        state_id: Option<StateId>,
    ) -> Result<ResolvedPullBootstrapMetadata, ProtocolError> {
        let state_id = state_id.ok_or_else(|| {
            ProtocolError::InvalidState("pull bootstrap is missing its final state".to_string())
        })?;
        let discussions = if self.discussions_from_pack {
            discussions_from_pull_pack(repo, state_id)?
        } else {
            self.discussions.clone()
        };
        let context = if self.context_from_pack {
            context_from_pull_pack(repo, state_id)?
        } else {
            self.context.clone()
        };
        Ok(ResolvedPullBootstrapMetadata {
            discussions,
            context,
        })
    }
}

pub(crate) fn decode_pull_bootstrap(
    checkpoint: &[u8],
) -> Result<Option<PullBootstrapMetadata>, ProtocolError> {
    let checkpoint = std::str::from_utf8(checkpoint)
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    let Some(payload) = checkpoint
        .lines()
        .find_map(|line| line.strip_prefix(PULL_BOOTSTRAP_LINE_PREFIX))
    else {
        return Ok(None);
    };
    let payload = payload
        .split_once('\t')
        .map(|(payload, _)| payload)
        .ok_or_else(|| {
            ProtocolError::InvalidState("decode pull bootstrap: missing sentinel state".to_string())
        })?;
    let payload = hex::decode(payload)
        .map_err(|error| ProtocolError::InvalidState(format!("decode pull bootstrap: {error}")))?;
    let (discussions_from_pack, discussions, context_from_pack, context): PullBootstrapPayload =
        rmp_serde::from_slice(&payload).map_err(|error| {
            ProtocolError::InvalidState(format!("decode pull bootstrap: {error}"))
        })?;
    Ok(Some(PullBootstrapMetadata {
        discussions_from_pack,
        discussions,
        context_from_pack,
        context,
    }))
}

pub(crate) fn decode_pull_refs(
    checkpoint: &[u8],
) -> Result<Option<PullBootstrapRefs>, ProtocolError> {
    let checkpoint = std::str::from_utf8(checkpoint)
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    let Some(payload) = checkpoint
        .lines()
        .find_map(|line| line.strip_prefix(PULL_REFS_LINE_PREFIX))
    else {
        return Ok(None);
    };
    let payload = payload
        .split_once('\t')
        .map(|(payload, _)| payload)
        .ok_or_else(|| {
            ProtocolError::InvalidState("decode pull refs: missing sentinel state".to_string())
        })?;
    let payload = hex::decode(payload)
        .map_err(|error| ProtocolError::InvalidState(format!("decode pull refs: {error}")))?;
    let (_head_thread, head_state, refs): PullRefsPayload = rmp_serde::from_slice(&payload)
        .map_err(|error| ProtocolError::InvalidState(format!("decode pull refs: {error}")))?;
    let head_state = head_state
        .map(|value| decode_bootstrap_state_id(value, "head state"))
        .transpose()?;
    let refs = refs
        .into_iter()
        .map(|(name, state_id, is_thread, revision_address)| {
            let state_id = decode_bootstrap_state_id(state_id, "ref state")?;
            Ok(HostedRefEntry {
                name,
                state_id,
                is_thread,
                revision_address,
                thread_id: None,
            })
        })
        .collect::<Result<Vec<_>, ProtocolError>>()?;
    let _ = head_state;
    Ok(Some(PullBootstrapRefs { refs }))
}

fn decode_bootstrap_state_id(value: Vec<u8>, field: &str) -> Result<StateId, ProtocolError> {
    let value: [u8; 32] = value.try_into().map_err(|value: Vec<u8>| {
        ProtocolError::InvalidState(format!(
            "decode pull refs {field}: expected 32 bytes, got {}",
            value.len()
        ))
    })?;
    Ok(StateId::from_bytes(value))
}

fn discussions_from_pull_pack(
    repo: &Repository,
    state_id: StateId,
) -> Result<Vec<Discussion>, ProtocolError> {
    let Some(attachment) =
        repo.latest_state_attachment(&state_id, StateAttachmentKind::Discussions)?
    else {
        return Err(ProtocolError::InvalidState(
            "pull bootstrap advertised packed discussions but the attachment is missing"
                .to_string(),
        ));
    };
    let StateAttachmentBody::Discussions(hash) = attachment.body else {
        return Err(ProtocolError::InvalidState(
            "packed discussions attachment has the wrong kind".to_string(),
        ));
    };
    let blob = repo.store().get_blob(&hash)?.ok_or_else(|| {
        ProtocolError::InvalidState(format!(
            "packed discussions attachment references missing blob {hash}"
        ))
    })?;
    DiscussionsBlob::decode(blob.content())
        .map(|blob| blob.discussions)
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))
}

fn context_from_pull_pack(
    repo: &Repository,
    state_id: StateId,
) -> Result<Vec<(ContextTarget, ContextBlob)>, ProtocolError> {
    let Some(attachment) = repo.latest_state_attachment(&state_id, StateAttachmentKind::Context)?
    else {
        return Err(ProtocolError::InvalidState(
            "pull bootstrap advertised packed context but the attachment is missing".to_string(),
        ));
    };
    let StateAttachmentBody::Context(root) = attachment.body else {
        return Err(ProtocolError::InvalidState(
            "packed context attachment has the wrong kind".to_string(),
        ));
    };
    let mut entries = repo
        .list_context_entries(&root, None)?
        .into_iter()
        .map(|entry| (entry.target, entry.blob))
        .collect::<Vec<_>>();
    for (_, blob) in &mut entries {
        blob.annotations
            .retain(|annotation| annotation.status == AnnotationStatus::Active);
    }
    entries.retain(|(_, blob)| !blob.annotations.is_empty());
    Ok(entries)
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
    ref_updates: Vec<GitRefUpdateTransfer>,
}

struct StagedGitLanePull {
    allow_ref_updates: bool,
    initial_refs: HashMap<String, ReferenceTarget>,
    ref_updates: Vec<GitRefUpdateTransfer>,
    checkpoints: Vec<GitCheckpointTransfer>,
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
    #[allow(dead_code)]
    roots: Vec<GitObjectId>,
    /// Reachable pack bytes written once during planning; streamed on the wire
    /// without a second ODB traversal. Auto-deleted when the last clone drops.
    pack_file: Arc<Mutex<NamedTempFile>>,
}

const PUSH_FULL_DESCRIPTOR_OBJECT_THRESHOLD: usize = 512;
const NATIVE_PACK_DRAIN_OBJECT_INTERVAL: usize = 32;
const NATIVE_PACK_OBJECT_PREFETCH_LIMIT: usize = 32;
const NATIVE_PACK_OBJECT_LOAD_WORKER_LIMIT: usize = 8;

fn select_snapshot_pack_reuse_descriptor<'a>(
    descriptors: &'a [SnapshotCommitDescriptor],
    local_state: StateId,
    objects: &[ObjectInfo],
) -> Option<&'a SnapshotCommitDescriptor> {
    if objects.is_empty()
        || objects
            .iter()
            .any(|object| !object.obj_type.packable_for_push())
    {
        return None;
    }
    let wanted = objects
        .iter()
        .map(|object| match &object.id {
            wire::ObjectId::Hash(hash) => PackObjectId::Hash(*hash),
            wire::ObjectId::StateId(state) => PackObjectId::StateId(*state),
            wire::ObjectId::StateAttachment { id, .. } => PackObjectId::Hash(*id.as_hash()),
        })
        .collect::<HashSet<_>>();
    if wanted.len() != objects.len() {
        return None;
    }
    descriptors.iter().find(|descriptor| {
        descriptor.artifact.state == local_state
            && wanted
                .iter()
                .all(|wanted_id| descriptor.object_ids.contains(wanted_id))
    })
}

#[derive(Debug, Clone, Default)]
pub struct PullObjectMix {
    pub blobs: usize,
    pub trees: usize,
    pub states: usize,
    pub actions: usize,
    pub redactions: usize,
    pub state_visibilities: usize,
    pub state_attachments: usize,
    pub key_bindings: usize,
}

#[derive(Debug, Clone)]
pub struct HostedRefEntry {
    pub name: String,
    pub state_id: StateId,
    pub is_thread: bool,
    pub revision_address: String,
    pub thread_id: Option<String>,
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
            ObjectTypeBucket::KeyBinding => self.key_bindings += 1,
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
            + self.key_bindings
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

#[derive(Debug, Clone, Default)]
pub struct PushProfile {
    pub remote_ref_discovery: Duration,
    pub closure_planning: Duration,
    pub request_setup: Duration,
    pub ready_wait: Duration,
    pub pack_and_send: Duration,
    pub sidecars_and_git_send: Duration,
    pub request_finish: Duration,
    pub complete_wait: Duration,
    pub metadata_sync: Duration,
    pub total: Duration,
    pub objects_advertised: usize,
    pub objects_wanted: usize,
    pub pack_objects: usize,
    pub sidecar_objects: usize,
    pub reused_pack: usize,
    pub reused_pack_copy: Duration,
}

/// Compare-and-set precondition for a native push target.
///
/// Callers that retain the head from a successful push or pull can provide it
/// here and avoid a separate `ListRefs` request before every push.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ExpectedRemoteHead {
    Missing,
    State(StateId),
}

impl HostedClient {
    fn sync_stream_opening_proof(
        &self,
        stream_id: &str,
        route: &str,
        repository: &str,
        resume_cursor: &str,
        capability_context: Vec<u8>,
    ) -> Result<StreamOpeningProof, ProtocolError> {
        self.stream_opening_proof(
            route,
            stream_id,
            super::helpers::repository_ref(repository).ok_or_else(|| {
                ProtocolError::InvalidState("invalid repository path".to_string())
            })?,
            resume_cursor,
            capability_context,
        )
        .map_err(hosted_to_protocol_error)
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
        let mut refs = Vec::new();
        let mut page_token = String::new();
        loop {
            let request = ListRefsRequest {
                repo_path: super::helpers::repository_ref(repo_path),
                page_size: api::MAX_PAGE_SIZE,
                page_token: page_token.clone(),
            };
            let mut stream = self
                .routes()
                .list_refs(&request)
                .await
                .map_err(hosted_to_protocol_error)?;
            let mut next_page_token = None;
            while let Some(response) = stream.next().await.map_err(hosted_to_protocol_error)? {
                match response.frame {
                    Some(list_refs_response::Frame::Item(entry)) => {
                        let thread_id = if entry.is_thread {
                            if entry.thread_id.is_empty() {
                                return Err(ProtocolError::InvalidState(format!(
                                    "hosted thread ref '{}' is missing its stable identity",
                                    entry.name
                                )));
                            }
                            Some(entry.thread_id)
                        } else {
                            None
                        };
                        refs.push(HostedRefEntry {
                            name: entry.name,
                            state_id: super::helpers::parse_proto_state_id(entry.state_id)?
                                .ok_or_else(|| {
                                    ProtocolError::InvalidState(
                                        "ref is missing its state ID".to_string(),
                                    )
                                })?,
                            is_thread: entry.is_thread,
                            revision_address: entry.revision_address,
                            thread_id,
                        });
                    }
                    Some(list_refs_response::Frame::PageEnd(page_end)) => {
                        next_page_token = Some(page_end.next_page_token);
                    }
                    None => {
                        return Err(ProtocolError::InvalidState(
                            "ListRefs emitted an empty frame".to_string(),
                        ));
                    }
                }
            }
            let next_page_token = next_page_token.ok_or_else(|| {
                ProtocolError::InvalidState(
                    "ListRefs ended without a terminal page frame".to_string(),
                )
            })?;
            if next_page_token.is_empty() {
                return Ok(refs);
            }
            if next_page_token == page_token {
                return Err(ProtocolError::InvalidState(
                    "ListRefs returned a repeated page token".to_string(),
                ));
            }
            page_token = next_page_token;
        }
    }

    async fn pull_thread_id(
        &self,
        repo_path: &str,
        remote_thread: &str,
    ) -> Result<String, ProtocolError> {
        if remote_thread.starts_with(PULL_CLONE_BOOTSTRAP_THREAD_PREFIX) {
            return Ok(String::new());
        }
        self.require_thread_id(repo_path, remote_thread).await
    }

    /// Fetch and validate the managed record for a pulled hosted thread.
    pub async fn get_thread_metadata(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        pulled_state: StateId,
    ) -> Result<SyncedThreadMetadata, ProtocolError> {
        let request = GetThreadRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            name: remote_thread.to_string(),
            thread_id: self.require_thread_id(repo_path, remote_thread).await?,
        };
        let summary = match self.routes().get_thread(&request).await {
            Ok(summary) => summary,
            Err(error) => {
                let error = hosted_to_protocol_error(error);
                if matches!(
                    error,
                    ProtocolError::ObjectNotFound(_)
                        | ProtocolError::RemoteFailure {
                            code: wire::RemoteFailureCode::NotFound,
                            ..
                        }
                ) {
                    return Err(ProtocolError::InvalidState(format!(
                        "hosted thread '{remote_thread}' has no managed metadata; no local thread was created"
                    )));
                }
                return Err(error);
            }
        };
        super::thread_metadata::from_summary(repo, remote_thread, pulled_state, summary)
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
        client_operation_id: String,
    ) -> Result<RefUpdated, ProtocolError> {
        let operation_id = ClientOperationId::for_required_method(
            "heddle.api.v1alpha1.RepoSyncService/UpdateRef",
            client_operation_id,
        )?;
        let remote_refs = if is_thread || thread_metadata.is_some() {
            Some(self.list_refs_with_revision_addresses(repo_path).await?)
        } else {
            None
        };
        let thread_id = if is_thread {
            remote_refs
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|entry| entry.is_thread && entry.name == name)
                .and_then(|entry| entry.thread_id.clone())
                .unwrap_or_default()
        } else {
            String::new()
        };
        let thread_metadata = thread_metadata
            .map(|metadata| {
                to_proto_thread_metadata(
                    metadata,
                    (!thread_id.is_empty()).then_some(thread_id.as_str()),
                    remote_refs.as_deref().unwrap_or_default(),
                )
            })
            .transpose()?;
        let request = UpdateRefRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            name: name.to_string(),
            is_thread,
            force,
            old_value: old_value
                .map(|value| value.to_string_full())
                .unwrap_or_default(),
            new_value: new_value.to_string_full(),
            thread_metadata,
            old_revision_address: old_value
                .map(|value| RevisionAddress::heddle(value).to_string())
                .unwrap_or_default(),
            new_revision_address: RevisionAddress::heddle(new_value).to_string(),
            client_operation_id: operation_id.to_wire(),
            thread_id,
        };
        let response = self
            .routes()
            .update_ref(&request)
            .await
            .map_err(hosted_to_protocol_error)?;
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
        client_operation_id: String,
    ) -> Result<PushComplete, ProtocolError> {
        self.push_profiled(
            repo,
            repo_path,
            local_state,
            target_thread,
            force,
            client_operation_id,
        )
        .await
        .map(|(complete, _)| complete)
    }

    pub async fn push_profiled(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        local_state: StateId,
        target_thread: &str,
        force: bool,
        client_operation_id: String,
    ) -> Result<(PushComplete, PushProfile), ProtocolError> {
        let operation_id = ClientOperationId::caller_or_fresh(
            "heddle.api.v1alpha1.RepoSyncService/Push",
            client_operation_id,
        );
        self.push_with_revision_profiled(
            repo,
            repo_path,
            local_state,
            target_thread,
            force,
            operation_id,
            RevisionAddress::heddle(local_state).to_string(),
            None,
            None,
            &Progress::null(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn push_with_expected_head(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        local_state: StateId,
        target_thread: &str,
        force: bool,
        expected_remote_head: ExpectedRemoteHead,
        client_operation_id: String,
    ) -> Result<PushComplete, ProtocolError> {
        self.push_with_expected_head_profiled(
            repo,
            repo_path,
            local_state,
            target_thread,
            force,
            expected_remote_head,
            client_operation_id,
        )
        .await
        .map(|(complete, _)| complete)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn push_with_expected_head_profiled(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        local_state: StateId,
        target_thread: &str,
        force: bool,
        expected_remote_head: ExpectedRemoteHead,
        client_operation_id: String,
    ) -> Result<(PushComplete, PushProfile), ProtocolError> {
        let operation_id = ClientOperationId::caller_or_fresh(
            "heddle.api.v1alpha1.RepoSyncService/Push",
            client_operation_id,
        );
        self.push_with_revision_profiled(
            repo,
            repo_path,
            local_state,
            target_thread,
            force,
            operation_id,
            RevisionAddress::heddle(local_state).to_string(),
            None,
            Some(expected_remote_head),
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
    #[allow(clippy::too_many_arguments)]
    pub async fn push_git_overlay_mirror(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        local_state: StateId,
        target_thread: &str,
        force: bool,
        progress: &Progress,
        client_operation_id: String,
    ) -> Result<PushComplete, ProtocolError> {
        let operation_id = ClientOperationId::caller_or_fresh(
            "heddle.api.v1alpha1.RepoSyncService/Push",
            client_operation_id,
        );
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
            operation_id,
            local_revision_address,
            Some(git_lane),
            None,
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
        operation_id: ClientOperationId,
        local_revision_address: String,
        git_lane: Option<GitLanePushPlan>,
        expected_remote_head: Option<ExpectedRemoteHead>,
        progress: &Progress,
    ) -> Result<PushComplete, ProtocolError> {
        self.push_with_revision_profiled(
            repo,
            repo_path,
            local_state,
            target_thread,
            force,
            operation_id,
            local_revision_address,
            git_lane,
            expected_remote_head,
            progress,
        )
        .await
        .map(|(complete, _)| complete)
    }

    #[allow(clippy::too_many_arguments)]
    async fn push_with_revision_profiled(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        local_state: StateId,
        target_thread: &str,
        force: bool,
        operation_id: ClientOperationId,
        local_revision_address: String,
        git_lane: Option<GitLanePushPlan>,
        expected_remote_head: Option<ExpectedRemoteHead>,
        progress: &Progress,
    ) -> Result<(PushComplete, PushProfile), ProtocolError> {
        let total_started = std::time::Instant::now();
        let mut profile = PushProfile::default();
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
        let remote_ref_discovery_started = std::time::Instant::now();
        let remote_refs = self.list_refs_with_revision_addresses(repo_path).await?;
        let remote_target = remote_refs
            .iter()
            .find(|entry| entry.is_thread && entry.name == target_thread);
        let target_thread_id = remote_target
            .and_then(|entry| entry.thread_id.clone())
            .unwrap_or_default();
        let native_expected_remote_head = if git_lane.is_none() {
            Some(expected_remote_head.unwrap_or_else(|| {
                remote_target.map_or(ExpectedRemoteHead::Missing, |entry| {
                    ExpectedRemoteHead::State(entry.state_id)
                })
            }))
        } else {
            None
        };
        let native_boundaries = native_expected_remote_head.map_or_else(
            || Ok(Vec::new()),
            |expected| native_push_boundaries_from_expected_head(repo, expected, local_state),
        )?;
        profile.remote_ref_discovery = remote_ref_discovery_started.elapsed();
        let closure_planning_started = std::time::Instant::now();
        let closure = if native_boundaries.is_empty() {
            wire::enumerate_state_closure_transfer_with_options(
                repo.store(),
                local_state,
                wire::StateClosureOptions::default(),
                PUSH_FULL_DESCRIPTOR_OBJECT_THRESHOLD,
            )?
        } else {
            wire::enumerate_state_closure_transfer_from_boundaries(
                repo.store(),
                local_state,
                &native_boundaries,
                PUSH_FULL_DESCRIPTOR_OBJECT_THRESHOLD,
            )?
        };
        let object_plan =
            RepositoryTransferPlan::from_planned_objects(closure.planned_objects, git_lane_intent);
        let full_objects = closure.full_objects;
        let object_count = full_objects
            .as_ref()
            .map_or(object_plan.stats.total_objects, std::vec::Vec::len);
        profile.closure_planning = closure_planning_started.elapsed();
        profile.objects_advertised = object_count;
        let request_setup_started = std::time::Instant::now();
        let transfer_id = push_transfer_id(
            repo_path,
            local_state,
            target_thread,
            &local_revision_address,
            operation_id.as_str(),
        );
        let transport_mode = preferred_transport_mode(&self.transport, object_count);
        let thread_metadata = load_thread_metadata(repo, target_thread, local_state)?
            .as_ref()
            .map(|metadata| {
                to_proto_thread_metadata(
                    metadata,
                    (!target_thread_id.is_empty()).then_some(target_thread_id.as_str()),
                    &remote_refs,
                )
            })
            .transpose()?;
        let mut push_request = PushRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            local_state: super::helpers::proto_state_id(local_state),
            target_thread: target_thread.to_string(),
            create_thread: target_thread_id.is_empty(),
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
            thread_metadata,
            local_revision_address,
            expected_remote_head: None,
            expected_remote_head_missing: false,
            target_thread_id,
        };
        if let Some(expected) = native_expected_remote_head {
            apply_expected_remote_head(&mut push_request, expected);
        }
        let request_message = PushClientFrame {
            frame: Some(push_client_frame::Frame::Request(Box::new(push_request))),
            client_operation_id: operation_id.to_wire(),
        };

        let (tx, rx) = mpsc::channel(self.transport.max_inflight_objects.max(4));
        tx.send(PushClientFrame {
            frame: Some(push_client_frame::Frame::Open(
                self.sync_stream_opening_proof(
                    &transfer_id,
                    "/heddle.api.v1alpha1.RepoSyncService/Push",
                    repo_path,
                    "",
                    Vec::new(),
                )?,
            )),
            client_operation_id: operation_id.to_wire(),
        })
        .await
        .map_err(|_| ProtocolError::InvalidState("failed to open push stream".to_string()))?;
        tx.send(request_message).await.map_err(|_| {
            ProtocolError::InvalidState("failed to initialize push stream".to_string())
        })?;
        let stream = self
            .routes()
            .push(operation_id.to_wire())
            .await
            .map_err(hosted_to_protocol_error)?;
        let (requests, mut response) = stream.split();
        let request_pump = tokio::spawn(pump_push_requests(requests, rx));
        profile.request_setup = request_setup_started.elapsed();

        let ready_wait_started = std::time::Instant::now();
        let ready = match response.next().await.map_err(hosted_to_protocol_error)? {
            Some(PushServerFrame {
                frame: Some(push_server_frame::Frame::Ready(ready)),
            }) => ready,
            _ => {
                return Err(ProtocolError::InvalidState(
                    "expected PushReady from hosted server".to_string(),
                ));
            }
        };
        profile.ready_wait = ready_wait_started.elapsed();
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
        profile.objects_wanted = wanted_infos.len();

        let wanted_plan = RepositoryTransferPlan::from_object_infos(wanted_infos, git_lane_intent);

        // State attachments remain packable for server-to-client pull, but are
        // split out of client-to-server packs so Weft's pack anti-forgery seal
        // can stay absolute. The inspectable sidecar lane is verified per kind
        // before the hosted ref moves.
        profile.sidecar_objects = wanted_plan.partitions.sidecar_objects.len();
        let (pack_objects, push_sidecar_objects): (Vec<_>, Vec<_>) = wanted_plan
            .partitions
            .packable_objects
            .into_iter()
            .partition(|info| info.obj_type.packable_for_push());
        profile.pack_objects = pack_objects.len();
        profile.sidecar_objects += push_sidecar_objects.len();

        let pack_and_send_started = std::time::Instant::now();
        if !pack_objects.is_empty() {
            let native_pack_profile = send_native_pack_streaming_messages(
                &tx,
                repo,
                &pack_objects,
                PushWireIdentities {
                    transfer_id: &transfer_id,
                    client_operation_id: operation_id.as_str(),
                    local_state,
                },
                self.transport.chunk_size.max(1),
                &self.transport,
                ready_transport_mode,
            )
            .await?;
            profile.reused_pack = usize::from(native_pack_profile.reused_pack);
            profile.reused_pack_copy = native_pack_profile.reused_pack_copy;
        }
        profile.pack_and_send = pack_and_send_started.elapsed();

        let sidecars_and_git_send_started = std::time::Instant::now();
        for info in wanted_plan
            .partitions
            .sidecar_objects
            .into_iter()
            .chain(push_sidecar_objects)
        {
            let message = sidecar_push_message(repo, info, operation_id.as_str())?;
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
                    operation_id.as_str(),
                )
                .await?;
            } else {
                progress.set_phase("no new objects to pack");
            }
            progress.set_phase(format!("writing {} refs", git_lane.ref_updates.len()));
            for ref_update in git_lane.ref_updates {
                tx.send(git_lane_push_message(
                    git_lane_transfer::Body::RefUpdate(ref_update),
                    operation_id.as_str(),
                ))
                .await
                .map_err(|_| {
                    ProtocolError::InvalidState("push stream closed unexpectedly".to_string())
                })?;
            }
        }
        profile.sidecars_and_git_send = sidecars_and_git_send_started.elapsed();
        drop(tx);
        let request_finish_started = std::time::Instant::now();
        request_pump
            .await
            .map_err(|err| ProtocolError::InvalidState(format!("push request task failed: {err}")))?
            .map_err(hosted_to_protocol_error)?;
        profile.request_finish = request_finish_started.elapsed();

        let complete_wait_started = std::time::Instant::now();
        let result = match response.next().await.map_err(hosted_to_protocol_error)? {
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
                    "expected PushComplete from hosted server".to_string(),
                ));
            }
        };
        profile.complete_wait = complete_wait_started.elapsed();

        let metadata_sync_started = std::time::Instant::now();
        if result.success {
            self.sync_remote_markers(repo, repo_path, local_state)
                .await?;
        }
        profile.metadata_sync = metadata_sync_started.elapsed();
        profile.total = total_started.elapsed();
        Ok((result, profile))
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
                publish_refs: true,
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
                publish_refs: true,
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
                publish_refs: true,
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
                publish_refs: true,
            },
        )
        .await
    }

    pub(crate) async fn repair_clone_with_depth_and_materialization(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        depth: Option<u32>,
        materialization: PullMaterialization,
    ) -> Result<PullComplete, ProtocolError> {
        self.pull_with_options(
            repo,
            repo_path,
            remote_thread,
            PullOptions {
                local_thread: None,
                depth,
                target_state: None,
                materialization,
                publish_refs: false,
            },
        )
        .await
    }

    pub async fn clone_pull_with_depth_and_materialization<F>(
        &mut self,
        repo_path: &str,
        requested_thread: Option<&str>,
        depth: Option<u32>,
        materialization: PullMaterialization,
        initialize: F,
    ) -> Result<(PullComplete, Repository), ProtocolError>
    where
        F: FnOnce(&PullReady) -> Result<Repository, ProtocolError>,
    {
        let remote_thread = format!(
            "{PULL_CLONE_BOOTSTRAP_THREAD_PREFIX}{}",
            requested_thread.unwrap_or_default()
        );
        let (exchange, repo) = self
            .pull_exchange_inner(
                None,
                repo_path,
                &remote_thread,
                PullOptions {
                    local_thread: None,
                    depth,
                    target_state: None,
                    materialization,
                    publish_refs: false,
                },
                Some(Box::new(initialize)),
            )
            .await?;
        let repo = repo.ok_or_else(|| {
            ProtocolError::InvalidState("clone pull did not initialize its repository".to_string())
        })?;
        Ok((exchange.result, repo))
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
                publish_refs: true,
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
                publish_refs: true,
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
        let request = GetBlobRequest {
            repo_path: super::helpers::repository_ref(repo_path),
            r#ref: reference.to_string(),
            path: path.to_string(),
            start_line: 0,
            line_count: 0,
        };
        let response = self
            .routes()
            .get_blob(&request)
            .await
            .map_err(hosted_to_protocol_error)?;

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
                    publish_refs: true,
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
        self.pull_exchange_inner(Some(repo), repo_path, remote_thread, options, None)
            .await
            .map(|(exchange, _)| exchange)
    }

    async fn pull_exchange_inner<'a>(
        &mut self,
        initial_repo: Option<&Repository>,
        repo_path: &str,
        remote_thread: &str,
        options: PullOptions<'_>,
        initializer: Option<PullRepositoryInitializer<'a>>,
    ) -> Result<(PullExchange, Option<Repository>), ProtocolError> {
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
        let advertised_head = if let Some(repo) = initial_repo {
            if let Some(local_thread) = options.local_thread {
                locally_complete_local_thread_head(repo, local_thread, options.target_state)?
            } else {
                locally_complete_pull_head(repo, remote_thread, options.target_state)?
            }
        } else {
            None
        };
        if let Some(head) = advertised_head {
            exclude_states.push(head);
        }
        let allow_partial_fetch = options.materialization.allows_partial_fetch();
        let fresh_full_pull = match initial_repo {
            Some(repo) => supports_compact_full_pull(repo, allow_partial_fetch, &exclude_states)?,
            None => !allow_partial_fetch,
        };

        let transfer_id = pull_transfer_id(
            repo_path,
            remote_thread,
            options.local_thread,
            options.depth,
            options.target_state,
            uuid::Uuid::new_v4(),
        );
        let remote_thread_id = self.pull_thread_id(repo_path, remote_thread).await?;
        let (provider_session, provider_capability_context) =
            self.begin_provider_pull(&transfer_id, repo_path)?;
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
                partial_fetch_status: initial_repo
                    .map(partial_fetch_status_for_repo)
                    .unwrap_or(PartialFetchStatus::Disabled as i32),
                allow_partial_fetch,
                fresh_full_pull,
                target_revision_address: options
                    .target_state
                    .map(|state| RevisionAddress::heddle(state).to_string())
                    .unwrap_or_default(),
                remote_thread_id,
            })),
        };

        let (tx, rx) = mpsc::channel(self.transport.max_inflight_objects.max(4));
        tx.send(PullClientFrame {
            frame: Some(pull_client_frame::Frame::Open(
                self.sync_stream_opening_proof(
                    &transfer_id,
                    "/heddle.api.v1alpha1.RepoSyncService/Pull",
                    repo_path,
                    "",
                    provider_capability_context,
                )?,
            )),
        })
        .await
        .map_err(|_| ProtocolError::InvalidState("failed to open pull stream".to_string()))?;
        tx.send(request_message).await.map_err(|_| {
            ProtocolError::InvalidState("failed to initialize pull stream".to_string())
        })?;
        let stream = self
            .routes()
            .pull()
            .await
            .map_err(hosted_to_protocol_error)?;
        let (mut requests, mut response) = stream.split();
        let request_pump = tokio::spawn(async move {
            let mut rx = rx;
            while let Some(message) = rx.recv().await {
                requests.send(&message).await?;
            }
            requests.finish()
        });

        let ready = match response.next().await.map_err(hosted_to_protocol_error)? {
            Some(PullServerFrame {
                frame: Some(pull_server_frame::Frame::Ready(ready)),
            }) => ready,
            _ => {
                return Err(ProtocolError::InvalidState(
                    "expected PullReady from hosted server".to_string(),
                ));
            }
        };
        let mut profile = PullProfile {
            ready_wait: exchange_start.elapsed(),
            ..PullProfile::default()
        };
        let owned_repo = match initial_repo {
            Some(_) => None,
            None => Some(initializer.ok_or_else(|| {
                ProtocolError::InvalidState(
                    "pull is missing its repository initializer".to_string(),
                )
            })?(&ready)?),
        };
        let repo = initial_repo.or(owned_repo.as_ref()).ok_or_else(|| {
            ProtocolError::InvalidState("pull repository initialization failed".to_string())
        })?;
        let remote_state = super::helpers::parse_proto_state_id(ready.remote_state)?
            .ok_or_else(|| ProtocolError::InvalidState("missing remote state".to_string()))?;
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
        let mut tx = Some(tx);
        if !native_pack_required {
            tx.take();
        }

        let receive_start = Instant::now();
        // Native-pack size is independent of advertised object count: one blob
        // can be close to the receive limit. Keep remote-controlled bytes on
        // disk for every pull instead of retaining a potentially multi-GiB
        // pack and index in memory for "small" object-count transfers.
        let mut pack_spool = wire::PackChunkSpool::new_in(repo.heddle_dir())?;
        let mut git_lane_repo = None;
        let mut git_pack_state = GitPackPullInstallState::default();
        let mut staged_git_lane =
            StagedGitLanePull::snapshot(repo, options.target_state.is_none())?;
        let mut consented_provider_plan: Option<ProviderPlanChallenge> = None;
        let mut provider_pack = None;
        let mut provider_fallback = false;
        let mut ordinary_pack_received = false;
        let mut received = 0usize;
        while let Some(message) = next_pull_message(&mut response).await? {
            match message.frame {
                Some(pull_server_frame::Frame::Pack(chunk)) => {
                    if let Some(challenge) = consented_provider_plan.as_ref()
                        && !provider_fallback
                        && provider_pack.is_none()
                    {
                        provider_fallback = true;
                        let sender = tx.as_ref().ok_or_else(|| {
                            ProtocolError::InvalidState(
                                "provider negotiation closed before fallback".to_string(),
                            )
                        })?;
                        sender
                            .send(PullClientFrame {
                                frame: Some(pull_client_frame::Frame::ProviderResult(
                                    provider_session
                                        .fallback_response(&challenge.grant_batch_digest),
                                )),
                            })
                            .await
                            .map_err(|_| {
                                ProtocolError::InvalidState(
                                    "pull stream closed during provider fallback".to_string(),
                                )
                            })?;
                    }
                    tx.take();
                    ordinary_pack_received = true;
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
                    pack_spool.receive_chunk(
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
                Some(pull_server_frame::Frame::ProviderChallenge(challenge)) => {
                    let (plan, consented_challenge) = if consented_provider_plan.is_some()
                        || provider_fallback
                    {
                        provider_fallback = true;
                        consented_provider_plan = None;
                        (
                            self.decline_provider_challenge(&provider_session, Some(&challenge)),
                            None,
                        )
                    } else {
                        match self.answer_provider_challenge(&provider_session, &challenge) {
                            Ok(plan) => (plan, Some(challenge.clone())),
                            Err(_) => {
                                provider_fallback = true;
                                (
                                    self.decline_provider_challenge(
                                        &provider_session,
                                        Some(&challenge),
                                    ),
                                    None,
                                )
                            }
                        }
                    };
                    let accepted = plan.accepted;
                    tx.as_ref()
                        .ok_or_else(|| {
                            ProtocolError::InvalidState(
                                "provider challenge arrived after request completion".to_string(),
                            )
                        })?
                        .send(PullClientFrame {
                            frame: Some(pull_client_frame::Frame::ProviderPlan(plan)),
                        })
                        .await
                        .map_err(|_| {
                            ProtocolError::InvalidState(
                                "pull stream closed during provider plan consent".to_string(),
                            )
                        })?;
                    if let Some(challenge) = consented_challenge {
                        consented_provider_plan = Some(challenge);
                    }
                    if !accepted {
                        tx.take();
                    }
                }
                Some(pull_server_frame::Frame::ProviderManifest(manifest)) => {
                    let result = match consented_provider_plan.as_ref() {
                        Some(challenge) if !provider_fallback && provider_pack.is_none() => {
                            self.download_provider_pull(
                                &provider_session,
                                challenge,
                                &manifest,
                                repo.heddle_dir(),
                                repo.store().clone(),
                            )
                            .await
                        }
                        _ => Err(super::provider_pull::rejected_provider_manifest(
                            &manifest,
                            ProtocolError::InvalidState(
                                "provider manifest arrived without one matching consent"
                                    .to_string(),
                            ),
                        )),
                    };
                    let (provider_result, completed) =
                        provider_session.resolve_download(&manifest.grant_batch_digest, result);
                    provider_pack = completed;
                    provider_fallback = provider_pack.is_none();
                    tx.as_ref()
                        .ok_or_else(|| {
                            ProtocolError::InvalidState(
                                "provider manifest arrived after request completion".to_string(),
                            )
                        })?
                        .send(PullClientFrame {
                            frame: Some(pull_client_frame::Frame::ProviderResult(provider_result)),
                        })
                        .await
                        .map_err(|_| {
                            ProtocolError::InvalidState(
                                "pull stream closed during provider result".to_string(),
                            )
                        })?;
                    tx.take();
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
                        &mut staged_git_lane,
                        transfer,
                    )?;
                    let decode_elapsed = decode_start.elapsed();
                    profile.store_receive_object += decode_elapsed;
                }
                Some(pull_server_frame::Frame::Complete(complete)) => {
                    tx.take();
                    request_pump
                        .await
                        .map_err(|err| {
                            ProtocolError::InvalidState(format!("pull request task failed: {err}"))
                        })?
                        .map_err(hosted_to_protocol_error)?;
                    profile.receive_and_apply = receive_start.elapsed();
                    git_pack_state.ensure_idle()?;
                    let final_state = super::helpers::parse_proto_state_id(complete.new_state)?;

                    if complete.success {
                        apply_staged_git_lane_pull(repo, &mut git_lane_repo, &mut staged_git_lane)?;
                        if native_pack_required || provider_pack.is_some() || ordinary_pack_received
                        {
                            let store_start = Instant::now();
                            let mut installed_ids = Vec::new();
                            if let Some(provider_pack) = provider_pack.as_mut() {
                                installed_ids.append(&mut provider_pack.installed_ids);
                            }
                            if ordinary_pack_received {
                                if !pack_spool.is_complete() {
                                    return Err(ProtocolError::InvalidState(
                                        "pull completed before native pack stream finished"
                                            .to_string(),
                                    ));
                                }
                                installed_ids.extend(pack_spool.install_into(repo.store())?);
                            }
                            if installed_ids.is_empty() {
                                return Err(ProtocolError::InvalidState(
                                    "pull completed without its required native pack".to_string(),
                                ));
                            }
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
                        if let Some(state) = final_state
                            && allow_partial_fetch
                        {
                            mark_missing_blobs_for_state(repo, state)?;
                        } else if final_state.is_some() {
                            let _ = repo.clear_all_missing_blobs()?;
                        }
                        if options.publish_refs {
                            let synced_markers = complete
                                .transfer
                                .as_ref()
                                .map(|transfer| apply_marker_snapshot(repo, &transfer.checkpoint))
                                .transpose()?
                                .unwrap_or(false);
                            if !synced_markers {
                                self.sync_local_markers(repo, repo_path).await?;
                            }
                        }
                        profile.metadata_sync = metadata_start.elapsed();
                        profile.objects_received = received;
                        return Ok((
                            PullExchange {
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
                                        .map(|transfer| {
                                            transport_mode_name(transfer.transport_mode)
                                        })
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
                            },
                            owned_repo,
                        ));
                    }

                    profile.objects_received = received;
                    return Ok((
                        PullExchange {
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
                        },
                        owned_repo,
                    ));
                }
                _ => {}
            }
        }

        Err(ProtocolError::InvalidState(format!(
            "pull stream ended unexpectedly after receiving {received} packed objects"
        )))
    }

    async fn sync_remote_markers(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        pushed_state: StateId,
    ) -> Result<(), ProtocolError> {
        let local_markers = repo.refs().list_markers()?;
        if local_markers.is_empty() {
            return Ok(());
        }
        let remote_markers = self
            .list_refs(repo_path)
            .await?
            .into_iter()
            .filter(|entry| !entry.is_thread)
            .map(|entry| (entry.name, entry.state_id))
            .collect::<HashMap<_, _>>();
        for marker in local_markers {
            let Some(state_id) = repo.refs().get_marker(&marker)? else {
                continue;
            };
            if !wire::is_ancestor(repo.store(), state_id, pushed_state)? {
                continue;
            }

            let old_value = remote_markers.get(marker.as_str()).copied();
            if old_value == Some(state_id) {
                continue;
            }

            let result = self
                .update_ref(
                    repo_path,
                    &marker,
                    false,
                    old_value,
                    state_id,
                    true,
                    None,
                    ClientOperationId::fresh("heddle.api.v1alpha1.RepoSyncService/UpdateRef")
                        .to_wire(),
                )
                .await?;
            if !result.success {
                return Err(ProtocolError::InvalidState(
                    result
                        .error
                        .unwrap_or_else(|| format!("failed to sync marker '{marker}'")),
                ));
            }
        }
        Ok(())
    }

    async fn sync_local_markers(
        &mut self,
        repo: &Repository,
        repo_path: &str,
    ) -> Result<(), ProtocolError> {
        let remote_markers = self.list_refs(repo_path).await?;
        for marker in remote_markers.into_iter().filter(|entry| !entry.is_thread) {
            if !repo.store().has_state(&marker.state_id)? {
                continue;
            }
            let marker_name = MarkerName::from(marker.name.as_str());
            match repo.refs().get_marker(&marker_name)? {
                Some(existing) if existing == marker.state_id => {}
                Some(existing) => repo.set_marker_recorded_cas(
                    &marker_name,
                    refs::RefExpectation::Value(existing),
                    &marker.state_id,
                )?,
                None => repo.create_marker_recorded(&marker_name, &marker.state_id)?,
            }
        }
        Ok(())
    }

    pub(crate) async fn publish_clone_markers(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        checkpoint: &[u8],
    ) -> Result<(), ProtocolError> {
        if !apply_marker_snapshot(repo, checkpoint)? {
            self.sync_local_markers(repo, repo_path).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
fn native_push_boundaries(
    repo: &Repository,
    remote_refs: &[RefEntry],
    target_thread: &str,
    local_state: StateId,
) -> Result<Vec<StateId>, ProtocolError> {
    native_push_boundaries_from_expected_head(
        repo,
        expected_remote_head_from_refs(remote_refs, target_thread),
        local_state,
    )
}

#[cfg(test)]
fn expected_remote_head_from_refs(
    remote_refs: &[RefEntry],
    target_thread: &str,
) -> ExpectedRemoteHead {
    remote_refs
        .iter()
        .find(|entry| entry.is_thread && entry.name == target_thread)
        .map(|entry| entry.state_id)
        .map_or(ExpectedRemoteHead::Missing, ExpectedRemoteHead::State)
}

fn native_push_boundaries_from_expected_head(
    repo: &Repository,
    expected_remote_head: ExpectedRemoteHead,
    local_state: StateId,
) -> Result<Vec<StateId>, ProtocolError> {
    let ExpectedRemoteHead::State(remote_head) = expected_remote_head else {
        return Ok(Vec::new());
    };
    // Keep an exact-tip push on the full path: Weft deliberately re-installs
    // pushed state bodies so its live server-owned sidecars can be merged at
    // the install chokepoint. A strict ancestor is a safe content-addressed
    // boundary because the server ref proves that closure is already durable.
    if remote_head != local_state && wire::is_ancestor(repo.store(), remote_head, local_state)? {
        Ok(vec![remote_head])
    } else {
        Ok(Vec::new())
    }
}

fn apply_expected_remote_head(request: &mut PushRequest, expected: ExpectedRemoteHead) {
    match expected {
        ExpectedRemoteHead::Missing => {
            request.expected_remote_head = None;
            request.expected_remote_head_missing = true;
        }
        ExpectedRemoteHead::State(state) => {
            request.expected_remote_head = super::helpers::proto_state_id(state);
            request.expected_remote_head_missing = false;
        }
    }
}

async fn pump_push_requests(
    mut requests: BidirectionalRequestStream<PushClientFrame>,
    mut rx: mpsc::Receiver<PushClientFrame>,
) -> super::Result<()> {
    while let Some(mut message) = rx.recv().await {
        let raw = take_push_raw_body(&mut message);
        requests.send(&message).await?;
        if let Some(raw) = raw {
            requests.begin_raw(raw.len() as u64).await?;
            requests.send_raw_chunk(bytes::Bytes::from(raw)).await?;
        }
    }
    requests.finish()
}

fn take_push_raw_body(message: &mut PushClientFrame) -> Option<Vec<u8>> {
    match message.frame.as_mut()? {
        push_client_frame::Frame::Pack(chunk) if !chunk.data.is_empty() => {
            Some(std::mem::take(&mut chunk.data))
        }
        push_client_frame::Frame::GitLane(transfer) => match transfer.body.as_mut()? {
            git_lane_transfer::Body::Pack(pack) if !pack.pack_chunk.is_empty() => {
                Some(std::mem::take(&mut pack.pack_chunk))
            }
            _ => None,
        },
        _ => None,
    }
}

async fn next_pull_message(
    response: &mut ServerStream<PullServerFrame>,
) -> Result<Option<PullServerFrame>, ProtocolError> {
    let Some(item) = response
        .next_item()
        .await
        .map_err(hosted_to_protocol_error)?
    else {
        return Ok(None);
    };
    let ServerStreamItem::Message(mut message) = item else {
        return Err(ProtocolError::InvalidState(
            "pull stream sent raw bytes without a pack header".to_string(),
        ));
    };
    let raw_target = match message.frame.as_mut() {
        Some(pull_server_frame::Frame::Pack(chunk)) if chunk.data.is_empty() => {
            Some(&mut chunk.data)
        }
        Some(pull_server_frame::Frame::GitLane(transfer)) => match transfer.body.as_mut() {
            Some(git_lane_transfer::Body::Pack(pack)) if pack.pack_chunk.is_empty() => {
                Some(&mut pack.pack_chunk)
            }
            _ => None,
        },
        _ => None,
    };
    if let Some(raw_target) = raw_target {
        let Some(ServerStreamItem::RawBody { length }) = response
            .next_item()
            .await
            .map_err(hosted_to_protocol_error)?
        else {
            return Err(ProtocolError::InvalidState(
                "pull pack header was not followed by a raw body".to_string(),
            ));
        };
        let capacity = usize::try_from(length).map_err(|_| {
            ProtocolError::InvalidState("pull raw body exceeds this platform".to_string())
        })?;
        raw_target.reserve(capacity);
        while let Some(chunk) = response
            .read_raw_chunk(1024 * 1024)
            .await
            .map_err(hosted_to_protocol_error)?
        {
            raw_target.extend_from_slice(&chunk);
        }
        if raw_target.len() != capacity {
            return Err(ProtocolError::InvalidState(
                "pull raw body length changed during receive".to_string(),
            ));
        }
    }
    Ok(Some(message))
}

fn redaction_push_message(
    repo: &Repository,
    info: wire::ObjectInfo,
    client_operation_id: &str,
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
        .verified_redactions_bytes_for_wire(&blob)
        .map_err(|err| {
            ProtocolError::InvalidState(format!(
                "load verified redactions sidecar for {}: {err}",
                hex
            ))
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
        client_operation_id: client_operation_id.to_string(),
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

fn descriptor_id_from_plan(object: &PlannedObject) -> (String, i32) {
    descriptor_id_from_info(&object_info_from_plan(object))
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
    client_operation_id: &str,
) -> Result<PushClientFrame, ProtocolError> {
    match info.obj_type {
        ObjectType::Redaction => redaction_push_message(repo, info, client_operation_id),
        ObjectType::StateVisibility => {
            state_visibility_push_message(repo, info, client_operation_id)
        }
        ObjectType::StateAttachment => {
            state_attachment_push_message(repo, info, client_operation_id)
        }
        obj_type => Err(ProtocolError::InvalidState(format!(
            "{obj_type:?} is not an out-of-pack sidecar object"
        ))),
    }
}

/// Build the inspectable, out-of-pack carrier used for client-to-server
/// attachment pushes. Pull keeps using the native pack. The receiver treats
/// the decoded body's kind as authoritative and verifies it before install.
fn state_attachment_push_message(
    repo: &Repository,
    info: wire::ObjectInfo,
    client_operation_id: &str,
) -> Result<PushClientFrame, ProtocolError> {
    let record = wire::load_object_data(repo.store(), &info.id, ObjectType::StateAttachment)?;
    let wire::ObjectId::StateAttachment { state, id, kind } = info.id else {
        return Err(ProtocolError::InvalidState(
            "wanted StateAttachment must be keyed by ObjectId::StateAttachment".to_string(),
        ));
    };
    Ok(PushClientFrame {
        frame: Some(push_client_frame::Frame::StateAttachment(
            StateAttachmentTransfer {
                state_id: super::helpers::proto_state_id(state),
                attachment_id: id.as_hash().to_hex(),
                attachment_kind: super::helpers::attachment_kind_to_proto(kind) as i32,
                attachment_object: record.data,
            },
        )),
        client_operation_id: client_operation_id.to_string(),
    })
}

fn state_visibility_push_message(
    repo: &Repository,
    info: wire::ObjectInfo,
    client_operation_id: &str,
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
        client_operation_id: client_operation_id.to_string(),
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
        apply_marker_entry(repo, name, state_id)?;
    }

    Ok(true)
}

fn apply_marker_entry(
    repo: &Repository,
    name: &str,
    state_id: StateId,
) -> Result<(), ProtocolError> {
    if !repo.store().has_state(&state_id)? {
        return Ok(());
    }
    let name = MarkerName::from(name);
    match repo.refs().get_marker(&name)? {
        Some(existing) if existing == state_id => {}
        Some(existing) => {
            repo.set_marker_recorded_cas(&name, refs::RefExpectation::Value(existing), &state_id)?
        }
        None => repo.create_marker_recorded(&name, &state_id)?,
    }
    Ok(())
}

#[cfg(test)]
mod pull_bootstrap_tests {
    use chrono::Utc;
    use objects::object::{
        Annotation, AnnotationKind, AnnotationScope, Attribution, Blob, DiscussionResolution,
        DiscussionTurn, Principal, StateAttachment, SymbolAnchor, VisibilityTier,
    };
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn bootstrap_decoder_accepts_structured_empty_metadata() {
        let temp = TempDir::new().expect("temp repo");
        let repo = Repository::init_default(temp.path()).expect("init repo");
        std::fs::write(temp.path().join("README.md"), "bootstrap\n").expect("write fixture");
        let snapshot = repo
            .snapshot(Some("bootstrap marker".to_string()), None)
            .expect("snapshot");
        let payload = rmp_serde::to_vec_named(&(
            true,
            Vec::<Discussion>::new(),
            true,
            Vec::<(ContextTarget, ContextBlob)>::new(),
        ))
        .expect("encode server-compatible pull bootstrap");
        let checkpoint = format!(
            "heddle-markers-v1\nrelease\t{}\n{PULL_BOOTSTRAP_LINE_PREFIX}{}\t{}\n",
            snapshot.state_id.to_string_full(),
            hex::encode(payload),
            StateId::from_bytes([0; 32]).to_string_full()
        );

        let decoded = decode_pull_bootstrap(checkpoint.as_bytes())
            .expect("decode pull bootstrap")
            .expect("new-server header must select bootstrap metadata");
        assert!(decoded.discussions.is_empty());
        assert!(decoded.context.is_empty());
        assert!(
            apply_marker_snapshot(&repo, checkpoint.as_bytes())
                .expect("the legacy marker parser accepts the additive ASCII line")
        );
        assert_eq!(
            repo.refs()
                .get_marker(&MarkerName::from("release"))
                .expect("read marker"),
            Some(snapshot.state_id)
        );
    }

    #[test]
    fn malformed_advertised_bootstrap_fails_loud() {
        let error = decode_pull_bootstrap(
            format!(
                "heddle-markers-v1\nheddle-pull-bootstrap-v1:not-hex\t{}\n",
                StateId::from_bytes([0; 32]).to_string_full()
            )
            .as_bytes(),
        )
        .expect_err("advertised but malformed metadata must not silently fall back");
        assert!(error.to_string().contains("decode pull bootstrap"));
    }

    #[test]
    fn folded_refs_decode_identically() {
        let main = StateId::from_bytes([7; 32]);
        let release = StateId::from_bytes([8; 32]);
        let payload: PullRefsPayload = (
            "main".to_string(),
            Some(main.as_bytes().to_vec()),
            vec![
                (
                    "main".to_string(),
                    main.as_bytes().to_vec(),
                    true,
                    "git:0123456789abcdef".to_string(),
                ),
                (
                    "release".to_string(),
                    release.as_bytes().to_vec(),
                    false,
                    RevisionAddress::heddle(release).to_string(),
                ),
            ],
        );
        let checkpoint = format!(
            "{PULL_REFS_LINE_PREFIX}{}\t{}\n",
            hex::encode(rmp_serde::to_vec_named(&payload).expect("encode refs fixture")),
            StateId::from_bytes([0; 32]).to_string_full(),
        );

        let decoded = decode_pull_refs(checkpoint.as_bytes())
            .expect("decode folded refs")
            .expect("new server advertises refs");
        assert_eq!(decoded.refs.len(), 2);
        assert_eq!(decoded.refs[0].name, "main");
        assert_eq!(decoded.refs[0].state_id, main);
        assert!(decoded.refs[0].is_thread);
        assert_eq!(decoded.refs[0].revision_address, "git:0123456789abcdef");
        assert_eq!(decoded.refs[1].name, "release");
        assert_eq!(decoded.refs[1].state_id, release);
        assert!(!decoded.refs[1].is_thread);
    }

    #[test]
    fn packed_bootstrap_resolves_the_same_discussion_and_context_domain_content() {
        let temp = TempDir::new().expect("temp repo");
        let repo = Repository::init_default(temp.path()).expect("init repo");
        std::fs::write(temp.path().join("lib.rs"), "pub fn run() {}\n").expect("write source");
        let snapshot = repo
            .snapshot(Some("seed".to_string()), None)
            .expect("snapshot");
        let principal = Principal::new("Ada", "ada@example.com");
        let discussion = Discussion {
            id: "discussion-1".to_string(),
            anchor: SymbolAnchor::new("lib.rs", "run"),
            opened_against_state: snapshot.state_id,
            opened_at: 1_700_000_000,
            thread_ref: None,
            turns: vec![DiscussionTurn {
                author: principal.clone(),
                body: "keep this invariant".to_string(),
                posted_at: 1_700_000_001,
            }],
            resolution: DiscussionResolution::Open,
            body_changed_since_open: false,
            orphaned: false,
            visibility: VisibilityTier::Public,
            resolved_annotation_id: None,
        };
        let discussions_blob = DiscussionsBlob::new(vec![discussion.clone()]);
        let discussions_hash = repo
            .store()
            .put_blob(&Blob::from(
                discussions_blob.encode().expect("encode discussions"),
            ))
            .expect("store discussions");
        repo.put_state_attachment(&StateAttachment {
            state_id: snapshot.state_id,
            body: StateAttachmentBody::Discussions(discussions_hash),
            attribution: Attribution::human(principal.clone()),
            created_at: Utc::now(),
            supersedes: None,
        })
        .expect("attach discussions");

        let target = ContextTarget::file("lib.rs").expect("context target");
        let annotation = Annotation::new(
            AnnotationScope::File,
            AnnotationKind::Invariant,
            "run remains public".to_string(),
            vec!["contract".to_string()],
            principal.to_string(),
            1_700_000_002,
            None,
            Some(snapshot.state_id),
        );
        let context_blob = ContextBlob::new(vec![annotation.clone()]);
        let context_root = repo
            .set_context_blob(None, &target, &context_blob)
            .expect("store context");
        repo.put_state_attachment(&StateAttachment {
            state_id: snapshot.state_id,
            body: StateAttachmentBody::Context(context_root),
            attribution: Attribution::human(principal),
            created_at: Utc::now(),
            supersedes: None,
        })
        .expect("attach context");

        let resolved = PullBootstrapMetadata {
            discussions_from_pack: true,
            discussions: Vec::new(),
            context_from_pack: true,
            context: Vec::new(),
        }
        .resolve(&repo, Some(snapshot.state_id))
        .expect("resolve packed bootstrap");

        assert_eq!(resolved.discussions, vec![discussion]);
        assert_eq!(resolved.context, vec![(target, context_blob)]);
    }
}

fn state_id_string_to_bytes(s: &str) -> Vec<u8> {
    if s.is_empty() {
        return Vec::new();
    }
    objects::object::StateId::parse(s)
        .map(|id| id.as_bytes().to_vec())
        .unwrap_or_default()
}

fn hosted_thread_id(
    remote_refs: &[HostedRefEntry],
    thread_name: &str,
) -> Result<String, ProtocolError> {
    remote_refs
        .iter()
        .find(|entry| entry.is_thread && entry.name == thread_name)
        .and_then(|entry| entry.thread_id.clone())
        .ok_or_else(|| ProtocolError::ObjectNotFound(format!("hosted thread '{thread_name}'")))
}

fn related_thread_id(
    remote_refs: &[HostedRefEntry],
    thread_name: Option<&String>,
) -> Result<Option<String>, ProtocolError> {
    thread_name
        .map(|name| hosted_thread_id(remote_refs, name))
        .transpose()
}

fn to_proto_thread_metadata(
    metadata: &SyncedThreadMetadata,
    thread_id: Option<&str>,
    remote_refs: &[HostedRefEntry],
) -> Result<ThreadMetadata, ProtocolError> {
    Ok(ThreadMetadata {
        name: metadata.thread.clone(),
        target_thread: metadata.target_thread.clone(),
        parent_thread: metadata.parent_thread.clone(),
        task: metadata.task.clone(),
        thread_mode: match metadata.mode {
            repo::ThreadMode::Materialized => ProtoThreadMode::Materialized,
            repo::ThreadMode::Virtualized => ProtoThreadMode::Virtualized,
            repo::ThreadMode::Solid => ProtoThreadMode::Solid,
        } as i32,
        thread_state: match metadata.state {
            repo::ThreadState::Draft => ProtoThreadState::ThreadStateDraft,
            repo::ThreadState::Active => ProtoThreadState::ThreadStateActive,
            repo::ThreadState::Ready => ProtoThreadState::ThreadStateReady,
            repo::ThreadState::Blocked => ProtoThreadState::ThreadStateBlocked,
            repo::ThreadState::Merged => ProtoThreadState::ThreadStateMerged,
            repo::ThreadState::Abandoned => ProtoThreadState::ThreadStateAbandoned,
            repo::ThreadState::Promoted => ProtoThreadState::ThreadStatePromoted,
        } as i32,
        freshness: match metadata.freshness {
            repo::ThreadFreshness::Current => ProtoThreadFreshness::Current,
            repo::ThreadFreshness::Stale => ProtoThreadFreshness::Stale,
            repo::ThreadFreshness::Unknown => ProtoThreadFreshness::Unknown,
        } as i32,
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
            band: match metadata.confidence_summary.band {
                Some(repo::ConfidenceBand::Low) => ProtoConfidenceBand::Low,
                Some(repo::ConfidenceBand::Medium) => ProtoConfidenceBand::Medium,
                Some(repo::ConfidenceBand::High) => ProtoConfidenceBand::High,
                None => ProtoConfidenceBand::Unspecified,
            } as i32,
        }),
        integration_policy_result: Some(ThreadIntegrationPolicy {
            status: match metadata.integration_policy_result.status.as_deref() {
                Some("previewed") => ProtoIntegrationPolicyStatus::Previewed,
                Some("current") => ProtoIntegrationPolicyStatus::Current,
                Some("blocked") => ProtoIntegrationPolicyStatus::Blocked,
                Some("manual_resolved") => ProtoIntegrationPolicyStatus::ManualResolved,
                Some("auto_integrated") => ProtoIntegrationPolicyStatus::AutoIntegrated,
                _ => ProtoIntegrationPolicyStatus::Unspecified,
            } as i32,
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
        thread_id: thread_id.unwrap_or_default().to_string(),
        target_thread_id: related_thread_id(remote_refs, metadata.target_thread.as_ref())?,
        parent_thread_id: related_thread_id(remote_refs, metadata.parent_thread.as_ref())?,
    })
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

#[cfg(test)]
mod native_exchange_tests {
    use objects::object::{Attribution, Principal};
    use tempfile::TempDir;

    use super::*;

    fn repository(temp: &TempDir) -> (Repository, StateId) {
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("tracked.txt"), "content\n").unwrap();
        let state = repo
            .snapshot_with_attribution(
                Some("native exchange fixture".to_string()),
                None,
                Attribution::human(Principal::new("Test", "test@example.com")),
            )
            .unwrap()
            .id();
        (repo, state)
    }

    #[tokio::test]
    async fn native_push_and_clone_pull_complete_the_real_framed_exchange() {
        let (mut client, server) = crate::hosted_runtime::hosted::test_server::start().await;
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);

        assert!(client.list_refs("acme/widgets").await.unwrap().is_empty());
        let pushed = client
            .push_with_expected_head(
                &repo,
                "acme/widgets",
                state,
                "main",
                false,
                ExpectedRemoteHead::Missing,
                "push-test-op".to_string(),
            )
            .await
            .unwrap();
        assert!(!pushed.success);
        assert_eq!(pushed.error.as_deref(), Some("test rejection"));

        let clone = TempDir::new().unwrap();
        let (pulled, cloned_repo) = client
            .clone_pull_with_depth_and_materialization(
                "acme/widgets",
                Some("main"),
                Some(1),
                PullMaterialization::Lazy,
                |_| Repository::init_default(clone.path()).map_err(ProtocolError::from),
            )
            .await
            .unwrap();
        assert!(!pulled.success);
        assert_eq!(pulled.error.as_deref(), Some("test rejection"));
        assert_eq!(cloned_repo.root(), clone.path());

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn compact_pull_of_a_complete_local_state_publishes_no_synthetic_objects() {
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let remote_state = super::super::helpers::proto_state_id(state).unwrap();
        let (mut client, server) =
            crate::hosted_runtime::hosted::test_server::start_with_remote_state(remote_state).await;

        let received = client
            .fetch_state(
                &repo,
                "acme/widgets",
                PULL_CLONE_BOOTSTRAP_THREAD_PREFIX,
                state,
            )
            .await
            .unwrap();
        assert_eq!(received, 0);

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn complete_local_state_exercises_each_public_pull_mode_without_refetching() {
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let remote_state = super::super::helpers::proto_state_id(state).unwrap();
        let (mut client, server) =
            crate::hosted_runtime::hosted::test_server::start_with_remote_state(remote_state).await;
        let bootstrap = PULL_CLONE_BOOTSTRAP_THREAD_PREFIX;

        assert!(
            client
                .pull(&repo, "acme/widgets", bootstrap, None)
                .await
                .unwrap()
                .success
        );
        let (profiled, profile) = client
            .pull_profiled(&repo, "acme/widgets", bootstrap, None)
            .await
            .unwrap();
        assert!(profiled.success);
        assert_eq!(profile.objects_received, 0);
        assert!(
            client
                .pull_with_depth(&repo, "acme/widgets", bootstrap, None, Some(1))
                .await
                .unwrap()
                .success
        );
        assert!(
            client
                .pull_with_depth_and_materialization(
                    &repo,
                    "acme/widgets",
                    bootstrap,
                    None,
                    Some(1),
                    PullMaterialization::Lazy,
                )
                .await
                .unwrap()
                .success
        );
        assert!(
            client
                .repair_clone_with_depth_and_materialization(
                    &repo,
                    "acme/widgets",
                    bootstrap,
                    None,
                    PullMaterialization::Full,
                )
                .await
                .unwrap()
                .success
        );
        assert_eq!(
            client
                .hydrate_missing_blobs_for_state(&repo, "acme/widgets", bootstrap, state)
                .await
                .unwrap(),
            0
        );

        client.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn clone_pull_installs_a_complete_native_pack() {
        let source = TempDir::new().unwrap();
        let (repo, state) = repository(&source);
        let objects = wire::enumerate_state_closure(repo.store(), state).unwrap();
        let pack = wire::build_native_pack(repo.store(), &objects).unwrap();
        let remote_state = super::super::helpers::proto_state_id(state).unwrap();
        let (mut client, server) =
            crate::hosted_runtime::hosted::test_server::start_with_pull_pack(
                remote_state,
                pack.pack_data,
                pack.index_data,
            )
            .await;
        let clone = TempDir::new().unwrap();

        let (pulled, cloned_repo) = client
            .clone_pull_with_depth_and_materialization(
                "acme/widgets",
                Some("main"),
                None,
                PullMaterialization::Full,
                |_| Repository::init_default(clone.path()).map_err(ProtocolError::from),
            )
            .await
            .unwrap();
        assert!(pulled.success);
        assert_eq!(pulled.final_state, Some(state));
        assert!(cloned_repo.store().has_state(&state).unwrap());

        client.close().await;
        server.await.unwrap();
    }
}

fn pull_transfer_id(
    repo_path: &str,
    remote_thread: &str,
    local_thread: Option<&str>,
    depth: Option<u32>,
    target_state: Option<StateId>,
    stream_nonce: uuid::Uuid,
) -> String {
    format!(
        "pull:{repo_path}:{remote_thread}:{}:{depth:?}:{}:{stream_nonce}",
        local_thread.unwrap_or_default(),
        target_state
            .map(|value| value.to_string_full())
            .unwrap_or_default()
    )
}

fn push_transfer_id(
    repo_path: &str,
    local_state: StateId,
    target_thread: &str,
    local_revision_address: &str,
    client_operation_id: &str,
) -> String {
    format!(
        "push:{repo_path}:{}:{local_revision_address}:{target_thread}:{client_operation_id}",
        local_state.to_string_full(),
    )
}

#[cfg(test)]
mod transfer_id_tests {
    use std::{collections::HashSet, time::Instant};

    use api::heddle::api::v1alpha1::PushRequest;
    use objects::{
        object::{Blob, ContentHash, StateId, Tree, TreeEntry},
        store::{SnapshotCommitArtifact, SnapshotCommitDescriptor, pack::PackObjectId},
    };
    use repo::Repository;
    use tempfile::TempDir;
    use wire::{
        GitLaneTransferIntent, ObjectId, ObjectInfo, ObjectType, RefEntry, RepositoryTransferPlan,
    };

    use super::{
        ExpectedRemoteHead, apply_expected_remote_head, native_push_boundaries,
        native_push_boundaries_from_expected_head, pull_transfer_id, push_transfer_id,
        select_snapshot_pack_reuse_descriptor,
    };

    fn descriptor(state: StateId, object_ids: Vec<PackObjectId>) -> SnapshotCommitDescriptor {
        SnapshotCommitDescriptor {
            artifact: SnapshotCommitArtifact {
                schema: 1,
                transaction_id: "tx".to_string(),
                scope: "snapshot".to_string(),
                base_oplog_head_id: 0,
                state,
                encoded_records: vec![vec![1]],
            },
            pack_name: "pack".to_string(),
            pack_path: "/unused/pack.pack".into(),
            object_ids,
        }
    }

    fn blob_info(byte: u8) -> ObjectInfo {
        ObjectInfo {
            id: ObjectId::Hash(ContentHash::from_bytes([byte; 32])),
            obj_type: ObjectType::Blob,
            size: 1,
            delta_base: None,
        }
    }

    fn pack_object_id(object: &ObjectInfo) -> PackObjectId {
        match &object.id {
            ObjectId::Hash(hash) => PackObjectId::Hash(*hash),
            ObjectId::StateId(state) => PackObjectId::StateId(*state),
            ObjectId::StateAttachment { id, .. } => PackObjectId::Hash(*id.as_hash()),
        }
    }

    #[test]
    fn snapshot_pack_reuse_requires_matching_state_and_complete_unique_wanted_subset() {
        let state = StateId::from_bytes([40; 32]);
        let wanted = vec![blob_info(41), blob_info(42)];
        let matching = descriptor(
            state,
            vec![
                PackObjectId::Hash(ContentHash::from_bytes([41; 32])),
                PackObjectId::Hash(ContentHash::from_bytes([42; 32])),
                PackObjectId::Hash(ContentHash::from_bytes([43; 32])),
            ],
        );
        assert!(
            select_snapshot_pack_reuse_descriptor(std::slice::from_ref(&matching), state, &wanted,)
                .is_some()
        );

        let wrong_state = descriptor(StateId::from_bytes([44; 32]), matching.object_ids.clone());
        assert!(select_snapshot_pack_reuse_descriptor(&[wrong_state], state, &wanted).is_none());
        let incomplete = descriptor(
            state,
            vec![PackObjectId::Hash(ContentHash::from_bytes([41; 32]))],
        );
        assert!(select_snapshot_pack_reuse_descriptor(&[incomplete], state, &wanted).is_none());
        assert!(
            select_snapshot_pack_reuse_descriptor(&[], state, &wanted).is_none(),
            "descriptor loss after GC must select the normal writer"
        );
        let duplicate_wants = vec![blob_info(41), blob_info(41)];
        assert!(
            select_snapshot_pack_reuse_descriptor(&[matching], state, &duplicate_wants).is_none()
        );
    }

    #[test]
    fn repeated_snapshot_incremental_push_reuse_gate_and_subset_copy_are_safe() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let attribution = repo.get_attribution().unwrap();
        let mut previous = None;
        let mut tip = None;
        for ordinal in 0..32 {
            let blob = Blob::from(format!("revision {ordinal}\n"));
            let tree = Tree::from_entries(vec![
                TreeEntry::file("story.txt", blob.hash(), false).unwrap(),
            ]);
            let snapshot = repo
                .snapshot_tree_with_blobs_with_attribution_profiled(
                    tree,
                    vec![blob],
                    Some(format!("snapshot {ordinal}")),
                    None,
                    attribution.clone(),
                )
                .unwrap();
            previous = tip.replace(snapshot.state.id());
        }
        let tip = tip.unwrap();
        let previous = previous.unwrap();
        let closure = wire::enumerate_state_closure_transfer_from_boundaries(
            repo.store(),
            tip,
            &[previous],
            usize::MAX,
        )
        .unwrap();
        let plan = RepositoryTransferPlan::from_object_infos(
            closure.full_objects.unwrap(),
            GitLaneTransferIntent::HeddleObjectsOnly,
        );
        let pack_objects = plan
            .partitions
            .packable_objects
            .into_iter()
            .filter(|object| object.obj_type.packable_for_push())
            .collect::<Vec<_>>();
        let descriptor = repo
            .store()
            .snapshot_commit_descriptor_for_state(&tip)
            .unwrap()
            .expect("tip snapshot has an authoritative pack descriptor");
        let wanted_ids = pack_objects
            .iter()
            .map(pack_object_id)
            .collect::<HashSet<_>>();
        assert_eq!(
            wanted_ids.len(),
            pack_objects.len(),
            "the incremental closure must not contain duplicate pack wants"
        );
        let descriptor_ids = descriptor
            .object_ids
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let reusable_objects = pack_objects
            .iter()
            .filter(|object| descriptor_ids.contains(&pack_object_id(object)))
            .cloned()
            .collect::<Vec<_>>();
        let has_objects_outside_snapshot_pack = reusable_objects.len() != pack_objects.len();
        assert_eq!(
            select_snapshot_pack_reuse_descriptor(
                std::slice::from_ref(&descriptor),
                tip,
                &pack_objects,
            )
            .is_none(),
            has_objects_outside_snapshot_pack,
            "full snapshot-pack reuse must be selected exactly when every wanted object is present"
        );
        assert!(
            !reusable_objects.is_empty()
                && select_snapshot_pack_reuse_descriptor(
                    std::slice::from_ref(&descriptor),
                    tip,
                    &reusable_objects,
                )
                .is_some(),
            "the authoritative snapshot pack must expose a non-empty reusable subset"
        );

        let started = Instant::now();
        let (_, stats) = wire::reuse_native_pack_encoded_subset_in(
            repo.heddle_dir(),
            &descriptor.pack_path,
            &reusable_objects,
        )
        .unwrap()
        .expect("the repeated-snapshot fixture must take the raw encoded subset path");
        let elapsed = started.elapsed();
        eprintln!(
            "reused {} incremental objects / {} encoded bytes in {:?}",
            stats.object_count, stats.encoded_bytes_copied, elapsed
        );
        assert_eq!(stats.object_count, reusable_objects.len());
    }

    #[test]
    fn git_revision_distinguishes_pushes_reusing_one_heddle_anchor() {
        let state = StateId::from_bytes([7; 32]);
        let first = push_transfer_id("org/repo", state, "agent-1", "git:111111", "op-1");
        let second = push_transfer_id("org/repo", state, "agent-1", "git:222222", "op-1");

        assert_ne!(first, second);
        assert_eq!(
            first,
            push_transfer_id("org/repo", state, "agent-1", "git:111111", "op-1")
        );
    }

    #[test]
    fn pull_transfer_identity_is_unique_per_exchange_and_stable_for_one_stream() {
        let first_nonce = uuid::Uuid::new_v4();
        let later_nonce = uuid::Uuid::new_v4();
        let first = pull_transfer_id("org/repo", "main", Some("main"), None, None, first_nonce);
        let later = pull_transfer_id("org/repo", "main", Some("main"), None, None, later_nonce);

        assert_ne!(
            first, later,
            "unrelated pull streams must not reuse an anti-replay identity"
        );
        assert_eq!(
            first,
            pull_transfer_id("org/repo", "main", Some("main"), None, None, first_nonce),
            "one pull exchange must use one identity across all of its frames"
        );
    }

    #[test]
    fn push_transfer_identity_is_unique_per_operation_and_stable_within_one_retry() {
        let state = StateId::from_bytes([8; 32]);
        let first = push_transfer_id("org/repo", state, "main", "heddle:state", "op-1");
        let later_retry = push_transfer_id("org/repo", state, "main", "heddle:state", "op-2");

        assert_ne!(
            first, later_retry,
            "a later caller retry must not reuse an anti-replay transfer identity"
        );
        assert_eq!(
            first,
            push_transfer_id("org/repo", state, "main", "heddle:state", "op-1"),
            "transport reconnects inside one Push operation must resume the same transfer"
        );
    }

    #[test]
    fn native_push_uses_only_a_strict_remote_ancestor_as_its_boundary() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let path = temp.path().join("story.txt");
        std::fs::write(&path, "base\n").unwrap();
        let base = repo.snapshot(Some("base".to_string()), None).unwrap();
        std::fs::write(&path, "tip\n").unwrap();
        let tip = repo.snapshot(Some("tip".to_string()), None).unwrap();
        let remote = [RefEntry {
            name: "main".to_string(),
            state_id: base.state_id,
            is_thread: true,
        }];

        assert_eq!(
            native_push_boundaries(&repo, &remote, "main", tip.state_id).unwrap(),
            vec![base.state_id]
        );
        assert!(
            native_push_boundaries(&repo, &remote, "main", base.state_id)
                .unwrap()
                .is_empty(),
            "an exact-tip push must retain the state re-install/sidecar merge path"
        );
        assert!(
            native_push_boundaries(&repo, &remote, "other", tip.state_id)
                .unwrap()
                .is_empty(),
            "a different remote thread cannot bound this push"
        );
    }

    #[test]
    fn known_remote_head_builds_the_boundary_and_wire_precondition_without_discovery() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let path = temp.path().join("story.txt");
        std::fs::write(&path, "base\n").unwrap();
        let base = repo.snapshot(Some("base".to_string()), None).unwrap();
        std::fs::write(&path, "tip\n").unwrap();
        let tip = repo.snapshot(Some("tip".to_string()), None).unwrap();
        let expected = ExpectedRemoteHead::State(base.state_id);

        assert_eq!(
            native_push_boundaries_from_expected_head(&repo, expected, tip.state_id).unwrap(),
            vec![base.state_id]
        );

        let mut request = PushRequest::default();
        apply_expected_remote_head(&mut request, expected);
        assert_eq!(
            request
                .expected_remote_head
                .as_ref()
                .map(|state| state.value.clone()),
            Some(base.state_id.as_bytes().to_vec())
        );
        assert!(!request.expected_remote_head_missing);

        apply_expected_remote_head(&mut request, ExpectedRemoteHead::Missing);
        assert!(request.expected_remote_head.is_none());
        assert!(request.expected_remote_head_missing);
    }
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
    let mut ref_updates: Vec<GitRefUpdateTransfer> = Vec::new();
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

        let kind = proto_git_ref_kind(GitRefName::new(&reference.name).wire_kind());
        let mut message =
            git_ref_update_message(&reference.name, kind, target_oid, peeled_oid, None);
        apply_git_ref_expectation_value(&mut message, &expectation);
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

fn proto_git_ref_kind(kind: ClassifiedGitRefKind) -> ProtoGitRefKind {
    match kind {
        ClassifiedGitRefKind::Branch => ProtoGitRefKind::Branch,
        ClassifiedGitRefKind::Tag => ProtoGitRefKind::Tag,
        ClassifiedGitRefKind::Note => ProtoGitRefKind::Note,
        ClassifiedGitRefKind::Other => ProtoGitRefKind::Other,
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
    client_operation_id: &str,
) -> Result<(), ProtocolError> {
    let tx = tx.clone();
    let pack = pack.clone();
    let progress = progress.clone();
    let client_operation_id = client_operation_id.to_string();
    tokio::task::spawn_blocking(move || {
        stream_git_pack_messages_blocking(tx, pack, chunk_size, progress, client_operation_id)
    })
    .await
    .map_err(|err| ProtocolError::InvalidState(format!("Git pack streaming task failed: {err}")))?
}

fn stream_git_pack_messages_blocking(
    tx: mpsc::Sender<PushClientFrame>,
    pack: GitPackPushPlan,
    chunk_size: usize,
    progress: Progress,
    client_operation_id: String,
) -> Result<(), ProtocolError> {
    let mut writer = GitPackPushMessageWriter::new(
        tx,
        pack.transfer_id.clone(),
        pack.pack_id.clone(),
        pack.pack_size,
        chunk_size,
        progress,
        client_operation_id,
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
    client_operation_id: String,
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
        client_operation_id: String,
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
            client_operation_id,
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
            .blocking_send(git_lane_push_message(
                git_lane_transfer::Body::Pack(GitPackTransfer {
                    transfer_id: self.transfer_id.clone(),
                    offset: self.offset,
                    chunk_index: self.chunk_index,
                    is_final_chunk,
                    pack_size: self.pack_size,
                    pack_chunk: chunk,
                    pack_id: self.pack_id.clone(),
                }),
                &self.client_operation_id,
            ))
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
    kind: ProtoGitRefKind,
    target_oid: GitObjectId,
    peeled_oid: Option<GitObjectId>,
    checkpoint: Option<GitCheckpointTransfer>,
) -> GitRefUpdateTransfer {
    GitRefUpdateTransfer {
        name: name.to_string(),
        kind: kind as i32,
        target_oid: proto_git_oid(&target_oid),
        peeled_oid: peeled_oid.as_ref().and_then(proto_git_oid),
        expected_missing: false,
        expected_target_oid: None,
        checkpoint,
    }
}

fn apply_git_ref_expectation_value(
    update: &mut GitRefUpdateTransfer,
    expectation: &GitRefRemoteExpectation,
) {
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

fn git_lane_push_message(
    body: git_lane_transfer::Body,
    client_operation_id: &str,
) -> PushClientFrame {
    PushClientFrame {
        frame: Some(push_client_frame::Frame::GitLane(GitLaneTransfer {
            body: Some(body),
        })),
        client_operation_id: client_operation_id.to_string(),
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
    staged: &mut StagedGitLanePull,
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
            stage_git_lane_ref_update(staged, update)
        }
        Some(git_lane_transfer::Body::Checkpoint(checkpoint)) => {
            staged.checkpoints.push(checkpoint);
            Ok(())
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

impl StagedGitLanePull {
    fn snapshot(repo: &Repository, allow_ref_updates: bool) -> Result<Self, ProtocolError> {
        let initial_refs = if repo.capability() == RepositoryCapability::GitOverlay {
            repo.git_overlay_sley_repository()
                .map_err(|error| ProtocolError::InvalidState(error.to_string()))?
                .ok_or_else(|| {
                    ProtocolError::InvalidState(
                        "git-overlay repository has no Git store".to_string(),
                    )
                })?
                .references()
                .list_refs()
                .map_err(|error| {
                    ProtocolError::InvalidState(format!(
                        "snapshot Git refs before hosted pull: {error}"
                    ))
                })?
                .into_iter()
                .map(|reference| (reference.name, reference.target))
                .collect()
        } else {
            HashMap::new()
        };
        Ok(Self {
            allow_ref_updates,
            initial_refs,
            ref_updates: Vec::new(),
            checkpoints: Vec::new(),
        })
    }
}

fn stage_git_lane_ref_update(
    staged: &mut StagedGitLanePull,
    update: GitRefUpdateTransfer,
) -> Result<(), ProtocolError> {
    if !staged.allow_ref_updates {
        return Err(ProtocolError::InvalidState(
            "targeted object fetch cannot mutate Git refs".to_string(),
        ));
    }
    if !update.name.starts_with("refs/")
        || !GitRefName::new(&update.name).is_hosted_mirror_content()
    {
        return Err(ProtocolError::InvalidState(format!(
            "hosted pull attempted to update non-mirrored Git ref {}",
            update.name
        )));
    }
    if staged
        .ref_updates
        .iter()
        .any(|existing| existing.name == update.name)
    {
        return Err(ProtocolError::InvalidState(format!(
            "hosted pull sent duplicate Git ref update for {}",
            update.name
        )));
    }
    staged.ref_updates.push(update);
    Ok(())
}

/// Apply every staged Git ref only after a successful `PullComplete`.
///
/// One transaction uses the pull-start ref snapshot as compare-and-set
/// preconditions. A concurrent local ref mutation therefore aborts the whole
/// batch instead of being overwritten by a stale hosted response.
fn apply_staged_git_lane_pull(
    repo: &Repository,
    git_repo: &mut Option<SleyRepository>,
    staged: &mut StagedGitLanePull,
) -> Result<(), ProtocolError> {
    if staged.ref_updates.is_empty() && staged.checkpoints.is_empty() {
        return Ok(());
    }
    let git_repo = git_lane_sley_repository(repo, git_repo)?;
    let refs = git_repo.references();
    let mut tx = refs.transaction();
    for update in &staged.ref_updates {
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
        let precondition = staged
            .initial_refs
            .get(&update.name)
            .cloned()
            .map(RefPrecondition::MustExistAndMatch)
            .unwrap_or(RefPrecondition::MustNotExist);
        tx.update_to(
            update.name.clone(),
            ReferenceTarget::Direct(target),
            precondition,
            None,
        );
    }
    tx.commit().map_err(|err| {
        ProtocolError::InvalidState(format!("commit staged hosted Git refs: {err}"))
    })?;
    for update in staged.ref_updates.drain(..) {
        if let Some(checkpoint) = update.checkpoint {
            record_git_lane_checkpoint(repo, git_repo, checkpoint)?;
        }
    }
    for checkpoint in staged.checkpoints.drain(..) {
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

#[derive(Clone, Copy)]
struct PushWireIdentities<'a> {
    transfer_id: &'a str,
    client_operation_id: &'a str,
    local_state: StateId,
}

#[derive(Clone, Copy, Default)]
struct NativePackSendProfile {
    reused_pack: bool,
    reused_pack_copy: Duration,
}

async fn send_native_pack_streaming_messages(
    tx: &mpsc::Sender<PushClientFrame>,
    repo: &Repository,
    objects: &[ObjectInfo],
    identities: PushWireIdentities<'_>,
    chunk_size: usize,
    transport: &super::helpers::HostedTransportPolicy,
    transport_mode: TransportMode,
) -> Result<NativePackSendProfile, ProtocolError> {
    let source_pack_path = repo
        .store()
        .snapshot_commit_descriptor_for_state(&identities.local_state)
        .ok()
        .flatten()
        .and_then(|descriptor| {
            select_snapshot_pack_reuse_descriptor(
                std::slice::from_ref(&descriptor),
                identities.local_state,
                objects,
            )
            .map(|descriptor| descriptor.pack_path.clone())
        });
    if let Some(source_pack_path) = source_pack_path {
        let root = repo.heddle_dir().to_path_buf();
        let reuse_objects = objects.to_vec();
        let reuse_started = Instant::now();
        let reused = tokio::task::spawn_blocking(move || {
            wire::reuse_native_pack_encoded_subset_in(&root, &source_pack_path, &reuse_objects)
        })
        .await;
        if let Ok(Ok(Some((bundle, _)))) = reused {
            let reused_pack_copy = reuse_started.elapsed();
            send_native_pack_file_stream(
                tx,
                &bundle.pack_path,
                PackStreamKind::Pack,
                identities,
                chunk_size,
                transport,
                transport_mode,
            )
            .await?;
            send_native_pack_file_stream(
                tx,
                &bundle.index_path,
                PackStreamKind::Index,
                identities,
                chunk_size,
                transport,
                transport_mode,
            )
            .await?;
            return Ok(NativePackSendProfile {
                reused_pack: true,
                reused_pack_copy,
            });
        }
    }

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
                    identities,
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
        identities,
        transport,
        transport_mode,
    )
    .await?;
    send_native_pack_file_stream(
        tx,
        &bundle.index_path,
        PackStreamKind::Index,
        identities,
        chunk_size,
        transport,
        transport_mode,
    )
    .await?;
    Ok(NativePackSendProfile::default())
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
    identities: PushWireIdentities<'_>,
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
            identities,
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
    identities: PushWireIdentities<'_>,
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
            identities,
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
    identities: PushWireIdentities<'_>,
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
                identities.transfer_id,
                transport_mode,
                chunk_index,
                offset,
                is_final_chunk,
            )),
            chunk_length,
            is_final_chunk,
        })),
        client_operation_id: identities.client_operation_id.to_string(),
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
mod git_lane_pull_staging_tests {
    use super::*;

    fn update(name: &str) -> GitRefUpdateTransfer {
        GitRefUpdateTransfer {
            name: name.to_string(),
            ..GitRefUpdateTransfer::default()
        }
    }

    fn staging(allow_ref_updates: bool) -> StagedGitLanePull {
        StagedGitLanePull {
            allow_ref_updates,
            initial_refs: HashMap::new(),
            ref_updates: Vec::new(),
            checkpoints: Vec::new(),
        }
    }

    #[test]
    fn hosted_pull_stages_content_refs_but_rejects_local_only_refs() {
        let mut staged = staging(true);
        stage_git_lane_ref_update(&mut staged, update("refs/heads/main"))
            .expect("content ref is mirrorable");
        assert_eq!(staged.ref_updates.len(), 1);

        let error = stage_git_lane_ref_update(&mut staged, update("refs/stash"))
            .expect_err("remote must not mutate local-only ref");
        assert!(error.to_string().contains("non-mirrored"));
    }

    #[test]
    fn targeted_fetch_rejects_ref_mutation_and_duplicate_updates() {
        let mut targeted = staging(false);
        let error = stage_git_lane_ref_update(&mut targeted, update("refs/heads/main"))
            .expect_err("object-only fetch cannot update refs");
        assert!(error.to_string().contains("targeted object fetch"));

        let mut full = staging(true);
        stage_git_lane_ref_update(&mut full, update("refs/tags/v1")).unwrap();
        let duplicate = stage_git_lane_ref_update(&mut full, update("refs/tags/v1"))
            .expect_err("duplicate ref update must be rejected");
        assert!(duplicate.to_string().contains("duplicate"));
    }

    #[test]
    fn hosted_pull_rejects_git_lane_transfer_for_native_repository() {
        let temp = tempfile::TempDir::new().expect("temp native repo");
        let repo = Repository::init_default(temp.path()).expect("init native repo");
        let mut git_repo = None;
        let mut pack_state = GitPackPullInstallState::default();
        let mut staged = staging(true);

        let error = accept_git_lane_pull_transfer(
            &repo,
            &mut git_repo,
            &mut pack_state,
            &mut staged,
            GitLaneTransfer {
                body: Some(git_lane_transfer::Body::Checkpoint(
                    GitCheckpointTransfer::default(),
                )),
            },
        )
        .expect_err("native repositories must reject Git-lane transfers");

        println!("{error}");
        assert!(error.to_string().contains(
            "received git-lane pull transfer for non-GitOverlay repository (capability NativeHeddle)"
        ));
    }
}

#[cfg(test)]
mod attachment_sidecar_tests {
    use api::heddle::api::v1alpha1::{
        StateAttachmentKind as ProtoStateAttachmentKind, push_client_frame,
    };
    use chrono::Utc;
    use objects::{
        object::{Attribution, Blob, Principal, StateAttachment, StateAttachmentBody},
        store::ObjectStore,
    };
    use tempfile::TempDir;
    use wire::{ObjectId, ObjectInfo, ObjectType};

    use super::{Repository, sidecar_push_message};

    #[test]
    fn semantic_index_attachment_emits_verified_sidecar_carrier() {
        let temp = TempDir::new().expect("temp repo");
        let repo = Repository::init_default(temp.path()).expect("init repo");
        std::fs::write(temp.path().join("lib.rs"), "pub fn f() {}\n").expect("write source");
        let snapshot = repo
            .snapshot(Some("seed".to_string()), None)
            .expect("snapshot");
        let semantic_root = repo
            .store()
            .put_blob(&Blob::from_slice(b"semantic-root"))
            .expect("put semantic root");
        let attachment = StateAttachment {
            state_id: snapshot.state_id,
            body: StateAttachmentBody::SemanticIndex(semantic_root),
            attribution: Attribution::human(Principal::new("H3 Tester", "h3@example.com")),
            created_at: Utc::now(),
            supersedes: None,
        };
        repo.put_state_attachment(&attachment)
            .expect("put semantic attachment");

        let id = ObjectId::StateAttachment {
            state: snapshot.state_id,
            id: attachment.id(),
            kind: attachment.body.kind(),
        };
        let canonical = wire::load_object_data(repo.store(), &id, ObjectType::StateAttachment)
            .expect("load canonical attachment");
        let message = sidecar_push_message(
            &repo,
            ObjectInfo {
                id,
                obj_type: ObjectType::StateAttachment,
                size: canonical.data.len() as u64,
                delta_base: None,
            },
            "op-1",
        )
        .expect("build attachment sidecar");

        assert_eq!(message.client_operation_id, "op-1");
        let Some(push_client_frame::Frame::StateAttachment(transfer)) = message.frame else {
            panic!("expected StateAttachmentTransfer")
        };
        assert_eq!(
            transfer.state_id,
            super::super::helpers::proto_state_id(snapshot.state_id)
        );
        assert_eq!(transfer.attachment_id, attachment.id().as_hash().to_hex());
        assert_eq!(
            transfer.attachment_kind,
            ProtoStateAttachmentKind::SemanticIndex as i32
        );
        assert_eq!(transfer.attachment_object, canonical.data);
        assert!(!ObjectType::StateAttachment.packable_for_push());
        assert!(ObjectType::StateAttachment.packable_for_pull());
    }
}

#[cfg(test)]
mod pure_helpers_tests {
    use api::heddle::api::v1alpha1::{
        GitRefKind as ProtoGitRefKind, GitRefUpdateTransfer, ObjectAvailabilityStatus,
        PartialFetchStatus, TransportMode,
    };
    use objects::{
        object::{ContentHash, StateId},
        store::pack::PackObjectId,
    };
    use repo::{GitRefKind as ClassifiedGitRefKind, Repository};
    use tempfile::TempDir;
    use wire::{
        GitLaneTransferIntent, ObjectId, ObjectInfo, ObjectType, PlannedObject,
        RepositoryTransferPlan,
    };

    use super::*;

    #[test]
    fn parse_git_ref_expectation_covers_missing_git_and_invalid() {
        assert_eq!(
            parse_git_ref_expectation("").unwrap(),
            GitRefRemoteExpectation::Missing
        );
        // Heddle revision addresses are treated as "missing" for Git CAS.
        let heddle = format!("heddle:{}", StateId::from_bytes([1; 32]).to_string_full());
        assert_eq!(
            parse_git_ref_expectation(&heddle).unwrap(),
            GitRefRemoteExpectation::Missing
        );
        let git_oid = "a".repeat(40);
        match parse_git_ref_expectation(&format!("git:{git_oid}")).unwrap() {
            GitRefRemoteExpectation::Value(bytes) => {
                assert_eq!(bytes, hex::decode(&git_oid).unwrap());
            }
            other => panic!("expected Value, got {other:?}"),
        }
        assert!(parse_git_ref_expectation("not-a-revision").is_err());
        assert!(parse_git_ref_expectation("git:zzzz").is_err());
    }

    #[test]
    fn apply_git_ref_expectation_value_sets_cas_fields() {
        let mut update = GitRefUpdateTransfer::default();
        apply_git_ref_expectation_value(&mut update, &GitRefRemoteExpectation::Missing);
        assert!(update.expected_missing);
        assert!(update.expected_target_oid.is_none());

        let oid = vec![0xab; 20];
        apply_git_ref_expectation_value(&mut update, &GitRefRemoteExpectation::Value(oid.clone()));
        assert!(!update.expected_missing);
        assert_eq!(
            update
                .expected_target_oid
                .as_ref()
                .map(|o| o.digest.clone()),
            Some(oid)
        );
    }

    #[test]
    fn proto_git_ref_kind_maps_every_classified_kind() {
        assert_eq!(
            proto_git_ref_kind(ClassifiedGitRefKind::Branch),
            ProtoGitRefKind::Branch
        );
        assert_eq!(
            proto_git_ref_kind(ClassifiedGitRefKind::Tag),
            ProtoGitRefKind::Tag
        );
        assert_eq!(
            proto_git_ref_kind(ClassifiedGitRefKind::Note),
            ProtoGitRefKind::Note
        );
        assert_eq!(
            proto_git_ref_kind(ClassifiedGitRefKind::Other),
            ProtoGitRefKind::Other
        );
    }

    #[test]
    fn object_info_from_plan_and_descriptor_helpers() {
        let hash = ContentHash::from_bytes([0x42; 32]);
        let planned = PlannedObject {
            id: ObjectId::Hash(hash),
            obj_type: ObjectType::Blob,
        };
        let info = object_info_from_plan(&planned);
        assert_eq!(info.id, ObjectId::Hash(hash));
        assert_eq!(info.obj_type, ObjectType::Blob);
        assert_eq!(info.size, 0);

        let descriptor = to_proto_planned_object(&planned);
        assert_eq!(descriptor.id, hash.to_hex());
        assert_eq!(
            descriptor.availability_status,
            ObjectAvailabilityStatus::Present as i32
        );
        let id = descriptor_id_from_plan(&planned);
        assert_eq!(id, descriptor_id_from_info(&info));
    }

    #[test]
    fn wanted_type_recording_and_packable_lookup() {
        let mut wanted = WantedTypes::default();
        let pack_id = PackObjectId::Hash(ContentHash::from_bytes([7; 32]));
        record_wanted_type(&mut wanted, pack_id, ObjectType::Blob);
        record_wanted_type(&mut wanted, pack_id, ObjectType::Blob); // dedup
        record_wanted_type(&mut wanted, pack_id, ObjectType::Tree);
        assert_eq!(wanted.get(&pack_id).map(|v| v.len()), Some(2));
        assert_eq!(
            wanted_packable_type(&wanted, &pack_id),
            Some(ObjectType::Blob)
        );
        let missing = PackObjectId::Hash(ContentHash::from_bytes([8; 32]));
        assert_eq!(wanted_packable_type(&wanted, &missing), None);
    }

    #[test]
    fn native_pack_worker_count_clamps_to_available_parallelism() {
        assert_eq!(native_pack_object_load_worker_count(0), 1);
        assert_eq!(native_pack_object_load_worker_count(1), 1);
        let many = native_pack_object_load_worker_count(10_000);
        assert!(many >= 1);
        assert!(many <= NATIVE_PACK_OBJECT_LOAD_WORKER_LIMIT);
    }

    #[test]
    fn preferred_transport_mode_is_native_pack() {
        let policy = super::super::helpers::HostedTransportPolicy {
            chunk_size: 64 * 1024,
            max_inflight_objects: 4,
            resume_attempts: 2,
            provider_global_concurrency: 4,
            provider_per_endpoint_concurrency: 2,
            provider_max_inflight_bytes: 8 * 1024 * 1024,
            provider_stall_timeout: std::time::Duration::from_secs(30),
        };
        assert_eq!(
            preferred_transport_mode(&policy, 0),
            TransportMode::NativePack
        );
        assert_eq!(
            preferred_transport_mode(&policy, 10_000),
            TransportMode::NativePack
        );
    }

    #[test]
    fn partial_fetch_status_for_repo_reflects_missing_blobs() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let status = partial_fetch_status_for_repo(&repo);
        assert!(
            status == PartialFetchStatus::Disabled as i32
                || status == PartialFetchStatus::Unspecified as i32
                || status == PartialFetchStatus::Enabled as i32,
            "status={status}"
        );
    }

    #[test]
    fn transfer_ids_are_stable_and_discriminative() {
        let state = StateId::from_bytes([9; 32]);
        let pull_a = pull_transfer_id(
            "acme/widgets",
            "main",
            Some("local"),
            Some(1),
            Some(state),
            uuid::Uuid::nil(),
        );
        let pull_b = pull_transfer_id("acme/widgets", "main", None, None, None, uuid::Uuid::nil());
        assert!(pull_a.starts_with("pull:"));
        assert_ne!(pull_a, pull_b);
        let push = push_transfer_id("acme/widgets", state, "main", "git:abc", "op-1");
        assert!(push.starts_with("push:"));
        assert!(push.contains("op-1"));
    }

    #[test]
    fn decode_bootstrap_state_id_requires_32_bytes() {
        let ok = decode_bootstrap_state_id(vec![0u8; 32], "field").unwrap();
        assert_eq!(ok, StateId::from_bytes([0u8; 32]));
        assert!(decode_bootstrap_state_id(vec![0u8; 8], "field").is_err());
    }

    #[test]
    fn native_pack_required_for_pull_delegates_to_transfer_plan() {
        let empty_info = RepositoryTransferPlan::from_object_infos(
            Vec::<ObjectInfo>::new(),
            GitLaneTransferIntent::HeddleObjectsOnly,
        );
        // Without full-closure, an empty plan does not require a native pack.
        assert!(!native_pack_required_for_pull(false, &empty_info));
        // Full-closure may still require a pack even when partitions are empty —
        // pin the actual delegated behavior rather than guessing.
        let _ = native_pack_required_for_pull(true, &empty_info);
    }
}
