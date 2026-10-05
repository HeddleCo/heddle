use heddle_object_model::object::{State, thread_replication::ThreadOperationBody};

use super::*;

fn sign_import(f: &Value, operation: &ThreadOperation) -> wire::SignedDelegatedImportOperationV1 {
    let mut signed: wire::SignedDelegatedImportOperationV1 = record(f, "operation_main");
    let body = signed.body.as_mut().expect("body");
    let ThreadOperationBody::Capture(capture) = &operation.body else {
        panic!("Capture")
    };
    body.expected_frontier_digest = contract::frontier_digest(&wire::ImportFrontierV1 {
        format_version: 1,
        thread_id: operation.thread.as_bytes().to_vec(),
        operation_ids: operation
            .parents
            .iter()
            .map(|id| id.as_bytes().to_vec())
            .collect(),
    })
    .expect("expected frontier");
    body.resulting_frontier_digest = contract::frontier_digest(&wire::ImportFrontierV1 {
        format_version: 1,
        thread_id: operation.thread.as_bytes().to_vec(),
        operation_ids: vec![operation.id().expect("id").as_bytes().to_vec()],
    })
    .expect("result frontier");
    body.resulting_content_digest = contract::content_digest(&wire::ImportContentV1 {
        format_version: 1,
        canonical_capture: rmp_serde::to_vec_named(&capture.result).expect("capture"),
    })
    .expect("content");
    let signer = crate::Ed25519Signer::from_seed(
        &hex::decode(f["keys"]["job"]["seed_hex"].as_str().expect("seed")).expect("hex"),
    )
    .expect("signer");
    signed.job_signature = Some(wire::AuthorizationSignature {
        signer_key_id: hybrid_codec::key_id(signer.public_key()),
        signature: signer
            .sign(&hybrid_codec::signing_digest(contract::OPERATION_DOMAIN, body).expect("digest"))
            .expect("signature"),
    });
    signed
}
fn original(f: &Value) -> (ThreadGenesis, ThreadOperation, wire::ImportIdentityV1) {
    let payload: wire::ImportGenesisWitnessV1 = record(f, "genesis_payload");
    let (_, genesis) = verify_native_genesis(payload.original_genesis.as_ref().expect("genesis"))
        .expect("original genesis");
    let (_, operation) =
        verify_native_operation(&record(f, "converted_main")).expect("original operation");
    (genesis, operation, record(f, "identity"))
}
fn state(operation: &ThreadOperation) -> State {
    operation.source_state().expect("decode").expect("state")
}
fn with_state(mut operation: ThreadOperation, state: &State) -> ThreadOperation {
    let ThreadOperationBody::Capture(capture) = &mut operation.body else {
        panic!("Capture")
    };
    capture.result.state = state.encode_current_msgpack().expect("State bytes");
    operation
}
fn assert_one_import(operation: ThreadOperation, expected: &State) {
    let f = fixture();
    let (genesis, _, identity) = original(&f);
    let delegation = delegation(&f, 1100);
    let signed = sign_import(&f, &operation);
    let bound = DelegatedImport::bind(
        &signed,
        delegation.scope(),
        &genesis,
        &identity,
        &operation,
        &[],
    )
    .expect("one carrier-bound tip operation");
    assert!(bound.converted().parents.is_empty());
    // State IDs are derived when decoding; changing parent bytes must not
    // compare the stale cached ID of the builder with the decoded content.
    let expected =
        State::decode_current_msgpack(&expected.encode_current_msgpack().expect("State bytes"))
            .expect("derived State ID");
    assert_eq!(state(bound.converted()), expected);
    let signer = crate::Ed25519Signer::from_seed(
        &hex::decode(f["keys"]["job"]["seed_hex"].as_str().expect("seed")).expect("hex"),
    )
    .expect("signer");
    let native = SignedOperation::sign(bound.converted(), &signer).expect("native signature");
    let verified = native.verify().expect("native verification");
    DelegatedImport::bind(
        &signed,
        delegation.scope(),
        &genesis,
        &identity,
        &verified,
        &[],
    )
    .expect("same authenticated verdict after decoding");
    let original = wire::SignedRecord {
        format: record::<wire::SignedRecord>(&f, "converted_main").format,
        canonical_record: native.canonical,
        signatures: vec![wire::RecordSignature {
            public_key: signer.public_key().to_vec(),
            signature: native.signature,
        }],
    };
    let payload: wire::ImportGenesisWitnessV1 = record(&f, "genesis_payload");
    let evidence = WitnessEvidence::resolve(
        &trusted_set(&f, "retired_set", 1350000),
        &record(&f, "genesis_admission"),
        Some(&record(&f, "genesis_proof")),
        false,
        1350000,
    )
    .expect("original genesis evidence");
    let records = [
        payload.original_genesis.clone().expect("original genesis"),
        original.clone(),
    ];
    assert!(
        NativeClosure::verify(&records).is_err(),
        "carrierless closure stays strict"
    );
    let closure = NativeClosure::verify_with_imports(&records, &[], |_, op, _| {
        assert_eq!(op, &operation);
        Ok(Some(bound.clone()))
    })
    .expect("State ancestors require no native operations or separate carriers");
    assert_eq!(closure.operations.len(), 1);
    let genesis = verify_genesis_payload(&payload, &evidence, &delegation, |_| false)
        .expect("verified genesis");
    verify_delegated_import(&signed, &delegation, &genesis, &original, &[])
        .expect("dual signature verification of one tip");
}
#[test]
fn imported_root_is_one_operation_with_empty_frontier() {
    let f = fixture();
    let (_, operation, _) = original(&f);
    let root = state(&operation);
    assert!(root.parents.is_empty());
    assert_one_import(operation, &root);
}
#[test]
fn imported_linear_git_tip_is_one_operation_with_empty_frontier() {
    let f = fixture();
    let (_, operation, _) = original(&f);
    let root = state(&operation);
    let mut tip = root.clone();
    tip.parents = vec![root.id()];
    tip.intent = Some("linear Git tip".into());
    assert_one_import(with_state(operation, &tip), &tip);
}
#[test]
fn imported_merge_of_disjoint_git_roots_preserves_order_in_one_operation() {
    let f = fixture();
    let (_, operation, _) = original(&f);
    let left = state(&operation);
    let mut right = left.clone();
    right.intent = Some("disjoint Git root".into());
    let mut merge = left.clone();
    merge.parents = vec![right.id(), left.id()];
    merge.intent = Some("ordered Git merge".into());
    assert_one_import(with_state(operation, &merge), &merge);
}
#[test]
fn imported_capture_cannot_use_seed_or_a_foreign_carrier() {
    let f = fixture();
    let (genesis, operation, identity) = original(&f);
    let delegation = delegation(&f, 1100);
    let other_payload: wire::ImportGenesisWitnessV1 = record(&f, "genesis_dev_payload");
    let (_, other_genesis) = verify_native_genesis(
        other_payload
            .original_genesis
            .as_ref()
            .expect("other genesis"),
    )
    .expect("signed other genesis");
    let (_, other_operation) =
        verify_native_operation(&record(&f, "converted_dev")).expect("signed other tip");
    assert!(
        DelegatedImport::bind(
            &sign_import(&f, &operation),
            delegation.scope(),
            &other_genesis,
            &identity,
            &other_operation,
            &[]
        )
        .is_err(),
        "carrier for another Thread/genesis cannot unlock ancestry"
    );
    assert!(
        operation.validate_parents(&genesis, &[]).is_err(),
        "carrierless and ordinary parentless Captures remain strict"
    );
    let mut foreign_state = state(&operation);
    foreign_state.intent = Some("different converted tip".into());
    let foreign = with_state(operation.clone(), &foreign_state);
    assert!(
        DelegatedImport::bind(
            &sign_import(&f, &operation),
            delegation.scope(),
            &genesis,
            &identity,
            &foreign,
            &[]
        )
        .is_err(),
        "a valid imported root cannot borrow another operation's carrier"
    );
    let mut foreign_frontier = foreign.clone();
    foreign_frontier
        .parents
        .insert(operation.id().expect("causal parent"));
    foreign_state.parents = vec![state(&operation).id()];
    let foreign_frontier = with_state(foreign_frontier, &foreign_state);
    assert!(
        DelegatedImport::bind(
            &sign_import(&f, &operation),
            delegation.scope(),
            &genesis,
            &identity,
            &foreign_frontier,
            std::slice::from_ref(&operation)
        )
        .is_err(),
        "a valid descendant cannot borrow another frontier's carrier"
    );
    let mut seeded = state(&operation);
    seeded.parents = vec![genesis.base];
    let seeded = with_state(operation.clone(), &seeded);
    let seeded_carrier = sign_import(&f, &seeded);
    assert!(
        DelegatedImport::bind(
            &seeded_carrier,
            delegation.scope(),
            &genesis,
            &identity,
            &seeded,
            &[]
        )
        .is_err(),
        "even a genuine carrier cannot introduce the seed"
    );
    assert!(
        DelegatedImport::bind(
            &sign_import(&f, &operation),
            delegation.scope(),
            &genesis,
            &identity,
            &seeded,
            &[]
        )
        .is_err(),
        "carrier must bind this exact operation"
    );
    let mut duplicated = state(&operation);
    let arbitrary = heddle_object_model::object::StateId::from_bytes([98; 32]);
    duplicated.parents = vec![arbitrary, arbitrary];
    let duplicated = with_state(operation.clone(), &duplicated);
    assert!(
        DelegatedImport::bind(
            &sign_import(&f, &duplicated),
            delegation.scope(),
            &genesis,
            &identity,
            &duplicated,
            &[]
        )
        .is_err(),
        "Git parent order is retained, but duplicates are invalid"
    );
    let mut changed = operation.clone();
    changed.parents.insert(operation.id().expect("parent id"));
    assert!(
        DelegatedImport::bind(
            &sign_import(&f, &operation),
            delegation.scope(),
            &genesis,
            &identity,
            &changed,
            std::slice::from_ref(&operation)
        )
        .is_err(),
        "different frontier cannot reuse a carrier"
    );
    assert!(
        DelegatedImport::bind(
            &sign_import(&f, &changed),
            delegation.scope(),
            &genesis,
            &identity,
            &changed,
            &[operation]
        )
        .is_err(),
        "genuine job signature cannot widen the fixed branch frontier"
    );
}

