use crypto::{Ed25519Signer, thread_operation::SignedOperation};
use heddle_object_model::object::{
    Attribution, CollaborationActor, Principal, State, Tree,
    thread_replication::{
        AuthoredCapture, GenesisOwner, ThreadOperation, ThreadOperationBody,
        metadata::{Control, ThreadControl},
    },
};

use super::*;

fn signer() -> Ed25519Signer {
    Ed25519Signer::from_seed(&[61; 32]).expect("original key")
}
fn acceptor() -> Ed25519Signer {
    Ed25519Signer::from_seed(&[62; 32]).expect("acceptor key")
}
fn spool() -> Uuid {
    Uuid::from_u128(501)
}
fn account() -> Uuid {
    Uuid::from_u128(502)
}
fn spool_genesis() -> ContentHash {
    ContentHash::compute(b"independently verified Spool genesis")
}
fn author(agent: &str) -> SourceAuthor {
    // This fixture tests explicit cryptographic selection only. These bounded
    // bytes are not claimed to be a currently authorized accepting credential.
    SourceAuthor::account(
        spool(),
        CollaborationActor {
            principal_id: account(),
            agent_id: Some(agent.into()),
        },
        vec![7; 64],
    )
    .expect("signed authority binding")
}
fn operation_wire(operation: &ThreadOperation) -> SignedRecord {
    let signed = SignedOperation::sign(operation, &signer()).expect("original signature");
    SignedRecord {
        format: heddle_object_model::object::thread_replication::OPERATION_FORMAT.into(),
        canonical_record: signed.canonical,
        signatures: vec![RecordSignature {
            public_key: operation.publisher.to_vec(),
            signature: signed.signature,
        }],
    }
}
fn fixture(local: bool) -> (PublishContentClientFrame, PublicationOriginals, State, Tree) {
    let tree = Tree::new();
    let genesis = ThreadGenesis {
        version: 1,
        spool: spool().to_string(),
        parent: None,
        base: StateId::from_bytes([9; 32]),
        name: "original".into(),
        intent: "offline".into(),
        owner: if local {
            GenesisOwner::LocalKey(signer().public_key().try_into().expect("key"))
        } else {
            GenesisOwner::Account(account())
        },
        creator: signer().public_key().try_into().expect("key"),
        nonce: vec![1],
    };
    let state = State::new_snapshot(
        tree.hash(),
        vec![genesis.base],
        Attribution::human(Principal::new("offline", "")),
    );
    let operation = ThreadOperation {
        version: 1,
        thread: genesis.id().expect("genesis ID"),
        parents: BTreeSet::new(),
        publisher: genesis.creator,
        body: ThreadOperationBody::Capture(AuthoredCapture {
            result: state.encode_current_msgpack().expect("state").into(),
            author: if local {
                SourceAuthor::LocalKey
            } else {
                author("original-agent")
            },
        }),
    };
    let thread = ThreadRef {
        spool: Some(SpoolRef {
            id: spool().to_string(),
        }),
        id: Some(ThreadId {
            value: operation.thread.as_bytes().to_vec(),
        }),
    };
    let extent = |n, kind| {
        let address = ObjectAddress {
            algorithm: "blake3".into(),
            digest: vec![n; 32],
        };
        PackExtent {
            pack: Some(address.clone()),
            kind: kind as i32,
            offset: 0,
            length: 100,
            extent_digest: Some(address),
        }
    };
    let open = PublishContentClientFrame {
        client_operation_id: Uuid::from_u128(503).to_string(),
        body: Some(publish_content_client_frame::Body::Open(
            PublishContentOpen {
                thread: Some(thread.clone()),
                revision: Some(RevisionRef {
                    spool: thread.spool.clone(),
                    revision: Some(revision_ref::Revision::State(
                        api::heddle::api::v1alpha1::StateId {
                            value: state.id().as_bytes().to_vec(),
                        },
                    )),
                }),
                packs: vec![
                    extent(1, pack_extent::Kind::NativePack),
                    extent(2, pack_extent::Kind::NativeIndex),
                ],
                source: Some(EndpointRef {
                    public_key: vec![31; 32],
                    kind: EndpointKind::Device as i32,
                }),
                destination: Some(EndpointRef {
                    public_key: vec![32; 32],
                    kind: EndpointKind::Weft as i32,
                }),
                ..Default::default()
            },
        )),
    };
    let originals = PublicationOriginals {
        geneses: vec![ThreadGenesisRecord {
            genesis: Some(
                crate::replication::opening::sign_genesis(&genesis, &signer())
                    .expect("genesis signature"),
            ),
            creator_authority: if local { vec![] } else { vec![4; 64] },
            ..Default::default()
        }],
        operations: vec![ReplicationOperations {
            operations: vec![operation_wire(&operation)],
            ..Default::default()
        }],
    };
    (open, originals, state, tree)
}
fn signed_publication(local: bool) -> PreparedPublication {
    let (opening, originals, _, _) = fixture(local);
    let mut prepared = PreparedPublication::new(opening, originals, spool_genesis())
        .expect("prepare exact originals");
    prepared
        .sign_acceptance(
            author("explicit-acceptor"),
            [BoundaryOriginalKind::Source].into(),
            &acceptor(),
        )
        .expect("explicit acceptance");
    prepared
}
fn rejection<T>(result: Result<T, Error>, text: &str) {
    let Err(error) = result else {
        panic!("expected rejection: {text}")
    };
    assert!(error.to_string().contains(text), "{error}");
}
#[test]
fn publication_acceptance_uses_full_shared_manifest_without_reauthoring() {
    let (opening, originals, _, _) = fixture(false);
    let exact = originals.operations[0].operations[0].clone();
    let mut prepared =
        PreparedPublication::new(opening, originals, spool_genesis()).expect("prepare");
    assert!(
        prepared.plan().proposed().is_empty(),
        "preparing does not implicitly accept work"
    );
    assert_eq!(prepared.plan().manifest().entries.len(), 2);
    let genesis = &prepared.plan().manifest().entries[0];
    assert!(matches!(genesis.subject, ManifestSubject::Genesis(_)));
    assert_eq!(
        genesis.authority.as_ref().expect("account").actor.agent_id,
        None,
        "genesis does not claim human or agent identity"
    );
    let id = prepared
        .sign_acceptance(
            author("explicit-acceptor"),
            [
                BoundaryOriginalKind::Source,
                BoundaryOriginalKind::AccountGenesis,
            ]
            .into(),
            &acceptor(),
        )
        .expect("sign explicit acceptance");
    assert_eq!(prepared.originals().operations[0].operations[0], exact);
    assert_eq!(prepared.plan().proposed()[&id].subjects().len(), 2);
    let expected_manifest = prepared.plan().manifest().clone();
    let expected_intent = prepared.plan().intent().clone();
    let (opening, originals, _) = prepared.into_parts();
    // Generic live/genesis matching stays fail-closed, even for a valid explicit
    // signature. Only full publication staging can consume fresh candidates.
    assert!(
        crate::replication::opening::verify_genesis_record(
            &originals.geneses[0],
            match &opening.body {
                Some(publish_content_client_frame::Body::Open(open)) =>
                    open.thread.as_ref().expect("thread"),
                _ => panic!("open"),
            }
        )
        .is_err()
    );
    let (stripped, plan) = proposed_publication(&opening, originals, spool_genesis())
        .expect("host recomputes same full selection");
    assert_eq!(plan.manifest(), &expected_manifest);
    assert_eq!(plan.intent(), &expected_intent);
    assert_eq!(stripped.operations[0].operations[0], exact);
    assert!(stripped.geneses[0].boundary_acceptances.is_empty());
}
#[test]
fn publication_acceptance_rejects_changed_destination_originals_and_operation() {
    for change in 0..6 {
        let prepared = signed_publication(false);
        let (mut opening, mut originals, _) = prepared.into_parts();
        match change {
            0 => {
                if let Some(publish_content_client_frame::Body::Open(open)) = &mut opening.body {
                    open.destination.as_mut().expect("destination").public_key[0] ^= 1;
                }
            }
            1 => {
                let mut original = crate::replication::decode_record(
                    originals.operations[0].operations[0].clone(),
                )
                .expect("original")
                .verify()
                .expect("signature");
                original
                    .parents
                    .insert(ContentHash::compute(b"additional original dependency"));
                originals.operations[0].operations[0] = operation_wire(&original);
            }
            2 => {
                let mut original = crate::replication::decode_record(
                    originals.operations[0].operations[0].clone(),
                )
                .expect("original")
                .verify()
                .expect("signature");
                original
                    .parents
                    .insert(ContentHash::compute(b"extra original"));
                originals.operations[0]
                    .operations
                    .push(operation_wire(&original));
            }
            3 => opening.client_operation_id = Uuid::from_u128(999).to_string(),
            4 => originals.geneses[0].creator_authority.push(1),
            _ => {
                if let Some(publish_content_client_frame::Body::Open(open)) = &mut opening.body {
                    open.packs[0].length += 1;
                }
            }
        }
        rejection(
            proposed_publication(&opening, originals, spool_genesis()),
            "differs from exact publication",
        );
    }
    let prepared = signed_publication(false);
    let (mut opening, originals, _) = prepared.into_parts();
    if let Some(publish_content_client_frame::Body::Open(open)) = &mut opening.body {
        open.checkpoint = Some(TransferCheckpoint {
            committed_bytes: 123,
            ..Default::default()
        });
    }
    assert!(
        proposed_publication(&opening, originals, spool_genesis()).is_ok(),
        "resume cursor is not immutable intent"
    );
}
#[test]
fn publication_acceptance_rejects_unused_duplicate_metadata_and_local_relabeling() {
    let prepared = signed_publication(false);
    let (opening, mut originals, _) = prepared.into_parts();
    let candidate = originals.geneses[0].boundary_acceptances[0].clone();
    originals.operations[0].boundary_acceptances.push(candidate);
    rejection(
        proposed_publication(&opening, originals, spool_genesis()),
        "duplicate fresh acceptance",
    );
    let (opening, mut originals, _, _) = fixture(false);
    let original = crate::replication::decode_record(originals.operations[0].operations[0].clone())
        .expect("original")
        .verify()
        .expect("signature");
    let SourceAuthor::Account {
        actor,
        authority_digest,
        authority,
        ..
    } = author("original-agent")
    else {
        panic!("account")
    };
    let control = ThreadControl {
        version: 1,
        spool: spool(),
        actor,
        authority_digest,
        authority_envelope: authority,
        client_operation_id: Uuid::from_u128(700),
        occurred_at_ms: 1,
        control: Control::Name("authored metadata".into()),
    };
    originals.operations[0].operations[0] = operation_wire(&ThreadOperation {
        body: ThreadOperationBody::Metadata(control.encode().expect("control")),
        ..original
    });
    let mut prepared =
        PreparedPublication::new(opening, originals, spool_genesis()).expect("metadata manifest");
    rejection(
        prepared.sign_acceptance(
            author("acceptor"),
            [BoundaryOriginalKind::Source].into(),
            &acceptor(),
        ),
        "selects no original",
    );
    let (opening, originals, _, _) = fixture(true);
    let mut prepared =
        PreparedPublication::new(opening, originals, spool_genesis()).expect("local provenance");
    rejection(
        prepared.sign_acceptance(
            author("acceptor"),
            [
                BoundaryOriginalKind::Source,
                BoundaryOriginalKind::AccountGenesis,
            ]
            .into(),
            &acceptor(),
        ),
        "selects no original",
    );
    assert!(
        prepared.originals().geneses[0].ownership_claims.is_empty(),
        "an acceptance never fabricates explicit ownership"
    );
    let mut wrong = prepared.plan().manifest().clone();
    wrong.entries[0].authority = Some(
        heddle_object_model::object::thread_authority_admission::OriginalAuthorityBinding {
            spool: spool(),
            actor: CollaborationActor {
                principal_id: account(),
                agent_id: Some("unsigned-agent".into()),
            },
            authority_digest: ContentHash::compute(b"proof"),
        },
    );
    assert!(
        wrong
            .encode()
            .expect_err("no unsigned genesis agent")
            .to_string()
            .contains("unsigned agent")
    );
}
#[test]
fn publication_acceptance_actual_artifacts_preserve_selection_and_cleanup_twice() {
    use heddle_pack::store::pack::{ObjectType, PackBuilder, PackObjectId};
    let scratch = tempfile::tempdir().expect("scratch");
    for _ in 0..2 {
        let (mut opening, originals, state, tree) = fixture(false);
        let mut builder = PackBuilder::for_repack(Default::default(), 0);
        builder.add_id(
            PackObjectId::StateId(state.id()),
            ObjectType::State,
            state.encode_current_msgpack().expect("state"),
        );
        builder.add_id(
            PackObjectId::Hash(tree.hash()),
            ObjectType::Tree,
            tree.encode_canonical().expect("tree"),
        );
        let (pack, index, _) = builder.build().expect("pack");
        let directory = tempfile::Builder::new()
            .prefix("fresh-publication-")
            .tempdir_in(scratch.path())
            .expect("staging");
        if let Some(publish_content_client_frame::Body::Open(open)) = &mut opening.body {
            for ((extent, name), bytes) in open
                .packs
                .iter_mut()
                .zip(["source.pack", "source.idx"])
                .zip([pack, index])
            {
                std::fs::write(directory.path().join(name), &bytes).expect("artifact");
                let address = ObjectAddress {
                    algorithm: "blake3".into(),
                    digest: blake3::hash(&bytes).as_bytes().to_vec(),
                };
                extent.pack = Some(address.clone());
                extent.extent_digest = Some(address);
                extent.length = bytes.len() as u64;
            }
        }
        let mut prepared =
            PreparedPublication::new(opening, originals, spool_genesis()).expect("prepare");
        prepared
            .sign_acceptance(
                author("acceptor"),
                [BoundaryOriginalKind::Source].into(),
                &acceptor(),
            )
            .expect("explicit selection");
        let (opening, originals, _) = prepared.into_parts();
        let staged = super::super::validate_proposed_source_artifacts(
            directory,
            &opening,
            originals,
            spool_genesis(),
        )
        .expect("actual closure plus proposed selection");
        assert_eq!(staged.artifacts().state().id(), state.id());
        assert_eq!(staged.acceptances().proposed().len(), 1);
        assert!(
            staged
                .artifacts()
                .artifact_paths()
                .iter()
                .all(|p| p.exists())
        );
        drop(staged);
        assert_eq!(
            std::fs::read_dir(scratch.path()).expect("scratch").count(),
            0
        );
    }
}
#[test]
fn publication_claim_selection_retains_both_original_signatures_and_actor() {
    use crypto::thread_ownership_claim::SignedOwnershipClaim;
    use heddle_object_model::object::thread_replication::ownership_claim::ThreadOwnershipClaim;
    let (opening, mut originals, _, _) = fixture(true);
    let original = crate::replication::decode_record(originals.operations[0].operations[0].clone())
        .expect("original")
        .verify()
        .expect("signature");
    let claim = ThreadOwnershipClaim {
        version: 1,
        thread: original.thread,
        prior_local_key: original.publisher,
        accepting_publisher: acceptor().public_key().try_into().expect("key"),
        acceptance: author("original-claim-agent"),
        source_frontier: [original.id().expect("original ID")].into(),
    };
    let proof = SignedOwnershipClaim::sign(&claim, &signer(), &acceptor())
        .expect("both original signatures");
    let exact = crate::thread_ownership::encode(&proof).expect("claim wire");
    originals.geneses[0].ownership_claims.push(exact.clone());
    let mut prepared = PreparedPublication::new(opening, originals, spool_genesis())
        .expect("explicit claim provenance");
    let id = prepared
        .sign_acceptance(
            author("new-acceptor"),
            [BoundaryOriginalKind::OwnershipClaim].into(),
            &acceptor(),
        )
        .expect("fresh claim selection");
    let subject = ManifestSubject::OwnershipClaim(claim.id().expect("claim ID"));
    assert_eq!(
        prepared.plan().proposed()[&id].subjects(),
        &BTreeSet::from([subject.clone()])
    );
    let descriptor = prepared
        .plan()
        .manifest()
        .entries
        .iter()
        .find(|entry| entry.subject == subject)
        .expect("claim descriptor");
    assert_eq!(
        descriptor
            .authority
            .as_ref()
            .expect("authority")
            .actor
            .agent_id
            .as_deref(),
        Some("original-claim-agent")
    );
    assert_eq!(prepared.originals().geneses[0].ownership_claims[0], exact);
    let (opening, mut originals, _) = prepared.into_parts();
    originals.geneses[0].ownership_claims[0].signatures[0].signature[0] ^= 1;
    assert!(
        proposed_publication(&opening, originals, spool_genesis()).is_err(),
        "fresh acceptance cannot replace the local owner's co-signature"
    );
}

