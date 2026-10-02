// SPDX-License-Identifier: Apache-2.0
use super::*;
use crypto::{Ed25519Signer, Signer, thread_operation::SignedGenesis};
use objects::{
    object::{
        Attribution, AttributionBasis, AttributionClaim, AttributionEvidenceV1, AttributionSource,
        Principal, State, Tree,
        thread_replication::{
            AuthoredCapture, GenesisOwner, ThreadGenesis, ThreadOperation, ThreadOperationBody,
        },
    },
    store::ObjectStore,
};
use repo::{Repository, thread_replication::ThreadReplica};
use std::sync::Arc;

#[tokio::test]
async fn source_formats_gate_queued_and_immediate_exports_without_rewriting_legacy() {
    let temp = tempfile::tempdir().expect("repository");
    let repository = Repository::init_default(temp.path()).expect("init");
    let signer = Ed25519Signer::from_seed(&[37; 32]).expect("key");
    let publisher = signer.public_key().try_into().expect("public key");
    let genesis = ThreadGenesis {
        version: 1,
        spool: "01980000-0000-7000-8000-000000000001".into(),
        parent: None,
        base: repository.head().expect("head").expect("base"),
        name: "source-formats".into(),
        intent: "preserve exact committed source".into(),
        creator: publisher,
        owner: GenesisOwner::LocalKey(publisher),
        nonce: vec![],
    };
    let replica = ThreadReplica::create(
        repository.heddle_dir(),
        &SignedGenesis::sign(&genesis, &signer).expect("genesis signature"),
    )
    .expect("replica");
    let facets = BTreeSet::from([ThreadFacet::Source]);
    replica.set_sharing([2; 32], &facets).expect("sharing");
    let evidence = AttributionEvidenceV1 {
        harness: Some(AttributionClaim::new(
            "codex",
            AttributionBasis::RequestReported,
            AttributionSource::HarnessHook,
        )),
        ..Default::default()
    }
    .to_blob()
    .expect("evidence");
    repository
        .store()
        .put_blob(&evidence)
        .expect("required evidence");
    let legacy = State::new_snapshot(
        Tree::new().hash(),
        vec![genesis.base],
        Attribution::human(Principal::new("Author", "author@example.test")),
    );
    let rich = legacy.clone().with_attribution_evidence(evidence.hash());
    let mut records = Vec::new();
    for state in [&legacy, &rich] {
        let operation = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: BTreeSet::new(),
            publisher,
            body: ThreadOperationBody::Capture(AuthoredCapture::local(
                state.encode_current_msgpack().expect("state").into(),
            )),
        };
        let signed = SignedOperation::sign(&operation, &signer).expect("signature");
        assert_eq!(
            replica
                .receive(&signed, repository.store(), |_| Ok(()))
                .expect("admission"),
            Admission::Accepted
        );
        records.push((operation.id().expect("operation ID"), signed));
    }
    let make_session = || {
        Session::new(
            native::LocalReplica::new(replica.clone(), Arc::new(repository.store().clone())),
            [2; 32],
            facets.clone(),
            8,
        )
        .expect("session")
    };
    let legacy_frame = make_session()
        .export_operation(records[0].0)
        .await
        .expect("legacy export");
    let Frame::Operations(legacy_batch) = legacy_frame else {
        panic!("operations")
    };
    assert_eq!(
        legacy_batch.operations[0].canonical_record,
        records[0].1.canonical
    );
    assert_eq!(
        legacy_batch.operations[0].signatures[0].signature,
        records[0].1.signature
    );
    for formats in [vec![], vec![0], vec![99]] {
        let mut session = make_session().with_peer_native_source_formats(formats);
        assert!(matches!(
            session.export_operation(records[1].0).await,
            Err(StoreError::Protocol(Error::NativeSourceFormat(
                api::source_format::NativeSourceFormatError::UnsupportedByPeer(_)
            )))
        ));
        assert!(matches!(
            session
                .handle(Frame::Need(ReplicationNeed {
                    operation_ids: vec![records[1].0.as_bytes().to_vec()]
                }))
                .await,
            Err(StoreError::Protocol(Error::NativeSourceFormat(_)))
        ));
    }
    let mut session = make_session()
        .with_peer_native_source_formats(vec![99, api::source_format::STATE_V6_ATTRIBUTION_V1]);
    let queued = session
        .handle(Frame::Need(ReplicationNeed {
            operation_ids: vec![records[1].0.as_bytes().to_vec()],
        }))
        .await
        .expect("compatible queue");
    assert!(matches!(queued.first(), Some(Outbound::Operation(_))));
    let Frame::Operations(batch) = session
        .export_operation(records[1].0)
        .await
        .expect("compatible export")
    else {
        panic!("operations")
    };
    assert_eq!(batch.operations[0].canonical_record, records[1].1.canonical);
    assert_eq!(
        batch.operations[0].signatures[0].signature,
        records[1].1.signature
    );
    let decoded = decode_record(batch.operations[0].clone())
        .expect("unchanged signature")
        .verify()
        .expect("valid capture");
    assert_eq!(
        decoded
            .source_state()
            .expect("state")
            .expect("capture")
            .attribution_evidence,
        Some(evidence.hash())
    );
}
