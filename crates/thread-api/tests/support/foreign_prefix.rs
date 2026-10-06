//! Genuine signed native prefixes with a foreign edge to each selected predecessor.
use api::{heddle::api::v1alpha2 as wire, hybrid_codec as codec, witness_trust};
use crypto::{
    Ed25519Signer, Signer,
    thread_operation::{SignedGenesis, SignedOperation},
};
use objects::object::{
    Attribution, Principal, State, Tree,
    thread_replication::{
        AuthoredCapture, ThreadOperationBody, initial_base::synthetic_initial_base,
    },
};
use prost::Message;
use repo::thread_replication::authority::{PublicEvidence, PublicProof};

#[derive(Clone)]
pub(crate) struct Prefix {
    pub proof: PublicProof,
    pub original: wire::SignedRecord,
    pub originals: Vec<wire::SignedRecord>,
}

pub(crate) fn graph(edges: &[Vec<usize>]) -> Vec<Prefix> {
    use biscuit_verifier::signature_v1::BiscuitBuilderV1Ext;
    let f: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/writer-authority-alpha35.json"))
            .expect("writer fixture");
    let template = wire::NativePublicProofBundleV1::decode(
        hex::decode(
            f["vectors"]["p2_owner_bundle"]["wire_hex"]
                .as_str()
                .expect("wire"),
        )
        .expect("hex")
        .as_slice(),
    )
    .expect("template");
    let signer = |role: &str| {
        Ed25519Signer::from_seed(
            &hex::decode(f["keys"][role]["seed_hex"].as_str().expect("seed")).expect("hex"),
        )
        .expect("signer")
    };
    let device = signer("device");
    let witness = signer("witness");
    let template_genesis = &template.genesis_witnesses[0];
    let (_, base_genesis) = crypto::import_authority::verify_native_genesis(
        template_genesis.original_genesis.as_ref().expect("genesis"),
    )
    .expect("genesis signature");
    let base_p2 = &template.authority_witnesses[0];
    let (_, base_operation) = crypto::import_authority::verify_native_operation(
        base_p2.original.as_ref().expect("operation"),
    )
    .expect("operation signature");
    let ThreadOperationBody::Metadata(bytes) = &base_operation.body else {
        panic!("metadata template")
    };
    let control = objects::object::thread_replication::metadata::ThreadControl::decode(bytes)
        .expect("author");
    let mut envelope = wire::ThreadControlAuthority::decode(base_p2.authority_envelope.as_slice())
        .expect("authority");
    let seed = hex::decode(f["keys"]["device"]["seed_hex"].as_str().expect("seed")).expect("hex");
    let pair = biscuit_auth::KeyPair::from(
        &biscuit_auth::PrivateKey::from_bytes(&seed, biscuit_auth::Algorithm::Ed25519)
            .expect("mint"),
    );
    let token = biscuit_auth::Biscuit::builder().code(format!("user(\"{}\"); session(\"prefix-chain\"); device_pop_key(\"{}\"); check if operation(\"PublishContent\"); check if resource(\"spool\", \"acme/imports\"); check if time($now), $now < 1970-01-01T00:30:00Z;", control.actor.principal_id, hex::encode(device.public_key()))).expect("facts").build_v1(&pair).expect("capability");
    envelope.sealed_biscuit = token.seal().expect("seal").to_vec().expect("bytes");
    let envelope = envelope.encode_to_vec();
    let foreign: serde_json::Value = serde_json::from_str(include_str!(
        "../fixtures/foreign-dependencies-alpha34.json"
    ))
    .expect("mixed fixture");
    let record = |name: &str| {
        hex::decode(
            foreign["wire_vectors"][name]["wire_hex"]
                .as_str()
                .expect("wire"),
        )
        .expect("hex")
    };
    let mut imported = wire::ImportPublicProofBundleV1::decode(record("import_stage").as_slice())
        .expect("import stage");
    let import_tips = ["import_tip_0", "import_tip_1"]
        .map(|n| wire::SignedRecord::decode(record(n).as_slice()).expect("tip"));
    let (_, import_tip) =
        crypto::import_authority::verify_native_operation(&import_tips[0]).expect("import tip");
    let linear = edges.len() <= 34
        && edges
            .iter()
            .enumerate()
            .all(|(j, e)| if j == 0 { e.is_empty() } else { e == &[j - 1] });
    let mut bundles: Vec<Prefix> = Vec::new();
    for (i, parents) in edges.iter().enumerate() {
        let is_import = (linear && i == 0)
            || parents
                .first()
                .is_some_and(|&p| matches!(bundles[p].proof, PublicProof::Native(_)));
        assert!(
            parents.is_empty()
                || parents
                    .iter()
                    .all(|&p| matches!(bundles[p].proof, PublicProof::Native(_)) == is_import)
        );
        let mut bundle = template.clone();
        bundle.witness_set = imported.witness_set.clone();
        bundle.authority_witnesses.truncate(1);
        let selected_p2 = codec::canonical(&bundle.authority_witnesses[0]).expect("payload");
        bundle.statements.retain(|s| {
            s.body.as_ref().expect("body").purpose == 1
                || s.body.as_ref().expect("body").canonical_payload == selected_p2
        });
        let mut genesis = base_genesis.clone();
        let leaf = edges.len() > 34 && parents.is_empty();
        genesis.nonce = (i as u64).to_be_bytes().to_vec();
        genesis.name = format!("prefix-{i}");
        let signed = SignedGenesis::sign(&genesis, &device).expect("genesis signature");
        let original = wire::SignedRecord {
            format: objects::object::thread_replication::GENESIS_FORMAT.into(),
            canonical_record: signed.canonical,
            signatures: vec![wire::RecordSignature {
                public_key: device.public_key().to_vec(),
                signature: signed.signature,
            }],
        };
        let p1 = &mut bundle.genesis_witnesses[0];
        p1.original_genesis = Some(original.clone());
        let binding = p1.binding.as_mut().expect("binding");
        let body = binding.body.as_mut().expect("body");
        body.genesis_digest = genesis.id().expect("id").as_bytes().to_vec();
        body.original_signatures_digest = signatures(std::slice::from_ref(&original));
        binding
            .creator_signature
            .as_mut()
            .expect("signature")
            .signature = device
            .sign(
                &codec::signing_digest(api::native_witness::GENESIS_DOMAIN, body).expect("digest"),
            )
            .expect("signature");
        let mut operation = base_operation.clone();
        operation.thread = genesis.id().expect("id");
        operation.parents.clear();
        if is_import {
            operation.thread = import_tip.thread;
            operation.parents.insert(import_tip.id().expect("id"));
        }
        let base = if is_import {
            import_tip
                .source_state()
                .expect("source")
                .expect("state")
                .id()
        } else {
            synthetic_initial_base().expect("base").id()
        };
        // Foreign lists are digest ordered. For the depth control, make the
        // newest predecessor sort first so the traversal must reach the full
        // chain before any shorter prefix is cached.
        let mut attempt = 0;
        let signed_original = loop {
            let state = State::new_snapshot(
                Tree::new().hash(),
                vec![base],
                Attribution::human(Principal::new(format!("prefix-{i}-{attempt}"), "")),
            );
            operation.body = ThreadOperationBody::Capture(
                AuthoredCapture::account(
                    state.encode_current_msgpack().expect("state").into(),
                    control.spool,
                    control.actor.clone(),
                    envelope.clone(),
                )
                .expect("author"),
            );
            let signed = SignedOperation::sign(&operation, &device).expect("operation signature");
            let record = wire::SignedRecord {
                format: objects::object::thread_replication::OPERATION_FORMAT.into(),
                canonical_record: signed.canonical,
                signatures: vec![wire::RecordSignature {
                    public_key: device.public_key().to_vec(),
                    signature: signed.signature,
                }],
            };
            let first =
                api::import_authority::signed_native_digest(&record).expect("digest")[0] as usize;
            if !linear || (249 - i * 7..=255 - i * 7).contains(&first) {
                break record;
            }
            attempt += 1;
        };
        let p2 = &mut bundle.authority_witnesses[0];
        p2.original = Some(signed_original);
        p2.authority_envelope = envelope.clone();
        p2.dependencies = if is_import {
            vec![
                imported
                    .genesis_witnesses
                    .iter()
                    .find(|g| {
                        g.binding
                            .as_ref()
                            .expect("binding")
                            .body
                            .as_ref()
                            .expect("body")
                            .genesis_digest
                            == operation.thread.as_bytes()
                    })
                    .expect("main genesis")
                    .original_genesis
                    .clone()
                    .expect("genesis"),
                import_tips[0].clone(),
            ]
        } else {
            vec![original]
        };
        bundle.foreign_dependencies.clear();
        for &parent in parents {
            let prior = &bundles[parent];
            let record = &prior.original;
            let prior_thread =
                if record.format == objects::object::thread_replication::GENESIS_FORMAT {
                    crypto::import_authority::verify_native_genesis(record)
                        .expect("signature")
                        .1
                        .id()
                        .expect("thread")
                } else {
                    crypto::import_authority::verify_native_operation(record)
                        .expect("signature")
                        .1
                        .thread
                };
            let order = prior
                .proof
                .statements()
                .iter()
                .find(|s| {
                    let body = s.body.as_ref().expect("body");
                    if record.format == objects::object::thread_replication::GENESIS_FORMAT {
                        body.purpose == 1
                    } else {
                        body.canonical_payload
                            == codec::canonical(match &prior.proof {
                                PublicProof::Native(b) => &b.authority_witnesses[0],
                                PublicProof::Import(b) => b
                                    .authority_witnesses
                                    .iter()
                                    .find(|p| p.original.as_ref() == Some(record))
                                    .expect("P2"),
                            })
                            .expect("payload")
                    }
                })
                .expect("statement")
                .body
                .as_ref()
                .expect("body")
                .admission_order;
            bundle.foreign_dependencies.push(wire::ForeignDependencyV1 {
                format_version: 1,
                thread_genesis_digest: prior_thread.as_bytes().to_vec(),
                signed_native_digest: api::import_authority::signed_native_digest(record)
                    .expect("digest"),
                origin: if is_import { 2 } else { 1 },
                prefix_admission_order: order,
            });
            p2.dependencies.push(record.clone());
        }
        p2.dependencies
            .sort_by_key(|r| api::import_authority::signed_native_digest(r).expect("digest"));
        bundle
            .foreign_dependencies
            .sort_by_key(|r| r.signed_native_digest.clone());
        let original = p2.original.clone().expect("original");
        for signed in &mut bundle.statements {
            let s = signed.body.as_mut().expect("body");
            if s.purpose == 1 {
                s.canonical_payload = codec::canonical(&bundle.genesis_witnesses[0]).expect("P1");
                s.authority_digest = api::native_witness::signed_genesis_digest(
                    bundle.genesis_witnesses[0]
                        .binding
                        .as_ref()
                        .expect("binding"),
                )
                .expect("digest");
                s.original_signatures_digest = signatures(&[bundle.genesis_witnesses[0]
                    .original_genesis
                    .clone()
                    .expect("genesis")]);
            } else {
                s.admission_order = 1000 + i as u64;
                s.observed_at_unix_millis = 1_200_000;
                let p = &bundle.authority_witnesses[0];
                s.canonical_payload = codec::canonical(p).expect("P2");
                s.authority_digest = codec::hash(&[
                    b"heddle-hosted-authority-envelope-v1",
                    &(p.authority_envelope.len() as u32).to_be_bytes(),
                    &p.authority_envelope,
                ]);
                s.original_signatures_digest = signatures(
                    &p.original
                        .iter()
                        .chain(&p.dependencies)
                        .cloned()
                        .collect::<Vec<_>>(),
                );
            }
            signed.signature = witness
                .sign(&witness_trust::statement_signing_digest(s).expect("digest"))
                .expect("signature");
        }
        bundle.statements.sort_by_key(|s| {
            witness_trust::statement_signing_digest(s.body.as_ref().expect("body")).expect("digest")
        });
        let proof = if is_import {
            let p2 = bundle.authority_witnesses[0].clone();
            let mut s = bundle
                .statements
                .iter()
                .find(|s| s.body.as_ref().expect("body").purpose == 2)
                .expect("P2")
                .clone();
            s.body.as_mut().expect("body").admission_order = 1000 + i as u64;
            s.signature = witness
                .sign(
                    &witness_trust::statement_signing_digest(s.body.as_ref().expect("body"))
                        .expect("digest"),
                )
                .expect("signature");
            imported.authority_witnesses.push(p2);
            imported.statements.push(s);
            imported
                .foreign_dependencies
                .extend(bundle.foreign_dependencies);
            imported
                .foreign_dependencies
                .sort_by_key(|r| r.signed_native_digest.clone());
            imported.foreign_dependencies.dedup_by(|a, b| a == b);
            imported.authority_witnesses.sort_by_key(|p| {
                codec::signing_digest("heddle-import-authority-witness-payload-v1", p)
                    .expect("digest")
            });
            imported.statements.sort_by_key(|s| {
                witness_trust::statement_signing_digest(s.body.as_ref().expect("body"))
                    .expect("digest")
            });
            PublicProof::from(imported.clone())
        } else {
            PublicProof::from(bundle)
        };
        proof
            .validate()
            .unwrap_or_else(|e| panic!("genuine canonical prefix {i} imported={is_import}: {e:?}"));
        let original = if leaf {
            proof.native().expect("native leaf").genesis_witnesses[0]
                .original_genesis
                .clone()
                .expect("genesis endpoint")
        } else {
            original
        };
        let mut originals = if is_import {
            import_tips.to_vec()
        } else {
            vec![]
        };
        originals.push(original.clone());
        bundles.push(Prefix {
            proof,
            original,
            originals,
        });
    }
    bundles
}

