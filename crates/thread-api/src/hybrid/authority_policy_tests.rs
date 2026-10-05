use crypto::{Ed25519Signer, Signer};
use prost::Message;
use repo::thread_replication::{ThreadReplica, hosted_trust::*};

use super::*;

fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!("../../tests/fixtures/hybrid-alpha33.json"))
        .expect("tagged fixture")
}
fn record<T: Message + Default>(name: &str) -> T {
    let f = fixture();
    let v = f["signed_vectors"]
        .get(name)
        .or_else(|| f["wire_vectors"].get(name))
        .expect("vector");
    T::decode(
        hex::decode(v["wire_hex"].as_str().expect("wire"))
            .expect("hex")
            .as_slice(),
    )
    .expect("record")
}
fn signer(role: &str) -> Ed25519Signer {
    Ed25519Signer::from_seed(
        &hex::decode(fixture()["keys"][role]["seed_hex"].as_str().expect("seed")).expect("hex"),
    )
    .expect("signer")
}
fn resign_observations(bundle: &mut wire::ImportPublicProofBundleV1) {
    let set = bundle.witness_set.as_mut().expect("set");
    let active = set
        .body
        .as_mut()
        .expect("body")
        .entries
        .iter_mut()
        .find(|e| e.state == 1)
        .expect("active executor");
    active.active_from_unix_millis = 0;
    let executor = active.executor_id.clone();
    set.body_digest = api::hybrid_codec::hash(&[&api::witness_trust::set_signing_bytes(
        set.body.as_ref().expect("body"),
    )
    .expect("bytes")]);
    set.root_signature = signer("root")
        .sign(
            &api::witness_trust::set_signing_bytes(set.body.as_ref().expect("body"))
                .expect("bytes"),
        )
        .expect("root signature");
    for signed in &mut bundle.statements {
        let body = signed.body.as_mut().expect("statement");
        body.executor_id = executor.clone();
        signed.signature = signer("next_witness")
            .sign(&api::witness_trust::statement_signing_digest(body).expect("digest"))
            .expect("witness signature");
    }
    bundle.history_proofs.clear();
}
struct ReceiverClock(i64);
impl Clock for ReceiverClock {
    fn now_millis(&self) -> repo::thread_replication::Result<i64> {
        Ok(self.0)
    }
    fn elapsed_millis(&self) -> repo::thread_replication::Result<u64> {
        Ok(0)
    }
}
fn install(
    bundle: wire::ImportPublicProofBundleV1,
) -> repo::thread_replication::Result<Vec<ThreadReplica>> {
    install_at(bundle, 1_350_000)
}
fn install_at(
    bundle: wire::ImportPublicProofBundleV1,
    now: i64,
) -> repo::thread_replication::Result<Vec<ThreadReplica>> {
    let limits = VerificationLimits::new(30 * 24 * 60 * 60).expect("limits");
    let pinned = tests::selected(&bundle, limits);
    let history = AcceptedHistory::from_selected_spool(&bundle, &pinned, now / 1000, limits)
        .map_err(authority_error)?;
    let directory = tempfile::tempdir().expect("fresh receiver");
    let repository = repo::Repository::init_default(directory.path()).expect("repository");
    let root = RootSelection {
        authority: "https://weft.example.test".into(),
        root_id: "descriptor-root-1".into(),
        public_key: hex::decode(
            fixture()["keys"]["root"]["public_key_hex"]
                .as_str()
                .expect("root"),
        )
        .expect("hex")
        .try_into()
        .expect("root key"),
    };
    select_root(repository.heddle_dir(), &root)?;
    select_spool(
        repository.heddle_dir(),
        pinned.owner_genesis().spool_uuid(),
        *history.genesis(),
        *history.initial_owner(),
    )?;
    let trust = HostedTrust::open(repository.heddle_dir(), &root.authority, ReceiverClock(now))?;
    let authority = SelectedAuthority::new(
        history,
        bundle.clone(),
        |_: &wire::ImportPublicProofBundleV1, _: i64, _: &TrustTransaction<'_>| Ok(()),
    );
    ThreadReplica::install_hybrid_import(
        repository.heddle_dir(),
        &trust,
        &bundle.encode_to_vec(),
        &[record("converted_main"), record("converted_dev")],
        &authority,
        repository.store(),
        |_| Ok(()),
    )
}
fn genesis_policy() -> wire::ImportPublicProofBundleV1 {
    let mut bundle = tests::bundle();
    bundle.policies.clear();
    for signed in &mut bundle.statements {
        let body = signed.body.as_mut().expect("body");
        body.policy_sequence = 0;
        body.policy_state_hash = vec![0; 32];
    }
    resign_observations(&mut bundle);
    bundle
}
#[test]
fn published_import_at_genesis_policy_installs_on_fresh_receiver() {
    let replicas = install(genesis_policy())
        .expect("authenticated genesis policy must install published import evidence");
    assert_eq!(replicas.len(), 2);
}

