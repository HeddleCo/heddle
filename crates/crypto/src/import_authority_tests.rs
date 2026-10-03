use api::hybrid_codec;
use prost::Message;
use serde_json::Value;

use super::*;

fn fixture() -> Value {
    serde_json::from_str(include_str!(
        "../tests/fixtures/import-authority-host-witness-v1.json"
    ))
    .expect("API fixed vectors")
}
fn record<T: Message + Default>(f: &Value, name: &str) -> T {
    let v = f["signed_vectors"]
        .get(name)
        .or_else(|| f["wire_vectors"].get(name))
        .expect("vector");
    hybrid_codec::strict_decode(
        &hex::decode(v["wire_hex"].as_str().expect("hex")).expect("bytes"),
        contract::MAX_BUNDLE_BYTES,
    )
    .expect("fixed protobuf")
}
fn key(f: &Value, name: &str) -> Vec<u8> {
    hex::decode(f["keys"][name]["public_key_hex"].as_str().expect("key")).expect("bytes")
}
fn trusted_set(f: &Value, name: &str, now: i64) -> VerifiedWitnessSet {
    witness_trust::verify_set(
        &record(f, name),
        &witness_trust::SetExpectation {
            authority: "https://weft.example.test",
            root_id: "descriptor-root-1",
            root_public_key: &key(f, "root"),
            root_epoch: 1,
            now_unix_millis: now,
            clock_floor_unix_millis: 1000000,
            known_job_keys: &[key(f, "job"), key(f, "renew_job")],
        },
        None,
    )
    .expect("root-authenticated set")
}
fn delegation(f: &Value, now: i64) -> VerifiedImportDelegation {
    use heddleco_capability_verifier::{
        self as verifier,
        import_delegation::{CurrentContext, Selection},
    };
    let h: wire::OwnerHistory = record(f, "owner_history");
    let owner = verifier::verify_owner_root(h.root.as_ref().expect("root")).expect("owner");
    let genesis: wire::SignedSpoolOwnerGenesis = record(f, "spool_owner_genesis");
    let digest =
        verifier::creation::spool_genesis_digest(genesis.genesis.as_ref().expect("genesis"))
            .expect("digest");
    let limits = verifier::VerificationLimits::new(3600).expect("limits");
    let ring = verifier::verify_clone_keyring(
        wire::CloneAuthorizationKeyring {
            format_version: 1,
            spool_uuid: genesis.genesis.as_ref().expect("body").spool_uuid.clone(),
            canonical_spool_path_segments: vec!["example".into()],
            owner_genesis: Some(genesis),
            owner_root: h.root,
            accepted_state_hash: owner.state_hash().to_vec(),
            pin: Some(wire::CloneOwnerPin {
                kind: 2,
                expected_owner_id: owner.owner_id().to_vec(),
                first_seen_unix_seconds: 1000,
            }),
            ..Default::default()
        },
        now,
        limits,
        &[],
    )
    .expect("keyring");
    let c = CurrentContext {
        selection: Selection {
            spool_genesis_digest: &digest,
            initial_owner_id: &owner.owner_id(),
            owner: &owner,
            keyring: &ring,
            limits,
        },
        now,
        forbidden_job_keys: &[key(f, "witness"), key(f, "root")],
        known_job_associations: &[],
    };
    verifier::import_delegation::verify_current(
        &record(f, "delegation"),
        Some(&record(f, "permission")),
        &c,
        |_| false,
    )
    .expect("portable authority")
}

