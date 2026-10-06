use api::hybrid_codec;
use prost::Message;
use serde_json::Value;

use super::*;
use crate::Signer;

#[path = "import_ancestry_tests.rs"]
mod ancestry;

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
            known_job_keys: &[key(f, "job"), key(f, "device")],
        },
        None,
    )
    .expect("root-authenticated set")
}
fn delegation(f: &Value, now: i64) -> VerifiedImportDelegation {
    delegation_record(f, now, &record(f, "delegation"))
}
fn delegation_record(
    f: &Value,
    now: i64,
    signed: &wire::SignedImportJobDelegationV1,
) -> VerifiedImportDelegation {
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
        now_millis: now * 1000,
        forbidden_job_keys: &[key(f, "witness"), key(f, "root")],
        known_job_associations: &[],
    };
    verifier::import_delegation::verify_current(signed, Some(&record(f, "permission")), &c, |_| {
        false
    })
    .expect("portable authority")
}

fn fixture_carriers(f: &Value) -> VerifiedImportCarriers {
    let original = delegation(f, 1100);
    VerifiedImportCarriers::new(record(f, "complete_export"), original.scope().clone())
        .expect("signed import carriers")
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
    let bundle: wire::ImportPublicProofBundleV1 = record(&f, "complete_export");
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
    let b: wire::ImportPublicProofBundleV1 = record(&f, "complete_export");
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
    let original = verify_genesis_payload(&p, &genesis_evidence, &d, &[], |_| false)
        .expect("original portable creation authority");
    let converted: wire::SignedRecord = record(&f, "converted_main");
    let content = verify_delegated_import(
        &record(&f, "operation_main"),
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
        verify_delegated_import(&job, &d, &original, &wrong_role, &[]),
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
        verify_delegated_import(&record(&f, "operation_main"), &d, &original, &altered, &[]),
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
    verify_genesis_payload(&payload, &evidence, &d, &[], |_| false)
        .expect("independent native genesis control");
    let mut changed = payload.clone();
    let mut other: wire::SignedImportMemberPermissionV1 = record(&f, "permission");
    other.body.as_mut().expect("permission").cancellation_id = vec![99; 32];
    other.owner_signature = Some(sign_fixture(
        &f,
        "owner",
        contract::PERMISSION_DOMAIN,
        other.body.as_ref().expect("body"),
    ));
    changed.creator_authority_envelope = b"heddle-signed-import-member-permission-v1\0".to_vec();
    changed
        .creator_authority_envelope
        .extend(hybrid_codec::canonical(&other).expect("other authentic permission"));
    let binding = changed.binding.as_mut().expect("binding");
    let body = binding.body.as_mut().expect("body");
    body.creator_authority_envelope_digest =
        hybrid_codec::hash(&[&changed.creator_authority_envelope]);
    binding.creator_signature = Some(sign_fixture(&f, "device", contract::GENESIS_DOMAIN, body));
    let mut certificate: wire::SignedImportJobDelegationV1 = record(&f, "delegation");
    let body = certificate.body.as_mut().expect("body");
    let id = binding.body.as_ref().expect("body").genesis_digest.clone();
    body.branch_manifest
        .iter_mut()
        .find(|m| m.limit.as_ref().is_some_and(|l| l.genesis_digest == id))
        .expect("branch")
        .genesis_authority_digest =
        contract::signed_genesis_digest(binding).expect("binding digest");
    certificate.delegating_signature = Some(sign_fixture(
        &f,
        "device",
        contract::DELEGATION_DOMAIN,
        body,
    ));
    let verified = delegation_record(&f, 1100, &certificate);
    let binding_digest = contract::signed_genesis_digest(binding).expect("binding digest");
    let mut statement: host::SignedHostedWitnessStatementV1 = record(&f, "genesis_admission");
    let body = statement.body.as_mut().expect("body");
    body.canonical_payload = hybrid_codec::canonical(&changed).expect("payload");
    body.authority_digest = binding_digest;
    statement.signature = fixture_signer(&f, "witness")
        .sign(&witness_trust::statement_signing_digest(body).expect("statement digest"))
        .expect("authentic witness");
    let current = trusted_set(&f, "current_set", 1100000);
    let changed_evidence = WitnessEvidence::resolve(&current, &statement, None, false, 1100000)
        .expect("genuine surrounding witness");
    contract::verify_witness_payload(
        statement.body.as_ref().expect("body"),
        WitnessPayload::Genesis(&changed),
    )
    .expect("all other commitments and signatures match");
    assert!(matches!(
        verify_genesis_payload(&changed, &changed_evidence, &verified, &[], |_| false),
        Err(Error::Contract(Reject::ImportPermission))
    ));
    verify_genesis_payload(&payload, &evidence, &d, &[], |_| false)
        .expect("unchanged exact envelope control");
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
    let b: wire::ImportPublicProofBundleV1 = record(&f, "complete_export");
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
    assert!(
        NativeClosure::verify(&originals).is_err(),
        "ordinary native closure cannot claim import ancestry"
    );
    let imports = fixture_carriers(&f);
    let mut closure =
        NativeClosure::verify_with_imports(&originals, &[], |g, o, p| imports.bind(g, o, p))
            .expect("authenticated import causal and claim closure");
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
    let author = crate::writer_authority::HostAuthorAuthority {
        owner: &owner,
        mint_roots: &[],
    };
    let context = NativeAuthorityContext {
        author_authority: &author,
        owner: &owner,
        spool_uuid: uuid::Uuid::from_slice(&identity.spool_uuid).expect("Spool"),
        spool_genesis: &digest,
        transfer_sequence: 0,
        spool_path: "example",
        witness_set: &set,
        original_geneses: &OriginalGeneses::import(&b.genesis_witnesses)
            .expect("genesis envelopes"),
        known_job_associations: &[],
        forbidden_authority_keys: &[key(&f, "root"), key(&f, "witness"), key(&f, "next_witness")],
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
        verify_authority_payload(&p, &evidence, &mut closure, &context, |_| false)
            .expect("independent original authority");
        assert!(
            matches!(verify_authority_payload(&p, &evidence, &mut closure, &context, |_| true), Err(Error::Authority(heddleco_capability_verifier::Error::Invalid(reason))) if reason == "original Thread mint root or publisher is revoked")
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
    assert!(
        matches!(
            verify_landing_payload(&landing, &evidence, &mut closure, &context, |_| false),
            Err(Error::Contract(Reject::Scope))
        ),
        "P4 cannot replace a Review's own P2"
    );
    // This payload-only corpus predates complete P2 carriers for P4 Reviews.
    // Verify each exact original at its own independently signed admission.
    for original in landing
        .source_operation
        .iter()
        .chain(&landing.review_evidence)
    {
        let operation = verify_native_operation(original)
            .expect("native signature")
            .1;
        let authority = match &operation.body {
            heddle_object_model::object::thread_replication::ThreadOperationBody::Metadata(
                bytes,
            ) => {
                heddle_object_model::object::thread_replication::metadata::ThreadControl::decode(
                    bytes,
                )
                .expect("control")
                .authority_envelope
            }
            _ => match operation.source_author().expect("author") {
                Some(heddle_object_model::object::thread_replication::SourceAuthor::Account {
                    authority,
                    ..
                }) => authority,
                _ => continue,
            },
        };
        let payload = wire::ImportAuthorityWitnessV1 {
            format_version: 1,
            kind: 1,
            original: Some(original.clone()),
            authority_envelope: authority,
            dependencies: vec![],
            boundary_acceptances: vec![],
        };
        let mut signed: host::SignedHostedWitnessStatementV1 = record(&f, "authority_admission");
        let s = signed.body.as_mut().expect("body");
        s.executor_id = set
            .body()
            .entries
            .iter()
            .find(|e| e.state == 1)
            .expect("current executor")
            .executor_id
            .clone();
        s.admission_order = evidence
            .signed()
            .body
            .as_ref()
            .expect("P4 body")
            .admission_order
            - 1;
        s.observed_at_unix_millis = 1_300_000;
        s.canonical_payload = hybrid_codec::canonical(&payload).expect("payload");
        s.publisher_key_id = hybrid_codec::key_id(&operation.publisher);
        s.authority_digest = hybrid_codec::hash(&[
            b"heddle-hosted-authority-envelope-v1",
            &(payload.authority_envelope.len() as u32).to_be_bytes(),
            &payload.authority_envelope,
        ]);
        let mut framed = (original.signatures.len() as u32).to_be_bytes().to_vec();
        for signature in &original.signatures {
            framed.extend(hybrid_codec::canonical(signature).expect("signature"));
        }
        s.original_signatures_digest =
            hybrid_codec::hash(&[b"heddle-hosted-original-signatures-v1", &framed]);
        let signer = crate::Ed25519Signer::from_seed(
            &hex::decode(
                f["keys"]["next_witness"]["seed_hex"]
                    .as_str()
                    .expect("seed"),
            )
            .expect("hex"),
        )
        .expect("witness");
        signed.signature = signer
            .sign(&witness_trust::statement_signing_digest(s).expect("digest"))
            .expect("signature");
        let admission = WitnessEvidence::resolve(&set, &signed, None, false, 1_350_000)
            .expect("own P2 testimony");
        verify_authority_payload(&payload, &admission, &mut closure, &context, |_| false)
            .expect("own source/Review P2");
    }
    verify_landing_payload(&landing, &evidence, &mut closure, &context, |_| false)
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
        verify_landing_payload(&bad, &evidence, &mut closure, &context, |_| false),
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
fn review_alpha20_boundary_vectors_resolve_exact_originals() {
    let f = fixture();
    let set = trusted_set(&f, "current_set", 1100000);
    for v in f["boundary_vectors"]["passing"]
        .as_array()
        .expect("vectors")
    {
        let statement = record(&f, v["statement"].as_str().expect("statement"));
        WitnessEvidence::resolve(&set, &statement, None, false, 1100000)
            .expect("published boundary control");
    }
}

fn boundary_vector(
    f: &Value,
    statement_name: &str,
    payload_name: &str,
    kind: &str,
    retired: bool,
) -> Result<()> {
    let now = if retired { 1350000 } else { 1100000 };
    let set = trusted_set(
        f,
        if retired {
            "retired_set"
        } else {
            "current_set"
        },
        now,
    );
    let statement = record(f, statement_name);
    let proof_name = statement_name.replace("_statement", "_proof");
    let proof = retired.then(|| record(f, &proof_name));
    let evidence = WitnessEvidence::resolve(&set, &statement, proof.as_ref(), false, now)?;
    let b: wire::ImportPublicProofBundleV1 = record(f, "complete_export");
    let base: wire::ImportAuthorityWitnessV1 = record(f, "authority_admission_payload");
    let mut originals = b.original_geneses.clone();
    originals.extend(base.original);
    originals.extend(base.dependencies);
    let mut genesis_payloads = b.genesis_witnesses.clone();
    let (genesis, authority, boundaries) = if kind == "genesis" {
        let p: wire::ImportGenesisWitnessV1 = record(f, payload_name);
        originals.extend(p.original_genesis.clone());
        genesis_payloads.retain(|g| g.original_genesis != p.original_genesis);
        genesis_payloads.push(p.clone());
        let boundaries = p.boundary_acceptance.iter().cloned().collect::<Vec<_>>();
        (Some(p), None, boundaries)
    } else {
        let p: wire::ImportAuthorityWitnessV1 = record(f, payload_name);
        originals.extend(p.original.clone());
        originals.extend(p.dependencies.clone());
        let boundaries = p.boundary_acceptances.clone();
        (None, Some(p), boundaries)
    };
    let mut closure = NativeClosure::verify_with_boundaries(&originals, &boundaries)?;
    let h: wire::OwnerHistory = record(f, "owner_history");
    let owner = heddleco_capability_verifier::verify_owner_root(h.root.as_ref().expect("root"))
        .expect("owner");
    let identity: wire::ImportIdentityV1 = record(f, "identity");
    let digest = identity
        .spool_genesis_digest
        .as_slice()
        .try_into()
        .expect("digest");
    let author = crate::writer_authority::HostAuthorAuthority {
        owner: &owner,
        mint_roots: &[],
    };
    let context = NativeAuthorityContext {
        author_authority: &author,
        owner: &owner,
        spool_uuid: uuid::Uuid::from_slice(&identity.spool_uuid).expect("Spool"),
        spool_genesis: &digest,
        transfer_sequence: 0,
        spool_path: "example",
        witness_set: &set,
        original_geneses: &OriginalGeneses::import(&genesis_payloads).expect("genesis envelopes"),
        known_job_associations: &[],
        forbidden_authority_keys: &[key(f, "root"), key(f, "witness"), key(f, "next_witness")],
    };
    if let Some(p) = genesis {
        verify_genesis_payload_at_boundary(
            &p,
            &evidence,
            &delegation(f, 1100),
            &closure,
            &context,
            |_| false,
        )?;
    } else {
        verify_authority_payload(
            authority.as_ref().expect("payload"),
            &evidence,
            &mut closure,
            &context,
            |_| false,
        )?;
    }
    Ok(())
}
#[test]
fn review_boundary_native_selection_and_substitution_gates() {
    let f = fixture();
    for v in f["boundary_vectors"]["passing"]
        .as_array()
        .expect("passing")
    {
        for retired in [false, true] {
            boundary_vector(
                &f,
                v["statement"].as_str().expect("statement"),
                v["payload"].as_str().expect("payload"),
                v["kind"].as_str().expect("kind"),
                retired,
            )
            .expect("exact boundary control");
        }
    }
    for v in f["boundary_vectors"]["negative"]
        .as_array()
        .expect("negatives")
    {
        assert!(
            matches!(
                boundary_vector(
                    &f,
                    v["statement"].as_str().expect("statement"),
                    v["payload"].as_str().expect("payload"),
                    "genesis",
                    false
                ),
                Err(Error::Contract(Reject::BoundaryAcceptance))
            ),
            "{}",
            v["name"]
        );
        boundary_vector(
            &f,
            v["control_statement"].as_str().expect("control"),
            v["control_payload"].as_str().expect("control"),
            "genesis",
            false,
        )
        .expect("neighbor control");
    }
    assert!(matches!(
        boundary_vector(
            &f,
            "boundary_dependency_missing_statement",
            "boundary_dependency_missing_payload",
            "authority",
            false
        ),
        Err(Error::Contract(Reject::BoundaryAcceptance))
    ));
    for v in f["boundary_vectors"]["native_negative"]
        .as_array()
        .expect("native negatives")
    {
        for retired in [false, true] {
            assert!(
                matches!(
                    boundary_vector(
                        &f,
                        v["statement"].as_str().expect("statement"),
                        v["payload"].as_str().expect("payload"),
                        v["kind"].as_str().expect("kind"),
                        retired
                    ),
                    Err(Error::Contract(Reject::BoundaryAcceptance))
                ),
                "{}",
                v["name"]
            );
        }
    }
    boundary_vector(
        &f,
        "boundary_complete_2_statement",
        "boundary_complete_2_payload",
        "genesis",
        false,
    )
    .expect("complete selection control");
    boundary_vector(
        &f,
        "boundary_multiple_dependencies_statement",
        "boundary_multiple_dependencies_payload",
        "authority",
        false,
    )
    .expect("multiple acceptance control");
}
#[test]
fn review_published_empty_single_even_odd_proofs_isolate_shape() {
    let f = fixture();
    for tree in f["trees"].as_array().expect("trees") {
        let bytes = |v: &Value| hex::decode(v.as_str().expect("hex")).expect("bytes");
        let leaves = tree["leaves_hex"]
            .as_array()
            .expect("leaves")
            .iter()
            .map(bytes)
            .collect::<Vec<_>>();
        assert_eq!(
            witness_trust::merkle_root(&leaves).expect("root"),
            bytes(&tree["root_hex"])
        );
        let entry = host::HostedWitnessEntryV1 {
            executor_id: vec![1; 32],
            state: 2,
            archive_root: bytes(&tree["root_hex"]),
            archive_leaf_count: tree["count"].as_u64().expect("count"),
            ..Default::default()
        };
        for path in tree["paths"].as_array().expect("paths") {
            let p = host::HostedWitnessHistoryProofV1 {
                executor_id: entry.executor_id.clone(),
                purpose: 3,
                leaf_index: path["index"].as_u64().expect("index"),
                leaf_count: entry.archive_leaf_count,
                siblings: path["siblings_hex"]
                    .as_array()
                    .expect("siblings")
                    .iter()
                    .map(bytes)
                    .collect(),
            };
            let leaf = &leaves[p.leaf_index as usize];
            witness_trust::verify_inclusion(leaf, &p, &entry).expect("published path control");
            let mut malformed = p.clone();
            malformed.siblings.push(vec![0; 32]);
            assert_eq!(
                witness_trust::verify_inclusion(leaf, &malformed, &entry),
                Err(Reject::Proof)
            );
            if !p.siblings.is_empty() {
                let mut missing = p.clone();
                missing.siblings.pop();
                assert_eq!(
                    witness_trust::verify_inclusion(leaf, &missing, &entry),
                    Err(Reject::Proof)
                );
                let mut wrong = p.clone();
                wrong.siblings[0][0] ^= 1;
                assert_eq!(
                    witness_trust::verify_inclusion(leaf, &wrong, &entry),
                    Err(Reject::Proof)
                );
            }
            witness_trust::verify_inclusion(leaf, &p, &entry).expect("unchanged shape control");
        }
    }
}

fn fixture_signer(f: &Value, role: &str) -> crate::Ed25519Signer {
    crate::Ed25519Signer::from_seed(
        &hex::decode(f["keys"][role]["seed_hex"].as_str().expect("seed")).expect("bytes"),
    )
    .expect("signer")
}
fn sign_fixture<T: hybrid_codec::Canonical>(
    f: &Value,
    role: &str,
    domain: &str,
    body: &T,
) -> wire::AuthorizationSignature {
    wire::AuthorizationSignature {
        signer_key_id: hybrid_codec::key_id(&key(f, role)),
        signature: fixture_signer(f, role)
            .sign(&hybrid_codec::signing_digest(domain, body).expect("digest"))
            .expect("signature"),
    }
}

// Each published substitution must reach its API binding gate with valid
// signatures and surrounding payload commitments. The native adapter has its
// own independent selection tests above.
fn boundary_api_substitution(vector: &str) {
    let f = fixture();
    let set = trusted_set(&f, "current_set", 1100000);
    let control: host::SignedHostedWitnessStatementV1 = record(&f, "boundary_genesis_statement");
    WitnessEvidence::resolve(&set, &control, None, false, 1100000).expect("signed control");
    contract::verify_witness_payload(
        control.body.as_ref().expect("control body"),
        WitnessPayload::Genesis(&record(&f, "boundary_genesis_payload")),
    )
    .expect("exact binding control");
    let statement: host::SignedHostedWitnessStatementV1 =
        record(&f, &format!("{vector}_statement"));
    let result = WitnessEvidence::resolve(&set, &statement, None, false, 1100000).and_then(|_| {
        contract::verify_witness_payload(
            statement.body.as_ref().expect("body"),
            WitnessPayload::Genesis(&record(&f, &format!("{vector}_payload"))),
        )
        .map_err(Error::from)
    });
    assert!(
        matches!(result, Err(Error::Contract(Reject::BoundaryAcceptance))),
        "{vector}: {result:?}"
    );
}
macro_rules! boundary_api_negative {
    ($test:ident, $vector:literal) => {
        #[test]
        fn $test() {
            boundary_api_substitution($vector);
        }
    };
}
boundary_api_negative!(
    review_boundary_api_original_substitution,
    "acceptance_swapped_between_originals"
);
boundary_api_negative!(
    review_boundary_api_manifest_substitution,
    "manifest_mismatch"
);
boundary_api_negative!(review_boundary_api_intent_substitution, "intent_mismatch");
boundary_api_negative!(
    review_boundary_api_receipt_substitution,
    "receipt_from_another_acceptance"
);
boundary_api_negative!(review_boundary_api_required_binding, "missing_binding");

macro_rules! boundary_native_negative {
    ($test:ident, $statement:literal, $payload:literal, $kind:literal) => {
        #[test]
        fn $test() {
            let f = fixture();
            for retired in [false, true] {
                boundary_vector(
                    &f,
                    "boundary_complete_2_statement",
                    "boundary_complete_2_payload",
                    "genesis",
                    retired,
                )
                .expect("complete selection control");
                assert!(
                    matches!(
                        boundary_vector(&f, $statement, $payload, $kind, retired),
                        Err(Error::Contract(Reject::BoundaryAcceptance))
                    ),
                    "native selection: {} retired={retired}",
                    $statement
                );
            }
        }
    };
}
boundary_native_negative!(
    review_boundary_native_boundary_omission_2,
    "boundary_omission_2_statement",
    "boundary_omission_2_payload",
    "genesis"
);
boundary_native_negative!(
    review_boundary_native_boundary_duplicate_2,
    "boundary_duplicate_2_statement",
    "boundary_duplicate_2_payload",
    "genesis"
);
boundary_native_negative!(
    review_boundary_native_boundary_substitution_2,
    "boundary_substitution_2_statement",
    "boundary_substitution_2_payload",
    "genesis"
);
boundary_native_negative!(
    review_boundary_native_boundary_extra_2,
    "boundary_extra_2_statement",
    "boundary_extra_2_payload",
    "genesis"
);
boundary_native_negative!(
    review_boundary_native_boundary_omission_3,
    "boundary_omission_3_statement",
    "boundary_omission_3_payload",
    "genesis"
);
boundary_native_negative!(
    review_boundary_native_boundary_duplicate_3,
    "boundary_duplicate_3_statement",
    "boundary_duplicate_3_payload",
    "genesis"
);
boundary_native_negative!(
    review_boundary_native_boundary_substitution_3,
    "boundary_substitution_3_statement",
    "boundary_substitution_3_payload",
    "genesis"
);
boundary_native_negative!(
    review_boundary_native_boundary_extra_3,
    "boundary_extra_3_statement",
    "boundary_extra_3_payload",
    "genesis"
);
boundary_native_negative!(
    review_boundary_native_boundary_invalid_dependency_acceptance,
    "boundary_invalid_dependency_acceptance_statement",
    "boundary_invalid_dependency_acceptance_payload",
    "authority"
);