#[test]
fn receiver_requires_unique_consumed_publications_and_atomic_genesis_pairs() {
    for name in [
        "duplicate_p1",
        "duplicate_p3",
        "unconsumed_p3",
        "executor_mismatch",
        "p1_foreign_transaction",
        "p1_time_mismatch",
        "p1_order_after_p3",
        "p1_without_publication",
        "p1_p3_outside_window",
    ] {
        let result = install_at(record(name), 1_200_000);
        let expected = if name == "p1_p3_outside_window" {
            Reject::Expired
        } else {
            Reject::Transition
        };
        assert!(
            matches!(result, Err(repo::thread_replication::Error::Hybrid(ref rejection)) if rejection == &expected),
            "receiver must refuse {name} for its pairing/window violation: {:?}",
            result.as_ref().err()
        );
    }
    // Array order cannot choose a different publication to pair with P1.
    let mut control: wire::ImportPublicProofBundleV1 = record("current_export");
    control.statements.reverse();
    assert_eq!(
        install_at(control, 1_200_000)
            .expect("exact consumed P3s in any array order")
            .len(),
        2
    );
}
#[test]
fn positive_policy_missing_chain_rejects() {
    let mut bundle = tests::bundle();
    bundle.policies.clear();
    assert!(
        install(bundle).is_err(),
        "positive policy head requires its complete signed chain"
    );
}
#[test]
fn unknown_or_absent_policy_head_rejects() {
    for hash in [vec![1; 32], Vec::new()] {
        let mut bundle = genesis_policy();
        bundle.statements[0]
            .body
            .as_mut()
            .expect("body")
            .policy_state_hash = hash;
        resign_observations(&mut bundle);
        assert!(
            install(bundle).is_err(),
            "absent or unknown observation is not authenticated genesis"
        );
    }
}

#[test]
fn signed_record_cannot_replace_implicit_genesis_policy() {
    let mut bundle = genesis_policy();
    let mut record: wire::SignedSpoolPolicyRecord = record("signed_policy");
    let body = record.body.as_mut().expect("policy body");
    body.sequence = 0;
    body.policy_state_hash = vec![0; 32];
    record.owner_signature = Some(wire::AuthorizationSignature {
        signer_key_id: api::hybrid_codec::key_id(signer("owner").public_key()),
        signature: signer("owner")
            .sign(
                &heddleco_capability_verifier::policy::policy_signature_digest(body)
                    .expect("digest"),
            )
            .expect("signature"),
    });
    bundle.policies.push(record);
    assert!(
        install(bundle).is_err(),
        "implicit genesis accepts no signed replacement record"
    );
}
#[test]
fn real_policy_revoked_job_key_rejects() {
    let mut bundle = tests::bundle();
    let job = bundle.delegations[0]
        .body
        .as_ref()
        .expect("body")
        .job_key_id
        .clone();
    let record = &mut bundle.policies[0];
    let body = record.body.as_mut().expect("policy body");
    let policy = body.policy.as_mut().expect("policy");
    policy.revoked_key_ids.push(job);
    policy.revoked_key_ids.sort();
    policy.revoked_key_ids.dedup();
    body.policy_state_hash = heddleco_capability_verifier::policy::policy_state_hash(body)
        .expect("policy hash")
        .to_vec();
    record.owner_signature = Some(wire::AuthorizationSignature {
        signer_key_id: api::hybrid_codec::key_id(signer("owner").public_key()),
        signature: signer("owner")
            .sign(
                &heddleco_capability_verifier::policy::policy_signature_digest(body)
                    .expect("policy digest"),
            )
            .expect("policy signature"),
    });
    for signed in &mut bundle.statements {
        signed.body.as_mut().expect("body").policy_state_hash = body.policy_state_hash.clone();
    }
    resign_observations(&mut bundle);
    assert!(
        install(bundle).is_err(),
        "real signed policy revocation must reject"
    );
}

