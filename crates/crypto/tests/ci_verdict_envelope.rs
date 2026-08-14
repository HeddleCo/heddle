mod ci_verdict_support;

use ci_verdict_support::{SIGNED_AT, bindings, passing_body, sign, test_signer};
use crypto::{
    CI_VERDICT_BODY_SCHEMA_VERSION, CI_VERDICT_DOMAIN, Ed25519Signer,
    SIGNED_VERDICT_FORMAT_VERSION, SignedVerdict, SignedVerdictError, Signer, SignerError,
    SignerKind, ci_verdict_signing_payload, signed_verdict_from_signer,
};
use objects::object::{ChangeId, ContentHash};
use serde_json::Value;

fn verification_failed(result: Result<(), SignedVerdictError>) -> bool {
    matches!(
        result,
        Err(SignedVerdictError::Signer(SignerError::VerificationFailed))
    )
}

#[test]
fn v2_payload_pins_domain_and_every_binding_in_order() {
    let body = passing_body();
    let (change_id, tree_digest) = bindings();
    let content_hash = body.content_hash();
    let payload = ci_verdict_signing_payload(
        &content_hash,
        &change_id,
        &tree_digest,
        SignerKind::ServiceAccount,
        SIGNED_AT,
    );

    assert_eq!(&payload[..21], b"heddle-ci-verdict-v2\0");
    assert_eq!(&payload[21..53], content_hash.as_bytes());
    assert_eq!(&payload[53..69], &[2; 16]);
    assert_eq!(&payload[69..101], &[3; 32]);
    assert_eq!(&payload[101..117], b"service_account\0");
    assert_eq!(&payload[117..], b"2026-06-11T18:05:54Z\0");
    assert_eq!(CI_VERDICT_DOMAIN, b"heddle-ci-verdict-v2\0");
}

#[test]
fn signatures_from_the_colliding_v1_scheme_are_rejected() {
    let mut signed = sign(passing_body(), SignerKind::ServiceAccount, SIGNED_AT);
    let signer = test_signer();
    let mut legacy_payload = Vec::new();
    legacy_payload.extend_from_slice(b"heddle-ci-verdict-v1\0");
    legacy_payload.extend_from_slice(signed.content_hash.as_bytes());
    legacy_payload.extend_from_slice(signed.change_id.as_bytes());
    legacy_payload.extend_from_slice(signed.tree_digest.as_bytes());
    signed.signature = hex::encode(signer.sign(&legacy_payload).expect("sign legacy payload"));

    assert!(verification_failed(signed.verify()));
}

#[test]
fn each_signed_envelope_binding_rejects_tampering() {
    let verdict = sign(passing_body(), SignerKind::Device, SIGNED_AT);

    let mut content_hash = verdict.clone();
    content_hash.content_hash = ContentHash::from_bytes([9; 32]);
    assert!(matches!(
        content_hash.verify(),
        Err(SignedVerdictError::BodyDigestMismatch { .. })
    ));

    let mut change_id = verdict.clone();
    change_id.change_id = ChangeId::from_bytes([9; 16]);
    assert!(verification_failed(change_id.verify()));

    let mut tree_digest = verdict.clone();
    tree_digest.tree_digest = ContentHash::from_bytes([9; 32]);
    assert!(verification_failed(tree_digest.verify()));

    let mut signer_kind = verdict.clone();
    signer_kind.signer_kind = SignerKind::ServiceAccount;
    assert!(verification_failed(signer_kind.verify()));

    let mut signed_at = verdict;
    signed_at.signed_at = "2026-06-11T18:05:55Z".to_string();
    assert!(verification_failed(signed_at.verify()));
}

#[test]
fn key_algorithm_and_signature_tampering_are_rejected() {
    let verdict = sign(passing_body(), SignerKind::ServiceAccount, SIGNED_AT);

    let mut algorithm = verdict.clone();
    algorithm.algorithm = "unknown".to_string();
    assert!(matches!(
        algorithm.verify(),
        Err(SignedVerdictError::Signer(
            SignerError::UnsupportedAlgorithm(_)
        ))
    ));

    let mut public_key = verdict.clone();
    let other = Ed25519Signer::from_seed(&[8; 32]).expect("other signer");
    public_key.public_key = hex::encode(other.public_key());
    assert!(verification_failed(public_key.verify()));

    let mut signature = verdict;
    let mut bytes = hex::decode(&signature.signature).expect("signature hex");
    bytes[0] ^= 1;
    signature.signature = hex::encode(bytes);
    assert!(verification_failed(signature.verify()));
}

