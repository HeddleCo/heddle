use std::collections::BTreeSet;

use crypto::{
    Ed25519Signer, Signer as _,
    thread_operation::{SignedGenesis, SignedOperation},
};
use objects::object::{
    CollaborationActor, ContentHash,
    thread_replication::{
        ThreadGenesis, ThreadOperation, ThreadOperationBody,
        metadata::{AUTHORITY_FORMAT, Control, Lifecycle, Property, ThreadControl},
    },
};

use super::*;

#[test]
fn causal_property_heads_converge_without_overwriting_parallel_fields_and_cas_is_atomic() {
    let a = tempfile::tempdir().expect("replica a");
    let b = tempfile::tempdir().expect("replica b");
    let repository = crate::Repository::init_default(a.path()).expect("repository a");
    let other_repository = crate::Repository::init_default(b.path()).expect("repository b");
    let signer = Ed25519Signer::from_seed(&[19; 32]).expect("publisher");
    let genesis = ThreadGenesis {
        version: 1,
        spool: uuid::Uuid::from_u128(1).to_string(),
        parent: None,
        base: repository.head().expect("HEAD").expect("base"),
        name: "original".into(),
        intent: "multi-device".into(),
        creator: signer.public_key().try_into().expect("key"),
        owner: objects::object::thread_replication::GenesisOwner::LocalKey(
            signer.public_key().try_into().expect("key"),
        ),
        nonce: vec![],
    };
    let signed = SignedGenesis::sign(&genesis, &signer).expect("genesis");
    let left = ThreadReplica::create(repository.heddle_dir(), &signed).expect("left");
    let right = ThreadReplica::create(other_repository.heddle_dir(), &signed).expect("right");
    let make = |command: u128, control: Control, parents: BTreeSet<ContentHash>| {
        let authority = b"causal store fixture; authority checked by admission callback";
        let value = ThreadControl {
            version: 1,
            spool: uuid::Uuid::from_u128(1),
            actor: CollaborationActor {
                principal_id: uuid::Uuid::from_u128(2),
                agent_id: None,
            },
            authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, authority),
            authority_envelope: authority.to_vec(),
            client_operation_id: uuid::Uuid::from_u128(command),
            occurred_at_ms: 1000,
            control,
        };
        SignedOperation::sign(
            &ThreadOperation {
                version: 1,
                thread: left.thread_id(),
                publisher: signer.public_key().try_into().expect("key"),
                parents,
                body: ThreadOperationBody::Metadata(value.encode().expect("control")),
            },
            &signer,
        )
        .expect("signed operation")
    };
    let first = make(3, Control::Name("first".into()), BTreeSet::new());
    let first_id = first.verify().expect("proof").id().expect("ID");
    let second = make(4, Control::Name("second".into()), BTreeSet::new());
    let second_id = second.verify().expect("proof").id().expect("ID");
    for operation in [&first, &second] {
        assert_eq!(
            left.receive(operation, repository.store(), |_| Ok(()))
                .expect("left admission"),
            Admission::Accepted
        );
    }
    for operation in [&second, &first] {
        assert_eq!(
            right
                .receive(operation, other_repository.store(), |_| Ok(()))
                .expect("right admission"),
            Admission::Accepted
        );
    }
    let ids = |replica: &ThreadReplica, property: &Property| {
        replica
            .metadata_frontier(property)
            .expect("indexed frontier")
            .into_iter()
            .map(|(id, _)| id)
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(
        ids(&left, &Property::Name),
        BTreeSet::from([first_id, second_id])
    );
    assert_eq!(ids(&left, &Property::Name), ids(&right, &Property::Name));
    let lifecycle = make(5, Control::Lifecycle(Lifecycle::Active), BTreeSet::new());
    assert_eq!(
        left.receive_control_cas(&lifecycle, repository.store(), |_| Ok(()))
            .expect("independent property write"),
        Admission::Accepted
    );
    let descendant = make(
        6,
        Control::Name("branch successor".into()),
        BTreeSet::from([first_id]),
    );
    let descendant_id = descendant.verify().expect("proof").id().expect("ID");
    let generation = left.generation().expect("generation");
    assert!(
        left.receive_control_cas(&descendant, repository.store(), |_| Ok(()))
            .expect_err("must observe both name candidates")
            .to_string()
            .contains("frontier changed")
    );
    assert_eq!(left.generation().expect("unchanged generation"), generation);
    assert!(
        left.operation(&descendant_id).expect("lookup").is_none(),
        "failed CAS installs no operation or command receipt"
    );
    left.receive(&descendant, repository.store(), |_| Ok(()))
        .expect("historical concurrent branch");
    assert_eq!(
        ids(&left, &Property::Name),
        BTreeSet::from([descendant_id, second_id])
    );
    let resolution = make(
        7,
        Control::Name("resolved".into()),
        BTreeSet::from([descendant_id, second_id]),
    );
    let resolution_id = resolution.verify().expect("proof").id().expect("ID");
    assert_eq!(
        left.receive_control_cas(&resolution, repository.store(), |_| Ok(()))
            .expect("explicit resolution"),
        Admission::Accepted
    );
    assert_eq!(
        right
            .receive(&resolution, other_repository.store(), |_| Ok(()))
            .expect("out of order"),
        Admission::Pending
    );
    right
        .receive(&descendant, other_repository.store(), |_| Ok(()))
        .expect("missing parent arrives");
    assert_eq!(ids(&left, &Property::Name), BTreeSet::from([resolution_id]));
    assert_eq!(ids(&right, &Property::Name), ids(&left, &Property::Name));
    assert_eq!(
        ids(&left, &Property::Lifecycle).len(),
        1,
        "resolving name cannot erase lifecycle"
    );
    left.receive_control_cas(&resolution, repository.store(), |_| Ok(()))
        .expect("exact retry");
    let changed = make(
        7,
        Control::Name("changed retry".into()),
        BTreeSet::from([resolution_id]),
    );
    assert!(
        left.receive_control_cas(&changed, repository.store(), |_| Ok(()))
            .expect_err("command identity immutable")
            .to_string()
            .contains("reused")
    );
    assert_eq!(
        left.metadata_property_page(None, 1).expect("page"),
        vec![Property::Lifecycle]
    );
    assert_eq!(
        left.metadata_property_page(Some("lifecycle"), 1)
            .expect("next page"),
        vec![Property::Name]
    );
}

