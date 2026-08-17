// SPDX-License-Identifier: Apache-2.0

use api::heddle::api::v1alpha1::{
    AuthorizationKeyAlgorithm, AuthorizationSignature, AuthorizationVerificationKey,
    PurgeOperationSigningBody, SidecarAuthorization, SignedSpoolOwnerGenesis, SpoolOwnerGenesis,
};
use crypto::Signer;
use heddleco_capability_verifier::{
    Decision,
    conformance::{ConformanceFixture, FIXTURE_V2_JSON, run_fixture},
};
use prost::Message;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::Repository;

fn purge_fixture() -> ConformanceFixture {
    serde_json::from_str(FIXTURE_V2_JSON).expect("published purge fixture")
}

fn decode<T: Message + Default>(value: &str) -> T {
    T::decode(hex::decode(value).expect("fixture hex").as_slice()).expect("fixture protobuf")
}

fn pinned_repository() -> (TempDir, Repository, ConformanceFixture) {
    let fixture = purge_fixture();
    let valid = fixture
        .cases
        .iter()
        .find(|case| case.name == "owner-anchored-purge")
        .expect("valid fixture case");
    let genesis: SignedSpoolOwnerGenesis = decode(&valid.owner_genesis_hex);
    let temp = TempDir::new().expect("temp repo");
    let repo = Repository::init_default(temp.path()).expect("init repo");
    repo.verify_and_pin_owner_genesis(2, Some(&genesis), &valid.spool_path_segments)
        .expect("pin fixture genesis");
    (temp, repo, fixture)
}

fn generated_genesis(spool_uuid: [u8; 16]) -> SignedSpoolOwnerGenesis {
    let signer = crypto::Ed25519Signer::generate().expect("owner keypair");
    let owner_public_key = AuthorizationVerificationKey {
        algorithm: AuthorizationKeyAlgorithm::Ed25519 as i32,
        public_key: signer.public_key().to_vec(),
    };
    let mut key_id_body = Vec::with_capacity(36);
    key_id_body.extend_from_slice(&owner_public_key.algorithm.to_be_bytes());
    key_id_body.extend_from_slice(&owner_public_key.public_key);
    let signer_key_id = Sha256::new()
        .chain_update(b"heddle-key-v1")
        .chain_update(key_id_body)
        .finalize()
        .to_vec();
    let digest = Sha256::new()
        .chain_update(&owner_public_key.public_key)
        .chain_update(spool_uuid)
        .finalize();
    SignedSpoolOwnerGenesis {
        genesis: Some(SpoolOwnerGenesis {
            spool_uuid: spool_uuid.to_vec(),
            owner_public_key: Some(owner_public_key),
        }),
        owner_signature: Some(AuthorizationSignature {
            signer_key_id,
            signature: signer.sign(&digest).expect("sign owner genesis"),
        }),
    }
}

