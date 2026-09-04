// SPDX-License-Identifier: Apache-2.0

use std::{ffi::OsString, sync::MutexGuard};

use api::heddle::api::v1alpha1::{
    BootstrapOwnerRootRequest, CreateAgentAccountResponse, OwnerKeyBindingKind,
    OwnerKeyTransitionKind, RecoveryGuardian, RecoveryGuardianKind, RecoveryPolicy,
    RegisterPublicKeyRequest, SignedOwnerKeyTransition, SignedOwnerRoot,
};
use config::credentials;
use crypto::{Ed25519Signer, Signer as _};
use prost::Message;
use repo::{
    OWNER_TRANSITION_DOMAIN, authorization_key_id, claim_deferred_human_transition,
    ed25519_verification_key, owner_key_transition_body, seq0_authority_public_key,
    sign_agent_claim_binding, sign_canonical,
};

fn build_with_browser_proofs(
    agent: &Ed25519Signer,
    human: &Ed25519Signer,
    signed_root: &SignedOwnerRoot,
    policy: RecoveryPolicy,
    guardian_signers: &[Ed25519Signer],
    valid_from_unix_seconds: i64,
) -> SignedOwnerKeyTransition {
    let next_authority_key =
        ed25519_verification_key(human.public_key()).expect("human public key");
    let nonce = [0x91; 32];
    let unsigned = claim_deferred_human_transition(
        signed_root,
        next_authority_key.clone(),
        policy.clone(),
        valid_from_unix_seconds,
        nonce,
    )
    .expect("browser transition");
    let body = owner_key_transition_body(&unsigned).expect("browser canonical body");
    let next_authority_key_proof =
        sign_canonical(human, OWNER_TRANSITION_DOMAIN, &body).expect("human proof");
    let next_recovery_key_proofs = policy
        .guardians
        .iter()
        .map(|guardian| {
            let public_key = &guardian.key.as_ref().expect("guardian key").public_key;
            let signer = guardian_signers
                .iter()
                .find(|signer| signer.public_key() == public_key)
                .expect("guardian signer");
            sign_canonical(signer, OWNER_TRANSITION_DOMAIN, &body).expect("guardian proof")
        })
        .collect();
    build_claim_deferred_human(
        agent,
        signed_root,
        BrowserClaimDeferredHuman {
            next_authority_key,
            next_authority_key_proof,
            next_recovery_policy: policy,
            next_recovery_key_proofs,
            valid_from_unix_seconds,
            nonce,
        },
    )
    .expect("device co-signs ClaimDeferredHuman")
}
use tempfile::TempDir;

use super::{
    auth_login_agent::finish_invite_create_from_response,
    hosted::{CallContextFactory, HostedError},
    identity_state,
    owner_root::{
        BootstrapOwnerRootExtension, BrowserClaimDeferredHuman, RegisterPublicKeyClaimExtension,
        build_claim_deferred_human, encode_bootstrap_owner_root, encode_register_public_key_claim,
        load_recorded_root, prepare_register_public_key_claim, require_enrolling_device_proof_key,
    },
};

struct IsolatedHome {
    _guard: MutexGuard<'static, ()>,
    _temp: TempDir,
    prev_home: Option<OsString>,
    prev_heddle_home: Option<OsString>,
    prev_credential: Option<OsString>,
}

impl IsolatedHome {
    fn new() -> Self {
        let guard = credentials::lock_test_env();
        let temp = TempDir::new().expect("temp home");
        let prev_home = std::env::var_os("HOME");
        let prev_heddle_home = std::env::var_os("HEDDLE_HOME");
        let prev_credential = std::env::var_os("HEDDLE_CREDENTIAL");
        unsafe {
            std::env::set_var("HOME", temp.path());
            std::env::remove_var("HEDDLE_HOME");
            std::env::remove_var("HEDDLE_CREDENTIAL");
        }
        Self {
            _guard: guard,
            _temp: temp,
            prev_home,
            prev_heddle_home,
            prev_credential,
        }
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        unsafe {
            match &self.prev_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &self.prev_heddle_home {
                Some(value) => std::env::set_var("HEDDLE_HOME", value),
                None => std::env::remove_var("HEDDLE_HOME"),
            }
            match &self.prev_credential {
                Some(value) => std::env::set_var("HEDDLE_CREDENTIAL", value),
                None => std::env::remove_var("HEDDLE_CREDENTIAL"),
            }
        }
    }
}

