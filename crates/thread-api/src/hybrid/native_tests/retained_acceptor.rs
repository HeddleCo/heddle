use api::{
    heddle::api::{common as host, v1alpha2 as wire},
    hybrid_codec, writer_authority as writer,
};
use crypto::{Ed25519Signer, Signer};
use heddleco_capability_verifier as capability;
use objects::object::{
    original_boundary_acceptance::{AdmissionBasis, FORMAT, OriginalBoundaryAcceptance},
    thread_authority_admission::ThreadAuthorityAdmission,
    thread_genesis_admission::ThreadGenesisAdmission,
    thread_replication::SourceAuthor,
};
use prost::Message;

fn fixture(file: &str) -> serde_json::Value {
    let text = match file {
        "boundary-acceptor-alpha36.json" => {
            include_str!("../../../tests/fixtures/boundary-acceptor-alpha36.json")
        }
        "writer-authority-alpha35.json" => {
            include_str!("../../../tests/fixtures/writer-authority-alpha35.json")
        }
        _ => panic!("unknown fixture"),
    };
    serde_json::from_str(text).expect("frozen API vectors")
}
fn vector<T: Message + Default>(file: &str, name: &str) -> T {
    let f = fixture(file);
    hybrid_codec::strict_decode(
        &hex::decode(f["vectors"][name]["wire_hex"].as_str().expect("wire")).expect("hex"),
        1048576,
    )
    .expect("canonical vector")
}
fn signer(role: &str) -> Ed25519Signer {
    Ed25519Signer::from_seed(
        &hex::decode(
            fixture("boundary-acceptor-alpha36.json")["keys"][role]["seed_hex"]
                .as_str()
                .expect("seed"),
        )
        .expect("hex"),
    )
    .expect("signer")
}
fn verify_history(h: &wire::OwnerHistory) -> capability::VerifiedOwnerState {
    let mut state =
        capability::verify_owner_root(h.root.as_ref().expect("root")).expect("verified root");
    for transition in &h.accepted_transitions {
        state = capability::apply_accepted_transition(
            &state,
            transition,
            1100,
            capability::VerificationLimits::new(3600).expect("limits"),
        )
        .expect("verified transition");
    }
    assert_eq!(h.state_hash, state.state_hash());
    state
}
fn verify_statement(s: &host::SignedHostedWitnessStatementV1) {
    hybrid_codec::verify(
        signer("witness").public_key(),
        &api::witness_trust::statement_signing_digest(s.body.as_ref().expect("body"))
            .expect("digest"),
        &s.signature,
    )
    .expect("authenticated statement signature");
}
fn sign_statement(s: &mut host::SignedHostedWitnessStatementV1) {
    s.signature = signer("witness")
        .sign(
            &api::witness_trust::statement_signing_digest(s.body.as_ref().expect("body"))
                .expect("digest"),
        )
        .expect("witness signature");
}
pub(super) fn original_signatures_digest(
    records: &[wire::SignedRecord],
    extra: &[wire::RecordSignature],
) -> Result<Vec<u8>, hybrid_codec::Reject> {
    let count = records.iter().map(|r| r.signatures.len()).sum::<usize>() + extra.len();
    let mut framed = (count as u32).to_be_bytes().to_vec();
    for signature in records.iter().flat_map(|r| &r.signatures).chain(extra) {
        framed.extend(hybrid_codec::canonical(signature)?);
    }
    Ok(hybrid_codec::hash(&[
        b"heddle-hosted-original-signatures-v1",
        &framed,
    ]))
}
pub(super) fn rotated_boundary(purpose: u32) -> wire::NativePublicProofBundleV1 {
    let b: wire::NativePublicProofBundleV1 = vector(
        "boundary-acceptor-alpha36.json",
        if purpose == 1 {
            "native_p1_control"
        } else {
            "native_p2_control"
        },
    );
    rotate_boundary(b, purpose, &signer("paired_leaf"))
}
fn rotate_boundary(
    mut b: wire::NativePublicProofBundleV1,
    purpose: u32,
    accepting_signer: &impl Signer,
) -> wire::NativePublicProofBundleV1 {
    api::native_witness::validate_public_bundle(&b).expect("unchanged control");
    let old_boundary = if purpose == 1 {
        b.genesis_witnesses[0]
            .boundary_acceptance
            .clone()
            .expect("boundary")
    } else {
        b.authority_witnesses[0].boundary_acceptances[0].clone()
    };
    let mut boundary = old_boundary.clone();
    let signed = boundary.signed_acceptance.as_ref().expect("acceptance");
    let mut acceptance =
        OriginalBoundaryAcceptance::decode(&signed.canonical_record).expect("acceptance");
    let SourceAuthor::Account {
        actor,
        authority,
        spool,
        ..
    } = &acceptance.accepting_author
    else {
        panic!("account")
    };
    let actor = actor.clone();
    let spool = *spool;
    let mut env = writer::decode_authority(authority).expect("acceptor envelope");
    let h = env.owner.as_mut().expect("history");
    let initial = verify_history(h);
    let template: wire::OwnerHistory =
        vector("writer-authority-alpha35.json", "verified_rotate_history");
    let mut t = template.accepted_transitions[0]
        .transition
        .clone()
        .expect("rotation");
    t.owner_id = initial.owner_id().to_vec();
    t.previous_state_hash = initial.state_hash().to_vec();
    t.next_recovery_policy = Some(initial.recovery_policy().clone());
    let body = crypto::owner_root::owner_key_transition_body(&t).expect("rotation canonical");
    let digest = hybrid_codec::hash(&[crypto::owner_root::OWNER_TRANSITION_DOMAIN, &body]);
    let auth = |role: &str| wire::AuthorizationSignature {
        signer_key_id: hybrid_codec::key_id(signer(role).public_key()),
        signature: signer(role).sign(&digest).expect("signature"),
    };
    let transition = wire::SignedOwnerKeyTransition {
        transition: Some(t),
        authorizations: vec![auth("owner")],
        next_authority_key_proof: Some(auth("rotated_owner")),
        next_recovery_key_proofs: vec![],
    };
    let current = capability::apply_accepted_transition(
        &initial,
        &transition,
        1100,
        capability::VerificationLimits::new(3600).expect("limits"),
    )
    .expect("genuine owner Rotate");
    h.accepted_transitions.push(transition);
    h.state_hash = current.state_hash().to_vec();
    verify_history(h);
    let rotated_history = h.clone();
    acceptance.accepting_author = SourceAuthor::account(spool, actor, env.encode_to_vec())
        .expect("rotated accepting authority");
    let signed = crypto::original_boundary_acceptance::SignedBoundaryAcceptance::sign(
        &acceptance,
        accepting_signer,
    )
    .expect("signed accepting authority");
    let signed_record = wire::SignedRecord {
        format: FORMAT.into(),
        canonical_record: signed.canonical,
        signatures: vec![wire::RecordSignature {
            public_key: acceptance.accepting_publisher.to_vec(),
            signature: signed.signature,
        }],
    };
    boundary.signed_acceptance = Some(signed_record);
    let id = acceptance.id().expect("acceptance ID");
    for receipt in &mut boundary.original_receipts {
        match receipt.format.as_str() {
            "heddle-thread-genesis-admission-v2" => {
                let mut r = ThreadGenesisAdmission::decode(&receipt.canonical_record)
                    .expect("genesis receipt");
                r.basis = AdmissionBasis::BoundaryAcceptance { acceptance: id };
                let signed = crypto::thread_genesis_admission::SignedGenesisAdmission::sign(
                    &r,
                    &signer("witness"),
                )
                .expect("signed receipt");
                receipt.canonical_record = signed.canonical;
                receipt.signatures[0].signature = signed.signature;
            }
            "heddle-thread-authority-admission-v3" => {
                let mut r = ThreadAuthorityAdmission::decode(&receipt.canonical_record)
                    .expect("source receipt");
                r.basis = AdmissionBasis::BoundaryAcceptance { acceptance: id };
                let signed = crypto::thread_authority_admission::SignedAuthorityAdmission::sign(
                    &r,
                    &signer("witness"),
                )
                .expect("signed receipt");
                receipt.canonical_record = signed.canonical;
                receipt.signatures[0].signature = signed.signature;
            }
            _ => panic!("receipt format"),
        }
    }
    boundary
        .original_receipts
        .sort_by_key(|r| api::import_authority::signed_native_digest(r).expect("digest"));
    let binding = boundary.binding.as_mut().expect("binding");
    binding.acceptance_id = id.as_bytes().to_vec();
    binding.signed_acceptance_digest = api::import_authority::signed_native_digest(
        boundary.signed_acceptance.as_ref().expect("acceptance"),
    )
    .expect("digest");
    binding.original_receipt_digests = boundary
        .original_receipts
        .iter()
        .map(|r| api::import_authority::signed_native_digest(r).expect("digest"))
        .collect();
    api::import_authority::verify_boundary_acceptance(&boundary)
        .expect("genuine complete boundary signatures and commitments");
    for p in &mut b.genesis_witnesses {
        if p.boundary_acceptance.as_ref() != Some(&old_boundary) {
            continue;
        }
        let previous = hybrid_codec::canonical(p).expect("old payload");
        p.boundary_acceptance = Some(boundary.clone());
        let canonical = hybrid_codec::canonical(p).expect("payload");
        let s = b
            .statements
            .iter_mut()
            .find(|s| {
                s.body
                    .as_ref()
                    .is_some_and(|s| s.purpose == 1 && s.canonical_payload == previous)
            })
            .expect("P1");
        let s_body = s.body.as_mut().expect("body");
        s_body.canonical_payload = canonical;
        s_body.boundary_acceptance = boundary.binding.clone();
        s_body.owner_state_hash = rotated_history.state_hash.clone();
        sign_statement(s);
    }
    for p in &mut b.authority_witnesses {
        if !p.boundary_acceptances.contains(&old_boundary) {
            continue;
        }
        let previous = hybrid_codec::canonical(p).expect("old payload");
        p.boundary_acceptances = vec![boundary.clone()];
        for dependency in &mut p.dependencies {
            if old_boundary.signed_acceptance.as_ref() == Some(dependency) {
                *dependency = boundary.signed_acceptance.clone().expect("acceptance");
            } else if old_boundary.original_receipts.contains(dependency) {
                *dependency = boundary
                    .original_receipts
                    .iter()
                    .find(|r| r.format == dependency.format)
                    .expect("receipt")
                    .clone();
            }
        }
        p.dependencies
            .sort_by_key(|r| api::import_authority::signed_native_digest(r).expect("digest"));
        let canonical = hybrid_codec::canonical(p).expect("payload");
        let s = b
            .statements
            .iter_mut()
            .find(|s| {
                s.body
                    .as_ref()
                    .is_some_and(|s| s.purpose == 2 && s.canonical_payload == previous)
            })
            .expect("P2");
        let s_body = s.body.as_mut().expect("body");
        s_body.canonical_payload = canonical;
        s_body.original_signatures_digest = original_signatures_digest(
            &std::iter::once(p.original.clone().expect("original"))
                .chain(p.dependencies.clone())
                .collect::<Vec<_>>(),
            &[],
        )
        .expect("original commitment");
        s_body.boundary_acceptance = boundary.binding.clone();
        s_body.owner_state_hash = rotated_history.state_hash.clone();
        sign_statement(s);
    }
    b.owner_histories.push(rotated_history);
    b.statements.sort_by_key(|s| {
        api::witness_trust::statement_signing_digest(s.body.as_ref().expect("body"))
            .expect("digest")
    });
    b
}

