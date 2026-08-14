use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use crypto::{Ed25519Signer, Signer as _};

use super::{
    claim_authorization::{
        consent_reply, resolved_reply, signed_consent, validate_credential_id, validate_handle,
    },
    identity_state::ClaimState,
};

fn state(node_id: String) -> ClaimState {
    ClaimState::new(
        "api.heddle.test".into(),
        "account-7".into(),
        "subject-7".into(),
        "steady-heron".into(),
        node_id,
    )
}

#[test]
fn signed_consent_binds_handle_and_credential_id() {
    let signer = Ed25519Signer::generate().expect("signer");
    let consent = signed_consent(
        &state(hex::encode(signer.public_key())),
        "human-handle",
        "Y3JlZGVudGlhbA",
        123_456,
        &signer,
    )
    .expect("signed consent");
    let statement: serde_json::Value =
        serde_json::from_str(&consent.statement).expect("statement JSON");
    assert_eq!(statement["handle"], "human-handle");
    assert_eq!(statement["credentialId"], "Y3JlZGVudGlhbA");
    let signature = URL_SAFE_NO_PAD
        .decode(&consent.signature)
        .expect("signature encoding");
    Ed25519Signer::verify_with_public_key(
        consent.statement.as_bytes(),
        signer.public_key(),
        &signature,
    )
    .expect("signature verifies");
}

#[test]
fn malformed_promotion_inputs_are_rejected() {
    assert!(validate_handle("UPPERCASE").is_err());
    assert!(validate_handle("-leading").is_err());
    assert!(validate_credential_id("not+base64url").is_err());
}

#[test]
fn consumed_link_refuses_resolve_and_second_consent() {
    let mut state = state("11".repeat(32));
    state.mark_consented();
    let resolve = resolved_reply(&state, br#"{"kind":"resolve"}"#).expect("resolve refusal");
    let consent = consent_reply(
        &mut state,
        br#"{"kind":"consent","handle":"human-handle","credentialId":"Y3JlZA"}"#,
    )
    .expect("consent refusal");
    for reply in [resolve, consent] {
        let reply: serde_json::Value = serde_json::from_slice(&reply).expect("reply JSON");
        assert_eq!(reply["kind"], "refused");
        assert_eq!(reply["refusal"], "claimed");
    }
}