#[test]
fn unsupported_versions_are_distinct_from_body_tampering() {
    let verdict = sign(passing_body(), SignerKind::ServiceAccount, SIGNED_AT);

    let mut old_envelope = verdict.clone();
    old_envelope.format_version = 1;
    assert!(matches!(
        old_envelope.verify(),
        Err(SignedVerdictError::UnsupportedFormatVersion {
            found: 1,
            supported: SIGNED_VERDICT_FORMAT_VERSION,
        })
    ));

    let mut newer_schema = verdict.clone();
    newer_schema.body.schema_version = CI_VERDICT_BODY_SCHEMA_VERSION + 1;
    assert!(matches!(
        newer_schema.verify(),
        Err(SignedVerdictError::UnsupportedSchemaVersion { .. })
    ));
    assert!(!matches!(
        newer_schema.verify(),
        Err(SignedVerdictError::BodyDigestMismatch { .. })
    ));

    let mut old_schema = verdict;
    old_schema.body.schema_version = 0;
    assert!(matches!(
        old_schema.verify(),
        Err(SignedVerdictError::UnsupportedSchemaVersion {
            found: 0,
            supported: CI_VERDICT_BODY_SCHEMA_VERSION,
        })
    ));
}

#[test]
fn signing_rejects_unsupported_schema_and_malformed_signed_at() {
    let signer = test_signer();
    let (change_id, tree_digest) = bindings();
    let mut old_body = passing_body();
    old_body.schema_version = 0;
    assert!(matches!(
        signed_verdict_from_signer(
            old_body,
            &change_id,
            &tree_digest,
            SignerKind::ServiceAccount,
            SIGNED_AT.to_string(),
            &signer,
        ),
        Err(SignedVerdictError::UnsupportedSchemaVersion { found: 0, .. })
    ));

    assert!(matches!(
        signed_verdict_from_signer(
            passing_body(),
            &change_id,
            &tree_digest,
            SignerKind::ServiceAccount,
            "not-a-timestamp".to_string(),
            &signer,
        ),
        Err(SignedVerdictError::InvalidSignedAt(_))
    ));

    let mut malformed = sign(passing_body(), SignerKind::ServiceAccount, SIGNED_AT);
    malformed.signed_at = "not-a-timestamp".to_string();
    assert!(matches!(
        malformed.verify(),
        Err(SignedVerdictError::InvalidSignedAt(_))
    ));
}

#[test]
fn device_verdicts_are_explicitly_advisory_only() {
    let device = sign(passing_body(), SignerKind::Device, SIGNED_AT);
    let service = sign(passing_body(), SignerKind::ServiceAccount, SIGNED_AT);

    assert!(device.is_advisory_only());
    assert!(!service.is_advisory_only());
}

#[test]
fn every_v2_envelope_field_is_required_on_the_wire() {
    let encoded = serde_json::to_value(sign(passing_body(), SignerKind::ServiceAccount, SIGNED_AT))
        .expect("encode verdict");

    for field in [
        "format_version",
        "body",
        "content_hash",
        "change_id",
        "tree_digest",
        "signer_kind",
        "signed_at",
        "algorithm",
        "public_key",
        "signature",
    ] {
        let mut incomplete = encoded.clone();
        let Value::Object(ref mut object) = incomplete else {
            panic!("verdict must serialize as an object");
        };
        object.remove(field);
        assert!(
            serde_json::from_value::<SignedVerdict>(incomplete).is_err(),
            "missing {field} must not deserialize through a fallback"
        );
    }
}

#[test]
fn signed_verdict_round_trips_losslessly_and_verifies() {
    let verdict = sign(passing_body(), SignerKind::ServiceAccount, SIGNED_AT);
    let encoded = serde_json::to_vec(&verdict).expect("encode signed verdict");
    let decoded: SignedVerdict = serde_json::from_slice(&encoded).expect("decode signed verdict");

    assert_eq!(decoded, verdict);
    decoded.verify().expect("verify decoded verdict");
}
