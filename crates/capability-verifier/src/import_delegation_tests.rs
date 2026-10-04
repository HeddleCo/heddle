use heddle_api::{
    heddle::api::common as host, hybrid_codec, import_authority as contract, witness_trust,
};
use prost::Message;
use serde_json::Value;

use super::*;
use crate::wire::*;

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../conformance/hybrid/import-authority-host-witness-v1.json"
    ))
    .expect("published fixture")
}
fn record<T: Message + Default>(f: &Value, name: &str) -> T {
    let v = f["signed_vectors"]
        .get(name)
        .or_else(|| f["wire_vectors"].get(name))
        .expect("vector");
    hybrid_codec::strict_decode(
        &hex::decode(v["wire_hex"].as_str().expect("wire hex")).expect("bytes"),
        contract::MAX_BUNDLE_BYTES,
    )
    .expect("fixed canonical wire")
}
fn key(f: &Value, name: &str) -> Vec<u8> {
    hex::decode(f["keys"][name]["public_key_hex"].as_str().expect("key")).expect("key bytes")
}
fn selected(f: &Value) -> (VerifiedOwnerState, VerifiedCloneKeyring, [u8; 32]) {
    let history: OwnerHistory = record(f, "owner_history");
    let owner = crate::verify_owner_root(history.root.as_ref().expect("root")).expect("owner");
    let genesis: SignedSpoolOwnerGenesis = record(f, "spool_owner_genesis");
    let digest = crate::creation::spool_genesis_digest(genesis.genesis.as_ref().expect("genesis"))
        .expect("Spool digest");
    let keyring = crate::verify_clone_keyring(
        CloneAuthorizationKeyring {
            format_version: 1,
            spool_uuid: genesis
                .genesis
                .as_ref()
                .expect("genesis")
                .spool_uuid
                .clone(),
            canonical_spool_path_segments: vec!["heddleco".into(), "example".into()],
            owner_genesis: Some(genesis),
            owner_root: history.root,
            accepted_transitions: history.accepted_transitions,
            accepted_state_hash: owner.state_hash().to_vec(),
            pin: Some(CloneOwnerPin {
                kind: 2,
                expected_owner_id: owner.owner_id().to_vec(),
                first_seen_unix_seconds: 1000,
            }),
            ..Default::default()
        },
        1100,
        VerificationLimits::new(3600).expect("limits"),
        &[],
    )
    .expect("verified lineage");
    (owner, keyring, digest)
}
fn context<'a>(
    owner: &'a VerifiedOwnerState,
    keyring: &'a VerifiedCloneKeyring,
    digest: &'a [u8; 32],
    initial: &'a [u8; 32],
    forbidden: &'a [Vec<u8>],
    now: i64,
) -> CurrentContext<'a> {
    CurrentContext {
        selection: Selection {
            spool_genesis_digest: digest,
            initial_owner_id: initial,
            owner,
            keyring,
            limits: VerificationLimits::new(3600).expect("limits"),
        },
        now_millis: now * 1000,
        forbidden_job_keys: forbidden,
        known_job_associations: &[],
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn portable_import_permission_scope_and_current_expiry() {
    let f = fixture();
    let (owner, keyring, digest) = selected(&f);
    let initial = owner.owner_id();
    let d: SignedImportJobDelegationV1 = record(&f, "delegation");
    let p = record(&f, "permission");
    let o = record(&f, "operation_main");
    let forbidden = vec![key(&f, "root"), key(&f, "witness"), key(&f, "next_witness")];
    let c = context(&owner, &keyring, &digest, &initial, &forbidden, 1100);
    let verified = verify_current(&d, Some(&p), &c, |_| false).expect("owner-device-job control");
    verify_new_operation(&o, &verified, Some(&p), &c, |_| false).expect("in-window control");
    let bad = record(&f, "scope_violation");
    assert_eq!(
        verify_new_operation(&bad, &verified, Some(&p), &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Scope))
    );
    assert!(matches!(
        verify_current(&d, None, &c, |_| false),
        Err(Error::Hybrid(contract::Reject::ImportPermission))
    ));
    for now in [999, 1300, 1400] {
        let c = context(&owner, &keyring, &digest, &initial, &forbidden, now);
        assert!(matches!(
            verify_current(&d, Some(&p), &c, |_| false),
            Err(Error::Hybrid(contract::Reject::Expired))
        ));
    }
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn commit_admission_future_within_skew_preserves_execution_start() {
    let f = fixture();
    let (owner, keyring, digest) = selected(&f);
    let initial = owner.owner_id();
    let signed: SignedImportJobDelegationV1 = record(&f, "commit_future_within_skew");
    let parent = record(&f, "permission");
    let prepared = record(&f, "commit_preparation");
    let geneses = [record(&f, "genesis_dev"), record(&f, "genesis_main")];
    let operation = record(&f, "commit_future_operation");
    let forbidden = vec![key(&f, "root"), key(&f, "witness"), key(&f, "next_witness")];
    let mut c = context(&owner, &keyring, &digest, &initial, &forbidden, 1100);
    let result =
        verify_commit_admission(&prepared, &signed, Some(&parent), &geneses, &c, |_| false);
    println!(
        "Commit admission at T=1100, N=1200: {:?}",
        result.as_ref().map(|_| ())
    );
    let admitted = result.expect("scheduled Commit at the skew edge is eligible for admission");
    assert_eq!(c.now_millis, 1100000, "admission never advances host time");
    assert_eq!(
        admitted.signed(),
        &signed,
        "retain the exact signed certificate"
    );
    c.now_millis = 1199000;
    assert!(matches!(
        verify_current(&signed, Some(&parent), &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Expired))
    ));
    assert_eq!(
        verify_new_operation(&operation, &admitted, Some(&parent), &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Expired))
    );
    c.now_millis = 1200000;
    verify_current(&signed, Some(&parent), &c, |_| false).expect("execution starts at N");
    verify_new_operation(&operation, &admitted, Some(&parent), &c, |_| false)
        .expect("frozen job-signed operation at N");
    c.now_millis = signed.body.as_ref().expect("body").expires_at_unix_seconds * 1000;
    assert!(matches!(
        verify_current(&signed, Some(&parent), &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Expired))
    ));
    println!("Execution: N-1 Expired; N PASS; E Expired");
}

fn admission_preparation(
    f: &Value,
    signed: &SignedImportJobDelegationV1,
) -> PrepareImportJobResponse {
    let mut prepared: PrepareImportJobResponse = record(f, "commit_preparation");
    prepared.proposal = Some(contract::delegation_preparation(
        signed.body.as_ref().expect("body"),
    ));
    prepared
}

