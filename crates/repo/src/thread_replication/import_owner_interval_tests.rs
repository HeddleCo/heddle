use heddleco_capability_verifier as verifier;

use super::*;

#[test]
fn owner_at_before_claim_returns_prior_authority_and_true_interval() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../capability-verifier/conformance/hybrid/claimed-owner-expiry-v1.json"
    ))
    .expect("signed owner-history fixture");
    let mut ring: wire::CloneAuthorizationKeyring = wire::CloneAuthorizationKeyring::decode(
        hex::decode(fixture["keyring_hex"].as_str().expect("keyring"))
            .expect("hex")
            .as_slice(),
    )
    .expect("wire");
    let claimed = &fixture["cases"][0];
    let history = wire::OwnerHistory::decode(
        hex::decode(
            claimed["owner_history_hex"]
                .as_str()
                .expect("claimed history"),
        )
        .expect("hex")
        .as_slice(),
    )
    .expect("signed claim history");
    assert_eq!(history.root, ring.owner_root);
    ring.accepted_transitions = history.accepted_transitions;
    ring.accepted_state_hash = history.state_hash;
    let limits = verifier::VerificationLimits::new(3600).expect("limits");
    let verified = verifier::verify_clone_keyring(ring.clone(), 1250, limits, &[])
        .expect("independently verified owner history");
    let mut states = BTreeMap::new();
    let mut owner = verifier::verify_owner_root(ring.owner_root.as_ref().expect("root"))
        .expect("root signature");
    states.insert(owner.state_hash(), owner.clone());
    for transition in &ring.accepted_transitions {
        owner = verifier::apply_accepted_transition(&owner, transition, 1250, limits)
            .expect("accepted claim");
        states.insert(owner.state_hash(), owner.clone());
    }
    let genesis: [u8; 32] = hex::decode(fixture["spool_genesis_hex"].as_str().expect("genesis"))
        .expect("hex")
        .try_into()
        .expect("digest");
    let initial: [u8; 32] = hex::decode(fixture["initial_owner_hex"].as_str().expect("owner"))
        .expect("hex")
        .try_into()
        .expect("owner id");
    let facts = import_owner_facts(
        &states,
        Selection {
            owner: &owner,
            keyring: &verified,
            spool_genesis_digest: &genesis,
            initial_owner_id: &initial,
            limits,
        },
        Vec::new(),
        &[],
    )
    .expect("verified effective timeline");
    assert_eq!(facts.len(), 2);
    let current = &facts[1].identity;
    let prior = owner_fact_at(&facts, current, Some(1050)).expect("before claim");
    assert_eq!(
        (prior.from, prior.until, prior.expiry),
        (0, Some(1100), 1200)
    );
    assert_ne!(prior.identity.owner_state_hash, current.owner_state_hash);
    assert_ne!(prior.key, facts[1].key);
    let claimed = owner_fact_at(&facts, current, Some(1100)).expect("claim activation");
    assert_eq!(
        (claimed.from, claimed.until, claimed.expiry),
        (1100, None, i64::MAX)
    );
    let certificate = &fixture["cases"][0]["certificate_hex"];
    let certificate = wire::SignedImportJobDelegationV1::decode(
        hex::decode(certificate.as_str().expect("certificate"))
            .expect("hex")
            .as_slice(),
    )
    .expect("signed claimed-state certificate");
    let expectation = contract::ImportOwnerExpectation {
        identity: &prior.identity,
        owner_public_key: &prior.key,
        owner_chain_digest: &prior.chain,
        authority_expires_at_seconds: prior.expiry,
        now_unix_seconds: 1050,
        forbidden_job_keys: &prior.forbidden,
        known_job_associations: &[],
    };
    assert!(
        contract::verify_delegation(&certificate, None, &expectation).is_err(),
        "the API refuses claimed authority resolved before the claim"
    );
}

#[test]
fn owner_at_transfer_caps_prior_root_and_starts_new_owner_at_acceptance() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/hybrid/import-authority-host-witness-v1.json"
    ))
    .expect("fixture");
    let device = crypto::Ed25519Signer::from_seed(&[72; 32]).expect("device");
    let (bundle, authority, _) =
        super::super::hosted_trust_tests::transferred_bundle(&fixture, &device);
    let states =
        public_owners((&bundle).into(), 1350).expect("authenticated transfer and owner histories");
    let prior = import_owner_facts(
        &states,
        authority.selection(0),
        Vec::new(),
        &bundle.ownership_transfers,
    )
    .expect("old resource owner");
    let next = import_owner_facts(
        &states,
        authority.selection(1),
        Vec::new(),
        &bundle.ownership_transfers,
    )
    .expect("new resource owner");
    assert_eq!(
        (prior[0].from, prior[0].until),
        (0, Some(1250)),
        "prior root ends at accepted transfer"
    );
    assert_eq!(
        (next[0].from, next[0].until),
        (1250, None),
        "new root starts at accepted transfer, not account creation"
    );
    let current = next[0].identity.clone();
    let mut timeline = prior;
    timeline.extend(next);
    let before = owner_fact_at(&timeline, &current, Some(1249)).expect("prior state");
    let after = owner_fact_at(&timeline, &current, Some(1250)).expect("transferred state");
    assert_ne!(before.identity.owner_id, after.identity.owner_id);
    let expectation = after.expectation(&[]);
    let expected = contract::ImportOwnerExpectation {
        identity: expectation.identity,
        owner_public_key: expectation.owner_public_key,
        owner_chain_digest: expectation.owner_chain_digest,
        authority_expires_at_seconds: expectation.authority_expires_at_seconds,
        now_unix_seconds: 1250,
        forbidden_job_keys: expectation.forbidden_job_keys,
        known_job_associations: &[],
    };
    assert!(
        contract::verify_delegation(
            &bundle.delegations[0],
            bundle.member_permission.as_ref(),
            &expected
        )
        .is_err(),
        "prior resource authority cannot execute after transfer"
    );
}
