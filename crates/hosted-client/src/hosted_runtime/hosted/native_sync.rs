// SPDX-License-Identifier: Apache-2.0
//! Hosted push/pull over v2 `PublishContent` and `Fetch`.
//!
//! Request and pack construction matches local device publication/fetch:
//! `SourcePack::prepare_with_references` plus `Remote::publish_content` /
//! `Remote::fetch_content`. No v1 RepoSyncService frames.
use std::time::Instant;

use api::heddle::api::{
    v1alpha1::{PullReady, StateId as ApiStateId},
    v2alpha1::{
        self as contract, EndpointKind, EndpointRef, FetchOpen, ObservationMode,
        ObserveIdentityRequest, ObserveOptions, ObserveThreadsRequest, RevisionRef, SpoolRef,
        StartThreadRequest, ThreadId, ThreadOverview, ThreadQuery, ThreadRef, TransferSelection,
        identity_event, revision_ref, thread_list_event, thread_query,
    },
};
use crypto::{
    Signer as _,
    thread_operation::{SignedGenesis, SignedOperation},
};
use objects::{
    object::{
        CollaborationActor, ContentHash, StateId, ThreadName,
        thread_replication::{
            AuthoredCapture, OPERATION_FORMAT, ThreadGenesis, ThreadOperation, ThreadOperationBody,
            hosted_import::synthetic_initial_base,
        },
    },
    store::ObjectStore,
};
use repo::{Repository, SyncedThreadMetadata, ThreadManager, thread_replication::ThreadReplica};
use thread_api::{
    publication::{PublicationOptions, PublicationOriginals, SourceBudget, SourcePack},
    rpc,
};
use uuid::Uuid;
use wire::{ProtocolError, PullComplete, PushComplete, RefEntry};

use super::{
    HostedClient, HostedRefEntry, PullBootstrapRefs, PullMaterialization,
    helpers::native_client_error,
    persist_advertised_thread_identity,
    sync::{PullProfile, PushProfile},
};

const PUBLISH: &str = "heddle.api.v2alpha1.SyncService/PublishContent";
const START: &str = "heddle.api.v2alpha1.ThreadService/StartThread";
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

    async fn current_creator_authority(
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
        let spool = self.resolve_spool_ref(repo_path).await?;
        let signer = self
            .claim_proof_signer()
            .ok_or(super::HostedError::SigningIdentityRequired)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let creator = signer
            .public_key()
            .try_into()
            .map_err(|_| ProtocolError::InvalidState("invalid publisher key".into()))?;
        let (owner_id, creator_authority, _) = self.current_creator_authority().await?;
        let _ = local_state;
        let base_state = synthetic_initial_base().map_err(native_error)?;
        repo.store().put_state(&base_state)?;
        let base = base_state.id();
        let genesis = ThreadGenesis {
            owner: objects::object::thread_replication::GenesisOwner::Account(owner_id),
            version: 1,
            spool: spool.id.clone(),
            parent: None,
            base,
            name: name.into(),
            intent: String::new(),
            creator,
            nonce: Uuid::now_v7().as_bytes().to_vec(),
        };
        let signed = thread_api::replication::opening::sign_genesis(&genesis, signer)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let remote = self.native().await.map_err(native_error)?;
        let started = remote
            .api
            .call::<rpc::ThreadServiceStartThread>(&StartThreadRequest {
                creator_authority: creator_authority.clone(),
                client_operation_id,
                spool: Some(spool.clone()),
                thread_genesis: Some(signed.clone()),
            })
            .await
            .map_err(native_client_error)?;
        let reference = started
            .thread
            .as_ref()
            .and_then(|overview| overview.r#ref.clone())
            .ok_or_else(|| ProtocolError::InvalidState("StartThread returned no Thread".into()))?;
        let original = SignedGenesis {
            canonical: signed.canonical_record,
            signature: signed
                .signatures
                .first()
                .map(|signature| signature.signature.clone())
                .ok_or_else(|| ProtocolError::InvalidState("signed genesis missing".into()))?,
        };
        let replica = ThreadReplica::create_from_original_authority(
            repo.heddle_dir(),
            &original,
            &creator_authority,
        )
        .map_err(replica_err)?;
        replica.bind_local_name(name).map_err(replica_err)?;
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
        if let Some(overview) = self
            .observe_thread_overviews(repo_path)
            .await?
            .into_iter()
            .find(|overview| overview.name == name)
            && let Some(reference) = overview.r#ref
        {
            let (_, creator_authority, _) = self.current_creator_authority().await?;
            if let Ok(id) = overview_thread_id_from_ref(&reference)
                && let Ok(replica) = ThreadReplica::open(repo.heddle_dir(), id)
            {
                let _ = replica.bind_local_name(name);
            }
            return Ok((reference, creator_authority));
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
        let publish_id = operation_id(PUBLISH, client_operation_id.clone());
        let start_id = operation_id(START, String::new());
        let (reference, creator_authority) = self
            .ensure_hosted_thread(repo, repo_path, target_thread, local_state, start_id)
            .await?;
        let receipt = self
            .publish_local_state(
                repo,
                &reference,
                target_thread,
                local_state,
                &creator_authority,
                publish_id,
            )
            .await?;
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
        if replica
            .source_operation_page(local_state, None, 1)
            .map_err(replica_err)?
            .is_empty()
        {
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
            record_hosted_capture(
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
        F: FnOnce(&PullReady, &PullBootstrapRefs) -> Result<Repository, ProtocolError>,
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
        let repo = initialize(&PullReady::default(), &advertised)?;
        let complete = self
            .fetch_hosted_thread(&repo, repo_path, &track, Some(&track), None)
            .await?;
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
        let remote = self.native().await.map_err(native_error)?;
        let revision = match target_state {
            Some(state) => Some(revision_ref(&spool, state)),
            None => overview.source_heads.into_iter().next(),
        };
        if revision.is_none() && target_state.is_none() {
            return Err(ProtocolError::InvalidState(
                "Fetch requires a started Thread with a published source revision".into(),
            ));
        }
        let open = FetchOpen {
            thread: Some(reference.clone()),
            revision: revision.clone(),
            selection: Some(TransferSelection {
                facets: vec![contract::SharedFacet::Source as i32],
                ..Default::default()
            }),
            ..Default::default()
        };
        let download = remote
            .fetch_content(open, thread_api::fetch::Limits::default())
            .await
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let scratch = repo.heddle_dir().join("source-transfers");
        objects::fs_atomic::create_private_dir_all(&scratch)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let staged = download
            .stage(&scratch)
            .await
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
        let now = chrono::Utc::now().timestamp();
        let final_state = staged
            .install(repo, now)
            .map_err(|error| ProtocolError::InvalidState(error.to_string()))?;
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
            checkpoint: Vec::new(),
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