// Re-sign the portable bindings for a changed authority. Native originals and
// creator envelopes remain separate host gates, just as in the API vectors.
fn admission_bindings(
    f: &Value,
    signed: &mut SignedImportJobDelegationV1,
    role: &str,
) -> Vec<SignedImportGenesisAuthorityV1> {
    let d = signed.body.as_mut().expect("delegation");
    let mut geneses: Vec<SignedImportGenesisAuthorityV1> =
        vec![record(f, "genesis_dev"), record(f, "genesis_main")];
    for (g, manifest) in geneses.iter_mut().zip(&mut d.branch_manifest) {
        let body = g.body.as_mut().expect("genesis binding");
        body.identity = d.identity.clone();
        body.creator_public_key = d.delegating_public_key.clone();
        body.parent_permission_digest = d.parent_permission_digest.clone();
        body.owner_chain_digest = d.owner_chain_digest.clone();
        g.creator_signature = Some(sign_changed(f, role, contract::GENESIS_DOMAIN, body));
        manifest.genesis_authority_digest =
            contract::signed_genesis_digest(g).expect("binding digest");
    }
    signed.delegating_signature = Some(sign_changed(f, role, contract::DELEGATION_DOMAIN, d));
    geneses
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn commit_admission_direct_owner_and_member_skew_edges() {
    let f = fixture();
    let (owner, keyring, digest) = selected(&f);
    let initial = owner.owner_id();
    let parent = record(&f, "permission");
    let c = context(&owner, &keyring, &digest, &initial, &[], 1100);
    for (name, role, member, geneses) in [
        (
            "direct_owner",
            "owner",
            None,
            vec![
                record(&f, "direct_genesis_0"),
                record(&f, "direct_genesis_1"),
            ],
        ),
        (
            "commit_future_within_skew",
            "device",
            Some(&parent),
            vec![record(&f, "genesis_dev"), record(&f, "genesis_main")],
        ),
    ] {
        for (start, accepted) in [(1130, true), (1131, false)] {
            let mut signed: SignedImportJobDelegationV1 = record(&f, name);
            let body = signed.body.as_mut().expect("body");
            body.not_before_unix_seconds = start;
            signed.delegating_signature =
                Some(sign_changed(&f, role, contract::DELEGATION_DOMAIN, body));
            let mut prepared = admission_preparation(&f, &signed);
            prepared.clock_skew_allowance_seconds = 30;
            let result =
                verify_commit_admission(&prepared, &signed, member, &geneses, &c, |_| false);
            if accepted {
                result.expect("N = T+S is admissible for owner and member");
            } else {
                assert!(matches!(
                    result,
                    Err(Error::Hybrid(contract::Reject::ValidityBounds))
                ));
            }
        }
    }
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn commit_admission_requires_host_order_and_exclusive_expiry() {
    let f = fixture();
    let (owner, keyring, digest) = selected(&f);
    let initial = owner.owner_id();
    let signed = record(&f, "commit_future_within_skew");
    let parent = record(&f, "permission");
    let geneses = [record(&f, "genesis_dev"), record(&f, "genesis_main")];
    let mut prepared: PrepareImportJobResponse = record(&f, "commit_preparation");
    prepared.prepared_at_unix_seconds = 1101;
    prepared.reservation_expires_at_unix_seconds = 4701;
    let mut c = context(&owner, &keyring, &digest, &initial, &[], 1100);
    assert!(matches!(
        verify_commit_admission(&prepared, &signed, Some(&parent), &geneses, &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Expired))
    ));
    c.now_millis = 1101000;
    verify_commit_admission(&prepared, &signed, Some(&parent), &geneses, &c, |_| false)
        .expect("host reaches Prepare time");

    let expired = record(&f, "commit_window_expired");
    prepared = record(&f, "commit_preparation");
    c.now_millis = 1100000;
    assert!(matches!(
        verify_commit_admission(&prepared, &expired, Some(&parent), &geneses, &c, |_| false),
        Err(Error::Hybrid(contract::Reject::ValidityBounds))
    ));
    let control = record(&f, "delegation");
    verify_commit_admission(&prepared, &control, Some(&parent), &geneses, &c, |_| false)
        .expect("unexpired child control");

    let signed = record(&f, "commit_reservation");
    let parent = record(&f, "commit_reservation_parent");
    let geneses = [
        record(&f, "commit_reservation_genesis_dev"),
        record(&f, "commit_reservation_genesis_main"),
    ];
    c.now_millis = 4599000;
    verify_commit_admission(&prepared, &signed, Some(&parent), &geneses, &c, |_| false)
        .expect("last second of reservation");
    c.now_millis = 4600000;
    assert!(matches!(
        verify_commit_admission(&prepared, &signed, Some(&parent), &geneses, &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Expired))
    ));
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn commit_admission_requires_parent_at_actual_time_and_containment() {
    let f = fixture();
    let (owner, keyring, digest) = selected(&f);
    let initial = owner.owner_id();
    let mut c = context(&owner, &keyring, &digest, &initial, &[], 1100);
    for (start, end, reject) in [
        (1101, 1400, contract::Reject::Expired),
        (1201, 1400, contract::Reject::Expired),
        (1000, 1100, contract::Reject::Expired),
        (1000, 1299, contract::Reject::Scope),
    ] {
        let mut parent: SignedImportMemberPermissionV1 = record(&f, "permission");
        let body = parent.body.as_mut().expect("parent");
        body.not_before_unix_seconds = start;
        body.expires_at_unix_seconds = end;
        parent.owner_signature = Some(sign_changed(&f, "owner", contract::PERMISSION_DOMAIN, body));
        let mut signed: SignedImportJobDelegationV1 = record(&f, "commit_future_within_skew");
        signed
            .body
            .as_mut()
            .expect("child")
            .parent_permission_digest =
            contract::signed_permission_digest(&parent).expect("parent digest");
        let geneses = admission_bindings(&f, &mut signed, "device");
        let prepared = admission_preparation(&f, &signed);
        let result =
            verify_commit_admission(&prepared, &signed, Some(&parent), &geneses, &c, |_| false);
        assert!(
            matches!(result, Err(Error::Hybrid(actual)) if actual == reject),
            "parent window {start}..{end}"
        );
        if start == 1101 {
            c.now_millis = 1101000;
            verify_commit_admission(&prepared, &signed, Some(&parent), &geneses, &c, |_| false)
                .expect("same genuine parent becomes current at its not-before");
            c.now_millis = 1100000;
        }
    }
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn commit_admission_reuses_selected_lineage_roles_associations_and_revocations() {
    let f = fixture();
    let (owner, keyring, digest) = selected(&f);
    let initial = owner.owner_id();
    let signed: SignedImportJobDelegationV1 = record(&f, "commit_future_within_skew");
    let parent: SignedImportMemberPermissionV1 = record(&f, "permission");
    let prepared = record(&f, "commit_preparation");
    let geneses = [record(&f, "genesis_dev"), record(&f, "genesis_main")];
    let mut c = context(&owner, &keyring, &digest, &initial, &[], 1100);
    verify_commit_admission(&prepared, &signed, Some(&parent), &geneses, &c, |_| false)
        .expect("control");
    for id in [
        hybrid_codec::key_id(&key(&f, "owner")),
        hybrid_codec::key_id(&key(&f, "device")),
        hybrid_codec::key_id(&key(&f, "job")),
    ] {
        assert!(matches!(
            verify_commit_admission(
                &prepared,
                &signed,
                Some(&parent),
                &geneses,
                &c,
                |r| matches!(r, Revocation::Key(actual) if actual == id)
            ),
            Err(Error::Hybrid(contract::Reject::Revoked))
        ));
    }
    for id in [
        &signed.body.as_ref().expect("child").cancellation_id,
        &parent.body.as_ref().expect("parent").cancellation_id,
    ] {
        assert!(matches!(
            verify_commit_admission(
                &prepared,
                &signed,
                Some(&parent),
                &geneses,
                &c,
                |r| matches!(r, Revocation::Cancellation(namespace, actual)
                    if namespace == contract::CANCELLATION_NAMESPACE && actual == id)
            ),
            Err(Error::Hybrid(contract::Reject::Revoked))
        ));
    }
    for role in ["job", "device", "owner"] {
        let forbidden = [key(&f, role)];
        let role_context = context(&owner, &keyring, &digest, &initial, &forbidden, 1100);
        assert!(matches!(
            verify_commit_admission(
                &prepared,
                &signed,
                Some(&parent),
                &geneses,
                &role_context,
                |_| false
            ),
            Err(Error::Hybrid(contract::Reject::KeyRole))
        ));
    }
    let associations = [(key(&f, "job"), vec![0x88; 16])];
    c.known_job_associations = &associations;
    assert!(matches!(
        verify_commit_admission(&prepared, &signed, Some(&parent), &geneses, &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Scope))
    ));
    let wrong = [0x99; 32];
    c.known_job_associations = &[];
    c.selection.spool_genesis_digest = &wrong;
    assert!(matches!(
        verify_commit_admission(&prepared, &signed, Some(&parent), &geneses, &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Root))
    ));
    c.selection.spool_genesis_digest = &digest;
    c.selection.initial_owner_id = &wrong;
    assert!(matches!(
        verify_commit_admission(&prepared, &signed, Some(&parent), &geneses, &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Root))
    ));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn role_substitution_and_conflicting_job_associations() {
    let f = fixture();
    let (owner, keyring, digest) = selected(&f);
    let initial = owner.owner_id();
    let d = record(&f, "delegation");
    let p = record(&f, "permission");
    let normal = vec![key(&f, "witness"), key(&f, "root")];
    let c = context(&owner, &keyring, &digest, &initial, &normal, 1100);
    verify_current(&d, Some(&p), &c, |_| false).expect("separate roles control");
    for role in ["owner", "device", "witness", "root"] {
        let mut changed: SignedImportJobDelegationV1 = d.clone();
        let body = changed.body.as_mut().expect("body");
        body.job_public_key = key(&f, role);
        body.job_key_id = hybrid_codec::key_id(&body.job_public_key);
        changed.delegating_signature = Some(sign_changed(
            &f,
            "device",
            contract::DELEGATION_DOMAIN,
            body,
        ));
        assert!(
            matches!(
                verify_current(&changed, Some(&p), &c, |_| false),
                Err(Error::Hybrid(contract::Reject::KeyRole))
            ),
            "genuine device delegation cannot make {role} a job signer"
        );
    }
    let forbidden = vec![key(&f, "job")];
    let c = context(&owner, &keyring, &digest, &initial, &forbidden, 1100);
    assert!(matches!(
        verify_current(&d, Some(&p), &c, |_| false),
        Err(Error::Hybrid(contract::Reject::KeyRole))
    ));
    let associations = vec![(key(&f, "job"), vec![0x88; 16])];
    let mut c = context(&owner, &keyring, &digest, &initial, &normal, 1100);
    c.known_job_associations = &associations;
    assert!(matches!(
        verify_current(&d, Some(&p), &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Scope))
    ));
    let wrong = [0x99; 32];
    c.selection.spool_genesis_digest = &wrong;
    assert!(matches!(
        verify_current(&d, Some(&p), &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Root))
    ));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn cancellation_namespace_and_witnessed_history_are_separate() {
    let f = fixture();
    let (owner, keyring, digest) = selected(&f);
    let initial = owner.owner_id();
    let d: SignedImportJobDelegationV1 = record(&f, "delegation");
    let p = record(&f, "permission");
    let root = key(&f, "root");
    let jobs = vec![key(&f, "job"), key(&f, "renew_job")];
    let set = witness_trust::verify_set(
        &record(&f, "retired_set"),
        &witness_trust::SetExpectation {
            authority: "https://weft.example.test",
            root_id: "descriptor-root-1",
            root_public_key: &root,
            root_epoch: 1,
            now_unix_millis: 1350000,
            clock_floor_unix_millis: 1000000,
            known_job_keys: &jobs,
        },
        None,
    )
    .expect("root-authenticated seal");
    let s = record(&f, "publication_statement");
    let proof: host::HostedWitnessHistoryProofV1 = record(&f, "publication_proof");
    let resolved = witness_trust::resolve_statement(&set, &s, Some(&proof), false, 1350000)
        .expect("exact archived receipt");
    let forbidden = vec![root, key(&f, "witness"), key(&f, "next_witness")];
    let c = context(&owner, &keyring, &digest, &initial, &forbidden, 1350);
    assert!(matches!(
        verify_current(&d, Some(&p), &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Expired))
    ));
    verify_historical(&d, Some(&p), &c, &s, &resolved, &set, |_| false)
        .expect("committed history survives expiry");
    let c = context(&owner, &keyring, &digest, &initial, &forbidden, 1100);
    verify_current(&d, Some(&p), &c, |_| false).expect("current control");
    let cancellation = &d.body.as_ref().expect("body").cancellation_id;
    assert!(matches!(
        verify_current(
            &d,
            Some(&p),
            &c,
            |r| matches!(r,Revocation::Cancellation(namespace,id) if namespace==contract::CANCELLATION_NAMESPACE && id==cancellation)
        ),
        Err(Error::Hybrid(contract::Reject::Revoked))
    ));
}

