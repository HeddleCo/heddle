use heddle_client::owner_authorization::{
    AuthorizationError, AuthorizationKey, GuardianSigner, RecoverySetup, apply_transition,
    create_claim_transition, create_deferred_owner_root, create_direct_capability,
    create_recovery_policy_transition, create_recovery_transition, create_rotation_transition,
    verify_capability_chain, verify_owner_root,
    wire::{SignedOwnerKeyTransition, SpoolCapabilityAction},
};
use prost::Message;

use crate::common::{Fixture, NOW, limits};

#[test]
fn rotation_accepts_new_key_and_retires_previous_key_after_handover() {
    let fixture = Fixture::new();
    let old_capability = fixture.direct(&[SpoolCapabilityAction::Read]);
    let next = AuthorizationKey::from_seed([7; 32]).expect("next authority");
    let rotation = create_rotation_transition(
        &fixture.state,
        &fixture.authority,
        &next,
        NOW,
        NOW + 20,
        limits(),
    )
    .expect("rotation");
    let rotated =
        apply_transition(&fixture.state, &rotation, NOW, limits()).expect("apply rotation");

    verify_capability_chain(
        &rotated,
        std::slice::from_ref(&old_capability),
        NOW + 20,
        limits(),
    )
    .expect("old key remains valid through handover");
    assert!(matches!(
        verify_capability_chain(
            &rotated,
            std::slice::from_ref(&old_capability),
            NOW + 21,
            limits(),
        ),
        Err(AuthorizationError::Expired)
    ));

    let new_capability = create_direct_capability(
        &rotated,
        &next,
        fixture.subject(),
        vec![fixture.grant(&[SpoolCapabilityAction::Read])],
        NOW,
        NOW + 200,
        limits(),
    )
    .expect("new-key capability");
    verify_capability_chain(&rotated, &[new_capability], NOW + 21, limits())
        .expect("rotated key verifies");
}

#[test]
fn recovery_requires_the_committed_threshold() {
    let fixture = Fixture::new();
    let next = AuthorizationKey::from_seed([8; 32]).expect("recovered authority");
    let guardians = fixture.recovery.guardians();
    let complete = create_recovery_transition(
        &fixture.state,
        &[guardians[0].key(), guardians[1].key()],
        &next,
        NOW,
        limits(),
    )
    .expect("threshold recovery");
    let complete = SignedOwnerKeyTransition::decode(complete.encode_to_vec().as_slice())
        .expect("recovery transition round trip");
    apply_transition(&fixture.state, &complete, NOW, limits())
        .expect("threshold-satisfied transition verifies");

    let mut one_short = complete;
    one_short.authorizations.pop();
    assert!(matches!(
        apply_transition(&fixture.state, &one_short, NOW, limits()),
        Err(AuthorizationError::RecoveryThreshold {
            required: 2,
            actual: 1
        })
    ));
}

#[test]
fn recovery_policy_transition_requires_both_anchors_and_next_possession() {
    let fixture = Fixture::new();
    let next_recovery = RecoverySetup::recommended(vec![
        GuardianSigner::paper(AuthorizationKey::from_seed([21; 32]).expect("next paper")),
        GuardianSigner::social(AuthorizationKey::from_seed([22; 32]).expect("next social")),
    ])
    .expect("next recovery");
    let current = fixture.recovery.guardians();
    let transition = create_recovery_policy_transition(
        &fixture.state,
        &fixture.authority,
        &[current[0].key(), current[1].key()],
        &next_recovery,
        NOW,
        NOW + 20,
        limits(),
    )
    .expect("recovery policy transition");
    let transition = SignedOwnerKeyTransition::decode(transition.encode_to_vec().as_slice())
        .expect("policy transition round trip");
    let updated = apply_transition(&fixture.state, &transition, NOW, limits())
        .expect("policy transition verifies");
    assert_eq!(updated.recovery_policy().threshold, 2);
    assert_eq!(updated.recovery_policy().guardians.len(), 2);
}

#[test]
fn deferred_human_claim_round_trips_and_installs_the_human_key() {
    let origin = AuthorizationKey::from_seed([23; 32]).expect("origin");
    let deferred = create_deferred_owner_root([24; 16], &origin, NOW + 100).expect("deferred root");
    let deferred_state = verify_owner_root(&deferred).expect("deferred state");
    let human = AuthorizationKey::from_seed([25; 32]).expect("human");
    let human_recovery = RecoverySetup::recommended(vec![
        GuardianSigner::paper(AuthorizationKey::from_seed([26; 32]).expect("human paper")),
        GuardianSigner::social(AuthorizationKey::from_seed([27; 32]).expect("human social")),
    ])
    .expect("human recovery");
    let claim = create_claim_transition(
        &deferred_state,
        &origin,
        &human,
        &human_recovery,
        NOW,
        NOW + 20,
        limits(),
    )
    .expect("claim transition");
    let claim = SignedOwnerKeyTransition::decode(claim.encode_to_vec().as_slice())
        .expect("claim transition round trip");
    let claimed = apply_transition(&deferred_state, &claim, NOW, limits())
        .expect("claim transition verifies");
    assert_eq!(claimed.authority_key(), &human.verification_key());
}

#[test]
fn transition_with_broken_previous_state_hash_is_refused() {
    let fixture = Fixture::new();
    let next = AuthorizationKey::from_seed([9; 32]).expect("next authority");
    let mut rotation = create_rotation_transition(
        &fixture.state,
        &fixture.authority,
        &next,
        NOW,
        NOW + 20,
        limits(),
    )
    .expect("rotation");
    rotation
        .transition
        .as_mut()
        .expect("body")
        .previous_state_hash = vec![0xAA; 32];
    assert!(matches!(
        apply_transition(&fixture.state, &rotation, NOW, limits()),
        Err(AuthorizationError::BrokenChain(_))
    ));
}

#[test]
fn future_transition_is_not_applied_early() {
    let fixture = Fixture::new();
    let next = AuthorizationKey::from_seed([10; 32]).expect("next authority");
    let rotation = create_rotation_transition(
        &fixture.state,
        &fixture.authority,
        &next,
        NOW + 10,
        NOW + 20,
        limits(),
    )
    .expect("future rotation");
    assert!(matches!(
        apply_transition(&fixture.state, &rotation, NOW, limits()),
        Err(AuthorizationError::NotYetValid)
    ));
}
