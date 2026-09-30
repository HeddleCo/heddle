//! Pairing completion with a credential shaped like a real browser approval:
//! the approver's session, rooted at their owner-certified mint root and
//! attenuated to this device's freshly generated subject key (heddle#1921).
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use biscuit_verifier::{key_delegation, signature_v1::BiscuitBuilderV1Ext as _};
use chrono::{DateTime, Utc};
use crypto::{Ed25519Signer, Signer};
use objects::object::thread_replication::SourceAuthor;
use thread_api::contract as api;

use super::finish;
use crate::hosted_runtime::{auth::AuthLoginOutcome, auth_login_tests::IsolatedHome};

const SERVER: &str = "api.pairing.test";
const OPERATION: &str = "complete-pairing-op";
const ACCOUNT: [u8; 16] = [9; 16];

struct Keys {
    owner: Ed25519Signer,
    recovery: Ed25519Signer,
    mint_root: biscuit_auth::KeyPair,
    browser: Ed25519Signer,
    subject: Ed25519Signer,
    endpoint: Ed25519Signer,
}

fn keys() -> Keys {
    Keys {
        owner: Ed25519Signer::from_seed(&[31; 32]).expect("owner"),
        recovery: Ed25519Signer::from_seed(&[32; 32]).expect("recovery"),
        mint_root: keypair(33),
        browser: Ed25519Signer::from_seed(&[34; 32]).expect("approving browser"),
        subject: Ed25519Signer::from_seed(&[35; 32]).expect("paired subject"),
        endpoint: Ed25519Signer::from_seed(&[36; 32]).expect("device endpoint"),
    }
}

fn keypair(seed: u8) -> biscuit_auth::KeyPair {
    biscuit_auth::KeyPair::from(
        &biscuit_auth::PrivateKey::from_bytes(&[seed; 32], biscuit_auth::Algorithm::Ed25519)
            .expect("root key"),
    )
}

fn public(signer: &Ed25519Signer) -> [u8; 32] {
    signer.public_key().try_into().expect("ed25519 public key")
}

fn account() -> uuid::Uuid {
    uuid::Uuid::from_bytes(ACCOUNT)
}

fn owner_state(keys: &Keys) -> api::OwnerState {
    let root = repo::sign_custodial_owner_root(&keys.owner, &keys.recovery, ACCOUNT, [5; 32])
        .expect("owner root");
    let binding =
        repo::sign_custodial_owner_binding(&keys.owner, &root, [6; 32]).expect("owner binding");
    let verified = heddleco_capability_verifier::verify_owner_root(&root).expect("verify owner");
    api::OwnerState {
        owner: Some(api::PrincipalRef {
            id: account().to_string(),
        }),
        root: Some(root),
        binding: Some(binding),
        version: verified.state_hash().to_vec(),
        ..Default::default()
    }
}

/// The owner's public association of the approver's mint root with the account.
fn mint_root_attachment(
    keys: &Keys,
    owner: &api::OwnerState,
    now: i64,
) -> api::SignedOwnerMintRootAttachment {
    repo::sign_mint_root_attachment(
        &keys.owner,
        owner,
        &keys.mint_root.public().to_bytes(),
        now - 60,
        now + 3600,
        [7; 32],
    )
    .expect("mint-root attachment")
}

/// The approving browser's own session: rooted at `root`, bound to its key.
fn parent_session(root: &biscuit_auth::KeyPair, browser: &Ed25519Signer, now: i64) -> String {
    let expiry = DateTime::from_timestamp(now + 3600, 0).expect("expiry");
    let account = account();
    biscuit_auth::Biscuit::builder()
        .code(
            format!(
                "user(\"{account}\"); subject_kind(\"user\"); subject_user_uuid(\"{account}\"); \
                 session(\"parent-session\"); device_pop_key(\"{}\"); expires_at({}); \
                 check if time($now), $now < {};",
                hex::encode(browser.public_key()),
                expiry.to_rfc3339(),
                expiry.to_rfc3339(),
            )
            .as_str(),
        )
        .expect("session facts")
        .build_v1(root)
        .expect("parent session")
        .to_base64()
        .expect("encode parent session")
}

