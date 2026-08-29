// SPDX-License-Identifier: Apache-2.0

use api::heddle::api::v1alpha1::{
    OwnerKeyBindingKind, OwnerKeyTransitionKind, RecoveryGuardian, RecoveryGuardianKind,
    RecoveryPolicy,
};
use crypto::Signer;
use heddleco_capability_verifier::{
    VerificationLimits, apply_transition, verify_owner_key_binding, verify_owner_root,
    verify_spool_owner_genesis,
};

use crate::{
    ClaimDeferredHuman, authorization_key_id, ed25519_verification_key, registration_binding_nonce,
    require_genesis_matches_seq0, seq0_authority_public_key, sign_agent_claim_binding,
    sign_claim_deferred_human, sign_claimable_deferred_human_root, sign_spool_owner_genesis,
};

const NOW: i64 = 1_700_000_000;
const ACCOUNT: [u8; 16] = [0x11; 16];

fn paper_policy(left: &impl Signer, right: &impl Signer) -> RecoveryPolicy {
    let mut guardians = vec![
        RecoveryGuardian {
            kind: RecoveryGuardianKind::Paper as i32,
            key: Some(ed25519_verification_key(left.public_key()).expect("left guardian")),
        },
        RecoveryGuardian {
            kind: RecoveryGuardianKind::Paper as i32,
            key: Some(ed25519_verification_key(right.public_key()).expect("right guardian")),
        },
    ];
    guardians.sort_by_key(|guardian| {
        authorization_key_id(guardian.key.as_ref().expect("guardian key"))
    });
    RecoveryPolicy {
        threshold: 2,
        guardians,
        window_secs: None,
    }
}

#[test]
fn claimable_deferred_human_root_is_protocol_1_with_the_device_key() {
    let agent = crypto::Ed25519Signer::generate().expect("agent proof key");
    let signed = sign_claimable_deferred_human_root(&agent, ACCOUNT, [0x42; 32], NOW)
        .expect("mint claimable root");
    let state = verify_owner_root(&signed).expect("verifier accepts the minted root");
    let root = signed.root.as_ref().expect("root body");

    assert_eq!(root.format_version, 1);
    assert!(root.claimable_deferred_human);
    assert_eq!(
        root.claimable_until_unix_seconds,
        NOW + crate::CLAIMABLE_DEFERRED_HUMAN_TTL_SECS
    );
    assert_eq!(root.account_uuid, ACCOUNT);
    assert_eq!(
        root.authority_key.as_ref().expect("authority").public_key,
        agent.public_key()
    );
    assert_eq!(state.sequence(), 0);
    assert_eq!(
        seq0_authority_public_key(&signed).expect("seq-0 key"),
        agent.public_key()
    );
    let policy = root
        .recovery_policy
        .as_ref()
        .expect("empty recovery while claimable");
    assert_eq!(policy.threshold, 0);
    assert!(policy.guardians.is_empty());
}

