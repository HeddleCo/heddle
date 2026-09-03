use chrono::{Duration, Utc};
use crypto::{Ed25519Signer, Signer as _};

use super::root_mint::{
    ACCOUNT_ROOT_TTL, IndependentRootMint, agent_root_subject, authority_keypair,
    authority_session_fact, is_local_agent_root, local_agent_credential_needs_refresh,
    mint_agent_root, mint_independent_root, remint_stored_root,
};
use crate::hosted_runtime::{
    auth::headless_token_metadata,
    device_flow::{AgentTemplate, SAFE_AGENT_OPERATIONS, restrict_agent_account_root},
};

#[test]
fn agent_root_is_signed_by_the_same_seed_weft_will_register() {
    let signer = Ed25519Signer::generate().expect("seed");
    let root = mint_independent_root(IndependentRootMint {
        seed: &signer.to_seed(),
        subject: &agent_root_subject(signer.public_key()),
        ttl: ACCOUNT_ROOT_TTL,
        credential_id: None,
        session_id: None,
        expires_at: None,
    })
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
    assert_eq!(
        authority_session_fact(&root.token).expect("session"),
        format!("key:{}", root.public_key_hex())
    );
}

#[test]
fn remint_cannot_escape_the_registered_key_session() {
    let signer = Ed25519Signer::generate().expect("seed");
    let subject = "alice@example.com";
    let first = mint_independent_root(IndependentRootMint {
        seed: &signer.to_seed(),
        subject,
        ttl: ACCOUNT_ROOT_TTL,
        credential_id: Some("cred-1"),
        session_id: None,
        expires_at: None,
    })
    .expect("first root");
    let first_session = authority_session_fact(&first.token).expect("first session");
    assert_eq!(first_session, "cred:cred-1");

    let renewed = remint_stored_root(
        &first.private_key_pem,
        &first.subject,
        first.credential_id.as_deref(),
        Some(&first_session),
    )
    .expect("remint");
    assert_eq!(renewed.public_key, first.public_key);
    assert_eq!(renewed.subject, first.subject);
    assert_eq!(renewed.credential_id.as_deref(), Some("cred-1"));
    assert_ne!(renewed.token, first.token);
    assert_eq!(
        authority_session_fact(&renewed.token).expect("reminted session"),
        first_session,
        "RevokeSession on the registered credential/session must hit every remint"
    );

    let forgotten_session = remint_stored_root(
        &first.private_key_pem,
        &first.subject,
        first.credential_id.as_deref(),
        None,
    )
    .expect("deterministic remint");
    assert_eq!(
        authority_session_fact(&forgotten_session.token).expect("fallback session"),
        first_session,
        "omitting the previous session must still bind remint to cred:{{id}}, not a new UUID"
    );

    let authority = authority_keypair(&signer.to_seed()).expect("authority");
    biscuit_auth::Biscuit::from_base64(renewed.token.as_bytes(), authority.public())
        .expect("reminted root still verifies with the registered key");
}

#[test]
fn mint_rejects_an_empty_or_injected_subject() {
    let seed = [7_u8; 32];
    assert!(
        mint_independent_root(IndependentRootMint {
            seed: &seed,
            subject: "",
            ttl: ACCOUNT_ROOT_TTL,
            credential_id: None,
            session_id: None,
            expires_at: None,
        })
        .is_err()
    );
    assert!(
        mint_independent_root(IndependentRootMint {
            seed: &seed,
            subject: r#"alice") fact("evil"("#,
            ttl: ACCOUNT_ROOT_TTL,
            credential_id: None,
            session_id: None,
            expires_at: None,
        })
        .is_err()
    );
    assert!(
        mint_independent_root(IndependentRootMint {
            seed: &seed,
            subject: "alice",
            ttl: ACCOUNT_ROOT_TTL,
            credential_id: None,
            session_id: None,
            expires_at: Some(Utc::now() - Duration::hours(1)),
        })
        .is_err()
    );
}

