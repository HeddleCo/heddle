//! Golden signing-vector conformance test.
//!
//! Loads `tests/fixtures/vectors.json` and, for each vector, rebuilds the named
//! fixture and asserts that this crate reproduces the pinned `canonical_bytes`,
//! `body_digest`, signing preimage, and `signature` exactly — and that the
//! resulting [`SignedVerdict`] verifies. This is the artifact a cross-language
//! re-implementation checks itself against (see the header comment in the JSON).
//!
//! If any assertion here fails after an intentional pre-freeze change, regenerate
//! the vectors deliberately (the bytes are produced by the same fixtures + the
//! fixed seed below) — do NOT loosen the test.

use ed25519_dalek::SigningKey;
use serde_json::Value;
use treadle_schema::{
    CiVerdictBody, SIGNING_PAYLOAD_VERSION_TAG, SignerKind, fixture, sign, signed_payload,
};

/// The fixed deterministic test seed pinned in `vectors.json` (`test_key.seed_hex`).
/// NEVER use this key for anything real.
const TEST_SEED: [u8; 32] = [1u8; 32];

fn hexstr(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Map a `fixture` name in the JSON to its constructor. A vector naming an unknown
/// fixture fails loudly rather than silently skipping.
fn fixture_by_name(name: &str) -> CiVerdictBody {
    match name {
        "passing_body" => fixture::passing_body(),
        "maximal_body" => fixture::maximal_body(),
        "merge_basis_body" => fixture::merge_basis_body(),
        "branch_basis_body" => fixture::branch_basis_body(),
        other => panic!("vectors.json references unknown fixture {other:?}"),
    }
}

fn signer_kind_by_name(name: &str) -> SignerKind {
    match name {
        "service_account" => SignerKind::ServiceAccount,
        "delegated" => SignerKind::Delegated,
        "device" => SignerKind::Device,
        other => panic!("vectors.json has unknown signer_kind {other:?}"),
    }
}

#[test]
fn golden_vectors_reproduce() {
    let raw = include_str!("fixtures/vectors.json");
    let doc: Value = serde_json::from_str(raw).expect("vectors.json is valid JSON");

    // The fixed key the vectors were signed with must match what the JSON declares.
    let key = SigningKey::from_bytes(&TEST_SEED);
    let declared_pub = doc["test_key"]["public_key_hex"]
        .as_str()
        .expect("test_key.public_key_hex");
    assert_eq!(
        hexstr(key.verifying_key().as_bytes()),
        declared_pub,
        "fixed test key drifted from vectors.json"
    );
    assert_eq!(
        doc["test_key"]["seed_hex"].as_str().unwrap(),
        hexstr(&TEST_SEED),
        "seed_hex in vectors.json must match the test seed"
    );

    let vectors = doc["vectors"].as_array().expect("vectors array");
    assert!(!vectors.is_empty(), "expected at least one vector");

    for v in vectors {
        let name = v["name"].as_str().expect("vector.name");
        let fixture_name = v["fixture"].as_str().expect("vector.fixture");
        let signed_at = v["signed_at"]
            .as_str()
            .expect("vector.signed_at")
            .to_string();
        let signer_kind = signer_kind_by_name(v["signer_kind"].as_str().expect("signer_kind"));

        let body = fixture_by_name(fixture_name);

        // 1. canonical bytes (the UTF-8 form is the human-readable pin).
        let cb = body.canonical_bytes();
        let cb_utf8 = String::from_utf8(cb.clone()).expect("canonical bytes are utf8");
        assert_eq!(
            cb_utf8,
            v["canonical_bytes_utf8"].as_str().unwrap(),
            "[{name}] canonical bytes drifted"
        );

        // 2. body digest.
        let digest = body.body_digest();
        assert_eq!(
            digest,
            v["body_digest"].as_str().unwrap(),
            "[{name}] body_digest drifted"
        );

        // 3. signing preimage — independently rebuild it and compare to the pin,
        //    and assert it actually begins with the domain tag.
        let preimage = signed_payload(&digest, signer_kind, &signed_at);
        assert!(
            preimage.starts_with(SIGNING_PAYLOAD_VERSION_TAG),
            "[{name}] preimage missing domain tag"
        );
        assert_eq!(
            hexstr(&preimage),
            v["signing_preimage_hex"].as_str().unwrap(),
            "[{name}] signing preimage drifted"
        );

        // 4. signature, and the signed verdict verifies.
        let signed = sign(body, &key, signed_at.clone(), signer_kind);
        assert_eq!(
            signed.signature,
            v["signature"].as_str().unwrap(),
            "[{name}] signature drifted"
        );
        assert_eq!(
            signed.body_digest, digest,
            "[{name}] embedded digest mismatch"
        );
        signed
            .verify()
            .unwrap_or_else(|e| panic!("[{name}] golden vector must verify: {e:?}"));
    }
}

#[test]
fn domain_tag_is_exactly_what_the_json_declares() {
    let raw = include_str!("fixtures/vectors.json");
    let doc: Value = serde_json::from_str(raw).unwrap();
    // Tolerate any incidental whitespace in the declared hex (there is none today,
    // but keep the comparison robust to a future reformat).
    let declared: String = doc["signing_payload_version_tag_hex"]
        .as_str()
        .unwrap()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert_eq!(hexstr(SIGNING_PAYLOAD_VERSION_TAG), declared);
}
