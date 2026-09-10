use crypto::{
    Ed25519Signer, Signer, thread_authority_admission::SignedAuthorityAdmission,
    thread_operation::SignedOperation,
};
use heddle_object_model::object::{
    Attribution, CollaborationActor, Principal, State, Tree,
    original_boundary_acceptance::{BoundaryOriginalKind, OriginalBoundaryAcceptance},
    thread_authority_admission::{OriginalAuthoritySubject, ThreadAuthorityAdmission},
    thread_replication::{
        AuthoredCapture, SourceAuthor, ThreadOperation, ThreadOperationBody,
        integration::TrustedHostedExecutor,
    },
};

use super::*;
fn key(n: u8) -> Ed25519Signer {
    Ed25519Signer::from_seed(&[n; 32]).expect("key")
}
fn fixture(count: usize) -> (wire::ReplicationOperations, TrustedHostedExecutor) {
    let original = key(51);
    let acceptor = key(52);
    let executor = key(53);
    let spool = uuid::Uuid::from_u128(501);
    let account = uuid::Uuid::from_u128(502);
    let author = SourceAuthor::account(
        spool,
        CollaborationActor {
            principal_id: account,
            agent_id: Some("old-revoked-author".into()),
        },
        vec![8],
    )
    .expect("original provenance");
    let SourceAuthor::Account {
        actor,
        authority_digest,
        ..
    } = &author
    else {
        panic!("account")
    };
    let value = OriginalBoundaryAcceptance {
        version: 1,
        publication_intent: ContentHash::compute(b"original destination-bound publication"),
        originals_manifest: ContentHash::compute(
            b"complete original manifest, not disclosed to subset receiver",
        ),
        original_account: account,
        kinds: [BoundaryOriginalKind::Source].into(),
        accepting_publisher: acceptor.public_key().try_into().expect("key"),
        accepting_author: SourceAuthor::account(
            spool,
            CollaborationActor {
                principal_id: account,
                agent_id: Some("separate-explicit-acceptor".into()),
            },
            vec![77; 64 * 1024],
        )
        .expect("acceptance"),
    };
    let acceptance =
        SignedBoundaryAcceptance::sign(&value, &acceptor).expect("acceptance signature");
    let trust = TrustedHostedExecutor {
        spool,
        spool_genesis: ContentHash::compute(b"independently pinned Spool"),
        executor: executor.public_key().try_into().expect("key"),
    };
    let mut batch = wire::ReplicationOperations {
        boundary_acceptances: vec![encode(&acceptance).expect("wire acceptance")],
        ..Default::default()
    };
    for n in 0..count {
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![],
            Attribution::human(Principal::new(format!("offline {n}"), "")),
        );
        let operation = ThreadOperation {
            version: 1,
            thread: ContentHash::compute(b"original Thread"),
            parents: Default::default(),
            publisher: original.public_key().try_into().expect("key"),
            body: ThreadOperationBody::Capture(AuthoredCapture {
                result: state.encode_current_msgpack().expect("state").into(),
                author: author.clone(),
            }),
        };
        let signed = SignedOperation::sign(&operation, &original).expect("unchanged original");
        let receipt = ThreadAuthorityAdmission {
            version: 3,
            basis: AdmissionBasis::BoundaryAcceptance {
                acceptance: value.id().expect("acceptance ID"),
            },
            spool,
            spool_genesis: trust.spool_genesis,
            thread: operation.thread,
            subject: OriginalAuthoritySubject::Operation(operation.id().expect("ID")),
            actor: actor.clone(),
            publisher: operation.publisher,
            authority_digest: *authority_digest,
            executor: trust.executor,
            admitted_at_ms: 9000,
        };
        batch.operations.push(wire::SignedRecord {
            format: heddle_object_model::object::thread_replication::OPERATION_FORMAT.into(),
            canonical_record: signed.canonical,
            signatures: vec![wire::RecordSignature {
                public_key: operation.publisher.to_vec(),
                signature: signed.signature,
            }],
        });
        batch.authority_admissions.push(
            crate::authority_admission::encode(
                &SignedAuthorityAdmission::sign(&receipt, &executor).expect("receipt"),
            )
            .expect("wire receipt"),
        );
    }
    (batch, trust)
}
#[test]
fn boundary_batch_matches_shared_evidence_once_and_rejects_missing_or_unreferenced() {
    let (batch, trust) = fixture(128);
    let matched =
        crate::authority_admission::match_batch(&batch).expect("exact128 original receipt bundle");
    let first = matched[0]
        .authority_admission
        .as_ref()
        .expect("receipt")
        .boundary_acceptance
        .as_ref()
        .expect("evidence");
    assert_eq!(first.canonical.len() > 64 * 1024, true);
    for received in &matched {
        let receipt = received.authority_admission.as_ref().expect("receipt");
        assert!(
            std::sync::Arc::ptr_eq(
                first,
                receipt
                    .boundary_acceptance
                    .as_ref()
                    .expect("shared evidence")
            ),
            "one manifest acceptance must not be cloned per receipt"
        );
        receipt
            .verify(&received.original, &trust)
            .expect("independent receiver pin");
    }
    let mut missing = batch.clone();
    missing.boundary_acceptances.clear();
    assert!(
        crate::authority_admission::match_batch(&missing)
            .err()
            .expect("missing evidence")
            .to_string()
            .contains("missing matched")
    );
    let mut duplicate = batch.clone();
    duplicate
        .boundary_acceptances
        .push(batch.boundary_acceptances[0].clone());
    assert!(crate::authority_admission::match_batch(&duplicate).is_err());
    let mut unmatched = batch.clone();
    unmatched.authority_admissions.clear();
    assert!(
        crate::authority_admission::match_batch(&unmatched)
            .err()
            .expect("unreferenced evidence")
            .to_string()
            .contains("unreferenced")
    );
    let mut unknown = trust;
    unknown.executor = [55; 32];
    assert!(
        matched[0]
            .authority_admission
            .as_ref()
            .expect("receipt")
            .verify(&matched[0].original, &unknown)
            .is_err(),
        "structural matching never pins executor"
    );
    let mut damaged = batch;
    damaged.boundary_acceptances[0].signatures[0].signature[0] ^= 1;
    assert!(crate::authority_admission::match_batch(&damaged).is_err());
}
#[test]
fn boundary_receipt_cannot_relabel_original_scope_or_use_old_authorize_path() {
    let (batch, trust) = fixture(1);
    let received = crate::authority_admission::match_batch(&batch)
        .expect("match")
        .remove(0);
    let receipt = received.authority_admission.expect("receipt");
    let statement = receipt.verify_signature().expect("signature");
    let original = received.original.verify().expect("original");
    assert!(
        statement.authorize(&original, &trust).is_err(),
        "signature-only receipt cannot bypass required acceptance"
    );
    for mutation in 0..4 {
        let mut value = receipt
            .boundary_acceptance
            .as_ref()
            .expect("acceptance")
            .verify_signature()
            .expect("acceptance value");
        match mutation {
            0 => {
                value.kinds = [BoundaryOriginalKind::OwnershipClaim].into();
            }
            1 => {
                value.original_account = uuid::Uuid::from_u128(999);
                if let SourceAuthor::Account { actor, .. } = &mut value.accepting_author {
                    actor.principal_id = value.original_account;
                }
            }
            2 => {
                if let SourceAuthor::Account { spool, .. } = &mut value.accepting_author {
                    *spool = uuid::Uuid::from_u128(999);
                }
            }
            _ => {
                value.publication_intent = ContentHash::compute(b"different intent");
            }
        }
        let changed =
            SignedBoundaryAcceptance::sign(&value, &key(52)).expect("changed signed evidence");
        let mut candidate = receipt.clone();
        candidate.boundary_acceptance = Some(std::sync::Arc::new(changed));
        if mutation < 3 {
            let mut statement = statement.clone();
            statement.basis = AdmissionBasis::BoundaryAcceptance {
                acceptance: value.id().expect("newdigest"),
            };
            candidate = SignedAuthorityAdmission::sign(&statement, &key(53))
                .expect("receipt claiming wrong scope");
            candidate.boundary_acceptance = Some(std::sync::Arc::new(
                SignedBoundaryAcceptance::sign(&value, &key(52)).expect("signature"),
            ));
        }
        assert!(
            candidate.verify(&received.original, &trust).is_err(),
            "wrong kind/account/Spool or unmatched signed digest is not original authority"
        );
    }
    let mut old = statement.clone();
    old.basis = AdmissionBasis::OriginalAuthority;
    let mut old = SignedAuthorityAdmission::sign(&old, &key(53)).expect("original basis");
    old.boundary_acceptance = receipt.boundary_acceptance;
    assert!(
        old.verify(&received.original, &trust).is_err(),
        "stray evidence must not upgrade original-author testimony"
    );
    let mut old_version = statement;
    old_version.version = 2;
    assert!(
        old_version.encode().is_err(),
        "old format cannot silently gain a basis"
    );
}