#[test]
fn ordinary_native_parentless_capture_remains_strict_with_valid_signature() {
    let f = fixture();
    let (genesis, mut operation, _) = original(&f);
    let signer = fixture_signer(&f, "device");
    operation.publisher = signer.public_key().try_into().expect("device public key");
    let signed = SignedOperation::sign(&operation, &signer).expect("ordinary native signature");
    let ordinary = signed.verify().expect("ordinary native authentication");
    assert!(
        ordinary.validate_parents(&genesis, &[]).is_err(),
        "ordinary native Capture cannot use import ancestry"
    );
}

#[test]
fn imported_capture_with_nonempty_frontier_keeps_exact_native_ancestry() {
    let f = fixture();
    let (genesis, parent, identity) = original(&f);
    let mut child = parent.clone();
    child.parents.insert(parent.id().expect("parent operation"));
    let frontier = contract::frontier_digest(&wire::ImportFrontierV1 {
        format_version: 1,
        thread_id: child.thread.as_bytes().to_vec(),
        operation_ids: child
            .parents
            .iter()
            .map(|id| id.as_bytes().to_vec())
            .collect(),
    })
    .expect("nonempty frontier");
    // Use genuine owner/device signatures so these negatives reach ancestry,
    // rather than rejecting a forged or out-of-scope carrier first.
    let mut permission: wire::SignedImportMemberPermissionV1 = record(&f, "permission");
    let body = permission.body.as_mut().expect("permission body");
    body.scope
        .as_mut()
        .expect("scope")
        .branches
        .iter_mut()
        .find(|b| b.target_thread_id == child.thread.as_bytes())
        .expect("branch")
        .expected_frontier_digest = frontier.clone();
    permission.owner_signature = Some(sign_fixture(&f, "owner", contract::PERMISSION_DOMAIN, body));
    let mut certificate: wire::SignedImportJobDelegationV1 = record(&f, "delegation");
    let body = certificate.body.as_mut().expect("certificate body");
    body.scope
        .as_mut()
        .expect("scope")
        .branches
        .iter_mut()
        .find(|b| b.target_thread_id == child.thread.as_bytes())
        .expect("branch")
        .expected_frontier_digest = frontier.clone();
    body.branch_manifest
        .iter_mut()
        .find(|b| b.limit.as_ref().expect("limit").target_thread_id == child.thread.as_bytes())
        .expect("manifest branch")
        .limit
        .as_mut()
        .expect("limit")
        .expected_frontier_digest = frontier;
    body.parent_permission_digest =
        contract::signed_permission_digest(&permission).expect("permission digest");
    certificate.delegating_signature = Some(sign_fixture(
        &f,
        "device",
        contract::DELEGATION_DOMAIN,
        body,
    ));
    let chain = hex::decode(
        f["context"]["owner_chain_digest_hex"]
            .as_str()
            .expect("chain"),
    )
    .expect("hex");
    let owner_key = key(&f, "owner");
    let delegation = contract::verify_delegation(
        &certificate,
        Some(&permission),
        &contract::ImportOwnerExpectation {
            identity: &identity,
            owner_public_key: &owner_key,
            owner_chain_digest: &chain,
            authority_expires_at_seconds: 2000,
            now_unix_seconds: 1100,
            forbidden_job_keys: &[],
            known_job_associations: &[],
        },
    )
    .expect("authenticated nonempty-frontier scope");
    let sign_child = |operation: &ThreadOperation| {
        let mut signed = sign_import(&f, operation);
        let body = signed.body.as_mut().expect("operation body");
        body.delegation_digest = delegation.digest().to_vec();
        signed.job_signature = Some(sign_fixture(&f, "job", contract::OPERATION_DOMAIN, body));
        signed
    };
    let parent_state = state(&parent);
    let mut matching = parent_state.clone();
    matching.parents = vec![parent_state.id()];
    matching.intent = Some("native descendant".into());
    let matching = with_state(child.clone(), &matching);
    DelegatedImport::bind(
        &sign_child(&matching),
        &delegation,
        &genesis,
        &identity,
        &matching,
        std::slice::from_ref(&parent),
    )
    .expect("exact nonempty-frontier control");
    for parents in [
        Vec::new(),
        vec![genesis.base],
        vec![heddle_object_model::object::StateId::from_bytes([99; 32])],
    ] {
        let mut wrong = parent_state.clone();
        wrong.parents = parents;
        let wrong = with_state(child.clone(), &wrong);
        assert!(
            DelegatedImport::bind(
                &sign_child(&wrong),
                &delegation,
                &genesis,
                &identity,
                &wrong,
                std::slice::from_ref(&parent)
            )
            .is_err(),
            "nonempty frontier cannot drop or invent source ancestry"
        );
    }
}