#[test]
fn claim_deferred_human_advances_authority_without_replacing_sequence_0() {
    let agent = crypto::Ed25519Signer::generate().expect("agent proof key");
    let human = crypto::Ed25519Signer::generate().expect("human device root");
    let g1 = crypto::Ed25519Signer::generate().expect("guardian 1");
    let g2 = crypto::Ed25519Signer::generate().expect("guardian 2");
    let signed = sign_claimable_deferred_human_root(&agent, ACCOUNT, [0x7; 32], NOW)
        .expect("mint claimable root");
    let seq0 = seq0_authority_public_key(&signed)
        .expect("seq-0")
        .to_vec();
    let policy = paper_policy(&g1, &g2);

    let signed_transition = sign_claim_deferred_human(ClaimDeferredHuman {
        current_authority: &agent,
        next_authority: &human,
        signed_root: &signed,
        next_recovery_policy: policy,
        next_guardian_signers: &[g1, g2],
        now_unix_seconds: NOW + 60,
        nonce: [0x9; 32],
    })
    .expect("claim transition");

    let transition = signed_transition.transition.as_ref().expect("transition");
    assert_eq!(transition.kind(), OwnerKeyTransitionKind::ClaimDeferredHuman);
    assert_eq!(transition.sequence, 1);
    assert_eq!(
        transition
            .next_authority_key
            .as_ref()
            .expect("next")
            .public_key,
        human.public_key()
    );

    let limits = VerificationLimits::new(30 * 24 * 60 * 60).expect("limits");
    let claimed = apply_transition(
        &verify_owner_root(&signed).expect("root"),
        &signed_transition,
        NOW + 60,
        limits,
    )
    .expect("verifier applies ClaimDeferredHuman");
    assert_eq!(claimed.sequence(), 1);
    assert_eq!(claimed.authority_key().public_key, human.public_key());
    assert_eq!(
        seq0_authority_public_key(&signed).expect("seq-0 after claim"),
        seq0.as_slice(),
        "claim must not rewrite sequence-0"
    );
    assert_eq!(seq0, agent.public_key());
}

#[test]
fn create_spool_genesis_uses_the_device_key_and_refuses_a_different_seq0() {
    let agent = crypto::Ed25519Signer::generate().expect("device proof key");
    let other = crypto::Ed25519Signer::generate().expect("throwaway key");
    let spool_uuid = uuid::Uuid::now_v7();
    assert_eq!(spool_uuid.get_version_num(), 7);

    let matched = sign_spool_owner_genesis(&agent, *spool_uuid.as_bytes()).expect("genesis");
    verify_spool_owner_genesis(&matched).expect("genesis verifies");
    require_genesis_matches_seq0(&matched, agent.public_key()).expect("same key as seq-0");

    let mismatched = sign_spool_owner_genesis(&other, *spool_uuid.as_bytes()).expect("other genesis");
    let error = require_genesis_matches_seq0(&mismatched, agent.public_key())
        .expect_err("throwaway per-spool key must not pass the seq-0 check");
    assert!(
        error.to_string().contains("sequence-0"),
        "mismatch must name the seq-0 pin: {error}"
    );
}

#[test]
fn claim_refuses_to_install_the_agent_key_as_the_human_next_authority() {
    let agent = crypto::Ed25519Signer::generate().expect("agent");
    let signed =
        sign_claimable_deferred_human_root(&agent, ACCOUNT, [0x3; 32], NOW).expect("root");
    let error = sign_claim_deferred_human(ClaimDeferredHuman {
        current_authority: &agent,
        next_authority: &agent,
        signed_root: &signed,
        next_recovery_policy: RecoveryPolicy {
            threshold: 0,
            guardians: Vec::new(),
            window_secs: None,
        },
        next_guardian_signers: &[] as &[crypto::Ed25519Signer],
        now_unix_seconds: NOW + 1,
        nonce: [0x4; 32],
    })
    .expect_err("human next authority is required");
    assert!(error.to_string().contains("human device root"), "{error}");
}

#[test]
fn agent_claim_binding_uses_registration_nonce_and_verifies_against_the_root() {
    let agent = crypto::Ed25519Signer::generate().expect("agent proof key");
    let signed = sign_claimable_deferred_human_root(&agent, ACCOUNT, [0x5; 32], NOW)
        .expect("mint claimable root");
    let operation_id = "op-bind-1";
    let binding = sign_agent_claim_binding(&agent, &signed, operation_id).expect("binding");
    assert_eq!(binding.kind(), OwnerKeyBindingKind::AgentClaim);
    assert_eq!(binding.binding_epoch, 1);
    assert_eq!(
        binding.challenge_nonce.as_slice(),
        registration_binding_nonce(operation_id).as_slice()
    );
    let state = verify_owner_root(&signed).expect("root");
    verify_owner_key_binding(&binding, &state, &ACCOUNT).expect("weft verifies the binding");
}
