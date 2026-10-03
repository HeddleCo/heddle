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
        now,
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
    let d = record(&f, "delegation");
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
    use ed25519_dalek::{Signer, SigningKey};
    let seed: [u8; 32] = hex::decode(f["keys"][role]["seed_hex"].as_str().expect("seed"))
        .expect("seed bytes")
        .try_into()
        .expect("seed length");
    AuthorizationSignature {
        signer_key_id: hybrid_codec::key_id(&key(f, role)),
        signature: SigningKey::from_bytes(&seed)
            .sign(&hybrid_codec::signing_digest(domain, body).expect("preimage"))
            .to_bytes()
            .to_vec(),
    }
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
    for field in 0..4 {
        let mut bad = d.clone();
        let b = bad.body.as_mut().expect("body");
        match field {
            0 => b.expires_at_unix_seconds += 1,
            1 => b.not_before_unix_seconds -= 1,
            2 => b.scope.as_mut().expect("scope").max_operations += 1,
            _ => {
                b.scope.as_mut().expect("scope").source_url = "https://github.com/other/repo".into()
            }
        };
        bad.delegating_signature = Some(sign_changed(&f, "device", contract::DELEGATION_DOMAIN, b));
        assert!(
            matches!(
                verify_current(&bad, Some(&p), &c, |_| false),
                Err(Error::Hybrid(contract::Reject::Scope))
            ),
            "parent attenuation {field}"
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
    // Self-PoP/subdelegation has valid signatures but no owner-authorized parent.
    let mut child = d.clone();
    let b = child.body.as_mut().expect("body");
    b.delegating_public_key = key(&f, "job");
    child.delegating_signature = Some(sign_changed(&f, "job", contract::DELEGATION_DOMAIN, b));
    assert!(matches!(
        verify_current(&child, Some(&p), &c, |_| false),
        Err(Error::Hybrid(contract::Reject::KeyRole)) | Err(Error::Hybrid(contract::Reject::Scope))
    ));
    verify_new_operation(&op, &staged, Some(&p), &c, |_| false)
        .expect("unchanged accepted context control");
}
