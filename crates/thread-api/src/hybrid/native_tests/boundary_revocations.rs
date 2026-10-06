use std::collections::BTreeSet;

use biscuit_verifier::signature_v1::BiscuitBuilderV1Ext;
use crypto::{Ed25519Signer, Signer};
use objects::object::{
    CollaborationActor, ContentHash,
    original_boundary_acceptance::{
        AdmissionBasis, BoundaryOriginalKind, OriginalBoundaryAcceptance, OriginalManifestEntry,
        OriginalPublicationManifest, PublicationIntent,
    },
    thread_authority_admission::{OriginalAuthoritySubject, ThreadAuthorityAdmission},
    thread_genesis_admission::ThreadGenesisAdmission,
    thread_replication::SourceAuthor,
};

use super::*;

fn signer(role: &str) -> Ed25519Signer {
    Ed25519Signer::from_seed(
        &hex::decode(fixture()["keys"][role]["seed_hex"].as_str().expect("seed")).expect("hex"),
    )
    .expect("signer")
}

fn signed_record(
    format: &str,
    canonical: Vec<u8>,
    signature: Vec<u8>,
    key: &[u8],
) -> wire::SignedRecord {
    wire::SignedRecord {
        format: format.into(),
        canonical_record: canonical,
        signatures: vec![wire::RecordSignature {
            public_key: key.to_vec(),
            signature,
        }],
    }
}

