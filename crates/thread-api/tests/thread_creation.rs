#![cfg(feature = "replication")]

use crypto::{Ed25519Signer, Signer};
use heddle_object_model::object::{StateId, thread_replication::ThreadGenesis};
use heddle_thread_api::{creation::ThreadCreation, replication::opening::verify_genesis};

fn genesis(signer: &Ed25519Signer) -> ThreadGenesis {
    ThreadGenesis {
        version: 1,
        spool: "01980000-0000-7000-8000-000000000001".into(),
        parent: None,
        base: StateId::from_bytes([17; 32]),
        name: "independent work".into(),
        intent: "retain identity while publishing".into(),
        creator: signer.public_key().try_into().expect("public key"),
        owner: heddle_object_model::object::thread_replication::GenesisOwner::LocalKey(
            signer.public_key().try_into().expect("public key"),
        ),
        nonce: vec![23; 16],
    }
}

#[test]
fn creation_and_later_publication_retain_the_original_record_and_thread_id() {
    let signer = Ed25519Signer::from_seed(&[11; 32]).expect("test creator");
    let genesis = genesis(&signer);
    let creation = ThreadCreation::sign("01980000-0000-7000-8000-000000000002", &genesis, &signer)
        .expect("prepare signed creation without a network call");
    assert_eq!(
        creation.reference().id.as_ref().expect("Thread ID").value,
        genesis.id().expect("native ID").as_bytes()
    );
    let original = creation
        .request()
        .thread_genesis
        .clone()
        .expect("signed genesis");
    assert_eq!(
        verify_genesis(&original, creation.reference()).expect("same replication verifier"),
        genesis
    );
    let retry =
        ThreadCreation::from_signed("01980000-0000-7000-8000-000000000003", original.clone())
            .expect("a later relay retains the creator's record");
    assert_eq!(retry.reference(), creation.reference());
    assert_eq!(retry.request().thread_genesis, Some(original));
    let mut independent = genesis;
    independent.nonce[0] += 1;
    let other = ThreadCreation::sign(
        "01980000-0000-7000-8000-000000000004",
        &independent,
        &signer,
    )
    .expect("separate attempt");
    assert_ne!(other.reference(), creation.reference());
}

#[test]
fn creation_rejects_a_changed_creator_record_and_invalid_operation_identity() {
    let signer = Ed25519Signer::from_seed(&[11; 32]).expect("creator");
    let genesis = genesis(&signer);
    assert!(ThreadCreation::sign("not-an-operation-uuid", &genesis, &signer).is_err());
    let other = Ed25519Signer::from_seed(&[12; 32]).expect("different key");
    assert!(
        ThreadCreation::sign("01980000-0000-7000-8000-000000000002", &genesis, &other).is_err()
    );
    let creation = ThreadCreation::sign("01980000-0000-7000-8000-000000000002", &genesis, &signer)
        .expect("signed genesis");
    let mut changed = creation.request().thread_genesis.clone().expect("record");
    let mut claimed = genesis;
    claimed.name = "different initial name".into();
    changed.canonical_record = claimed.encode().expect("changed canonical record");
    assert!(ThreadCreation::from_signed("01980000-0000-7000-8000-000000000002", changed).is_err());
}

#[test]
fn creation_rejects_mutable_or_noncanonical_spool_identity() {
    let signer = Ed25519Signer::from_seed(&[11; 32]).expect("creator");
    for spool in [
        "org/name",
        "00000000-0000-0000-0000-000000000000",
        "01980000-0000-7000-8000-ABCDEFABCDEF",
    ] {
        let mut genesis = genesis(&signer);
        genesis.spool = spool.into();
        assert!(
            ThreadCreation::sign("01980000-0000-7000-8000-000000000002", &genesis, &signer)
                .is_err(),
            "a private Thread must be publishable without changing its identity: {spool}"
        );
    }
}

#[test]
fn signed_genesis_matches_the_cross_language_vector() {
    let signer = Ed25519Signer::from_seed(&[11; 32]).expect("public test seed");
    let genesis = genesis(&signer);
    let creation = ThreadCreation::sign("01980000-0000-7000-8000-000000000002", &genesis, &signer)
        .expect("creation");
    let record = creation.request().thread_genesis.as_ref().expect("record");
    let values: std::collections::BTreeMap<_, _> = include_str!("fixtures/thread-genesis-v1.txt")
        .lines()
        .map(|line| line.split_once('=').expect("vector entry"))
        .collect();
    assert_eq!(hex::encode(&record.canonical_record), values["canonical"]);
    assert_eq!(
        hex::encode(&record.signatures[0].signature),
        values["signature"]
    );
    assert_eq!(hex::encode(genesis.creator), values["key"]);
    assert_eq!(genesis.id().expect("ID").to_hex(), values["id"]);
}
