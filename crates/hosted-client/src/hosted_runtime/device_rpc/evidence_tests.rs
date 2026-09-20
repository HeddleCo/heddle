//! Real Iroh evidence admission, immutable replay and current-head push projection.
use crypto::{Ed25519Signer, Signer};
use objects::object::{
    CollaborationActor, ContentHash, thread_replication::metadata::AUTHORITY_FORMAT,
};
use thread_api::evidence::{
    self as codec, CheckAcknowledgement, CheckAuthor, CheckEvidence, CheckOutcome,
};

use super::*;

pub(super) async fn roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    device: &DeviceRpc,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    spool: uuid::Uuid,
) {
    let signer = Ed25519Signer::from_seed(&[71; 32]).expect("owner signer");
    let now = chrono::Utc::now().timestamp();
    let authority = repo::device_authority::load(&device.home, now).expect("pinned owner");
    let minted = crate::hosted_runtime::root_mint::mint_agent_root(&[71; 32]).expect("root token");
    let public =
        biscuit_auth::PublicKey::from_bytes(signer.public_key(), biscuit_auth::Algorithm::Ed25519)
            .expect("public key");
    let token = biscuit_auth::Biscuit::from_base64(minted.token, public).expect("token");
    let envelope = repo::thread_replication::metadata::prepare_control_authority(
        &authority,
        &signer.public_key().try_into().expect("publisher"),
        &token,
        now,
    )
    .expect("original author envelope");
    let author = CheckAuthor {
        actor: CollaborationActor {
            principal_id: uuid::Uuid::from_bytes([9; 16]),
            agent_id: None,
        },
        publisher: signer.public_key().try_into().expect("publisher"),
        authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, &envelope),
        authority_envelope: envelope,
    };
    let value = CheckEvidence {
        version: 2,
        thread: replica.thread_id(),
        id: uuid::Uuid::new_v4(),
        spool,
        revision: replica.genesis().expect("genesis").base,
        check: "unit-tests".into(),
        outcome: CheckOutcome::Failed,
        detail: "One assertion needs attention".into(),
        artifacts: vec![],
        supersedes: vec![],
        author: author.clone(),
        completed_at_ms: now * 1000,
    };
    let record = codec::sign_evidence(&value, &signer).expect("signed result");
    let projection = codec::project(&record).expect("evidence projection");
    let request = RecordEvidenceRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        evidence: Some(projection.clone()),
    };
    let response = remote
        .api
        .call::<thread_api::rpc::EvidenceServiceRecordEvidence>(&request)
        .await
        .expect("record failed check outcome");
    assert_eq!(
        response,
        remote
            .api
            .call::<thread_api::rpc::EvidenceServiceRecordEvidence>(&request)
            .await
            .expect("exact receipt replay")
    );
    let verified = remote
        .api
        .call::<thread_api::rpc::EvidenceServiceVerifyEvidence>(&VerifyEvidenceRequest {
            evidence: vec![projection.clone()],
        })
        .await
        .expect("verify admitted evidence");
    assert_eq!(verified.results.len(), 1);
    assert!(verified.results[0].verified);
    let mut substituted = request.clone();
    substituted.client_operation_id = uuid::Uuid::new_v4().to_string();
    substituted.evidence.as_mut().expect("evidence").check = "unsigned replacement".into();
    assert!(
        remote
            .api
            .call::<thread_api::rpc::EvidenceServiceRecordEvidence>(&substituted)
            .await
            .is_err(),
        "display fields cannot replace signed evidence check"
    );
    let foreign = Ed25519Signer::from_seed(&[81; 32]).expect("other original signer");
    let mut foreign_value = value.clone();
    foreign_value.id = uuid::Uuid::new_v4();
    foreign_value.author.publisher = foreign.public_key().try_into().expect("publisher");
    let foreign_record =
        codec::sign_evidence(&foreign_value, &foreign).expect("valid foreign signature");
    let foreign_projection = codec::project(&foreign_record).expect("valid original projection");
    let verified = remote
        .api
        .call::<thread_api::rpc::EvidenceServiceVerifyEvidence>(&VerifyEvidenceRequest {
            evidence: vec![foreign_projection.clone()],
        })
        .await
        .expect("independent original verification");
    assert!(
        !verified.results[0].verified,
        "delivery owner does not authorize another original publisher"
    );
    assert!(
        remote
            .api
            .call::<thread_api::rpc::EvidenceServiceRecordEvidence>(&RecordEvidenceRequest {
                client_operation_id: uuid::Uuid::new_v4().to_string(),
                evidence: Some(foreign_projection)
            })
            .await
            .is_err(),
        "fresh evidence requires independent original-author admission"
    );
    // Identical source in another Thread does not transfer the authored result's audience.
    let stranger = Ed25519Signer::from_seed(&[114; 32]).expect("other device key");
    let foreign_genesis = objects::object::thread_replication::ThreadGenesis {
        owner: objects::object::thread_replication::GenesisOwner::LocalKey(
            stranger.public_key().try_into().expect("key"),
        ),
        creator: stranger.public_key().try_into().expect("key"),
        name: "private evidence origin".into(),
        nonce: vec![114],
        ..replica.genesis().expect("source genesis")
    };
    let signed = crypto::thread_operation::SignedGenesis::sign(&foreign_genesis, &stranger)
        .expect("foreign original genesis");
    let private = repo::thread_replication::ThreadReplica::create(repository.heddle_dir(), &signed)
        .expect("actual private origin");
    let mut private_value = value.clone();
    private_value.id = uuid::Uuid::new_v4();
    private_value.thread = private.thread_id();
    let private_record =
        codec::sign_evidence(&private_value, &signer).expect("valid owner-authored evidence");
    let private_projection = codec::project(&private_record).expect("private projection");
    assert!(
        remote
            .api
            .call::<thread_api::rpc::EvidenceServiceRecordEvidence>(&RecordEvidenceRequest {
                client_operation_id: uuid::Uuid::new_v4().to_string(),
                evidence: Some(private_projection.clone())
            })
            .await
            .is_err(),
        "accessible identical State cannot authorize a private evidence origin"
    );
    assert!(
        remote
            .api
            .call::<thread_api::rpc::EvidenceServiceVerifyEvidence>(&VerifyEvidenceRequest {
                evidence: vec![private_projection]
            })
            .await
            .is_err(),
        "verification cannot broaden the signed origin Thread audience"
    );
    let query = ObserveThreadRequest {
        thread: Some(ThreadRef {
            spool: Some(SpoolRef {
                id: spool.to_string(),
            }),
            id: Some(ThreadId {
                value: replica.thread_id().as_bytes().to_vec(),
            }),
        }),
        sections: vec![ThreadSection::Evidence as i32],
        observe: Some(ObserveOptions {
            mode: ObservationMode::Follow as i32,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut view = remote
        .api
        .observe::<thread_api::rpc::ThreadServiceObserveThread>(&query)
        .await
        .expect("evidence view");
    let mut found = false;
    loop {
        let event = view
            .next()
            .await
            .expect("snapshot frame")
            .expect("open view");
        if let Some(thread_event::Payload::Evidence(evidence)) = event.payload {
            assert_eq!(evidence.r#ref, projection.r#ref);
            found = true;
        }
        if matches!(
            event.frame.and_then(|f| f.body),
            Some(stream_frame::Body::Checkpoint(_))
        ) {
            break;
        }
    }
    assert!(
        found,
        "Thread evidence is projected from exact current source"
    );
    let ack = CheckAcknowledgement {
        version: 1,
        spool,
        evidence: value.id,
        evidence_digest: value.id().expect("digest"),
        revision: value.revision,
        policy_version: super::land::policy_version(repository).expect("current policy"),
        author,
        client_operation_id: uuid::Uuid::new_v4(),
        occurred_at_ms: now * 1000,
    };
    let request = AcknowledgeCheckRequest {
        client_operation_id: ack.client_operation_id.to_string(),
        evidence: projection.r#ref.clone(),
        revision: projection.revision.clone(),
        policy_version: ack.policy_version.as_bytes().to_vec(),
        acknowledgement: Some(
            codec::sign_acknowledgement(&ack, &signer).expect("signed acknowledgement"),
        ),
    };
    remote
        .api
        .call::<thread_api::rpc::EvidenceServiceAcknowledgeCheck>(&request)
        .await
        .expect("acknowledge failed outcome");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = view
                .next()
                .await
                .expect("committed evidence update")
                .expect("retained view");
            if let Some(thread_event::Payload::CheckAcknowledgement(value)) = event.payload {
                assert_eq!(value.evidence, projection.r#ref);
                break;
            }
        }
    })
    .await
    .expect("acknowledgement pushes without polling");
    drop(view);
    let stored = repo::device_evidence::get(repository.heddle_dir(), 1, value.id)
        .expect("stored original")
        .expect("evidence");
    assert_eq!(
        codec::verify_evidence(&SignedRecord::decode(stored.as_slice()).expect("wire original"))
            .expect("canonical original")
            .outcome,
        CheckOutcome::Failed,
        "acknowledgement cannot rewrite check outcome"
    );
    let mut wrong = ack.clone();
    wrong.client_operation_id = uuid::Uuid::new_v4();
    wrong.evidence_digest = ContentHash::from_bytes([42; 32]);
    let wrong_request = AcknowledgeCheckRequest {
        client_operation_id: wrong.client_operation_id.to_string(),
        acknowledgement: Some(
            codec::sign_acknowledgement(&wrong, &signer)
                .expect("wrong digest signed by real owner"),
        ),
        ..request
    };
    assert!(
        remote
            .api
            .call::<thread_api::rpc::EvidenceServiceAcknowledgeCheck>(&wrong_request)
            .await
            .is_err(),
        "acknowledgement binds exact admitted evidence digest"
    );
}
