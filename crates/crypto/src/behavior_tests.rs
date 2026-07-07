// SPDX-License-Identifier: Apache-2.0

use objects::{
    fs_atomic::write_file_atomic_secret,
    object::{Attribution, ContentHash, Principal, State},
};
use tempfile::TempDir;

use crate::{
    Ed25519Signer, P256Signer, Signer, SignerError, StateSignatureError, StateSigningExt,
    load_signer, verify_payload_signature,
};

fn assert_verifies_with_dispatch(signer: &dyn Signer, payload: &[u8]) {
    let signature = signer.sign(payload).expect("sign payload");

    verify_payload_signature(payload, signer.algorithm(), signer.public_key(), &signature)
        .expect("dispatch verifies signature");
}

fn assert_dispatch_rejects_wrong_payload_key_and_signature(signer: &dyn Signer, other_key: &[u8]) {
    let payload = b"payload covered by the signature";
    let signature = signer.sign(payload).expect("sign payload");

    verify_payload_signature(
        b"different payload",
        signer.algorithm(),
        signer.public_key(),
        &signature,
    )
    .expect_err("wrong payload must fail verification");

    verify_payload_signature(payload, signer.algorithm(), other_key, &signature)
        .expect_err("wrong key must fail verification");

    let mut corrupted_signature = signature;
    let first = corrupted_signature
        .first_mut()
        .expect("generated signatures are non-empty");
    *first ^= 0x01;
    verify_payload_signature(
        payload,
        signer.algorithm(),
        signer.public_key(),
        &corrupted_signature,
    )
    .expect_err("wrong signature must fail verification");
}

fn write_key(dir: &TempDir, name: &str, pem: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    write_file_atomic_secret(&path, pem.as_bytes()).expect("write private key");
    path
}

fn assert_loads_pkcs8_explicitly_and_implicitly(
    expected_algorithm: &str,
    expected_public_key: &[u8],
    pem: &str,
    explicit_algorithm: &str,
    file_name: &str,
) {
    let temp = TempDir::new().expect("create temp dir");
    let path = write_key(&temp, file_name, pem);

    let explicit = load_signer(&path, Some(explicit_algorithm)).expect("explicit algorithm loads");
    assert_eq!(explicit.algorithm(), expected_algorithm);
    assert_eq!(explicit.public_key(), expected_public_key);
    assert_verifies_with_dispatch(explicit.as_ref(), b"explicitly loaded key signs");

    let implicit = load_signer(&path, None).expect("implicit algorithm loads");
    assert_eq!(implicit.algorithm(), expected_algorithm);
    assert_eq!(implicit.public_key(), expected_public_key);
    assert_verifies_with_dispatch(implicit.as_ref(), b"implicitly loaded key signs");
}

fn sample_state() -> State {
    State::new(
        ContentHash::compute(b"crypto behavior test tree"),
        vec![],
        Attribution::human(Principal::new("Crypto Test", "crypto@example.com")),
    )
}

#[test]
fn verify_payload_signature_dispatches_supported_signers() {
    let ed25519 = Ed25519Signer::generate().expect("generate Ed25519 signer");
    let p256 = P256Signer::generate().expect("generate P-256 signer");
    let payload = b"dispatch payload";

    assert_verifies_with_dispatch(&ed25519, payload);
    assert_verifies_with_dispatch(&p256, payload);
}

#[test]
fn verify_payload_signature_accepts_ecdsa_p256_alias() {
    let signer = P256Signer::generate().expect("generate P-256 signer");
    let payload = b"P-256 alias payload";
    let signature = signer.sign(payload).expect("sign payload");

    verify_payload_signature(payload, "ecdsa-p256", signer.public_key(), &signature)
        .expect("ECDSA P-256 alias verifies through P-256 backend");
}

#[test]
fn verify_payload_signature_rejects_wrong_payload_key_and_signature() {
    let ed25519 = Ed25519Signer::generate().expect("generate Ed25519 signer");
    let other_ed25519 = Ed25519Signer::generate().expect("generate other Ed25519 signer");
    assert_dispatch_rejects_wrong_payload_key_and_signature(&ed25519, other_ed25519.public_key());

    let p256 = P256Signer::generate().expect("generate P-256 signer");
    let other_p256 = P256Signer::generate().expect("generate other P-256 signer");
    assert_dispatch_rejects_wrong_payload_key_and_signature(&p256, other_p256.public_key());
}

#[test]
fn verify_payload_signature_rejects_unsupported_algorithm() {
    let err = verify_payload_signature(b"payload", "dilithium3", b"public", b"signature")
        .expect_err("unsupported algorithm must fail");

    assert!(
        matches!(err, SignerError::UnsupportedAlgorithm(algorithm) if algorithm == "dilithium3")
    );
}

#[test]
fn load_signer_reads_generated_pkcs8_keys_explicitly_and_implicitly() {
    let ed25519 = Ed25519Signer::generate().expect("generate Ed25519 signer");
    let ed25519_pem = ed25519.to_pem().expect("export Ed25519 PEM");
    assert_loads_pkcs8_explicitly_and_implicitly(
        "ed25519",
        ed25519.public_key(),
        &ed25519_pem,
        "ed25519",
        "ed25519.pem",
    );

    let p256 = P256Signer::generate().expect("generate P-256 signer");
    let p256_pem = p256.to_pem().expect("export P-256 PEM");
    assert_loads_pkcs8_explicitly_and_implicitly(
        "p256",
        p256.public_key(),
        &p256_pem,
        "p256",
        "p256.pem",
    );
}

