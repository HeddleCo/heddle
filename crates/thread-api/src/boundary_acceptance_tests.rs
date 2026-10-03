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
    assert!(first.canonical.len() > 64 * 1024);
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
            .expect_err("missing evidence")
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
            .expect_err("unreferenced evidence")
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
#[test]
fn boundary_native_receivers_fail_closed_pending_api318() {
    let (batch, trust) = fixture(1);
    let received = crate::authority_admission::match_batch(&batch)
        .expect("exact original/acceptance/receipt control")
        .remove(0);
    let receipt = received.authority_admission.expect("boundary receipt");
    receipt
        .verify(&received.original, &trust)
        .expect("original scope, acceptance and independent executor comparisons remain");
    for _ in 0..2 {
        let directory = tempfile::tempdir().expect("receiver");
        let repository = repo::Repository::init_default(directory.path()).expect("repository");
        let replica = repository.native_thread("main").expect("receiver Thread");
        assert!(matches!(
            replica.authority_admission_trust(&receipt),
            Err(repo::thread_replication::Error::WitnessEvidenceRequired)
        ));
        let id = received
            .original
            .verify()
            .expect("original")
            .id()
            .expect("id");
        assert!(replica.operation(&id).expect("no installation").is_none());
        let reopened = repo::thread_replication::ThreadReplica::open(
            repository.heddle_dir(),
            replica.thread_id(),
        )
        .expect("restart");
        assert!(matches!(
            reopened.authority_admission_trust(&receipt),
            Err(repo::thread_replication::Error::WitnessEvidenceRequired)
        ));
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
    let (mut batch, _) = fixture(1);
    batch.boundary_acceptances[0].signatures[0].signature[0] ^= 1;
    assert!(
        crate::authority_admission::match_batch(&batch).is_err(),
        "acceptance signature must be verified before receipt matching"
    );
}
#[test]
fn boundary_acceptance_receipt_requires_independent_executor_pin() {
    let (batch, mut trust) = fixture(1);
    let received = crate::authority_admission::match_batch(&batch)
        .expect("structural match")
        .remove(0);
    trust.executor = [88; 32];
    assert!(
        received
            .authority_admission
            .expect("receipt")
            .verify(&received.original, &trust)
            .is_err(),
        "boundary evidence must never establish its own executor trust"
    );
}
#[test]
fn outgoing_batches_share_evidence_within_each_independently_verifiable_carrier() {
    use prost::Message;
    let (batch, _) = fixture(128);
    let received = crate::authority_admission::match_batch(&batch).expect("matched originals");
    let output = crate::authority_admission::batches(received, 256 * 1024, 64)
        .expect("bounded encoder")
        .collect::<Result<Vec<_>, _>>()
        .expect("carriers");
    assert_eq!(output.len(), 2);
    for carrier in &output {
        assert_eq!(carrier.operations.len(), 64);
        assert_eq!(
            carrier.boundary_acceptances.len(),
            1,
            "one shared acceptance per carrier"
        );
        assert!(carrier.encoded_len() <= 256 * 1024);
        assert_eq!(
            crate::authority_admission::match_batch(carrier)
                .expect("no prior frame state required")
                .len(),
            64
        );
    }
    let wire_bytes: usize = output.iter().map(Message::encoded_len).sum();
    assert!(
        wire_bytes < 512 * 1024,
        "128 references do not serialize 128 copies of 64 KiB proof"
    );
    let received = crate::authority_admission::match_batch(&batch).expect("originals");
    let observed = std::cell::Cell::new(0);
    let input = received
        .into_iter()
        .inspect(|_| observed.set(observed.get() + 1));
    let mut batches =
        crate::authority_admission::batches(input, 256 * 1024, 2).expect("lazy bounded encoder");
    assert_eq!(
        batches
            .next()
            .expect("carrier")
            .expect("bounded")
            .operations
            .len(),
        2
    );
    assert_eq!(
        observed.get(),
        2,
        "sender does not materialize complete ancestry before first frame"
    );
}
#[test]
fn outgoing_batches_fail_closed_when_one_complete_proof_exceeds_budget() {
    let (batch, _) = fixture(1);
    let received = crate::authority_admission::match_batch(&batch).expect("originals");
    let mut output = crate::authority_admission::batches(received, 1024, 64).expect("limits");
    let error = output
        .next()
        .expect("bounded error")
        .expect_err("complete proof does not fit");
    assert!(
        matches!(error, crate::transport::Error::OriginalOperationTooLarge {
        operation: 1, bytes, limit: 1024,
    } if bytes > 1024)
    );
    assert!(output.next().is_none(), "encoder is fused after denial");
}

#[test]
fn outgoing_batches_reject_same_canonical_evidence_with_different_signature() {
    let (batch, _) = fixture(2);
    let mut received = crate::authority_admission::match_batch(&batch).expect("matched originals");
    let changed = received[1]
        .authority_admission
        .as_mut()
        .expect("receipt")
        .boundary_acceptance
        .as_mut()
        .expect("matched evidence");
    std::sync::Arc::make_mut(changed).signature[0] ^= 1;
    let mut output =
        crate::authority_admission::batches(received, 256 * 1024, 64).expect("bounded encoder");
    let error = output.next().expect("explicit rejection").expect_err(
        "different retained signature must not be silently replaced by the first evidence",
    );
    assert!(
        error
            .to_string()
            .contains("conflicting boundary acceptance evidence in outgoing batch"),
        "{error}"
    );
    assert!(
        output.next().is_none(),
        "no partially normalized carrier after rejection"
    );
}
