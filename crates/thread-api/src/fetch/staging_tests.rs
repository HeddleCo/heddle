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
            body: ThreadOperationBody::Capture(
                state.encode_current_msgpack().expect("State").into(),
            ),
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
        body: ThreadOperationBody::Capture(
            source_state
                .encode_current_msgpack()
                .expect("source State")
                .into(),
        ),
    };
    let result = State::new_merge(
        original.tree,
        vec![target.base, source_state.id()],
        Attribution::human(Principal::new("integrator", "")),
    );
    let receipt = LocalIntegration {
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