#[test]
fn ed25519_pkcs8_loader_accepts_trailing_blank_line() {
    let signer = Ed25519Signer::generate().expect("generate Ed25519 signer");
    let mut pem = signer.to_pem().expect("export Ed25519 PEM");
    pem.push('\n');

    let loaded = Ed25519Signer::from_pem(&pem).expect("load Ed25519 PEM with trailing blank line");
    assert_eq!(loaded.public_key(), signer.public_key());
}

#[test]
fn load_signer_accepts_ecdsa_p256_alias_for_generated_pkcs8_key() {
    let signer = P256Signer::generate().expect("generate P-256 signer");
    let temp = TempDir::new().expect("create temp dir");
    let pem = signer.to_pem().expect("export P-256 PEM");
    let path = write_key(&temp, "p256.pem", &pem);

    let loaded = load_signer(&path, Some("ecdsa-p256")).expect("load alias");

    assert_eq!(loaded.algorithm(), "p256");
    assert_eq!(loaded.public_key(), signer.public_key());
    assert_verifies_with_dispatch(loaded.as_ref(), b"alias-loaded key signs");
}

#[test]
fn load_signer_rejects_unsupported_algorithm_hint_and_key_formats() {
    let temp = TempDir::new().expect("create temp dir");
    let signer = Ed25519Signer::generate().expect("generate Ed25519 signer");
    let pem = signer.to_pem().expect("export Ed25519 PEM");
    let path = write_key(&temp, "ed25519.pem", &pem);

    let err = match load_signer(&path, Some("ecdsa-p384")) {
        Ok(_) => panic!("unsupported hint must fail"),
        Err(err) => err,
    };
    assert!(
        matches!(err, SignerError::UnsupportedAlgorithm(algorithm) if algorithm == "ecdsa-p384")
    );

    let err = match load_signer(&path, Some("rsa")) {
        Ok(_) => panic!("removed RSA hint must fail"),
        Err(err) => err,
    };
    assert!(matches!(err, SignerError::UnsupportedAlgorithm(algorithm) if algorithm == "rsa"));

    let unsupported_path = write_key(
        &temp,
        "certificate.pem",
        "-----BEGIN CERTIFICATE-----\nnot-a-private-key\n-----END CERTIFICATE-----\n",
    );
    let err = match load_signer(&unsupported_path, None) {
        Ok(_) => panic!("unsupported PEM must fail"),
        Err(err) => err,
    };
    assert!(matches!(err, SignerError::UnknownKeyFormat));

    let openssh_path = write_key(
        &temp,
        "openssh.pem",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\n",
    );
    let err = match load_signer(&openssh_path, None) {
        Ok(_) => panic!("OpenSSH PEM must fail"),
        Err(err) => err,
    };
    assert!(matches!(err, SignerError::Pem(message) if message.contains("OpenSSH")));
}

#[test]
fn invalid_seed_public_keys_and_signature_shapes_are_rejected() {
    let err = match Ed25519Signer::from_seed(&[7; 31]) {
        Ok(_) => panic!("short seed must fail"),
        Err(err) => err,
    };
    assert!(matches!(err, SignerError::InvalidKey(message) if message.contains("32 bytes")));

    let payload = b"payload";
    let ed25519 = Ed25519Signer::generate().expect("generate Ed25519 signer");
    let ed25519_signature = ed25519.sign(payload).expect("sign payload");
    let err = verify_payload_signature(payload, "ed25519", &[0; 31], &ed25519_signature)
        .expect_err("short Ed25519 public key fails");
    assert!(matches!(err, SignerError::InvalidPublicKey(message) if message.contains("32 bytes")));
    let err = verify_payload_signature(payload, "ed25519", ed25519.public_key(), &[0; 63])
        .expect_err("short Ed25519 signature fails");
    assert!(matches!(err, SignerError::InvalidSignature(message) if message.contains("64 bytes")));

    let p256 = P256Signer::generate().expect("generate P-256 signer");
    let p256_signature = p256.sign(payload).expect("sign payload");
    let err = verify_payload_signature(payload, "p256", b"not a SEC1 point", &p256_signature)
        .expect_err("invalid P-256 public key fails");
    assert!(matches!(err, SignerError::InvalidPublicKey(_)));
    let err = verify_payload_signature(payload, "p256", p256.public_key(), b"not-a-p256-signature")
        .expect_err("invalid P-256 signature shape fails");
    assert!(matches!(err, SignerError::InvalidSignature(_)));
}

#[test]
fn state_signing_verifies_signed_state_and_rejects_tamper_or_unsigned_state() {
    let signer = Ed25519Signer::generate().expect("generate signer");
    let mut state = sample_state();

    let err = state
        .verify_signature()
        .expect_err("unsigned state must not verify");
    assert!(
        matches!(err, StateSignatureError::InvalidSignature(message) if message.contains("no signature"))
    );

    state.sign(&signer).expect("sign state");
    state
        .verify_signature()
        .expect("freshly signed state verifies");

    let tampered = state.with_intent("tampered after signing");
    let err = tampered
        .verify_signature()
        .expect_err("tampered state must not verify");
    assert!(matches!(
        err,
        StateSignatureError::Signer(SignerError::VerificationFailed)
    ));
}
