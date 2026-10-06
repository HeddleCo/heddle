use api::heddle::api::common as host;

use super::*;

fn writer_fixture() -> serde_json::Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/writer-authority-alpha35.json"
    ))
    .expect("frozen writer vectors")
}
fn writer_record<T: Message + Default>(name: &str) -> T {
    let f = writer_fixture();
    hybrid_codec::strict_decode(
        &hex::decode(f["vectors"][name]["wire_hex"].as_str().expect("wire")).expect("hex"),
        1048576,
    )
    .expect("writer vector")
}
fn with_authorities(
    mut bundle: wire::NativePublicProofBundleV1,
    names: &[&str],
) -> wire::NativePublicProofBundleV1 {
    for name in names {
        bundle.authority_witnesses.push(writer_record(name));
        bundle
            .statements
            .push(writer_record(&format!("{name}_statement")));
    }
    bundle.authority_witnesses.sort_by_key(|p| {
        hybrid_codec::signing_digest("heddle-import-authority-witness-payload-v1", p)
            .expect("payload")
    });
    bundle.statements.sort_by_key(|s| {
        witness_trust::statement_signing_digest(s.body.as_ref().expect("body")).expect("statement")
    });
    bundle
}
#[test]
fn cowriter_start_capture_own_and_owner_thread_and_review() {
    for (bundle, names) in [
        (
            writer_record("cowriter_start_bundle"),
            vec!["cowriter_capture_own_thread"],
        ),
        (
            record(&fixture(), "native_landing"),
            vec!["cowriter_capture_owner_thread", "cowriter_review"],
        ),
    ] {
        let bundle = with_authorities(bundle, &names);
        verify_semantics(&bundle, 1_100_000)
            .expect("Spool governance and co-writer account resolve independently");
        let originals = bundle
            .authority_witnesses
            .iter()
            .filter_map(|p| p.original.clone())
            .collect::<Vec<_>>();
        local_work::install_bundle("co-writer", bundle, &originals, None);
    }
}
#[test]
fn writer_account_and_owner_identity_negatives_reject_then_controls_pass() {
    for name in ["self_signed_owner_uuid", "account_mismatch"] {
        let payload: wire::NativeGenesisWitnessV1 = writer_record(name);
        let error = api::native_witness::verify_genesis_authority(
            payload.binding.as_ref().expect("binding"),
            payload.original_genesis.as_ref().expect("original"),
            &payload.creator_authority_envelope,
        )
        .expect_err("wrong author root");
        assert!(
            [
                hybrid_codec::Reject::Root,
                hybrid_codec::Reject::GenesisBinding
            ]
            .contains(&error),
            "{name}: {error:?}"
        );
        let bundle: wire::NativePublicProofBundleV1 = writer_record("cowriter_start_bundle");
        verify_semantics(&bundle, 1_100_000).expect("co-writer control");
    }
}
#[test]
fn p2_envelope_cannot_replace_the_verified_operation_author() {
    let denied: wire::ImportAuthorityWitnessV1 = writer_record("p2_envelope_not_op_author");
    let (_, op) = verify::verify_native_operation(denied.original.as_ref().expect("original"))
        .expect("verified author");
    let objects::object::thread_replication::ThreadOperationBody::Metadata(bytes) = &op.body else {
        panic!("metadata")
    };
    let control = objects::object::thread_replication::metadata::ThreadControl::decode(bytes)
        .expect("control");
    assert_eq!(
        api::writer_authority::verify_authority_actor_binding(
            &denied,
            control.actor.principal_id.as_bytes(),
            &writer_record::<wire::ThreadControlAuthority>("owner_envelope")
                .owner
                .expect("owner")
                .root
                .expect("root")
                .root
                .expect("body")
                .account_uuid,
            &writer_record::<wire::ThreadControlAuthority>("owner_envelope")
                .owner
                .expect("owner")
                .root
                .expect("root")
                .root
                .expect("body")
                .owner_id
        ),
        Err(hybrid_codec::Reject::GenesisBinding)
    );
    verify_semantics(&writer_record("p2_owner_bundle"), 1_100_000)
        .expect("matching signed actor control");
}
#[test]
fn landing_requester_must_be_the_verified_token_subject() {
    let denied: wire::HostedLandingWitnessV1 = writer_record("requester_not_token_subject");
    let subject = heddleco_capability_verifier::thread_control_authority::inspect_landing_subject(
        &denied.authority_envelope,
    )
    .expect("verified sealed subject");
    let request_key: [u8; 32] = denied
        .request
        .as_ref()
        .expect("request")
        .signature
        .as_ref()
        .expect("signature")
        .public_key
        .as_slice()
        .try_into()
        .expect("key");
    let owner: wire::ThreadControlAuthority = writer_record("owner_envelope");
    let root = owner
        .owner
        .expect("owner")
        .root
        .expect("root")
        .root
        .expect("body");
    assert_eq!(
        api::writer_authority::verify_landing_actor_binding(
            &denied,
            &subject.account_uuid,
            &subject.publisher,
            &request_key,
            &root.account_uuid,
            &root.owner_id
        ),
        Err(hybrid_codec::Reject::KeyRole)
    );
    verify_semantics(&writer_record("p4_owner_bundle"), 1_100_000)
        .expect("matching token/request control");
}
#[test]
fn actor_key_policy_cut_and_non_review_p4_reject_then_controls_pass() {
    for (name, control, reason) in [
        (
            "native_actor_key_revoked",
            "cowriter_start_bundle",
            hybrid_codec::Reject::Revoked,
        ),
        (
            "p4_non_review",
            "p4_owner_bundle",
            hybrid_codec::Reject::Semantic,
        ),
        (
            "kind_2_counterparty_revoked",
            "ownership_counterparty_control",
            hybrid_codec::Reject::Revoked,
        ),
        (
            "kind_3_counterparty_revoked",
            "ownership_counterparty_control",
            hybrid_codec::Reject::Revoked,
        ),
    ] {
        let passing: wire::NativePublicProofBundleV1 = writer_record(control);
        api::native_witness::validate_public_bundle(&passing)
            .expect("exact nearby control before mutation");
        let denied: wire::NativePublicProofBundleV1 = writer_record(name);
        assert_eq!(
            api::native_witness::validate_public_bundle(&denied),
            Err(reason),
            "{name}"
        );
        api::native_witness::validate_public_bundle(&passing)
            .expect("exact nearby control after refusal");
    }
}

