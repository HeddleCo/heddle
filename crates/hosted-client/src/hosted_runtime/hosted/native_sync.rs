// SPDX-License-Identifier: Apache-2.0
//! Hosted push/pull over v2 `PublishContent` and `Fetch`.
//!
//! Request and pack construction matches local device publication/fetch:
//! `SourcePack::prepare_with_references` plus `Remote::publish_content` /
//! `HostedClient::fetch_native_source`. Provider-preferred Fetch is selected
//! only when usable dial routes are present; otherwise the direct path runs.
//! No v1 RepoSyncService frames.
use std::time::Instant;

use api::heddle::api::{
    common::StateId as ApiStateId,
    v1alpha2::{
        self as contract, EndpointKind, EndpointRef, FetchOpen, ObservationMode,
        ObserveIdentityRequest, ObserveOptions, ObserveThreadsRequest, RevisionRef, SpoolRef,
        ThreadOverview, ThreadQuery, ThreadRef, TransferSelection, identity_event, revision_ref,
        thread_list_event, thread_query,
    },
};
use crypto::{Signer as _, thread_operation::SignedOperation};
use objects::{
    object::{
        CollaborationActor, ContentHash, StateId, ThreadName,
        thread_replication::{
            AuthoredCapture, GenesisOwner, OPERATION_FORMAT, ThreadGenesis, ThreadOperation,
            ThreadOperationBody, hosted_import::synthetic_initial_base,
        },
    },
    store::ObjectStore,
};
use repo::{Repository, SyncedThreadMetadata, ThreadManager, thread_replication::ThreadReplica};
use thread_api::{
    creation::ThreadCreation,
    publication::{PublicationOptions, PublicationOriginals, SourceBudget, SourcePack},
    rpc,
};
use uuid::Uuid;
use wire::{ProtocolError, PullComplete, PushComplete, RefEntry};

use super::{
    HostedClient, HostedRefEntry, PullBootstrapRefs, PullMaterialization,
    helpers::native_client_error,
    native_provider::preferred_fetch_open,
    persist_advertised_thread_identity,
    sync::{PullProfile, PushProfile, encode_empty_pull_bootstrap},
};

const PUBLISH: &str = "heddle.api.v1alpha2.SyncService/PublishContent";
const START: &str = "heddle.api.v1alpha2.ThreadService/StartThread";
const SOURCE_OBJECTS: usize = 100_000;
const SOURCE_BYTES: u64 = 256 * 1024 * 1024;
const ANCESTRY_RECORDS: usize = 10_000;
const ANCESTRY_BYTES: usize = 16 * 1024 * 1024;

fn native_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::InvalidState(error.to_string())
}

fn replica_err(error: repo::thread_replication::Error) -> ProtocolError {
    ProtocolError::InvalidState(error.to_string())
}

fn once_observe() -> ObserveOptions {
    ObserveOptions {
        mode: ObservationMode::Once as i32,
        ..Default::default()
    }
}

fn revision_ref(spool: &SpoolRef, state: StateId) -> RevisionRef {
    RevisionRef {
        spool: Some(spool.clone()),
        revision: Some(revision_ref::Revision::State(ApiStateId {
            value: state.as_bytes().to_vec(),
        })),
    }
}

fn state_from_revision(revision: Option<&RevisionRef>) -> Option<StateId> {
    match revision.and_then(|value| value.revision.as_ref()) {
        Some(revision_ref::Revision::State(id)) if id.value.len() == 32 => {
            id.value.as_slice().try_into().ok().map(StateId::from_bytes)
        }
        _ => None,
    }
}

fn overview_state(overview: &ThreadOverview) -> Option<StateId> {
    overview
        .source_heads
        .iter()
        .find_map(|head| state_from_revision(Some(head)))
}

fn overview_thread_id(overview: &ThreadOverview) -> Result<ContentHash, ProtocolError> {
    let value = overview
        .r#ref
        .as_ref()
        .and_then(|reference| reference.id.as_ref())
        .ok_or_else(|| ProtocolError::InvalidState("Thread identity absent".into()))?;
    let bytes: [u8; 32] = value
        .value
        .as_slice()
        .try_into()
        .map_err(|_| ProtocolError::InvalidState("Thread identity length".into()))?;
    Ok(ContentHash::from_bytes(bytes))
}

fn hosted_ref_from_overview(
    overview: &ThreadOverview,
) -> Result<Option<HostedRefEntry>, ProtocolError> {
    let Some(state_id) = overview_state(overview) else {
        return Ok(None);
    };
    let thread_id = overview_thread_id(overview)?;
    Ok(Some(HostedRefEntry::from_advertised(
        overview.name.clone(),
        state_id,
        true,
        repo::RevisionAddress::heddle(state_id).to_string(),
        Some(hex::encode(thread_id.as_bytes())),
    )))
}

fn signed_record(signed: &SignedOperation) -> Result<contract::SignedRecord, ProtocolError> {
    let operation = signed
        .verify()
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    Ok(contract::SignedRecord {
        format: OPERATION_FORMAT.into(),
        canonical_record: signed.canonical.clone(),
        signatures: vec![contract::RecordSignature {
            public_key: operation.publisher.to_vec(),
            signature: signed.signature.clone(),
        }],
    })
}

fn operation_id(method: &str, caller: String) -> String {
    super::operation_id::ClientOperationId::caller_or_fresh(method, caller).to_wire()
}

impl HostedClient {
    pub async fn list_refs(&mut self, repo_path: &str) -> Result<Vec<RefEntry>, ProtocolError> {
        Ok(self
            .list_refs_with_revision_addresses(repo_path)
            .await?
            .into_iter()
            .map(|entry| entry.to_wire_entry())
            .collect())
    }

    pub async fn list_refs_with_revision_addresses(
        &mut self,
        repo_path: &str,
    ) -> Result<Vec<HostedRefEntry>, ProtocolError> {
        Ok(self.advertised_pull_refs(repo_path).await?.refs)
    }