#[test]
fn thread_control_cross_language_golden_vectors() {
    use objects::object::{StateId, thread_replication::metadata::*};
    let signer = Ed25519Signer::from_seed(&[19; 32]).expect("golden signer");
    let thread = ContentHash::from_bytes([11; 32]);
    let heads = BTreeSet::from([
        ContentHash::from_bytes([12; 32]),
        ContentHash::from_bytes([13; 32]),
    ]);
    let authority = b"portable-authority-codec-fixture";
    let controls = [
        Control::Name("Updated Thread".into()),
        Control::Intent(Intent {
            outcome: "Deliver portable review".into(),
            acceptance_criteria: vec!["Both clients agree".into()],
            origin_urls: vec!["https://example.invalid/intent".into()],
            principal_approved: true,
        }),
        Control::Lifecycle(Lifecycle::Ready),
        Control::Sharing(SharingPolicy {
            ongoing: true,
            destinations: vec![Destination {
                endpoint: [7; 32],
                kind: EndpointKind::Weft,
                spool: uuid::Uuid::from_u128(4),
                facets: BTreeSet::from([
                    SharedFacet::Source,
                    SharedFacet::Collaboration,
                    SharedFacet::Metadata,
                ]),
            }],
        }),
        Control::Review(Review {
            id: uuid::Uuid::from_u128(5),
            source: StateId::from_bytes([21; 32]),
            target: StateId::from_bytes([22; 32]),
            policy_version: ContentHash::from_bytes([23; 32]),
            kind: ReviewKind::Approval,
            explanation: "Reviewed exact source and target".into(),
            revokes: None,
            expires_at_unix_seconds: Some(1900000100),
            coverage: None,
        }),
    ];
    let vectors: Vec<_> = controls.into_iter().map(|control| {
        let value = ThreadControl {
            version: 1, spool: uuid::Uuid::from_u128(1),
            actor: CollaborationActor { principal_id: uuid::Uuid::from_u128(2), agent_id: None },
            authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, authority), authority_envelope: authority.to_vec(),
            client_operation_id: uuid::Uuid::from_u128(3), occurred_at_ms: 1900000000123, control,
        };
        let operation = ThreadOperation { version: 1, thread, parents: heads.clone(), publisher: signer.public_key().try_into().expect("key"), body: ThreadOperationBody::Metadata(value.encode().expect("control")) };
        let signed = SignedOperation::sign(&operation, &signer).expect("signed control");
        assert_eq!(signed.verify().expect("original signature"), operation);
        serde_json::json!({
            "control": value,
            "control_hex": hex::encode(value.encode().expect("control")),
            "operation_hex": hex::encode(&signed.canonical),
            "operation_id": operation.id().expect("ID").to_hex(),
            "publisher_hex": hex::encode(signer.public_key()),
            "signature_hex": hex::encode(signed.signature),
            "property_version": property_version(thread, &value.property(), &heads).expect("version").to_hex(),
            "empty_property_version": property_version(thread, &value.property(), &BTreeSet::new()).expect("empty version").to_hex(),
        })
    }).collect();
    let actual = serde_json::to_value(vectors).expect("vectors");
    println!("THREAD_CONTROL_GOLDEN={}", actual);
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("thread_control_v1.json")).expect("fixture");
    assert_eq!(
        actual, expected,
        "Rust and browser must retain the same signed canonical bytes"
    );
}

