use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use objects::{
    object::{
        Attribution, Blob, Principal, Tree, TreeEntry, thread_replication::ThreadOperationBody,
    },
    store::pack::{ObjectType, PackBuilder, PackObjectId},
};

use super::*;

fn fixture(
    scratch: &Path,
    extra_blob: bool,
) -> (
    tempfile::TempDir,
    TransferReady,
    Vec<SignedOperation>,
    State,
) {
    let (_, mut ready, _, _) = crate::fetch::tests::fixture();
    let signer = Ed25519Signer::from_seed(&[61; 32]).expect("original creator");
    let genesis = replication::opening::verify_genesis(
        ready
            .thread_genesis
            .as_ref()
            .expect("genesis")
            .genesis
            .as_ref()
            .expect("signed genesis"),
        ready.thread.as_ref().expect("Thread"),
    )
    .expect("original Thread");
    let blob = Blob::new(b"selected source".to_vec());
    let tree = Tree::from_entries(vec![
        TreeEntry::file("main.rs", blob.hash(), false).expect("entry"),
    ]);
    let state = State::new_snapshot(
        tree.hash(),
        vec![genesis.base],
        Attribution::human(Principal::new("author", "author@example.test")),
    );
    ready.current.as_mut().expect("revision").revision = Some(revision_ref::Revision::State(
        api::heddle::api::v1alpha1::StateId {
            value: state.id().as_bytes().to_vec(),
        },
    ));
    let operation = SignedOperation::sign(
        &ThreadOperation {
            version: 1,
            thread: genesis.id().expect("Thread ID"),
            parents: BTreeSet::new(),
            publisher: signer.public_key().try_into().expect("key"),
            body: ThreadOperationBody::Capture(objects::object::thread_replication::AuthoredCapture::local(state.encode_current_msgpack().expect("State").into())),
        },
        &signer,
    )
    .expect("original source signature");
    let mut builder = PackBuilder::for_repack(Default::default(), 0);
    builder.add_id(
        PackObjectId::StateId(state.id()),
        ObjectType::State,
        state.encode_current_msgpack().expect("State"),
    );
    builder.add_id(
        PackObjectId::Hash(tree.hash()),
        ObjectType::Tree,
        tree.encode_canonical().expect("tree"),
    );
    builder.add_id(
        PackObjectId::Hash(blob.hash()),
        ObjectType::Blob,
        blob.into_content(),
    );
    if extra_blob {
        let extra = Blob::new(b"private source outside the selected closure".to_vec());
        builder.add_id(
            PackObjectId::Hash(extra.hash()),
            ObjectType::Blob,
            extra.into_content(),
        );
    }
    let (pack, index, _) = builder.build().expect("source artifacts");
    let directory = tempfile::Builder::new()
        .prefix("thread-download-")
        .tempdir_in(scratch)
        .expect("staging");
    std::fs::write(directory.path().join("source.pack"), pack).expect("pack");
    std::fs::write(directory.path().join("source.idx"), index).expect("index");
    (directory, ready, vec![operation], state)
}
#[test]
fn source_staging_keeps_original_proofs_and_releases_artifacts_twice() {
    let scratch = tempfile::tempdir().expect("scratch");
    for _ in 0..2 {
        let (directory, ready, operations, expected) = fixture(scratch.path(), false);
        let original = operations.clone();
        let staged = validate(directory, ready, operations, vec![]).expect("exact closure");
        assert_eq!(staged.state().id(), expected.id());
        assert_eq!(staged.operations(), original);
        assert!(staged.artifact_paths().iter().all(|path| path.exists()));
        drop(staged);
        assert_eq!(
            std::fs::read_dir(scratch.path())
                .expect("scratch entries")
                .count(),
            0
        );
    }
}
#[test]
fn source_staging_rejects_unselected_content_before_installation() {
    let scratch = tempfile::tempdir().expect("scratch");
    let (directory, ready, operations, _) = fixture(scratch.path(), true);
    let error = match validate(directory, ready, operations, vec![]) {
        Ok(_) => panic!("unselected source must be rejected"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("outside the selected revision"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_dir(scratch.path())
            .expect("scratch entries")
            .count(),
        0
    );
}
#[test]
fn source_staging_requires_the_original_causal_parents() {
    let scratch = tempfile::tempdir().expect("scratch");
    let (directory, ready, mut operations, _) = fixture(scratch.path(), false);
    let mut operation = operations[0].verify().expect("operation");
    operation
        .parents
        .insert(ContentHash::compute(b"missing original parent"));
    operations[0] = SignedOperation::sign(
        &operation,
        &Ed25519Signer::from_seed(&[61; 32]).expect("creator"),
    )
    .expect("valid signature");
    assert!(matches!(
        validate(directory, ready, operations, vec![]),
        Err(Error::Invalid("incomplete source ancestry"))
    ));
    assert_eq!(
        std::fs::read_dir(scratch.path())
            .expect("scratch entries")
            .count(),
        0
    );
}

fn integrated_fixture(
    scratch: &Path,
) -> (
    tempfile::TempDir,
    TransferReady,
    Vec<SignedOperation>,
    Vec<ThreadGenesisRecord>,
) {
    use objects::object::{
        VisibilityTier, thread_replication::local_integration::LocalIntegration,
    };
    let (directory, mut ready, _, original) = fixture(scratch, false);
    let signer = Ed25519Signer::from_seed(&[61; 32]).expect("signer");
    let target = replication::opening::verify_genesis(
        ready
            .thread_genesis
            .as_ref()
            .expect("genesis")
            .genesis
            .as_ref()
            .expect("signed genesis"),
        ready.thread.as_ref().expect("thread"),
    )
    .expect("target");
    let mut source = target.clone();
    source.name = "dependency".into();
    source.nonce = vec![1];
    let source_record =
        replication::opening::sign_genesis(&source, &signer).expect("source genesis");
    let source_state = State::new_snapshot(
        original.tree,
        vec![source.base],
        Attribution::human(Principal::new("source", "")),
    )
    .with_intent("dependency source");
    let source_op = ThreadOperation {
        version: 1,
        thread: source.id().expect("source Thread"),
        parents: BTreeSet::new(),
        publisher: signer.public_key().try_into().expect("key"),
        body: ThreadOperationBody::Capture(objects::object::thread_replication::AuthoredCapture::local(source_state
                .encode_current_msgpack()
                .expect("source State")
                .into())),
    };
    let result = State::new_merge(
        original.tree,
        vec![target.base, source_state.id()],
        Attribution::human(Principal::new("integrator", "")),
    );
    let receipt = LocalIntegration {
        author: heddle_object_model::object::thread_replication::SourceAuthor::LocalKey,
        version: 1,
        spool: source.spool.parse().expect("Spool"),
        device: signer.public_key().try_into().expect("key"),
        source_thread: source.id().expect("Thread"),
        source_operation: source_op.id().expect("source operation"),
        source_revision: source_state.id(),
        target_thread: target.id().expect("target Thread"),
        expected_target_frontier: BTreeSet::new(),
        result: result.encode_current_msgpack().expect("result").into(),
        result_visibility: VisibilityTier::Public,
        initiating_request_proof: ContentHash::from_bytes([3; 32]),
        local_policy_version: ContentHash::from_bytes([4; 32]),
        executed_at_ms: 0,
    };
    let integration = ThreadOperation {
        version: 1,
        thread: target.id().expect("target"),
        parents: BTreeSet::new(),
        publisher: receipt.device,
        body: ThreadOperationBody::LocalIntegration(receipt.encode().expect("receipt")),
    };
    ready.current.as_mut().expect("revision").revision = Some(revision_ref::Revision::State(
        api::heddle::api::v1alpha1::StateId {
            value: result.id().as_bytes().to_vec(),
        },
    ));
    let mut builder = PackBuilder::for_repack(Default::default(), 0);
    let reader = PackReader::open(
        &directory.path().join("source.pack"),
        &directory.path().join("source.idx"),
    )
    .expect("pack");
    reader
        .visit_objects(|id, kind, bytes| {
            if kind != ObjectType::State {
                builder.add_id(id, kind, bytes.to_vec());
            }
            Ok(())
        })
        .expect("reuse exact tree");
    drop(reader);
    builder.add_id(
        PackObjectId::StateId(result.id()),
        ObjectType::State,
        result.encode_current_msgpack().expect("result"),
    );
    let (pack, index, _) = builder.build().expect("pack");
    std::fs::write(directory.path().join("source.pack"), pack).expect("write pack");
    std::fs::write(directory.path().join("source.idx"), index).expect("write index");
    (
        directory,
        ready,
        vec![
            SignedOperation::sign(&integration, &signer).expect("integration signature"),
            SignedOperation::sign(&source_op, &signer).expect("source signature"),
        ],
        vec![ThreadGenesisRecord {
            ownership_claims: vec![], ownership_claim_admissions: vec![],
            genesis: Some(source_record),
            creator_authority: vec![],
            admission: None,
        }],
    )
}
#[test]
fn source_staging_verifies_foreign_genesis_and_installs_dependency_first() {
    let scratch = tempfile::tempdir().expect("scratch");
    let (directory, ready, operations, dependencies) = integrated_fixture(scratch.path());
    let dependency = operations[1].clone();
    let staged = validate(directory, ready, operations, dependencies)
        .expect("complete foreign source provenance");
    assert_eq!(staged.dependency_geneses().len(), 1);
    assert_eq!(staged.operations()[0], dependency);
    assert!(
        staged.operations()[1]
            .verify()
            .expect("integration")
            .local_integration()
            .expect("receipt")
            .is_some()
    );
}
#[test]
fn source_staging_rejects_missing_or_unrelated_foreign_source_provenance() {
    let scratch = tempfile::tempdir().expect("scratch");
    let (directory, ready, operations, _) = integrated_fixture(scratch.path());
    assert!(
        validate(directory, ready, operations, vec![]).is_err(),
        "missing creator genesis cannot authorize a dependency"
    );
    let (directory, ready, mut operations, dependencies) = integrated_fixture(scratch.path());
    operations.pop();
    assert!(matches!(
        validate(directory, ready, operations, dependencies),
        Err(Error::Invalid(
            "local integration original source proof absent"
        ))
    ));
}

fn publication_fixture(scratch: &Path, extra: bool) -> (tempfile::TempDir, PublishContentOpen, crate::publication::PublicationOriginals, State) {
    let (directory, ready, operations, state) = fixture(scratch, extra);
    let packs = ["source.pack", "source.idx"].into_iter().enumerate().map(|(index, name)| {
        let bytes = std::fs::read(directory.path().join(name)).expect("actual uploaded artifact");
        let address = ObjectAddress { algorithm: "blake3".into(), digest: blake3::hash(&bytes).as_bytes().to_vec() };
        PackExtent { pack: Some(address.clone()), offset: 0, length: bytes.len() as u64,
            extent_digest: Some(address), kind: if index == 0 { pack_extent::Kind::NativePack } else { pack_extent::Kind::NativeIndex } as i32 }
    }).collect();
    let originals = crate::publication::PublicationOriginals {
        geneses: vec![ready.thread_genesis.expect("original genesis")],
        operations: vec![ReplicationOperations { authority_admissions: vec![], operations: operations.into_iter().map(|signed| {
            let publisher = signed.verify().expect("original source").publisher;
            SignedRecord { format: heddle_object_model::object::thread_replication::OPERATION_FORMAT.into(),
                canonical_record: signed.canonical, signatures: vec![RecordSignature { public_key: publisher.to_vec(), signature: signed.signature }] }
        }).collect() }],
    };
    (directory, PublishContentOpen { thread: ready.thread, revision: ready.current, packs, ..Default::default() }, originals, state)
}

#[test]
fn publication_staging_validates_originals_actual_artifacts_and_cleanup_twice() {
    let scratch = tempfile::tempdir().expect("scratch");
    for _ in 0..2 {
        let (directory, opening, originals, state) = publication_fixture(scratch.path(), false);
        let path = directory.path().to_owned();
        let value = crate::publication::validate_source_artifacts(directory, &opening, originals).expect("validated publication");
        assert_eq!(value.state().id(), state.id());
        assert_eq!(value.operations().len(), 1);
        assert_eq!(value.geneses().count(), 1);
        assert!(path.exists());
        drop(value);
        assert!(!path.exists(), "completed staged publication owns scratch cleanup");
    }
}

#[test]
fn publication_staging_rejects_wrong_inventory_and_unsupplied_originals() {
    let scratch = tempfile::tempdir().expect("scratch");
    let (directory, mut opening, originals, _) = publication_fixture(scratch.path(), false);
    let path = directory.path().to_owned();
    opening.packs[0].pack.as_mut().expect("address").digest[0] ^= 1;
    opening.packs[0].extent_digest = opening.packs[0].pack.clone();
    assert!(matches!(crate::publication::validate_source_artifacts(directory, &opening, originals),
        Err(Error::Invalid("uploaded artifact digest differs"))), "actual artifact inventory must be checked");
    assert!(!path.exists());
    let (directory, opening, mut originals, _) = publication_fixture(scratch.path(), false);
    originals.operations[0].operations[0].signatures[0].signature[0] ^= 1;
    assert!(crate::publication::validate_source_artifacts(directory, &opening, originals).is_err(), "original source signatures are necessary");
    let (directory, opening, originals, _) = publication_fixture(scratch.path(), true);
    assert!(crate::publication::validate_source_artifacts(directory, &opening, originals).is_err(), "unselected private objects must not be staged as source");
}

#[test]
fn publication_staging_preserves_matched_account_admission_without_trusting_issuer() {
    use objects::object::{CollaborationActor, thread_replication::{AuthoredCapture, SourceAuthor, integration::TrustedHostedExecutor}, thread_authority_admission::ThreadAuthorityAdmission};
    let scratch = tempfile::tempdir().expect("scratch");
    let (directory, opening, mut originals, state) = publication_fixture(scratch.path(), false);
    let signer = Ed25519Signer::from_seed(&[61; 32]).expect("source author");
    let mut operation = replication::decode_record(originals.operations[0].operations[0].clone()).expect("source").verify().expect("signature");
    let result = operation.source_result().expect("result").expect("capture");
    let spool = uuid::Uuid::parse_str(&opening.thread.as_ref().expect("Thread").spool.as_ref().expect("Spool").id).expect("Spool UUID");
    let actor = CollaborationActor { principal_id: uuid::Uuid::from_u128(73), agent_id: Some("original-agent".into()) };
    let capture = AuthoredCapture::account(result, spool, actor.clone(), b"first-admitted original author envelope".to_vec()).expect("signed author binding");
    let SourceAuthor::Account { authority_digest, .. } = capture.author else { panic!("account author"); };
    operation.body = ThreadOperationBody::Capture(capture);
    let signed = SignedOperation::sign(&operation, &signer).expect("original source signature");
    originals.operations[0].operations[0] = SignedRecord { format: objects::object::thread_replication::OPERATION_FORMAT.into(), canonical_record: signed.canonical.clone(), signatures: vec![RecordSignature { public_key: operation.publisher.to_vec(), signature: signed.signature.clone() }] };
    let executor = Ed25519Signer::from_seed(&[74; 32]).expect("receipt issuer");
    let statement = ThreadAuthorityAdmission { version: 2, spool, spool_genesis: ContentHash::from_bytes([75;32]), thread: operation.thread,
        subject: objects::object::thread_authority_admission::OriginalAuthoritySubject::Operation(operation.id().expect("operation ID")), actor, publisher: operation.publisher, authority_digest,
        executor: executor.public_key().try_into().expect("executor key"), admitted_at_ms: 100 };
    let receipt = crypto::thread_authority_admission::SignedAuthorityAdmission::sign(&statement, &executor).expect("historical testimony");
    originals.operations[0].authority_admissions = vec![crate::authority_admission::encode(&receipt).expect("portable receipt")];
    let validated = crate::publication::validate_source_artifacts(directory, &opening, originals).expect("matched structurally valid source");
    assert_eq!(validated.state().id(), state.id());
    assert_eq!(validated.authority_admissions().get(&statement.subject.id()), Some(&receipt));
    let wrong_trust = TrustedHostedExecutor { spool, spool_genesis: statement.spool_genesis, executor: [76;32] };
    assert!(validated.authority_admissions()[&statement.subject.id()].verify(&signed, &wrong_trust).is_err(), "structural staging never enrolls its issuer");
}
