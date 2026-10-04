use serde_json::{Value, json};

use super::*;
use crate::{policy, wire::*};

fn observed_owner(
    root: &SignedOwnerRoot,
    transitions: Vec<SignedOwnerKeyTransition>,
    hash: &[u8],
) -> OwnerState {
    OwnerState {
        root: Some(root.clone()),
        accepted_transitions: transitions,
        version: hash.to_vec(),
        ..Default::default()
    }
}

fn policy_record(
    state: &VerifiedOwnerState,
    signer: &TestKey,
    head: SignedPolicyHead,
    sequence: u64,
    k: u64,
    revoked: Vec<Vec<u8>>,
) -> SignedSpoolPolicyRecord {
    let mut body = SignedPolicyBody {
        format_version: 1,
        spool_uuid: SPOOL.to_vec(),
        expected_head: Some(head),
        sequence,
        policy: Some(SignedSpoolPolicy {
            revoked_key_ids: revoked,
            max_audience: Some(Audience::Private as i32),
        }),
        merge_policies: policy::required_merge_policies(),
        owner_id: state.owner_id().to_vec(),
        owner_state_hash: state.state_hash().to_vec(),
        ownership_transfer_sequence: k,
        ..Default::default()
    };
    body.policy_state_hash = policy::policy_state_hash(&body)
        .expect("policy hash")
        .to_vec();
    let signature =
        signer.sign_digest(&policy::policy_signature_digest(&body).expect("policy digest"));
    SignedSpoolPolicyRecord {
        body: Some(body),
        owner_signature: Some(signature),
    }
}

