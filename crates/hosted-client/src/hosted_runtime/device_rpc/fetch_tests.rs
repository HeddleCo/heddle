//! Real Iroh selected source proofs include foreign integration ancestry and
//! never use target access to grant unrelated Thread ownership.
use std::collections::BTreeSet;

use crypto::{
    Ed25519Signer, Signer,
    thread_operation::{SignedGenesis, SignedOperation},
};
use objects::{
    object::{
        Attribution, Blob, CollaborationActor, CollaborationAnchor, CollaborationMetadata,
        CollaborationRevision, CollaborationScope, CollaborationSourceAnchor, ContextRevision,
        EntryVisibility, EntryVisibilityEntry, Principal, State, StateVisibility, Tree, TreeEntry,
        VisibilityTier,
        source_target::{
            SourceAffinity, SourceFileCore, SourceLineRange, SourceSelector, SourceTargetBinding,
            SourceTargetCore, SourceTargetReference, capture,
        },
        thread_replication::{
            Admission, GenesisOwner, ThreadGenesis, ThreadOperation, ThreadOperationBody,
        },
    },
    reference_store::Source,
    store::ObjectStore,
};

use super::*;
use crate::hosted_runtime::root_mint::mint_agent_root;
pub(super) async fn roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    endpoint_signer: &Ed25519Signer,
    owner: &OwnerState,
) {
    transfer(remote, repository, replica, endpoint_signer, owner, true).await;
}