#[test]
fn invite_create_mints_a_claimable_seq0_root_on_the_agent_proof_key() {
    let _home = IsolatedHome::new();
    let server = "api.owner-root.test";
    let output = finish_invite_create_from_response(
        server,
        CreateAgentAccountResponse {
            account_id: "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28".into(),
            pet_name: "quiet-otter".into(),
            agent_capability: Vec::new(),
            web_origin: String::new(),
        },
    )
    .expect("invite create");
    let state = identity_state::load()
        .expect("load")
        .expect("claim state stored");
    let signed = load_recorded_root(&state)
        .expect("load root")
        .expect("claimable root minted on signup");
    let root = signed.root.as_ref().expect("body");
    assert!(root.claimable_deferred_human);
    assert_eq!(root.format_version, 1);
    assert_eq!(
        seq0_authority_public_key(&signed).expect("seq-0"),
        hex::decode(&state.node_id).expect("node id").as_slice()
    );
    assert_eq!(
        state.seq0_public_key().expect("recorded seq-0"),
        hex::decode(&state.node_id).expect("node id")
    );
    assert_eq!(output.next.command, "heddle claim");
}

#[test]
fn claim_transition_is_claim_deferred_human_not_a_replacement_seq0() {
    let _home = IsolatedHome::new();
    finish_invite_create_from_response(
        "api.claim-transition.test",
        CreateAgentAccountResponse {
            account_id: "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28".into(),
            pet_name: "quiet-otter".into(),
            agent_capability: Vec::new(),
            web_origin: String::new(),
        },
    )
    .expect("signup");
    let state = identity_state::load().expect("load").expect("state");
    let signed = load_recorded_root(&state).expect("root").expect("minted");
    let agent_pem = credentials::get_server_credential("api.claim-transition.test")
        .expect("cred")
        .expect("stored")
        .private_key_pem
        .expect("proof pem");
    let agent = Ed25519Signer::from_pem(&agent_pem).expect("agent");
    let human = Ed25519Signer::generate().expect("human");
    let g1 = Ed25519Signer::generate().expect("g1");
    let g2 = Ed25519Signer::generate().expect("g2");
    let mut guardians = vec![
        RecoveryGuardian {
            kind: RecoveryGuardianKind::Paper as i32,
            key: Some(ed25519_verification_key(g1.public_key()).expect("g1")),
        },
        RecoveryGuardian {
            kind: RecoveryGuardianKind::Paper as i32,
            key: Some(ed25519_verification_key(g2.public_key()).expect("g2")),
        },
    ];
    guardians.sort_by_key(|guardian| authorization_key_id(guardian.key.as_ref().expect("key")));
    let transition = build_with_browser_proofs(
        &agent,
        &human,
        &signed,
        RecoveryPolicy {
            threshold: 2,
            guardians,
            window_secs: None,
        },
        &[g1, g2],
        chrono::Utc::now().timestamp(),
    );
    let body = transition.transition.as_ref().expect("body");
    assert_eq!(body.kind(), OwnerKeyTransitionKind::ClaimDeferredHuman);
    assert_eq!(body.sequence, 1);
    assert_eq!(
        body.next_authority_key.as_ref().expect("next").public_key,
        human.public_key()
    );
    assert_eq!(
        seq0_authority_public_key(&signed).expect("seq-0 survives claim"),
        agent.public_key(),
        "claim must not mint a replacement human sequence-0"
    );
}

#[test]
fn create_spool_genesis_refuses_a_key_that_is_not_seq0() {
    let _home = IsolatedHome::new();
    finish_invite_create_from_response(
        "api.seq0-mismatch.test",
        CreateAgentAccountResponse {
            account_id: "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28".into(),
            pet_name: "quiet-otter".into(),
            agent_capability: Vec::new(),
            web_origin: String::new(),
        },
    )
    .expect("signup");
    let other = Ed25519Signer::generate().expect("other key");
    let factory = CallContextFactory::default()
        .with_signing_key_pem(&other.to_pem().expect("pem"), "principal:other")
        .expect("factory");
    let error = factory
        .mint_spool_owner_genesis()
        .expect_err("CreateSpool must refuse a genesis key that is not seq-0");
    assert!(
        matches!(error, HostedError::Framing(ref message) if message.contains("sequence-0")),
        "{error}"
    );
}

