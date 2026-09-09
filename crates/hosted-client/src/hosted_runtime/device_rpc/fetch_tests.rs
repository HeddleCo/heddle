//! Real Iroh selected source proofs include foreign integration ancestry and
//! never use target access to grant unrelated Thread ownership.
use std::collections::BTreeSet;

use crypto::{
    Ed25519Signer, Signer,
    thread_operation::{SignedGenesis, SignedOperation},
};
use objects::{
    object::{
        Attribution, Principal, State,
        thread_replication::{
            Admission, GenesisOwner, ThreadGenesis, ThreadOperation, ThreadOperationBody,
        },
    },
    store::ObjectStore,
};

use super::*;
pub(super) async fn roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
) {
    let genesis = replica.genesis().expect("selected genesis");
    let selected = *replica
        .view()
        .expect("source view")
        .source_heads
        .first()
        .expect("selected source");
    let request = open(&genesis, selected);
    for _ in 0..2 {
        let download = remote
            .fetch_content(request.clone(), Default::default())
            .await
            .expect("owned device exact source Fetch");
        assert!(
            download.ready().owner_genesis.is_none(),
            "device-only source does not require Weft ownership"
        );
        let scratch = tempfile::tempdir().expect("download staging");
        let staged = download
            .stage(scratch.path())
            .await
            .expect("complete source closure");
        assert_eq!(staged.state().id(), selected);
        assert_eq!(
            staged.dependency_geneses().len(),
            1,
            "original integrated source Thread is portable"
        );
        let operations = staged.operations();
        assert!(operations.len() >= 2);
        assert!(
            operations
                .last()
                .expect("integration last")
                .verify()
                .expect("signed result")
                .local_integration()
                .expect("typed integration")
                .is_some()
        );
        let paths = staged.artifact_paths();
        assert!(paths.iter().all(|p| p.exists()));
        drop(staged);
        assert!(
            paths.iter().all(|p| !p.exists()),
            "staging cleanup removes both artifacts"
        );
    }
    let stranger = Ed25519Signer::from_seed(&[113; 32]).expect("stranger key");
    let foreign = ThreadGenesis {
        owner: GenesisOwner::LocalKey(stranger.public_key().try_into().expect("key")),
        name: "unowned exact source".into(),
        creator: stranger.public_key().try_into().expect("key"),
        nonce: vec![113],
        base: selected,
        ..genesis.clone()
    };
    let foreign_replica = repo::thread_replication::ThreadReplica::create(
        repository.heddle_dir(),
        &SignedGenesis::sign(&foreign, &stranger).expect("original genesis"),
    )
    .expect("local signed original");
    let source = repository
        .store()
        .get_state(&selected)
        .expect("source read")
        .expect("source object");
    let state = State::new_snapshot(
        source.tree,
        vec![selected],
        Attribution::human(Principal::new("foreign author", "")),
    );
    let operation = ThreadOperation {
        version: 1,
        thread: foreign.id().expect("Thread"),
        parents: BTreeSet::new(),
        publisher: foreign.creator,
        body: ThreadOperationBody::Capture(state.encode_current_msgpack().expect("State").into()),
    };
    assert_eq!(
        foreign_replica
            .receive(
                &SignedOperation::sign(&operation, &stranger).expect("original source"),
                repository.store(),
                |_| Ok(())
            )
            .expect("trusted local import fixture"),
        Admission::Accepted
    );
    assert!(
        remote
            .fetch_content(open(&foreign, state.id()), Default::default())
            .await
            .is_err(),
        "same Spool and existing source object do not grant unowned Thread access"
    );
}
fn open(genesis: &ThreadGenesis, state: objects::object::StateId) -> FetchOpen {
    let spool = SpoolRef {
        id: genesis.spool.clone(),
    };
    FetchOpen {
        thread: Some(ThreadRef {
            spool: Some(spool.clone()),
            id: Some(ThreadId {
                value: genesis.id().expect("id").as_bytes().to_vec(),
            }),
        }),
        revision: Some(RevisionRef {
            spool: Some(spool),
            revision: Some(revision_ref::Revision::State(
                api::heddle::api::v1alpha1::StateId {
                    value: state.as_bytes().to_vec(),
                },
            )),
        }),
        selection: Some(TransferSelection {
            facets: vec![SharedFacet::Source as i32],
            ..Default::default()
        }),
        ..Default::default()
    }
}