#[test]
fn publication_acceptance_bounds_and_unused_selection_are_load_bearing() {
    let mut prepared = signed_publication(false);
    let duplicate = prepared
        .plan()
        .proposed()
        .values()
        .next()
        .expect("proposal")
        .signed()
        .as_ref()
        .clone();
    rejection(prepared.accept(duplicate), "duplicate fresh acceptance");
    let value = prepared
        .acceptance(
            author("other-delegate"),
            acceptor().public_key().try_into().expect("key"),
            [BoundaryOriginalKind::Source].into(),
        )
        .expect("same selection different signer claim");
    rejection(
        prepared.accept(SignedBoundaryAcceptance::sign(&value, &acceptor()).expect("signature")),
        "overlapping fresh acceptance selection",
    );
    let (opening, mut originals, _) = signed_publication(false).into_parts();
    let record = originals.geneses[0].boundary_acceptances[0].clone();
    originals.geneses[0].boundary_acceptances = vec![record; 129];
    assert!(
        proposed_publication(&opening, originals, spool_genesis()).is_err(),
        "bounded before candidate cloning/decoding"
    );
    let (opening, originals, _, _) = fixture(false);
    let prepared = PreparedPublication::new(opening, originals, spool_genesis()).expect("prepare");
    let mut value = prepared
        .acceptance(
            author("acceptor"),
            acceptor().public_key().try_into().expect("key"),
            [BoundaryOriginalKind::Source].into(),
        )
        .expect("selection");
    let foreign = Uuid::from_u128(888);
    value.original_account = foreign;
    value.accepting_author = SourceAuthor::account(
        spool(),
        CollaborationActor {
            principal_id: foreign,
            agent_id: None,
        },
        vec![8],
    )
    .expect("foreign account signature claim");
    let signed =
        SignedBoundaryAcceptance::sign(&value, &acceptor()).expect("explicit foreign signature");
    let (opening, mut originals, _) = prepared.into_parts();
    originals.geneses[0]
        .boundary_acceptances
        .push(crate::boundary_acceptance::encode(&signed).expect("wire"));
    rejection(
        proposed_publication(&opening, originals, spool_genesis()),
        "selects no original",
    );
}