#[cfg(feature = "native")]
#[tokio::test]
async fn boundary_native_two_receiver_relay_preserves_exact_testimony_without_credential() {
    use std::sync::Arc;

    use crypto::{
        thread_genesis_admission::SignedGenesisAdmission, thread_operation::SignedGenesis,
    };
    use heddle_object_model::object::{
        thread_genesis_admission::{ENVELOPE_FORMAT, ThreadGenesisAdmission},
        thread_replication::{
            GenesisOwner, ThreadFacet, ThreadGenesis, integration::SPOOL_GENESIS_TRUST_FORMAT,
        },
    };
    use prost::Message;
    use repo::{Repository, thread_replication::ThreadReplica};

    use crate::replication::{Frame, Session, native::LocalReplica};
    let paths = [
        tempfile::tempdir().expect("sender"),
        tempfile::tempdir().expect("receiver"),
        tempfile::tempdir().expect("next receiver"),
    ];
    let repositories = paths
        .iter()
        .map(|path| Repository::init_default(path.path()).expect("repository"))
        .collect::<Vec<_>>();
    let (mut batch, mut trust) = fixture(1);
    let account = uuid::Uuid::from_u128(502);
    let spool_genesis = repo::sign_spool_owner_genesis(&key(54), *trust.spool.as_bytes())
        .expect("independent Spool root");
    trust.spool_genesis = ContentHash::compute_typed(
        SPOOL_GENESIS_TRUST_FORMAT,
        &spool_genesis
            .genesis
            .as_ref()
            .expect("genesis")
            .encode_to_vec(),
    );
    let genesis = ThreadGenesis {
        version: 1,
        spool: trust.spool.to_string(),
        parent: None,
        base: repositories[0].head().expect("head").expect("base"),
        name: "historical boundary relay".into(),
        intent: "no original live bearer".into(),
        creator: key(51).public_key().try_into().expect("key"),
        owner: GenesisOwner::Account(account),
        nonce: vec![1],
    };
    let original_genesis = SignedGenesis::sign(&genesis, &key(51)).expect("genesis");
    let genesis_receipt = ThreadGenesisAdmission {
        version: 2,
        basis: AdmissionBasis::OriginalAuthority,
        spool: trust.spool,
        spool_genesis: trust.spool_genesis,
        thread: genesis.id().expect("id"),
        owner: account,
        creator: genesis.creator,
        authority_digest: ContentHash::compute_typed(
            ENVELOPE_FORMAT,
            b"retained original genesis envelope",
        ),
        executor: trust.executor,
        admitted_at_ms: 8000,
    };
    let genesis_receipt = SignedGenesisAdmission::sign(&genesis_receipt, &key(53))
        .expect("pinned executor testimony");
    let replicas = repositories
        .iter()
        .map(|repository| {
            let replica = ThreadReplica::create_from_genesis_admission(
                repository.heddle_dir(),
                &original_genesis,
                b"retained original genesis envelope",
                &genesis_receipt,
                &trust,
            )
            .expect("independent genesis pin");
            repository
                .verify_and_pin_owner_genesis(
                    2,
                    Some(&spool_genesis),
                    &["selected".into(), "shared".into()],
                )
                .expect("independent trust setup");
            repository
                .pin_thread_hosted_executor(&replica, trust.executor)
                .expect("independent executor pin");
            replica
                .set_sharing([86; 32], &[ThreadFacet::Source].into())
                .expect("explicit export consent");
            replica
        })
        .collect::<Vec<_>>();
    let mut original = crate::replication::decode_record(batch.operations.remove(0))
        .expect("original")
        .verify()
        .expect("operation");
    original.thread = genesis.id().expect("Thread");
    if let ThreadOperationBody::Capture(capture) = &mut original.body {
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![genesis.base],
            Attribution::human(Principal::new("offline work", "")),
        );
        capture.result.state = state.encode_current_msgpack().expect("state");
    }
    let signed = SignedOperation::sign(&original, &key(51)).expect("original signature");
    let id = original.id().expect("ID");
    let mut statement =
        crate::authority_admission::verify_signature(&batch.authority_admissions[0])
            .expect("receipt");
    statement.thread = original.thread;
    statement.subject = OriginalAuthoritySubject::Operation(id);
    statement.spool_genesis = trust.spool_genesis;
    let mut receipt = SignedAuthorityAdmission::sign(&statement, &key(53)).expect("pinned receipt");
    receipt.boundary_acceptance = Some(Arc::new(
        decode(&batch.boundary_acceptances[0]).expect("evidence"),
    ));
    replicas[0]
        .receive_with_authority_admission(&signed, &receipt, repositories[0].store(), |_| Ok(()))
        .expect("original pinned admission");
    let session = |index: usize| {
        Session::new(
            LocalReplica::new(
                replicas[index].clone(),
                Arc::new(repositories[index].store().clone()),
            ),
            [86; 32],
            [ThreadFacet::Source].into(),
            8,
        )
        .expect("exact source session")
    };
    let mut exported = session(0)
        .export_operation(id)
        .await
        .expect("export evidence");
    for index in 1..3 {
        let Frame::Operations(batch) = exported else {
            panic!("originals")
        };
        assert_eq!(batch.boundary_acceptances.len(), 1);
        let mut missing = batch.clone();
        missing.boundary_acceptances.clear();
        let mut receiver = session(index);
        assert!(
            receiver.handle(Frame::Operations(missing)).await.is_err(),
            "no evidence cannot authorize new receiver"
        );
        assert!(replicas[index].operation(&id).expect("unchanged").is_none());
        receiver
            .handle(Frame::Operations(batch))
            .await
            .expect("actual native receive");
        let reopened =
            ThreadReplica::open(repositories[index].heddle_dir(), genesis.id().expect("ID"))
                .expect("reopen");
        let retained = reopened
            .operation_with_authority_admission(&id)
            .expect("lookup")
            .expect("original");
        assert_eq!(retained.original, signed);
        assert_eq!(retained.authority_admission, Some(receipt.clone()));
        exported = session(index)
            .export_operation(id)
            .await
            .expect("relay exact acceptance");
    }
}