/// Attenuate `parent` to `child`, signed by the parent's effective proof key.
fn attenuate(parent: &str, holder: &Ed25519Signer, child: &[u8; 32], now: i64) -> String {
    let statement = key_delegation::statement(parent, child).expect("delegation statement");
    let signature: [u8; 64] = holder
        .sign(&statement)
        .expect("sign delegation")
        .try_into()
        .expect("ed25519 signature");
    let expiry = DateTime::from_timestamp(now + 3600, 0).expect("expiry");
    key_delegation::append(
        parent,
        child,
        &signature,
        key_delegation::device_restrictions(expiry).expect("device restrictions"),
    )
    .expect("attenuated credential")
}

/// The paired credential weft issues: the approver's session attenuated to
/// this device's subject key.
fn paired_credential(keys: &Keys, now: i64) -> Vec<u8> {
    let parent = parent_session(&keys.mint_root, &keys.browser, now);
    decode(&attenuate(
        &parent,
        &keys.browser,
        &public(&keys.subject),
        now,
    ))
}

fn decode(token: &str) -> Vec<u8> {
    URL_SAFE.decode(token).expect("credential bytes")
}

struct Pairing {
    binding: api::RootAttachmentBinding,
    attachment: api::RootAttachment,
    response: api::AuthenticationResponse,
}

fn pairing(
    keys: &Keys,
    credential: Vec<u8>,
    root_public_key: Vec<u8>,
    mint_attachment: Option<api::SignedOwnerMintRootAttachment>,
    owner: api::OwnerState,
    now: i64,
) -> Pairing {
    let binding = api::RootAttachmentBinding {
        format_version: 2,
        account_id: account().to_string(),
        root_public_key,
        subject_public_key: keys.subject.public_key().to_vec(),
        device: Some(api::EndpointRef {
            public_key: keys.endpoint.public_key().to_vec(),
            kind: api::EndpointKind::Device as i32,
        }),
        not_before_unix_seconds: now,
        expires_at_unix_seconds: now + 300,
        credential_digest: blake3::hash(&credential).as_bytes().to_vec(),
        pairing_challenge: vec![4; 32],
    };
    let attachment =
        thread_api::root_attachment::sign_binding(&keys.subject, &keys.endpoint, binding.clone())
            .expect("attachment");
    let response = api::AuthenticationResponse {
        receipt: Some(api::MutationReceipt {
            client_operation_id: OPERATION.into(),
            outcome: Some(api::mutation_receipt::Outcome::Applied(
                api::Applied::default(),
            )),
            ..Default::default()
        }),
        principal: Some(api::PrincipalRecord {
            id: account().to_string(),
            account_id: account().to_string(),
            ..Default::default()
        }),
        ownership: Some(owner),
        credential: Some(api::CredentialResult {
            outcome: Some(api::credential_result::Outcome::Issued(
                api::IssuedCredential {
                    r#ref: Some(api::RecordRef {
                        id: "paired:1921".into(),
                        spool: None,
                    }),
                    biscuit: credential,
                    subject: account().to_string(),
                    proof_public_key: keys.subject.public_key().to_vec(),
                    kind: api::CredentialKind::Device as i32,
                    expires_at: Some(prost_types::Timestamp {
                        seconds: now + 300,
                        nanos: 0,
                    }),
                },
            )),
            session: Some(api::SessionRecord {
                r#ref: Some(api::RecordRef {
                    id: "parent-session".into(),
                    spool: None,
                }),
                ..Default::default()
            }),
            mint_root_attachment: mint_attachment,
            ..Default::default()
        }),
        ..Default::default()
    };
    Pairing {
        binding,
        attachment,
        response,
    }
}

fn run(keys: &Keys, pairing: Pairing) -> anyhow::Result<AuthLoginOutcome> {
    finish(
        SERVER,
        &keys.subject,
        &pairing.binding,
        &pairing.attachment,
        pairing.response,
        OPERATION,
    )
}

fn source_author(keys: &Keys) -> SourceAuthor {
    repo::identity::source_author::load(
        &repo::identity::heddle_home_dir(),
        &public(&keys.subject),
        uuid::Uuid::from_u128(1921),
    )
    .expect("load source author")
}