fn sign_changed<T: hybrid_codec::Canonical>(
    f: &Value,
    role: &str,
    domain: &str,
    body: &T,
) -> AuthorizationSignature {
    sign_digest(
        f,
        role,
        &hybrid_codec::signing_digest(domain, body).expect("preimage"),
    )
}

fn sign_digest(f: &Value, role: &str, digest: &[u8]) -> AuthorizationSignature {
    use ed25519_dalek::{Signer, SigningKey};
    let seed: [u8; 32] = hex::decode(f["keys"][role]["seed_hex"].as_str().expect("seed"))
        .expect("seed bytes")
        .try_into()
        .expect("seed length");
    AuthorizationSignature {
        signer_key_id: hybrid_codec::key_id(&key(f, role)),
        signature: SigningKey::from_bytes(&seed)
            .sign(digest)
            .to_bytes()
            .to_vec(),
    }
}

const CLAIM_TIME: i64 = 1100;
const CLAIM_DEADLINE: i64 = 1200;

fn deferred_owners(f: &Value) -> (VerifiedOwnerState, VerifiedOwnerState, OwnerHistory) {
    use crate::canonical::{
        OWNER_ROOT_DOMAIN, OWNER_TRANSITION_DOMAIN, digest, owner_root_body, owner_root_without_id,
        transition_body,
    };
    let mut history: OwnerHistory = record(f, "owner_history");
    let signed = history.root.as_mut().expect("root");
    let root = signed.root.as_mut().expect("root body");
    root.claimable_deferred_human = true;
    root.claimable_until_unix_seconds = CLAIM_DEADLINE;
    root.owner_id = digest(
        OWNER_ROOT_DOMAIN,
        &owner_root_without_id(root).expect("root id"),
    )
    .to_vec();
    let signing = digest(
        OWNER_ROOT_DOMAIN,
        &owner_root_body(root).expect("root body"),
    );
    signed.authority_proof = Some(sign_digest(f, "owner", &signing));
    signed.recovery_key_proofs = ["guardian_a", "guardian_b"]
        .map(|role| sign_digest(f, role, &signing))
        .to_vec();
    signed
        .recovery_key_proofs
        .sort_by(|a, b| a.signer_key_id.cmp(&b.signer_key_id));
    let deferred = crate::verify_owner_root(signed).expect("authentic deferred root");
    let transition = OwnerKeyTransition {
        format_version: 1,
        owner_id: deferred.owner_id().to_vec(),
        previous_state_hash: deferred.state_hash().to_vec(),
        sequence: 1,
        kind: OwnerKeyTransitionKind::ClaimDeferredHuman as i32,
        next_authority_key: Some(AuthorizationVerificationKey {
            algorithm: AuthorizationKeyAlgorithm::Ed25519 as i32,
            public_key: key(f, "device"),
        }),
        next_recovery_policy: Some(deferred.recovery_policy().clone()),
        valid_from_unix_seconds: CLAIM_TIME,
        previous_key_valid_until_unix_seconds: 0,
        nonce: vec![0x61; 32],
    };
    let signing = digest(
        OWNER_TRANSITION_DOMAIN,
        &transition_body(&transition).expect("claim"),
    );
    let mut claim = SignedOwnerKeyTransition {
        transition: Some(transition),
        authorizations: vec![sign_digest(f, "owner", &signing)],
        next_authority_key_proof: Some(sign_digest(f, "device", &signing)),
        next_recovery_key_proofs: ["guardian_a", "guardian_b"]
            .map(|role| sign_digest(f, role, &signing))
            .to_vec(),
    };
    claim
        .next_recovery_key_proofs
        .sort_by(|a, b| a.signer_key_id.cmp(&b.signer_key_id));
    let claimed = crate::apply_transition(
        &deferred,
        &claim,
        CLAIM_TIME,
        VerificationLimits::new(3600).expect("limits"),
    )
    .expect("human claims within deadline");
    history.accepted_transitions = vec![claim];
    history.state_hash = claimed.state_hash().to_vec();
    (deferred, claimed, history)
}

