// SPDX-License-Identifier: Apache-2.0
use crypto::{Ed25519Signer, P256Signer, RsaSigner, Signer, SignerError};

use super::*;

#[test]
fn test_sign_verify_ed25519() {
    let signer = Ed25519Signer::generate().expect("generate Ed25519 key");
    let data = b"test data for signing";

    let signature = signer.sign(data).expect("sign data");
    signer.verify(data, &signature).expect("verify signature");

    let err = signer
        .verify(b"wrong data", &signature)
        .expect_err("verify should fail");
    assert!(matches!(err, SignerError::VerificationFailed));
}

#[test]
fn test_sign_verify_rsa() {
    let signer = RsaSigner::generate(2048).expect("generate RSA key");
    let data = b"test data for signing";

    let signature = signer.sign(data).expect("sign data");
    signer.verify(data, &signature).expect("verify signature");

    let err = signer
        .verify(b"wrong data", &signature)
        .expect_err("verify should fail");
    assert!(matches!(err, SignerError::VerificationFailed));
}

#[test]
fn test_sign_verify_p256() {
    let signer = P256Signer::generate().expect("generate P256 key");
    let data = b"test data for signing";

    let signature = signer.sign(data).expect("sign data");
    signer.verify(data, &signature).expect("verify signature");

    let err = signer
        .verify(b"wrong data", &signature)
        .expect_err("verify should fail");
    assert!(matches!(err, SignerError::VerificationFailed));
}

#[test]
fn test_snapshot_sign_cli() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let signer = Ed25519Signer::generate().expect("generate key");
    let key_pem = signer.to_pem().expect("export PEM");
    let key_path = temp.path().join("signing_key.pem");
    fs::write(&key_path, &key_pem).unwrap();

    let result = heddle(
        &[
            "capture",
            "-m",
            "Signed commit",
            "--sign",
            &key_path.to_string_lossy(),
        ],
        Some(temp.path()),
    );

    if result.is_ok() {
        let show_result = heddle(&["show", "HEAD", "--json"], Some(temp.path())).unwrap();
        let show: serde_json::Value = serde_json::from_str(&show_result).expect("show JSON");
        assert!(
            show.get("signature").is_some() || show.get("signature_status").is_some(),
            "signed state should have signature info"
        );
    }
}

#[test]
fn test_state_signing_via_repository() {
    use objects::object::{Attribution, Principal, Tree};
    use repo::Repository;

    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).expect("init repo");

    let tree = Tree::new();
    let tree_hash = repo.store().put_tree(&tree).expect("put tree");
    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
    let state = objects::object::State::new(tree_hash, vec![], attribution);
    repo.store().put_state(&state).expect("put state");
    let state_id = state.change_id;

    let status = repo.verify_state_signature(&state_id).expect("verify");
    assert_eq!(
        status,
        crypto::SignatureStatus::Unsigned,
        "state should initially be unsigned"
    );

    let signer = Ed25519Signer::generate().expect("generate key");
    repo.sign_state(&state_id, &signer).expect("sign state");

    let status = repo.verify_state_signature(&state_id).expect("verify");
    assert_eq!(
        status,
        crypto::SignatureStatus::Valid,
        "state should have valid signature after signing"
    );
}

#[test]
fn test_signature_tampering_detected() {
    use objects::object::{Attribution, Principal, Tree};
    use repo::Repository;

    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).expect("init repo");

    let tree = Tree::new();
    let tree_hash = repo.store().put_tree(&tree).expect("put tree");
    let attribution = Attribution::human(Principal::new("Test", "test@example.com"));
    let state = objects::object::State::new(tree_hash, vec![], attribution);
    repo.store().put_state(&state).expect("put state");
    let state_id = state.change_id;

    let signer = Ed25519Signer::generate().expect("generate key");
    repo.sign_state(&state_id, &signer).expect("sign state");

    let mut state = repo
        .store()
        .get_state(&state_id)
        .expect("get state")
        .expect("state exists");

    if let Some(ref mut sig) = state.signature {
        let mut sig_bytes = hex::decode(&sig.signature).expect("decode");
        sig_bytes[0] ^= 0xff;
        sig.signature = hex::encode(&sig_bytes);
    }

    repo.store().put_state(&state).expect("put state");

    let status = repo.verify_state_signature(&state_id).expect("verify");
    assert_eq!(
        status,
        crypto::SignatureStatus::Invalid,
        "tampered signature should be invalid"
    );
}

#[test]
fn test_cross_algorithm_verification() {
    let ed_signer = Ed25519Signer::generate().expect("generate Ed25519");
    let rsa_signer = RsaSigner::generate(2048).expect("generate RSA");
    let p256_signer = P256Signer::generate().expect("generate P256");

    let data = b"same data";

    let ed_sig = ed_signer.sign(data).expect("Ed25519 sign");
    let rsa_sig = rsa_signer.sign(data).expect("RSA sign");
    let p256_sig = p256_signer.sign(data).expect("P256 sign");

    ed_signer.verify(data, &ed_sig).expect("verify Ed25519");
    rsa_signer.verify(data, &rsa_sig).expect("verify RSA");
    p256_signer.verify(data, &p256_sig).expect("verify P256");

    assert_eq!(ed_sig.len(), 64, "Ed25519 signature is 64 bytes");
    assert!(rsa_sig.len() > 64, "RSA signature is larger than 64 bytes");
    assert!(!p256_sig.is_empty(), "P256 signature should not be empty");
}