#[test]
fn valid_import_carrier_cannot_unlock_noncanonical_genesis_base() {
    use crate::{Ed25519Signer, thread_operation::SignedGenesis};
    fn sign<T: hybrid_codec::Canonical>(
        f: &Value,
        role: &str,
        domain: &str,
        body: &T,
    ) -> wire::AuthorizationSignature {
        let signer = Ed25519Signer::from_seed(
            &hex::decode(f["keys"][role]["seed_hex"].as_str().expect("seed")).expect("hex"),
        )
        .expect("signer");
        wire::AuthorizationSignature {
            signer_key_id: hybrid_codec::key_id(signer.public_key()),
            signature: signer
                .sign(&hybrid_codec::signing_digest(domain, body).expect("digest"))
                .expect("signature"),
        }
    }
    let f = fixture();
    let (mut genesis, mut operation, identity) = original(&f);
    genesis.base = heddle_object_model::object::StateId::from_bytes([93; 32]);
    let device = Ed25519Signer::from_seed(
        &hex::decode(f["keys"]["device"]["seed_hex"].as_str().expect("seed")).expect("hex"),
    )
    .expect("device");
    let original_genesis = SignedGenesis::sign(&genesis, &device).expect("valid creator signature");
    original_genesis
        .verify()
        .expect("signed noncanonical genesis is structurally valid");
    let id = genesis.id().expect("Thread");
    operation.thread = id;
    let mut binding: wire::SignedImportGenesisAuthorityV1 = record(&f, "genesis_main");
    let body = binding.body.as_mut().expect("binding");
    body.genesis_digest = id.as_bytes().to_vec();
    body.original_creator_signature = original_genesis.signature;
    binding.creator_signature = Some(sign(&f, "device", contract::GENESIS_DOMAIN, body));
    let mut permission: wire::SignedImportMemberPermissionV1 = record(&f, "permission");
    let mut certificate: wire::SignedImportJobDelegationV1 = record(&f, "delegation");
    let body = certificate.body.as_mut().expect("certificate");
    let branch = body
        .scope
        .as_mut()
        .expect("scope")
        .branches
        .iter_mut()
        .find(|b| b.ref_name == "refs/heads/main")
        .expect("main");
    branch.genesis_digest = id.as_bytes().to_vec();
    branch.target_thread_id = id.as_bytes().to_vec();
    branch.expected_frontier_digest = contract::frontier_digest(&wire::ImportFrontierV1 {
        format_version: 1,
        thread_id: id.as_bytes().to_vec(),
        operation_ids: vec![],
    })
    .expect("frontier");
    let manifest = body
        .branch_manifest
        .iter_mut()
        .find(|b| b.limit.as_ref().expect("limit").ref_name == "refs/heads/main")
        .expect("main manifest");
    manifest.limit = Some(branch.clone());
    manifest.genesis_authority_digest =
        contract::signed_genesis_digest(&binding).expect("binding digest");
    permission.body.as_mut().expect("permission").scope = body.scope.clone();
    permission.owner_signature = Some(sign(
        &f,
        "owner",
        contract::PERMISSION_DOMAIN,
        permission.body.as_ref().expect("body"),
    ));
    body.parent_permission_digest =
        contract::signed_permission_digest(&permission).expect("parent");
    // Genesis authority binds the regenerated parent too.
    let gb = binding.body.as_mut().expect("binding");
    gb.parent_permission_digest = body.parent_permission_digest.clone();
    binding.creator_signature = Some(sign(&f, "device", contract::GENESIS_DOMAIN, gb));
    manifest.genesis_authority_digest = contract::signed_genesis_digest(&binding).expect("binding");
    certificate.delegating_signature = Some(sign(&f, "device", contract::DELEGATION_DOMAIN, body));
    let chain = hex::decode(
        f["context"]["owner_chain_digest_hex"]
            .as_str()
            .expect("chain"),
    )
    .expect("hex");
    let owner_key = key(&f, "owner");
    let verified = contract::verify_delegation(
        &certificate,
        Some(&permission),
        &contract::ImportOwnerExpectation {
            identity: &identity,
            owner_public_key: &owner_key,
            owner_chain_digest: &chain,
            authority_expires_at_seconds: 2000,
            now_unix_seconds: 1100,
            forbidden_job_keys: &[],
            known_job_associations: &[],
        },
    )
    .expect("genuinely signed exact carrier authority");
    let mut signed = sign_import(&f, &operation);
    let body = signed.body.as_mut().expect("operation");
    body.genesis_digest = id.as_bytes().to_vec();
    body.target_thread_id = id.as_bytes().to_vec();
    body.delegation_digest = contract::signed_delegation_digest(&certificate).expect("certificate");
    signed.job_signature = Some(sign(&f, "job", contract::OPERATION_DOMAIN, body));
    contract::verify_operation(&signed, &verified)
        .expect("all signed operation bindings valid before ancestry");
    assert!(
        DelegatedImport::bind(&signed, &verified, &genesis, &identity, &operation, &[]).is_err(),
        "a valid carrier must still require the canonical synthetic base"
    );
}