#[test]
fn create_spool_genesis_matches_the_stored_seq0_proof_key() {
    let _home = IsolatedHome::new();
    finish_invite_create_from_response(
        "api.seq0-match.test",
        CreateAgentAccountResponse {
            account_id: "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28".into(),
            pet_name: "quiet-otter".into(),
            agent_capability: Vec::new(),
            web_origin: String::new(),
        },
    )
    .expect("signup");
    let pem = credentials::get_server_credential("api.seq0-match.test")
        .expect("cred")
        .expect("stored")
        .private_key_pem
        .expect("proof pem");
    let signer = Ed25519Signer::from_pem(&pem).expect("agent");
    let factory = CallContextFactory::default()
        .with_signing_key_pem(&pem, "principal:agent")
        .expect("factory");
    let genesis = factory
        .mint_spool_owner_genesis()
        .expect("same proof key as seq-0");
    assert_eq!(
        genesis
            .genesis
            .expect("body")
            .owner_public_key
            .expect("key")
            .public_key,
        signer.public_key()
    );
}

#[test]
fn bootstrap_owner_root_wire_carries_agent_claim_binding_on_tag_5() {
    let _home = IsolatedHome::new();
    finish_invite_create_from_response(
        "api.bootstrap-binding.test",
        CreateAgentAccountResponse {
            account_id: "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28".into(),
            pet_name: "quiet-otter".into(),
            agent_capability: Vec::new(),
            web_origin: String::new(),
        },
    )
    .expect("signup");
    let state = identity_state::load().expect("load").expect("state");
    let signed = load_recorded_root(&state).expect("root").expect("minted");
    let pem = credentials::get_server_credential("api.bootstrap-binding.test")
        .expect("cred")
        .expect("stored")
        .private_key_pem
        .expect("proof pem");
    let agent = Ed25519Signer::from_pem(&pem).expect("agent");
    let operation_id = "op-bootstrap-1";
    let binding = sign_agent_claim_binding(&agent, &signed, operation_id).expect("binding");
    assert_eq!(binding.kind(), OwnerKeyBindingKind::AgentClaim);
    let encoded = encode_bootstrap_owner_root(
        &BootstrapOwnerRootRequest {
            owner_root: Some(signed.clone()),
            approval: None,
            client_operation_id: operation_id.to_string(),
            owner_key_binding: None,
        },
        &binding,
    );
    let official = BootstrapOwnerRootRequest::decode(encoded.as_slice()).expect("official fields");
    assert!(official.owner_root.is_some());
    assert_eq!(official.client_operation_id, operation_id);
    let extension = BootstrapOwnerRootExtension::decode(encoded.as_slice()).expect("tag 5");
    let decoded = extension.owner_key_binding.expect("owner_key_binding");
    assert_eq!(decoded.kind(), OwnerKeyBindingKind::AgentClaim);
    assert_eq!(decoded.challenge_nonce, binding.challenge_nonce);
}