#[test]
fn root_authentication_and_semantic_sets_have_independent_controls() {
    let f = fixture();
    let root = key(&f, "root");
    let jobs = vec![key(&f, "job")];
    let expected = witness_trust::SetExpectation {
        authority: "https://weft.example.test",
        root_id: "descriptor-root-1",
        root_public_key: &root,
        root_epoch: 1,
        now_unix_millis: 1100000,
        clock_floor_unix_millis: 1000000,
        known_job_keys: &jobs,
    };
    witness_trust::verify_set(&record(&f, "current_set"), &expected, None).expect("root control");
    assert_eq!(
        witness_trust::verify_set(&record(&f, "wrong_root_set"), &expected, None),
        Err(Reject::Signature)
    );
    assert_eq!(
        witness_trust::verify_set(&record(&f, "semantic_invalid_set"), &expected, None),
        Err(Reject::Semantic)
    );
    assert_eq!(
        witness_trust::verify_set(&record(&f, "job_witness_set"), &expected, None),
        Err(Reject::JobAsWitness)
    );
    assert_eq!(
        witness_trust::verify_set(&record(&f, "wrong_role_set"), &expected, None),
        Err(Reject::JobAsWitness)
    );
    let mut altered: host::SignedHostedWitnessSetV1 = record(&f, "current_set");
    altered.body.as_mut().expect("body").generation += 1;
    assert_eq!(
        witness_trust::verify_set(&altered, &expected, None),
        Err(Reject::Signature)
    );
}

#[test]
fn retirement_backdating_requires_the_exact_sealed_statement() {
    let f = fixture();
    let set = trusted_set(&f, "retired_set", 1350000);
    let original = record(&f, "publication_statement");
    let proof = record(&f, "publication_proof");
    WitnessEvidence::resolve(&set, &original, Some(&proof), false, 1350000)
        .expect("sealed exact control");
    let backdated = record(&f, "backdated_new_statement");
    assert!(matches!(
        WitnessEvidence::resolve(&set, &backdated, Some(&proof), false, 1350000),
        Err(Error::Contract(Reject::Proof))
    ));
    assert!(matches!(
        WitnessEvidence::resolve(&set, &original, None, false, 1350000),
        Err(Error::Contract(Reject::Proof))
    ));
    assert!(matches!(
        WitnessEvidence::resolve(&set, &original, Some(&proof), true, 1350000),
        Err(Error::Contract(Reject::Expired))
    ));
}

#[test]
fn revocation_vs_retirement_rejects_identical_original_and_cached_context() {
    let f = fixture();
    let retired = trusted_set(&f, "retired_set", 1350000);
    let revoked = trusted_set(&f, "revoked_set", 1350000);
    let original = record(&f, "publication_statement");
    let proof = record(&f, "publication_proof");
    let evidence = WitnessEvidence::resolve(&retired, &original, Some(&proof), false, 1350000)
        .expect("retired control");
    assert!(matches!(
        WitnessEvidence::resolve(&revoked, &original, Some(&proof), false, 1350000),
        Err(Error::Contract(Reject::Revoked))
    ));
    assert!(matches!(
        evidence.recheck(&revoked, 1350000),
        Err(Error::Contract(Reject::StaleContext))
    ));
    assert!(matches!(
        evidence.recheck(&retired, 1700000),
        Err(Error::Contract(Reject::Expired))
    ));
}

#[test]
fn proof_substitution_shape_and_bounds_reject_before_native_authority() {
    let f = fixture();
    let set = trusted_set(&f, "retired_set", 1350000);
    let s = record(&f, "publication_statement");
    let p: host::HostedWitnessHistoryProofV1 = record(&f, "publication_proof");
    WitnessEvidence::resolve(&set, &s, Some(&p), false, 1350000).expect("proof control");
    for field in 0..6 {
        let mut p = p.clone();
        match field {
            0 => p.purpose = 1,
            1 => p.leaf_index = p.leaf_count,
            2 => p.leaf_count += 1,
            3 => {
                p.siblings.pop();
            }
            4 => p.executor_id[0] ^= 1,
            _ => p.siblings.push(vec![0; 32]),
        };
        assert!(matches!(
            WitnessEvidence::resolve(&set, &s, Some(&p), false, 1350000),
            Err(Error::Contract(Reject::Proof))
        ));
    }
    let mut big = p.clone();
    big.siblings = vec![vec![0; 32]; 65];
    assert!(matches!(
        WitnessEvidence::resolve(&set, &s, Some(&big), false, 1350000),
        Err(Error::Contract(Reject::Bounds))
    ));
}

