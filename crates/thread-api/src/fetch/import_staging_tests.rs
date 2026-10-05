use prost::Message;

use super::*;

#[test]
fn imported_root_stages_state_closure_with_one_authenticated_tip() {
    use repo::thread_replication::{delegated_import, hosted_trust::TrustTransaction};

    use crate::hybrid::authority::{AcceptedHistory, SelectedAuthority, tests};
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha32.json"))
            .expect("signed import fixture");
    let record = |name: &str| {
        let bytes = fixture["wire_vectors"]
            .get(name)
            .or_else(|| fixture["signed_vectors"].get(name))
            .expect("record");
        hex::decode(bytes["wire_hex"].as_str().expect("wire")).expect("hex")
    };
    let payload =
        ImportGenesisWitnessV1::decode(record("genesis_payload").as_slice()).expect("payload");
    let (signed, operation) = crypto::import_authority::verify_native_operation(
        &SignedRecord::decode(record("converted_main").as_slice()).expect("native original"),
    )
    .expect("native job signature");
    let state = operation.source_state().expect("source").expect("State");
    assert!(state.parents.is_empty());
    let mut bundle = tests::bundle();
    bundle.history_proofs = [
        "genesis_proof",
        "genesis_dev_proof",
        "publication_proof",
        "renewed_publication_proof",
    ]
    .iter()
    .map(|name| {
        api::heddle::api::common::HostedWitnessHistoryProofV1::decode(record(name).as_slice())
            .expect("exact inclusion proof")
    })
    .collect();
    let limits = heddleco_capability_verifier::VerificationLimits::new(3600).expect("limits");
    let pinned = tests::selected(&bundle, limits);
    let history = AcceptedHistory::from_selected_spool(&bundle, &pinned, 1350, limits)
        .expect("verified owner history");
    let authority = SelectedAuthority::new(
        history,
        bundle.clone(),
        |_: &ImportPublicProofBundleV1, _: i64, _: &TrustTransaction<'_>| Ok(()),
    );
    let pin = api::import_authority::ImportWitnessRootPin {
        authority: "https://weft.example.test".into(),
        root_id: "descriptor-root-1".into(),
        public_key: hex::decode(
            fixture["keys"]["root"]["public_key_hex"]
                .as_str()
                .expect("root"),
        )
        .expect("key"),
        epoch: 1,
    };
    let carriers = delegated_import::authenticate_import_carriers(
        &bundle,
        &authority,
        &pin,
        1_350_000,
        &[],
        &[],
        |_| Ok(()),
    )
    .expect("witnessed owner-authorized carriers");
    let (_, mut ready, _, _) = crate::fetch::tests::fixture();
    ready.thread.as_mut().expect("Thread").id = Some(ThreadId {
        value: operation.thread.as_bytes().to_vec(),
    });
    ready
        .thread
        .as_mut()
        .expect("Thread")
        .spool
        .as_mut()
        .expect("Spool")
        .id = uuid::Uuid::from_slice(
        &bundle.delegations[0]
            .body
            .as_ref()
            .expect("body")
            .identity
            .as_ref()
            .expect("identity")
            .spool_uuid,
    )
    .expect("Spool UUID")
    .to_string();
    ready.current.as_mut().expect("revision").spool =
        ready.thread.as_ref().expect("Thread").spool.clone();
    ready.current.as_mut().expect("revision").revision = Some(revision_ref::Revision::State(
        api::heddle::api::common::StateId {
            value: state.id().as_bytes().to_vec(),
        },
    ));
    ready.thread_genesis = Some(ThreadGenesisRecord {
        genesis: payload.original_genesis,
        creator_authority: payload.creator_authority_envelope,
        ..Default::default()
    });
    ready.import_authority = Some(bundle);
    ready.full_closure_available = true;
    let tree = Tree::new();
    assert_eq!(state.tree, tree.hash());
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
    let (pack, index, _) = builder.build().expect("closure pack");
    let directory = || {
        let directory = tempfile::tempdir().expect("staging directory");
        std::fs::write(directory.path().join("source.pack"), &pack).expect("pack");
        std::fs::write(directory.path().join("source.idx"), &index).expect("index");
        directory
    };
    assert!(
        validate(directory(), ready.clone(), vec![signed.clone()], vec![]).is_err(),
        "a carried but unauthenticated bundle cannot unlock import ancestry"
    );
    let staged = validate_with_receipts_and_carriers(
        directory(),
        ready,
        vec![signed],
        vec![],
        vec![],
        Some(carriers),
    )
    .expect("exact authenticated root stages");
    assert_eq!(staged.operations().len(), 1);
    let destination = tempfile::tempdir().expect("receiver");
    let repository = repo::Repository::init_default(destination.path()).expect("repository");
    staged
        .install_source_objects(&repository)
        .expect("install actual State closure");
    use objects::store::ObjectStore;
    assert_eq!(
        repository
            .store()
            .get_state(&state.id())
            .expect("State read"),
        Some(state)
    );
}