fn production_cases() -> Vec<Value> {
    let mut cases = Vec::new();
    let fixture: Value =
        serde_json::from_str(conformance::KEYRING_FIXTURE_V2_JSON).expect("keyring fixture");
    for c in fixture["cases"].as_array().expect("cases") {
        let wire = CloneAuthorizationKeyring::decode(
            hex::decode(c["keyring_hex"].as_str().expect("keyring"))
                .expect("hex")
                .as_slice(),
        )
        .expect("keyring decode");
        let observed = observed_owner(
            wire.owner_root.as_ref().expect("root"),
            wire.accepted_transitions.clone(),
            &wire.accepted_state_hash,
        );
        cases.push(json!({"id": format!("keyring-{}", c["name"].as_str().expect("name")), "api":"resource-keyring", "keyring_hex":c["keyring_hex"], "current_owner_hex":hex::encode(observed.encode_to_vec()), "now":c["now_unix_seconds"], "expected_accept":c["expected_accept"]}));
    }
    let fixture: Value =
        serde_json::from_str(conformance::TRANSFER_FIXTURE_V2_JSON).expect("transfer fixture");
    for c in fixture["cases"].as_array().expect("cases") {
        let history = |field: &str| {
            let root = SignedOwnerRoot::decode(
                hex::decode(c[field].as_str().expect("root"))
                    .expect("hex")
                    .as_slice(),
            )
            .expect("root decode");
            let state = verify_owner_root(&root).expect("fixture root");
            hex::encode(
                OwnerHistory {
                    root: Some(root),
                    accepted_transitions: vec![],
                    state_hash: state.state_hash().to_vec(),
                }
                .encode_to_vec(),
            )
        };
        cases.push(json!({"id":format!("transfer-{}", c["name"].as_str().expect("name")), "api":"transfer", "transfer_hex":c["transfer_hex"], "source_history_hex":history("source_owner_root_hex"), "destination_history_hex":history("destination_owner_root_hex"), "resource_uuid_hex":c["resource_uuid_hex"], "sequence":1, "now":NOW,"expected_accept":c["expected_accept"]}));
    }
    let browser: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/browser_spool_creation_interop.json"
    ))
    .expect("browser fixture");
    cases.push(json!({"id":"genesis-browser-delegated", "api":"genesis", "genesis_hex":browser["signed_spool_owner_genesis_hex"],"now":browser["now_unix_seconds"], "expected_accept":true}));
    let delegated = SignedSpoolOwnerGenesis::decode(
        hex::decode(
            browser["signed_spool_owner_genesis_hex"]
                .as_str()
                .expect("genesis"),
        )
        .expect("hex")
        .as_slice(),
    )
    .expect("delegated genesis");
    let history = delegated
        .delegated_creation
        .as_ref()
        .expect("creation proof")
        .owner_history
        .as_ref()
        .expect("owner history");
    let root = history.root.as_ref().expect("root");
    let ring = CloneAuthorizationKeyring {
        format_version: 1,
        spool_uuid: delegated.genesis.as_ref().expect("body").spool_uuid.clone(),
        owner_genesis: Some(delegated.clone()),
        owner_root: Some(root.clone()),
        accepted_transitions: history.accepted_transitions.clone(),
        accepted_state_hash: history.state_hash.clone(),
        canonical_spool_path_segments: vec!["example".into()],
        pin: Some(CloneOwnerPin {
            kind: 2,
            expected_owner_id: root.root.as_ref().expect("root body").owner_id.clone(),
            first_seen_unix_seconds: 0,
        }),
        ..Default::default()
    };
    let mut delegated_observation = observed_owner(
        root,
        history.accepted_transitions.clone(),
        &history.state_hash,
    );
    delegated_observation.resource_keyring = Some(ring.clone());
    let mut delegated_keyring = json!({"id":"keyring-browser-delegated-creation","api":"resource-keyring","keyring_hex":hex::encode(ring.encode_to_vec()),"current_owner_hex":hex::encode(delegated_observation.encode_to_vec()),"now":browser["now_unix_seconds"],"expected_accept":true});
    cases.push(delegated_keyring.clone());
    delegated_keyring["id"] = json!("delegated-creation-transfer-chain-zero");
    delegated_keyring["api"] = json!("transfer-chain");
    cases.push(delegated_keyring);
    let mut broken = cases
        .iter()
        .find(|c| c["id"] == "genesis-browser-delegated")
        .expect("browser case")
        .clone();

    broken["id"] = json!("genesis-browser-missing-restriction");
    broken["genesis_hex"] = browser["missing_restriction_signed_spool_owner_genesis_hex"].clone();
    broken["expected_accept"] = json!(false);
    cases.push(broken);
    // API's immutable self-signed genesis vector, independent of this crate's signer.
    let api: Value =
        serde_json::from_str(include_str!("../tests/fixtures/api_owner_authz_v2.json"))
            .expect("API vector");
    let genesis = SignedSpoolOwnerGenesis {
        genesis: Some(SpoolOwnerGenesis {
            spool_uuid: hex::decode(api["spool_uuid_hex"].as_str().expect("UUID")).expect("hex"),
            owner_public_key: Some(AuthorizationVerificationKey {
                algorithm: 1,
                public_key: hex::decode(api["signer_public_key_hex"].as_str().expect("key"))
                    .expect("hex"),
            }),
        }),
        owner_signature: Some(AuthorizationSignature {
            signer_key_id: policy::owner_key_id(&AuthorizationVerificationKey {
                algorithm: 1,
                public_key: hex::decode(api["signer_public_key_hex"].as_str().expect("key"))
                    .expect("hex"),
            })
            .to_vec(),
            signature: hex::decode(api["genesis_signature_hex"].as_str().expect("signature"))
                .expect("hex"),
        }),
        delegated_creation: None,
    };
    cases.push(json!({"id":"genesis-api-self-signed","api":"genesis","genesis_hex":hex::encode(genesis.encode_to_vec()),"now":NOW,"expected_accept":true}));
    let hybrid: Value = serde_json::from_str(include_str!(
        "../conformance/hybrid/import-authority-host-witness-v1.json"
    ))
    .expect("API HYBRID fixture");
    let bytes = |name: &str| {
        hex::decode(
            hybrid["wire_vectors"][name]["wire_hex"]
                .as_str()
                .expect("wire vector"),
        )
        .expect("hex")
    };
    let h = OwnerHistory::decode(bytes("owner_history").as_slice()).expect("history");
    let g =
        SignedSpoolOwnerGenesis::decode(bytes("spool_owner_genesis").as_slice()).expect("genesis");
    let owner = verify_owner_root(h.root.as_ref().expect("root")).expect("API owner");
    cases.push(json!({"id":"owner-root-api-fixed-vector","api":"owner-root","root_hex":hex::encode(h.root.as_ref().expect("root").encode_to_vec()),"now":1100,"expected_accept":true}));
    let ring = CloneAuthorizationKeyring {
        format_version: 1,
        spool_uuid: g.genesis.as_ref().expect("genesis body").spool_uuid.clone(),
        owner_genesis: Some(g),
        owner_root: h.root.clone(),
        accepted_transitions: h.accepted_transitions.clone(),
        accepted_state_hash: h.state_hash.clone(),
        canonical_spool_path_segments: vec!["example".into()],
        pin: Some(CloneOwnerPin {
            kind: 2,
            expected_owner_id: owner.owner_id().to_vec(),
            first_seen_unix_seconds: 1000,
        }),
        ..Default::default()
    };
    let api_owner = observed_owner(
        h.root.as_ref().expect("root"),
        h.accepted_transitions,
        &h.state_hash,
    );
    cases.push(json!({"id":"policy-api-fixed-vector","api":"policy","keyring_hex":hex::encode(ring.encode_to_vec()),"current_owner_hex":hex::encode(api_owner.encode_to_vec()),"records_hex":[hex::encode(bytes("signed_policy"))],"now":1100,"expected_accept":true}));
    let (mut keyring, _) = base_keyring();
    let source_root = keyring.owner_root.clone().expect("source root");
    let source = verify_owner_root(&source_root).expect("source");
    let destination_key = TestKey::new(11);
    let destination_root = signed_root(
        [0x33; 16],
        &destination_key,
        &[
            (&TestKey::new(12), RecoveryGuardianKind::Paper),
            (&TestKey::new(13), RecoveryGuardianKind::Social),
        ],
    );
    let destination = verify_owner_root(&destination_root).expect("destination");
    let transfer = signed_transfer(
        OWNER_UUID,
        &source,
        &TestKey::new(1),
        [0x33; 16],
        &destination,
        &destination_key,
    );
    let mut audit = ResourceTransferAuditRecord {
        transfer: Some(transfer),
        committed_at_unix_seconds: NOW,
        ..Default::default()
    };
    audit.audit_record_hash = resource_transfer_audit_hash(&audit)
        .expect("audit")
        .to_vec();
    keyring.ownership_transfers.push(audit);
    keyring.transfer_owner_histories = vec![
        OwnerHistory {
            root: Some(source_root),
            accepted_transitions: vec![],
            state_hash: source.state_hash().to_vec(),
        },
        OwnerHistory {
            root: Some(destination_root.clone()),
            accepted_transitions: vec![],
            state_hash: destination.state_hash().to_vec(),
        },
    ];
    let observed = observed_owner(&destination_root, vec![], &destination.state_hash());
    let baseline = json!({"id":"keyring-transferred","api":"resource-keyring","keyring_hex":hex::encode(keyring.encode_to_vec()),"current_owner_hex":hex::encode(observed.encode_to_vec()),"now":NOW,"expected_accept":true});
    cases.push(baseline.clone());
    let mut chain = baseline.clone();
    chain["id"] = json!("accepted-transfer-chain");
    chain["api"] = json!("transfer-chain");
    cases.push(chain);
    let rotated = rotation(&destination, &destination_key, &TestKey::new(14));
    let rotated_state =
        apply_accepted_transition(&destination, &rotated, NOW, limits()).expect("later rotation");
    let mut after = baseline.clone();
    after["id"] = json!("keyring-transferred-then-rotated");
    after["current_owner_hex"] = json!(hex::encode(
        observed_owner(
            &destination_root,
            vec![rotated],
            &rotated_state.state_hash()
        )
        .encode_to_vec()
    ));
    cases.push(after);
    // Both sides, audit links and sequence failures must be independently rejected.
    for (id, code, mode) in [
        ("transfer-wrong-signature-side", "invalid_signature", 0),
        ("transfer-sequence-gap", "broken_chain", 1),
        ("transfer-broken-audit-predecessor", "broken_chain", 2),
        ("transfer-missing-source-history", "broken_chain", 3),
    ] {
        let mut bad = keyring.clone();
        match mode {
            0 => {
                let accept = bad.ownership_transfers[0]
                    .transfer
                    .as_mut()
                    .expect("transfer")
                    .acceptance
                    .as_mut()
                    .expect("acceptance");
                accept
                    .signed_handoff
                    .as_mut()
                    .expect("handoff")
                    .source_signature = accept.destination_signature.clone();
            }
            1 => {
                bad.ownership_transfers[0]
                    .transfer
                    .as_mut()
                    .expect("transfer")
                    .acceptance
                    .as_mut()
                    .expect("acceptance")
                    .signed_handoff
                    .as_mut()
                    .expect("signed")
                    .handoff
                    .as_mut()
                    .expect("handoff")
                    .transfer_sequence = 2
            }
            2 => bad.ownership_transfers[0].previous_audit_record_hash = vec![1; 32],
            _ => {
                bad.transfer_owner_histories.remove(0);
            }
        }
        bad.ownership_transfers[0].audit_record_hash =
            resource_transfer_audit_hash(&bad.ownership_transfers[0])
                .expect("new audit hash")
                .to_vec();
        let mut c = baseline.clone();
        c["id"] = json!(id);
        c["keyring_hex"] = json!(hex::encode(bad.encode_to_vec()));
        c["expected_accept"] = json!(false);
        c["expected_code"] = json!(code);
        cases.push(c);
    }
    let first = policy_record(
        &source,
        &TestKey::new(1),
        policy::zero_head(),
        1,
        0,
        vec![vec![0x44; 32]],
    );
    let head = SignedPolicyHead {
        state_hash: first.body.as_ref().expect("body").policy_state_hash.clone(),
        sequence: 1,
    };
    let second = policy_record(
        &destination,
        &destination_key,
        head.clone(),
        2,
        1,
        vec![vec![0x44; 32]],
    );
    let mut c = baseline.clone();
    c["id"] = json!("policy-across-transfer");
    c["api"] = json!("policy");
    c["records_hex"] = json!([
        hex::encode(first.encode_to_vec()),
        hex::encode(second.encode_to_vec())
    ]);
    cases.push(c.clone());
    let old_owner_current = verify_clone_keyring(keyring.clone(), NOW, limits(), &[])
        .expect("keyring")
        .owner_state()
        .clone();
    let late_first = policy_record(
        &old_owner_current,
        &TestKey::new(7),
        policy::zero_head(),
        1,
        0,
        vec![vec![0x44; 32]],
    );
    let late_head = SignedPolicyHead {
        sequence: 1,
        state_hash: late_first
            .body
            .as_ref()
            .expect("body")
            .policy_state_hash
            .clone(),
    };
    let after_late = policy_record(
        &destination,
        &destination_key,
        late_head,
        2,
        1,
        vec![vec![0x44; 32]],
    );
    let mut backdated_authority = c.clone();
    backdated_authority["id"] = json!("policy-source-state-after-handoff");
    backdated_authority["records_hex"] = json!([
        hex::encode(late_first.encode_to_vec()),
        hex::encode(after_late.encode_to_vec())
    ]);
    backdated_authority["expected_accept"] = json!(false);
    backdated_authority["expected_code"] = json!("policy_not_owner_signed");
    cases.push(backdated_authority);
    for (id, code, mode) in [
        ("policy-tampered-signature", "policy_not_owner_signed", 0),
        ("policy-broken-head", "policy_broken_chain", 1),
        ("policy-sequence-gap", "policy_broken_chain", 2),
        ("policy-drop-revocation", "policy_grow_only_dropped", 3),
        ("policy-revoke-authority", "policy_self_revocation", 4),
        (
            "policy-wrong-owner-after-transfer",
            "policy_not_owner_signed",
            5,
        ),
        ("policy-backdated-tip", "policy_backdated_transfer", 6),
    ] {
        let mut bad = second.clone();
        match mode {
            0 => bad.owner_signature.as_mut().expect("signature").signature[0] ^= 1,
            1 => {
                let mut wrong = head.clone();
                wrong.state_hash[0] ^= 1;
                bad = policy_record(
                    &destination,
                    &destination_key,
                    wrong,
                    2,
                    1,
                    vec![vec![0x44; 32]],
                );
            }
            2 => {
                bad = policy_record(
                    &destination,
                    &destination_key,
                    head.clone(),
                    3,
                    1,
                    vec![vec![0x44; 32]],
                )
            }
            3 => bad = policy_record(&destination, &destination_key, head.clone(), 2, 1, vec![]),
            4 => {
                let mut ids = vec![
                    vec![0x44; 32],
                    policy::owner_key_id(&destination_key.wire()).to_vec(),
                ];
                ids.sort();
                bad = policy_record(&destination, &destination_key, head.clone(), 2, 1, ids);
            }
            5 => {
                bad = policy_record(
                    &source,
                    &TestKey::new(1),
                    head.clone(),
                    2,
                    1,
                    vec![vec![0x44; 32]],
                )
            }
            _ => {
                bad = policy_record(
                    &source,
                    &TestKey::new(1),
                    head.clone(),
                    2,
                    0,
                    vec![vec![0x44; 32]],
                )
            }
        }
        let mut bad_case = c.clone();
        bad_case["id"] = json!(id);
        bad_case["records_hex"] = json!([
            hex::encode(first.encode_to_vec()),
            hex::encode(bad.encode_to_vec())
        ]);
        bad_case["expected_accept"] = json!(false);
        bad_case["expected_code"] = json!(code);
        cases.push(bad_case);
    }
    // The destination rotated before accepting this Spool. Its earlier root
    // key remains signature provenance but cannot sign in the new phase.
    let entry_rotation = rotation(&destination, &destination_key, &TestKey::new(14));
    let entry_state = apply_accepted_transition(&destination, &entry_rotation, NOW, limits())
        .expect("entry state");
    let mut later_entry = keyring.clone();
    later_entry.ownership_transfers[0].transfer = Some(signed_transfer(
        OWNER_UUID,
        &source,
        &TestKey::new(1),
        [0x33; 16],
        &entry_state,
        &TestKey::new(14),
    ));
    later_entry.ownership_transfers[0].audit_record_hash =
        resource_transfer_audit_hash(&later_entry.ownership_transfers[0])
            .expect("audit hash")
            .to_vec();
    later_entry.transfer_owner_histories[1] = OwnerHistory {
        root: Some(destination_root.clone()),
        accepted_transitions: vec![entry_rotation.clone()],
        state_hash: entry_state.state_hash().to_vec(),
    };
    let entry_observed = observed_owner(
        &destination_root,
        vec![entry_rotation],
        &entry_state.state_hash(),
    );
    let mut pre_entry = c.clone();
    pre_entry["id"] = json!("policy-destination-state-before-handoff");
    pre_entry["keyring_hex"] = json!(hex::encode(later_entry.encode_to_vec()));
    pre_entry["current_owner_hex"] = json!(hex::encode(entry_observed.encode_to_vec()));
    pre_entry["expected_accept"] = json!(false);
    pre_entry["expected_code"] = json!("policy_not_owner_signed");
    cases.push(pre_entry.clone());
    pre_entry["id"] = json!("policy-rotated-destination-handoff");
    pre_entry["expected_accept"] = json!(true);
    pre_entry
        .as_object_mut()
        .expect("case")
        .remove("expected_code");
    let correct_entry = policy_record(
        &entry_state,
        &TestKey::new(14),
        head.clone(),
        2,
        1,
        vec![vec![0x44; 32]],
    );
    pre_entry["records_hex"] = json!([
        hex::encode(first.encode_to_vec()),
        hex::encode(correct_entry.encode_to_vec())
    ]);
    cases.push(pre_entry.clone());
    // A rotated signing owner may not revoke its retired root key. A policy
    // signed at the root also cannot revoke a later accepted authority key.
    for (id, state, signer, forbidden) in [
        (
            "policy-rotated-owner-revoke-history",
            &entry_state,
            TestKey::new(14),
            destination_key.wire(),
        ),
        (
            "policy-historical-state-revoke-later-authority",
            &destination,
            TestKey::new(11),
            TestKey::new(14).wire(),
        ),
    ] {
        let mut ids = vec![vec![0x44; 32], policy::owner_key_id(&forbidden).to_vec()];
        ids.sort();
        let bad = policy_record(state, &signer, head.clone(), 2, 1, ids);
        let mut case = c.clone();
        case["id"] = json!(id);
        case["current_owner_hex"] = json!(hex::encode(entry_observed.encode_to_vec()));
        case["records_hex"] = json!([
            hex::encode(first.encode_to_vec()),
            hex::encode(bad.encode_to_vec())
        ]);
        case["expected_accept"] = json!(false);
        case["expected_code"] = json!("policy_self_revocation");
        cases.push(case);
    }
    // Revocations introduced by a previous owner carry through a transfer,
    // even if the new owner's history contains the already revoked key.
    let mut inherited_ids = vec![policy::owner_key_id(&destination_key.wire()).to_vec()];
    inherited_ids.sort();
    let inherited_first = policy_record(
        &source,
        &TestKey::new(1),
        policy::zero_head(),
        1,
        0,
        inherited_ids.clone(),
    );
    let inherited_head = SignedPolicyHead {
        sequence: 1,
        state_hash: inherited_first
            .body
            .as_ref()
            .expect("body")
            .policy_state_hash
            .clone(),
    };
    let inherited_second = policy_record(
        &entry_state,
        &TestKey::new(14),
        inherited_head,
        2,
        1,
        inherited_ids,
    );
    let mut inherited = c.clone();
    inherited["id"] = json!("policy-inherited-history-key-revocation");
    inherited["current_owner_hex"] = json!(hex::encode(entry_observed.encode_to_vec()));
    inherited["records_hex"] = json!([
        hex::encode(inherited_first.encode_to_vec()),
        hex::encode(inherited_second.encode_to_vec())
    ]);
    cases.push(inherited);
    let mut empty = c.clone();
    empty["id"] = json!("policy-empty-chain");
    empty["records_hex"] = json!([]);
    empty["expected_accept"] = json!(false);
    empty["expected_code"] = json!("policy_invalid");
    cases.push(empty);
    let mut incomplete = c;
    incomplete["id"] = json!("policy-missing-transfer-tip");
    incomplete["records_hex"] = json!([hex::encode(first.encode_to_vec())]);
    incomplete["expected_accept"] = json!(false);
    incomplete["expected_code"] = json!("policy_backdated_transfer");
    cases.push(incomplete);
    for case in &mut cases {
        for field in ["now", "max_ttl", "sequence"] {
            if let Some(value) = case.get(field).cloned() {
                case[field] = json!(value.to_string());
            }
        }
    }
    cases
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn observed_production_vectors_reject_for_intended_reasons() {
    for c in production_cases() {
        let result = conformance::production::evaluate(&c).expect("evaluate");
        assert_eq!(
            result.get("ok").is_some(),
            c["expected_accept"].as_bool().expect("expected"),
            "{}: {result}",
            c["id"]
        );
        if c["expected_accept"] == true
            && matches!(
                c["api"].as_str(),
                Some("resource-keyring" | "transfer-chain")
            )
        {
            let observed = OwnerState::decode(
                hex::decode(c["current_owner_hex"].as_str().expect("owner"))
                    .expect("hex")
                    .as_slice(),
            )
            .expect("observed owner");
            let root = observed
                .root
                .as_ref()
                .expect("root")
                .root
                .as_ref()
                .expect("root body");
            assert_eq!(
                result["ok"]["current_owner"]["state_hash_hex"],
                hex::encode(&observed.version),
                "exact current state"
            );
            assert_eq!(
                result["ok"]["current_owner"]["owner_uuid_hex"],
                hex::encode(&root.account_uuid),
                "sole owner UUID"
            );
            assert_eq!(
                result["ok"]["current_owner"]["owner_id_hex"],
                hex::encode(&root.owner_id),
                "root identity"
            );
            let ring = CloneAuthorizationKeyring::decode(
                hex::decode(c["keyring_hex"].as_str().expect("keyring"))
                    .expect("hex")
                    .as_slice(),
            )
            .expect("keyring");
            assert_eq!(
                result["ok"]["accepted_transfer_sequence"],
                ring.ownership_transfers.len().to_string(),
                "accepted sequence"
            );
            assert_eq!(
                result["ok"]["ownership_transfers"]
                    .as_array()
                    .expect("transfers")
                    .len(),
                ring.ownership_transfers.len()
            );
        }
        if let Some(code) = c.get("expected_code") {
            assert_eq!(&result["error"]["code"], code, "{}", c["id"]);
        }
    }
}