#[cfg(feature = "native")]
#[tokio::test]
async fn publication_prepare_sign_send_uses_one_exchange_without_identity_refresh() {
    use objects::store::{FsStore, ObjectStore};
    let (opening, originals, state, tree) = fixture(false);
    let scratch = tempfile::tempdir().expect("source scratch");
    let store = FsStore::new(scratch.path().join("objects"));
    store.init().expect("store");
    store.put_tree(&tree).expect("selected tree");
    let pack = super::super::SourcePack::prepare(
        &store,
        &state,
        scratch.path(),
        super::super::SourceBudget {
            max_objects: 16,
            max_decoded_bytes: 1024 * 1024,
        },
    )
    .expect("selected source");
    // This capacity-one transport permits only PublishContent, drains incoming
    // originals before packs, and returns a complete exact inventory receipt.
    // It is a client transport test, not fresh hosted authority admission.
    let (remote, _, _) = super::super::tests::fixture(false);
    let Some(publish_content_client_frame::Body::Open(open)) = opening.body else {
        panic!("Open")
    };
    let thread = remote.thread(open.thread.expect("Thread"));
    let mut prepared = thread
        .prepare_publication(
            &pack,
            originals,
            super::super::PublicationOptions {
                client_operation_id: opening.client_operation_id,
                source: open.source.expect("source"),
                sharing_policy_version: vec![],
                checkpoint: None,
            },
            spool_genesis(),
        )
        .expect("local preparation");
    prepared
        .sign_acceptance(
            author("explicit-acceptor"),
            [BoundaryOriginalKind::Source].into(),
            &acceptor(),
        )
        .expect("explicit sign");
    let receipt = thread
        .send_prepared(&pack, &prepared)
        .await
        .expect("one source exchange");
    assert_eq!(
        receipt.client_operation_id,
        prepared.opening().client_operation_id
    );
    assert_eq!(
        receipt
            .accepted_inventory
            .as_ref()
            .expect("inventory")
            .digest,
        pack.inventory_digest().expect("same inventory")
    );
}

