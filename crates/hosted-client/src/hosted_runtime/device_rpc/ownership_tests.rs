//! The browser authorizes one exact claim; the owned device supplies only the
//! matching retained local-owner signature.
use crypto::{Ed25519Signer, Signer, thread_ownership_claim::SignedOwnershipAcceptance};
use objects::{
    object::{
        CollaborationActor,
        thread_replication::{
            GenesisOwner, SourceAuthor, ThreadFacet, ownership_claim::ThreadOwnershipClaim,
        },
    },
    store::ObjectStore,
};

use super::*;

pub(super) async fn claim(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
) {
    let genesis = replica.genesis().expect("original genesis");
    let GenesisOwner::LocalKey(prior_local_key) = genesis.owner else {
        panic!("fixture starts locally owned")
    };
    let spool: uuid::Uuid = genesis.spool.parse().expect("Spool UUID");
    let signer = Ed25519Signer::from_seed(&[71; 32]).expect("account acceptor");
    let now = chrono::Utc::now().timestamp();
    let home = repo::identity::heddle_home_dir();
    let authority = repo::device_authority::load(&home, now).expect("independent device owner");
    let minted = crate::hosted_runtime::root_mint::mint_agent_root(&[71; 32])
        .expect("actual account capability");
    let mint = biscuit_verifier::PublicKey::from_bytes(
        signer.public_key(),
        biscuit_auth::Algorithm::Ed25519,
    )
    .expect("mint");
    let token =
        biscuit_verifier::parse_token(&minted.token, &[mint]).expect("sealed account token");
    let envelope = repo::thread_replication::metadata::prepare_control_authority(
        &authority,
        &signer.public_key().try_into().expect("key"),
        &token,
        now,
    )
    .expect("portable account acceptance");
    let reference = ThreadRef {
        spool: Some(SpoolRef {
            id: genesis.spool.clone(),
        }),
        id: Some(ThreadId {
            value: replica.thread_id().as_bytes().to_vec(),
        }),
    };
    let mut page = remote
        .api
        .observe::<thread_api::rpc::ThreadServiceObserveThread>(&ObserveThreadRequest {
            thread: Some(reference),
            sections: vec![ThreadSection::Overview as i32],
            observe: Some(ObserveOptions {
                mode: ObservationMode::Once as i32,
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("ordinary page supplies claim preparation");
    let mut observed = None;
    loop {
        let event = page
            .next()
            .await
            .expect("overview event")
            .expect("overview stream");
        if let Some(thread_event::Payload::Overview(value)) = event.payload {
            observed = Some(value);
        }
        if matches!(
            event.frame.and_then(|frame| frame.body),
            Some(stream_frame::Body::Checkpoint(_))
        ) {
            break;
        }
    }
    drop(page);
    let observed = observed.expect("ordinary Thread overview");
    assert!(
        matches!(observed.ownership.and_then(|value|value.owner),Some(thread_ownership::Owner::LocalKey(key)) if key==prior_local_key)
    );
    let frontier = observed
        .source_frontier
        .expect("observed original operation frontier");
    assert!(frontier.complete);
    let observed_frontier = frontier
        .operation_ids
        .into_iter()
        .map(|id| objects::object::ContentHash::from_bytes(id.try_into().expect("operation ID")))
        .collect();
    assert_eq!(
        replica
            .frontier_page(ThreadFacet::Source, None, 128)
            .expect("actual source frontier")
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        observed_frontier
    );
    let statement = ThreadOwnershipClaim {
        version: 1,
        thread: replica.thread_id(),
        prior_local_key,
        accepting_publisher: signer.public_key().try_into().expect("acceptor"),
        acceptance: SourceAuthor::account(
            spool,
            CollaborationActor {
                principal_id: uuid::Uuid::from_bytes([9; 16]),
                agent_id: None,
            },
            envelope,
        )
        .expect("account author"),
        source_frontier: observed_frontier,
    };
    let acceptance =
        SignedOwnershipAcceptance::sign(&statement, &signer).expect("accept exact claim");
    let request = ClaimThreadOwnershipRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        thread: Some(ThreadRef {
            spool: Some(SpoolRef {
                id: genesis.spool.clone(),
            }),
            id: Some(ThreadId {
                value: replica.thread_id().as_bytes().to_vec(),
            }),
        }),
        claim: Some(
            thread_api::thread_ownership::encode_acceptance(&acceptance).expect("wire acceptance"),
        ),
    };
    let generation = replica.generation().expect("before rejected acceptor");
    let foreign = Ed25519Signer::from_seed(&[78; 32]).expect("different acceptor");
    let mut foreign_statement = statement.clone();
    foreign_statement.accepting_publisher = foreign.public_key().try_into().expect("foreign key");
    let mut rejected = request.clone();
    rejected.client_operation_id = uuid::Uuid::new_v4().to_string();
    rejected.claim = Some(
        thread_api::thread_ownership::encode_acceptance(
            &SignedOwnershipAcceptance::sign(&foreign_statement, &foreign)
                .expect("valid foreign signature"),
        )
        .expect("foreign acceptance"),
    );
    let error = remote
        .api
        .call::<thread_api::rpc::ThreadServiceClaimThreadOwnership>(&rejected)
        .await
        .expect_err("acceptor must match original account capability");
    assert!(
        error.to_string().contains("publisher or account differs"),
        "reject exact original acceptor mismatch: {error}"
    );
    assert!(
        replica
            .ownership_claims()
            .expect("no claim on rejection")
            .is_empty()
    );
    assert_eq!(
        replica.generation().expect("no rejected claim mutation"),
        generation
    );
    let response = remote
        .api
        .call::<thread_api::rpc::ThreadServiceClaimThreadOwnership>(&request)
        .await
        .expect("device co-signs explicit ownership claim");
    let repeated = remote
        .api
        .call::<thread_api::rpc::ThreadServiceClaimThreadOwnership>(&request)
        .await
        .expect("exact claim command replay");
    assert_eq!(
        response, repeated,
        "claim replay preserves original durable response"
    );
    let thread_api::thread_ownership::ClaimProof::Complete(proof) =
        thread_api::thread_ownership::decode(response.claim.as_ref().expect("dual proof"))
            .expect("verify returned claim")
    else {
        panic!("device must return both signatures")
    };
    assert_eq!(proof.verify().expect("canonical claim"), statement);
    assert_eq!(replica.genesis().expect("unchanged genesis"), genesis);
    assert_eq!(
        replica.effective_owner().expect("effective owner"),
        GenesisOwner::Account(uuid::Uuid::from_bytes([9; 16]))
    );
    conflict_status(remote, repository, &authority, &signer, &statement).await;
}

async fn conflict_status(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    authority: &repo::device_authority::DeviceAuthority,
    account: &Ed25519Signer,
    previous: &ThreadOwnershipClaim,
) {
    let base = repository.head().expect("head").expect("base");
    let replica = repository
        .create_native_thread(
            "ownership-conflict-private-name",
            base,
            None,
            "private recovery intent",
        )
        .expect("local recovery Thread");
    let tree = repository
        .store()
        .get_state(&base)
        .expect("base State")
        .expect("base exists")
        .tree;
    let capture = objects::object::State::new_snapshot(
        tree,
        vec![base],
        objects::object::Attribution::human(objects::object::Principal::new("local owner", "")),
    )
    .with_intent("private conflicted source");
    repository
        .store()
        .put_state(&capture)
        .expect("source State");
    repository
        .record_native_capture("ownership-conflict-private-name", capture.id())
        .expect("actual source capture");
    let local = repository
        .native_thread_signer(&replica)
        .expect("actual local owner");
    let mut statement = ThreadOwnershipClaim {
        thread: replica.thread_id(),
        prior_local_key: local.public_key().try_into().expect("local key"),
        source_frontier: replica
            .frontier_page(ThreadFacet::Source, None, 128)
            .expect("cutoff")
            .into_iter()
            .collect(),
        ..previous.clone()
    };
    let path = repo::device_catalog::load(
        &repo::identity::heddle_home_dir(),
        replica
            .genesis()
            .expect("genesis")
            .spool
            .parse()
            .expect("Spool UUID"),
    )
    .expect("Spool")
    .capability_path;
    let first =
        crypto::thread_ownership_claim::SignedOwnershipClaim::sign(&statement, &local, account)
            .expect("first valid claim");
    replica
        .claim_ownership(&first, authority, &path, chrono::Utc::now().timestamp())
        .expect("first account claim");
    let reminted = crate::hosted_runtime::root_mint::mint_agent_root(&[71; 32])
        .expect("independent second valid account acceptance");
    let mint = biscuit_verifier::PublicKey::from_bytes(
        account.public_key(),
        biscuit_auth::Algorithm::Ed25519,
    )
    .expect("original mint");
    let second_token = biscuit_verifier::parse_token(&reminted.token, &[mint])
        .expect("genuine independently signed original capability");
    let proof = repo::thread_replication::metadata::prepare_control_authority(
        authority,
        &account.public_key().try_into().expect("account key"),
        &second_token,
        chrono::Utc::now().timestamp(),
    )
    .expect("different valid acceptance");
    let SourceAuthor::Account { spool, actor, .. } = previous.acceptance.clone() else {
        panic!("account")
    };
    statement.acceptance =
        SourceAuthor::account(spool, actor, proof).expect("different valid account acceptance");
    let second =
        crypto::thread_ownership_claim::SignedOwnershipClaim::sign(&statement, &local, account)
            .expect("second valid claim");
    let conflict = replica
        .claim_ownership(&second, authority, &path, chrono::Utc::now().timestamp())
        .expect_err("competing claim fails closed");
    assert!(
        conflict.to_string().contains("conflicting"),
        "different valid acceptance must reach conflict admission: {conflict}"
    );
    assert!(
        !super::auth::thread_visible(repository, &replica, uuid::Uuid::from_bytes([9; 16]), None)
            .expect("conflict denies this origin without aborting candidate search")
    );
    let mut content = remote
        .api
        .observe::<thread_api::rpc::ContentServiceReadContent>(&ReadContentRequest {
            revision: Some(RevisionRef {
                spool: Some(SpoolRef {
                    id: spool.to_string(),
                }),
                revision: Some(revision_ref::Revision::State(
                    api::heddle::api::v1alpha1::StateId {
                        value: base.as_bytes().to_vec(),
                    },
                )),
            }),
            selections: vec![ContentRead {
                selection_id: "shared-base".into(),
                selection: Some(content_read::Selection::State(StateRead::default())),
            }],
            ..Default::default()
        })
        .await
        .expect("immutable source remains reachable via independently authorized original Thread");
    let mut source_seen = false;
    while let Some(event) = content.next().await.expect("source frame") {
        if matches!(event.payload, Some(content_event::Payload::State(_))) {
            source_seen = true;
        }
    }
    assert!(
        source_seen,
        "authorized shared State remains readable despite another conflicted origin"
    );
    let reference = ThreadRef {
        spool: Some(SpoolRef {
            id: spool.to_string(),
        }),
        id: Some(ThreadId {
            value: replica.thread_id().as_bytes().to_vec(),
        }),
    };
    let mut stream = remote
        .api
        .observe::<thread_api::rpc::ThreadServiceObserveThread>(&ObserveThreadRequest {
            thread: Some(reference.clone()),
            sections: vec![
                ThreadSection::Overview as i32,
                ThreadSection::Captures as i32,
                ThreadSection::Collaboration as i32,
            ],
            observe: Some(ObserveOptions {
                mode: ObservationMode::Once as i32,
                ..Default::default()
            }),
            ..Default::default()
        })
        .await
        .expect("actual original owner may inspect conflict status");
    let mut overview_seen = false;
    let mut unavailable = std::collections::BTreeSet::new();
    loop {
        let event = stream
            .next()
            .await
            .expect("status event")
            .expect("open status view");
        match event.payload {
            Some(thread_event::Payload::Overview(view)) => {
                assert_eq!(view.r#ref, Some(reference.clone()));
                assert!(
                    view.name.is_empty()
                        && view.intent.is_none()
                        && view.source_heads.is_empty()
                        && view.source_frontier.is_none(),
                    "conflict status never exposes private data"
                );
                let Some(thread_ownership::Owner::Conflict(conflict)) =
                    view.ownership.and_then(|value| value.owner)
                else {
                    panic!("explicit conflict status")
                };
                assert_eq!(conflict.claim_ids.len(), 2);
                overview_seen = true;
            }
            Some(thread_event::Payload::Status(status)) => {
                assert_eq!(status.coverage, Coverage::Unavailable as i32);
                unavailable.insert(status.section);
            }
            None => {}
            Some(other) => {
                panic!("conflict recovery must not emit requested private records: {other:?}")
            }
        }
        if matches!(
            event.frame.and_then(|frame| frame.body),
            Some(stream_frame::Body::Checkpoint(_))
        ) {
            break;
        }
    }
    assert!(overview_seen);
    assert_eq!(
        unavailable,
        ["captures".to_string(), "collaboration".to_string()].into()
    );
}
