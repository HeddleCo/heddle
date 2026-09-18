#![cfg(feature = "replication")]

use crypto::{Ed25519Signer, Signer, thread_operation::SignedGenesis};
use heddle_object_model::object::thread_replication::{GenesisOwner, ThreadGenesis, hosted_import};

#[test]
fn account_thread_genesis_matches_browser_fixture() {
    let signer = Ed25519Signer::from_seed(&[42; 32]).expect("fixed test creator");
    let seed = hosted_import::synthetic_initial_base().expect("system pre-history");
    let genesis = ThreadGenesis {
        version: 1,
        spool: "123e4567-e89b-12d3-a456-426614174000".into(),
        parent: None,
        base: seed.id(),
        name: "imported-project".into(),
        intent: "Import the granted repository".into(),
        owner: GenesisOwner::Account(
            uuid::Uuid::parse_str("123e4567-e89b-12d3-a456-426614174001").expect("fixed account"),
        ),
        creator: signer.public_key().try_into().expect("Ed25519 key"),
        nonce: vec![7; 16],
    };
    let signed = SignedGenesis::sign(&genesis, &signer).expect("creator signed canonical record");
    assert_eq!(
        signed
            .verify()
            .expect("independent Rust signature verifier"),
        genesis
    );
    let expected = include_str!("fixtures/thread-genesis-browser-v1.txt");
    let actual = format!(
        "canonical={}\nsignature={}\nkey={}\nid={}\nbase={}\n",
        hex::encode(&signed.canonical),
        hex::encode(&signed.signature),
        hex::encode(signer.public_key()),
        hex::encode(genesis.id().expect("identity").as_bytes()),
        hex::encode(seed.id().as_bytes()),
    );
    assert_eq!(actual, expected, "browser and Rust must share exact bytes");
}