fn writer_signer(role: &str) -> crypto::Ed25519Signer {
    crypto::Ed25519Signer::from_seed(
        &hex::decode(
            writer_fixture()["keys"][role]["seed_hex"]
                .as_str()
                .expect("seed"),
        )
        .expect("hex"),
    )
    .expect("signer")
}
fn rebind_statement(
    signed: &mut host::SignedHostedWitnessStatementV1,
    payload: &[u8],
    authority: Vec<u8>,
    signatures: Vec<u8>,
) {
    use crypto::Signer;
    let s = signed.body.as_mut().expect("body");
    s.canonical_payload = payload.to_vec();
    s.authority_digest = authority;
    s.original_signatures_digest = signatures;
    signed.signature = writer_signer("witness")
        .sign(&witness_trust::statement_signing_digest(s).expect("statement digest"))
        .expect("signature");
}
#[test]
fn paired_cowriter_after_rotate_uses_witness_admitted_attachment() {
    use crypto::Signer;
    let mut bundle: wire::NativePublicProofBundleV1 = writer_record("cowriter_start_bundle");
    let mut envelope: wire::ThreadControlAuthority = writer_record("cowriter_envelope");
    let history: wire::OwnerHistory = writer_record("verified_rotate_history");
    let owner = heddleco_capability_verifier::creation::history_state(&history, 1100)
        .expect("verified Rotate");
    let wire::thread_control_authority::MintRootAssociation::OwnerMintRootAttachment(certificate) =
        envelope.mint_root_association.clone().expect("certificate")
    else {
        panic!("attachment")
    };
    let account: [u8; 16] = owner
        .signed_root()
        .root
        .as_ref()
        .expect("root")
        .account_uuid
        .as_slice()
        .try_into()
        .expect("account");
    let publisher: [u8; 32] = writer_signer("cowriter_device")
        .public_key()
        .try_into()
        .expect("publisher");
    envelope.owner = Some(history.clone());
    let bytes = envelope.encode_to_vec();
    let context = || heddleco_capability_verifier::thread_control_authority::Context {
        owner: &owner,
        account_uuid: &account,
        publisher: &publisher,
        agent_id: None,
        method: "/heddle.api.v1alpha2.ThreadService/StartThread",
        spool_path: "acme/imports",
        now: 1100,
    };
    // Absence of a durable admission cannot be repaired by an old owner's signature.
    assert!(heddleco_capability_verifier::thread_control_authority::verify_genesis_with_retained_mint_roots(&bytes, context(), &[], |_| false).is_err());
    let p = &mut bundle.genesis_witnesses[0];
    p.creator_authority_envelope = bytes;
    let binding = p.binding.as_mut().expect("binding");
    let body = binding.body.as_mut().expect("body");
    body.creator_authority_envelope_digest = hybrid_codec::hash(&[&p.creator_authority_envelope]);
    binding
        .creator_signature
        .as_mut()
        .expect("signature")
        .signature = writer_signer("cowriter_device")
        .sign(
            &hybrid_codec::signing_digest("heddle-native-genesis-authority-v1", body)
                .expect("digest"),
        )
        .expect("binding signature");
    let signatures = body.original_signatures_digest.clone();
    let authority = api::native_witness::signed_genesis_digest(binding).expect("authority");
    let payload = hybrid_codec::canonical(p).expect("payload");
    rebind_statement(&mut bundle.statements[0], &payload, authority, signatures);
    bundle.owner_histories.push(history);
    verify_semantics(&bundle, 1_100_000)
        .expect("paired actor survives verified Rotate through witness admission");
    let originals = [bundle.genesis_witnesses[0]
        .original_genesis
        .clone()
        .expect("genesis")];
    local_work::install_bundle("rotated paired co-writer", bundle, &originals, None);
    let recovered: wire::OwnerHistory = writer_record("verified_recover_history");
    heddleco_capability_verifier::creation::history_state(&recovered, 1100)
        .expect("verified Recover");
    let attachment = certificate.attachment.as_ref().expect("body");
    assert_eq!(
        api::writer_authority::retained_mint_root_issuer(
            &recovered,
            &attachment.owner_state_hash,
            attachment.owner_sequence
        )
        .err(),
        Some(hybrid_codec::Reject::Root)
    );
}
#[test]
fn forged_old_owner_certificate_cannot_join_durable_attachment_inventory() {
    use crypto::Signer;
    let mut env: wire::ThreadControlAuthority = writer_record("cowriter_envelope");
    let history: wire::OwnerHistory = writer_record("verified_rotate_history");
    let owner = heddleco_capability_verifier::creation::history_state(&history, 1100)
        .expect("verified owner Rotate");
    let real: wire::SignedOwnerMintRootAttachment = writer_record("paired_after_rotate");
    let forged: wire::SignedOwnerMintRootAttachment = writer_record("forged_old_owner_certificate");
    env.owner = Some(history);
    let account: [u8; 16] = owner
        .signed_root()
        .root
        .as_ref()
        .expect("root")
        .account_uuid
        .as_slice()
        .try_into()
        .expect("account");
    let publisher: [u8; 32] = writer_signer("cowriter_device")
        .public_key()
        .try_into()
        .expect("publisher");
    let context = || heddleco_capability_verifier::thread_control_authority::Context {
        owner: &owner,
        account_uuid: &account,
        publisher: &publisher,
        agent_id: None,
        method: "/heddle.api.v1alpha2.ThreadService/StartThread",
        spool_path: "acme/imports",
        now: 1100,
    };
    heddleco_capability_verifier::thread_control_authority::verify_genesis_with_retained_mint_roots(&env.encode_to_vec(), context(), std::slice::from_ref(&real), |_| false).expect("exact durable admission control");
    env.mint_root_association =
        Some(wire::thread_control_authority::MintRootAssociation::OwnerMintRootAttachment(forged));
    assert!(heddleco_capability_verifier::thread_control_authority::verify_genesis_with_retained_mint_roots(&env.encode_to_vec(), context(), &[real], |_| false).is_err());
}