#[test]
#[ignore = "maintainer-only production corpus regeneration"]
fn print_production_fixture() {
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"format_version":1,"cases":production_cases()}))
            .expect("JSON")
    );
}

#[cfg(target_arch = "wasm32")]
fn evaluate_binding(c: &Value) -> Value {
    use wasm_bindgen::{JsCast, JsValue};

    use crate::wasm;
    let bytes = |name: &str| hex::decode(c[name].as_str().expect("hex field")).expect("hex");
    let now = c["now"]
        .as_str()
        .expect("decimal now")
        .parse::<i64>()
        .expect("now");
    let result = match c["api"].as_str().expect("API") {
        "owner-root" => wasm::verify_owner_root_binding(&bytes("root_hex")),
        "resource-keyring" => wasm::verify_resource_keyring_binding(
            &bytes("keyring_hex"),
            &bytes("current_owner_hex"),
            now.into(),
            3600_i64.into(),
        ),
        "transfer-chain" => wasm::verify_ownership_transfer_chain_binding(
            &bytes("keyring_hex"),
            &bytes("current_owner_hex"),
            now.into(),
            3600_i64.into(),
        ),
        "transfer" => wasm::verify_ownership_transfer_binding(
            &bytes("transfer_hex"),
            &bytes("source_history_hex"),
            &bytes("destination_history_hex"),
            &bytes("resource_uuid_hex"),
            c["sequence"]
                .as_str()
                .expect("decimal sequence")
                .parse::<u64>()
                .expect("sequence")
                .into(),
            now.into(),
            3600_i64.into(),
        ),
        "genesis" => wasm::verify_spool_owner_genesis_binding(&bytes("genesis_hex"), now.into()),
        "policy" => {
            let records = c["records_hex"]
                .as_array()
                .expect("records")
                .iter()
                .map(|v| {
                    let bytes = hex::decode(v.as_str().expect("record")).expect("hex");
                    JsValue::from(js_sys::Uint8Array::from(bytes.as_slice()))
                })
                .collect();
            wasm::verify_signed_policy_chain_binding(
                records,
                &bytes("keyring_hex"),
                &bytes("current_owner_hex"),
                now.into(),
                3600_i64.into(),
            )
        }
        _ => panic!("unknown test API"),
    };
    let (field, value) = match result {
        Ok(value) => ("ok", value),
        Err(error) => ("error", error),
    };
    assert!(value.is_object());
    assert!(!value.is_instance_of::<js_sys::Error>());
    let parsed: Value = serde_json::from_str(
        &js_sys::JSON::stringify(&value)
            .expect("JS JSON")
            .as_string()
            .expect("string"),
    )
    .expect("JSON");
    json!({field:parsed})
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen_test]
fn production_bindings_match_native_objects_and_error_codes() {
    let fixture: Value =
        serde_json::from_str(include_str!("../conformance/fixtures/production-v1.json"))
            .expect("production fixture");
    for c in fixture["cases"].as_array().expect("cases") {
        assert_eq!(
            evaluate_binding(c),
            conformance::production::evaluate(c).expect("native result"),
            "{}",
            c["id"]
        );
    }
    let error = crate::wasm::verify_signed_policy_chain_binding(
        vec![wasm_bindgen::JsValue::from_str("not protobuf bytes")],
        &[],
        &[],
        NOW.into(),
        3600_i64.into(),
    )
    .expect_err("wrong input type");
    assert_eq!(
        js_sys::Reflect::get(&error, &"code".into())
            .expect("code")
            .as_string()
            .as_deref(),
        Some("invalid")
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn single_policy_provenance_rejects_malformed_predecessor() {
    let (keyring, _) = base_keyring();
    let owner = verify_owner_root(keyring.owner_root.as_ref().expect("root")).expect("owner");
    let signed = policy_record(
        &owner,
        &TestKey::new(1),
        SignedPolicyHead {
            sequence: 0,
            state_hash: vec![0; 31],
        },
        1,
        0,
        vec![],
    );
    assert!(matches!(
        crate::import_delegation::verify_policy_record(&signed, &[&owner]),
        Err(Error::Policy(policy::OwnerGovernanceError::Invalid(_)))
    ));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn single_policy_provenance_accepts_inherited_history_key_revocation() {
    let fixture: Value =
        serde_json::from_str(include_str!("../conformance/fixtures/production-v1.json"))
            .expect("production fixture");
    let case = fixture["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| case["id"] == "policy-inherited-history-key-revocation")
        .expect("inherited revocation fixture");
    let bytes = |field: &str| hex::decode(case[field].as_str().expect("wire hex")).expect("bytes");
    let records: Vec<_> = case["records_hex"]
        .as_array()
        .expect("records")
        .iter()
        .map(|record| hex::decode(record.as_str().expect("record hex")).expect("record bytes"))
        .collect();
    let now = case["now"]
        .as_str()
        .expect("time")
        .parse()
        .expect("seconds");
    let keyring = bytes("keyring_hex");
    let current_owner = bytes("current_owner_hex");
    crate::observed::verify_signed_policy_chain_bytes(
        &records,
        &keyring,
        &current_owner,
        now,
        3600,
    )
    .expect("inherited revocation passes full-chain admission");
    let (_, owner) = crate::observed::ownership(&keyring, &current_owner, now, limits())
        .expect("verified owner");
    let mut tip = SignedSpoolPolicyRecord::decode(records.last().expect("tip").as_slice())
        .expect("signed policy");
    crate::import_delegation::verify_policy_record(&tip, &[&owner])
        .expect("retained tip preserves the predecessor's revocation");
    tip.owner_signature.as_mut().expect("signature").signature[0] ^= 1;
    assert!(matches!(
        crate::import_delegation::verify_policy_record(&tip, &[&owner]),
        Err(Error::Policy(policy::OwnerGovernanceError::NotOwnerSigned))
    ));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn policy_rejects_rotated_owner_history_keys_at_introduction() {
    for case in production_cases().into_iter().filter(|c| {
        matches!(
            c["id"].as_str(),
            Some(
                "policy-rotated-owner-revoke-history"
                    | "policy-historical-state-revoke-later-authority"
            )
        )
    }) {
        let result = conformance::production::evaluate(&case).expect("evaluate");
        assert_eq!(
            result["error"]["code"], "policy_self_revocation",
            "{}: {result}",
            case["id"]
        );
    }
    let (ring, _) = base_keyring();
    let owner = verify_clone_keyring(ring, NOW, limits(), &[])
        .expect("ring")
        .owner_state()
        .clone();
    let signed = policy_record(
        &owner,
        &TestKey::new(7),
        policy::zero_head(),
        1,
        0,
        vec![policy::owner_key_id(&TestKey::new(1).wire()).to_vec()],
    );
    assert!(matches!(
        policy::verify_signed_spool_policy_record(policy::VerifySignedPolicy {
            signed: &signed,
            spool_uuid: SPOOL,
            accepted_head: &policy::zero_head(),
            accepted_owner_id: owner.owner_id(),
            accepted_owner_state_hash: owner.state_hash(),
            required_transfer_sequence: 0,
            authority_key: owner.authority_key(),
            owner_authority_key_ids: &owner.authority_key_ids().collect::<Vec<_>>(),
            accepted_grow_only: &std::collections::BTreeMap::new(),
            ancestor_ceiling: None,
        }),
        Err(policy::OwnerGovernanceError::SelfRevocation)
    ));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn single_policy_provenance_bounds_revocation_count() {
    let (ring, _) = base_keyring();
    let owner = verify_owner_root(ring.owner_root.as_ref().expect("root")).expect("owner");
    for count in [4096, 4097] {
        let ids = (0..count)
            .map(|i: u64| {
                let mut id = vec![0; 32];
                id[24..].copy_from_slice(&i.to_be_bytes());
                id
            })
            .collect();
        let signed = policy_record(&owner, &TestKey::new(1), policy::zero_head(), 1, 0, ids);
        let result = crate::import_delegation::verify_policy_record(&signed, &[&owner]);
        if count == 4096 {
            result.expect("4096 revocations are permitted");
        } else {
            assert!(matches!(
                result,
                Err(Error::Policy(policy::OwnerGovernanceError::Invalid(_)))
            ));
        }
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn import_byte_adapter_uses_general_owner_transition_bound() {
    use std::collections::BTreeSet;

    use heddle_api::{hybrid_codec, import_authority as contract};
    let fixture: Value = serde_json::from_str(include_str!(
        "../conformance/hybrid/import-authority-host-witness-v1.json"
    ))
    .expect("fixture");
    let mut certificate = SignedImportJobDelegationV1::decode(
        hex::decode(
            fixture["signed_vectors"]["delegation"]["wire_hex"]
                .as_str()
                .expect("wire"),
        )
        .expect("hex")
        .as_slice(),
    )
    .expect("certificate");
    let (mut ring, _) = base_keyring();
    ring.accepted_transitions.clear();
    let root = ring.owner_root.clone().expect("root");
    let initial = verify_owner_root(&root).expect("root state");
    ring.accepted_state_hash = initial.state_hash().to_vec();
    let genesis_digest = crate::creation::spool_genesis_digest(
        ring.owner_genesis
            .as_ref()
            .expect("genesis")
            .genesis
            .as_ref()
            .expect("body"),
    )
    .expect("digest");
    let mut history = OwnerHistory {
        root: Some(root),
        accepted_transitions: vec![],
        state_hash: initial.state_hash().to_vec(),
    };
    let mut owner = initial.clone();
    let mut signer = TestKey::new(1);
    for count in 1..=VerificationLimits::MAX_TRANSITIONS + 1 {
        let next = TestKey::new(if count % 2 == 0 { 8 } else { 7 });
        let signed = rotation(&owner, &signer, &next);
        owner =
            apply_accepted_transition(&owner, &signed, NOW, limits()).expect("accepted rotation");
        signer = next;
        history.accepted_transitions.push(signed);
        history.state_hash = owner.state_hash().to_vec();
        if ![
            65,
            VerificationLimits::MAX_TRANSITIONS,
            VerificationLimits::MAX_TRANSITIONS + 1,
        ]
        .contains(&count)
        {
            continue;
        }
        let body = certificate.body.as_mut().expect("body");
        body.identity = Some(ImportIdentityV1 {
            spool_uuid: SPOOL.to_vec(),
            spool_genesis_digest: genesis_digest.to_vec(),
            owner_id: owner.owner_id().to_vec(),
            owner_account_uuid: OWNER_UUID.to_vec(),
            owner_state_hash: owner.state_hash().to_vec(),
            ownership_transfer_sequence: 0,
        });
        body.delegating_public_key = signer.wire().public_key;
        body.parent_permission_digest = vec![0; 32];
        body.not_before_unix_seconds = NOW - 1;
        body.expires_at_unix_seconds = NOW + 100;
        body.owner_chain_digest = contract::owner_chain_digest(&ImportOwnerChainV1 {
            spool_genesis_digest: genesis_digest.to_vec(),
            owner_state_hashes: BTreeSet::from([
                initial.state_hash().to_vec(),
                owner.state_hash().to_vec(),
            ])
            .into_iter()
            .collect(),
            transfer_audit_hashes: vec![],
        })
        .expect("chain digest");
        certificate.delegating_signature = Some(
            signer.sign_digest(
                &hybrid_codec::signing_digest(contract::DELEGATION_DOMAIN, body)
                    .expect("digest")
                    .try_into()
                    .expect("digest width"),
            ),
        );
        let result = crate::import_delegation::verify_bytes(
            &certificate.encode_to_vec(),
            &[],
            &ring.encode_to_vec(),
            &history.encode_to_vec(),
            &initial.owner_id(),
            &genesis_digest,
            "[]",
            "[]",
            "[]",
            "[]",
            NOW,
            3600,
        );
        if count <= VerificationLimits::MAX_TRANSITIONS {
            let digest = result.expect("complete history within general bound");
            assert_eq!(
                digest,
                contract::signed_delegation_digest(&certificate).expect("digest")
            );
        } else {
            assert!(matches!(result, Err(Error::TooLarge { .. })));
        }
    }
}