/// A rejected pairing leaves neither a stored credential nor a retained
/// source author behind.
fn assert_rejected(keys: &Keys, result: anyhow::Result<AuthLoginOutcome>, expected: &str) {
    let error = match result {
        Ok(_) => panic!("pairing must be rejected ({expected})"),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains(expected),
        "expected `{expected}` in rejection, got: {error}"
    );
    assert!(
        config::credentials::get_server_credential(SERVER)
            .expect("read credentials")
            .is_none(),
        "rejected pairing must not store a credential"
    );
    assert_eq!(source_author(keys), SourceAuthor::LocalKey);
}

#[test]
fn pairing_stores_credential_rooted_at_the_approvers_verified_mint_root() {
    let _process_env_guard = crate::test_process_env::exclusive_blocking();
    let _home = IsolatedHome::new();
    let keys = keys();
    let now = Utc::now().timestamp();
    let owner = owner_state(&keys);
    let credential = paired_credential(&keys, now);
    let attachment = mint_root_attachment(&keys, &owner, now);
    let outcome = run(
        &keys,
        pairing(
            &keys,
            credential.clone(),
            keys.mint_root.public().to_bytes().to_vec(),
            Some(attachment),
            owner,
            now,
        ),
    )
    .unwrap_or_else(|error| panic!("browser-approved pairing must complete: {error:#}"));
    assert!(matches!(
        outcome,
        AuthLoginOutcome::Authenticated {
            credential_saved: true,
            ..
        }
    ));
    let stored = config::credentials::get_server_credential(SERVER)
        .expect("read credentials")
        .expect("paired credential saved");
    assert_eq!(decode(&stored.token), credential);
    assert_eq!(stored.credential_id.as_deref(), Some("paired:1921"));
    let SourceAuthor::Account { actor, .. } = source_author(&keys) else {
        panic!("paired device must retain its account source author")
    };
    assert_eq!(actor.principal_id, account());
    let device = repo::identity::load_device(&repo::identity::device_identity_path())
        .expect("load device")
        .expect("paired device identity");
    assert_eq!(device.public_key, hex::encode(keys.subject.public_key()));
    assert!(
        device.credential_token.as_deref() == Some(stored.token.as_str()),
        "the exact paired bearer is retained for later uploads"
    );
}

#[test]
fn pairing_rejects_a_root_not_attached_to_the_account() {
    let _process_env_guard = crate::test_process_env::exclusive_blocking();
    let _home = IsolatedHome::new();
    let keys = keys();
    let now = Utc::now().timestamp();
    let owner = owner_state(&keys);
    // A well-formed credential attenuated to this device, but minted under a
    // root the owner never attached. A valid attachment for a different mint
    // root does not vouch for it.
    let other = keypair(41);
    let parent = parent_session(&other, &keys.browser, now);
    let credential = decode(&attenuate(
        &parent,
        &keys.browser,
        &public(&keys.subject),
        now,
    ));
    let attachment = mint_root_attachment(&keys, &owner, now);
    let result = run(
        &keys,
        pairing(
            &keys,
            credential,
            other.public().to_bytes().to_vec(),
            Some(attachment),
            owner,
            now,
        ),
    );
    assert_rejected(
        &keys,
        result,
        "verify approved root belongs to the paired account",
    );
}

#[test]
fn pairing_rejects_a_credential_not_attenuated_to_this_device() {
    let keys = keys();
    let now = Utc::now().timestamp();
    let stranger = Ed25519Signer::from_seed(&[42; 32]).expect("another device");
    let parent = parent_session(&keys.mint_root, &keys.browser, now);
    let cases = [
        ("the approver's own session", decode(&parent)),
        (
            "a session attenuated to another device",
            decode(&attenuate(&parent, &keys.browser, &public(&stranger), now)),
        ),
    ];
    for (case, credential) in cases {
        let _process_env_guard = crate::test_process_env::exclusive_blocking();
        let _home = IsolatedHome::new();
        let owner = owner_state(&keys);
        let attachment = mint_root_attachment(&keys, &owner, now);
        let result = run(
            &keys,
            pairing(
                &keys,
                credential,
                keys.mint_root.public().to_bytes().to_vec(),
                Some(attachment),
                owner,
                now,
            ),
        );
        eprintln!("case: {case}");
        assert_rejected(&keys, result, "exceeds credential subject");
    }
}