#[test]
fn expired_or_missing_local_agent_expiry_needs_refresh() {
    assert!(local_agent_credential_needs_refresh(None, Utc::now()));
    assert!(local_agent_credential_needs_refresh(
        Some("2020-01-01T00:00:00+00:00"),
        Utc::now()
    ));
    assert!(!local_agent_credential_needs_refresh(
        Some(&(Utc::now() + Duration::hours(2)).to_rfc3339()),
        Utc::now()
    ));
}

#[test]
fn expired_local_agent_root_remints_the_same_registered_key() {
    let signer = Ed25519Signer::generate().expect("seed");
    let seed = signer.to_seed();
    let first = mint_agent_root(&seed).expect("first agent root");
    let reminted = mint_agent_root(&seed).expect("remint from the same Iroh seed");
    assert_eq!(reminted.public_key, first.public_key);
    assert_eq!(
        authority_session_fact(&reminted.token).expect("reminted session"),
        authority_session_fact(&first.token).expect("first session")
    );
    assert_eq!(
        authority_session_fact(&first.token).expect("session"),
        format!("key:{}", first.public_key_hex())
    );
    assert!(reminted.expires_at > Utc::now());
}

#[test]
fn restricted_agent_capability_keeps_the_deny_floor() {
    let signer = Ed25519Signer::generate().expect("seed");
    let root = mint_agent_root(&signer.to_seed()).expect("agent root");
    let restricted =
        restrict_agent_account_root(&root.token, &signer, root.expires_at).expect("restrict");
    let metadata = headless_token_metadata(&restricted).expect("metadata");
    assert!(metadata.is_derived);
    assert!(is_local_agent_root(
        &metadata.subject,
        &metadata.proof_public_key_hex
    ));
    let biscuit =
        biscuit_auth::UnverifiedBiscuit::from_base64(restricted.as_bytes()).expect("parse");
    let block = biscuit.print_block_source(1).expect("attenuation");
    for denied in [
        "CreateServiceAccount",
        "DeleteSpool",
        "RevokeSession",
        "CreateAgentAccount",
        "ClaimHandle",
        "ClaimSignupInvite",
        "PromoteAgentAccount",
    ] {
        assert!(
            block.contains(&format!(r#"$op != "{denied}""#)),
            "deny floor missing {denied}: {block}"
        );
    }
    for everyday in ["CreateSignupInvite", "ListSignupInvites"] {
        assert!(
            !block.contains(&format!(r#"$op != "{everyday}""#)),
            "account-root deny floor must not include {everyday}: {block}"
        );
    }
    assert!(
        !block.contains("$op =="),
        "account root must not carry the SAFE child allowlist: {block}"
    );
}

/// The unclaimed agent-rooted login root is the account. Remint keeps the
/// deny floor but not the derive-agent child ceiling, so everyday account
/// ops including signup-invite mint/list must authorize.
#[test]
fn restricted_agent_root_can_mint_and_list_signup_invites() {
    let signer = Ed25519Signer::generate().expect("seed");
    let seed = signer.to_seed();
    let root = mint_agent_root(&seed).expect("agent root");
    let restricted =
        restrict_agent_account_root(&root.token, &signer, root.expires_at).expect("restrict");
    for allowed in ["CreateSignupInvite", "ListSignupInvites", "WhoAmI"] {
        authorize_restricted_root(&restricted, &seed, allowed)
            .unwrap_or_else(|error| panic!("account root must authorize {allowed}: {error}"));
    }
    for denied in [
        "CreateServiceAccount",
        "CreateAgentAccount",
        "RevokeSession",
        "ClaimSignupInvite",
    ] {
        assert!(
            authorize_restricted_root(&restricted, &seed, denied).is_err(),
            "account root must still deny {denied}"
        );
    }
}

/// Derived children stay on the SAFE / template ceiling. Invite mint and
/// list are everyday account-root ops, not child grants.
#[test]
fn derived_contributor_cannot_mint_or_list_signup_invites() {
    let contributor = AgentTemplate::Contributor.operations();
    for template in AgentTemplate::ALL {
        let operations = template.operations();
        for denied in ["CreateSignupInvite", "ListSignupInvites"] {
            assert!(
                !operations.iter().any(|operation| operation == denied),
                "template {:?} must not grant {denied}",
                template.as_str()
            );
        }
    }
    for denied in ["CreateSignupInvite", "ListSignupInvites"] {
        assert!(
            !SAFE_AGENT_OPERATIONS.contains(&denied),
            "SAFE_AGENT_OPERATIONS is the derive-agent child ceiling and must not include {denied}"
        );
        assert!(
            !contributor.iter().any(|operation| operation == denied),
            "contributor child must not receive {denied}"
        );
    }
}

/// weft#2041: the owner-root pin presents the FULL unrestricted client-minted
/// root as its bearer. Prove that token authorizes `BootstrapOwnerRoot` at the
/// Biscuit layer — the server requires *some* agent bearer for the pin, and the
/// previous proof-only session carried none (the `invalid bearer capability`
/// this fixes). The restricted account root authorizes it too (the deny floor
/// does not cover `BootstrapOwnerRoot`), so choosing the full root is a
/// least-dependency call, not a caveat workaround.
#[test]
fn agent_roots_authorize_the_owner_root_pin() {
    let signer = Ed25519Signer::generate().expect("seed");
    let seed = signer.to_seed();
    let root = mint_agent_root(&seed).expect("agent root");
    authorize_restricted_root(&root.token, &seed, "BootstrapOwnerRoot")
        .expect("the full unrestricted root must authorize the owner-root pin");

    let restricted =
        restrict_agent_account_root(&root.token, &signer, root.expires_at).expect("restrict");
    authorize_restricted_root(&restricted, &seed, "BootstrapOwnerRoot").expect(
        "the account-root deny floor does not cover BootstrapOwnerRoot (heddle#1600 keeps it \
         so derive-agent children inherit it)",
    );
}

fn authorize_restricted_root(
    token: &str,
    seed: &[u8; 32],
    operation: &str,
) -> Result<(), biscuit_auth::error::Token> {
    use biscuit_auth::{builder::AuthorizerBuilder, datalog::RunLimits};

    let authority = authority_keypair(seed).expect("authority");
    let root_public = authority.public();
    let biscuit = biscuit_auth::Biscuit::from_base64(token.as_bytes(), move |_| Ok(root_public))?;
    let mut authorizer = AuthorizerBuilder::new()
        .set_limits(RunLimits {
            max_facts: 1000,
            max_iterations: 100,
            max_time: std::time::Duration::from_secs(1),
        })
        .fact(format!("time({})", Utc::now().to_rfc3339()).as_str())?
        .fact(format!(r#"operation("{operation}")"#).as_str())?
        .policy("allow if true")?
        .build(&biscuit)?;
    authorizer.authorize().map(|_| ())
}

/// Regression: the agent ceiling must carry the personal-spool discovery +
/// provisioning ops. Without them, an unclaimed agent-rooted account's
/// `heddle push <host>` aborts client-side inside `auto_provision_hosted_repo`
/// before `GetCurrentUserSpool`/`CreateSpool` are ever sent — the account can
/// mint, WhoAmI, and ListRefs, but never provision its own spool. weft admits
/// both server-side for unclaimed agent-rooted accounts (weft#1852/#1853).
#[test]
fn safe_ceiling_allows_host_only_spool_provisioning() {
    for op in [
        "GetCurrentUserSpool",
        "CreateSpool",
        "BootstrapOwnerRoot",
        "GetCurrentOwnerKeyring",
    ] {
        assert!(
            SAFE_AGENT_OPERATIONS.contains(&op),
            "agent ceiling missing {op}: host-only auto-provision / claimable owner-root cannot fire it"
        );
    }
}