#[test]
fn alpha36_revoked_original_contract_is_fixed() {
    let b: wire::NativePublicProofBundleV1 = vector(
        "boundary-acceptor-alpha36.json",
        "native_p2_original_revoked",
    );
    api::native_witness::validate_public_bundle(&b).expect("revoked original remains valid");
    for signed in &b.statements {
        verify_statement(signed);
    }
}
#[test]
fn ordinary_retained_attachment_helper_control() {
    let p: wire::ImportAuthorityWitnessV1 =
        vector("writer-authority-alpha35.json", "admitted_original_payload");
    let s: host::SignedHostedWitnessStatementV1 = vector(
        "writer-authority-alpha35.json",
        "admitted_original_statement",
    );
    verify_statement(&s);
    let h: wire::OwnerHistory = vector("writer-authority-alpha35.json", "verified_rotate_history");
    verify_history(&h);
    let env = writer::decode_authority(&p.authority_envelope).expect("envelope");
    let Some(wire::thread_control_authority::MintRootAssociation::OwnerMintRootAttachment(a)) =
        env.mint_root_association
    else {
        panic!("attachment")
    };
    let body = a.attachment.as_ref().expect("attachment body");
    let issuer = writer::retained_mint_root_issuer(&h, &body.owner_state_hash, body.owner_sequence)
        .expect("verified retained issuer");
    let admitted = writer::admitted_owner_mint_root_attachment(
        s.body.as_ref().expect("body"),
        writer::WriterWitnessPayload::Import(api::import_authority::WitnessPayload::Authority(&p)),
    )
    .expect("attested exact original attachment");
    writer::verify_retained_writer_attachment(
        &a.encode_to_vec(),
        &env.mint_root_public_key,
        &issuer,
        &admitted,
        1100,
    )
    .expect("ordinary retained helper control");
}
fn check_rotated_boundary_acceptor(purpose: u32) {
    let b = rotated_boundary(purpose);
    api::native_witness::validate_public_bundle(&b)
        .expect("alpha36 accepts exact rotated boundary carrier");
    let root = hex::decode(
        fixture("boundary-acceptor-alpha36.json")["keys"]["root"]["public_key_hex"]
            .as_str()
            .expect("root"),
    )
    .expect("hex");
    let set = api::witness_trust::verify_set(
        b.witness_set.as_ref().expect("witness set"),
        &api::witness_trust::SetExpectation {
            authority: "https://weft.example.test",
            root_id: "descriptor-root-1",
            root_public_key: &root,
            root_epoch: 1,
            now_unix_millis: 1_100_000,
            clock_floor_unix_millis: 1_000_000,
            known_job_keys: &[],
        },
        None,
    )
    .expect("independent root-authenticated set");
    api::native_witness::verify_bundle_witnesses(&b, &set, 1_100_000, &[])
        .expect("authenticated native witness statements");
    let p = &b.genesis_witnesses[0];
    let boundary = if purpose == 1 {
        p.boundary_acceptance.as_ref().expect("boundary")
    } else {
        &b.authority_witnesses[0].boundary_acceptances[0]
    };
    let signed = boundary.signed_acceptance.as_ref().expect("acceptance");
    let acceptance = crypto::original_boundary_acceptance::SignedBoundaryAcceptance {
        canonical: signed.canonical_record.clone(),
        signature: signed.signatures[0].signature.clone(),
    }
    .verify_signature()
    .expect("native acceptance signature");
    let SourceAuthor::Account {
        actor, authority, ..
    } = &acceptance.accepting_author
    else {
        panic!("account")
    };
    let env = writer::decode_authority(authority).expect("acceptor envelope");
    let history = env.owner.as_ref().expect("owner history");
    let state = verify_history(history);
    assert_eq!(state.sequence(), 1);
    let Some(wire::thread_control_authority::MintRootAssociation::OwnerMintRootAttachment(a)) =
        env.mint_root_association.as_ref()
    else {
        panic!("attachment")
    };
    let body = a.attachment.as_ref().expect("attachment body");
    assert_eq!(body.owner_sequence, 0);
    let issuer =
        writer::retained_mint_root_issuer(history, &body.owner_state_hash, body.owner_sequence)
            .expect("verified history retains issuer after Rotate");
    let manifest =
        objects::object::original_boundary_acceptance::OriginalPublicationManifest::decode(
            &boundary.originals_manifest,
        )
        .expect("manifest");
    for entry in &manifest.entries {
        let (kind, method) = match &entry.subject {
            objects::object::original_boundary_acceptance::ManifestSubject::Genesis(_) => (
                capability::boundary_authority::BoundarySubjectKind::AccountGenesis,
                "/heddle.api.v1alpha2.ThreadService/StartThread",
            ),
            objects::object::original_boundary_acceptance::ManifestSubject::Source(_) => (
                capability::boundary_authority::BoundarySubjectKind::Source,
                "/heddle.api.v1alpha2.SyncService/PublishContent",
            ),
            _ => panic!("subject"),
        };
        capability::boundary_authority::verify_accepting_authority(
            authority,
            capability::thread_control_authority::Context {
                owner: &state,
                account_uuid: actor.principal_id.as_bytes(),
                publisher: &acceptance.accepting_publisher,
                agent_id: actor.agent_id.as_deref(),
                method,
                spool_path: "acme/imports",
                now: 1100,
            },
            capability::boundary_authority::OriginalSubjectScope {
                kind,
                account: acceptance.original_account.as_bytes(),
                thread: entry.thread.as_bytes(),
                subject: entry.subject.id().as_bytes(),
                publisher: &entry.publisher,
                agent_id: entry
                    .authority
                    .as_ref()
                    .and_then(|a| a.actor.agent_id.as_deref()),
            },
            std::slice::from_ref(a),
            |_| false,
        )
        .expect("host inventory verifies full current accepting authority after Rotate");
    }
    println!(
        "PASS: full verified owner Rotate, retained issuer, native acceptance signature, sealed accepting token, authenticated set and statements"
    );
    let mut failures = Vec::new();
    for signed in &b.statements {
        let s = signed.body.as_ref().expect("body");
        if s.basis != 2 {
            continue;
        }
        let payload = match s.purpose {
            1 => writer::WriterWitnessPayload::NativeGenesis(p),
            2 => writer::WriterWitnessPayload::Import(
                api::import_authority::WitnessPayload::Authority(&b.authority_witnesses[0]),
            ),
            _ => panic!("purpose"),
        };
        let admitted = writer::admitted_owner_mint_root_attachment(s, payload)
            .expect("authenticated attachment extraction");
        let result = writer::verify_retained_writer_attachment(
            &a.encode_to_vec(),
            &env.mint_root_public_key,
            &issuer,
            &admitted,
            1100,
        );
        println!(
            "basis={} purpose={} mandatory acceptor retained attachment: {result:?}",
            s.basis, s.purpose
        );
        if result.is_err() {
            failures.push(s.purpose);
        }
    }
    assert!(
        failures.is_empty(),
        "mandatory helper must attest the signature-bound accepting attachment, failures at purposes {failures:?}"
    );
}
#[test]
fn alpha36_p1_rotated_boundary_acceptor_must_receive_attested_attachment() {
    check_rotated_boundary_acceptor(1);
}
#[test]
fn alpha36_p2_rotated_boundary_acceptor_must_receive_attested_attachment() {
    check_rotated_boundary_acceptor(2);
}

