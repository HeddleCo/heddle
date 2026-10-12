// SPDX-License-Identifier: MIT OR Apache-2.0
// The same pre-upgrade digest vectors run in native Rust and WASM.
use super::*;

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn rustcrypto_digest_golden_vectors() {
    let body = b"canonical purge payload v2\0\xff";
    let mut actual = serde_json::Map::new();
    for domain in [
        OWNER_ROOT_DOMAIN,
        OWNER_TRANSITION_DOMAIN,
        OWNER_CAPABILITY_DOMAIN,
        crate::canonical::OWNER_CAPABILITY_V3_DOMAIN,
        crate::canonical::OWNER_BINDING_DOMAIN,
        TRANSFER_HANDOFF_DOMAIN,
        TRANSFER_ACCEPTANCE_DOMAIN,
        TRANSFER_AUDIT_DOMAIN,
        PURGE_OPERATION_DOMAIN,
    ] {
        actual.insert(
            String::from_utf8(domain.to_vec()).expect("domain"),
            hex::encode(digest(domain, body)).into(),
        );
    }
    let owner = TestKey::new(0x1f);
    actual.insert("key_id".into(), hex::encode(key_id(&owner.wire())).into());
    assert_eq!(key_id(&owner.wire()), policy::owner_key_id(&owner.wire()));
    actual.insert(
        "payload_sha256".into(),
        hex::encode(Sha256::digest(body)).into(),
    );
    actual.insert(
        "genesis_digest".into(),
        hex::encode(
            Sha256::new()
                .chain_update(&owner.wire().public_key)
                .chain_update(SPOOL)
                .finalize(),
        )
        .into(),
    );
    actual.insert(
        "passkey_rp_id_hash".into(),
        hex::encode(Sha256::digest(b"heddle.test")).into(),
    );
    actual.insert(
        "passkey_client_data_hash".into(),
        hex::encode(Sha256::digest(
            br#"{"type":"webauthn.get","challenge":"golden","origin":"https://heddle.test"}"#,
        ))
        .into(),
    );
    let mut policy_body = wire::SignedPolicyBody {
        format_version: 1,
        spool_uuid: SPOOL.to_vec(),
        sequence: 1,
        policy: Some(wire::SignedSpoolPolicy {
            revoked_key_ids: vec![vec![0x55; 32]],
            max_audience: Some(1),
        }),
        owner_id: key_id(&owner.wire()).to_vec(),
        owner_state_hash: vec![0x33; 32],
        ..Default::default()
    };
    let state_hash = policy::policy_state_hash(&policy_body).expect("policy hash");
    actual.insert("policy_state_hash".into(), hex::encode(state_hash).into());
    policy_body.policy_state_hash = state_hash.to_vec();
    actual.insert(
        "policy_signature_digest".into(),
        hex::encode(
            policy::policy_signature_digest(&policy_body).expect("policy signature digest"),
        )
        .into(),
    );
    let golden: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/rustcrypto-digests-v1.json"))
            .expect("old-code digest fixture");
    assert_eq!(
        serde_json::Value::Object(actual),
        golden,
        "native and WASM must retain all pre-upgrade digest bytes"
    );
}
