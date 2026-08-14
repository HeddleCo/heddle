//! Cross-language golden vectors for the complete v2 verdict signing scheme.

mod ci_verdict_support;

use ci_verdict_support::{branch_basis_body, maximal_body, passing_body, test_signer};
use crypto::{
    CI_VERDICT_DOMAIN, CiVerdictBody, Signer, SignerKind, ci_verdict_signing_payload,
    signed_verdict_from_signer,
};
use objects::object::{ChangeId, ContentHash};
use serde_json::Value;

fn fixture(name: &str) -> CiVerdictBody {
    match name {
        "passing_body" => passing_body(),
        "maximal_body" => maximal_body(),
        "branch_basis_body" => branch_basis_body(),
        other => panic!("unknown golden-vector fixture {other:?}"),
    }
}

fn signer_kind(name: &str) -> SignerKind {
    match name {
        "service_account" => SignerKind::ServiceAccount,
        "device" => SignerKind::Device,
        other => panic!("unknown golden-vector signer kind {other:?}"),
    }
}

fn change_id(value: &Value) -> ChangeId {
    let bytes = hex::decode(value.as_str().expect("change_id_hex")).expect("change id hex");
    ChangeId::try_from_slice(&bytes).expect("16-byte change id")
}

#[test]
fn golden_vectors_reproduce_canonical_body_hash_preimage_and_signature() {
    let document: Value = serde_json::from_str(include_str!("fixtures/ci_verdict_v2.json"))
        .expect("golden vectors are valid JSON");
    let signer = test_signer();

    assert_eq!(
        hex::encode(CI_VERDICT_DOMAIN),
        document["signing_payload_version_tag_hex"]
            .as_str()
            .expect("tag hex")
    );
    assert_eq!(
        hex::encode(signer.public_key()),
        document["test_key"]["public_key_hex"]
            .as_str()
            .expect("public key hex")
    );

    let vectors = document["vectors"].as_array().expect("vectors array");
    assert!(
        !vectors.is_empty(),
        "at least one golden vector is required"
    );
    for vector in vectors {
        let name = vector["name"].as_str().expect("vector name");
        let body = fixture(vector["fixture"].as_str().expect("fixture name"));
        let kind = signer_kind(vector["signer_kind"].as_str().expect("signer kind"));
        let signed_at = vector["signed_at"].as_str().expect("signed_at");
        let change_id = change_id(&vector["change_id_hex"]);
        let tree_digest =
            ContentHash::from_hex(vector["tree_digest_hex"].as_str().expect("tree_digest_hex"))
                .expect("tree digest hex");

        assert_eq!(
            String::from_utf8(body.canonical_bytes()).expect("canonical JSON is UTF-8"),
            vector["canonical_bytes_utf8"]
                .as_str()
                .expect("canonical bytes"),
            "[{name}] canonical body bytes drifted"
        );
        let content_hash = body.content_hash();
        assert_eq!(
            content_hash.to_hex(),
            vector["content_hash_hex"]
                .as_str()
                .expect("content_hash_hex"),
            "[{name}] content hash drifted"
        );
        let payload =
            ci_verdict_signing_payload(&content_hash, &change_id, &tree_digest, kind, signed_at);
        assert_eq!(
            hex::encode(payload),
            vector["signing_preimage_hex"]
                .as_str()
                .expect("signing preimage"),
            "[{name}] signing preimage drifted"
        );

        let signed = signed_verdict_from_signer(
            body,
            &change_id,
            &tree_digest,
            kind,
            signed_at.to_string(),
            &signer,
        )
        .expect("sign golden vector");
        assert_eq!(
            signed.signature,
            vector["signature_hex"].as_str().expect("signature hex"),
            "[{name}] signature drifted"
        );
        signed
            .verify()
            .unwrap_or_else(|error| panic!("[{name}] golden vector must verify: {error}"));
    }
}