#[test]
fn browser_signed_read_coverage_decodes_and_verifies_in_rust() {
    use objects::object::thread_replication::metadata::{ReviewCoverage, ReviewKind};
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("thread_read_browser_v1.json"))
            .expect("browser-generated fixture");
    let field = |name: &str| fixture[name].as_str().expect("fixture field");
    let signed = SignedOperation {
        canonical: hex::decode(field("canonical_hex")).expect("canonical bytes"),
        signature: hex::decode(field("signature_hex")).expect("signature bytes"),
    };
    let original = signed
        .verify()
        .expect("browser Ed25519 signature and operation codec");
    assert_eq!(hex::encode(original.publisher), field("publisher_hex"));
    assert_eq!(original.thread.to_hex(), field("thread_hex"));
    let ThreadOperationBody::Metadata(bytes) = original.body else {
        panic!("signed metadata original")
    };
    let control = ThreadControl::decode(&bytes).expect("browser canonical review control");
    let Control::Review(review) = control.control else {
        panic!("signed review")
    };
    assert_eq!(review.kind, ReviewKind::Read);
    assert_eq!(
        review.coverage,
        Some(ReviewCoverage::Symbols(vec![
            objects::object::thread_replication::metadata::ReviewSymbolAnchor {
                file: field("expected_path").into(),
                symbol: field("expected_symbol").into(),
            },
        ]))
    );
    let mut changed = signed;
    changed.signature[0] ^= 1;
    assert!(
        changed.verify().is_err(),
        "changed browser proof must fail verification"
    );
}

