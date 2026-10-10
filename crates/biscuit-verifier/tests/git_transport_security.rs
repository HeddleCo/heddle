//! Independent cryptographic regression tests of Git capability attenuation.
//! Signing fixtures use public synthetic seeds and never register real authority.
use biscuit_auth::{Algorithm, Biscuit, KeyPair, PrivateKey, builder::BlockBuilder};
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signer as _, SigningKey};
use heddle_biscuit_verifier::{
    git_transport::{
        GIT_SERVICE_AUDIENCE, GitAction, GitChallenge, GitExchange, GitScope, credential_digest,
        verify_exchange_proof,
    },
    key_delegation,
    signature_v1::BiscuitBuilderV1Ext as _,
    verify_at_with_extra_facts,
};

fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 0).expect("fixed fixture time")
}
fn root() -> KeyPair {
    KeyPair::from(&PrivateKey::from_bytes(&[41; 32], Algorithm::Ed25519).expect("synthetic key"))
}
fn scope() -> GitScope {
    GitScope {
        service_audience: GIT_SERVICE_AUDIENCE.into(),
        tenant_spool_id: uuid::Uuid::from_bytes([1; 16]),
        spool_id: uuid::Uuid::from_bytes([2; 16]),
        repository_path: "org/project".into(),
        thread_id: [3; 32],
        action: GitAction::Read,
        disclosure_audience: "public".into(),
    }
}
fn token(extra: &str) -> String {
    Biscuit::builder().code(format!(
        "user(\"synthetic-account\"); session(\"synthetic-session\"); issued_at({}); expires_at({}); device_pop_key(\"{}\"); right(\"spool\", \"org/project\", \"write\"); check if time($now), $now < {}; {extra}",
        now().to_rfc3339(), (now() + Duration::hours(1)).to_rfc3339(), hex::encode(root().public().to_bytes()), (now() + Duration::hours(1)).to_rfc3339()).as_str())
        .expect("authority syntax").build_v1(&root()).expect("signed root").to_base64().expect("token")
}
fn child(
    parent: &str,
    signer: &SigningKey,
    next: &SigningKey,
    restrictions: BlockBuilder,
) -> String {
    let key = next.verifying_key().to_bytes();
    let statement = key_delegation::statement(parent, &key).expect("statement");
    key_delegation::append(
        parent,
        &key,
        &signer.sign(&statement).to_bytes(),
        restrictions,
    )
    .expect("append signed transition")
}
fn verify(
    token: &str,
    scope: &GitScope,
) -> Result<heddle_biscuit_verifier::BiscuitFacts, heddle_biscuit_verifier::BiscuitError> {
    verify_at_with_extra_facts(
        token,
        &[root().public()],
        &[],
        scope.action.operation(),
        Some(("spool", &scope.repository_path)),
        &[scope.request_fact().expect("verified request fact")],
        now(),
    )
}

#[test]
fn only_the_final_proof_key_can_exchange_a_three_hop_attenuated_git_credential() {
    let keys: Vec<_> = [41, 42, 43, 44]
        .into_iter()
        .map(|seed| SigningKey::from_bytes(&[seed; 32]))
        .collect();
    let mut current = token("check if operation(\"GitRead\");");
    for pair in keys.windows(2) {
        current = child(
            &current,
            &pair[0],
            &pair[1],
            BlockBuilder::new()
                .check("check if operation(\"GitRead\")")
                .expect("restriction"),
        );
    }
    let facts = verify(&current, &scope()).expect("valid three-hop chain");
    let exchange = GitExchange {
        scope: scope(),
        credential_digest: credential_digest(&current, None).expect("digest"),
        challenge: GitChallenge {
            nonce: [7; 32],
            issued_at_seconds: now().timestamp(),
            expires_at_seconds: now().timestamp() + 60,
        },
        session_expires_at_seconds: now().timestamp() + 300,
    };
    let statement = exchange.signing_bytes().expect("exchange statement");
    for (index, key) in keys.iter().enumerate() {
        let result = verify_exchange_proof(
            &exchange,
            &current,
            None,
            &key.sign(&statement).to_bytes(),
            &facts,
            now(),
        );
        assert_eq!(
            result.is_ok(),
            index == keys.len() - 1,
            "ancestor index {index}"
        );
    }
    let mut write = scope();
    write.action = GitAction::Write;
    assert!(
        verify(&current, &write).is_err(),
        "leaf possession cannot remove ancestor read-only checks"
    );
}

#[test]
fn serialized_client_biscuit_cannot_be_spliced_to_an_unrelated_gateway_proof_key() {
    let parent = token("");
    let attacker = SigningKey::from_bytes(&[90; 32]);
    let forged = child(&parent, &attacker, &attacker, BlockBuilder::new());
    assert!(
        verify(&forged, &scope()).is_err(),
        "Biscuit appendability is not parent-key authorization"
    );
    let authorized = child(
        &parent,
        &SigningKey::from_bytes(&[41; 32]),
        &SigningKey::from_bytes(&[42; 32]),
        BlockBuilder::new(),
    );
    verify(&authorized, &scope()).expect("control valid child");
    let wrong_ancestor = child(
        &authorized,
        &SigningKey::from_bytes(&[41; 32]),
        &attacker,
        BlockBuilder::new(),
    );
    assert!(
        verify(&wrong_ancestor, &scope()).is_err(),
        "even a real root key cannot skip the effective delegated parent"
    );
}

#[test]
fn cryptographically_valid_descendants_cannot_assert_or_derive_git_request_context() {
    let parent = token("");
    let root_signer = SigningKey::from_bytes(&[41; 32]);
    let leaf = SigningKey::from_bytes(&[42; 32]);
    for malicious in [
        "git_transport_request_v1(\"forged\");",
        "git_transport_request_v1($v) <- user($v);",
        "git_transport_request_v1(\"forged\") <- true;",
    ] {
        let restriction = BlockBuilder::new()
            .code(malicious)
            .expect("valid hostile block source");
        let forged = child(&parent, &root_signer, &leaf, restriction);
        assert!(verify(&forged, &scope()).is_err(), "{malicious}");
    }
}