#[test]
fn pairing_rejects_a_binding_root_the_credential_does_not_chain_to() {
    let _process_env_guard = crate::test_process_env::exclusive_blocking();
    let _home = IsolatedHome::new();
    let keys = keys();
    let now = Utc::now().timestamp();
    let owner = owner_state(&keys);
    // The binding names the owner's own authority key, a root that is
    // genuinely verified for this account, while the credential chains to the
    // mint root. The verified root must be the one the credential chains to.
    let credential = paired_credential(&keys, now);
    let attachment = mint_root_attachment(&keys, &owner, now);
    let result = run(
        &keys,
        pairing(
            &keys,
            credential,
            keys.owner.public_key().to_vec(),
            Some(attachment),
            owner,
            now,
        ),
    );
    assert_rejected(&keys, result, "credential is not valid");
}

#[test]
fn retaining_under_an_approver_root_still_binds_the_credential_to_this_device() {
    use crate::hosted_runtime::source_author::{CredentialRoot, VerifiedMintRoot, retain};

    let _process_env_guard = crate::test_process_env::exclusive_blocking();
    let _home = IsolatedHome::new();
    let keys = keys();
    let now = Utc::now().timestamp();
    let owner = owner_state(&keys);
    let authority = repo::device_authority::DeviceAuthority {
        mint_roots: vec![mint_root_attachment(&keys, &owner, now)],
        owner,
        revoked_ids: Vec::new(),
        revoked_mint_roots: Vec::new(),
        revoked_publishers: Vec::new(),
    };
    repo::device_authority::publish(&repo::identity::heddle_home_dir(), &authority, now)
        .expect("publish owner authority");
    let pem = keys.subject.to_pem().expect("subject PEM");
    repo::identity::link_device_key(keys.subject.public_key(), &pem, SERVER)
        .expect("link paired device key");
    let mint_root = keys.mint_root.public().to_bytes();
    assert!(
        VerifiedMintRoot::verify(&authority, &keypair(41).public().to_bytes(), now).is_err(),
        "a root the owner never attached is not a verified mint root"
    );
    let credential = |token: Vec<u8>| config::credentials::ServerCredential {
        mint_root_attachment: None,
        token: URL_SAFE.encode(token),
        subject: account().to_string(),
        device_id: Some("paired:1921".into()),
        credential_id: Some("paired:1921".into()),
        private_key_pem: Some(pem.clone()),
        expires_at: None,
    };
    let verified = || VerifiedMintRoot::verify(&authority, &mint_root, now).expect("mint root");

    // Rooted at the verified approver root, but its proof key is still the
    // approver's browser: the root alone must not make it this device's author.
    let unattenuated = decode(&parent_session(&keys.mint_root, &keys.browser, now));
    let error = match retain(
        SERVER,
        &credential(unattenuated),
        CredentialRoot::Paired(verified()),
    ) {
        Ok(()) => panic!("an approver session not attenuated to this device must be rejected"),
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("another signing key"), "{error}");
    assert_eq!(source_author(&keys), SourceAuthor::LocalKey);

    // The same verified root cannot vouch for a token minted elsewhere.
    let foreign = decode(&attenuate(
        &parent_session(&keypair(41), &keys.browser, now),
        &keys.browser,
        &public(&keys.subject),
        now,
    ));
    assert!(
        retain(
            SERVER,
            &credential(foreign),
            CredentialRoot::Paired(verified())
        )
        .is_err(),
        "a token rooted elsewhere does not verify under the approved root"
    );
    assert_eq!(source_author(&keys), SourceAuthor::LocalKey);

    // A paired credential is never its own device-key root.
    let paired = paired_credential(&keys, now);
    assert!(
        retain(
            SERVER,
            &credential(paired.clone()),
            CredentialRoot::DeviceKey
        )
        .is_err(),
        "a paired credential does not chain to the device key"
    );

    // Control: the attenuated credential under the verified root is retained.
    retain(
        SERVER,
        &credential(paired),
        CredentialRoot::Paired(verified()),
    )
    .unwrap_or_else(|error| panic!("attenuated paired credential: {error:#}"));
    assert!(matches!(
        source_author(&keys),
        SourceAuthor::Account { actor, .. } if actor.principal_id == account()
    ));
}
