//! Real bidirectional replication, including live external commits and
//! original-author checks, separated from checkout and download stack frames.
use super::*;

pub(super) async fn roundtrip(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    device: &DeviceRpc,
    repository: &repo::Repository,
    replica: &repo::thread_replication::ThreadReplica,
    home: &std::path::Path,
    browser: &Endpoint,
    base: objects::object::StateId,
) {
    let spool: uuid::Uuid = replica
        .genesis()
        .expect("genesis")
        .spool
        .parse()
        .expect("Spool");
    // The server must respond before the browser closes its input stream.
    // Metadata received here changes the Thread graph, never either checkout.
    {
        use crypto::{Signer as _, thread_operation::SignedOperation};
        use objects::object::{
            Attribution, Principal, State, Tree,
            thread_replication::{
                Admission, OPERATION_FORMAT, ThreadFacet, ThreadOperation, ThreadOperationBody,
            },
        };
        let reference = ThreadRef {
            spool: Some(SpoolRef {
                id: spool.to_string(),
            }),
            id: Some(ThreadId {
                value: replica.thread_id().as_bytes().to_vec(),
            }),
        };
        let (mut input, mut output) = remote
            .api
            .exchange::<thread_api::rpc::SyncServiceReplicateThread>(&ReplicateThreadRequest {
                body: Some(replicate_thread_request::Body::Open(ReplicationOpen {
                    thread: Some(reference),
                    facets: vec![
                        SharedFacet::Source as i32,
                        SharedFacet::Collaboration as i32,
                        SharedFacet::Metadata as i32,
                    ],
                    record_formats: vec![OPERATION_FORMAT.into()],
                    session_nonce: uuid::Uuid::now_v7().as_bytes().to_vec(),
                    source: Some(EndpointRef {
                        public_key: browser.id().as_bytes().to_vec(),
                        kind: EndpointKind::Device as i32,
                    }),
                    destination: Some(device.endpoint()),
                    ..Default::default()
                })),
            })
            .await
            .expect("real bidi open");
        let ready = tokio::time::timeout(std::time::Duration::from_secs(5), output.next())
            .await
            .expect("Ready before client FIN")
            .expect("Ready frame")
            .expect("Ready");
        assert!(matches!(
            ready.body,
            Some(replicate_thread_response::Body::Ready(_))
        ));
        let have = tokio::time::timeout(std::time::Duration::from_secs(5), output.next())
            .await
            .expect("Have deadline")
            .expect("Have frame")
            .expect("Have");
        let Some(replicate_thread_response::Body::Have(have)) = have.body else {
            panic!("initial native frontier")
        };
        let original = have
            .frontiers
            .iter()
            .flat_map(|frontier| frontier.heads.iter())
            .next()
            .expect("real nonempty frontier")
            .clone();
        // Announcement is deliberately paged by facet. Drain the actual
        // expected frontier before measuring idle traffic; a queued Metadata
        // page is initial synchronization, not a heartbeat.
        let expected: std::collections::BTreeSet<_> = replica
            .view()
            .expect("current frontier")
            .frontiers
            .into_iter()
            .flat_map(|(facet, heads)| {
                heads.into_iter().map(move |head| {
                    (
                        thread_api::replication::wire_facet(facet),
                        head.as_bytes().to_vec(),
                    )
                })
            })
            .collect();
        let mut announced: std::collections::BTreeSet<_> = have
            .frontiers
            .iter()
            .flat_map(|frontier| {
                frontier
                    .heads
                    .iter()
                    .map(move |head| (frontier.facet, head.clone()))
            })
            .collect();
        while announced != expected {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), output.next())
                .await
                .expect("initial frontier completion deadline")
                .expect("frontier frame")
                .expect("open stream");
            let Some(replicate_thread_response::Body::Have(page)) = frame.body else {
                panic!("initial frontier page")
            };
            for frontier in page.frontiers {
                assert!(
                    !frontier.heads.is_empty(),
                    "announcement never sends empty frontier pages"
                );
                for head in frontier.heads {
                    assert!(
                        expected.contains(&(frontier.facet, head.clone())),
                        "only actual admitted heads may be announced"
                    );
                    assert!(
                        announced.insert((frontier.facet, head)),
                        "unchanged initial frontier is announced once"
                    );
                }
            }
        }
        input
            .send(&ReplicateThreadRequest {
                body: Some(replicate_thread_request::Body::Need(ReplicationNeed {
                    operation_ids: vec![original.clone()],
                })),
            })
            .await
            .expect("request original proof");
        let exported = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let frame = output.next().await.expect("export frame").expect("open");
                if let Some(replicate_thread_response::Body::Operations(records)) = frame.body {
                    break records;
                }
            }
        })
        .await
        .expect("export deadline");
        let record = exported
            .operations
            .into_iter()
            .next()
            .expect("signed record");
        let verified = thread_api::replication::decode_record(record)
            .expect("original proof")
            .verify()
            .expect("signature");
        assert_eq!(verified.id().expect("ID").as_bytes().as_slice(), original);
        let idle =
            tokio::time::timeout(std::time::Duration::from_millis(1100), output.next()).await;
        assert!(
            idle.is_err(),
            "idle replication emits no empty Have heartbeat: {idle:?}"
        );
        let publisher = Ed25519Signer::from_seed(&[71; 32]).expect("browser publisher");
        let source_now = chrono::Utc::now().timestamp();
        let source_authority = repo::device_authority::load(home, source_now)
            .expect("independently enrolled original owner");
        let source_token = mint_agent_root(&[71; 32]).expect("original source credential");
        let source_mint = biscuit_verifier::PublicKey::from_bytes(
            publisher.public_key(),
            biscuit_auth::Algorithm::Ed25519,
        )
        .expect("original mint");
        let source_token = biscuit_verifier::parse_token(&source_token.token, &[source_mint])
            .expect("verified original source Biscuit");
        let source_proof = repo::thread_replication::metadata::prepare_control_authority(
            &source_authority,
            &publisher.public_key().try_into().expect("mint key"),
            &source_token,
            source_now,
        )
        .expect("sealed source author");
        let make_operation = |intent: &str| {
            let mut state = State::new_snapshot(
                Tree::new().hash(),
                vec![base],
                Attribution::human(Principal::new(intent, "")),
            );
            state.intent = Some(intent.into());
            let operation = ThreadOperation {
                version: 1,
                thread: replica.thread_id(),
                parents: Default::default(),
                publisher: publisher.public_key().try_into().expect("key"),
                body: ThreadOperationBody::Capture(
                    objects::object::thread_replication::AuthoredCapture::account(
                        state.encode_current_msgpack().expect("State").into(),
                        spool,
                        objects::object::CollaborationActor {
                            principal_id: uuid::Uuid::from_bytes([9; 16]),
                            agent_id: None,
                        },
                        source_proof.clone(),
                    )
                    .expect("signed original account author"),
                ),
            };
            SignedOperation::sign(&operation, &publisher).expect("signed source")
        };
        let incoming = make_operation("browser causal branch");
        replica
            .verify_source_authority(
                &incoming.verify().expect("source original"),
                &source_authority,
                &repo::device_catalog::load(home, spool)
                    .expect("registered source Spool")
                    .capability_path,
                source_now,
            )
            .expect("independently verified original source author before delivery");

        let incoming_id = incoming.verify().expect("proof").id().expect("ID");
        assert!(replica.operation(&incoming_id).expect("lookup").is_none());
        input
            .send(&ReplicateThreadRequest {
                body: Some(replicate_thread_request::Body::Operations(
                    ReplicationOperations {
                        boundary_acceptances: Vec::new(),
                        authority_admissions: Vec::new(),
                        operations: vec![SignedRecord {
                            format: OPERATION_FORMAT.into(),
                            canonical_record: incoming.canonical,
                            signatures: vec![RecordSignature {
                                public_key: publisher.public_key().to_vec(),
                                signature: incoming.signature,
                            }],
                        }],
                    },
                )),
            })
            .await
            .expect("send new source branch");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let frame = output.next().await.expect("receipt frame").expect("open");
                if let Some(replicate_thread_response::Body::Receipt(receipt)) = frame.body
                    && receipt
                        .accepted_operation_ids
                        .contains(&incoming_id.as_bytes().to_vec())
                {
                    break;
                }
            }
        })
        .await
        .expect("new record durably acknowledged");
        assert_eq!(
            replica
                .operation(&incoming_id)
                .expect("lookup")
                .expect("durable operation")
                .1,
            Admission::Accepted
        );
        // Drain activity caused by the browser write before committing a new
        // external write; only the post-commit feed can rescue that idle state.
        loop {
            match tokio::time::timeout(std::time::Duration::from_millis(250), output.next()).await {
                Err(_) => break,
                Ok(Ok(Some(_))) => {}
                other => panic!("stream ended while draining: {other:?}"),
            }
        }
        let local_update = make_operation("independent process branch");
        let local_id = local_update.verify().expect("proof").id().expect("ID");
        replica
            .receive(&local_update, repository.store(), |_| Ok(()))
            .expect("independent committed write");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let frame = output.next().await.expect("push frame").expect("open");
                if let Some(replicate_thread_response::Body::Have(have)) = frame.body
                    && have
                        .frontiers
                        .iter()
                        .any(|frontier| frontier.heads.contains(&local_id.as_bytes().to_vec()))
                {
                    break;
                }
            }
        })
        .await
        .expect("post-commit marker pushes external update");
        assert!(
            replica
                .frontier_page(ThreadFacet::Source, None, 64)
                .expect("frontier")
                .contains(&incoming_id)
        );
        // Metadata uses the same live RPC, but original user authority is
        // independently admitted rather than inferred from the current sender.
        use objects::object::{
            CollaborationActor, ContentHash,
            thread_replication::metadata::{AUTHORITY_FORMAT, Control, ThreadControl},
        };
        let now = chrono::Utc::now().timestamp();
        let authority = repo::device_authority::load(home, now).expect("enrolled account");
        let original_token = mint_agent_root(&[71; 32]).expect("original owner credential");
        let mint_key = biscuit_verifier::PublicKey::from_bytes(
            publisher.public_key(),
            biscuit_auth::Algorithm::Ed25519,
        )
        .expect("mint key");
        let parsed = biscuit_verifier::parse_token(&original_token.token, &[mint_key])
            .expect("original Biscuit");
        let proof = repo::thread_replication::metadata::prepare_control_authority(
            &authority,
            &publisher.public_key().try_into().expect("mint key"),
            &parsed,
            now,
        )
        .expect("sealed public authority");
        let control = ThreadControl {
            version: 1,
            spool,
            actor: CollaborationActor {
                principal_id: uuid::Uuid::from_bytes([9; 16]),
                agent_id: None,
            },
            authority_digest: ContentHash::compute_typed(AUTHORITY_FORMAT, &proof),
            authority_envelope: proof,
            client_operation_id: uuid::Uuid::now_v7(),
            occurred_at_ms: now * 1000,
            control: Control::Name("browser signed name".into()),
        };
        let native = ThreadOperation {
            version: 1,
            thread: replica.thread_id(),
            parents: Default::default(),
            publisher: publisher.public_key().try_into().expect("key"),
            body: ThreadOperationBody::Metadata(control.encode().expect("metadata")),
        };
        let signed =
            SignedOperation::sign(&native, &publisher).expect("original metadata signature");
        let metadata_id = native.id().expect("metadata ID");
        input
            .send(&ReplicateThreadRequest {
                body: Some(replicate_thread_request::Body::Operations(
                    ReplicationOperations {
                        boundary_acceptances: Vec::new(),
                        authority_admissions: Vec::new(),
                        operations: vec![SignedRecord {
                            format: OPERATION_FORMAT.into(),
                            canonical_record: signed.canonical.clone(),
                            signatures: vec![RecordSignature {
                                public_key: publisher.public_key().to_vec(),
                                signature: signed.signature.clone(),
                            }],
                        }],
                    },
                )),
            })
            .await
            .expect("send original metadata");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let frame = output
                    .next()
                    .await
                    .expect("metadata receipt frame")
                    .expect("live stream");
                if let Some(replicate_thread_response::Body::Receipt(receipt)) = frame.body {
                    if receipt
                        .accepted_operation_ids
                        .contains(&metadata_id.as_bytes().to_vec())
                    {
                        break;
                    }
                    assert!(
                        receipt.rejected.is_empty(),
                        "valid original-author metadata must be accepted"
                    );
                }
            }
        })
        .await
        .expect("metadata admission deadline");
        assert!(
            replica
                .original_authority_admitted(&signed)
                .expect("durable original-author receipt")
        );
        input
            .send(&ReplicateThreadRequest {
                body: Some(replicate_thread_request::Body::Need(ReplicationNeed {
                    operation_ids: vec![metadata_id.as_bytes().to_vec()],
                })),
            })
            .await
            .expect("request original metadata");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let frame = output
                    .next()
                    .await
                    .expect("metadata export frame")
                    .expect("live stream");
                if let Some(replicate_thread_response::Body::Operations(operations)) = frame.body {
                    for record in operations.operations {
                        let exported = thread_api::replication::decode_record(record)
                            .expect("original export signature");
                        if exported.verify().expect("operation").id().expect("ID") == metadata_id {
                            assert_eq!(exported, signed);
                            return;
                        }
                    }
                }
            }
        })
        .await
        .expect("metadata facet exports original signed operation");

        // Current owner delivery cannot manufacture another publisher's original
        // authority. This proof is valid for its root key, not the new signer.
        let unrelated = Ed25519Signer::from_seed(&[74; 32]).expect("unrelated publisher");
        let mut unowned = native;
        unowned.publisher = unrelated.public_key().try_into().expect("key");
        let unowned = SignedOperation::sign(&unowned, &unrelated)
            .expect("cryptographically valid distinct publisher");
        let unowned_id = unowned.verify().expect("signature").id().expect("ID");
        input
            .send(&ReplicateThreadRequest {
                body: Some(replicate_thread_request::Body::Operations(
                    ReplicationOperations {
                        boundary_acceptances: Vec::new(),
                        authority_admissions: Vec::new(),
                        operations: vec![SignedRecord {
                            format: OPERATION_FORMAT.into(),
                            canonical_record: unowned.canonical,
                            signatures: vec![RecordSignature {
                                public_key: unrelated.public_key().to_vec(),
                                signature: unowned.signature,
                            }],
                        }],
                    },
                )),
            })
            .await
            .expect("submit distinct original publisher");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Ok(Some(frame)) = output.next().await {
                if let Some(replicate_thread_response::Body::Receipt(receipt)) = frame.body {
                    assert!(
                        !receipt
                            .accepted_operation_ids
                            .contains(&unowned_id.as_bytes().to_vec()),
                        "delivery authority cannot replace original author authority"
                    );
                }
            }
        })
        .await
        .expect("original author denial terminates stream");
        assert!(
            replica
                .operation(&unowned_id)
                .expect("unowned lookup")
                .is_none(),
            "denied original authority never persists"
        );
        drop(input);
        drop(output);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if device
                    .feeds
                    .lock()
                    .expect("feeds")
                    .get(&spool)
                    .is_none_or(|feed| feed.strong_count() == 0)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("replication cancellation drops OS watcher");
    }
}