#[test]
fn canonical_wire_and_size_limits_reject_before_any_trusted_installation() {
    let f = fixture();
    let bundle: wire::ImportPublicProofBundleV1 = record(&f, "complete_renewed_export");
    let mut bytes = bundle.encode_to_vec();
    let decoded: wire::ImportPublicProofBundleV1 =
        hybrid_codec::strict_decode(&bytes, contract::MAX_BUNDLE_BYTES)
            .expect("canonical bounded control");
    assert_eq!(decoded, bundle);
    bytes.extend_from_slice(&[0xf8, 0x07, 0x01]);
    assert_eq!(
        hybrid_codec::strict_decode::<wire::ImportPublicProofBundleV1>(
            &bytes,
            contract::MAX_BUNDLE_BYTES
        ),
        Err(Reject::Canonical)
    );
    assert_eq!(
        hybrid_codec::strict_decode::<wire::ImportPublicProofBundleV1>(
            &vec![0; contract::MAX_BUNDLE_BYTES + 1],
            contract::MAX_BUNDLE_BYTES
        ),
        Err(Reject::Bounds)
    );
}

#[test]
fn job_content_and_publication_have_separate_signatures_and_native_bindings() {
    let f = fixture();
    let d = delegation(&f, 1100);
    let b: wire::ImportPublicProofBundleV1 = record(&f, "complete_renewed_export");
    let p: wire::ImportGenesisWitnessV1 = record(&f, "genesis_payload");
    verify_native_genesis(p.original_genesis.as_ref().expect("genesis"))
        .expect("native genesis signature control");
    let genesis_set = trusted_set(&f, "retired_set", 1350000);
    let genesis_evidence = WitnessEvidence::resolve(
        &genesis_set,
        &record(&f, "genesis_admission"),
        Some(&record(&f, "genesis_proof")),
        false,
        1350000,
    )
    .expect("original creation witness");
    let original = verify_genesis_payload(&p, &genesis_evidence, &d, |_| false)
        .expect("original portable creation authority");
    let converted: wire::SignedRecord = record(&f, "converted_main");
    let content = verify_delegated_import(
        &record(&f, "operation_main"),
        &d,
        &d,
        &original,
        &converted,
        &[],
    )
    .expect("scoped converted content");
    // An otherwise genuine API job signature cannot promote a witness key
    // into the native converter role. Keep content, scope and signatures valid.
    let (_, mut native) = verify_native_operation(&converted).expect("native converter control");
    native.publisher = key(&f, "witness").try_into().expect("witness key");
    let witness_seed =
        hex::decode(f["keys"]["witness"]["seed_hex"].as_str().expect("seed")).expect("bytes");
    let witness_signer = crate::Ed25519Signer::from_seed(&witness_seed).expect("witness signer");
    let witnessed_native = SignedOperation::sign(&native, &witness_signer)
        .expect("genuine wrong-role native signature");
    let wrong_role = wire::SignedRecord {
        format: converted.format.clone(),
        canonical_record: witnessed_native.canonical,
        signatures: vec![wire::RecordSignature {
            public_key: native.publisher.to_vec(),
            signature: witnessed_native.signature,
        }],
    };
    let mut job: wire::SignedDelegatedImportOperationV1 = record(&f, "operation_main");
    let body = job.body.as_mut().expect("body");
    body.resulting_frontier_digest = contract::frontier_digest(&wire::ImportFrontierV1 {
        format_version: 1,
        thread_id: native.thread.as_bytes().to_vec(),
        operation_ids: vec![native.id().expect("id").as_bytes().to_vec()],
    })
    .expect("exact frontier");
    use crate::Signer;
    let job_seed =
        hex::decode(f["keys"]["job"]["seed_hex"].as_str().expect("seed")).expect("bytes");
    job.job_signature = Some(wire::AuthorizationSignature {
        signer_key_id: hybrid_codec::key_id(&key(&f, "job")),
        signature: crate::Ed25519Signer::from_seed(&job_seed)
            .expect("job signer")
            .sign(
                &hybrid_codec::signing_digest(contract::OPERATION_DOMAIN, body)
                    .expect("exact API domain"),
            )
            .expect("genuine job signature"),
    });
    contract::verify_operation(&job, d.scope())
        .expect("valid job scope/signature surrounding control");
    assert!(matches!(
        verify_delegated_import(&job, &d, &d, &original, &wrong_role, &[]),
        Err(Error::Contract(Reject::KeyRole))
    ));
    let set = trusted_set(&f, "retired_set", 1350000);
    let evidence = WitnessEvidence::resolve(
        &set,
        &record(&f, "publication_statement"),
        Some(&record(&f, "publication_proof")),
        false,
        1350000,
    )
    .expect("publication receipt");
    verify_publication(
        &content,
        &d,
        &record(&f, "partial_manifest"),
        &evidence,
        &set,
        1350000,
    )
    .expect("exact publication control");
    assert!(matches!(
        verify_publication(
            &content,
            &d,
            b.terminal_manifest.as_ref().expect("later snapshot"),
            &evidence,
            &set,
            1350000
        ),
        Err(Error::Contract(Reject::Scope))
    ));
    assert!(matches!(
        verify_delegated_import(
            &record(&f, "scope_violation"),
            &d,
            &d,
            &original,
            &converted,
            &[]
        )
        , Err(Error::Object(heddle_object_model::error::HeddleError::InvalidObject(reason))) if reason == Reject::Scope.to_string()
    ));
    let bad: wire::SignedDelegatedImportOperationV1 = record(&f, "witness_without_job");
    assert!(matches!(
        contract::verify_operation(&bad, d.scope()),
        Err(Reject::Signature)
    ));
    let mut altered = converted.clone();
    altered.signatures[0].signature[0] ^= 1;
    assert!(matches!(
        verify_delegated_import(
            &record(&f, "operation_main"),
            &d,
            &d,
            &original,
            &altered,
            &[]
        ),
        Err(Error::Native(crate::thread_operation::Error::Signature(_)))
    ));
}

