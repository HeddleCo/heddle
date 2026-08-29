// SPDX-License-Identifier: Apache-2.0

use api::heddle::api::v1alpha1::{
    AuthorizationSignature, AuthorizationVerificationKey, OwnerKeyBindingKind, OwnerKeyTransition,
    OwnerKeyTransitionKind, RecoveryGuardian, RecoveryGuardianKind, RecoveryPolicy,
    SignedOwnerRoot,
};
use crypto::{Signer, SignerError};
use heddleco_capability_verifier::{
    VerificationLimits, apply_transition, verify_owner_key_binding, verify_owner_root,
    verify_spool_owner_genesis,
};

use crate::{
    ClaimDeferredHuman, authorization_key_id, ed25519_verification_key, registration_binding_nonce,
    require_genesis_matches_seq0, seq0_authority_public_key, sign_agent_claim_binding,
    sign_canonical, sign_claim_deferred_human, sign_claimable_deferred_human_root,
    sign_spool_owner_genesis,
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
    guardians
        .sort_by_key(|guardian| authorization_key_id(guardian.key.as_ref().expect("guardian key")));
    RecoveryPolicy {
        threshold: 2,
        guardians,
        window_secs: None,
    }
}

fn browser_claim_proofs(
    signed_root: &SignedOwnerRoot,
    human: &crypto::Ed25519Signer,
    policy: &RecoveryPolicy,
    guardian_signers: &[crypto::Ed25519Signer],
    valid_from_unix_seconds: i64,
    nonce: [u8; 32],
) -> (
    AuthorizationVerificationKey,
    AuthorizationSignature,
    Vec<AuthorizationSignature>,
) {
    let next_authority_key =
        ed25519_verification_key(human.public_key()).expect("browser human public key");
    let transition = crate::claim_deferred_human_transition(
        signed_root,
        next_authority_key.clone(),
        policy.clone(),
        valid_from_unix_seconds,
        nonce,
    )
    .expect("browser claim transition");
    let body = crate::owner_key_transition_body(&transition).expect("browser canonical body");
    let next_authority_key_proof =
        sign_canonical(human, crate::OWNER_TRANSITION_DOMAIN, &body).expect("human proof");
    let next_recovery_key_proofs = policy
        .guardians
        .iter()
        .map(|guardian| {
            let public_key = &guardian.key.as_ref().expect("guardian key").public_key;
            let signer = guardian_signers
                .iter()
                .find(|signer| signer.public_key() == public_key)
                .expect("browser owns guardian key");
            sign_canonical(signer, crate::OWNER_TRANSITION_DOMAIN, &body).expect("guardian proof")
        })
        .collect();
    (
        next_authority_key,
        next_authority_key_proof,
        next_recovery_key_proofs,
    )
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
fn agent_cosign_accepts_separately_produced_browser_proofs_without_human_signer() {
    let agent = crypto::Ed25519Signer::generate().expect("agent proof key");
    let signed = sign_claimable_deferred_human_root(&agent, ACCOUNT, [0x7; 32], NOW)
        .expect("mint claimable root");
    let seq0 = seq0_authority_public_key(&signed).expect("seq-0").to_vec();
    let valid_from = NOW + 60;
    let nonce = [0x9; 32];
    let (
        human_public_key,
        policy,
        next_authority_key,
        next_authority_key_proof,
        next_recovery_key_proofs,
    ) = {
        let human = crypto::Ed25519Signer::generate().expect("browser human device root");
        let g1 = crypto::Ed25519Signer::generate().expect("browser guardian 1");
        let g2 = crypto::Ed25519Signer::generate().expect("browser guardian 2");
        let policy = paper_policy(&g1, &g2);
        let (next_authority_key, next_authority_key_proof, next_recovery_key_proofs) =
            browser_claim_proofs(&signed, &human, &policy, &[g1, g2], valid_from, nonce);
        (
            human.public_key().to_vec(),
            policy,
            next_authority_key,
            next_authority_key_proof,
            next_recovery_key_proofs,
        )
    };

    let signed_transition = sign_claim_deferred_human(ClaimDeferredHuman {
        current_authority: &agent,
        signed_root: &signed,
        next_authority_key,
        next_authority_key_proof,
        next_recovery_policy: policy,
        next_recovery_key_proofs,
        valid_from_unix_seconds: valid_from,
        nonce,
    })
    .expect("claim transition");

    let transition = signed_transition.transition.as_ref().expect("transition");
    assert_eq!(
        transition.kind(),
        OwnerKeyTransitionKind::ClaimDeferredHuman
    );
    assert_eq!(transition.sequence, 1);
    assert_eq!(
        transition
            .next_authority_key
            .as_ref()
            .expect("next")
            .public_key,
        human_public_key
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
    assert_eq!(claimed.authority_key().public_key, human_public_key);
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

    let mismatched =
        sign_spool_owner_genesis(&other, *spool_uuid.as_bytes()).expect("other genesis");
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
    let signed = sign_claimable_deferred_human_root(&agent, ACCOUNT, [0x3; 32], NOW).expect("root");
    let error = sign_claim_deferred_human(ClaimDeferredHuman {
        current_authority: &agent,
        signed_root: &signed,
        next_authority_key: ed25519_verification_key(agent.public_key()).expect("agent public"),
        next_authority_key_proof: AuthorizationSignature::default(),
        next_recovery_policy: RecoveryPolicy {
            threshold: 0,
            guardians: Vec::new(),
            window_secs: None,
        },
        next_recovery_key_proofs: Vec::new(),
        valid_from_unix_seconds: NOW + 1,
        nonce: [0x4; 32],
    })
    .expect_err("human next authority is required");
    assert!(error.to_string().contains("human device root"), "{error}");
}

#[test]
fn claim_deferred_human_canonical_body_matches_shared_field_order_vector() {
    let transition = OwnerKeyTransition {
        format_version: 1,
        owner_id: vec![0x11; 32],
        previous_state_hash: vec![0x22; 32],
        sequence: 1,
        kind: OwnerKeyTransitionKind::ClaimDeferredHuman as i32,
        next_authority_key: Some(AuthorizationVerificationKey {
            algorithm: 1,
            public_key: vec![0x33; 32],
        }),
        next_recovery_policy: Some(RecoveryPolicy {
            threshold: 0,
            guardians: Vec::new(),
            window_secs: None,
        }),
        valid_from_unix_seconds: 1,
        previous_key_valid_until_unix_seconds: 0,
        nonce: vec![0x44; 32],
    };

    assert_eq!(
        crate::OWNER_TRANSITION_DOMAIN,
        b"heddle-owner-key-transition-v1"
    );
    assert_eq!(
        hex::encode(crate::owner_key_transition_body(&transition).expect("canonical body")),
        concat!(
            "00000001",
            "00000020",
            "1111111111111111111111111111111111111111111111111111111111111111",
            "00000020",
            "2222222222222222222222222222222222222222222222222222222222222222",
            "0000000000000001",
            "00000004",
            "00000001",
            "00000020",
            "3333333333333333333333333333333333333333333333333333333333333333",
            "00000000",
            "00000000",
            "0000000000093a80",
            "0000000000000001",
            "0000000000000000",
            "00000020",
            "4444444444444444444444444444444444444444444444444444444444444444",
        ),
        "must match tapestry transitionBody and weft transition_body byte-for-byte"
    );
}

struct PanicOnSignAgent {
    public_key: Vec<u8>,
}

impl Signer for PanicOnSignAgent {
    fn algorithm(&self) -> &'static str {
        "ed25519"
    }

    fn public_key(&self) -> &[u8] {
        &self.public_key
    }

    fn sign(&self, _data: &[u8]) -> Result<Vec<u8>, SignerError> {
        panic!("the agent must not sign before browser proofs verify")
    }

    fn verify(&self, _data: &[u8], _signature: &[u8]) -> Result<(), SignerError> {
        Err(SignerError::VerificationFailed)
    }
}

#[test]
fn forged_owner_state_binding_is_rejected_before_the_device_signs() {
    let agent = crypto::Ed25519Signer::generate().expect("agent");
    let human = crypto::Ed25519Signer::generate().expect("browser human key");
    let signed_root =
        sign_claimable_deferred_human_root(&agent, ACCOUNT, [0x51; 32], NOW).expect("root");
    let policy = RecoveryPolicy {
        threshold: 0,
        guardians: Vec::new(),
        window_secs: None,
    };
    let next_authority_key =
        ed25519_verification_key(human.public_key()).expect("human public key");
    let mut forged_transition = crate::claim_deferred_human_transition(
        &signed_root,
        next_authority_key.clone(),
        policy.clone(),
        NOW + 1,
        [0x52; 32],
    )
    .expect("browser transition");
    forged_transition.owner_id[0] ^= 0xff;
    forged_transition.previous_state_hash[0] ^= 0xff;
    forged_transition.sequence = 2;
    let forged_body =
        crate::owner_key_transition_body(&forged_transition).expect("forged body encodes");
    let next_authority_key_proof =
        sign_canonical(&human, crate::OWNER_TRANSITION_DOMAIN, &forged_body)
            .expect("browser signs forged body");
    let public_only_agent = PanicOnSignAgent {
        public_key: agent.public_key().to_vec(),
    };

    let error = sign_claim_deferred_human(ClaimDeferredHuman {
        current_authority: &public_only_agent,
        signed_root: &signed_root,
        next_authority_key,
        next_authority_key_proof,
        next_recovery_policy: policy,
        next_recovery_key_proofs: Vec::new(),
        valid_from_unix_seconds: NOW + 1,
        nonce: [0x52; 32],
    })
    .expect_err("proof over a different canonical body must fail closed");
    assert!(
        error.to_string().contains("human authority proof"),
        "{error}"
    );
}

#[test]
fn claim_after_sequence_zero_deadline_is_rejected_before_the_device_signs() {
    let agent = crypto::Ed25519Signer::generate().expect("agent");
    let human = crypto::Ed25519Signer::generate().expect("browser human key");
    let signed_root =
        sign_claimable_deferred_human_root(&agent, ACCOUNT, [0x61; 32], NOW).expect("root");
    let public_only_agent = PanicOnSignAgent {
        public_key: agent.public_key().to_vec(),
    };

    let error = sign_claim_deferred_human(ClaimDeferredHuman {
        current_authority: &public_only_agent,
        signed_root: &signed_root,
        next_authority_key: ed25519_verification_key(human.public_key()).expect("human public key"),
        next_authority_key_proof: AuthorizationSignature::default(),
        next_recovery_policy: RecoveryPolicy {
            threshold: 0,
            guardians: Vec::new(),
            window_secs: None,
        },
        next_recovery_key_proofs: Vec::new(),
        valid_from_unix_seconds: NOW + crate::CLAIMABLE_DEFERRED_HUMAN_TTL_SECS + 1,
        nonce: [0x62; 32],
    })
    .expect_err("expired claimable sequence-0 root must fail closed");
    assert!(error.to_string().contains("claimable deadline"), "{error}");
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
