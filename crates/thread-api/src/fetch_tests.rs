use crypto::{Ed25519Signer, Signer};
use objects::object::{StateId, thread_replication::ThreadGenesis};

use super::*;

pub(super) fn fixture() -> (FetchOpen, TransferReady, EndpointRef, [Vec<u8>; 2]) {
    let signer = Ed25519Signer::from_seed(&[61; 32]).expect("test creator");
    let spool_id = uuid::Uuid::from_u128(0x01980000000070008000000000000001);
    let genesis = ThreadGenesis {
        owner: objects::object::thread_replication::GenesisOwner::LocalKey(
            signer.public_key().try_into().expect("public key"),
        ),
        version: 1,
        spool: spool_id.to_string(),
        parent: None,
        base: StateId::from_bytes([17; 32]),
        name: "download".into(),
        intent: "source".into(),
        creator: signer.public_key().try_into().expect("public key"),
        nonce: vec![],
    };
    let thread = ThreadRef {
        spool: Some(SpoolRef {
            id: spool_id.to_string(),
        }),
        id: Some(ThreadId {
            value: genesis.id().expect("identity").as_bytes().to_vec(),
        }),
    };
    let revision = RevisionRef {
        spool: thread.spool.clone(),
        revision: Some(revision_ref::Revision::State(
            api::heddle::api::v1alpha1::StateId {
                value: [31; 32].to_vec(),
            },
        )),
    };
    let endpoint = EndpointRef {
        public_key: vec![42; 32],
        kind: EndpointKind::Weft as i32,
    };
    let artifacts = [
        b"independently verified pack bytes".to_vec(),
        b"independently verified index bytes".to_vec(),
    ];
    let packs = artifacts
        .iter()
        .enumerate()
        .map(|(i, bytes)| {
            let address = ObjectAddress {
                algorithm: "blake3".into(),
                digest: blake3::hash(bytes).as_bytes().to_vec(),
            };
            PackExtent {
                pack: Some(address.clone()),
                kind: if i == 0 {
                    pack_extent::Kind::NativePack
                } else {
                    pack_extent::Kind::NativeIndex
                } as i32,
                offset: 0,
                length: bytes.len() as u64,
                extent_digest: Some(address),
            }
        })
        .collect();
    let open = FetchOpen {
        thread: Some(thread.clone()),
        revision: Some(revision.clone()),
        selection: Some(TransferSelection {
            facets: vec![SharedFacet::Source as i32],
            ..Default::default()
        }),
        ..Default::default()
    };
    let ready = TransferReady {
        endpoint: Some(endpoint.clone()),
        thread: Some(thread),
        current: Some(revision),
        owner_genesis: Some(
            repo::sign_spool_owner_genesis(&signer, *spool_id.as_bytes()).expect("owner signature"),
        ),
        ownership: Some(OwnerState::default()),
        thread_genesis: Some(ThreadGenesisRecord {
            ownership_claims: vec![], ownership_claim_admissions: vec![],
            genesis: Some(
                replication::opening::sign_genesis(&genesis, &signer).expect("creator signature"),
            ),
            creator_authority: vec![],
            admission: None,
        }),
        packs,
        checkpoint: Some(TransferCheckpoint {
            transfer_id: vec![9; 16],
            plan_digest: vec![5; 32],
            ..Default::default()
        }),
        budget: Some(ReadBudget {
            max_items: 128,
            max_frame_bytes: 16 * 1024,
            max_snapshot_bytes: 1024 * 1024,
        }),
        full_closure_available: true,
        ..Default::default()
    };
    (open, ready, endpoint, artifacts)
}
fn chunk(ready: &TransferReady, artifact: usize, data: Vec<u8>) -> FetchServerFrame {
    let mut extent = ready.packs[artifact].clone();
    extent.length = data.len() as u64;
    extent.extent_digest = Some(ObjectAddress {
        algorithm: "blake3".into(),
        digest: blake3::hash(&data).as_bytes().to_vec(),
    });
    FetchServerFrame {
        body: Some(fetch_server_frame::Body::Pack(PackChunk {
            extent: Some(extent),
            data,
        })),
    }
}
fn complete(ready: &TransferReady) -> FetchServerFrame {
    let mut checkpoint = ready.checkpoint.clone().expect("checkpoint");
    checkpoint.committed_bytes = ready.packs.iter().map(|p| p.length).sum();
    FetchServerFrame {
        body: Some(fetch_server_frame::Body::Complete(FetchComplete {
            revision: ready.current.clone(),
            checkpoint: Some(checkpoint),
            closure: Coverage::Complete as i32,
            missing: vec![],
        })),
    }
}
#[test]
fn native_fetch_commits_only_after_both_hashed_artifacts_and_exact_checkpoint() {
    let (open, ready, endpoint, artifacts) = fixture();
    let mut download = Validation::new(open, ready.clone(), Some(&endpoint), Limits::default())
        .expect("admission");
    assert!(matches!(
        download.accept(complete(&ready)),
        Err(Error::Invalid(
            "download is not a complete exact source closure"
        ))
    ));
    assert!(!download.done);
    for (i, data) in artifacts.into_iter().enumerate() {
        assert!(matches!(
            download
                .accept(chunk(&ready, i, data))
                .expect("verified chunk"),
            Item::Pack(_)
        ));
    }
    assert!(matches!(
        download.accept(complete(&ready)).expect("complete"),
        Item::Complete(_)
    ));
    assert!(download.done);
    assert!(
        download.accept(complete(&ready)).is_err(),
        "terminal checkpoint cannot replay as a new commit"
    );
}
#[test]
fn native_fetch_rejects_rehashed_corruption_against_original_whole_artifact() {
    let (open, ready, endpoint, mut artifacts) = fixture();
    let mut download = Validation::new(open, ready.clone(), Some(&endpoint), Limits::default())
        .expect("admission");
    artifacts[0][0] ^= 1;
    assert!(matches!(
        download.accept(chunk(&ready, 0, artifacts[0].clone())),
        Err(Error::Invalid("whole artifact hash mismatch"))
    ));
    assert!(!download.done);
}
#[test]
fn native_fetch_rejects_different_endpoint_and_truncated_index() {
    let (open, ready, endpoint, artifacts) = fixture();
    let wrong = EndpointRef {
        public_key: vec![1; 32],
        ..endpoint.clone()
    };
    assert!(matches!(
        Validation::new(open.clone(), ready.clone(), Some(&wrong), Limits::default()),
        Err(Error::Invalid(
            "admission does not match requested endpoint and revision"
        ))
    ));
    let mut download = Validation::new(open, ready.clone(), Some(&endpoint), Limits::default())
        .expect("admission");
    download
        .accept(chunk(&ready, 0, artifacts[0].clone()))
        .expect("pack");
    download
        .accept(chunk(&ready, 1, artifacts[1][..3].to_vec()))
        .expect("index prefix");
    assert!(matches!(
        download.accept(complete(&ready)),
        Err(Error::Invalid(
            "download is not a complete exact source closure"
        ))
    ));
}
#[test]
fn native_fetch_rejects_cross_spool_genesis_before_source() {
    let (open, mut ready, endpoint, _) = fixture();
    ready
        .owner_genesis
        .as_mut()
        .expect("owner")
        .genesis
        .as_mut()
        .expect("genesis")
        .spool_uuid = uuid::Uuid::now_v7().as_bytes().to_vec();
    assert!(matches!(
        Validation::new(open, ready, Some(&endpoint), Limits::default()),
        Err(Error::Invalid(
            "original owner genesis must bind this spool"
        ))
    ));
}
