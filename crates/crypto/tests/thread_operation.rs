use std::collections::BTreeSet;

use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use heddle_object_model::object::{
    Attribution, ContentHash, Principal, State, Tree,
    thread_replication::{AuthoredCapture, SourceAuthor, ThreadOperation, ThreadOperationBody},
};

#[test]
fn portable_thread_signatures_bind_publisher_and_canonical_operation() {
    let signer = Ed25519Signer::from_seed(&[41; 32]).expect("fixed test key");
    let other = Ed25519Signer::from_seed(&[42; 32]).expect("different test key");
    let state = State::new_snapshot(
        Tree::new().hash(),
        vec![],
        Attribution::human(Principal::new("Author", "author@example.test")),
    );
    let operation = ThreadOperation {
        version: 1,
        thread: ContentHash::from_bytes([1; 32]),
        parents: BTreeSet::new(),
        publisher: signer.public_key().try_into().expect("Ed25519 public key"),
        body: ThreadOperationBody::Capture(AuthoredCapture::local(
            state.encode_current_msgpack().expect("state").into(),
        )),
    };
    let signed = SignedOperation::sign(&operation, &signer).expect("signed operation");
    assert_eq!(signed.verify().expect("portable verification"), operation);
    assert!(SignedOperation::sign(&operation, &other).is_err());

    let mut changed = operation.clone();
    changed.thread = ContentHash::from_bytes([2; 32]);
    assert!(
        SignedOperation {
            canonical: changed.encode().expect("canonical changed thread"),
            ..signed.clone()
        }
        .verify()
        .is_err()
    );
    changed = operation;
    changed.publisher = other.public_key().try_into().expect("other Ed25519 key");
    assert!(
        SignedOperation {
            canonical: changed.encode().expect("canonical changed publisher"),
            ..signed.clone()
        }
        .verify()
        .is_err()
    );
    let mut unsigned_domain = signed;
    unsigned_domain.signature = signer
        .sign(&unsigned_domain.canonical)
        .expect("wrong domain signature");
    assert!(unsigned_domain.verify().is_err());
}

#[test]
fn capture_signature_binds_original_account_agent_and_authority() {
    use heddle_object_model::object::CollaborationActor;
    let signer = Ed25519Signer::from_seed(&[71; 32]).expect("source signer");
    let state = State::new_snapshot(
        Tree::new().hash(),
        vec![],
        Attribution::human(Principal::new("author", "author@example.test")),
    );
    let capture = AuthoredCapture::account(
        state.encode_current_msgpack().expect("source").into(),
        "00000000-0000-0000-0000-000000000001"
            .parse()
            .expect("Spool"),
        CollaborationActor {
            principal_id: "00000000-0000-0000-0000-000000000002"
                .parse()
                .expect("account"),
            agent_id: Some("original-agent".into()),
        },
        b"canonical authority tested independently at admission".to_vec(),
    )
    .expect("bound original author");
    let operation = ThreadOperation {
        version: 1,
        thread: ContentHash::from_bytes([3; 32]),
        parents: BTreeSet::new(),
        publisher: signer.public_key().try_into().expect("public key"),
        body: ThreadOperationBody::Capture(capture),
    };
    let signed = SignedOperation::sign(&operation, &signer).expect("original signature");
    assert_eq!(signed.verify().expect("verify original"), operation);
    for change in 0..4 {
        let mut substituted = operation.clone();
        let ThreadOperationBody::Capture(capture) = &mut substituted.body else {
            panic!("capture")
        };
        let SourceAuthor::Account {
            spool,
            actor,
            authority,
            authority_digest,
        } = &mut capture.author
        else {
            panic!("account author")
        };
        match change {
            0 => {
                actor.principal_id = "00000000-0000-0000-0000-000000000003"
                    .parse()
                    .expect("other account")
            }
            1 => actor.agent_id = Some("courier-agent".into()),
            2 => {
                *spool = "00000000-0000-0000-0000-000000000004"
                    .parse()
                    .expect("other Spool")
            }
            _ => {
                *authority = b"another internally consistent authority envelope".to_vec();
                *authority_digest = ContentHash::compute_typed(
                    heddle_object_model::object::thread_replication::metadata::AUTHORITY_FORMAT,
                    authority,
                );
            }
        }
        assert!(
            SignedOperation {
                canonical: substituted.encode().expect("canonical substitute"),
                signature: signed.signature.clone()
            }
            .verify()
            .is_err(),
            "courier cannot relabel original source author field {change}"
        );
    }
    assert_eq!(
        operation.source_state().expect("source projection"),
        Some(state)
    );
}