/// Genuine owner-attached mint root, publisher and credential all differ from
/// the retained offline author. Both P1 and P2 keep the exact original bytes.
pub(super) fn boundary_bundle(source: bool) -> wire::NativePublicProofBundleV1 {
    let mut bundle: wire::NativePublicProofBundleV1 = record(
        &fixture(),
        if source {
            "account_source"
        } else {
            "start_thread"
        },
    );
    let template: wire::NativePublicProofBundleV1 = record(&fixture(), "boundary_acceptance");
    let mut boundary = template.genesis_witnesses[0]
        .boundary_acceptance
        .clone()
        .expect("boundary");
    let original = bundle.genesis_witnesses[0]
        .original_genesis
        .as_ref()
        .expect("genesis");
    let (_, genesis) = verify::verify_native_genesis(original).expect("original signature");
    let entry = OriginalManifestEntry::from_genesis(
        &genesis,
        &bundle.genesis_witnesses[0].creator_authority_envelope,
    )
    .expect("original identity");
    let actor = entry.authority.as_ref().expect("account").actor.clone();
    let mut entries = vec![entry];
    if source {
        let (_, operation) = verify::verify_native_operation(
            bundle.authority_witnesses[0]
                .original
                .as_ref()
                .expect("source"),
        )
        .expect("original source signature");
        entries.push(OriginalManifestEntry::from_operation(&operation).expect("source identity"));
    }
    let manifest = OriginalPublicationManifest::new(entries).expect("manifest");
    let intent = PublicationIntent::decode(&boundary.publication_intent).expect("intent");
    assert_eq!(intent.thread, genesis.id().expect("Thread"));
    let publisher = Ed25519Signer::from_seed(&[61; 32]).expect("accepting publisher");
    let mint = Ed25519Signer::from_seed(&[62; 32]).expect("accepting mint root");
    let original_envelope: wire::ThreadControlAuthority = hybrid_codec::strict_decode(
        &bundle.genesis_witnesses[0].creator_authority_envelope,
        65536,
    )
    .expect("original envelope");
    let owner = original_envelope.owner.as_ref().expect("owner");
    let current =
        heddleco_capability_verifier::verify_owner_root(owner.root.as_ref().expect("root"))
            .expect("owner");
    let attachment = wire::MintRootAttachment {
        format_version: 1,
        account_uuid: actor.principal_id.as_bytes().to_vec(),
        owner_state_hash: current.state_hash().to_vec(),
        owner_sequence: current.sequence(),
        owner_key: Some(current.authority_key().clone()),
        mint_root_key: Some(repo::ed25519_verification_key(mint.public_key()).expect("mint key")),
        not_before_unix_seconds: 1000,
        expires_at_unix_seconds: 3600,
        nonce: vec![63; 32],
    };
    let association = wire::SignedOwnerMintRootAttachment {
        owner_signature: Some(wire::AuthorizationSignature {
            signer_key_id: hybrid_codec::key_id(signer("owner").public_key()),
            signature: signer("owner")
                .sign(
                    &heddleco_capability_verifier::creation::mint_root_signing_digest(&attachment)
                        .expect("attachment digest"),
                )
                .expect("owner signature"),
        }),
        attachment: Some(attachment),
    };
    let key = biscuit_auth::KeyPair::from(
        &biscuit_auth::PrivateKey::from_bytes(&[62; 32], biscuit_auth::Algorithm::Ed25519)
            .expect("mint key"),
    );
    let token = biscuit_auth::Biscuit::builder().code(format!(
        "user(\"{}\"); session(\"boundary-acceptor\"); credential_id(\"boundary-credential\"); device_pop_key(\"{}\"); check if operation(\"StartThread\") or operation(\"PublishContent\"); check if resource(\"spool\", \"acme/imports\"); check if time($now), $now < 1970-01-01T01:00:00Z;",
        actor.principal_id, hex::encode(publisher.public_key()),
    )).expect("acceptor restrictions").build_v1(&key).expect("credential");
    let envelope = heddleco_capability_verifier::thread_control_authority::encode(
        owner,
        mint.public_key(),
        Some(
            wire::thread_control_authority::MintRootAssociation::OwnerMintRootAttachment(
                association,
            ),
        ),
        &token,
    )
    .expect("sealed acceptance authority");
    let acceptance = OriginalBoundaryAcceptance {
        version: 1,
        publication_intent: intent.id().expect("intent ID"),
        originals_manifest: manifest.id().expect("manifest ID"),
        original_account: actor.principal_id,
        kinds: if source {
            BTreeSet::from([
                BoundaryOriginalKind::AccountGenesis,
                BoundaryOriginalKind::Source,
            ])
        } else {
            BTreeSet::from([BoundaryOriginalKind::AccountGenesis])
        },
        accepting_publisher: publisher.public_key().try_into().expect("publisher"),
        accepting_author: SourceAuthor::account(
            intent.spool,
            CollaborationActor {
                principal_id: actor.principal_id,
                agent_id: None,
            },
            envelope,
        )
        .expect("acceptor"),
    };
    let signed = crypto::original_boundary_acceptance::SignedBoundaryAcceptance::sign(
        &acceptance,
        &publisher,
    )
    .expect("distinct acceptance signature");
    boundary.signed_acceptance = Some(signed_record(
        objects::object::original_boundary_acceptance::FORMAT,
        signed.canonical,
        signed.signature,
        publisher.public_key(),
    ));
    boundary.originals_manifest = manifest.encode().expect("manifest");
    let witness = signer("witness");
    boundary.original_receipts.clear();
    for entry in &manifest.entries {
        let identity = entry.authority.as_ref().expect("account original");
        let basis = AdmissionBasis::BoundaryAcceptance {
            acceptance: acceptance.id().expect("acceptance ID"),
        };
        let receipt = match &entry.subject {
            objects::object::original_boundary_acceptance::ManifestSubject::Genesis(_) => {
                let receipt = ThreadGenesisAdmission {
                    version: 2,
                    basis,
                    spool: intent.spool,
                    spool_genesis: intent.spool_genesis,
                    thread: entry.thread,
                    owner: identity.actor.principal_id,
                    creator: entry.publisher,
                    authority_digest: ContentHash::compute_typed(
                        objects::object::thread_genesis_admission::ENVELOPE_FORMAT,
                        &bundle.genesis_witnesses[0].creator_authority_envelope,
                    ),
                    executor: witness.public_key().try_into().expect("executor"),
                    admitted_at_ms: 1_100_000,
                };
                let signed = crypto::thread_genesis_admission::SignedGenesisAdmission::sign(
                    &receipt, &witness,
                )
                .expect("receipt");
                signed_record(
                    objects::object::thread_genesis_admission::FORMAT,
                    signed.canonical,
                    signed.signature,
                    witness.public_key(),
                )
            }
            objects::object::original_boundary_acceptance::ManifestSubject::Source(id) => {
                let receipt = ThreadAuthorityAdmission {
                    version: 3,
                    basis,
                    spool: intent.spool,
                    spool_genesis: intent.spool_genesis,
                    thread: entry.thread,
                    subject: OriginalAuthoritySubject::Operation(*id),
                    actor: identity.actor.clone(),
                    publisher: entry.publisher,
                    authority_digest: identity.authority_digest,
                    executor: witness.public_key().try_into().expect("executor"),
                    admitted_at_ms: 1_100_000,
                };
                let signed = crypto::thread_authority_admission::SignedAuthorityAdmission::sign(
                    &receipt, &witness,
                )
                .expect("receipt");
                signed_record(
                    objects::object::thread_authority_admission::FORMAT,
                    signed.canonical,
                    signed.signature,
                    witness.public_key(),
                )
            }
            _ => panic!("genesis/source fixture"),
        };
        boundary.original_receipts.push(receipt);
    }
    boundary
        .original_receipts
        .sort_by_key(|r| api::import_authority::signed_native_digest(r).expect("receipt digest"));
    let binding = boundary.binding.as_mut().expect("binding");
    binding.acceptance_id = acceptance.id().expect("ID").as_bytes().to_vec();
    binding.signed_acceptance_digest = api::import_authority::signed_native_digest(
        boundary.signed_acceptance.as_ref().expect("acceptance"),
    )
    .expect("digest");
    binding.originals_manifest_digest = api::import_authority::boundary_octets_digest(
        "heddle-boundary-originals-manifest-v1",
        &boundary.originals_manifest,
    );
    binding.original_receipt_digests = boundary
        .original_receipts
        .iter()
        .map(|r| api::import_authority::signed_native_digest(r).expect("digest"))
        .collect();
    api::import_authority::verify_boundary_acceptance(&boundary).expect("complete exact evidence");
    let old_genesis = hybrid_codec::canonical(&bundle.genesis_witnesses[0]).expect("payload");
    bundle.genesis_witnesses[0].boundary_acceptance = Some(boundary.clone());
    for signed in &mut bundle.statements {
        let body = signed.body.as_mut().expect("statement");
        if body.purpose == 1 {
            assert_eq!(body.canonical_payload, old_genesis);
            body.canonical_payload =
                hybrid_codec::canonical(&bundle.genesis_witnesses[0]).expect("payload");
        } else {
            let payload = bundle
                .authority_witnesses
                .iter_mut()
                .find(|p| hybrid_codec::canonical(*p).expect("payload") == body.canonical_payload)
                .expect("source payload");
            payload.boundary_acceptances = vec![boundary.clone()];
            body.canonical_payload = hybrid_codec::canonical(payload).expect("payload");
        }
        body.basis = 2;
        body.boundary_acceptance = boundary.binding.clone();
    }
    resign_statements(&mut bundle);
    bundle
}