#[test]
fn publication_fresh_carrier_count_is_checked_before_signature_decoding() {
    let (opening, originals, _, _) = fixture(false);
    let prepared = PreparedPublication::new(opening, originals, spool_genesis()).expect("prepare");
    let base = prepared
        .acceptance(
            author("acceptor"),
            acceptor().public_key().try_into().expect("key"),
            [BoundaryOriginalKind::Source].into(),
        )
        .expect("selection");
    let (_, mut originals, _) = prepared.into_parts();
    for n in 0..129 {
        let value = OriginalBoundaryAcceptance {
            accepting_author: author(&format!("explicit-acceptor-{n}")),
            ..base.clone()
        };
        originals.geneses[0].boundary_acceptances.push(
            crate::boundary_acceptance::encode(
                &SignedBoundaryAcceptance::sign(&value, &acceptor())
                    .expect("valid distinct signature"),
            )
            .expect("wire"),
        );
    }
    let error = originals
        .validate_bounds()
        .expect_err("129 distinct acceptances exceed shared publication count");
    assert!(
        error
            .to_string()
            .contains("boundary acceptance count exceeded"),
        "{error}"
    );
}

#[test]
fn fresh_manifest_binds_even_the_unsigned_genesis_envelope_sidecar() {
    let (opening, mut originals, _) = signed_publication(false).into_parts();
    originals.geneses[0].creator_authority.push(42);
    assert!(
        proposed_publication(&opening, originals, spool_genesis()).is_err(),
        "changed creator envelope must invalidate the full manifest acceptance"
    );
}
#[test]
fn fresh_intent_binds_the_exact_receiving_endpoint() {
    let (mut opening, originals, _) = signed_publication(false).into_parts();
    if let Some(publish_content_client_frame::Body::Open(open)) = &mut opening.body {
        open.destination.as_mut().expect("destination").public_key[0] ^= 1;
    }
    assert!(
        proposed_publication(&opening, originals, spool_genesis()).is_err(),
        "fresh acceptance must reject another receiving endpoint"
    );
}
#[test]
fn fresh_genesis_manifest_cannot_invent_an_agent() {
    let (opening, originals, _, _) = fixture(false);
    let prepared = PreparedPublication::new(opening, originals, spool_genesis()).expect("prepare");
    let mut manifest = prepared.plan().manifest().clone();
    manifest.entries[0]
        .authority
        .as_mut()
        .expect("account descriptor")
        .actor
        .agent_id = Some("unsigned-agent".into());
    assert!(
        manifest.encode().is_err(),
        "genesis descriptor must not assert an unsigned agent"
    );
}