#[test]
fn rust_emitted_review_record_is_a_portable_browser_fixture() {
    use api::heddle::api::v1alpha2 as wire;
    use objects::object::{StateId, thread_replication::metadata::*};
    use prost::Message as _;
    let signer = Ed25519Signer::from_seed(&[29; 32]).expect("fixture signer");
    let thread = ContentHash::from_bytes([11; 32]);
    let spool = uuid::Uuid::from_u128(1);
    let actor = uuid::Uuid::from_u128(2);
    let review_id = uuid::Uuid::from_u128(5);
    let source = StateId::from_bytes([21; 32]);
    let target = StateId::from_bytes([22; 32]);
    let policy_version = ContentHash::from_bytes([23; 32]);
    let value = ThreadControl {
        version: 1,
        spool,
        actor: CollaborationActor {
            principal_id: actor,
            agent_id: None,
        },
        authority_digest: ContentHash::compute_typed(
            AUTHORITY_FORMAT,
            b"portable-authority-codec-fixture",
        ),
        authority_envelope: b"portable-authority-codec-fixture".to_vec(),
        client_operation_id: uuid::Uuid::from_u128(3),
        occurred_at_ms: 1900000000123,
        control: Control::Review(Review {
            id: review_id,
            source,
            target,
            policy_version,
            kind: ReviewKind::Read,
            explanation: "Read exact source".into(),
            revokes: None,
            expires_at_unix_seconds: None,
            coverage: Some(ReviewCoverage::Symbols(vec![ReviewSymbolAnchor {
                file: "src/main.rs".into(),
                symbol: "run".into(),
            }])),
        }),
    };
    let operation = ThreadOperation {
        version: 1,
        thread,
        parents: BTreeSet::new(),
        publisher: signer.public_key().try_into().expect("publisher"),
        body: ThreadOperationBody::Metadata(value.encode().expect("control")),
    };
    let signed = SignedOperation::sign(&operation, &signer).expect("Rust signed original");
    let spool_ref = wire::SpoolRef {
        id: spool.to_string(),
    };
    let revision = |state: StateId| wire::RevisionRef {
        spool: Some(spool_ref.clone()),
        revision: Some(wire::revision_ref::Revision::State(
            api::heddle::api::common::StateId {
                value: state.as_bytes().to_vec(),
            },
        )),
    };
    let record = wire::ReviewRecord {
        decision: Some(wire::ReviewDecision {
            r#ref: Some(wire::RecordRef {
                spool: Some(spool_ref.clone()),
                id: review_id.to_string(),
            }),
            thread: Some(wire::ThreadRef {
                spool: Some(spool_ref.clone()),
                id: Some(wire::ThreadId {
                    value: thread.as_bytes().to_vec(),
                }),
            }),
            source: Some(revision(source)),
            target: Some(revision(target)),
            policy_version: policy_version.as_bytes().to_vec(),
            principal_id: actor.to_string(),
            kind: wire::review_decision::Kind::Read as i32,
            explanation: "Read exact source".into(),
            coverage: Some(wire::ReviewCoverage {
                selection: Some(wire::review_coverage::Selection::Symbols(
                    wire::ReviewSymbols {
                        anchors: vec![wire::ReviewSymbolAnchor {
                            path: "src/main.rs".into(),
                            symbol: "run".into(),
                        }],
                    },
                )),
            }),
            ..Default::default()
        }),
        original: Some(wire::SignedRecord {
            format: objects::object::thread_replication::OPERATION_FORMAT.into(),
            canonical_record: signed.canonical,
            signatures: vec![wire::RecordSignature {
                public_key: signer.public_key().to_vec(),
                signature: signed.signature,
            }],
        }),
    };
    let actual = serde_json::json!({ "review_record_hex": hex::encode(record.encode_to_vec()) });
    println!("RUST_REVIEW_RECORD={actual}");
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("review_record_rust_v1.json")).expect("fixture");
    assert_eq!(
        actual, expected,
        "Rust-emitted original must match checked browser fixture"
    );
}