#[test]
fn published_purge_accept_deny_matrix_holds_against_clone_pin() {
    assert!(
        run_fixture(FIXTURE_V2_JSON)
            .expect("run canonical published matrix")
            .iter()
            .all(|outcome| outcome.matches),
        "published verifier matrix must match every expected decision"
    );
    let (_temp, repo, fixture) = pinned_repository();
    for case in fixture
        .cases
        .iter()
        // These two fixture cases vary caller-owned context. Heddle exercises
        // them separately through clone pinning below and in the genesis test.
        .filter(|case| !matches!(case.name.as_str(), "wrong-spool" | "forged-genesis"))
    {
        let authorization: SidecarAuthorization = decode(&case.authorization_hex);
        let body: PurgeOperationSigningBody = decode(&case.operation_body_hex);
        let blob_hash = &body
            .purge_identity
            .as_ref()
            .expect("fixture purge identity")
            .blob_hash;
        let payload = hex::decode(&case.payload_hex).expect("fixture payload");
        let result = repo.verify_owner_purge_authorization(
            blob_hash,
            &payload,
            Some(&authorization),
            case.now_unix_seconds,
        );
        let should_accept = matches!(case.expected, Decision::Purge);
        assert_eq!(
            result.is_ok(),
            should_accept,
            "published case {} returned {result:#?}",
            case.name
        );
    }

    let valid = fixture
        .cases
        .iter()
        .find(|case| case.name == "owner-anchored-purge")
        .expect("valid fixture case");
    let authorization: SidecarAuthorization = decode(&valid.authorization_hex);
    let body: PurgeOperationSigningBody = decode(&valid.operation_body_hex);
    let payload = hex::decode(&valid.payload_hex).expect("fixture payload");
    let other = TempDir::new().expect("other spool repo");
    let other_repo = Repository::init_default(other.path()).expect("other spool repo");
    let other_genesis = generated_genesis([0x33; 16]);
    other_repo
        .verify_and_pin_owner_genesis(2, Some(&other_genesis), &["other".to_owned()])
        .expect("pin another spool");
    other_repo
        .verify_owner_purge_authorization(
            &body.purge_identity.expect("fixture identity").blob_hash,
            &payload,
            Some(&authorization),
            valid.now_unix_seconds,
        )
        .expect_err("an authorization for another spool must be denied");
}

#[test]
fn purge_without_authorization_is_denied_even_with_legacy_trust_tables() {
    let (temp, _repo, fixture) = pinned_repository();
    let config_path = temp.path().join(".heddle/config.toml");
    let mut config = std::fs::read_to_string(&config_path).expect("read config");
    config.push_str(
        "\n[redact]\ntrusted_keys = [{ algorithm = \"ed25519\", public_key = \"00\" }]\n\
         [metadata]\ntrusted_keys = [{ algorithm = \"ed25519\", public_key = \"00\" }]\n\
         [purge]\ntrusted_keys = [{ algorithm = \"ed25519\", public_key = \"00\" }]\n",
    );
    std::fs::write(&config_path, config).expect("write legacy config");
    let repo = Repository::open(temp.path()).expect("legacy tables are ignored");
    let valid = fixture
        .cases
        .iter()
        .find(|case| case.name == "owner-anchored-purge")
        .expect("valid fixture case");
    let body: PurgeOperationSigningBody = decode(&valid.operation_body_hex);
    let blob_hash = &body
        .purge_identity
        .as_ref()
        .expect("fixture purge identity")
        .blob_hash;
    let payload = hex::decode(&valid.payload_hex).expect("fixture payload");
    repo.verify_owner_purge_authorization(blob_hash, &payload, None, valid.now_unix_seconds)
        .expect_err("legacy trusted_keys must not grant purge authority");
}

#[test]
fn clone_pin_rejects_forged_and_later_first_seen_genesis() {
    let fixture = purge_fixture();
    let forged = fixture
        .cases
        .iter()
        .find(|case| case.name == "forged-genesis")
        .expect("forged fixture case");
    let forged_genesis: SignedSpoolOwnerGenesis = decode(&forged.owner_genesis_hex);
    let fresh = TempDir::new().expect("fresh temp repo");
    let fresh_repo = Repository::init_default(fresh.path()).expect("fresh repo");
    fresh_repo
        .verify_and_pin_owner_genesis(2, Some(&forged_genesis), &forged.spool_path_segments)
        .expect_err("forged self-signature must not establish a pin");
    assert!(
        !fresh
            .path()
            .join(".heddle/owner-authorization.bin")
            .exists()
    );

    let (_temp, repo, valid_fixture) = pinned_repository();
    let other_genesis = generated_genesis([0x44; 16]);
    let path = &valid_fixture.cases[0].spool_path_segments;
    let error = repo
        .verify_and_pin_owner_genesis(2, Some(&other_genesis), path)
        .expect_err("a later first-seen valid genesis must not replace the TOFU pin");
    assert!(
        error.to_string().contains("first-operation trust"),
        "{error:#}"
    );
}
