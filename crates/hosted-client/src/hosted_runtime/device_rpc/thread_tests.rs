use crypto::{Ed25519Signer, Signer};
use objects::object::{
    ContentHash,
    thread_replication::{
        ThreadGenesis,
        metadata::{Control, Intent, Lifecycle, Property, Review, ReviewKind, SharingPolicy},
    },
};
use thread_api::{
    Remote,
    thread_control::{Author, PreparedControl},
    transport::IrohTransport,
};

use super::*;

pub(super) async fn roundtrip(
    remote: &Remote<IrohTransport<thread_api::credentials::Credentials>>,
    device: &DeviceRpc,
    repository: &repo::Repository,
    spool: uuid::Uuid,
) {
    let signer = Ed25519Signer::from_seed(&[71; 32]).expect("owner signer");
    let base = repository.head().expect("head").expect("base");
    let genesis = ThreadGenesis {
        owner: objects::object::thread_replication::GenesisOwner::Account(uuid::Uuid::from_bytes(
            [9; 16],
        )),
        version: 1,
        spool: spool.to_string(),
        parent: None,
        base,
        name: "commands".into(),
        intent: "initial".into(),
        creator: signer.public_key().try_into().expect("key"),
        nonce: vec![37; 32],
    };
    let authority = repo::device_authority::load(&device.home, chrono::Utc::now().timestamp())
        .expect("authority");
    let token = crate::hosted_runtime::root_mint::mint_agent_root(&[71; 32]).expect("root token");
    let key = biscuit_verifier::PublicKey::from_bytes(
        signer.public_key(),
        biscuit_auth::Algorithm::Ed25519,
    )
    .expect("root key");
    let parsed = biscuit_verifier::parse_token(&token.token, &[key]).expect("token");
    let proof = repo::thread_replication::metadata::prepare_control_authority(
        &authority,
        &signer.public_key().try_into().expect("key"),
        &parsed,
        chrono::Utc::now().timestamp(),
    )
    .expect("proof");
    let request = StartThreadRequest {
        creator_authority: proof.clone(),
        client_operation_id: uuid::Uuid::now_v7().to_string(),
        spool: Some(SpoolRef {
            id: spool.to_string(),
        }),
        thread_genesis: Some(
            thread_api::replication::opening::sign_genesis(&genesis, &signer).expect("genesis"),
        ),
    };
    let started = remote
        .api
        .call::<thread_api::rpc::ThreadServiceStartThread>(&request)
        .await
        .expect("native StartThread");
    assert_eq!(
        started,
        remote
            .api
            .call::<thread_api::rpc::ThreadServiceStartThread>(&request)
            .await
            .expect("exact retry")
    );
    let foreign_signer = Ed25519Signer::from_seed(&[88; 32]).expect("different publisher");
    let foreign = ThreadGenesis {
        creator: foreign_signer.public_key().try_into().expect("key"),
        nonce: vec![88],
        ..genesis.clone()
    };
    let failure = remote
        .api
        .call::<thread_api::rpc::ThreadServiceStartThread>(&StartThreadRequest {
            client_operation_id: uuid::Uuid::now_v7().to_string(),
            thread_genesis: Some(
                thread_api::replication::opening::sign_genesis(&foreign, &foreign_signer)
                    .expect("valid foreign signature"),
            ),
            ..request.clone()
        })
        .await
        .expect_err("creator cannot be substituted under current delivery");
    assert!(
        failure.to_string().contains("creator"),
        "precise original creator failure: {failure}"
    );
    assert!(
        repo::thread_replication::ThreadReplica::open(
            repository.heddle_dir(),
            foreign.id().expect("ID")
        )
        .is_err(),
        "invalid original creator never creates a replica"
    );
    let first = started.thread.expect("Thread overview");
    assert_eq!(first.name, "commands");
    assert_eq!(first.metadata_frontiers.len(), 6);
    let prepare = |view: &ThreadOverview, control| {
        PreparedControl::sign(
            view,
            control,
            Author {
                account: uuid::Uuid::from_bytes([9; 16]),
                agent_id: None,
                authority_envelope: &proof,
            },
            uuid::Uuid::now_v7(),
            chrono::Utc::now().timestamp_millis(),
            &signer,
        )
        .expect("prepare from observed frontier")
    };
    let name = prepare(&first, Control::Name("renamed".into()));
    let stale = prepare(&first, Control::Name("stale edit".into()));
    let lifecycle = prepare(&first, Control::Lifecycle(Lifecycle::Active));
    let renamed = remote
        .api
        .call::<thread_api::rpc::ThreadServiceRenameThread>(&name.rename().expect("request"))
        .await
        .expect("rename");
    assert_eq!(renamed.thread.as_ref().expect("overview").name, "renamed");
    // Another field changed the overall view; the original lifecycle frontier
    // is still current and must remain usable without another read.
    let active = remote
        .api
        .call::<thread_api::rpc::ThreadServiceChangeLifecycle>(
            &lifecycle.change_lifecycle().expect("request"),
        )
        .await
        .expect("independent lifecycle CAS");
    assert_eq!(
        active.thread.as_ref().expect("overview").lifecycle,
        ThreadLifecycle::Active as i32
    );
    let rejected = remote
        .api
        .call::<thread_api::rpc::ThreadServiceRenameThread>(&stale.rename().expect("request"))
        .await
        .expect_err("stale same-field edit");
    assert!(
        rejected.to_string().contains("frontier"),
        "precise field CAS failure: {rejected}"
    );
    // Simulate lost response after durable admission by removing only the saved
    // response journal; exact signed operation must still replay without CAS error.
    let path = repository
        .heddle_dir()
        .join("device-thread-commands")
        .join(format!("{}.json", name.control.client_operation_id));
    std::fs::remove_file(path).expect("simulate lost command journal");
    let retried = remote
        .api
        .call::<thread_api::rpc::ThreadServiceRenameThread>(&name.rename().expect("request"))
        .await
        .expect("durable operation retry");
    assert_eq!(retried.thread.as_ref().expect("overview").name, "renamed");
    let intent = prepare(
        active.thread.as_ref().expect("overview"),
        Control::Intent(Intent {
            outcome: "new goal".into(),
            acceptance_criteria: vec!["verified".into()],
            origin_urls: vec![],
            principal_approved: true,
        }),
    );
    let changed = remote
        .api
        .call::<thread_api::rpc::ThreadServiceReviseIntent>(
            &intent.revise_intent().expect("request"),
        )
        .await
        .expect("intent");
    assert_eq!(
        changed
            .thread
            .as_ref()
            .expect("overview")
            .intent
            .as_ref()
            .expect("intent")
            .outcome,
        "new goal"
    );
    let sharing = prepare(
        changed.thread.as_ref().expect("overview"),
        Control::Sharing(SharingPolicy {
            ongoing: true,
            destinations: vec![],
        }),
    );
    let _shared = remote
        .api
        .call::<thread_api::rpc::ThreadServiceSetSharingPolicy>(
            &sharing.set_sharing().expect("request"),
        )
        .await
        .expect("sharing");
    let policy = ContentHash::from_bytes(
        changed
            .thread
            .as_ref()
            .expect("overview")
            .review_policy_version
            .clone()
            .try_into()
            .expect("policy"),
    );
    let review = prepare(
        changed.thread.as_ref().expect("overview"),
        Control::Review(Review {
            id: uuid::Uuid::now_v7(),
            source: base,
            target: base,
            policy_version: policy,
            kind: ReviewKind::Approval,
            explanation: "exact comparison".into(),
            revokes: None,
            expires_at_unix_seconds: None,
        }),
    );
    let mut observed = remote
        .observe::<thread_api::rpc::ThreadServiceObserveThread>(
            ObserveThreadRequest {
                thread: first.r#ref.clone(),
                sections: vec![
                    ThreadSection::Overview as i32,
                    ThreadSection::Review as i32,
                    ThreadSection::Sharing as i32,
                ],
                observe: Some(ObserveOptions {
                    mode: ObservationMode::Follow as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("Thread follow");
    let initial = tokio::time::timeout(std::time::Duration::from_secs(5), observed.next_commit())
        .await
        .expect("snapshot deadline")
        .expect("protocol")
        .expect("snapshot");
    assert!(initial.replace);
    remote
        .api
        .call::<thread_api::rpc::ThreadServiceRecordReview>(
            &review.record_review().expect("request"),
        )
        .await
        .expect("original signed review");
    tokio::time::timeout(std::time::Duration::from_secs(5),async {
        loop {
            let batch=observed.next_commit().await.expect("follow protocol").expect("retained Thread");
            if batch.changes.iter().any(|change| matches!(change,thread_event::Payload::Review(value) if value.explanation=="exact comparison")) {break;}
        }
    }).await.expect("post-commit review push");
    // Current field heads came from the same endpoint view and retain exact
    // original operations for independent devices, rather than projected authors.
    let replica = repo::thread_replication::ThreadReplica::open(
        repository.heddle_dir(),
        genesis.id().expect("Thread ID"),
    )
    .expect("replica");
    assert_eq!(
        replica
            .metadata_frontier(&Property::Name)
            .expect("name heads")
            .len(),
        1
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(1100),
            observed.next_commit()
        )
        .await
        .is_err(),
        "idle Thread emits no heartbeat"
    );
    drop(observed);
}