#[test]
fn genesis_original_owner_and_exact_envelope_remain_mandatory() {
    let f = fixture();
    let d = delegation(&f, 1100);
    let set = trusted_set(&f, "retired_set", 1350000);
    let s = record(&f, "genesis_admission");
    let proof = record(&f, "genesis_proof");
    let evidence =
        WitnessEvidence::resolve(&set, &s, Some(&proof), false, 1350000).expect("genesis witness");
    let payload: wire::ImportGenesisWitnessV1 = record(&f, "genesis_payload");
    verify_genesis_payload(&payload, &evidence, &d, |_| false)
        .expect("independent native genesis control");
    let mut changed = payload.clone();
    changed.creator_authority_envelope.push(0);
    assert!(matches!(
        verify_genesis_payload(&changed, &evidence, &d, |_| false),
        Err(Error::Contract(Reject::Scope))
    ));
    let missing: wire::ImportAuthorityWitnessV1 = record(&f, "missing_owner_payload");
    let genuine = record(&f, "witness_without_owner");
    let current = trusted_set(&f, "current_set", 1100000);
    let genuine = WitnessEvidence::resolve(&current, &genuine, None, false, 1100000)
        .expect("witness signature alone valid");
    assert_eq!(
        contract::verify_witness_payload(
            genuine.signed().body.as_ref().expect("body"),
            WitnessPayload::Authority(&missing)
        ),
        Err(Reject::Signature)
    );
}