#[test]
fn cowriter_claim_and_land_request_verify_and_install() {
    use crypto::Signer;
    let mut landing: wire::NativePublicProofBundleV1 = record(&fixture(), "native_landing");
    let old = hybrid_codec::canonical(&landing.landing_witnesses[0]).expect("old P4");
    let payload: wire::HostedLandingWitnessV1 = writer_record("cowriter_land_request");
    let signatures = retained_acceptor::original_signatures_digest(
        &payload
            .execution
            .iter()
            .chain(payload.source_operation.iter())
            .chain(&payload.review_evidence)
            .cloned()
            .collect::<Vec<_>>(),
        &[payload
            .request
            .as_ref()
            .expect("request")
            .signature
            .clone()
            .expect("signature")],
    )
    .expect("signatures");
    let s = landing
        .statements
        .iter_mut()
        .find(|s| s.body.as_ref().expect("body").canonical_payload == old)
        .expect("P4");
    s.body.as_mut().expect("body").publisher_key_id =
        hybrid_codec::key_id(writer_signer("cowriter_device").public_key());
    rebind_statement(
        s,
        &hybrid_codec::canonical(&payload).expect("payload"),
        hybrid_codec::hash(&[
            b"heddle-hosted-authority-envelope-v1",
            &(payload.authority_envelope.len() as u32).to_be_bytes(),
            &payload.authority_envelope,
        ]),
        signatures,
    );
    let execution = payload.execution.clone().expect("execution");
    landing.landing_witnesses = vec![payload];
    landing.statements.sort_by_key(|s| {
        witness_trust::statement_signing_digest(s.body.as_ref().expect("body")).expect("digest")
    });
    verify_semantics(&landing, 1_100_000)
        .expect("co-writer requester, independent original sources and Reviews");
    local_work::install_bundle("co-writer LandThread", landing, &[execution], None);

    let mut claiming: wire::NativePublicProofBundleV1 = record(&fixture(), "local_adopt_push");
    let index = claiming
        .authority_witnesses
        .iter()
        .position(|p| p.kind == 2)
        .expect("claim");
    let mut p = claiming.authority_witnesses[index].clone();
    let old = hybrid_codec::canonical(&p).expect("old P2");
    let mut claim =
        objects::object::thread_replication::ownership_claim::ThreadOwnershipClaim::decode(
            &p.original.as_ref().expect("original").canonical_record,
        )
        .expect("claim");
    let template: wire::ImportAuthorityWitnessV1 = writer_record("cowriter_claim");
    let template =
        objects::object::thread_replication::ownership_claim::ThreadOwnershipClaim::decode(
            &template.original.expect("original").canonical_record,
        )
        .expect("co-writer claim");
    claim.acceptance = template.acceptance;
    claim.accepting_publisher = template.accepting_publisher;
    let local = fixture()["keys"]
        .as_object()
        .expect("keys")
        .iter()
        .find(|(_, value)| {
            value["public_key_hex"].as_str() == Some(&hex::encode(claim.prior_local_key))
        })
        .map(|(role, _)| writer_signer(role))
        .expect("original local signer");
    let signed = crypto::thread_ownership_claim::SignedOwnershipClaim::sign(
        &claim,
        &local,
        &writer_signer("cowriter_device"),
    )
    .expect("native signatures");
    p.original = Some(wire::SignedRecord {
        format: objects::object::thread_replication::ownership_claim::FORMAT.into(),
        canonical_record: signed.canonical,
        signatures: vec![
            wire::RecordSignature {
                public_key: claim.prior_local_key.to_vec(),
                signature: signed.local_signature,
            },
            wire::RecordSignature {
                public_key: claim.accepting_publisher.to_vec(),
                signature: signed.acceptance_signature,
            },
        ],
    });
    p.authority_envelope =
        writer_record::<wire::ThreadControlAuthority>("cowriter_envelope").encode_to_vec();
    let signatures = retained_acceptor::original_signatures_digest(
        &p.original
            .iter()
            .chain(&p.dependencies)
            .cloned()
            .collect::<Vec<_>>(),
        &[],
    )
    .expect("signatures");
    let s = claiming
        .statements
        .iter_mut()
        .find(|s| s.body.as_ref().expect("body").canonical_payload == old)
        .expect("claim statement");
    s.body.as_mut().expect("body").publisher_key_id =
        hybrid_codec::key_id(writer_signer("cowriter_device").public_key());
    rebind_statement(
        s,
        &hybrid_codec::canonical(&p).expect("payload"),
        hybrid_codec::hash(&[
            b"heddle-hosted-authority-envelope-v1",
            &(p.authority_envelope.len() as u32).to_be_bytes(),
            &p.authority_envelope,
        ]),
        signatures,
    );
    let original = p.original.clone().expect("claim");
    claiming.authority_witnesses[index] = p;
    claiming.authority_witnesses.sort_by_key(|p| {
        hybrid_codec::signing_digest("heddle-import-authority-witness-payload-v1", p)
            .expect("digest")
    });
    claiming.statements.sort_by_key(|s| {
        witness_trust::statement_signing_digest(s.body.as_ref().expect("body")).expect("digest")
    });
    verify_semantics(&claiming, 1_100_000).expect("co-writer claim authority");
    local_work::install_bundle(
        "co-writer ClaimThreadOwnership",
        claiming,
        &[original],
        None,
    );
}