fn deferred_keyring(f: &Value, owner: &VerifiedOwnerState) -> VerifiedCloneKeyring {
    let (_, ring, _) = selected(f);
    let mut wire = ring.wire().clone();
    wire.owner_root = Some(owner.signed_root().clone());
    wire.accepted_state_hash = owner.state_hash().to_vec();
    wire.pin.as_mut().expect("pin").expected_owner_id = owner.owner_id().to_vec();
    crate::verify_clone_keyring(
        wire,
        1050,
        VerificationLimits::new(3600).expect("limits"),
        &[],
    )
    .expect("deferred lineage")
}

fn direct_delegation(
    f: &Value,
    c: &CurrentContext<'_>,
    role: &str,
    not_before: i64,
    expires_at: i64,
) -> SignedImportJobDelegationV1 {
    let mut signed: SignedImportJobDelegationV1 = record(f, "delegation");
    let body = signed.body.as_mut().expect("delegation body");
    let (identity, chain, _) = expectation(&c.selection, c.now_millis / 1000).expect("selection");
    body.identity = Some(identity);
    body.owner_chain_digest = chain;
    body.delegating_public_key = key(f, role);
    body.parent_permission_digest = vec![0; 32];
    body.not_before_unix_seconds = not_before;
    body.expires_at_unix_seconds = expires_at;
    signed.delegating_signature = Some(sign_changed(f, role, contract::DELEGATION_DOMAIN, body));
    signed
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn commit_admission_uses_effective_owner_expiry() {
    let f = fixture();
    let (deferred, claimed, _) = deferred_owners(&f);
    let ring = deferred_keyring(&f, &deferred);
    let (_, _, digest) = selected(&f);
    let initial = deferred.owner_id();
    for (owner, role, now, expiry, rejected) in [
        (&deferred, "owner", 1050, CLAIM_DEADLINE, false),
        (&deferred, "owner", 1050, CLAIM_DEADLINE + 1, true),
        (&claimed, "device", 1250, 1290, false),
    ] {
        let c = context(owner, &ring, &digest, &initial, &[], now);
        let mut signed = direct_delegation(&f, &c, role, now + 30, expiry);
        let geneses = admission_bindings(&f, &mut signed, role);
        let mut prepared = admission_preparation(&f, &signed);
        prepared.prepared_at_unix_seconds = now;
        prepared.reservation_expires_at_unix_seconds = now + 3600;
        prepared.clock_skew_allowance_seconds = 30;
        let result = verify_commit_admission(&prepared, &signed, None, &geneses, &c, |_| false);
        if rejected {
            assert!(matches!(
                result,
                Err(Error::Hybrid(contract::Reject::Scope))
            ));
        } else {
            result.expect("admission uses the expiry of the effective selected owner state");
        }
    }
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn commit_admission_future_owner_transition_cannot_authorize_at_t() {
    let f = fixture();
    let (deferred, claimed, _) = deferred_owners(&f);
    let ring = deferred_keyring(&f, &deferred);
    let (_, _, digest) = selected(&f);
    let initial = deferred.owner_id();
    let mut c = context(&claimed, &ring, &digest, &initial, &[], CLAIM_TIME);
    let mut signed = direct_delegation(&f, &c, "device", CLAIM_TIME + 29, CLAIM_DEADLINE + 50);
    let geneses = admission_bindings(&f, &mut signed, "device");
    let mut prepared = admission_preparation(&f, &signed);
    prepared.clock_skew_allowance_seconds = 30;
    c.now_millis = (CLAIM_TIME - 1) * 1000;
    assert!(matches!(
        verify_commit_admission(&prepared, &signed, None, &geneses, &c, |_| false),
        Err(Error::NotYetValid)
    ));
    c.selection.owner = &deferred;
    assert!(matches!(
        verify_commit_admission(&prepared, &signed, None, &geneses, &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Root))
    ));
    c.selection.owner = &claimed;
    c.now_millis = CLAIM_TIME * 1000;
    verify_commit_admission(&prepared, &signed, None, &geneses, &c, |_| false)
        .expect("the same claim authorizes admission only after its actual activation");
}

#[test]
#[cfg(not(target_arch = "wasm32"))]
fn commit_admission_preserves_preparation_signatures_and_genesis_bindings() {
    let f = fixture();
    let (owner, keyring, digest) = selected(&f);
    let initial = owner.owner_id();
    let c = context(&owner, &keyring, &digest, &initial, &[], 1100);
    let prepared = record(&f, "commit_preparation");
    let parent = record(&f, "permission");
    let geneses = [record(&f, "genesis_dev"), record(&f, "genesis_main")];
    let control = record(&f, "delegation");
    verify_commit_admission(&prepared, &control, Some(&parent), &geneses, &c, |_| false)
        .expect("unmodified frozen Commit control");
    for (name, reject) in [
        (
            "commit_frozen_logicalJobId",
            contract::Reject::PreparedFields,
        ),
        (
            "commit_frozen_manifest_reordered",
            contract::Reject::PreparedFields,
        ),
        ("commit_bad_signature", contract::Reject::Signature),
        ("commit_genesis_digest", contract::Reject::GenesisBinding),
        ("commit_genesis_branch", contract::Reject::GenesisBinding),
    ] {
        let signed = record(&f, name);
        let result =
            verify_commit_admission(&prepared, &signed, Some(&parent), &geneses, &c, |_| false);
        assert!(
            matches!(result, Err(Error::Hybrid(actual)) if actual == reject),
            "{name}"
        );
    }
    assert!(matches!(
        verify_commit_admission(
            &prepared,
            &control,
            Some(&parent),
            &geneses[..1],
            &c,
            |_| false
        ),
        Err(Error::Hybrid(contract::Reject::GenesisBinding))
    ));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn claimed_human_import_authority_survives_original_claim_deadline() {
    let f = fixture();
    let (deferred, claimed, _) = deferred_owners(&f);
    let ring = deferred_keyring(&f, &deferred);
    let (_, _, digest) = selected(&f);
    let initial = deferred.owner_id();
    let c = context(&claimed, &ring, &digest, &initial, &[], CLAIM_DEADLINE + 50);
    ring.verify_current_owner(&claimed, CLAIM_DEADLINE + 50, c.selection.limits)
        .expect("claimed current owner remains valid");
    let signed = direct_delegation(&f, &c, "device", CLAIM_DEADLINE + 49, CLAIM_DEADLINE + 90);
    let result = verify_current(&signed, None, &c, |_| false);
    println!(
        "claimed human import after original deadline: {:?}",
        result.as_ref().map(|_| ())
    );
    result.expect("accepted human authority survives the original deferred deadline");
    assert_eq!(claimed.authority_expires_at_seconds(), i64::MAX);
    assert!(matches!(
        verify_current(
            &signed,
            None,
            &c,
            |r| matches!(r, Revocation::Key(id) if id == hybrid_codec::key_id(&key(&f, "device")))
        ),
        Err(Error::Hybrid(contract::Reject::Revoked))
    ));
    assert_eq!(
        claimed.issuer_at(&deferred.state_hash(), CLAIM_DEADLINE + 50),
        Err(Error::Expired)
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn unclaimed_deferred_import_authority_keeps_its_deadline() {
    let f = fixture();
    let (deferred, _, _) = deferred_owners(&f);
    let ring = deferred_keyring(&f, &deferred);
    let (_, _, digest) = selected(&f);
    let initial = deferred.owner_id();
    let mut c = context(&deferred, &ring, &digest, &initial, &[], 1050);
    assert_eq!(deferred.authority_expires_at_seconds(), CLAIM_DEADLINE);
    let signed = direct_delegation(&f, &c, "owner", 1000, CLAIM_DEADLINE);
    verify_current(&signed, None, &c, |_| false).expect("unclaimed in-window control");
    let too_long = direct_delegation(&f, &c, "owner", 1000, CLAIM_DEADLINE + 1);
    assert!(matches!(
        verify_current(&too_long, None, &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Scope))
    ));
    c.now_millis = CLAIM_DEADLINE * 1000;
    assert!(matches!(
        verify_current(&signed, None, &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Expired))
    ));
    c.now_millis += 1000;
    assert!(matches!(
        verify_current(&signed, None, &c, |_| false),
        Err(Error::Expired)
    ));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn claimed_import_authority_still_rejects_rotated_issuers() {
    use crate::canonical::{OWNER_TRANSITION_DOMAIN, digest, transition_body};
    let f = fixture();
    let (deferred, claimed, history) = deferred_owners(&f);
    let ring = deferred_keyring(&f, &deferred);
    let (_, _, genesis) = selected(&f);
    let initial = deferred.owner_id();
    let mut c = context(&claimed, &ring, &genesis, &initial, &[], 1250);
    let signed = direct_delegation(&f, &c, "device", 1249, 1290);
    verify_current(&signed, None, &c, |_| false).expect("claimed authority control");
    let mut rotation = history.accepted_transitions[0]
        .transition
        .clone()
        .expect("claim body");
    rotation.kind = OwnerKeyTransitionKind::Rotate as i32;
    rotation.sequence = 2;
    rotation.previous_state_hash = claimed.state_hash().to_vec();
    rotation.valid_from_unix_seconds = 1251;
    rotation
        .next_authority_key
        .as_mut()
        .expect("key")
        .public_key = key(&f, "rotated_owner");
    let signing = digest(
        OWNER_TRANSITION_DOMAIN,
        &transition_body(&rotation).expect("rotation"),
    );
    let rotated = crate::apply_transition(
        &claimed,
        &SignedOwnerKeyTransition {
            transition: Some(rotation),
            authorizations: vec![sign_digest(&f, "device", &signing)],
            next_authority_key_proof: Some(sign_digest(&f, "rotated_owner", &signing)),
            next_recovery_key_proofs: vec![],
        },
        1251,
        c.selection.limits,
    )
    .expect("accepted rotation");
    c.selection.owner = &rotated;
    c.now_millis = 1252000;
    assert!(matches!(
        verify_current(&signed, None, &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Root))
    ));
    let retired = direct_delegation(&f, &c, "device", 1251, 1290);
    assert!(matches!(
        verify_current(&retired, None, &c, |_| false),
        Err(Error::Hybrid(contract::Reject::ImportPermission))
    ));
    assert_eq!(
        rotated.issuer_at(&claimed.state_hash(), 1252),
        Err(Error::Expired)
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn historical_import_expiry_uses_owner_state_at_observation() {
    let f = fixture();
    let (deferred, claimed, _) = deferred_owners(&f);
    let ring = deferred_keyring(&f, &deferred);
    let (_, _, digest) = selected(&f);
    let initial = deferred.owner_id();
    let root = key(&f, "root");
    let set = witness_trust::verify_set(
        &record(&f, "current_set"),
        &witness_trust::SetExpectation {
            authority: "https://weft.example.test",
            root_id: "descriptor-root-1",
            root_public_key: &root,
            root_epoch: 1,
            now_unix_millis: 1260000,
            clock_floor_unix_millis: 1000000,
            known_job_keys: &[key(&f, "job")],
        },
        None,
    )
    .expect("authenticated current witness set");
    for (owner, role, observation, expiry, expected) in [
        (&deferred, "owner", 1050, CLAIM_DEADLINE, None),
        (
            &deferred,
            "owner",
            1050,
            CLAIM_DEADLINE + 1,
            Some(contract::Reject::Scope),
        ),
        (&claimed, "device", 1250, 1290, None),
        (&claimed, "device", 1150, 1290, None),
    ] {
        let mut c = context(owner, &ring, &digest, &initial, &[], observation);
        let signed = direct_delegation(&f, &c, role, observation - 1, expiry);
        let (identity, _, bound) =
            expectation(&c.selection, observation).expect("historical selection");
        assert_eq!(
            bound,
            if observation < CLAIM_TIME {
                CLAIM_DEADLINE
            } else {
                i64::MAX
            }
        );
        let mut statement: host::SignedHostedWitnessStatementV1 =
            record(&f, "publication_statement");
        let body = statement.body.as_mut().expect("statement body");
        body.spool_uuid = identity.spool_uuid;
        body.spool_genesis_digest = identity.spool_genesis_digest;
        body.owner_id = identity.owner_id;
        body.owner_state_hash = identity.owner_state_hash;
        body.ownership_transfer_sequence = identity.ownership_transfer_sequence;
        body.authority_digest =
            contract::signed_delegation_digest(&signed).expect("delegation digest");
        body.observed_at_unix_millis = observation * 1000;
        statement.signature = sign_digest(
            &f,
            "witness",
            &witness_trust::statement_signing_digest(body).expect("statement digest"),
        )
        .signature;
        let resolved = witness_trust::resolve_statement(&set, &statement, None, false, 1260000)
            .expect("authenticated historical observation");
        c.now_millis = 1260000;
        let result = verify_historical(&signed, None, &c, &statement, &resolved, &set, |_| false);
        match expected {
            Some(reject) => {
                assert!(matches!(result, Err(Error::Hybrid(actual)) if actual == reject))
            }
            None => {
                result.expect("historical authority at observation");
            }
        }
        if observation < CLAIM_TIME {
            c.selection.owner = &claimed;
            assert!(matches!(
                verify_historical(&signed, None, &c, &statement, &resolved, &set, |_| false),
                Err(Error::Hybrid(contract::Reject::Root))
            ));
        }
    }
}

#[test]
#[ignore = "regenerate the signed claimed-owner differential fixture"]
#[cfg(not(target_arch = "wasm32"))]
fn print_claimed_owner_fixture_json() {
    let f = fixture();
    let (deferred, claimed, history) = deferred_owners(&f);
    let ring = deferred_keyring(&f, &deferred);
    let (_, _, digest) = selected(&f);
    let initial = deferred.owner_id();
    let mut cases = Vec::new();
    for (id, owner, role, now, expiry, error) in [
        (
            "claimed-human-after-deadline",
            &claimed,
            "device",
            1250,
            1290,
            None,
        ),
        (
            "unclaimed-before-deadline",
            &deferred,
            "owner",
            1050,
            1200,
            None,
        ),
        (
            "unclaimed-exceeds-deadline",
            &deferred,
            "owner",
            1050,
            1201,
            Some("delegation scope violation"),
        ),
        (
            "unclaimed-after-deadline",
            &deferred,
            "owner",
            1201,
            1200,
            Some("owner-authorization object is expired"),
        ),
    ] {
        let c = context(owner, &ring, &digest, &initial, &[], now.min(1199));
        let signed = direct_delegation(&f, &c, role, 1000, expiry);
        let mut selected_history = history.clone();
        if role == "owner" {
            selected_history.accepted_transitions.clear();
            selected_history.state_hash = deferred.state_hash().to_vec();
        }
        cases.push(serde_json::json!({
            "id": id,
            "certificate_hex": hex::encode(signed.encode_to_vec()),
            "owner_history_hex": hex::encode(selected_history.encode_to_vec()),
            "now": now.to_string(),
            "expected_error": error,
        }));
    }
    println!(
        "CLAIMED_OWNER_FIXTURE={}",
        serde_json::json!({
            "keyring_hex": hex::encode(ring.wire().encode_to_vec()),
            "initial_owner_hex": hex::encode(initial),
            "spool_genesis_hex": hex::encode(digest),
            "cases": cases,
        })
    );
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn genuine_job_signatures_cannot_widen_any_operation_binding() {
    let f = fixture();
    let (owner, ring, digest) = selected(&f);
    let initial = owner.owner_id();
    let forbidden = vec![key(&f, "root"), key(&f, "witness")];
    let c = context(&owner, &ring, &digest, &initial, &forbidden, 1100);
    let d = record(&f, "delegation");
    let p = record(&f, "permission");
    let verified = verify_current(&d, Some(&p), &c, |_| false).expect("current control");
    let original: SignedDelegatedImportOperationV1 = record(&f, "operation_main");
    verify_new_operation(&original, &verified, Some(&p), &c, |_| false)
        .expect("unmodified genuine control");
    for field in 0..15 {
        let mut changed = original.clone();
        let b = changed.body.as_mut().expect("body");
        match field {
            0 => b.spool_uuid[0] ^= 1,
            1 => b.spool_genesis_digest[0] ^= 1,
            2 => b.logical_job_id[0] ^= 1,
            3 => b.retry_lineage_id[0] ^= 1,
            4 => b.delegation_digest[0] ^= 1,
            5 => b.ref_name = "refs/heads/unauthorized".into(),
            6 => b.slot_id += 1,
            7 => {
                b.hash_algorithm = 2;
                b.observed_commit_oid = vec![1; 32];
            }
            8 => b.observed_commit_oid[0] ^= 1,
            9 => b.genesis_digest[0] ^= 1,
            10 => b.target_thread_id[0] ^= 1,
            11 => b.expected_frontier_digest[0] ^= 1,
            12 => b.options_digest[0] ^= 1,
            13 => b.converter_version = "git-converter/2.0".into(),
            _ => b.result_bytes = u64::MAX,
        };
        changed.job_signature = Some(sign_changed(&f, "job", contract::OPERATION_DOMAIN, b));
        assert_eq!(
            verify_new_operation(&changed, &verified, Some(&p), &c, |_| false),
            Err(Error::Hybrid(contract::Reject::Scope)),
            "operation binding {field}"
        );
    }
}
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn permission_attenuation_and_staged_current_revocations_are_rechecked() {
    let f = fixture();
    let (owner, ring, digest) = selected(&f);
    let initial = owner.owner_id();
    let forbidden = vec![key(&f, "root"), key(&f, "witness")];
    let c = context(&owner, &ring, &digest, &initial, &forbidden, 1100);
    let d: SignedImportJobDelegationV1 = record(&f, "delegation");
    let p: SignedImportMemberPermissionV1 = record(&f, "permission");
    let op = record(&f, "operation_main");
    let staged = verify_current(&d, Some(&p), &c, |_| false).expect("staged in-window authority");
    for role in ["owner", "device", "job"] {
        let id = hybrid_codec::key_id(&key(&f, role));
        assert_eq!(
            verify_new_operation(
                &op,
                &staged,
                Some(&p),
                &c,
                |r| matches!(r,Revocation::Key(k) if k==id)
            ),
            Err(Error::Hybrid(contract::Reject::Revoked)),
            "new {role} revocation"
        );
    }
    let expired = context(&owner, &ring, &digest, &initial, &forbidden, 1300);
    assert_eq!(
        verify_new_operation(&op, &staged, Some(&p), &expired, |_| false),
        Err(Error::Hybrid(contract::Reject::Expired))
    );
    for field in 0..9 {
        let mut bad = d.clone();
        let b = bad.body.as_mut().expect("body");
        match field {
            0 => b.expires_at_unix_seconds += 1,
            1 => b.not_before_unix_seconds -= 1,
            2 => b.scope.as_mut().expect("scope").max_operations += 1,
            3 => {
                b.scope.as_mut().expect("scope").source_url = "https://github.com/other/repo".into()
            }
            4 => b.scope.as_mut().expect("scope").provider = "other-provider".into(),
            5 => b.scope.as_mut().expect("scope").destination_version[0] ^= 1,
            6 => b.scope.as_mut().expect("scope").max_result_bytes += 1,
            7 => {
                let scope = b.scope.as_mut().expect("scope");
                scope.branches[0].ref_mode = 2;
                scope.branches[0].ref_disclosure = 1;
                scope.branches[0].pinned_commit_oid.clear();
                b.branch_manifest[0].limit = Some(scope.branches[0].clone());
            }
            _ => b.purpose = 2,
        };
        bad.delegating_signature = Some(sign_changed(&f, "device", contract::DELEGATION_DOMAIN, b));
        let expected = match field {
            8 => contract::Reject::Version,
            _ => contract::Reject::Scope,
        };
        let rejection = verify_current(&bad, Some(&p), &c, |_| false).err();
        assert!(
            matches!(rejection, Some(Error::Hybrid(reason)) if reason == expected),
            "parent attenuation {field}: {rejection:?}"
        );
    }
    let mut nondelegable = p.clone();
    let b = nondelegable.body.as_mut().expect("body");
    b.purpose = 2;
    nondelegable.owner_signature = Some(sign_changed(&f, "owner", contract::PERMISSION_DOMAIN, b));
    assert!(matches!(
        verify_current(&d, Some(&nondelegable), &c, |_| false),
        Err(Error::Hybrid(contract::Reject::ImportPermission))
    ));
    // A distinct child key avoids the same-key gate; the authentic parent
    // authorizes the device, not this job acting as a delegation issuer.
    let mut child = d.clone();
    let b = child.body.as_mut().expect("body");
    b.delegating_public_key = key(&f, "job");
    b.job_public_key = key(&f, "renew_job");
    b.job_key_id = hybrid_codec::key_id(&b.job_public_key);
    child.delegating_signature = Some(sign_changed(&f, "job", contract::DELEGATION_DOMAIN, b));
    assert!(matches!(
        verify_current(&child, Some(&p), &c, |_| false),
        Err(Error::Hybrid(contract::Reject::Scope))
    ));
    verify_new_operation(&op, &staged, Some(&p), &c, |_| false)
        .expect("unchanged accepted context control");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn review_wrong_role_delegators_have_genuine_owner_permissions() {
    let f = fixture();
    let (owner, ring, digest) = selected(&f);
    let initial = owner.owner_id();
    let forbidden = vec![key(&f, "root"), key(&f, "witness"), key(&f, "next_witness")];
    let mut c = context(&owner, &ring, &digest, &initial, &forbidden, 1100);
    let d: SignedImportJobDelegationV1 = record(&f, "delegation");
    let p: SignedImportMemberPermissionV1 = record(&f, "permission");
    verify_current(&d, Some(&p), &c, |_| false).expect("ordinary device control");
    let associations = vec![(
        key(&f, "job"),
        d.body.as_ref().expect("body").logical_job_id.clone(),
    )];
    c.known_job_associations = &associations;
    for role in ["witness", "job"] {
        let mut parent = p.clone();
        let b = parent.body.as_mut().expect("body");
        b.subject_public_key = key(&f, role);
        parent.owner_signature = Some(sign_changed(&f, "owner", contract::PERMISSION_DOMAIN, b));
        let mut child = d.clone();
        let b = child.body.as_mut().expect("body");
        b.delegating_public_key = key(&f, role);
        b.job_public_key = key(&f, "renew_job");
        b.job_key_id = hybrid_codec::key_id(&b.job_public_key);
        b.parent_permission_digest =
            contract::signed_permission_digest(&parent).expect("parent digest");
        child.delegating_signature = Some(sign_changed(&f, role, contract::DELEGATION_DOMAIN, b));
        assert!(
            matches!(
                verify_current(&child, Some(&parent), &c, |_| false),
                Err(Error::Hybrid(contract::Reject::KeyRole))
            ),
            "genuine owner-signed {role} delegator must reject"
        );
    }
    verify_current(&d, Some(&p), &c, |_| false).expect("unchanged device control");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn review_witness_freshness_preserves_receiver_milliseconds() {
    let f = fixture();
    let (owner, ring, digest) = selected(&f);
    let initial = owner.owner_id();
    let forbidden = vec![key(&f, "root"), key(&f, "witness")];
    let mut c = context(&owner, &ring, &digest, &initial, &forbidden, 1350);
    c.now_millis = 1350002;
    let mut signed: host::SignedHostedWitnessSetV1 = record(&f, "retired_set");
    signed.body.as_mut().expect("body").issued_at_unix_millis = 1350001;
    let preimage =
        witness_trust::set_signing_bytes(signed.body.as_ref().expect("body")).expect("bytes");
    signed.body_digest = hybrid_codec::hash(&[&preimage]);
    let seed: [u8; 32] = hex::decode(f["keys"]["root"]["seed_hex"].as_str().expect("seed"))
        .expect("bytes")
        .try_into()
        .expect("seed width");
    use ed25519_dalek::Signer;
    signed.root_signature = ed25519_dalek::SigningKey::from_bytes(&seed)
        .sign(&preimage)
        .to_bytes()
        .to_vec();
    let root = key(&f, "root");
    let set = witness_trust::verify_set(
        &signed,
        &witness_trust::SetExpectation {
            authority: "https://weft.example.test",
            root_id: "descriptor-root-1",
            root_public_key: &root,
            root_epoch: 1,
            now_unix_millis: c.now_millis,
            clock_floor_unix_millis: 0,
            known_job_keys: &[],
        },
        None,
    )
    .expect("fractional issued time control");
    let statement = record(&f, "publication_statement");
    let proof = record(&f, "publication_proof");
    let resolved =
        witness_trust::resolve_statement(&set, &statement, Some(&proof), false, c.now_millis)
            .expect("resolution control");
    verify_historical(
        &record(&f, "delegation"),
        Some(&record(&f, "permission")),
        &c,
        &statement,
        &resolved,
        &set,
        |_| false,
    )
    .expect("freshness must retain milliseconds");
    let statement = record(&f, "genesis_admission");
    let proof = record(&f, "genesis_proof");
    let resolved =
        witness_trust::resolve_statement(&set, &statement, Some(&proof), false, c.now_millis)
            .expect("genesis resolution control");
    verify_historical_genesis(
        &record(&f, "delegation"),
        Some(&record(&f, "permission")),
        &c,
        &record(&f, "genesis_payload"),
        &statement,
        &resolved,
        &set,
        |_| false,
    )
    .expect("genesis freshness retains milliseconds");
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn review_historical_genesis_and_publication_exclude_selected_witness_as_creator() {
    use ed25519_dalek::{Signer, SigningKey};
    let f = fixture();
    let (owner, ring, digest) = selected(&f);
    let initial = owner.owner_id();
    let device = key(&f, "device");
    let root = key(&f, "root");
    let seed: [u8; 32] = hex::decode(f["keys"]["device"]["seed_hex"].as_str().expect("seed"))
        .expect("seed bytes")
        .try_into()
        .expect("seed width");
    let mut signed: host::SignedHostedWitnessSetV1 = record(&f, "current_set");
    let set_body = signed.body.as_mut().expect("set");
    let entry = set_body
        .entries
        .iter_mut()
        .find(|e| e.executor_id == set_body.current_executor_id)
        .expect("current");
    entry.public_key = device.clone();
    entry.executor_id = witness_trust::witness_id(&device);
    set_body.current_executor_id = entry.executor_id.clone();
    set_body
        .entries
        .sort_by(|a, b| a.executor_id.cmp(&b.executor_id));
    let root_seed: [u8; 32] = hex::decode(f["keys"]["root"]["seed_hex"].as_str().expect("seed"))
        .expect("bytes")
        .try_into()
        .expect("width");
    signed.body_digest =
        hybrid_codec::hash(&[&witness_trust::set_signing_bytes(set_body).expect("set preimage")]);
    signed.root_signature = SigningKey::from_bytes(&root_seed)
        .sign(&witness_trust::set_signing_bytes(set_body).expect("set preimage"))
        .to_bytes()
        .to_vec();
    let set = witness_trust::verify_set(
        &signed,
        &witness_trust::SetExpectation {
            authority: "https://weft.example.test",
            root_id: "descriptor-root-1",
            root_public_key: &root,
            root_epoch: 1,
            now_unix_millis: 1100000,
            clock_floor_unix_millis: 0,
            known_job_keys: &[],
        },
        None,
    )
    .expect("genuine root-selected witness");
    let d = record(&f, "delegation");
    let p = record(&f, "permission");
    let allowed = vec![root.clone(), key(&f, "witness"), key(&f, "next_witness")];
    let control_set = witness_trust::verify_set(
        &record(&f, "current_set"),
        &witness_trust::SetExpectation {
            authority: "https://weft.example.test",
            root_id: "descriptor-root-1",
            root_public_key: &root,
            root_epoch: 1,
            now_unix_millis: 1100000,
            clock_floor_unix_millis: 0,
            known_job_keys: &[],
        },
        None,
    )
    .expect("ordinary device control set");
    let allowed_context = context(&owner, &ring, &digest, &initial, &allowed, 1100);
    let forbidden = vec![root, device.clone()];
    let forbidden_context = context(&owner, &ring, &digest, &initial, &forbidden, 1100);
    for genesis in [false, true] {
        let mut statement: host::SignedHostedWitnessStatementV1 = record(
            &f,
            if genesis {
                "genesis_admission"
            } else {
                "publication_statement"
            },
        );
        let control_resolved =
            witness_trust::resolve_statement(&control_set, &statement, None, false, 1100000)
                .expect("ordinary testimony");
        if genesis {
            verify_historical_genesis(
                &d,
                Some(&p),
                &allowed_context,
                &record(&f, "genesis_payload"),
                &statement,
                &control_resolved,
                &control_set,
                |_| false,
            )
            .expect("ordinary creator control");
        } else {
            verify_historical(
                &d,
                Some(&p),
                &allowed_context,
                &statement,
                &control_resolved,
                &control_set,
                |_| false,
            )
            .expect("ordinary delegator control");
        }
        let body = statement.body.as_mut().expect("statement");
        body.executor_id = witness_trust::witness_id(&device);
        statement.signature = SigningKey::from_bytes(&seed)
            .sign(&witness_trust::statement_signing_digest(body).expect("preimage"))
            .to_bytes()
            .to_vec();
        let resolved = witness_trust::resolve_statement(&set, &statement, None, false, 1100000)
            .expect("genuine witness testimony");
        let verify = |c| {
            if genesis {
                verify_historical_genesis(
                    &d,
                    Some(&p),
                    c,
                    &record(&f, "genesis_payload"),
                    &statement,
                    &resolved,
                    &set,
                    |_| false,
                )
            } else {
                verify_historical(&d, Some(&p), c, &statement, &resolved, &set, |_| false)
            }
        };
        if genesis {
            contract::verify_witness_payload(
                statement.body.as_ref().expect("body"),
                contract::WitnessPayload::Genesis(&record(&f, "genesis_payload")),
            )
            .expect("unchanged valid surrounding native commitments");
        }

        assert!(
            matches!(
                verify(&forbidden_context),
                Err(Error::Hybrid(contract::Reject::KeyRole))
            ),
            "historical genesis={genesis}"
        );
    }
}