#[test]
#[allow(deprecated)]
fn register_public_key_claim_sends_claim_deferred_human_on_tag_16() {
    let _home = IsolatedHome::new();
    finish_invite_create_from_response(
        "api.register-claim.test",
        CreateAgentAccountResponse {
            account_id: "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28".into(),
            pet_name: "quiet-otter".into(),
            agent_capability: Vec::new(),
            web_origin: String::new(),
        },
    )
    .expect("signup");
    let state = identity_state::load().expect("load").expect("state");
    let signed = load_recorded_root(&state).expect("root").expect("minted");
    let pem = credentials::get_server_credential("api.register-claim.test")
        .expect("cred")
        .expect("stored")
        .private_key_pem
        .expect("proof pem");
    let agent = Ed25519Signer::from_pem(&pem).expect("agent");
    let human = Ed25519Signer::generate().expect("human");
    let g1 = Ed25519Signer::generate().expect("g1");
    let g2 = Ed25519Signer::generate().expect("g2");
    let mut guardians = vec![
        RecoveryGuardian {
            kind: RecoveryGuardianKind::Paper as i32,
            key: Some(ed25519_verification_key(g1.public_key()).expect("g1")),
        },
        RecoveryGuardian {
            kind: RecoveryGuardianKind::Paper as i32,
            key: Some(ed25519_verification_key(g2.public_key()).expect("g2")),
        },
    ];
    guardians.sort_by_key(|guardian| authorization_key_id(guardian.key.as_ref().expect("key")));
    let transition = build_with_browser_proofs(
        &agent,
        &human,
        &signed,
        RecoveryPolicy {
            threshold: 2,
            guardians,
            window_secs: None,
        },
        &[g1, g2],
        chrono::Utc::now().timestamp(),
    );
    let request = RegisterPublicKeyRequest {
        challenge_id: "challenge-1".into(),
        device_proof_public_key: agent.public_key().to_vec(),
        client_operation_id: "op-claim-1".into(),
        ..Default::default()
    };
    let encoded = encode_register_public_key_claim(&request, &transition).expect("wire");
    let official = RegisterPublicKeyRequest::decode(encoded.as_slice()).expect("official");
    assert!(official.owner_root.is_none());
    assert!(official.owner_root_proof_of_possession.is_none());
    assert!(official.owner_key_binding.is_none());
    assert!(official.device_public_key.is_empty());
    assert!(official.biscuit_authority_public_key.is_empty());
    assert_eq!(official.device_proof_public_key, agent.public_key());
    let extension = RegisterPublicKeyClaimExtension::decode(encoded.as_slice()).expect("tag 16");
    let sent = extension
        .claim_deferred_human
        .expect("claim_deferred_human");
    assert_eq!(
        sent.transition.as_ref().expect("body").kind(),
        OwnerKeyTransitionKind::ClaimDeferredHuman
    );
    assert_eq!(
        seq0_authority_public_key(&signed).expect("seq-0 survives claim encode"),
        agent.public_key()
    );
}

#[test]
fn register_public_key_claim_refuses_a_replacement_seq0() {
    let error = encode_register_public_key_claim(
        &RegisterPublicKeyRequest {
            owner_root: Some(SignedOwnerRoot::default()),
            client_operation_id: "op-bad".into(),
            ..Default::default()
        },
        &Default::default(),
    )
    .expect_err("replacement seq-0 must not be encoded");
    assert!(
        error.to_string().contains("must not send owner_root"),
        "{error}"
    );
}

#[test]
#[allow(deprecated)]
fn register_public_key_claim_refuses_retired_device_public_key() {
    let error = encode_register_public_key_claim(
        &RegisterPublicKeyRequest {
            device_public_key: vec![0x11; 32],
            device_proof_public_key: vec![0x22; 32],
            client_operation_id: "op-retired".into(),
            ..Default::default()
        },
        &Default::default(),
    )
    .expect_err("retired device_public_key must not be encoded");
    assert!(
        error
            .to_string()
            .contains("must not send device_public_key"),
        "{error}"
    );
}

#[test]
fn register_public_key_claim_refuses_retired_biscuit_authority_public_key() {
    let error = encode_register_public_key_claim(
        &RegisterPublicKeyRequest {
            biscuit_authority_public_key: vec![0x11; 32],
            device_proof_public_key: vec![0x22; 32],
            client_operation_id: "op-authority".into(),
            ..Default::default()
        },
        &Default::default(),
    )
    .expect_err("retired biscuit_authority_public_key must not be encoded");
    assert!(
        error
            .to_string()
            .contains("must not send biscuit_authority_public_key"),
        "{error}"
    );
}

#[test]
fn register_public_key_claim_requires_device_proof_public_key() {
    let error = encode_register_public_key_claim(
        &RegisterPublicKeyRequest {
            client_operation_id: "op-empty".into(),
            ..Default::default()
        },
        &Default::default(),
    )
    .expect_err("empty device_proof_public_key must not be encoded");
    assert!(
        error
            .to_string()
            .contains("device_proof_public_key must be 32 bytes"),
        "{error}"
    );
}