#[test]
fn receiver_policy_cuts_the_independent_actor_publisher_and_mint() {
    use crypto::Signer;
    use heddleco_capability_verifier::thread_control_authority::Revocation;

    use crate::hybrid::authority::tests::{bundle as imported, selected};
    let control: wire::NativePublicProofBundleV1 = writer_record("cowriter_start_bundle");
    let denied: wire::NativePublicProofBundleV1 = writer_record("native_actor_key_revoked");
    let limits = heddleco_capability_verifier::VerificationLimits::new(3600).expect("limits");
    let make = |bundle| {
        SelectedAuthority::new_native(
            AcceptedHistory::from_native_spool(
                &control,
                &selected(&imported(), limits),
                1100,
                limits,
            )
            .expect("independent governance"),
            bundle,
            |_: &wire::NativePublicProofBundleV1,
             _: i64,
             _: &repo::thread_replication::hosted_trust::TrustTransaction<'_>| Ok(()),
        )
    };
    let passing = make(control.clone());
    let rejected = make(denied.clone());
    let key = writer_signer("cowriter_device");
    for cut in [
        Revocation::Publisher(key.public_key()),
        Revocation::MintRoot(key.public_key()),
    ] {
        assert!(
            !passing.native_revoked(control.statements[0].body.as_ref().expect("body"), cut),
            "unrevoked independently resolved actor"
        );
        assert!(
            rejected.native_revoked(denied.statements[0].body.as_ref().expect("body"), cut),
            "policy must cut co-writer's key"
        );
    }
}