#[test]
fn explicit_fresh_acceptance_cannot_reclassify_already_retained_evidence() {
    use crypto::thread_authority_admission::SignedAuthorityAdmission;
    use heddle_object_model::object::thread_authority_admission::{
        OriginalAuthorityBinding, OriginalAuthoritySubject, ThreadAuthorityAdmission,
    };
    let prepared = signed_publication(false);
    let signed = prepared
        .plan()
        .proposed()
        .values()
        .next()
        .expect("proposal")
        .signed()
        .as_ref()
        .clone();
    let id = signed
        .verify_signature()
        .expect("acceptance")
        .id()
        .expect("acceptance ID");
    let (opening, mut originals, _) = prepared.into_parts();
    let original = crate::replication::decode_record(originals.operations[0].operations[0].clone())
        .expect("original")
        .verify()
        .expect("signature");
    let binding = OriginalAuthorityBinding::from_operation(&original)
        .expect("binding")
        .expect("account");
    let receipt = ThreadAuthorityAdmission {
        version: 3,
        basis: AdmissionBasis::BoundaryAcceptance { acceptance: id },
        spool: spool(),
        spool_genesis: spool_genesis(),
        thread: original.thread,
        subject: OriginalAuthoritySubject::Operation(original.id().expect("ID")),
        actor: binding.actor,
        publisher: original.publisher,
        authority_digest: binding.authority_digest,
        executor: acceptor().public_key().try_into().expect("executor key"),
        admitted_at_ms: 9000,
    };
    originals.operations[0].authority_admissions.push(
        crate::authority_admission::encode(
            &SignedAuthorityAdmission::sign(&receipt, &acceptor()).expect("executor receipt"),
        )
        .expect("receipt wire"),
    );
    originals.operations[0].boundary_acceptances =
        std::mem::take(&mut originals.geneses[0].boundary_acceptances);
    let mut prepared = PreparedPublication::new(opening, originals, spool_genesis()).expect(
        "retained receipt structurally matches original; receiver still independently pins issuer",
    );
    assert!(prepared.plan().proposed().is_empty());
    rejection(
        prepared.accept(signed),
        "duplicate fresh acceptance or retained evidence",
    );
    assert!(
        prepared.plan().proposed().is_empty(),
        "rejected reclassification leaves plan unchanged"
    );
    assert!(
        prepared.originals().geneses[0]
            .boundary_acceptances
            .is_empty(),
        "no candidate carrier appended on rejection"
    );
}