#[test]
fn retained_account_source_admission_requires_exact_original_and_pinned_executor() {
    use crypto::thread_authority_admission::SignedAuthorityAdmission;
    use heddle_object_model::object::{
        CollaborationActor, thread_authority_admission::ThreadAuthorityAdmission,
        thread_replication::integration::TrustedHostedExecutor,
    };
    let signer = Ed25519Signer::from_seed(&[72; 32]).expect("source key");
    let executor = Ed25519Signer::from_seed(&[73; 32]).expect("executor key");
    let spool = "00000000-0000-0000-0000-000000000005"
        .parse()
        .expect("Spool");
    let actor = CollaborationActor {
        principal_id: "00000000-0000-0000-0000-000000000006"
            .parse()
            .expect("account"),
        agent_id: Some("source-agent".into()),
    };
    let state = State::new_snapshot(
        Tree::new().hash(),
        vec![],
        Attribution::human(Principal::new("author", "author@example.test")),
    );
    let capture = AuthoredCapture::account(
        state.encode_current_msgpack().expect("State").into(),
        spool,
        actor.clone(),
        vec![8; 32],
    )
    .expect("original author");
    let SourceAuthor::Account {
        authority_digest, ..
    } = capture.author
    else {
        panic!("account")
    };
    let operation = ThreadOperation {
        version: 1,
        thread: ContentHash::from_bytes([7; 32]),
        parents: BTreeSet::new(),
        publisher: signer.public_key().try_into().expect("source key"),
        body: ThreadOperationBody::Capture(capture),
    };
    let original = SignedOperation::sign(&operation, &signer).expect("original");
    let trust = TrustedHostedExecutor {
        spool,
        spool_genesis: ContentHash::from_bytes([9; 32]),
        executor: executor.public_key().try_into().expect("executor key"),
    };
    let statement = ThreadAuthorityAdmission {
        version: 2,
        spool,
        spool_genesis: trust.spool_genesis,
        thread: operation.thread,
        subject: heddle_object_model::object::thread_authority_admission::OriginalAuthoritySubject::Operation(operation.id().expect("operation ID")),
        actor,
        publisher: operation.publisher,
        authority_digest,
        executor: trust.executor,
        admitted_at_ms: 1,
    };
    let receipt = SignedAuthorityAdmission::sign(&statement, &executor).expect("first admission");
    assert_eq!(
        receipt
            .verify(&original, &trust)
            .expect("retained source admission"),
        statement
    );
    let mut wrong_trust = trust.clone();
    wrong_trust.spool_genesis = ContentHash::from_bytes([10; 32]);
    assert!(
        receipt.verify(&original, &wrong_trust).is_err(),
        "carried executor key cannot replace independent trust"
    );
    let mut local = operation;
    let ThreadOperationBody::Capture(capture) = &mut local.body else {
        panic!("capture")
    };
    capture.author = SourceAuthor::LocalKey;
    let original_local = SignedOperation::sign(&local, &signer).expect("local key capture");
    let mut relabeled = statement;
    relabeled.subject = heddle_object_model::object::thread_authority_admission::OriginalAuthoritySubject::Operation(local.id().expect("local operation"));
    let false_account = SignedAuthorityAdmission::sign(&relabeled, &executor)
        .expect("signed but wrong author type");
    assert!(
        false_account.verify(&original_local, &trust).is_err(),
        "account receipt cannot silently claim a local-key capture"
    );
}