#[test]
fn rotated_boundary_acceptor_p1_p2_pass_full_native_and_transaction_verification() {
    for purpose in [1, 2] {
        let bundle = rotate_boundary(
            super::boundary_revocations::boundary_bundle(purpose == 2),
            purpose,
            &Ed25519Signer::from_seed(&[61; 32]).expect("acceptor"),
        );
        super::verify_semantics(&bundle, 1_100_000)
            .expect("full native rotated accepting authority");
        let originals = if purpose == 1 {
            vec![
                bundle.genesis_witnesses[0]
                    .original_genesis
                    .clone()
                    .expect("genesis"),
            ]
        } else {
            vec![
                bundle.authority_witnesses[0]
                    .original
                    .clone()
                    .expect("source"),
            ]
        };
        super::local_work::install_bundle(
            "full rotated accepting authority",
            bundle,
            &originals,
            None,
        );
    }
}

#[test]
fn boundary_p1_prefix_refuses_selection_beyond_cutoff() {
    let control = super::boundary_revocations::boundary_bundle(true);
    super::verify_semantics(&control, 1_100_000)
        .expect("complete P1/P2 boundary acceptance control");
    println!("complete P1/P2 carrier: full native verification passed");
    let source = control.authority_witnesses[0]
        .original
        .clone()
        .expect("source original");
    let p1 = control
        .statements
        .iter()
        .filter_map(|s| s.body.as_ref())
        .find(|s| s.purpose == 1)
        .expect("P1");
    let p2 = control
        .statements
        .iter()
        .filter_map(|s| s.body.as_ref())
        .find(|s| s.purpose == 2)
        .expect("P2");
    assert!(p1.admission_order < p2.admission_order);
    let p1_order = p1.admission_order;
    let p2_order = p2.admission_order;
    let genesis = control.genesis_witnesses[0]
        .original_genesis
        .as_ref()
        .expect("genesis");
    let (_, decoded) =
        crypto::import_authority::verify_native_genesis(genesis).expect("verified genesis");
    let reference = wire::ForeignDependencyV1 {
        format_version: 1,
        origin: 2,
        thread_genesis_digest: decoded.id().expect("ID").as_bytes().to_vec(),
        signed_native_digest: api::import_authority::signed_native_digest(genesis)
            .expect("signed digest"),
        prefix_admission_order: p1_order,
    };
    let proof = crate::hybrid::authority::PublicProof::from(control);
    assert!(
        matches!(
            proof.prefix(&reference),
            Err(api::hybrid_codec::Reject::Scope)
        ),
        "projection must fail closed when a complete selection needs a later raw original"
    );
    let crate::hybrid::authority::PublicProof::Native(mut prefix) = proof else {
        panic!("native origin")
    };
    prefix
        .statements
        .retain(|s| s.body.as_ref().expect("body").admission_order <= p1_order);
    prefix.authority_witnesses.clear();
    api::native_witness::validate_public_bundle(&prefix).expect("ordinary API projection checks");
    assert_eq!(
        prefix.statements.len(),
        1,
        "genesis cutoff must exclude the later P2"
    );
    assert!(
        prefix.authority_witnesses.is_empty(),
        "future P2 sidecar cannot travel in P1 prefix"
    );
    assert_eq!(
        prefix.genesis_witnesses[0]
            .boundary_acceptance
            .as_ref()
            .expect("unchanged acceptance")
            .original_receipts
            .len(),
        2,
        "signed acceptance covers both genesis and source"
    );
    println!(
        "ordinary API prefix checks passed: P1 cutoff {}; later P2 {}; two unchanged receipts",
        p1_order, p2_order
    );
    super::verify_semantics_with_originals(&prefix, 1_100_000, &[], &[], &[source])
        .expect("the exact omitted source resolves the native selection failure");
    println!("P1 prefix plus the omitted source original: full native verification passed");
    assert!(matches!(
        super::verify_semantics(&prefix, 1_100_000)
            .expect_err("incomplete selection")
            .downcast_ref::<crypto::import_authority::Error>(),
        Some(crypto::import_authority::Error::Contract(
            api::hybrid_codec::Reject::Scope
        ))
    ));
    let genesis = prefix.genesis_witnesses[0]
        .original_genesis
        .clone()
        .expect("genesis");
    super::local_work::install_bundle(
        "incomplete boundary prefix",
        *prefix,
        &[genesis],
        Some(api::hybrid_codec::Reject::Scope),
    );
}