#[test]
fn boundary_receipt_versions_keep_maximum_basis_inside_storage_bound() {
    use heddle_object_model::object::thread_genesis_admission::ThreadGenesisAdmission;
    let (batch, trust) = fixture(1);
    let mut operation =
        crate::authority_admission::verify_signature(&batch.authority_admissions[0])
            .expect("receipt");
    operation.actor.agent_id = Some("x".repeat(256));
    let bytes = operation.encode().expect("maximum actor and basis");
    assert!(bytes.len() <= heddle_object_model::object::thread_authority_admission::MAX_BYTES);
    assert_eq!(
        ThreadAuthorityAdmission::decode(&bytes).expect("canonical roundtrip"),
        operation
    );
    let genesis = ThreadGenesisAdmission {
        version: 2,
        basis: operation.basis.clone(),
        spool: trust.spool,
        spool_genesis: trust.spool_genesis,
        thread: operation.thread,
        owner: operation.actor.principal_id,
        creator: operation.publisher,
        authority_digest: operation.authority_digest,
        executor: trust.executor,
        admitted_at_ms: i64::MAX,
    };
    let bytes = genesis.encode().expect("genesis basis");
    assert!(bytes.len() <= heddle_object_model::object::thread_genesis_admission::MAX_BYTES);
    assert_eq!(
        ThreadGenesisAdmission::decode(&bytes).expect("genesis canonical"),
        genesis
    );
    let mut old = genesis;
    old.version = 1;
    assert!(
        old.encode().is_err(),
        "old genesis receipt has no new basis interpretation"
    );
}

#[test]
fn boundary_acceptance_signature_is_required_before_matching_receipts() {
    let (mut batch,_)=fixture(1);
    batch.boundary_acceptances[0].signatures[0].signature[0]^=1;
    assert!(crate::authority_admission::match_batch(&batch).is_err(),"acceptance signature must be verified before receipt matching");
}
#[test]
fn boundary_acceptance_receipt_requires_independent_executor_pin() {
    let (batch,mut trust)=fixture(1);
    let received=crate::authority_admission::match_batch(&batch).expect("structural match").remove(0);
    trust.executor=[88;32];
    assert!(received.authority_admission.expect("receipt").verify(&received.original,&trust).is_err(),"boundary evidence must never establish its own executor trust");
}