#[test]
#[allow(deprecated)]
fn claim_prepares_register_public_key_claim_deferred_human_for_send() {
    let _home = IsolatedHome::new();
    finish_invite_create_from_response(
        "api.prepare-claim.test",
        CreateAgentAccountResponse {
            account_id: "7ed1b633-64dd-4b78-b3a8-7f8e08fc4a28".into(),
            pet_name: "quiet-otter".into(),
            agent_capability: Vec::new(),
            web_origin: String::new(),
        },
    )
    .expect("signup");
    let mut state = identity_state::load().expect("load").expect("state");
    let signed = load_recorded_root(&state).expect("root").expect("minted");
    let pem = credentials::get_server_credential("api.prepare-claim.test")
        .expect("cred")
        .expect("stored")
        .private_key_pem
        .expect("proof pem");
    let agent = Ed25519Signer::from_pem(&pem).expect("agent");
    let human = Ed25519Signer::generate().expect("human");
    let g1 = Ed25519Signer::generate().expect("g1");
    let g2 = Ed25519Signer::generate().expect("g2");
    let mut guardians = vec![
        RecoveryGuardian {
            kind: RecoveryGuardianKind::Paper as i32,
            key: Some(ed25519_verification_key(g1.public_key()).expect("g1")),
        },
        RecoveryGuardian {
            kind: RecoveryGuardianKind::Paper as i32,
            key: Some(ed25519_verification_key(g2.public_key()).expect("g2")),
        },
    ];
    guardians.sort_by_key(|guardian| authorization_key_id(guardian.key.as_ref().expect("key")));
    let transition = build_with_browser_proofs(
        &agent,
        &human,
        &signed,
        RecoveryPolicy {
            threshold: 2,
            guardians,
            window_secs: None,
        },
        &[g1, g2],
        chrono::Utc::now().timestamp(),
    );
    prepare_register_public_key_claim(
        &mut state,
        RegisterPublicKeyRequest {
            challenge_id: "challenge-prepare".into(),
            device_proof_public_key: agent.public_key().to_vec(),
            client_operation_id: "op-prepare".into(),
            ..Default::default()
        },
        transition,
    )
    .expect("prepare");
    let encoded = state
        .take_pending_register_public_key()
        .expect("take")
        .expect("pending");
    let official = RegisterPublicKeyRequest::decode(encoded.as_slice()).expect("official");
    assert!(official.owner_root.is_none());
    assert!(official.owner_key_binding.is_none());
    assert!(official.device_public_key.is_empty());
    assert_eq!(official.device_proof_public_key, agent.public_key());
    let extension = RegisterPublicKeyClaimExtension::decode(encoded.as_slice()).expect("tag 16");
    assert_eq!(
        extension
            .claim_deferred_human
            .expect("transition")
            .transition
            .expect("body")
            .kind(),
        OwnerKeyTransitionKind::ClaimDeferredHuman
    );
}

#[test]
#[allow(deprecated)]
fn pending_register_public_key_must_match_this_device_proof_key() {
    let signer = Ed25519Signer::generate().expect("device");
    let encoded = encode_register_public_key_claim(
        &RegisterPublicKeyRequest {
            device_proof_public_key: signer.public_key().to_vec(),
            client_operation_id: "op-match".into(),
            ..Default::default()
        },
        &{
            let mut transition = SignedOwnerKeyTransition::default();
            transition.transition = Some(api::heddle::api::v1alpha1::OwnerKeyTransition {
                kind: OwnerKeyTransitionKind::ClaimDeferredHuman as i32,
                ..Default::default()
            });
            transition
        },
    )
    .expect("one-key request");
    require_enrolling_device_proof_key(&encoded, &signer).expect("matching device key");
    let other = Ed25519Signer::generate().expect("other device");
    let error = require_enrolling_device_proof_key(&encoded, &other)
        .expect_err("a different device key must not prove this enrollment");
    assert!(
        error
            .to_string()
            .contains("must be this device's proof key"),
        "{error}"
    );
}