fn resign_statements(bundle: &mut wire::NativePublicProofBundleV1) {
    for signed in &mut bundle.statements {
        signed.signature = signer("witness")
            .sign(
                &witness_trust::statement_signing_digest(signed.body.as_ref().expect("body"))
                    .expect("digest"),
            )
            .expect("signature");
    }
    bundle.statements.sort_by_key(|s| {
        witness_trust::statement_signing_digest(s.body.as_ref().expect("body")).expect("digest")
    });
}

fn revoke(bundle: &mut wire::NativePublicProofBundleV1, key: &[u8]) {
    let policy = &mut bundle.policies[0];
    let body = policy.body.as_mut().expect("policy");
    body.policy.as_mut().expect("policy").revoked_key_ids = vec![hybrid_codec::key_id(key)];
    body.policy_state_hash = heddleco_capability_verifier::policy::policy_state_hash(body)
        .expect("hash")
        .to_vec();
    policy.owner_signature = Some(wire::AuthorizationSignature {
        signer_key_id: hybrid_codec::key_id(signer("owner").public_key()),
        signature: signer("owner")
            .sign(
                &heddleco_capability_verifier::policy::policy_signature_digest(body)
                    .expect("digest"),
            )
            .expect("signature"),
    });
    for signed in &mut bundle.statements {
        signed.body.as_mut().expect("body").policy_state_hash = body.policy_state_hash.clone();
    }
    resign_statements(bundle);
}

#[test]
fn native_boundary_genesis_fresh_receiver_retains_accepting_authority() {
    let bundle = boundary_bundle(false);
    let originals = vec![
        bundle.genesis_witnesses[0]
            .original_genesis
            .clone()
            .expect("genesis"),
    ];
    local_work::install_bundle("distinct genesis acceptor", bundle, &originals, None);
}

#[test]
fn native_boundary_source_fresh_receiver_retains_accepting_authority() {
    let bundle = boundary_bundle(true);
    let originals = vec![
        bundle.authority_witnesses[0]
            .original
            .clone()
            .expect("source"),
    ];
    local_work::install_bundle("distinct source acceptor", bundle, &originals, None);
}