#[test]
fn spool_owner_pre_recover_history_cannot_revive_cut_device() {
    use biscuit_verifier::signature_v1::BiscuitBuilderV1Ext;
    use crypto::Signer;
    use objects::object::thread_replication::{
        ThreadOperationBody,
        metadata::{AUTHORITY_FORMAT, ThreadControl},
    };

    let mut bundle: wire::NativePublicProofBundleV1 = writer_record("p2_owner_bundle");
    verify_semantics(&bundle, 1_100_000).expect("pre-Recover device control");
    let selected_payload =
        hybrid_codec::canonical(&bundle.authority_witnesses[0]).expect("selected P2");
    let mut history = bundle.owner_histories[0].clone();
    let initial = heddleco_capability_verifier::creation::history_state(&history, 1100)
        .expect("independent owner root");
    let template: wire::OwnerHistory = writer_record("verified_recover_history");
    let mut transition = template.accepted_transitions[0].clone();
    let body = transition.transition.as_mut().expect("Recover");
    body.owner_id = initial.owner_id().to_vec();
    body.previous_state_hash = initial.state_hash().to_vec();
    let digest = hybrid_codec::hash(&[
        crypto::owner_root::OWNER_TRANSITION_DOMAIN,
        &crypto::owner_root::owner_key_transition_body(body).expect("Recover body"),
    ]);
    let authorization = |role: &str| wire::AuthorizationSignature {
        signer_key_id: hybrid_codec::key_id(writer_signer(role).public_key()),
        signature: writer_signer(role)
            .sign(&digest)
            .expect("Recover signature"),
    };
    transition.authorizations = initial
        .recovery_policy()
        .guardians
        .iter()
        .map(|g| {
            let role = ["guardian_a", "guardian_b"]
                .into_iter()
                .find(|r| {
                    writer_signer(r).public_key() == g.key.as_ref().expect("guardian").public_key
                })
                .expect("enrolled guardian");
            authorization(role)
        })
        .collect();
    transition.next_authority_key_proof = Some(authorization("rotated_owner"));
    transition.next_recovery_key_proofs = body
        .next_recovery_policy
        .as_ref()
        .expect("recovery policy")
        .guardians
        .iter()
        .map(|g| {
            let role = ["next_recovery_a", "next_recovery_b"]
                .into_iter()
                .find(|r| {
                    writer_signer(r).public_key() == g.key.as_ref().expect("guardian").public_key
                })
                .expect("new guardian");
            authorization(role)
        })
        .collect();
    let current = heddleco_capability_verifier::apply_accepted_transition(
        &initial,
        &transition,
        1100,
        heddleco_capability_verifier::VerificationLimits::new(3600).expect("limits"),
    )
    .expect("genuine recovery cuts prior device attachments");
    history.accepted_transitions.push(transition);
    history.state_hash = current.state_hash().to_vec();
    bundle.owner_histories.push(history.clone());
    let statement = bundle
        .statements
        .iter_mut()
        .find(|s| s.body.as_ref().expect("body").canonical_payload == selected_payload)
        .expect("P2");
    statement.body.as_mut().expect("body").owner_state_hash = history.state_hash.clone();
    statement.signature = writer_signer("witness")
        .sign(
            &witness_trust::statement_signing_digest(statement.body.as_ref().expect("body"))
                .expect("statement digest"),
        )
        .expect("fresh witness signature");
    bundle.statements.sort_by_key(|s| {
        witness_trust::statement_signing_digest(s.body.as_ref().expect("body")).expect("digest")
    });
    api::native_witness::verify_bundle_witnesses(&bundle, &set(&bundle, 1_100_000), 1_100_000, &[])
        .expect("portable testimony still passes; native owner authority must reject");
    let error = verify_semantics(&bundle, 1_100_000).expect_err(
        "receiver-pinned Recover must reject truncated history with the cut device attachment",
    );
    assert!(
        error.to_string().contains("recovered") || error.to_string().contains("Root"),
        "{error}"
    );

    // The current owner issues a fresh capability to this publisher. The old
    // device's own mint root and attachment no longer authorize its writes.
    let payload = &mut bundle.authority_witnesses[0];
    let mut envelope =
        api::writer_authority::decode_authority(&payload.authority_envelope).expect("envelope");
    let (_, mut operation) =
        verify::verify_native_operation(payload.original.as_ref().expect("original"))
            .expect("signed original");
    let ThreadOperationBody::Metadata(bytes) = &operation.body else {
        panic!("metadata");
    };
    let mut control = ThreadControl::decode(bytes).expect("metadata author");
    let actor = &control.actor;
    let method = control
        .authorization_method()
        .rsplit('/')
        .next()
        .expect("method");
    let seed = hex::decode(
        writer_fixture()["keys"]["rotated_owner"]["seed_hex"]
            .as_str()
            .expect("seed"),
    )
    .expect("hex");
    let pair = biscuit_auth::KeyPair::from(
        &biscuit_auth::PrivateKey::from_bytes(&seed, biscuit_auth::Algorithm::Ed25519)
            .expect("mint key"),
    );
    let token = biscuit_auth::Biscuit::builder().code(format!(
        "user(\"{}\"); session(\"recovered-owner\"); device_pop_key(\"{}\"); check if operation(\"{method}\"); check if resource(\"spool\", \"acme/imports\"); check if time($now), $now < 1970-01-01T00:30:00Z;",
        actor.principal_id, hex::encode(operation.publisher),
    )).expect("capability facts").build_v1(&pair).expect("current owner token");
    envelope.owner = Some(history);
    envelope.mint_root_public_key = current.authority_key().public_key.clone();
    envelope.mint_root_association = None;
    envelope.sealed_biscuit = token.seal().expect("seal").to_vec().expect("sealed bytes");
    payload.authority_envelope = envelope.encode_to_vec();
    control.authority_envelope = payload.authority_envelope.clone();
    control.authority_digest =
        objects::object::ContentHash::compute_typed(AUTHORITY_FORMAT, &control.authority_envelope);
    operation.body = ThreadOperationBody::Metadata(control.encode().expect("current author"));
    let signed =
        crypto::thread_operation::SignedOperation::sign(&operation, &writer_signer("device"))
            .expect("current original");
    let original = payload.original.as_mut().expect("original");
    original.canonical_record = signed.canonical;
    original.signatures[0].signature = signed.signature;
    let statement = bundle
        .statements
        .iter_mut()
        .find(|s| s.body.as_ref().expect("body").canonical_payload == selected_payload)
        .expect("P2");
    rebind_statement(
        statement,
        &hybrid_codec::canonical(payload).expect("payload"),
        hybrid_codec::hash(&[
            b"heddle-hosted-authority-envelope-v1",
            &(payload.authority_envelope.len() as u32).to_be_bytes(),
            &payload.authority_envelope,
        ]),
        retained_acceptor::original_signatures_digest(
            &payload
                .original
                .iter()
                .chain(&payload.dependencies)
                .cloned()
                .collect::<Vec<_>>(),
            &[],
        )
        .expect("signatures"),
    );
    bundle.statements.sort_by_key(|s| {
        witness_trust::statement_signing_digest(s.body.as_ref().expect("body")).expect("digest")
    });
    bundle.authority_witnesses.sort_by_key(|p| {
        hybrid_codec::signing_digest("heddle-import-authority-witness-payload-v1", p)
            .expect("payload digest")
    });
    verify_semantics(&bundle, 1_100_000)
        .expect("current owner history and fresh current-owner capability pass");
}