// Exercise SelectedAuthority directly: upstream bundle validation is deliberately
// outside this test, so another verifier cannot mask a missing local guard.
#[test]
fn selected_authority_zero_policy_record_refuses_import_revocations() {
    use repo::thread_replication::delegated_import::AcceptedAuthority;
    let bundle = genesis_policy();
    let limits = VerificationLimits::new(3600).expect("limits");
    let pinned = tests::selected(&bundle, limits);
    let history = AcceptedHistory::from_selected_spool(&bundle, &pinned, 1350, limits)
        .expect("verified independent owner");
    let mut authority = SelectedAuthority::new(
        history,
        bundle,
        |_: &wire::ImportPublicProofBundleV1, _: i64, _: &TrustTransaction<'_>| Ok(()),
    );
    let statement = authority
        .bundle
        .statements
        .iter()
        .find_map(|s| s.body.as_ref().filter(|s| s.purpose == 1))
        .expect("genesis observation")
        .clone();
    let job = authority.bundle.delegations[0]
        .body
        .as_ref()
        .expect("delegation")
        .job_key_id
        .clone();
    let import = permission::import_delegation::Revocation::Key(&job);
    assert!(!authority.import_revoked(&statement, import));
    let mut replacement: wire::SignedSpoolPolicyRecord = record("signed_policy");
    let body = replacement.body.as_mut().expect("policy");
    body.sequence = 0;
    body.policy_state_hash = vec![0; 32];
    replacement.owner_signature = Some(wire::AuthorizationSignature {
        signer_key_id: api::hybrid_codec::key_id(signer("owner").public_key()),
        signature: signer("owner")
            .sign(&permission::policy::policy_signature_digest(body).expect("digest"))
            .expect("signed zero record"),
    });
    authority.bundle.policies.push(replacement);
    assert!(
        authority.import_revoked(&statement, import),
        "local genesis guard must reject a signed zero record for imports"
    );
    authority.bundle.policies.clear();
    for (sequence, hash) in [(0, Vec::new()), (0, vec![1; 32]), (1, vec![1; 32])] {
        let mut unknown = statement.clone();
        unknown.policy_sequence = sequence;
        unknown.policy_state_hash = hash;
        assert!(
            authority.import_revoked(&unknown, import),
            "unknown or missing policy fails closed directly"
        );
    }
}

#[test]
fn staging_checks_each_publication_after_job_key_revocation() {
    use repo::thread_replication::delegated_import;
    let mut bundle = tests::bundle();
    let mut later = bundle.policies[0].clone();
    let body = later.body.as_mut().expect("policy");
    body.expected_head = Some(wire::SignedPolicyHead {
        state_hash: body.policy_state_hash.clone(),
        sequence: body.sequence,
    });
    body.sequence += 1;
    body.policy.as_mut().expect("policy").revoked_key_ids.push(
        bundle.delegations[0]
            .body
            .as_ref()
            .expect("job")
            .job_key_id
            .clone(),
    );
    body.policy.as_mut().expect("policy").revoked_key_ids.sort();
    body.policy_state_hash = permission::policy::policy_state_hash(body)
        .expect("hash")
        .to_vec();
    later.owner_signature = Some(wire::AuthorizationSignature {
        signer_key_id: api::hybrid_codec::key_id(signer("owner").public_key()),
        signature: signer("owner")
            .sign(&permission::policy::policy_signature_digest(body).expect("digest"))
            .expect("signature"),
    });
    for statement in &mut bundle.statements {
        let s = statement.body.as_mut().expect("statement");
        if s.observed_at_unix_millis >= 1_200_000 {
            s.policy_sequence = body.sequence;
            s.policy_state_hash = body.policy_state_hash.clone();
        }
    }
    bundle.policies.push(later);
    resign_observations(&mut bundle);
    let limits = VerificationLimits::new(3600).expect("limits");
    let pinned = tests::selected(&bundle, limits);
    let history = AcceptedHistory::from_selected_spool(&bundle, &pinned, 1350, limits)
        .expect("verified history");
    let authority = SelectedAuthority::new(
        history,
        bundle.clone(),
        |_: &wire::ImportPublicProofBundleV1, _: i64, _: &TrustTransaction<'_>| Ok(()),
    );
    let pin = api::import_authority::ImportWitnessRootPin {
        authority: "https://weft.example.test".into(),
        root_id: "descriptor-root-1".into(),
        public_key: signer("root").public_key().to_vec(),
        epoch: 1,
    };
    let result = delegated_import::authenticate_import_carriers(
        &bundle,
        &authority,
        &pin,
        1_350_000,
        &[],
        &[],
        |_| Ok(()),
    );
    assert!(
        matches!(
            result,
            Err(repo::thread_replication::Error::ImportAuthority(
                permission::Error::Hybrid(Reject::Revoked)
            ))
        ),
        "each operation must check its own publication's job revocation"
    );
    let control = tests::bundle();
    let pinned = tests::selected(&control, limits);
    let history = AcceptedHistory::from_selected_spool(&control, &pinned, 1350, limits)
        .expect("control history");
    let authority = SelectedAuthority::new(
        history,
        control.clone(),
        |_: &wire::ImportPublicProofBundleV1, _: i64, _: &TrustTransaction<'_>| Ok(()),
    );
    delegated_import::authenticate_import_carriers(
        &control,
        &authority,
        &pin,
        1_350_000,
        &[],
        &[],
        |_| Ok(()),
    )
    .expect("both publications live under one delegation");
}