#[test]
fn native_authority_ownership_and_landing_preserve_original_closure() {
    let f = fixture();
    let set = trusted_set(&f, "retired_set", 1350000);
    let b: wire::ImportPublicProofBundleV1 = record(&f, "complete_renewed_export");
    let mut originals = b.original_geneses.clone();
    originals.extend([record(&f, "converted_main"), record(&f, "converted_dev")]);
    for name in [
        "authority_admission_payload",
        "ownership_admission_payload",
        "resolution_admission_payload",
    ] {
        let p: wire::ImportAuthorityWitnessV1 = record(&f, name);
        originals.extend(p.original.iter().cloned());
        originals.extend(p.dependencies);
    }
    let landing: wire::HostedLandingWitnessV1 = record(&f, "landing_payload");
    originals.extend(landing.execution.iter().cloned());
    originals.extend(landing.source_operation.iter().cloned());
    originals.extend(landing.review_evidence.iter().cloned());
    let closure = NativeClosure::verify(&originals).expect("native causal and claim closure");
    let history: wire::OwnerHistory = record(&f, "owner_history");
    let owner =
        heddleco_capability_verifier::verify_owner_root(history.root.as_ref().expect("root"))
            .expect("native owner");
    let identity: wire::ImportIdentityV1 = record(&f, "identity");
    let digest = identity
        .spool_genesis_digest
        .as_slice()
        .try_into()
        .expect("Spool genesis");
    let context = NativeAuthorityContext {
        owner: &owner,
        spool_uuid: uuid::Uuid::from_slice(&identity.spool_uuid).expect("Spool"),
        spool_genesis: &digest,
        transfer_sequence: 0,
        spool_path: "example",
    };
    for (name, statement, proof) in [
        (
            "authority_admission_payload",
            "authority_admission",
            "authority_proof",
        ),
        (
            "ownership_admission_payload",
            "ownership_admission",
            "ownership_proof",
        ),
        (
            "resolution_admission_payload",
            "resolution_admission",
            "resolution_proof",
        ),
    ] {
        let p = record(&f, name);
        let evidence = WitnessEvidence::resolve(
            &set,
            &record(&f, statement),
            Some(&record(&f, proof)),
            false,
            1350000,
        )
        .expect("exact authority witness");
        verify_authority_payload(&p, &evidence, &closure, &context, |_| false)
            .expect("independent original authority");
        assert!(
            matches!(verify_authority_payload(&p, &evidence, &closure, &context, |_| true), Err(Error::Authority(heddleco_capability_verifier::Error::Invalid(reason))) if reason == "original Thread mint root or publisher is revoked")
        );
    }
    let evidence = WitnessEvidence::resolve(
        &set,
        &record(&f, "landing_statement"),
        Some(&record(&f, "landing_proof")),
        false,
        1350000,
    )
    .expect("exact landing witness");
    verify_landing_payload(&landing, &evidence, &closure, &context, |_| false)
        .expect("independent landing request/source/review");
    let mut bad = landing.clone();
    bad.request
        .as_mut()
        .expect("request")
        .signature
        .as_mut()
        .expect("signature")
        .signature =
        record::<host::SignedHostedWitnessStatementV1>(&f, "landing_statement").signature;
    assert!(matches!(
        verify_landing_payload(&bad, &evidence, &closure, &context, |_| false),
        Err(Error::Contract(Reject::Signature))
    ));
    let mut incomplete = originals.clone();
    let removal = incomplete
        .iter()
        .position(|r| r.format == heddle_object_model::object::thread_replication::GENESIS_FORMAT)
        .expect("genesis dependency");
    let removed = incomplete.remove(removal);
    incomplete.retain(|r| r != &removed);
    assert!(matches!(
        NativeClosure::verify(&incomplete),
        Err(Error::Contract(Reject::Scope))
    ));
}