fn selected_authority_from_control(
    control: &wire::NativePublicProofBundleV1,
    bundle: wire::NativePublicProofBundleV1,
) -> impl AcceptedAuthority + use<> {
    use crate::hybrid::authority::tests::{bundle as imported, selected};
    let limits = heddleco_capability_verifier::VerificationLimits::new(3600).expect("limits");
    let history =
        AcceptedHistory::from_native_spool(control, &selected(&imported(), limits), 1100, limits)
            .expect("independent unrevoked lineage");
    SelectedAuthority::new_native(
        history,
        bundle,
        |_: &wire::NativePublicProofBundleV1,
         _: i64,
         _: &repo::thread_replication::hosted_trust::TrustTransaction<'_>| Ok(()),
    )
}

#[test]
fn native_boundary_acceptor_publisher_and_mint_revocations_reject() {
    for seed in [61, 62] {
        let mut bundle = boundary_bundle(true);
        let control = bundle.clone();
        let key = Ed25519Signer::from_seed(&[seed; 32]).expect("acceptor key");
        let unrevoked = selected_authority_from_control(&control, control.clone());
        for s in &control.statements {
            use heddleco_capability_verifier::thread_control_authority::Revocation;
            let r = if seed == 61 {
                Revocation::Publisher(key.public_key())
            } else {
                Revocation::MintRoot(key.public_key())
            };
            assert!(
                !unrevoked.native_revoked(s.body.as_ref().expect("body"), r),
                "unrevoked acceptor is independently selected before cutting it"
            );
        }
        revoke(
            &mut bundle,
            Ed25519Signer::from_seed(&[seed; 32])
                .expect("acceptor key")
                .public_key(),
        );
        let error = verify_semantics(&bundle, 1_100_000).expect_err("revoked acceptor");
        assert!(
            matches!(
                error.downcast_ref::<crate::hybrid::authority::Error>(),
                Some(crate::hybrid::authority::Error::Rejected(
                    hybrid_codec::Reject::Revoked
                ))
            ),
            "{error}"
        );
    }
}

#[test]
fn native_boundary_revoked_original_keeps_provenance_and_signatures() {
    let mut bundle = boundary_bundle(true);
    let original = bundle.genesis_witnesses[0]
        .original_genesis
        .clone()
        .expect("original");
    // native-host-witness.md:131–135: "The original binding/envelope and original
    // creator signature remain mandatory." "Never assert historical non-revocation"
    revoke(&mut bundle, &original.signatures[0].public_key);
    verify_semantics(&bundle, 1_100_000).expect("revocation does not erase original provenance");
    let sources = vec![
        bundle.authority_witnesses[0]
            .original
            .clone()
            .expect("source"),
    ];
    local_work::install_bundle("revoked original", bundle.clone(), &sources, None);
    assert_eq!(bundle.genesis_witnesses[0].original_genesis, Some(original));
    for purpose in [1, 2] {
        let mut forged = bundle.clone();
        let record = if purpose == 1 {
            forged.genesis_witnesses[0]
                .original_genesis
                .as_mut()
                .expect("genesis")
        } else {
            forged.authority_witnesses[0]
                .original
                .as_mut()
                .expect("source")
        };
        record.signatures[0].signature[0] ^= 1;
        for signed in &mut forged.statements {
            let body = signed.body.as_mut().expect("body");
            if body.purpose == purpose {
                if purpose == 1 {
                    let payload = &forged.genesis_witnesses[0];
                    body.canonical_payload = hybrid_codec::canonical(payload).expect("payload");
                    let mut framed = 1u32.to_be_bytes().to_vec();
                    framed.extend(
                        hybrid_codec::canonical(
                            &payload
                                .original_genesis
                                .as_ref()
                                .expect("original")
                                .signatures[0],
                        )
                        .expect("signature"),
                    );
                    body.original_signatures_digest =
                        hybrid_codec::hash(&[b"heddle-hosted-original-signatures-v1", &framed]);
                } else {
                    let payload = &forged.authority_witnesses[0];
                    body.canonical_payload = hybrid_codec::canonical(payload).expect("payload");
                    let signatures = payload
                        .original
                        .iter()
                        .chain(&payload.dependencies)
                        .flat_map(|r| &r.signatures)
                        .collect::<Vec<_>>();
                    let mut framed = (signatures.len() as u32).to_be_bytes().to_vec();
                    for signature in signatures {
                        framed.extend(hybrid_codec::canonical(signature).expect("signature"));
                    }
                    body.original_signatures_digest =
                        hybrid_codec::hash(&[b"heddle-hosted-original-signatures-v1", &framed]);
                }
            }
        }
        resign_statements(&mut forged);
        let fresh = set(&forged, 1_100_000);
        for signed in &forged.statements {
            WitnessEvidence::resolve(&fresh, signed, None, false, 1_100_000)
                .expect("valid witness signature after forging original");
        }
        let error =
            verify_semantics(&forged, 1_100_000).expect_err("original signature remains mandatory");
        assert!(
            matches!(
                error.downcast_ref::<crate::hybrid::authority::Error>(),
                Some(crate::hybrid::authority::Error::Rejected(
                    hybrid_codec::Reject::Signature
                ))
            ),
            "{error:?}"
        );
    }
}

