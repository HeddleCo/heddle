use chrono::{Duration, Utc};
use crypto::{Ed25519Signer, Signer as _};

use super::root_mint::{
    IndependentRootMint, agent_root_subject, is_local_agent_root,
    local_agent_credential_needs_refresh, mint_agent_root, mint_independent_root,
};

#[test]
fn mint_rejects_an_empty_or_injected_subject() {
    let seed = [7_u8; 32];
    let empty = mint_independent_root(IndependentRootMint {
        seed: &seed,
        subject: "",
        ttl: Duration::days(1),
    });
    assert!(empty.is_err(), "empty subject must fail");
    let injected = mint_independent_root(IndependentRootMint {
        seed: &seed,
        subject: "alice\")\nright(\"admin",
        ttl: Duration::days(1),
    });
    assert!(injected.is_err(), "injected subject must fail");
}

#[test]
fn agent_root_is_signed_by_the_same_seed_weft_will_register() {
    let seed = [9_u8; 32];
    let root = mint_agent_root(&seed).expect("mint agent root");
    let signer = Ed25519Signer::from_seed(&seed).expect("seed signer");
    assert_eq!(root.public_key.as_slice(), signer.public_key());
    assert_eq!(root.subject, agent_root_subject(signer.public_key()));
    assert!(is_local_agent_root(&root.subject, &root.public_key_hex()));
}

#[test]
fn expired_or_missing_local_agent_expiry_needs_refresh() {
    let now = Utc::now();
    assert!(local_agent_credential_needs_refresh(None, now));
    assert!(local_agent_credential_needs_refresh(
        Some(&(now - Duration::seconds(1)).to_rfc3339()),
        now
    ));
    assert!(!local_agent_credential_needs_refresh(
        Some(&(now + Duration::hours(1)).to_rfc3339()),
        now
    ));
    assert!(local_agent_credential_needs_refresh(
        Some("not-a-timestamp"),
        now
    ));
}