#[test]
fn frozen_old_parentless_capture_stays_strict_and_retired_import_kind_refuses() {
    let f: serde_json::Value = serde_json::from_str(include_str!(
        "../tests/fixtures/hybrid-native-old-parentless-v1.json"
    ))
    .expect("frozen API negative");
    let decode = |field: &str| hex::decode(f[field].as_str().expect("hex field")).expect("hex");
    let genesis = crate::thread_operation::SignedGenesis {
        canonical: decode("genesis_canonical_hex"),
        signature: decode("genesis_signature_hex"),
    }
    .verify()
    .expect("genuine frozen genesis signature");
    let record = wire::SignedRecord::decode(decode("capture").as_slice()).expect("record");
    let (_, operation) =
        verify_native_operation(&record).expect("genuine frozen operation signature");
    assert!(operation.parents.is_empty());
    assert!(state(&operation).parents.is_empty());
    assert!(
        operation.validate_parents(&genesis, &[]).is_err(),
        "frozen carrierless parentless Capture remains strict"
    );
    let legacy = wire::SignedRecord::decode(decode("legacy_wire_hex").as_slice())
        .expect("legacy signed wire");
    assert!(
        verify_native_operation(&legacy).is_err(),
        "retired HostedImport has no structural admission path"
    );
}