pub(crate) fn chain(depth: usize) -> Vec<Prefix> {
    graph(
        &(0..=depth)
            .map(|i| if i == 0 { vec![] } else { vec![i - 1] })
            .collect::<Vec<_>>(),
    )
}
fn signatures(records: &[wire::SignedRecord]) -> Vec<u8> {
    let mut bytes = (records.iter().map(|r| r.signatures.len()).sum::<usize>() as u32)
        .to_be_bytes()
        .to_vec();
    for r in records {
        for signature in &r.signatures {
            bytes.extend(codec::canonical(signature).expect("signature"));
        }
    }
    codec::hash(&[b"heddle-hosted-original-signatures-v1", &bytes])
}

pub(crate) struct Source {
    pub genesis: wire::ThreadGenesisRecord,
    pub operations: Vec<wire::SignedRecord>,
    pub state: State,
    pub pack: Vec<u8>,
    pub index: Vec<u8>,
    pub owner: wire::OwnerState,
    pub owner_genesis: wire::SignedSpoolOwnerGenesis,
}
pub(crate) fn source(prefix: &Prefix) -> Source {
    use objects::store::pack::{ObjectType, PackBuilder, PackObjectId};
    let (_, op) =
        crypto::import_authority::verify_native_operation(&prefix.original).expect("operation");
    let state = op.source_state().expect("source").expect("state");
    let (genesis, history, owner_genesis) = match &prefix.proof {
        PublicProof::Native(b) => {
            let g = &b.genesis_witnesses[0];
            (
                wire::ThreadGenesisRecord {
                    genesis: g.original_genesis.clone(),
                    creator_authority: g.creator_authority_envelope.clone(),
                    native_genesis_authority: g.binding.clone(),
                    ..Default::default()
                },
                &b.owner_histories[0],
                b.owner_genesis.clone().expect("Spool"),
            )
        }
        PublicProof::Import(b) => {
            let g = b
                .genesis_witnesses
                .iter()
                .find(|g| {
                    g.binding
                        .as_ref()
                        .expect("binding")
                        .body
                        .as_ref()
                        .expect("body")
                        .genesis_digest
                        == op.thread.as_bytes()
                })
                .expect("main genesis");
            (
                wire::ThreadGenesisRecord {
                    genesis: g.original_genesis.clone(),
                    creator_authority: g.creator_authority_envelope.clone(),
                    ..Default::default()
                },
                &b.owner_histories[0],
                b.owner_genesis.clone().expect("Spool"),
            )
        }
    };
    let initial =
        heddleco_capability_verifier::verify_owner_root(history.root.as_ref().expect("root"))
            .expect("root");
    let owner = wire::OwnerState {
        owner: Some(wire::PrincipalRef {
            id: uuid::Uuid::from_slice(
                &history
                    .root
                    .as_ref()
                    .expect("root")
                    .root
                    .as_ref()
                    .expect("body")
                    .account_uuid,
            )
            .expect("account")
            .to_string(),
        }),
        root: history.root.clone(),
        accepted_transitions: history.accepted_transitions.clone(),
        version: history.state_hash.clone(),
        resource_keyring: Some(wire::CloneAuthorizationKeyring {
            format_version: 1,
            spool_uuid: owner_genesis
                .genesis
                .as_ref()
                .expect("body")
                .spool_uuid
                .clone(),
            canonical_spool_path_segments: vec!["acme".into(), "imports".into()],
            owner_genesis: Some(owner_genesis.clone()),
            owner_root: history.root.clone(),
            accepted_transitions: history.accepted_transitions.clone(),
            accepted_state_hash: history.state_hash.clone(),
            pin: Some(wire::CloneOwnerPin {
                kind: 2,
                expected_owner_id: initial.owner_id().to_vec(),
                first_seen_unix_seconds: 1000,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let operations = prefix
        .originals
        .iter()
        .filter(|r| {
            crypto::import_authority::verify_native_operation(r)
                .expect("operation")
                .1
                .thread
                == op.thread
        })
        .cloned()
        .collect::<Vec<_>>();
    let tree = Tree::new();
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
    Source {
        genesis,
        operations,
        state,
        pack,
        index,
        owner,
        owner_genesis,
    }
}