#[test]
fn ordinary_native_p1_p2_revocations_still_reject() {
    use heddleco_capability_verifier::thread_control_authority::Revocation;

    use crate::hybrid::authority::tests::{bundle as imported, selected};

    for name in ["start_thread", "account_source"] {
        let mut bundle: wire::NativePublicProofBundleV1 = record(&fixture(), name);
        verify_semantics(&bundle, 1_100_000).expect("ordinary control");
        let publisher = bundle.genesis_witnesses[0]
            .original_genesis
            .as_ref()
            .expect("original")
            .signatures[0]
            .public_key
            .clone();
        let control = bundle.clone();
        revoke(&mut bundle, &publisher);
        let limits = heddleco_capability_verifier::VerificationLimits::new(3600).expect("limits");
        let history = AcceptedHistory::from_native_spool(
            &control,
            &selected(&imported(), limits),
            1100,
            limits,
        )
        .expect("selected lineage");
        let authority = SelectedAuthority::new_native(
            history,
            bundle.clone(),
            |_: &wire::NativePublicProofBundleV1,
             _: i64,
             _: &repo::thread_replication::hosted_trust::TrustTransaction<'_>| Ok(()),
        );
        for statement in bundle
            .statements
            .iter()
            .map(|s| s.body.as_ref().expect("body"))
        {
            assert!(
                authority.native_revoked(statement, Revocation::Publisher(&publisher)),
                "purpose {}",
                statement.purpose
            );
        }
        assert!(
            verify_semantics(&bundle, 1_100_000).is_err(),
            "ordinary revocation still rejects"
        );
    }
}

#[test]
fn native_boundary_revocation_selects_exact_acceptor_credential_identities() {
    use heddleco_capability_verifier::thread_control_authority::Revocation;

    use crate::hybrid::authority::tests::{bundle as imported, selected};

    let bundle = boundary_bundle(true);
    let limits = heddleco_capability_verifier::VerificationLimits::new(3600).expect("limits");
    let history =
        AcceptedHistory::from_native_spool(&bundle, &selected(&imported(), limits), 1100, limits)
            .expect("selected lineage");
    let authority = SelectedAuthority::new_native(
        history,
        bundle.clone(),
        |_: &wire::NativePublicProofBundleV1,
         _: i64,
         _: &repo::thread_replication::hosted_trust::TrustTransaction<'_>| Ok(()),
    );
    let acceptance = OriginalBoundaryAcceptance::decode(
        bundle.genesis_witnesses[0]
            .boundary_acceptance
            .as_ref()
            .expect("boundary")
            .signed_acceptance
            .as_ref()
            .expect("acceptance")
            .canonical_record
            .as_slice(),
    )
    .expect("acceptance");
    let SourceAuthor::Account {
        authority: envelope,
        ..
    } = acceptance.accepting_author
    else {
        panic!("account");
    };
    let envelope: wire::ThreadControlAuthority =
        hybrid_codec::strict_decode(&envelope, 65536).expect("envelope");
    let root = biscuit_verifier::parse_ed25519_public_keys_hex(
        &hex::encode(&envelope.mint_root_public_key),
        1,
    )
    .expect("mint root")[0];
    let token = biscuit_verifier::signature_v1::verify(&envelope.sealed_biscuit, root)
        .expect("signed credential");
    let facts = biscuit_verifier::inspect_verified_credential(&token, &root).expect("identities");
    assert!(
        !facts.revocation_ids.is_empty(),
        "signed-block identities are exercised"
    );
    let original_envelope: wire::ThreadControlAuthority = hybrid_codec::strict_decode(
        &bundle.genesis_witnesses[0].creator_authority_envelope,
        65536,
    )
    .expect("original envelope");
    let original_root = biscuit_verifier::parse_ed25519_public_keys_hex(
        &hex::encode(&original_envelope.mint_root_public_key),
        1,
    )
    .expect("original root")[0];
    let original_token =
        biscuit_verifier::signature_v1::verify(&original_envelope.sealed_biscuit, original_root)
            .expect("original credential signature");
    let original_facts =
        biscuit_verifier::inspect_verified_credential(&original_token, &original_root)
            .expect("original identities");
    for statement in bundle
        .statements
        .iter()
        .map(|s| s.body.as_ref().expect("body"))
    {
        assert!(!authority.native_revoked(
            statement,
            Revocation::MintRoot(&envelope.mint_root_public_key)
        ));
        assert!(!authority.native_revoked(
            statement,
            Revocation::Publisher(&acceptance.accepting_publisher)
        ));
        for id in facts.revocation_identities() {
            assert!(
                !authority.native_revoked(statement, Revocation::Credential(id)),
                "{id}"
            );
        }
        for id in original_facts.revocation_identities() {
            assert!(
                authority.native_revoked(statement, Revocation::Credential(id)),
                "original {id} is provenance only"
            );
        }
        assert!(authority.native_revoked(statement, Revocation::Credential("unrelated-session")));
        assert!(authority.native_revoked(
            statement,
            Revocation::Publisher(signer("device").public_key())
        ));
        assert!(authority.native_revoked(
            statement,
            Revocation::MintRoot(signer("device").public_key())
        ));
    }
}

