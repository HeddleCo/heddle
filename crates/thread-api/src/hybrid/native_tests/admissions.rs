use super::*;

#[test]
fn native_account_integration_requires_its_exact_purpose2_authority() {
    use crypto::{Ed25519Signer, Signer};
    use objects::object::thread_replication::{OPERATION_FORMAT, SourceAuthor};

    let f = fixture();
    let mut bundle: wire::NativePublicProofBundleV1 = record(&f, "account_integration_as_local");
    assert!(
        api::native_witness::validate_public_bundle(&bundle).is_err(),
        "account work cannot use the local-work role"
    );
    let original = bundle
        .authority_witnesses
        .iter()
        .flat_map(|p| &p.dependencies)
        .find(|r| {
            r.format == OPERATION_FORMAT
                && verify::verify_native_operation(r).is_ok_and(|(_, op)| {
                    op.local_integration().is_ok_and(|i| {
                        i.is_some_and(|i| matches!(i.author, SourceAuthor::Account { .. }))
                    })
                })
        })
        .expect("signed account integration")
        .clone();
    let (_, operation) = verify::verify_native_operation(&original).expect("native proof");
    let Some(SourceAuthor::Account { authority, .. }) = operation.source_author().expect("author")
    else {
        panic!("account integration");
    };
    let mut dependencies: Vec<_> = bundle
        .genesis_witnesses
        .iter()
        .filter_map(|p| p.original_genesis.clone())
        .chain(
            bundle
                .authority_witnesses
                .iter()
                .flat_map(|p| p.dependencies.iter().cloned()),
        )
        .filter(|r| r != &original)
        .collect();
    dependencies.sort_by_key(|r| api::import_authority::signed_native_digest(r).expect("digest"));
    dependencies.dedup();
    let payload = wire::ImportAuthorityWitnessV1 {
        format_version: 1,
        kind: 1,
        original: Some(original.clone()),
        dependencies,
        authority_envelope: authority,
        boundary_acceptances: vec![],
    };
    let mut statement = bundle
        .statements
        .iter()
        .find(|s| s.body.as_ref().is_some_and(|s| s.purpose == 2))
        .expect("selected native admission context")
        .clone();
    let body = statement.body.as_mut().expect("body");
    body.canonical_payload = hybrid_codec::canonical(&payload).expect("payload");
    body.publisher_key_id = hybrid_codec::key_id(&operation.publisher);
    let envelope = &payload.authority_envelope;
    body.authority_digest = hybrid_codec::hash(&[
        b"heddle-hosted-authority-envelope-v1",
        &(envelope.len() as u32).to_be_bytes(),
        envelope,
    ]);
    let signatures: Vec<_> = std::iter::once(&original)
        .chain(&payload.dependencies)
        .flat_map(|r| &r.signatures)
        .collect();
    let mut bytes = (signatures.len() as u32).to_be_bytes().to_vec();
    for signature in signatures {
        bytes.extend(hybrid_codec::canonical(signature).expect("signature"));
    }
    body.original_signatures_digest =
        hybrid_codec::hash(&[b"heddle-hosted-original-signatures-v1", &bytes]);
    body.admission_order += 100;
    let seed = hex::decode(f["keys"]["witness"]["seed_hex"].as_str().expect("seed")).expect("hex");
    statement.signature = Ed25519Signer::from_seed(&seed)
        .expect("witness")
        .sign(&witness_trust::statement_signing_digest(body).expect("digest"))
        .expect("witness signature");
    bundle.authority_witnesses.push(payload);
    bundle.authority_witnesses.sort_by_key(|p| {
        hybrid_codec::signing_digest("heddle-import-authority-witness-payload-v1", p)
            .expect("digest")
    });
    bundle.statements.push(statement);
    bundle.statements.sort_by_key(|s| {
        witness_trust::statement_signing_digest(s.body.as_ref().expect("body")).expect("digest")
    });
    verify_semantics(&bundle, 1_100_000).expect("original account integration authority");
    local_work::install_bundle(
        "account integration with purpose 2",
        bundle,
        &[original],
        None,
    );
}

#[test]
fn native_conflicting_first_admissions_reject_before_mutation() {
    use crypto::{Ed25519Signer, Signer};

    let f = fixture();
    let mut bundle: wire::NativePublicProofBundleV1 = record(&f, "account_source");
    let originals = vec![
        bundle.authority_witnesses[0]
            .original
            .clone()
            .expect("source"),
    ];
    local_work::install_bundle("sole admission", bundle.clone(), &originals, None);
    let mut extra = bundle.authority_witnesses[0].clone();
    let original_payload = hybrid_codec::canonical(&extra).expect("payload");
    assert!(extra.dependencies.is_empty());
    extra.dependencies.push(
        bundle.genesis_witnesses[0]
            .original_genesis
            .clone()
            .expect("genesis"),
    );
    let mut statement = bundle
        .statements
        .iter()
        .find(|s| {
            s.body
                .as_ref()
                .is_some_and(|s| s.canonical_payload == original_payload)
        })
        .expect("statement")
        .clone();
    let body = statement.body.as_mut().expect("body");
    body.canonical_payload = hybrid_codec::canonical(&extra).expect("new payload");
    let original = extra.original.as_ref().expect("original");
    let count = original.signatures.len()
        + extra
            .dependencies
            .iter()
            .map(|r| r.signatures.len())
            .sum::<usize>();
    let mut signatures = (count as u32).to_be_bytes().to_vec();
    for signature in std::iter::once(original)
        .chain(&extra.dependencies)
        .flat_map(|r| &r.signatures)
    {
        signatures.extend(hybrid_codec::canonical(signature).expect("signature framing"));
    }
    body.original_signatures_digest =
        hybrid_codec::hash(&[b"heddle-hosted-original-signatures-v1", &signatures]);
    body.admission_order += 100;
    let seed = hex::decode(f["keys"]["witness"]["seed_hex"].as_str().expect("seed")).expect("hex");
    let signer = Ed25519Signer::from_seed(&seed).expect("witness");
    statement.signature = signer
        .sign(&witness_trust::statement_signing_digest(body).expect("digest"))
        .expect("valid conflicting witness signature");
    bundle.authority_witnesses.push(extra);
    bundle.authority_witnesses.sort_by_key(|p| {
        hybrid_codec::signing_digest("heddle-import-authority-witness-payload-v1", p)
            .expect("payload digest")
    });
    bundle.statements.push(statement);
    bundle.statements.sort_by_key(|s| {
        witness_trust::statement_signing_digest(s.body.as_ref().expect("body")).expect("digest")
    });
    verify_semantics(&bundle, 1_100_000).expect("each admission independently verifies");
    local_work::install_bundle(
        "conflicting first admissions",
        bundle,
        &originals,
        Some(hybrid_codec::Reject::SlotConflict),
    );
}