    async fn advertised_pull_refs(
        &mut self,
        repo_path: &str,
    ) -> Result<PullBootstrapRefs, ProtocolError> {
        let overviews = self.observe_thread_overviews(repo_path).await?;
        let mut refs = Vec::new();
        let mut head_thread = None;
        for overview in overviews {
            if head_thread.is_none() && (overview.name == "main" || overview.name == "master") {
                head_thread = Some(overview.name.clone());
            }
            if let Some(entry) = hosted_ref_from_overview(&overview)? {
                refs.push(entry);
            }
        }
        if head_thread.is_none() {
            head_thread = refs.first().map(|entry| entry.name.clone());
        }
        Ok(PullBootstrapRefs { head_thread, refs })
    }

    async fn observe_thread_overviews(
        &self,
        repo_path: &str,
    ) -> Result<Vec<ThreadOverview>, ProtocolError> {
        let spool = self.resolve_spool_ref(repo_path).await?;
        let remote = self.native().await.map_err(native_error)?;
        let mut observation = remote
            .observe::<rpc::ThreadServiceObserveThreads>(
                ObserveThreadsRequest {
                    query: Some(ThreadQuery {
                        spools: vec![spool],
                        order: thread_query::Order::NameAsc as i32,
                        ..Default::default()
                    }),
                    observe: Some(once_observe()),
                    ..Default::default()
                },
                None,
            )
            .await
            .map_err(native_error)?;
        let mut threads = Vec::new();
        while let Some(batch) = observation.next_commit().await.map_err(native_error)? {
            for change in batch.changes {
                if let thread_list_event::Payload::Thread(thread) = change {
                    threads.push(thread);
                }
            }
        }
        Ok(threads)
    }

    pub(super) async fn current_creator_authority(
        &self,
    ) -> Result<(Uuid, Vec<u8>, Option<String>), ProtocolError> {
        let owner = self.current_owner_state().await?;
        let owner_id = Uuid::parse_str(
            &owner
                .owner
                .as_ref()
                .ok_or_else(|| ProtocolError::InvalidState("current owner identity absent".into()))?
                .id,
        )
        .map_err(native_error)?;
        let remote = self.native().await.map_err(native_error)?;
        let mut observation = remote
            .observe::<rpc::IdentityServiceObserveIdentity>(
                ObserveIdentityRequest {
                    include_current_credential: true,
                    observe: Some(once_observe()),
                    ..Default::default()
                },
                None,
            )
            .await
            .map_err(native_error)?;
        let batch = observation
            .next_commit()
            .await
            .map_err(native_error)?
            .ok_or_else(|| {
                ProtocolError::InvalidState(
                    "identity observation ended without a checkpoint".into(),
                )
            })?;
        let (authority, agent_id) = batch
            .changes
            .into_iter()
            .find_map(|change| match change {
                identity_event::Payload::CurrentCredential(credential)
                    if !credential.thread_control_authority.is_empty() =>
                {
                    let agent_id = if credential.acting_agent_id.is_empty() {
                        None
                    } else {
                        Some(credential.acting_agent_id)
                    };
                    Some((credential.thread_control_authority, agent_id))
                }
                _ => None,
            })
            .ok_or_else(|| {
                ProtocolError::InvalidState(
                    "current credential has no portable Thread control authority".into(),
                )
            })?;
        Ok((owner_id, authority, agent_id))
    }

