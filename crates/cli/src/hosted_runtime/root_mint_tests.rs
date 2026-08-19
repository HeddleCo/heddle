use chrono::Utc;
use crypto::{Ed25519Signer, Signer as _};

use super::root_mint::{
    ACCOUNT_ROOT_TTL, IndependentRootKind, agent_root_subject, authority_keypair,
    is_local_agent_root, mint_anon_root, mint_independent_root, mint_new_independent_root,
    remint_stored_root,
};
use crate::hosted_runtime::{auth::headless_token_metadata, device_flow::authenticated_subject};

#[test]
fn agent_root_is_signed_by_the_same_seed_weft_will_register() {
    let signer = Ed25519Signer::generate().expect("seed");
    let root = mint_independent_root(
        &signer.to_seed(),
        &agent_root_subject(signer.public_key()),
        IndependentRootKind::Account,
        ACCOUNT_ROOT_TTL,
        None,
    )
    .expect("mint agent root");

    let authority = authority_keypair(&signer.to_seed()).expect("authority");
    biscuit_auth::Biscuit::from_base64(root.token.as_bytes(), authority.public())
        .expect("Weft verifies a client-minted root against the registered public key");
    let metadata = headless_token_metadata(&root.token).expect("metadata");
    assert!(!metadata.is_derived);
    assert_eq!(metadata.subject, root.subject);
    assert!(
        metadata
            .proof_public_key_hex
            .eq_ignore_ascii_case(&root.public_key_hex())
    );
    assert!(is_local_agent_root(
        &metadata.subject,
        &metadata.proof_public_key_hex
    ));
    let parsed_expiry = chrono::DateTime::parse_from_rfc3339(
        metadata.expires_at.as_deref().expect("authority expiry"),
    )
    .expect("rfc3339 expiry")
    .with_timezone(&Utc);
    assert!((parsed_expiry - root.expires_at).num_seconds().abs() <= 1);
}

#[test]
fn anon_root_carries_subject_kind_and_never_asks_weft() {
    let root = mint_anon_root().expect("mint anon");
    assert!(root.subject.starts_with("anon:"));
    let source = biscuit_auth::UnverifiedBiscuit::from_base64(root.token.as_bytes())
        .expect("parse")
        .print_block_source(0)
        .expect("authority");
    assert!(source.contains(r#"subject_kind("anon")"#), "{source}");
    assert_eq!(
        authenticated_subject(&root.token).expect("subject"),
        root.subject
    );
}

#[test]
fn remint_keeps_the_registered_key_and_replaces_only_the_token() {
    let first = mint_new_independent_root(
        "alice@example.com",
        IndependentRootKind::Account,
        ACCOUNT_ROOT_TTL,
        Some("cred-1"),
    )
    .expect("first root");
    let renewed = remint_stored_root(
        &first.private_key_pem,
        &first.subject,
        first.credential_id.as_deref(),
    )
    .expect("remint");
    assert_eq!(renewed.public_key, first.public_key);
    assert_eq!(renewed.subject, first.subject);
    assert_eq!(renewed.credential_id.as_deref(), Some("cred-1"));
    assert_ne!(renewed.token, first.token);
    let authority = authority_keypair(
        &Ed25519Signer::from_pem(&first.private_key_pem)
            .expect("pem")
            .to_seed(),
    )
    .expect("authority");
    biscuit_auth::Biscuit::from_base64(renewed.token.as_bytes(), authority.public())
        .expect("reminted root still verifies with the registered key");
}

#[test]
fn mint_rejects_an_empty_or_injected_subject() {
    let seed = [7_u8; 32];
    assert!(
        mint_independent_root(
            &seed,
            "",
            IndependentRootKind::Account,
            ACCOUNT_ROOT_TTL,
            None
        )
        .is_err()
    );
    assert!(
        mint_independent_root(
            &seed,
            r#"alice") fact("evil"("#,
            IndependentRootKind::Account,
            ACCOUNT_ROOT_TTL,
            None,
        )
        .is_err()
    );
}
