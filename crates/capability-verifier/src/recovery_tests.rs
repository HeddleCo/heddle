use super::*;

fn recovery_case() -> (VerifiedOwnerState, SignedOwnerKeyTransition) {
    let authority = TestKey::new(1);
    let paper = TestKey::new(2);
    let social = TestKey::new(3);
    let next = TestKey::new(7);
    let next_paper = TestKey::new(8);
    let next_social = TestKey::new(9);
    let guardians = [
        (&paper, RecoveryGuardianKind::Paper),
        (&social, RecoveryGuardianKind::Social),
    ];
    let root = signed_root_with_policy(
        OWNER_UUID,
        &authority,
        &guardians,
        recovery_policy(&guardians, Some(100)),
    );
    let state = verify_owner_root(&root).expect("root");
    let recover = recovery_transition(
        &state,
        &[&paper, &social],
        &next,
        recovery_policy(
            &[
                (&next_paper, RecoveryGuardianKind::Paper),
                (&next_social, RecoveryGuardianKind::Social),
            ],
            Some(20),
        ),
        NOW + 100,
        &[&next_paper, &next_social],
    );
    (state, recover)
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn recover_with_valid_new_policy_is_admitted_and_folded() {
    let (state, recover) = recovery_case();
    let recovered = apply_transition_with_timelock(&state, &recover, NOW + 100, NOW, limits())
        .expect("current guardians authorize atomic policy replacement");
    let transition = recover.transition.as_ref().expect("transition");
    assert_eq!(
        recovered.recovery_policy(),
        transition
            .next_recovery_policy
            .as_ref()
            .expect("next policy")
    );
    assert_eq!(recovered.authority_key(), &TestKey::new(7).wire());
    assert_eq!(recovered.sequence(), 1);
    assert_eq!(effective_recovery_window(recovered.recovery_policy()), 20);
    assert_eq!(effective_recovery_window(state.recovery_policy()), 100);
    assert!(matches!(
        apply_transition_with_timelock(&state, &recover, NOW + 99, NOW, limits()),
        Err(Error::NotYetValid)
    ));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn recover_without_next_policy_is_rejected() {
    let (state, _) = recovery_case();
    // The old wire contract echoed the current policy without new-key proofs.
    let legacy = recovery_transition(
        &state,
        &[&TestKey::new(2), &TestKey::new(3)],
        &TestKey::new(7),
        state.recovery_policy().clone(),
        NOW + 100,
        &[],
    );
    assert!(
        apply_transition_with_timelock(&state, &legacy, NOW + 100, NOW, limits()).is_err(),
        "Recover must reject a carried-forward policy without next guardian proofs"
    );
    let mut absent = legacy;
    absent
        .transition
        .as_mut()
        .expect("transition")
        .next_recovery_policy = None;
    assert!(matches!(
        apply_transition(&state, &absent, NOW + 100, limits()),
        Err(Error::Invalid(message)) if message == "transition has no next recovery policy"
    ));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn second_recover_by_old_guardians_is_rejected() {
    let (state, recover) = recovery_case();
    let recovered = apply_transition_with_timelock(&state, &recover, NOW + 100, NOW, limits())
        .expect("first recovery installs new guardians");
    let next = TestKey::new(10);
    let next_paper = TestKey::new(11);
    let next_social = TestKey::new(12);
    let policy = recovery_policy(
        &[
            (&next_paper, RecoveryGuardianKind::Paper),
            (&next_social, RecoveryGuardianKind::Social),
        ],
        Some(20),
    );
    let stolen_words = recovery_transition(
        &recovered,
        &[&TestKey::new(2), &TestKey::new(3)],
        &next,
        policy.clone(),
        NOW + 120,
        &[&next_paper, &next_social],
    );
    assert!(matches!(
        apply_transition_with_timelock(&recovered, &stolen_words, NOW + 120, NOW + 100, limits()),
        Err(Error::InvalidSignature)
    ));
    let legitimate = recovery_transition(
        &recovered,
        &[&TestKey::new(8), &TestKey::new(9)],
        &next,
        policy,
        NOW + 120,
        &[&next_paper, &next_social],
    );
    apply_transition_with_timelock(&recovered, &legitimate, NOW + 120, NOW + 100, limits())
        .expect("second recovery is authorized by the current guardians");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn recover_requires_current_threshold_and_every_next_guardian_proof() {
    let (state, valid) = recovery_case();
    let mut invalid = Vec::new();
    let mut missing = valid.clone();
    missing.next_recovery_key_proofs.pop();
    invalid.push(missing);
    let mut extra = valid.clone();
    extra
        .next_recovery_key_proofs
        .push(extra.next_recovery_key_proofs[0].clone());
    invalid.push(extra);
    let mut duplicate = valid.clone();
    duplicate.next_recovery_key_proofs[1] = duplicate.next_recovery_key_proofs[0].clone();
    invalid.push(duplicate);
    let mut reordered = valid.clone();
    reordered.next_recovery_key_proofs.reverse();
    invalid.push(reordered);
    let mut forged = valid.clone();
    forged.next_recovery_key_proofs[0].signature[0] ^= 1;
    invalid.push(forged);
    let mut insufficient = valid.clone();
    insufficient.authorizations.pop();
    invalid.push(insufficient);
    let mut next_guardians_authorize = valid.clone();
    next_guardians_authorize.authorizations = valid.next_recovery_key_proofs.clone();
    invalid.push(next_guardians_authorize);
    let mut no_next_authority = valid;
    no_next_authority.next_authority_key_proof = None;
    invalid.push(no_next_authority);
    for (index, signed) in invalid.iter().enumerate() {
        assert!(
            apply_transition_with_timelock(&state, signed, NOW + 100, NOW, limits()).is_err(),
            "invalid recovery proof case {index}"
        );
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn recover_and_policy_transition_reject_the_same_invalid_next_policies() {
    let (state, valid) = recovery_case();
    let policy = valid
        .transition
        .as_ref()
        .expect("transition")
        .next_recovery_policy
        .as_ref()
        .expect("policy");
    let mut policies = Vec::new();
    let mut empty = policy.clone();
    empty.threshold = 0;
    empty.guardians.clear();
    policies.push(empty);
    let mut insufficient = policy.clone();
    insufficient.threshold = 3;
    policies.push(insufficient);
    let mut weak = policy.clone();
    weak.threshold = 1;
    policies.push(weak);
    let mut unknown_kind = policy.clone();
    unknown_kind.guardians[0].kind = RecoveryGuardianKind::Unspecified as i32;
    policies.push(unknown_kind);
    for policy in policies {
        let recover = recovery_transition(
            &state,
            &[&TestKey::new(2), &TestKey::new(3)],
            &TestKey::new(7),
            policy.clone(),
            NOW + 100,
            &[&TestKey::new(8), &TestKey::new(9)],
        );
        let change = recovery_policy_transition(
            &state,
            &TestKey::new(1),
            &[&TestKey::new(2), &TestKey::new(3)],
            &[&TestKey::new(8), &TestKey::new(9)],
            policy,
            NOW + 100,
        );
        assert!(matches!(
            apply_transition(&state, &recover, NOW + 100, limits()),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            apply_transition(&state, &change, NOW + 100, limits()),
            Err(Error::Invalid(_))
        ));
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn recover_retires_old_threshold_even_with_fresh_possession_proofs() {
    let (state, _) = recovery_case();
    for window_secs in [Some(100), Some(20)] {
        let unchanged_guardians = recovery_transition(
            &state,
            &[&TestKey::new(2), &TestKey::new(3)],
            &TestKey::new(7),
            RecoveryPolicy {
                window_secs,
                ..state.recovery_policy().clone()
            },
            NOW + 100,
            &[&TestKey::new(2), &TestKey::new(3)],
        );
        assert!(matches!(
            apply_transition(&state, &unchanged_guardians, NOW + 100, limits()),
            Err(Error::Invalid(message)) if message.contains("retire the current policy")
        ));
    }
    let new_paper = TestKey::new(8);
    let retained_social = TestKey::new(3);
    let partial = recovery_transition(
        &state,
        &[&TestKey::new(2), &retained_social],
        &TestKey::new(7),
        recovery_policy(
            &[
                (&new_paper, RecoveryGuardianKind::Paper),
                (&retained_social, RecoveryGuardianKind::Social),
            ],
            Some(100),
        ),
        NOW + 100,
        &[&new_paper, &retained_social],
    );
    let recovered = apply_transition(&state, &partial, NOW + 100, limits())
        .expect("retaining a guardian cannot preserve the old threshold");
    let retry = recovery_transition(
        &recovered,
        &[&TestKey::new(2), &retained_social],
        &TestKey::new(10),
        recovery_policy(
            &[
                (&TestKey::new(11), RecoveryGuardianKind::Paper),
                (&TestKey::new(12), RecoveryGuardianKind::Social),
            ],
            Some(100),
        ),
        NOW + 200,
        &[&TestKey::new(11), &TestKey::new(12)],
    );
    assert!(matches!(
        apply_transition(&recovered, &retry, NOW + 200, limits()),
        Err(Error::InvalidSignature)
    ));
}

fn recovery_keyring_cases() -> Vec<serde_json::Value> {
    let (state, valid) = recovery_case();
    let recovered = apply_transition(&state, &valid, NOW + 100, limits()).expect("recovery");
    let next_policy = recovery_policy(
        &[
            (&TestKey::new(11), RecoveryGuardianKind::Paper),
            (&TestKey::new(12), RecoveryGuardianKind::Social),
        ],
        Some(20),
    );
    let second = |guardians: &[&TestKey]| {
        recovery_transition(
            &recovered,
            guardians,
            &TestKey::new(10),
            next_policy.clone(),
            NOW + 120,
            &[&TestKey::new(11), &TestKey::new(12)],
        )
    };
    let case = |name: &str, transitions: Vec<SignedOwnerKeyTransition>, expected_accept| {
        let last_body = transition_body(
            transitions
                .last()
                .expect("transition")
                .transition
                .as_ref()
                .expect("body"),
        )
        .expect("canonical body");
        let keyring = CloneAuthorizationKeyring {
            format_version: 1,
            spool_uuid: SPOOL.to_vec(),
            canonical_spool_path_segments: path(),
            pin: Some(CloneOwnerPin {
                kind: CloneOwnerPinKind::CloneTofu as i32,
                expected_owner_id: state.owner_id().to_vec(),
                first_seen_unix_seconds: NOW,
            }),
            owner_root: Some(state.signed_root().clone()),
            accepted_transitions: transitions,
            accepted_state_hash: digest(OWNER_TRANSITION_DOMAIN, &last_body).to_vec(),
            owner_genesis: Some(signed_genesis(SPOOL, &TestKey::new(1))),
            ownership_transfers: Vec::new(),
            transfer_owner_histories: Vec::new(),
        };
        serde_json::json!({"name": name, "expected_accept": expected_accept,
            "keyring_hex": hex::encode(keyring.encode_to_vec()), "now_unix_seconds": NOW + 120})
    };
    let legacy = recovery_transition(
        &state,
        &[&TestKey::new(2), &TestKey::new(3)],
        &TestKey::new(7),
        state.recovery_policy().clone(),
        NOW + 100,
        &[],
    );
    let legacy_body = legacy.transition.as_ref().expect("legacy body");
    let repeated_body = OwnerKeyTransition {
        sequence: 2,
        previous_state_hash: digest(
            OWNER_TRANSITION_DOMAIN,
            &transition_body(legacy_body).expect("legacy canonical body"),
        )
        .to_vec(),
        next_authority_key: Some(TestKey::new(10).wire()),
        valid_from_unix_seconds: NOW + 120,
        ..legacy_body.clone()
    };
    let body = transition_body(&repeated_body).expect("second legacy body");
    let mut repeated = SignedOwnerKeyTransition {
        transition: Some(repeated_body),
        authorizations: vec![
            TestKey::new(2).sign(OWNER_TRANSITION_DOMAIN, &body),
            TestKey::new(3).sign(OWNER_TRANSITION_DOMAIN, &body),
        ],
        next_authority_key_proof: Some(TestKey::new(10).sign(OWNER_TRANSITION_DOMAIN, &body)),
        next_recovery_key_proofs: Vec::new(),
    };
    repeated
        .authorizations
        .sort_by(|left, right| left.signer_key_id.cmp(&right.signer_key_id));
    let mut no_proofs = valid.clone();
    no_proofs.next_recovery_key_proofs.clear();
    let mut forged_proof = valid.clone();
    forged_proof.next_recovery_key_proofs[0].signature[0] ^= 1;
    vec![
        case("recover-new-policy", vec![valid.clone()], true),
        case("recover-without-next-policy", vec![legacy.clone()], false),
        case(
            "second-legacy-recover-used-words",
            vec![legacy, repeated],
            false,
        ),
        case("recover-without-next-proofs", vec![no_proofs], false),
        case("recover-forged-next-proof", vec![forged_proof], false),
        case(
            "second-recover-old-guardians",
            vec![valid.clone(), second(&[&TestKey::new(2), &TestKey::new(3)])],
            false,
        ),
        case(
            "second-recover-current-guardians",
            vec![valid, second(&[&TestKey::new(8), &TestKey::new(9)])],
            true,
        ),
    ]
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn recovery_rust_wasm_parity_vectors_are_current() {
    let fixture: serde_json::Value =
        serde_json::from_str(crate::conformance::KEYRING_FIXTURE_V2_JSON).expect("keyring fixture");
    let cases = fixture["cases"].as_array().expect("cases");
    for case in recovery_keyring_cases() {
        assert!(
            cases.contains(&case),
            "checked-in recovery vector {}",
            case["name"]
        );
    }
    let outcomes =
        crate::conformance::run_keyring_fixture(crate::conformance::KEYRING_FIXTURE_V2_JSON)
            .expect("same vectors on Rust and WASM");
    assert!(outcomes.iter().all(|outcome| outcome.matches));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn second_legacy_recover_by_used_words_is_rejected() {
    let outcome =
        crate::conformance::run_keyring_fixture(crate::conformance::KEYRING_FIXTURE_V2_JSON)
            .expect("keyring corpus")
            .into_iter()
            .find(|outcome| outcome.name == "second-legacy-recover-used-words")
            .expect("two Recover entries signed by the same used guardians");
    assert!(!outcome.accepted, "used words authorized a second Recover");
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
#[ignore = "maintainer-only fixture regeneration"]
fn print_recovery_keyring_fixture_json() {
    let mut fixture: serde_json::Value =
        serde_json::from_str(crate::conformance::KEYRING_FIXTURE_V2_JSON).expect("keyring fixture");
    let cases = fixture["cases"].as_array_mut().expect("cases");
    cases.retain(|case| !case["name"].as_str().expect("name").contains("recover"));
    cases.extend(recovery_keyring_cases());
    println!(
        "{}",
        serde_json::to_string_pretty(&fixture).expect("fixture JSON")
    );
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
#[ignore = "maintainer-only fixture regeneration"]
fn print_paper_recovery_fixture_json() {
    let authority = TestKey::new(1);
    let old_paper = TestKey::new(2);
    let old_paper_two = TestKey::new(3);
    let guardians = [
        (&old_paper, RecoveryGuardianKind::Paper),
        (&old_paper_two, RecoveryGuardianKind::Paper),
    ];
    let root = signed_root_with_policy(
        OWNER_UUID,
        &authority,
        &guardians,
        recovery_policy(&guardians, Some(100)),
    );
    let state = verify_owner_root(&root).expect("root");
    let mut transition = recovery_transition(
        &state,
        &[&old_paper, &old_paper_two],
        &TestKey::new(7),
        recovery_policy(
            &[
                (&TestKey::new(8), RecoveryGuardianKind::Paper),
                (&TestKey::new(9), RecoveryGuardianKind::Paper),
            ],
            Some(20),
        ),
        NOW + 100,
        &[],
    )
    .transition
    .expect("transition");
    transition.nonce = vec![3; 32];
    let body = transition_body(&transition).expect("body");
    let mut signed = SignedOwnerKeyTransition {
        transition: Some(transition),
        authorizations: vec![
            old_paper.sign(OWNER_TRANSITION_DOMAIN, &body),
            old_paper_two.sign(OWNER_TRANSITION_DOMAIN, &body),
        ],
        next_authority_key_proof: Some(TestKey::new(7).sign(OWNER_TRANSITION_DOMAIN, &body)),
        next_recovery_key_proofs: Vec::new(),
    };
    signed
        .authorizations
        .sort_by(|left, right| left.signer_key_id.cmp(&right.signer_key_id));
    sign_next_guardians(&mut signed, &[&TestKey::new(8), &TestKey::new(9)]);
    let fixture = serde_json::json!({
        "owner_root_hex": hex::encode(root.encode_to_vec()), "owner_state_hash_hex": hex::encode(state.state_hash()),
        "signed_transition_hex": hex::encode(signed.encode_to_vec()), "challenge_hex": hex::encode([3; 32]),
        "proposed_key_hex": hex::encode(TestKey::new(7).wire().public_key), "eligible_at_seconds": (NOW + 100).to_string(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&fixture).expect("fixture JSON")
    );
}