#[test]
fn native_boundary_forged_acceptor_key_outside_acceptance_signature_rejects() {
    let mut bundle = boundary_bundle(true);
    let unrevoked = selected_authority_from_control(&bundle, bundle.clone());
    for s in &bundle.statements {
        assert!(
            !unrevoked.native_revoked(
                s.body.as_ref().expect("body"),
                heddleco_capability_verifier::thread_control_authority::Revocation::Publisher(
                    Ed25519Signer::from_seed(&[61; 32])
                        .expect("acceptor")
                        .public_key()
                )
            )
        );
    }
    verify_semantics(&bundle, 1_100_000).expect("unforged acceptor passes");
    let old = hybrid_codec::canonical(&bundle.genesis_witnesses[0]).expect("payload");
    let mut boundary = bundle.genesis_witnesses[0]
        .boundary_acceptance
        .clone()
        .expect("boundary");
    let signed = boundary.signed_acceptance.as_mut().expect("acceptance");
    let wrong = Ed25519Signer::from_seed(&[64; 32]).expect("unrelated signer");
    let bytes = [signed.format.as_bytes(), b"\0", &signed.canonical_record].concat();
    signed.signatures[0].public_key = wrong.public_key().to_vec();
    signed.signatures[0].signature = wrong.sign(&bytes).expect("valid wrong-key signature");
    boundary
        .binding
        .as_mut()
        .expect("binding")
        .signed_acceptance_digest =
        api::import_authority::signed_native_digest(signed).expect("digest");
    // The portable framing gate authenticates the supplied signature, while
    // native selection must also bind that key to the canonical acceptor.
    api::import_authority::verify_boundary_acceptance(&boundary)
        .expect("valid transport signature");
    bundle.genesis_witnesses[0].boundary_acceptance = Some(boundary.clone());
    for signed in &mut bundle.statements {
        let body = signed.body.as_mut().expect("body");
        if body.purpose == 1 {
            assert_eq!(body.canonical_payload, old);
            body.canonical_payload =
                hybrid_codec::canonical(&bundle.genesis_witnesses[0]).expect("payload");
        } else {
            let payload = &mut bundle.authority_witnesses[0];
            payload.boundary_acceptances = vec![boundary.clone()];
            body.canonical_payload = hybrid_codec::canonical(payload).expect("payload");
        }
        body.boundary_acceptance = boundary.binding.clone();
    }
    resign_statements(&mut bundle);
    assert!(
        verify_semantics(&bundle, 1_100_000).is_err(),
        "acceptor must sign its own acceptance"
    );
    use crate::hybrid::authority::PublicEvidence;
    for statement in &bundle.statements {
        assert!(
            bundle
                .native_authorities(statement.body.as_ref().expect("body"))
                .is_none(),
            "unbound acceptor fails closed during identity selection"
        );
    }
}