#[test]
fn original_authority_receipt_is_durable_and_required_before_pending_causality_promotes() {
    let temporary = tempfile::tempdir().expect("repository");
    let repository = crate::Repository::init_default(temporary.path()).expect("repository");
    let signer = Ed25519Signer::from_seed(&[19; 32]).expect("signer");
    let genesis = ThreadGenesis {
        version: 1,
        spool: uuid::Uuid::from_u128(1).to_string(),
        parent: None,
        base: repository.head().expect("HEAD").expect("base"),
        name: "receipt".into(),
        intent: "retained admission".into(),
        creator: signer.public_key().try_into().expect("publisher"),
        owner: objects::object::thread_replication::GenesisOwner::LocalKey(
            signer.public_key().try_into().expect("publisher"),
        ),
        nonce: vec![],
    };
    let replica = ThreadReplica::create(
        repository.heddle_dir(),
        &SignedGenesis::sign(&genesis, &signer).expect("genesis"),
    )
    .expect("replica");
    let make = |command, parents| {
        let proof = b"admission callback verifies this fixture's original authority";
        let control = ThreadControl {
            version: 1,
            spool: uuid::Uuid::from_u128(1),
            actor: CollaborationActor {
                principal_id: uuid::Uuid::from_u128(2),
                agent_id: None,
            },
            authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, proof),
            authority_envelope: proof.to_vec(),
            client_operation_id: uuid::Uuid::from_u128(command),
            occurred_at_ms: 1000,
            control: Control::Name(format!("name-{command}")),
        };
        SignedOperation::sign(
            &ThreadOperation {
                version: 1,
                thread: replica.thread_id(),
                publisher: signer.public_key().try_into().expect("key"),
                parents,
                body: ThreadOperationBody::Metadata(control.encode().expect("control")),
            },
            &signer,
        )
        .expect("original operation")
    };
    let parent = make(3, BTreeSet::new());
    let parent_id = parent.verify().expect("parent").id().expect("parent ID");
    let child = make(4, BTreeSet::from([parent_id]));
    let child_id = child.verify().expect("child").id().expect("child ID");
    assert_eq!(
        replica
            .receive(&child, repository.store(), |_| Ok(()))
            .expect("authorized durable receipt"),
        Admission::Pending
    );
    let reopened =
        ThreadReplica::open(repository.heddle_dir(), replica.thread_id()).expect("restart");
    assert!(
        reopened
            .original_authority_admitted(&child)
            .expect("retained original authority"),
        "pending bytes must atomically retain host-verified original authority"
    );
    // A storage row without a verified receipt is not evidence of admission.
    // Parent arrival cannot manufacture that missing authority from the record.
    reopened
        .connect()
        .expect("fixture connection")
        .execute(
            "UPDATE operations SET authority_admitted=0 WHERE id=?1",
            [child_id.as_bytes()],
        )
        .expect("remove receipt fixture");
    assert!(
        !reopened
            .original_authority_admitted(&child)
            .expect("missing receipt")
    );
    reopened
        .receive(&parent, repository.store(), |_| Ok(()))
        .expect("parent arrives");
    assert_eq!(
        reopened
            .operation(&child_id)
            .expect("child row")
            .expect("retained child")
            .1,
        Admission::Pending,
        "causal readiness cannot invent original-author admission"
    );
    assert!(
        reopened
            .receive(&child, repository.store(), |_| Err(Error::Invalid(
                "original credential expired at fresh admission".into()
            )))
            .expect_err("unadmitted envelope cannot backdate authority")
            .to_string()
            .contains("expired")
    );
    assert_eq!(
        reopened
            .operation(&child_id)
            .expect("child row")
            .expect("retained child")
            .1,
        Admission::Pending
    );
    reopened
        .receive(&child, repository.store(), |_| Ok(()))
        .expect("independently fresh reauthorization");
    assert!(
        reopened
            .original_authority_admitted(&child)
            .expect("new durable receipt")
    );
    assert_eq!(
        reopened
            .operation(&child_id)
            .expect("child row")
            .expect("retained child")
            .1,
        Admission::Accepted
    );
}
