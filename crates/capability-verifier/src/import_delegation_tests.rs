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
        assert!(matches!(rejection, Some(Error::Hybrid(reason)) if reason == expected),
            "parent attenuation {field}: {rejection:?}");
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