pub(super) async fn partial_roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    endpoint_signer: &Ed25519Signer,
    owner: &OwnerState,
) {
    let visible = Blob::from_slice(b"visible source\nsecond line\n");
    let hidden = Blob::from_slice(b"hidden source\n");
    repository.store().put_blob(&visible).expect("visible blob");
    repository.store().put_blob(&hidden).expect("hidden blob");
    let tree = Tree::from_entries_salted_v4(
        vec![
            TreeEntry::file("visible.txt", visible.hash(), false).expect("visible entry"),
            TreeEntry::file("hidden.txt", hidden.hash(), false).expect("hidden entry"),
        ],
        vec![[11; 32], [12; 32]],
    )
    .expect("salted tree");
    repository.store().put_tree(&tree).expect("source tree");
    let state = State::new_snapshot(
        tree.hash(),
        vec![replica.genesis().expect("genesis").base],
        Attribution::human(Principal::new("Owner", "owner@test")),
    );
    repository.store().put_state(&state).expect("source state");
    let hidden_index = tree
        .entries()
        .iter()
        .position(|entry| entry.name() == "hidden.txt")
        .expect("hidden leaf");
    let sidecar = EntryVisibility::new(
        state.change_id,
        tree.hash(),
        vec![EntryVisibilityEntry {
            tree_id: tree.hash(),
            leaf_hash: tree
                .v4_leaf_hash_at(hidden_index)
                .expect("hidden commitment"),
            tier: VisibilityTier::Private {
                scope_label: "security".into(),
            },
        }],
    )
    .expect("visibility sidecar");
    repository
        .restore_entry_visibility_sidecar(
            &state.change_id,
            Some(sidecar.encode().expect("sidecar bytes")),
        )
        .expect("source visibility");
    let scope = CollaborationScope {
        spool: replica
            .genesis()
            .expect("genesis")
            .spool
            .parse()
            .expect("Spool UUID"),
        thread: Some(replica.thread_id()),
    };
    let file = SourceFileCore {
        scope: scope.clone(),
        revision: CollaborationRevision::State {
            state_id: state.id(),
        },
        path: "visible.txt".into(),
    };
    let core = SourceTargetCore {
        file: file.id().expect("file identity"),
        revision: file.revision.clone(),
        selector: SourceSelector::Lines {
            range: SourceLineRange {
                start: 1,
                end: 2,
                start_affinity: SourceAffinity::After,
                end_affinity: SourceAffinity::Before,
            },
        },
    };
    let signer = Ed25519Signer::from_seed(&[61; 32]).expect("context signer");
    let context = ContextRevision {
        version: 2,
        id: uuid::Uuid::from_u128(811),
        parents: vec![],
        metadata: CollaborationMetadata {
            scope,
            actor: CollaborationActor {
                principal_id: uuid::Uuid::from_u128(1),
                agent_id: None,
            },
            mentions: vec![],
        },
        anchor: CollaborationAnchor::Source {
            source: CollaborationSourceAnchor {
                revision: file.revision.clone(),
                path: file.path.clone(),
                symbol_id: String::new(),
                start_line: Some(2),
                end_line: Some(2),
                target: Some(SourceTargetReference {
                    target: core.id().expect("target identity"),
                    binding: SourceTargetBinding::ViewedThread,
                }),
            },
        },
        content: "Visible source annotation".into(),
        tags: vec![],
        supersedes: None,
        extracted_from: None,
        occurred_at_ms: 100,
    };
    let context_operation = ThreadOperation {
        version: 1,
        thread: replica.thread_id(),
        parents: BTreeSet::new(),
        publisher: signer.public_key().try_into().expect("publisher key"),
        body: ThreadOperationBody::Context(context.encode().expect("context bytes")),
    };
    assert_eq!(
        replica
            .receive(
                &SignedOperation::sign(&context_operation, &signer).expect("signed context"),
                repository.store(),
                |_| Ok(())
            )
            .expect("context admission"),
        Admission::Accepted
    );
    let id = repository
        .record_native_capture("device-test", state.id())
        .expect("signed source capture");
    let (signed, _) = replica
        .operation(&id)
        .expect("original lookup")
        .expect("original operation");
    let capture = signed
        .verify()
        .expect("original signature")
        .source_result()
        .expect("source body")
        .expect("capture");
    assert!(
        capture.source_targets.is_some(),
        "original source commits its reference descriptor"
    );
    let observed_spool = replica.genesis().expect("target genesis").spool;
    let mut target_view = remote
        .observe::<thread_api::rpc::CollaborationServiceObserveCollaboration>(
            ObserveCollaborationRequest {
                spool: Some(SpoolRef {
                    id: observed_spool.clone(),
                }),
                source_views: vec![SourceTargetView {
                    thread: Some(ThreadRef {
                        spool: Some(SpoolRef {
                            id: observed_spool.clone(),
                        }),
                        id: Some(ThreadId {
                            value: replica.thread_id().as_bytes().to_vec(),
                        }),
                    }),
                    revision: Some(RevisionRef {
                        spool: Some(SpoolRef {
                            id: observed_spool.clone(),
                        }),
                        revision: Some(revision_ref::Revision::State(
                            api::heddle::api::common::StateId {
                                value: state.id().as_bytes().to_vec(),
                            },
                        )),
                    }),
                }],
                ..Default::default()
            },
            None,
        )
        .await
        .expect("device collaboration target observation");
    let target_batch = target_view
        .next_commit()
        .await
        .expect("target stream")
        .expect("target snapshot");
    assert!(target_batch.changes.iter().any(|change| matches!(change,
        collaboration_event::Payload::SourceTarget(value)
            if matches!(&value.change, Some(source_target_resolution_event::Change::Upsert(resolution))
                if resolution.status == source_target_resolution::Status::Resolved as i32
                    && resolution.computed_for.as_ref().and_then(|v|v.revision.as_ref())
                        == Some(&revision_ref::Revision::State(api::heddle::api::common::StateId {
                            value: state.id().as_bytes().to_vec(),
                        }))
                    && resolution.location.as_ref().is_some_and(|location|location.path == "visible.txt"))
    )), "device must emit exact target projection for signed original");
    assert_eq!(
        capture
            .visibility
            .as_ref()
            .expect("signed privacy")
            .entries
            .len(),
        1
    );

    let private = State::new_snapshot(
        tree.hash(),
        vec![replica.genesis().expect("genesis").base],
        Attribution::human(Principal::new("Owner", "owner@test")),
    )
    .with_intent("whole-state private source");
    repository
        .store()
        .put_state(&private)
        .expect("private state");
    repository
        .put_state_visibility(StateVisibility {
            state: private.id(),
            tier: VisibilityTier::Private {
                scope_label: "security".into(),
            },
            embargo_until: None,
            declarer: repository.get_principal().expect("local declarer"),
            declared_at: chrono::Utc::now(),
            signature: None,
            supersedes: None,
        })
        .expect("signed state visibility");
    repository
        .record_native_capture("device-test", private.id())
        .expect("private original capture");
    let floor_thread = repository
        .create_native_thread(
            "signed-floor-test",
            replica.genesis().expect("genesis").base,
            None,
            "Signed floor",
        )
        .expect("independent signed-floor Thread");
    let floor_state = State::new_snapshot(
        tree.hash(),
        vec![floor_thread.genesis().expect("floor genesis").base],
        Attribution::human(Principal::new("Owner", "owner@test")),
    )
    .with_intent("signed floor without retained local sidecar");
    repository
        .store()
        .put_state(&floor_state)
        .expect("floor state");
    repository
        .put_state_visibility(StateVisibility {
            state: floor_state.id(),
            tier: VisibilityTier::Internal,
            embargo_until: None,
            declarer: repository.get_principal().expect("local declarer"),
            declared_at: chrono::Utc::now(),
            signature: None,
            supersedes: None,
        })
        .expect("authored Internal floor");
    repository
        .record_native_capture("signed-floor-test", floor_state.id())
        .expect("signed original with Internal floor");
    repository
        .restore_state_visibility_sidecar(&floor_state.id(), None)
        .expect("simulate metadata-only imported original without a local sidecar");
    assert_eq!(
        repository
            .effective_visibility_tier(&floor_state.id())
            .expect("local tier"),
        VisibilityTier::Public
    );
    let accountable = uuid::Uuid::parse_str(&owner.owner.as_ref().expect("owner account").id)
        .expect("owner UUID");
    assert_eq!(
        super::auth::source_visibility_floor(
            repository,
            &floor_thread,
            accountable,
            None,
            floor_state.id()
        )
        .expect("verified signed floor"),
        Some(VisibilityTier::Internal),
        "original signed visibility must survive a missing imported local sidecar"
    );

    let mut current_target_view = remote
        .observe::<thread_api::rpc::CollaborationServiceObserveCollaboration>(
            ObserveCollaborationRequest {
                spool: Some(SpoolRef {
                    id: observed_spool.clone(),
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("current device target observation");
    let current_batch = current_target_view
        .next_commit()
        .await
        .expect("current target stream")
        .expect("current target snapshot");
    assert!(current_batch.changes.iter().any(|change| matches!(change,
        collaboration_event::Payload::SourceTarget(value)
            if matches!(&value.change, Some(source_target_resolution_event::Change::Upsert(resolution))
                if resolution.status == source_target_resolution::Status::Unavailable as i32
                    && resolution.computed_for.is_none() && resolution.location.is_none())
    )), "concurrent or inaccessible current source must not disclose a target location");

    let genesis = replica.genesis().expect("selected Thread");
    let withheld = remote
        .fetch_content(open(&genesis, private.id()), Default::default())
        .await
        .err()
        .unwrap_or_else(|| panic!("whole-state private source unavailable"));
    assert_fetch_not_found(withheld);
    let mut observed = remote
        .observe::<thread_api::rpc::ThreadServiceObserveThread>(
            ObserveThreadRequest {
                thread: open(&genesis, state.id()).thread,
                sections: vec![
                    ThreadSection::Overview as i32,
                    ThreadSection::Captures as i32,
                ],
                ..Default::default()
            },
            None,
        )
        .await
        .expect("source-safe Thread observation");
    let batch = observed
        .next_commit()
        .await
        .expect("observation protocol")
        .expect("snapshot");
    let overview = batch
        .changes
        .iter()
        .find_map(|change| match change {
            thread_event::Payload::Overview(value) => Some(value),
            _ => None,
        })
        .expect("Thread overview");
    assert_eq!(
        overview.source_heads.len(),
        1,
        "whole-state private source head must not appear in Thread overview"
    );
    assert_eq!(
        overview.source_heads[0].revision.as_ref(),
        Some(&revision_ref::Revision::State(
            api::heddle::api::common::StateId {
                value: state.id().as_bytes().to_vec()
            }
        ))
    );
    assert!(
        overview.capture_count.is_none(),
        "unfiltered capture count must not leak"
    );
    assert!(overview.sections.iter().any(|status|
        status.section == "source" && status.coverage == Coverage::Partial as i32
    ), "mixed visible and withheld source tips have explicit partial coverage");
    assert_eq!(
        batch
            .changes
            .iter()
            .filter(|change| matches!(change, thread_event::Payload::Capture(_)))
            .count(),
        1,
        "whole-state private capture must be filtered before page projection"
    );
    let mut request = open(&genesis, state.id());
    request.selection.as_mut().expect("selection").allow_partial = true;
    let download = remote
        .fetch_content(request, Default::default())
        .await
        .expect("partial source Fetch");
    assert!(
        !download.ready().full_closure_available,
        "hidden leaf requires partial closure"
    );
    let scratch = tempfile::tempdir().expect("stage scratch");
    let staged = download
        .stage(scratch.path())
        .await
        .expect("verified HRT1 source");
    assert!(!staged.is_complete());
    assert_eq!(staged.state().id(), state.id());
    assert!(
        staged.operations().iter().any(|operation| operation
            .verify()
            .expect("original")
            .id()
            .expect("id")
            == id)
    );

    use base64::Engine as _;
    let root = Ed25519Signer::from_seed(&[71; 32]).expect("account authority");
    let seed_credential = mint_agent_root(&[71; 32]).expect("binding authority");
    let minted = crate::hosted_runtime::root_mint::remint_stored_root(
        &seed_credential.private_key_pem,
        &seed_credential.subject,
        None,
        Some("partial-source-transfer-binding"),
    )
    .expect("binding session");
    let credential = base64::engine::general_purpose::URL_SAFE
        .decode(minted.token)
        .expect("binding bytes");
    let now = chrono::Utc::now().timestamp();
    let binding = thread_api::root_attachment::sign_binding(
        &root,
        endpoint_signer,
        RootAttachmentBinding {
            format_version: 2,
            account_id: owner.owner.as_ref().expect("account").id.clone(),
            root_public_key: root.public_key().to_vec(),
            subject_public_key: root.public_key().to_vec(),
            device: Some(EndpointRef {
                kind: EndpointKind::Device as i32,
                public_key: endpoint_signer.public_key().to_vec(),
            }),
            credential_digest: blake3::hash(&credential).as_bytes().to_vec(),
            not_before_unix_seconds: now - 1,
            expires_at_unix_seconds: now + 300,
            pairing_challenge: vec![92; 32],
        },
    )
    .expect("signed endpoint binding");
    let authority = repo::device_authority::DeviceAuthority {
        owner: owner.clone(),
        mint_roots: vec![],
        revoked_ids: vec![],
        revoked_mint_roots: vec![],
        revoked_publishers: vec![],
    };
    let receiver = tempfile::tempdir().expect("partial receiver");
    let receiving_repository = repo::Repository::init(receiver.path()).expect("unseeded receiver");
    assert_eq!(
        staged
            .install_owned_device(
                &receiving_repository,
                &authority,
                thread_api::fetch::OwnedDeviceBinding {
                    attachment: &binding,
                    credential: &credential
                },
                &format!("spool/{}", genesis.spool),
                now,
            )
            .expect("install metadata and visible HRT1 closure"),
        state.id()
    );
    let installed = repo::thread_replication::ThreadReplica::open(
        receiving_repository.heddle_dir(),
        genesis.id().expect("Thread"),
    )
    .expect("installed Thread");
    assert!(
        installed
            .reference_projection_pending(id)
            .expect("pending reference projection")
    );
    assert!(
        !installed
            .has_source_possession(state.id())
            .expect("full possession")
    );

    let proof = signed
        .verify()
        .expect("original signature")
        .reference_proof(&genesis)
        .expect("typed source proof")
        .expect("signed descriptor");
    let closure = capture::closure(
        &Source(repository.store()),
        proof.descriptor,
        &proof.scope,
        proof.state,
    )
    .expect("exact original reference closure");
    assert!(closure.blobs.contains_key(&proof.descriptor));
    for (hash, bytes) in closure.blobs {
        assert_eq!(
            receiving_repository
                .store()
                .put_blob(&Blob::new(bytes))
                .expect("hydrate exact reference blob"),
            hash
        );
    }
    installed
        .complete_reference_projection(id, receiving_repository.store())
        .expect("hydrate verified reference projection");
    assert!(
        !installed
            .reference_projection_pending(id)
            .expect("projection complete")
    );
    assert!(
        !installed
            .has_source_possession(state.id())
            .expect("partial remains partial")
    );
}
pub(super) async fn initial_base_roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    replica: &repo::thread_replication::ThreadReplica,
) {
    let genesis = replica.genesis().expect("fresh Thread genesis");
    let seed = objects::object::thread_replication::hosted_import::synthetic_initial_base()
        .expect("canonical seed");
    assert_eq!(
        genesis.base,
        seed.id(),
        "fresh native Thread uses the portable seed"
    );
    let download = remote
        .fetch_content(open(&genesis, seed.id()), Default::default())
        .await
        .expect("exact known initial source");
    let scratch = tempfile::tempdir().expect("initial download staging");
    let staged = download
        .stage(scratch.path())
        .await
        .expect("validated initial source");
    assert_eq!(staged.state().id(), seed.id());
    assert!(
        staged.operations().is_empty(),
        "system seed is not attributed to a human Capture"
    );
    let unknown = remote
        .fetch_content(
            open(&genesis, objects::object::StateId::from_bytes([97; 32])),
            Default::default(),
        )
        .await
        .err()
        .unwrap_or_else(|| panic!("arbitrary hash cannot claim the system seed"));
    assert_fetch_not_found(unknown);
}
fn assert_fetch_not_found(error: thread_api::fetch::Error) {
    assert!(
        matches!(&error,
            thread_api::fetch::Error::Client(api::v2::client::ClientError::Transport(
                thread_api::transport::Error::Remote(failure)
            )) if failure.code == api::heddle::api::common::CallFailureCode::NotFound as i32
                && failure.message == "selected source unavailable"
        ),
        "missing and withheld source use the same typed public failure: {error}"
    );
}
pub(super) async fn claimed_roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    endpoint_signer: &Ed25519Signer,
    owner: &OwnerState,
) {
    transfer(remote, repository, replica, endpoint_signer, owner, false).await;
}
async fn transfer(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    endpoint_signer: &Ed25519Signer,
    owner: &OwnerState,
    integrated: bool,
) {
    let genesis = replica.genesis().expect("selected genesis");
    let selected = *replica
        .view()
        .expect("source view")
        .source_heads
        .first()
        .expect("selected source");
    let request = open(&genesis, selected);
    use base64::Engine as _;
    let root =
        Ed25519Signer::from_seed(&[71; 32]).expect("independently enrolled account authority");
    assert_ne!(
        root.public_key(),
        endpoint_signer.public_key(),
        "transport identity is distinct from mint authority"
    );
    let seed_credential = mint_agent_root(&[71; 32]).expect("current endpoint binding authority");
    let minted = crate::hosted_runtime::root_mint::remint_stored_root(
        &seed_credential.private_key_pem,
        &seed_credential.subject,
        None,
        Some("source-transfer-endpoint-binding-only"),
    )
    .expect("distinct authority-signed binding session");
    let binding_session = crate::hosted_runtime::root_mint::authority_session_fact(&minted.token)
        .expect("exact session identity");
    let credential = base64::engine::general_purpose::URL_SAFE
        .decode(minted.token)
        .expect("private binding bytes");
    let now = chrono::Utc::now().timestamp();
    let endpoint = EndpointRef {
        kind: EndpointKind::Device as i32,
        public_key: endpoint_signer.public_key().to_vec(),
    };
    let binding = thread_api::root_attachment::sign_binding(
        &root,
        endpoint_signer,
        RootAttachmentBinding {
            format_version: 2,
            account_id: owner.owner.as_ref().expect("account").id.clone(),
            root_public_key: root.public_key().to_vec(),
            subject_public_key: root.public_key().to_vec(),
            device: Some(endpoint),
            credential_digest: blake3::hash(&credential).as_bytes().to_vec(),
            not_before_unix_seconds: now - 1,
            expires_at_unix_seconds: now + 300,
            pairing_challenge: vec![91; 32],
        },
    )
    .expect("account and actual endpoint prove possession");
    let authority = repo::device_authority::DeviceAuthority {
        owner: owner.clone(),
        mint_roots: vec![],
        revoked_ids: vec![],
        revoked_mint_roots: vec![],
        revoked_publishers: vec![],
    };

    if integrated {
        let initial = remote
            .fetch_content(open(&genesis, genesis.base), Default::default())
            .await
            .expect("known initial base Fetch");
        let scratch = tempfile::tempdir().expect("initial staging scratch");
        let staged = initial
            .stage(scratch.path())
            .await
            .expect("initial source staging");
        let receiver = tempfile::tempdir().expect("initial receiver");
        let receiving_repository =
            repo::Repository::init(receiver.path()).expect("unseeded receiver");
        assert_eq!(
            staged
                .install_owned_device(
                    &receiving_repository,
                    &authority,
                    thread_api::fetch::OwnedDeviceBinding {
                        attachment: &binding,
                        credential: &credential,
                    },
                    &format!("spool/{}", genesis.spool),
                    now,
                )
                .expect("install exact initial source without invented author"),
            genesis.base
        );
        let installed = repo::thread_replication::ThreadReplica::open(
            receiving_repository.heddle_dir(),
            genesis.id().expect("Thread"),
        )
        .expect("installed initial Thread");
        assert!(
            installed
                .has_source_possession(genesis.base)
                .expect("base possession")
        );
    }

    for _ in 0..2 {
        let download = remote
            .fetch_content(request.clone(), Default::default())
            .await
            .expect("owned device exact source Fetch");
        assert!(
            download.ready().owner_genesis.is_none(),
            "device-only source does not require Weft ownership"
        );
        let scratch = tempfile::tempdir().expect("download staging");
        let staged = download
            .stage(scratch.path())
            .await
            .expect("complete source closure");
        assert_eq!(staged.state().id(), selected);
        assert_eq!(
            staged.dependency_geneses().len(),
            usize::from(integrated),
            "original integrated source Thread is portable"
        );
        let operations = staged.operations();
        assert!(!operations.is_empty());
        if integrated {
            let (position, integration) = operations
                .iter()
                .enumerate()
                .find_map(|(index, signed)| {
                    let operation = signed.verify().expect("original");
                    if operation.thread != genesis.id().expect("selected Thread") {
                        return None;
                    }
                    operation
                        .local_integration()
                        .expect("typed integration")
                        .map(|receipt| (index, receipt))
                })
                .expect("selected integration original");
            let source_position = operations
                .iter()
                .position(|signed| {
                    signed
                        .verify()
                        .expect("source original")
                        .id()
                        .expect("original ID")
                        == integration.source_operation
                })
                .expect("original integrated source proof");
            assert!(
                source_position < position,
                "source dependency must install before integration; later claim cutoff may follow both"
            );
        }
        let paths = staged.artifact_paths();
        assert!(paths.iter().all(|p| p.exists()));
        let receiver = tempfile::tempdir().expect("new receiving device repository");
        let receiving_repository = repo::Repository::init(receiver.path()).expect("receiver");
        assert_eq!(
            staged
                .install_owned_device(
                    &receiving_repository,
                    &authority,
                    thread_api::fetch::OwnedDeviceBinding {
                        attachment: &binding,
                        credential: &credential
                    },
                    &format!("spool/{}", genesis.spool),
                    now
                )
                .expect("actual independently bound device installation"),
            selected
        );
        let installed = repo::thread_replication::ThreadReplica::open(
            receiving_repository.heddle_dir(),
            genesis.id().expect("Thread"),
        )
        .expect("installed original Thread");
        assert!(
            installed
                .has_source_possession(selected)
                .expect("durable source availability")
        );
        assert_eq!(installed.genesis().expect("immutable identity"), genesis);
        if !integrated {
            assert!(
                matches!(
                    installed
                        .effective_owner()
                        .expect("explicit signed claim installed"),
                    GenesisOwner::Account(_)
                ),
                "claim must be applied after cutoff proof"
            );
            assert_eq!(
                installed.ownership_claims().expect("retained claim"),
                replica.ownership_claims().expect("source claim")
            );
        }

        assert!(
            paths.iter().all(|p| !p.exists()),
            "staging cleanup removes both artifacts"
        );
    }
    let roots = biscuit_verifier::parse_ed25519_public_keys_hex(&hex::encode(root.public_key()), 1)
        .expect("independent mint root");
    let checked = thread_api::root_attachment::verify(
        &binding,
        &credential,
        &roots,
        &owner.owner.as_ref().expect("account").id,
        binding.device.as_ref().expect("endpoint"),
        chrono::DateTime::from_timestamp(now, 0).expect("clock"),
    )
    .expect("bound credential IDs");
    assert!(
        checked
            .credential_revocation_ids()
            .contains(&binding_session),
        "binding retains exact session revocation identity"
    );
    let revoked = repo::device_authority::DeviceAuthority {
        owner: owner.clone(),
        mint_roots: vec![],
        revoked_ids: vec![binding_session.clone()],
        revoked_mint_roots: vec![],
        revoked_publishers: vec![],
    };
    let scratch = tempfile::tempdir().expect("revoked staging");
    let staged = remote
        .fetch_content(request.clone(), Default::default())
        .await
        .expect("same source current courier")
        .stage(scratch.path())
        .await
        .expect("complete original source");
    let receiver = tempfile::tempdir().expect("revoked receiver");
    let receiving_repository = repo::Repository::init(receiver.path()).expect("receiver");
    let error = staged
        .install_owned_device(
            &receiving_repository,
            &revoked,
            thread_api::fetch::OwnedDeviceBinding {
                attachment: &binding,
                credential: &credential,
            },
            &format!("spool/{}", genesis.spool),
            now,
        )
        .err()
        .unwrap_or_else(|| panic!("explicitly revoked binding cannot install source"));
    assert!(
        error
            .to_string()
            .contains("endpoint binding credential is explicitly revoked"),
        "binding revocation checked before original source installation: {error}"
    );
    let stranger = Ed25519Signer::from_seed(&[113; 32]).expect("stranger key");
    let foreign = ThreadGenesis {
        owner: GenesisOwner::LocalKey(stranger.public_key().try_into().expect("key")),
        name: "unowned exact source".into(),
        creator: stranger.public_key().try_into().expect("key"),
        nonce: vec![113],
        base: selected,
        ..genesis.clone()
    };
    let foreign_replica = repo::thread_replication::ThreadReplica::create(
        repository.heddle_dir(),
        &SignedGenesis::sign(&foreign, &stranger).expect("original genesis"),
    )
    .expect("local signed original");
    let source = repository
        .store()
        .get_state(&selected)
        .expect("source read")
        .expect("source object");
    let state = State::new_snapshot(
        source.tree,
        vec![selected],
        Attribution::human(Principal::new("foreign author", "")),
    );
    let operation = ThreadOperation {
        version: 1,
        thread: foreign.id().expect("Thread"),
        parents: BTreeSet::new(),
        publisher: foreign.creator,
        body: ThreadOperationBody::Capture(
            objects::object::thread_replication::AuthoredCapture::local(
                state.encode_current_msgpack().expect("State").into(),
            ),
        ),
    };
    assert_eq!(
        foreign_replica
            .receive(
                &SignedOperation::sign(&operation, &stranger).expect("original source"),
                repository.store(),
                |_| Ok(())
            )
            .expect("trusted local import fixture"),
        Admission::Accepted
    );
    assert!(
        remote
            .fetch_content(open(&foreign, state.id()), Default::default())
            .await
            .is_err(),
        "same Spool and existing source object do not grant unowned Thread access"
    );
}
fn open(genesis: &ThreadGenesis, state: objects::object::StateId) -> FetchOpen {
    let spool = SpoolRef {
        id: genesis.spool.clone(),
    };
    FetchOpen {
        thread: Some(ThreadRef {
            spool: Some(spool.clone()),
            id: Some(ThreadId {
                value: genesis.id().expect("id").as_bytes().to_vec(),
            }),
        }),
        revision: Some(RevisionRef {
            spool: Some(spool),
            revision: Some(revision_ref::Revision::State(
                api::heddle::api::common::StateId {
                    value: state.as_bytes().to_vec(),
                },
            )),
        }),
        selection: Some(TransferSelection {
            facets: vec![SharedFacet::Source as i32],
            ..Default::default()
        }),
        ..Default::default()
    }
}