fn resign_set(f: &Value, set: &mut host::SignedHostedWitnessSetV1) {
    use crate::{Ed25519Signer, Signer};
    let seed = hex::decode(f["keys"]["root"]["seed_hex"].as_str().expect("seed")).expect("bytes");
    let bytes =
        witness_trust::set_signing_bytes(set.body.as_ref().expect("body")).expect("preimage");
    set.body_digest = hybrid_codec::hash(&[&bytes]);
    set.root_signature = Ed25519Signer::from_seed(&seed)
        .expect("signer")
        .sign(&bytes)
        .expect("malformed set signature");
}
#[test]
fn genuine_root_signed_malformed_sets_and_unattested_witnesses() {
    let f = fixture();
    let root = key(&f, "root");
    let expected = witness_trust::SetExpectation {
        authority: "https://weft.example.test",
        root_id: "descriptor-root-1",
        root_public_key: &root,
        root_epoch: 1,
        now_unix_millis: 1350000,
        clock_floor_unix_millis: 1000000,
        known_job_keys: &[],
    };
    let original: host::SignedHostedWitnessSetV1 = record(&f, "retired_set");
    witness_trust::verify_set(&original, &expected, None).expect("root semantic control");
    for field in 0..7 {
        let mut bad = original.clone();
        let b = bad.body.as_mut().expect("body");
        match field {
            0 => b.entries.push(b.entries[0].clone()),
            1 => b.current_executor_id[0] ^= 1,
            2 => b.entries.reverse(),
            3 => b.entries[0].purposes = vec![1, 2, 3, 99],
            4 => b.entries[0].active_until_unix_millis = b.entries[0].active_from_unix_millis,
            5 => b
                .entries
                .iter_mut()
                .find(|e| e.state == 2)
                .expect("retired")
                .archive_root
                .clear(),
            _ => b.valid_until_unix_millis = b.issued_at_unix_millis + 300001,
        };
        resign_set(&f, &mut bad);
        assert_eq!(
            witness_trust::verify_set(&bad, &expected, None),
            Err(if field == 5 {
                Reject::Canonical
            } else {
                Reject::Semantic
            }),
            "malformed set {field}"
        );
    }
    let set = trusted_set(&f, "current_set", 1100000);
    let original: host::SignedHostedWitnessStatementV1 = record(&f, "publication_statement");
    WitnessEvidence::resolve(&set, &original, None, false, 1100000).expect("attested control");
    let mut absent = original.clone();
    absent.body.as_mut().expect("body").executor_id =
        witness_trust::witness_id(&key(&f, "wrong_root"));
    use crate::{Ed25519Signer, Signer};
    let seed =
        hex::decode(f["keys"]["wrong_root"]["seed_hex"].as_str().expect("seed")).expect("bytes");
    absent.signature = Ed25519Signer::from_seed(&seed)
        .expect("key")
        .sign(
            &witness_trust::statement_signing_digest(absent.body.as_ref().expect("body"))
                .expect("digest"),
        )
        .expect("genuine absent signature");
    assert!(matches!(
        WitnessEvidence::resolve(&set, &absent, None, false, 1100000),
        Err(Error::Contract(Reject::Root))
    ));
}
#[test]
fn boundary_acceptance_requires_api_318_even_with_valid_current_witness() {
    let f = fixture();
    let set = trusted_set(&f, "current_set", 1100000);
    let original: host::SignedHostedWitnessStatementV1 = record(&f, "genesis_admission");
    WitnessEvidence::resolve(&set, &original, None, false, 1100000)
        .expect("original authority control");
    let mut boundary = original;
    boundary.body.as_mut().expect("body").basis = 2;
    use crate::{Ed25519Signer, Signer};
    let seed =
        hex::decode(f["keys"]["witness"]["seed_hex"].as_str().expect("seed")).expect("bytes");
    boundary.signature = Ed25519Signer::from_seed(&seed)
        .expect("witness")
        .sign(
            &witness_trust::statement_signing_digest(boundary.body.as_ref().expect("body"))
                .expect("preimage"),
        )
        .expect("valid witness");
    assert!(matches!(
        WitnessEvidence::resolve(&set, &boundary, None, false, 1100000),
        Err(Error::BoundaryAcceptancePendingApi318)
    ));
}