    async fn start_hosted_thread(
        &self,
        repo: &Repository,
        repo_path: &str,
        name: &str,
        local_state: StateId,
        client_operation_id: String,
    ) -> Result<(ThreadRef, Vec<u8>), ProtocolError> {
        let _ = local_state;
        let replica = repo.native_thread(name).map_err(replica_err)?;
        let spool = self.resolve_spool_ref(repo_path).await?;
        let creator_authority = native_start_creator_authority(&replica, || async {
            self.current_creator_authority()
                .await
                .map(|(_, authority, _)| authority)
        })
        .await?;
        let creation = start_request_from_native_replica(
            &replica,
            &spool,
            client_operation_id,
            creator_authority.clone(),
        )?;
        let remote = self.native().await.map_err(native_error)?;
        let started = remote
            .api
            .call::<rpc::ThreadServiceStartThread>(creation.request())
            .await
            .map_err(native_client_error)?;
        let reference = started
            .thread
            .as_ref()
            .and_then(|overview| overview.r#ref.clone())
            .ok_or_else(|| ProtocolError::InvalidState("StartThread returned no Thread".into()))?;
        let hosted_id = overview_thread_id_from_ref(&reference)?;
        if hosted_id != replica.thread_id() {
            return Err(ProtocolError::InvalidState(
                "hosted Thread identity differs from local Thread".into(),
            ));
        }
        if ThreadReplica::open(repo.heddle_dir(), hosted_id)
            .map_err(replica_err)?
            .thread_id()
            != replica.thread_id()
        {
            return Err(ProtocolError::InvalidState(
                "local replica of the hosted Thread is missing; StartThread must retain genesis"
                    .into(),
            ));
        }
        Ok((reference, creator_authority))
    }

    async fn ensure_hosted_thread(
        &self,
        repo: &Repository,
        repo_path: &str,
        name: &str,
        local_state: StateId,
        client_operation_id: String,
    ) -> Result<(ThreadRef, Vec<u8>), ProtocolError> {
        let native = repo.native_thread(name).map_err(replica_err)?;
        if let Some(overview) = self
            .observe_thread_overviews(repo_path)
            .await?
            .into_iter()
            .find(|overview| overview.name == name)
            && let Some(reference) = overview.r#ref
        {
            let hosted_id = overview_thread_id_from_ref(&reference)?;
            if hosted_id == native.thread_id() {
                let creator_authority = native_start_creator_authority(&native, || async {
                    self.current_creator_authority()
                        .await
                        .map(|(_, authority, _)| authority)
                })
                .await?;
                return Ok((reference, creator_authority));
            }
            // A previously minted parallel replica is migration-only. New
            // publishes must not create one.
            if ThreadReplica::open(repo.heddle_dir(), hosted_id).is_ok() {
                let (_, creator_authority, _) = self.current_creator_authority().await?;
                return Ok((reference, creator_authority));
            }
            return Err(ProtocolError::InvalidState(
                "hosted Thread identity differs from local Thread".into(),
            ));
        }
        self.start_hosted_thread(repo, repo_path, name, local_state, client_operation_id)
            .await
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
        let _ = force;
        let started = Instant::now();
        let start_set = hosted_start_replicas(repo, target_thread)?;
        let mut receipt = None;
        for name in &start_set.names {
            let state = named_replica_state(repo, name, target_thread, local_state)?;
            let start_id = operation_id(START, String::new());
            let (reference, creator_authority) = self
                .ensure_hosted_thread(repo, repo_path, name, state, start_id)
                .await?;
            let publish_child_source =
                name != target_thread && replica_has_admitted_source(repo, name, state)?;
            if name != target_thread
                && !start_set.ancestors_of_target.contains(name)
                && !publish_child_source
            {
                continue;
            }
            let publish_id = if name == target_thread {
                operation_id(PUBLISH, client_operation_id.clone())
            } else {
                operation_id(PUBLISH, String::new())
            };
            let published = self
                .publish_local_state(
                    repo,
                    &reference,
                    name,
                    state,
                    &creator_authority,
                    publish_id,
                )
                .await?;
            if name == target_thread {
                receipt = Some(published);
            }
        }
        let receipt = receipt.ok_or_else(|| {
            ProtocolError::InvalidState(format!(
                "hosted push did not publish local Thread {target_thread:?}"
            ))
        })?;
        let new_state = state_from_revision(receipt.revision.as_ref());
        Ok((
            PushComplete {
                success: matches!(
                    receipt.outcome,
                    Some(contract::publication_receipt::Outcome::Accepted(_))
                ),
                new_state,
                error: None,
                transfer_id: String::new(),
                transport_mode: String::new(),
                resume_offset: 0,
                chunk_index: 0,
                checkpoint: Vec::new(),
                is_complete: true,
            },
            PushProfile {
                total: started.elapsed(),
                ..Default::default()
            },
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn push_with_expected_head_profiled(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        local_state: StateId,
        target_thread: &str,
        force: bool,
        _expected_remote_head: super::sync::ExpectedRemoteHead,
        client_operation_id: String,
    ) -> Result<(PushComplete, PushProfile), ProtocolError> {
        self.push_profiled(
            repo,
            repo_path,
            local_state,
            target_thread,
            force,
            client_operation_id,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn push_git_overlay_mirror(
        &mut self,
        _repo: &Repository,
        _repo_path: &str,
        _local_state: StateId,
        _target_thread: &str,
        _force: bool,
        _progress: &objects::Progress,
        _client_operation_id: String,
    ) -> Result<PushComplete, ProtocolError> {
        Err(ProtocolError::InvalidState(
            "hosted git-overlay push is not a v2 Weft method; capture natively or push through a Git remote".into(),
        ))
    }

    async fn publish_local_state(
        &self,
        repo: &Repository,
        reference: &ThreadRef,
        thread_name: &str,
        local_state: StateId,
        creator_authority: &[u8],
        client_operation_id: String,
    ) -> Result<contract::PublicationReceipt, ProtocolError> {
        let thread = overview_thread_id_from_ref(reference)?;
        let replica = match ThreadReplica::open(repo.heddle_dir(), thread) {
            Ok(replica) => replica,
            Err(_) => {
                return Err(ProtocolError::InvalidState(
                    "local replica of the hosted Thread is missing; StartThread must retain genesis"
                        .into(),
                ));
            }
        };
        replica.bind_local_name(thread_name).map_err(replica_err)?;
        let native = repo.native_thread(thread_name).map_err(replica_err)?;
        let split_replica = replica.thread_id() != native.thread_id();
        if replica
            .source_operation_page(local_state, None, 1)
            .map_err(replica_err)?
            .is_empty()
        {
            if !split_replica {
                return Err(ProtocolError::InvalidState(
                    "capture was not admitted on the local Thread".into(),
                ));
            }
            let (owner_id, _, agent_id) = self.current_creator_authority().await?;
            let spool = Uuid::parse_str(
                &reference
                    .spool
                    .as_ref()
                    .ok_or_else(|| ProtocolError::InvalidState("spool required".into()))?
                    .id,
            )
            .map_err(native_error)?;
            let signer = self
                .claim_proof_signer()
                .ok_or(super::HostedError::SigningIdentityRequired)
                .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
            admit_hosted_source_ancestry(
                repo,
                &replica,
                local_state,
                spool,
                owner_id,
                creator_authority,
                signer,
                agent_id,
            )?;
        }
        let operation = replica
            .source_operation_page(local_state, None, 1)
            .map_err(replica_err)?
            .first()
            .copied()
            .ok_or_else(|| {
                ProtocolError::InvalidState("capture was not admitted on the hosted Thread".into())
            })?;
        let stored = replica
            .source_ancestry(operation, ANCESTRY_RECORDS, ANCESTRY_BYTES)
            .map_err(replica_err)?;
        let state = repo
            .store()
            .get_state(&local_state)?
            .ok_or_else(|| ProtocolError::ObjectNotFound(local_state.to_string_full()))?;
        let genesis = replica.genesis().map_err(replica_err)?;
        let mut proofs = Vec::new();
        for item in &stored {
            let native = item
                .original
                .verify()
                .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
            if let Some(proof) = native
                .reference_proof(&genesis)
                .map_err(|error| ProtocolError::InvalidState(error.to_string()))?
            {
                proofs.push(proof);
            }
        }
        let scratch = repo.heddle_dir().join("source-transfers");
        objects::fs_atomic::create_private_dir_all(&scratch)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let pack = SourcePack::prepare_with_references(
            repo.store(),
            &state,
            &proofs,
            &scratch,
            SourceBudget {
                max_objects: SOURCE_OBJECTS,
                max_decoded_bytes: SOURCE_BYTES,
            },
        )
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let mut operations = Vec::new();
        let mut authority_admissions = Vec::new();
        let mut boundary_acceptances = Vec::new();
        for item in &stored {
            operations.push(signed_record(&item.original)?);
            if let Some(receipt) = &item.authority_admission {
                authority_admissions.push(
                    thread_api::authority_admission::encode(receipt)
                        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?,
                );
                boundary_acceptances.extend(
                    thread_api::boundary_acceptance::authority_evidence(Some(receipt))
                        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?,
                );
            }
        }
        let originals = PublicationOriginals {
            geneses: vec![replica.genesis_record().map_err(replica_err)?],
            operations: vec![contract::ReplicationOperations {
                boundary_acceptances,
                operations,
                authority_admissions,
            }],
        };
        let remote = self.native().await.map_err(native_error)?;
        let source = EndpointRef {
            kind: EndpointKind::Device as i32,
            public_key: self.connection.endpoint_id().as_bytes().to_vec(),
        };
        remote
            .thread(reference.clone())
            .publish_source(
                &pack,
                &originals,
                PublicationOptions {
                    client_operation_id,
                    source,
                    sharing_policy_version: Vec::new(),
                    checkpoint: None,
                },
            )
            .await
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))
    }

    pub async fn pull_profiled(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        local_thread: Option<&str>,
    ) -> Result<(PullComplete, PullProfile), ProtocolError> {
        let started = Instant::now();
        let complete = self
            .fetch_hosted_thread(repo, repo_path, remote_thread, local_thread, None)
            .await?;
        Ok((
            complete,
            PullProfile {
                receive_and_apply: started.elapsed(),
                ..Default::default()
            },
        ))
    }

    pub async fn pull_with_depth_and_materialization(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        local_thread: Option<&str>,
        _depth: Option<u32>,
        _materialization: PullMaterialization,
    ) -> Result<PullComplete, ProtocolError> {
        self.fetch_hosted_thread(repo, repo_path, remote_thread, local_thread, None)
            .await
    }

    pub async fn repair_clone_with_depth_and_materialization(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        _depth: Option<u32>,
        _materialization: PullMaterialization,
    ) -> Result<PullComplete, ProtocolError> {
        self.fetch_hosted_thread(repo, repo_path, remote_thread, None, None)
            .await
    }

    pub async fn clone_pull_with_depth_and_materialization<F>(
        &mut self,
        repo_path: &str,
        requested_thread: Option<&str>,
        _depth: Option<u32>,
        _materialization: PullMaterialization,
        initialize: F,
    ) -> Result<(PullComplete, Repository), ProtocolError>
    where
        F: FnOnce(&PullBootstrapRefs) -> Result<Repository, ProtocolError>,
    {
        let advertised = self.advertised_pull_refs(repo_path).await?;
        if advertised.refs.is_empty() {
            return Err(ProtocolError::InvalidState(
                "Fetch requires a started Thread with published source; this spool has none".into(),
            ));
        }
        let track = requested_thread
            .map(str::to_string)
            .or_else(|| advertised.head_thread.clone())
            .or_else(|| advertised.refs.first().map(|entry| entry.name.clone()))
            .ok_or_else(|| {
                ProtocolError::InvalidState("server does not advertise clone refs".into())
            })?;
        if !advertised.refs.iter().any(|entry| entry.name == track) {
            return Err(ProtocolError::ObjectNotFound(format!(
                "Thread '{track}' is not published on this spool"
            )));
        }
        let repo = initialize(&advertised)?;
        let complete = self
            .fetch_hosted_thread(&repo, repo_path, &track, Some(&track), None)
            .await?;
        for entry in &advertised.refs {
            if !entry.is_user_thread() || entry.name == track {
                continue;
            }
            self.fetch_hosted_thread(&repo, repo_path, &entry.name, Some(&entry.name), None)
                .await?;
        }
        Ok((complete, repo))
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
        self.fetch_hosted_thread(repo, repo_path, remote_thread, None, Some(target_state))
            .await
            .map(|complete| usize::from(complete.success))
    }

    pub async fn hydrate_missing_blobs_for_state(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        target_state: StateId,
    ) -> Result<usize, ProtocolError> {
        self.fetch_state(repo, repo_path, remote_thread, target_state)
            .await
    }

    async fn fetch_hosted_thread(
        &self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        local_thread: Option<&str>,
        target_state: Option<StateId>,
    ) -> Result<PullComplete, ProtocolError> {
        let spool = self.resolve_spool_ref(repo_path).await?;
        let overview = self
            .observe_thread_overviews(repo_path)
            .await?
            .into_iter()
            .find(|overview| overview.name == remote_thread)
            .ok_or_else(|| {
                ProtocolError::ObjectNotFound(format!(
                    "Thread '{remote_thread}' is not published on this spool"
                ))
            })?;
        let reference = overview
            .r#ref
            .clone()
            .ok_or_else(|| ProtocolError::InvalidState("observed Thread has no identity".into()))?;
        let revision = match target_state {
            Some(state) => Some(revision_ref(&spool, state)),
            None => overview.source_heads.into_iter().next(),
        };
        if revision.is_none() && target_state.is_none() {
            return Err(ProtocolError::InvalidState(
                "Fetch requires a started Thread with a published source revision".into(),
            ));
        }
        // ProviderDialRoute is a client hint on FetchOpen. Weft matches those
        // hints against configured shards; DescribeEndpoint and ThreadOverview
        // currently do not advertise them, so clone stays on direct Fetch until
        // a caller supplies usable routes here.
        let open = preferred_fetch_open(
            FetchOpen {
                thread: Some(reference.clone()),
                revision: revision.clone(),
                selection: Some(TransferSelection {
                    facets: vec![contract::SharedFacet::Source as i32],
                    ..Default::default()
                }),
                ..Default::default()
            },
            Vec::new(),
        );
        let scratch = repo.heddle_dir().join("source-transfers");
        objects::fs_atomic::create_private_dir_all(&scratch)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let staged = self
            .fetch_native_source(open, thread_api::fetch::Limits::default(), &scratch)
            .await
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let now = chrono::Utc::now().timestamp();
        let final_state = staged
            .install(repo, now)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let seed = synthetic_initial_base().map_err(native_error)?;
        repo.store().put_tree(&objects::object::Tree::new())?;
        repo.store().put_state(&seed)?;
        let track = local_thread.unwrap_or(remote_thread);
        repo.set_thread_recorded(&ThreadName::new(track), &final_state)?;
        if let Ok(id) = overview_thread_id_from_ref(&reference) {
            if let Ok(replica) = ThreadReplica::open(repo.heddle_dir(), id) {
                let _ = replica.bind_local_name(track);
            }
            persist_advertised_thread_identity(
                repo,
                &[HostedRefEntry::from_advertised(
                    track.to_string(),
                    final_state,
                    true,
                    repo::RevisionAddress::heddle(final_state).to_string(),
                    Some(hex::encode(id.as_bytes())),
                )],
                track,
                &final_state,
            )
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        }
        Ok(PullComplete {
            success: true,
            final_state: Some(final_state),
            error: None,
            transfer_id: String::new(),
            transport_mode: String::new(),
            resume_offset: 0,
            chunk_index: 0,
            checkpoint: encode_empty_pull_bootstrap(final_state)?,
            is_complete: true,
        })
    }

    pub async fn get_thread_metadata(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        pulled_state: StateId,
    ) -> Result<SyncedThreadMetadata, ProtocolError> {
        self.try_get_thread_metadata(repo, repo_path, remote_thread, pulled_state)
            .await?
            .ok_or_else(|| {
                ProtocolError::InvalidState(format!(
                    "hosted thread '{remote_thread}' has no managed metadata; no local thread was created"
                ))
            })
    }

    pub async fn try_get_thread_metadata(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        pulled_state: StateId,
    ) -> Result<Option<SyncedThreadMetadata>, ProtocolError> {
        let thread_id = match self.require_thread_id(repo_path, remote_thread).await {
            Ok(thread_id) => thread_id,
            Err(ProtocolError::ObjectNotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        match ThreadManager::new(repo.heddle_dir()).adopt_stable_identity_for_thread(
            repo,
            remote_thread,
            &thread_id,
            pulled_state,
        ) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(_) => Ok(ThreadManager::new(repo.heddle_dir())
                .find_synced_record_by_thread(repo, remote_thread, Some(pulled_state))
                .ok()
                .flatten()),
        }
    }

    pub async fn publish_clone_markers(
        &mut self,
        repo: &Repository,
        _repo_path: &str,
        checkpoint: &[u8],
        _depth: Option<u32>,
        _materialization: PullMaterialization,
    ) -> Result<(), ProtocolError> {
        let _ = (repo, checkpoint);
        Ok(())
    }

    pub async fn fetch_advertised_synthetic_frontier_objects(
        &mut self,
        _repo: &Repository,
        _repo_path: &str,
        _advertised: &[HostedRefEntry],
        _depth: Option<u32>,
        _materialization: PullMaterialization,
    ) -> Result<(), ProtocolError> {
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update_ref(
        &mut self,
        _repo_path: &str,
        _name: &str,
        _is_thread: bool,
        _old_value: Option<StateId>,
        _new_value: StateId,
        _force: bool,
        _thread_id: Option<String>,
        _client_operation_id: String,
    ) -> Result<wire::RefUpdated, ProtocolError> {
        Err(ProtocolError::InvalidState(
            "v2 Weft has no UpdateRef; source publication is SyncService/PublishContent".into(),
        ))
    }
}

struct HostedStartSet {
    names: Vec<String>,
    ancestors_of_target: std::collections::BTreeSet<String>,
}

/// Local named replicas on the target Thread's native spool, parents first.
/// Always includes the current Thread and any Thread whose `genesis.parent`
/// is the current Thread.
fn hosted_start_replicas(
    repo: &Repository,
    target_thread: &str,
) -> Result<HostedStartSet, ProtocolError> {
    let target = repo.native_thread(target_thread).map_err(replica_err)?;
    let spool = target.genesis().map_err(replica_err)?.spool;
    let mut selected = Vec::new();
    for (name, replica) in repo.list_native_threads().map_err(replica_err)? {
        let genesis = replica.genesis().map_err(replica_err)?;
        if genesis.spool != spool {
            continue;
        }
        selected.push((name, replica.thread_id(), genesis.parent));
    }
    if !selected.iter().any(|(name, _, _)| name == target_thread) {
        return Err(ProtocolError::InvalidState(format!(
            "Thread {target_thread:?} has no native identity"
        )));
    }
    let mut ancestors_of_target = std::collections::BTreeSet::new();
    let mut parent = selected
        .iter()
        .find(|(name, _, _)| name == target_thread)
        .and_then(|(_, _, parent)| *parent);
    let mut guard = 0usize;
    while let Some(id) = parent {
        guard += 1;
        if guard > 128 {
            return Err(ProtocolError::InvalidState(
                "local Thread parent chain exceeds bound".into(),
            ));
        }
        let Some((name, _, next)) = selected.iter().find(|(_, thread, _)| *thread == id) else {
            break;
        };
        ancestors_of_target.insert(name.clone());
        parent = *next;
    }
    let names = parent_first_names(selected)?;
    if !names.iter().any(|name| name == target_thread) {
        return Err(ProtocolError::InvalidState(format!(
            "Thread {target_thread:?} has no native identity"
        )));
    }
    Ok(HostedStartSet {
        names,
        ancestors_of_target,
    })
}

fn parent_first_names(
    mut selected: Vec<(String, ContentHash, Option<ContentHash>)>,
) -> Result<Vec<String>, ProtocolError> {
    let mut ordered = Vec::with_capacity(selected.len());
    while !selected.is_empty() {
        let Some(index) = selected.iter().position(|(_, _, parent)| {
            parent.is_none_or(|parent| !selected.iter().any(|(_, other, _)| *other == parent))
        }) else {
            return Err(ProtocolError::InvalidState(
                "local Thread parent cycle".into(),
            ));
        };
        ordered.push(selected.remove(index).0);
    }
    Ok(ordered)
}

fn named_replica_state(
    repo: &Repository,
    name: &str,
    target_thread: &str,
    local_state: StateId,
) -> Result<StateId, ProtocolError> {
    if name == target_thread {
        return Ok(local_state);
    }
    if let Some(state) = repo
        .refs()
        .get_thread(&ThreadName::new(name))
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?
    {
        return Ok(state);
    }
    let replica = repo.native_thread(name).map_err(replica_err)?;
    let replica_id = replica.thread_id();
    for (_, child) in repo.list_native_threads().map_err(replica_err)? {
        let genesis = child.genesis().map_err(replica_err)?;
        if genesis.parent == Some(replica_id) {
            return Ok(genesis.base);
        }
    }
    replica
        .genesis()
        .map(|genesis| genesis.base)
        .map_err(replica_err)
}

fn replica_has_admitted_source(
    repo: &Repository,
    name: &str,
    state: StateId,
) -> Result<bool, ProtocolError> {
    Ok(!repo
        .native_thread(name)
        .map_err(replica_err)?
        .source_operation_page(state, None, 1)
        .map_err(replica_err)?
        .is_empty())
}

fn start_request_from_native_replica(
    replica: &ThreadReplica,
    spool: &SpoolRef,
    client_operation_id: String,
    creator_authority: Vec<u8>,
) -> Result<ThreadCreation, ProtocolError> {
    let signed = replica.signed_genesis().map_err(replica_err)?;
    let genesis = signed
        .verify()
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    if genesis.spool != spool.id {
        return Err(ProtocolError::InvalidState(
            "local Thread spool differs from hosted spool".into(),
        ));
    }
    let local_id = genesis
        .id()
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    if local_id != replica.thread_id() {
        return Err(ProtocolError::InvalidState(
            "stored genesis differs from local Thread identity".into(),
        ));
    }
    let record = replica
        .genesis_record()
        .map_err(replica_err)?
        .genesis
        .ok_or_else(|| {
            ProtocolError::InvalidState("local Thread genesis record is missing".into())
        })?;
    let creation =
        ThreadCreation::from_signed_with_authority(client_operation_id, record, creator_authority)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    let submitted =
        creation.request().thread_genesis.as_ref().ok_or_else(|| {
            ProtocolError::InvalidState("StartThread request has no genesis".into())
        })?;
    let submitted = ThreadGenesis::decode(&submitted.canonical_record)
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    if submitted.parent != genesis.parent {
        return Err(ProtocolError::InvalidState(
            "StartThread would drop parent".into(),
        ));
    }
    if submitted.nonce != genesis.nonce
        || submitted.base != genesis.base
        || submitted.name != genesis.name
        || submitted.spool != genesis.spool
    {
        return Err(ProtocolError::InvalidState(
            "StartThread must submit the existing local genesis".into(),
        ));
    }
    let hosted_id = submitted
        .id()
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    if hosted_id != replica.thread_id() {
        return Err(ProtocolError::InvalidState(
            "hosted Thread identity differs from local Thread".into(),
        ));
    }
    Ok(creation)
}

async fn native_start_creator_authority<F, Fut>(
    replica: &ThreadReplica,
    account_authority: F,
) -> Result<Vec<u8>, ProtocolError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, ProtocolError>>,
{
    match replica.genesis().map_err(replica_err)?.owner {
        GenesisOwner::LocalKey(_) => Ok(Vec::new()),
        GenesisOwner::Account(_) => account_authority().await,
    }
}

fn overview_thread_id_from_ref(reference: &ThreadRef) -> Result<ContentHash, ProtocolError> {
    let value = reference
        .id
        .as_ref()
        .ok_or_else(|| ProtocolError::InvalidState("Thread identity absent".into()))?;
    let bytes: [u8; 32] = value
        .value
        .as_slice()
        .try_into()
        .map_err(|_| ProtocolError::InvalidState("Thread identity length".into()))?;
    Ok(ContentHash::from_bytes(bytes))
}

/// Flatten local source ancestry onto a hosted Thread as Captures, parents first.
/// After `land`, both parents become Captures on this Thread so a Capture of the
/// merge satisfies `validate_parents`. Do not mint LocalIntegration here: that
/// receipt is bound to the local Thread id.
#[allow(clippy::too_many_arguments)]
fn admit_hosted_source_ancestry(
    repo: &Repository,
    replica: &ThreadReplica,
    local_state: StateId,
    spool: Uuid,
    owner_id: Uuid,
    creator_authority: &[u8],
    signer: &crypto::Ed25519Signer,
    agent_id: Option<String>,
) -> Result<(), ProtocolError> {
    let base = replica.genesis().map_err(replica_err)?.base;
    let mut stack = vec![local_state];
    let mut on_path = std::collections::BTreeSet::new();
    let mut expanded = std::collections::BTreeSet::new();
    let mut remaining = SOURCE_OBJECTS;
    while let Some(state_id) = stack.last().copied() {
        if state_id == base
            || !replica
                .source_operation_page(state_id, None, 1)
                .map_err(replica_err)?
                .is_empty()
        {
            stack.pop();
            on_path.remove(&state_id);
            continue;
        }
        if expanded.contains(&state_id) {
            stack.pop();
            on_path.remove(&state_id);
            record_hosted_capture(
                repo,
                replica,
                state_id,
                spool,
                owner_id,
                creator_authority,
                signer,
                agent_id.clone(),
            )?;
            continue;
        }
        if !on_path.insert(state_id) {
            return Err(ProtocolError::InvalidState(
                "hosted source ancestry is cyclic".into(),
            ));
        }
        if remaining == 0 {
            return Err(ProtocolError::InvalidState(
                "hosted source ancestry exceeds object budget".into(),
            ));
        }
        remaining -= 1;
        expanded.insert(state_id);
        let state = repo
            .store()
            .get_state(&state_id)?
            .ok_or_else(|| ProtocolError::ObjectNotFound(state_id.to_string_full()))?;
        for parent in &state.parents {
            if *parent != base && on_path.contains(parent) {
                return Err(ProtocolError::InvalidState(
                    "hosted source ancestry is cyclic".into(),
                ));
            }
            stack.push(*parent);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_hosted_capture(
    repo: &Repository,
    replica: &ThreadReplica,
    state_id: StateId,
    spool: Uuid,
    owner_id: Uuid,
    creator_authority: &[u8],
    signer: &crypto::Ed25519Signer,
    agent_id: Option<String>,
) -> Result<(), ProtocolError> {
    if !replica
        .source_operation_page(state_id, None, 1)
        .map_err(replica_err)?
        .is_empty()
    {
        return Ok(());
    }
    let state = repo
        .store()
        .get_state(&state_id)?
        .ok_or_else(|| ProtocolError::ObjectNotFound(state_id.to_string_full()))?;
    let mut parents = std::collections::BTreeSet::new();
    for parent in &state.parents {
        parents.extend(
            replica
                .source_operation_page(*parent, None, 1024)
                .map_err(replica_err)?,
        );
    }
    let capture = replica
        .prepare_capture(repo, &state)
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    let operation = ThreadOperation {
        version: 1,
        thread: replica.thread_id(),
        parents,
        publisher: signer
            .public_key()
            .try_into()
            .map_err(|_| ProtocolError::InvalidState("invalid publisher key".into()))?,
        body: ThreadOperationBody::Capture(
            AuthoredCapture::account(
                capture,
                spool,
                CollaborationActor {
                    principal_id: owner_id,
                    agent_id,
                },
                creator_authority.to_vec(),
            )
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?,
        ),
    };
    let signed = SignedOperation::sign(&operation, signer)
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
    match replica
        .receive_prepared_source(&signed, repo.store(), |_| Ok(()))
        .map_err(|error| ProtocolError::InvalidState(error.to_string()))?
    {
        objects::object::thread_replication::Admission::Accepted => Ok(()),
        other => Err(ProtocolError::InvalidState(format!(
            "hosted capture was not admitted: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use crypto::{Ed25519Signer, Signer, thread_operation::SignedGenesis};
    use objects::{
        object::{
            Attribution, Principal, State, StateId, Tree,
            thread_replication::{
                GenesisOwner, ThreadGenesis, hosted_import::synthetic_initial_base,
            },
        },
        store::ObjectStore,
    };
    use repo::{Repository, thread_replication::ThreadReplica};
    use uuid::Uuid;

    use super::*;

    fn hosted_like_replica() -> (
        tempfile::TempDir,
        Repository,
        ThreadReplica,
        Ed25519Signer,
        Uuid,
        Uuid,
        Vec<u8>,
    ) {
        let dir = tempfile::tempdir().expect("workspace");
        let repo = Repository::init(dir.path()).expect("repo");
        let signer = Ed25519Signer::from_seed(&[71; 32]).expect("signer");
        let spool = Uuid::from_u128(1);
        let owner = Uuid::from_u128(2);
        let authority = vec![7; 32];
        let base = synthetic_initial_base().expect("seed");
        repo.store().put_tree(&Tree::new()).expect("empty tree");
        repo.store().put_state(&base).expect("seed state");
        let genesis = ThreadGenesis {
            version: 1,
            spool: spool.to_string(),
            parent: None,
            base: base.id(),
            name: "main".into(),
            intent: String::new(),
            creator: signer.public_key().try_into().expect("key"),
            owner: GenesisOwner::Account(owner),
            nonce: vec![9; 16],
        };
        let replica = ThreadReplica::create_from_original_authority(
            repo.heddle_dir(),
            &SignedGenesis::sign(&genesis, &signer).expect("genesis"),
            &authority,
        )
        .expect("hosted replica");
        (dir, repo, replica, signer, spool, owner, authority)
    }

    fn snapshot(repo: &Repository, parents: Vec<StateId>, intent: &str) -> State {
        let state = State::new_snapshot(
            Tree::new().hash(),
            parents,
            Attribution::human(Principal::new("dev", "dev@example.test")),
        )
        .with_intent(intent);
        repo.store().put_state(&state).expect("state");
        state
    }

    #[test]
    fn land_shaped_tip_requires_hosted_source_ancestry() {
        let (_dir, repo, replica, signer, spool, owner, authority) = hosted_like_replica();
        let base = replica.genesis().expect("genesis").base;
        let main_tip = snapshot(&repo, vec![base], "main");
        let feature_tip = snapshot(&repo, vec![main_tip.id()], "feature");
        let merge = snapshot(&repo, vec![main_tip.id(), feature_tip.id()], "land");

        let tip_only = record_hosted_capture(
            &repo,
            &replica,
            merge.id(),
            spool,
            owner,
            &authority,
            &signer,
            None,
        )
        .err()
        .unwrap_or_else(|| panic!("merge capture without parents"));
        let tip_only = tip_only.to_string();
        assert!(
            tip_only.contains("Rejected")
                && tip_only.contains("capture source ancestry differs from causal parents"),
            "admission must surface, got {tip_only}"
        );
        assert!(
            !tip_only.contains("prepared source did not settle"),
            "opaque settle error: {tip_only}"
        );
        assert!(
            replica
                .source_operation_page(merge.id(), None, 1)
                .expect("page")
                .is_empty()
        );

        admit_hosted_source_ancestry(
            &repo,
            &replica,
            merge.id(),
            spool,
            owner,
            &authority,
            &signer,
            None,
        )
        .expect("ancestry");
        for id in [main_tip.id(), feature_tip.id(), merge.id()] {
            assert_eq!(
                replica
                    .source_operation_page(id, None, 1)
                    .expect("admitted")
                    .len(),
                1
            );
        }
    }

    static HOME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct IsolatedNative {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
        _home: tempfile::TempDir,
        _dir: tempfile::TempDir,
        repo: Repository,
    }

    impl Drop for IsolatedNative {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe { std::env::set_var("HEDDLE_HOME", value) },
                None => unsafe { std::env::remove_var("HEDDLE_HOME") },
            }
        }
    }

    fn native_repo() -> IsolatedNative {
        let guard = HOME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("HEDDLE_HOME");
        let home = tempfile::tempdir().expect("heddle home");
        unsafe {
            std::env::set_var("HEDDLE_HOME", home.path());
        }
        let dir = tempfile::tempdir().expect("workspace");
        let repo = Repository::init_default(dir.path()).expect("native init");
        repo.seed_default_thread().expect("main");
        IsolatedNative {
            _guard: guard,
            previous,
            _home: home,
            _dir: dir,
            repo,
        }
    }

    #[test]
    fn start_thread_submits_existing_native_genesis_without_inventing_identity() {
        let isolated = native_repo();
        let repo = &isolated.repo;
        let main = repo.native_thread("main").expect("main");
        let genesis = main.genesis().expect("genesis");
        let nonce = genesis.nonce.clone();
        let spool = SpoolRef {
            id: genesis.spool.clone(),
        };
        let creation = start_request_from_native_replica(
            &main,
            &spool,
            Uuid::now_v7().to_string(),
            Vec::new(),
        )
        .expect("start request");
        let request = creation.request();
        assert!(
            request.creator_authority.is_empty(),
            "LocalKey genesis must not carry an account proof"
        );
        let submitted = ThreadGenesis::decode(
            &request
                .thread_genesis
                .as_ref()
                .expect("genesis")
                .canonical_record,
        )
        .expect("decode");
        assert_eq!(
            submitted.nonce, nonce,
            "StartThread must not invent a nonce"
        );
        assert_eq!(submitted.parent, genesis.parent);
        assert_eq!(submitted.base, genesis.base);
        assert_eq!(submitted.name, genesis.name);
        assert_eq!(submitted.spool, genesis.spool);
        assert_eq!(submitted.owner, genesis.owner);
        assert_eq!(
            submitted.id().expect("id"),
            main.thread_id(),
            "hosted id must equal local Thread id"
        );
        assert_eq!(
            creation.reference().id.as_ref().expect("thread id").value,
            main.thread_id().as_bytes()
        );
    }

    #[test]
    fn start_thread_preserves_child_parent_and_rejects_dropping_it() {
        let isolated = native_repo();
        let repo = &isolated.repo;
        let main = repo.native_thread("main").expect("main");
        let base = repo.head().expect("head").expect("base");
        let child = repo
            .create_native_thread("feature", base, Some("main"), "edit docs")
            .expect("feature");
        let genesis = child.genesis().expect("genesis");
        assert_eq!(genesis.parent, Some(main.thread_id()));
        let spool = SpoolRef {
            id: genesis.spool.clone(),
        };
        let creation = start_request_from_native_replica(
            &child,
            &spool,
            Uuid::now_v7().to_string(),
            Vec::new(),
        )
        .expect("child start");
        let submitted = ThreadGenesis::decode(
            &creation
                .request()
                .thread_genesis
                .as_ref()
                .expect("genesis")
                .canonical_record,
        )
        .expect("decode");
        assert_eq!(submitted.parent, Some(main.thread_id()));
        assert_eq!(submitted.nonce, genesis.nonce);
        assert_eq!(submitted.id().expect("id"), child.thread_id());
        let mismatch = SpoolRef {
            id: Uuid::from_u128(99).to_string(),
        };
        let error = start_request_from_native_replica(
            &child,
            &mismatch,
            Uuid::now_v7().to_string(),
            Vec::new(),
        )
        .err()
        .unwrap_or_else(|| panic!("spool mismatch"));
        assert!(
            error.to_string().contains("spool"),
            "fail closed on spool rewrite: {error}"
        );
    }

    #[test]
    fn start_thread_rejects_account_authority_on_local_key_genesis() {
        let isolated = native_repo();
        let repo = &isolated.repo;
        let main = repo.native_thread("main").expect("main");
        let spool = SpoolRef {
            id: main.genesis().expect("genesis").spool,
        };
        let error = start_request_from_native_replica(
            &main,
            &spool,
            Uuid::now_v7().to_string(),
            vec![7; 32],
        )
        .err()
        .unwrap_or_else(|| panic!("account proof on LocalKey"));
        assert!(
            error.to_string().contains("local-key") || error.to_string().contains("explicit claim"),
            "LocalKey StartThread must not rewrite owner to Account: {error}"
        );
    }

    #[test]
    fn hosted_push_starts_same_spool_child_after_parent() {
        let isolated = native_repo();
        let repo = &isolated.repo;
        let main = repo.native_thread("main").expect("main");
        let base = repo.head().expect("head").expect("base");
        let child = repo
            .create_native_thread("feature", base, Some("main"), "edit docs")
            .expect("feature");
        assert_eq!(
            child.genesis().expect("genesis").parent,
            Some(main.thread_id())
        );
        let from_main = hosted_start_replicas(repo, "main").expect("from main");
        assert_eq!(
            from_main
                .names
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["main", "feature"]
        );
        assert!(from_main.ancestors_of_target.is_empty());
        let from_feature = hosted_start_replicas(repo, "feature").expect("from feature");
        assert_eq!(
            from_feature
                .names
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["main", "feature"]
        );
        assert_eq!(
            from_feature
                .ancestors_of_target
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["main"]
        );
        let listed = repo.list_native_threads().expect("named replicas");
        assert_eq!(listed.len(), 2);
        assert!(
            listed.iter().any(
                |(name, replica)| name == "feature" && replica.thread_id() == child.thread_id()
            )
        );
    }
}
