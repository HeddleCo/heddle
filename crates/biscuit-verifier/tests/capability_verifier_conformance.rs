use heddleco_capability_verifier::conformance::{
    FIXTURE_V2_JSON, KEYRING_FIXTURE_V2_JSON, TRANSFER_FIXTURE_V2_JSON, run_fixture,
    run_keyring_fixture, run_transfer_fixture,
};

#[test]
fn published_capability_verifier_matches_every_canonical_fixture() {
    let outcomes = run_fixture(FIXTURE_V2_JSON).expect("canonical fixture runs");
    assert!(
        outcomes.iter().all(|outcome| outcome.matches),
        "decision fixture mismatch: {outcomes:#?}"
    );

    let transfer = run_transfer_fixture(TRANSFER_FIXTURE_V2_JSON).expect("transfer fixture runs");
    let keyring = run_keyring_fixture(KEYRING_FIXTURE_V2_JSON).expect("keyring fixture runs");
    for outcome in transfer.into_iter().chain(keyring) {
        assert_eq!(
            outcome.accepted, outcome.expected_accept,
            "{}",
            outcome.name
        );
        assert!(outcome.matches, "guard fixture mismatch: {outcome:#?}");
    }
}
