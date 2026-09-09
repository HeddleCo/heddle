//! Exercise the real native endpoint without any hosted service.
use std::{net::Ipv4Addr, sync::Arc};

use api::heddle::api::v2alpha1::*;
use crypto::Ed25519Signer;
use iroh::{Endpoint, RelayMode, endpoint::presets, protocol::Router};

use super::*;
use crate::hosted_runtime::{
    claim_authorization::StoredClaimAuthorization,
    hosted::claim_protocol::{ClaimProtocol, NATIVE_ALPN},
    root_mint::mint_agent_root,
};

#[tokio::test]
async fn real_device_rpc_captures_without_weft_and_rejects_unowned_authority() {
    let _guard = config::credentials::lock_test_env();
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                match &self.0 {
                    Some(value) => std::env::set_var("HEDDLE_HOME", value),
                    None => std::env::remove_var("HEDDLE_HOME"),
                }
            }
        }
    }
    let home = tempfile::tempdir().expect("home");
    let _restore = Restore(std::env::var_os("HEDDLE_HOME"));
    unsafe {
        std::env::set_var("HEDDLE_HOME", home.path());
    }

    let local = tempfile::tempdir().expect("repository");
    let repository = repo::Repository::init_default(local.path()).expect("repo");
    let root = Ed25519Signer::from_seed(&[71; 32]).expect("root");
    let recovery = Ed25519Signer::from_seed(&[72; 32]).expect("recovery");
    let signed =
        repo::sign_custodial_owner_root(&root, &recovery, [9; 16], [5; 32]).expect("owner root");
    let binding = repo::sign_custodial_owner_binding(&root, &signed, [6; 32]).expect("binding");
    let verified = heddleco_capability_verifier::verify_owner_root(&signed).expect("root proof");
    let owner = OwnerState {
        owner: Some(PrincipalRef {
            id: uuid::Uuid::from_bytes([9; 16]).to_string(),
        }),
        root: Some(signed),
        binding: Some(binding),
        version: verified.state_hash().to_vec(),
        ..Default::default()
    };
    repo::device_authority::publish(
        home.path(),
        &repo::device_authority::DeviceAuthority {
            owner: owner.clone(),
            mint_roots: vec![],
            revoked_ids: vec![],
            revoked_mint_roots: vec![],
            revoked_publishers: vec![],
        },
        chrono::Utc::now().timestamp(),
    )
    .expect("independent local enrollment");
    let base = repository.head().expect("head").expect("base");
    let replica = repository
        .create_native_thread("device-test", base, None, "device operations")
        .expect("Thread");
    let spool = uuid::Uuid::parse_str(&replica.genesis().expect("genesis").spool).expect("spool");
    repo::device_catalog::register(home.path(), &repository, spool).expect("catalog");
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("address")
        .bind()
        .await
        .expect("endpoint");
    let address = endpoint.addr();
    let key = *endpoint.id().as_bytes();
    let device = Arc::new(DeviceRpc::new(home.path().to_owned(), key));
    let (authorization, _, _) = StoredClaimAuthorization::new();
    let authorization = Arc::new(authorization);
    let protocol =
        ClaimProtocol::new(authorization.clone(), authorization, key).with_device(device.clone());
    let budgets = protocol.budgets();
    let router = Router::builder(endpoint)
        .accept(NATIVE_ALPN, protocol)
        .spawn();
    let browser = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("browser address")
        .bind()
        .await
        .expect("browser");
    let connection = browser
        .connect(address.clone(), NATIVE_ALPN)
        .await
        .expect("direct Iroh");
    let token = mint_agent_root(&[71; 32]).expect("self issued root").token;
    let credentials = thread_api::credentials::Credentials::Signed {
        signer: Arc::new(root),
        biscuit: token.into_bytes(),
        grant_envelope: Vec::new(),
    };
    let transport = thread_api::transport::IrohTransport::new(
        connection,
        credentials,
        256 * 1024,
        std::time::Duration::from_secs(10),
    )
    .expect("transport");
    let remote = thread_api::Remote::discover(transport, key, EndpointKind::Device)
        .await
        .expect("discover");
    let spool_ref = SpoolRef {
        id: spool.to_string(),
    };
    super::thread_tests::roundtrip(&remote, &device, &repository, spool).await;
    super::account_tests::roundtrip(&remote, &device, spool).await;
    super::capacity_tests::roundtrip(&remote, &device, &repository, &replica, &budgets).await;
    super::receipt_tests::roundtrip(
        &remote,
        &device,
        &repository,
        &replica,
        *browser.id().as_bytes(),
    )
    .await;
    let source = RevisionRef {
        spool: Some(spool_ref.clone()),
        revision: Some(revision_ref::Revision::State(
            api::heddle::api::v1alpha1::StateId {
                value: base.as_bytes().to_vec(),
            },
        )),
    };
    let materialize = MaterializeCheckoutRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        thread: Some(ThreadRef {
            spool: Some(spool_ref.clone()),
            id: Some(ThreadId {
                value: replica.thread_id().as_bytes().to_vec(),
            }),
        }),
        revision: Some(source.clone()),
        ..Default::default()
    };
    let result = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceMaterialize>(&materialize)
        .await
        .expect("materialize offline");
    let overview = result.checkout.expect("checkout overview");
    let checkout = overview.r#ref.clone().expect("checkout ref");
    let claimed = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceClaimCheckoutWriter>(&ClaimCheckoutWriterRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            checkout: Some(checkout.clone()),
            expected_checkout_version: overview.version.clone(),
            ..Default::default()
        })
        .await
        .expect("writer");
    let lease = claimed.lease.expect("lease");
    let conflicting = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceClaimCheckoutWriter>(&ClaimCheckoutWriterRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            checkout: Some(checkout.clone()),
            expected_checkout_version: overview.version.clone(),
            ..Default::default()
        })
        .await;
    assert!(
        conflicting.is_err(),
        "a second writer cannot claim the same physical checkout"
    );
    let mut observed = remote
        .observe::<thread_api::rpc::CheckoutServiceObserveCheckouts>(
            ObserveCheckoutsRequest {
                spool: Some(spool_ref.clone()),
                observe: Some(ObserveOptions {
                    mode: ObservationMode::Follow as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("live checkout view");
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), observed.next_commit())
        .await
        .expect("snapshot deadline")
        .expect("snapshot protocol")
        .expect("snapshot");
    assert!(snapshot.replace);
    assert_eq!(snapshot.changes.len(), 1);
    let idle_feed = device
        .feeds
        .lock()
        .expect("feeds")
        .get(&spool)
        .expect("active feed")
        .upgrade()
        .expect("feed retained");
    let idle_generation = *idle_feed.changes.borrow();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(1100),
            observed.next_commit()
        )
        .await
        .is_err(),
        "idle view emits no polling frames"
    );
    assert_eq!(
        *idle_feed.changes.borrow(),
        idle_generation,
        "idle SQL reads must not manufacture OS change events"
    );
    drop(idle_feed);
    std::fs::write(
        std::path::Path::new(&overview.display_path).join("hello.txt"),
        "native capture",
    )
    .expect("working edit");
    let request = CaptureCheckoutRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        checkout: Some(checkout),
        expected_checkout_version: overview.version,
        expected_source: Some(source),
        writer_lease_token: lease.token,
        summary: "device capture".into(),
        ..Default::default()
    };
    let captured = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceCapture>(&request)
        .await
        .expect("native capture");
    let changed = tokio::time::timeout(std::time::Duration::from_secs(5), observed.next_commit())
        .await
        .expect("filesystem/source update wakes live view");
    assert!(
        changed.is_ok() || matches!(changed, Err(thread_api::observation::Error::Reset(_))),
        "update either commits or explicitly resets a racing snapshot"
    );
    drop(observed);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let released = device
                .feeds
                .lock()
                .expect("feeds")
                .get(&spool)
                .is_none_or(|feed| feed.strong_count() == 0);
            if released {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancellation drops the observer and OS watcher");
    let retry = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceCapture>(&request)
        .await
        .expect("fresh proof exact retry");
    assert_eq!(
        captured, retry,
        "retries return the original durable receipt"
    );
    assert_eq!(replica.view().expect("source").source_heads.len(), 1);
    let target_replica = repository
        .create_native_thread("device-target", base, None, "local landing")
        .expect("target Thread");
    let captured_overview = captured.checkout.as_ref().expect("captured checkout");
    let policy = captured_overview
        .actions
        .iter()
        .find(|action| action.method.ends_with("/LandCheckout"))
        .expect("landing action")
        .observed_versions[0]
        .version
        .clone();
    let landing = LandCheckoutRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        checkout: captured_overview.r#ref.clone(),
        expected_checkout_version: captured_overview.version.clone(),
        source: captured_overview.materialized.clone(),
        target: Some(ThreadRef {
            spool: Some(spool_ref.clone()),
            id: Some(ThreadId {
                value: target_replica.thread_id().as_bytes().to_vec(),
            }),
        }),
        expected_target: materialize.revision.clone(),
        expected_policy_version: policy,
        writer_lease_token: request.writer_lease_token.clone(),
    };
    let landed = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceLandCheckout>(&landing)
        .await
        .expect("actual local merge landing");
    assert_eq!(
        target_replica
            .view()
            .expect("target view")
            .source_heads
            .len(),
        1
    );
    assert_eq!(
        landed
            .checkout
            .as_ref()
            .expect("source checkout")
            .materialized,
        captured_overview.materialized,
        "landing advances target Thread without rewriting source checkout"
    );
    assert_eq!(
        landed,
        remote
            .api
            .call::<thread_api::rpc::CheckoutServiceLandCheckout>(&landing)
            .await
            .expect("landing retry")
    );
    let second = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceMaterialize>(&MaterializeCheckoutRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            ..materialize.clone()
        })
        .await
        .expect("second independent checkout")
        .checkout
        .expect("overview");
    let second_writer = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceClaimCheckoutWriter>(&ClaimCheckoutWriterRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            checkout: second.r#ref.clone(),
            expected_checkout_version: second.version.clone(),
            ..Default::default()
        })
        .await
        .expect("separate physical checkout has independent writer")
        .lease
        .expect("writer");
    let refreshed = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceRefresh>(&RefreshCheckoutRequest {
            client_operation_id: uuid::Uuid::new_v4().to_string(),
            checkout: second.r#ref.clone(),
            expected_checkout_version: second.version,
            expected_source: second.materialized,
            target: captured_overview.materialized.clone(),
            writer_lease_token: second_writer.token.clone(),
        })
        .await
        .expect("refresh admitted source");
    let refreshed = refreshed.checkout.expect("refreshed checkout");
    assert_eq!(
        std::fs::read_to_string(std::path::Path::new(&refreshed.display_path).join("hello.txt"))
            .expect("refreshed content"),
        "native capture"
    );
    let recover = RecoverCheckoutRequest {
        client_operation_id: uuid::Uuid::new_v4().to_string(),
        checkout: refreshed.r#ref.clone(),
        expected_checkout_version: refreshed.version.clone(),
        recover_to: materialize.revision.clone(),
        writer_lease_token: second_writer.token.clone(),
    };
    let recovered = remote
        .api
        .call::<thread_api::rpc::CheckoutServiceRecover>(&recover)
        .await
        .expect("recover prior source into working files");
    assert_eq!(
        recovered
            .checkout
            .as_ref()
            .expect("recovered checkout")
            .materialized,
        refreshed.materialized,
        "recovery preserves recorded source"
    );
    assert!(
        !std::path::Path::new(&refreshed.display_path)
            .join("hello.txt")
            .exists()
    );
    assert_eq!(
        recovered,
        remote
            .api
            .call::<thread_api::rpc::CheckoutServiceRecover>(&recover)
            .await
            .expect("recovery retry")
    );
    remote
        .api
        .call::<thread_api::rpc::CheckoutServiceReleaseCheckoutWriter>(
            &ReleaseCheckoutWriterRequest {
                client_operation_id: uuid::Uuid::new_v4().to_string(),
                checkout: refreshed.r#ref,
                token: second_writer.token,
            },
        )
        .await
        .expect("release writer");
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
        let reference = materialize.thread.clone().expect("Thread reference");
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
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1100), output.next())
                .await
                .is_err(),
            "idle replication emits no empty Have heartbeat"
        );
        let publisher = Ed25519Signer::from_seed(&[71; 32]).expect("browser publisher");
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
                body: ThreadOperationBody::Capture(state.encode_current_msgpack().expect("State")),
            };
            SignedOperation::sign(&operation, &publisher).expect("signed source")
        };
        let incoming = make_operation("browser causal branch");
        let incoming_id = incoming.verify().expect("proof").id().expect("ID");
        assert!(replica.operation(&incoming_id).expect("lookup").is_none());
        input
            .send(&ReplicateThreadRequest {
                body: Some(replicate_thread_request::Body::Operations(
                    ReplicationOperations {
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
                if let Some(replicate_thread_response::Body::Receipt(receipt)) = frame.body {
                    if receipt
                        .accepted_operation_ids
                        .contains(&incoming_id.as_bytes().to_vec())
                    {
                        break;
                    }
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
                if let Some(replicate_thread_response::Body::Have(have)) = frame.body {
                    if have
                        .frontiers
                        .iter()
                        .any(|frontier| frontier.heads.contains(&local_id.as_bytes().to_vec()))
                    {
                        break;
                    }
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
        let authority = repo::device_authority::load(home.path(), now).expect("enrolled account");
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
                .control_authority_admitted(&signed)
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
            loop {
                match output.next().await {
                    Ok(Some(frame)) => {
                        if let Some(replicate_thread_response::Body::Receipt(receipt)) = frame.body
                        {
                            assert!(
                                !receipt
                                    .accepted_operation_ids
                                    .contains(&unowned_id.as_bytes().to_vec()),
                                "delivery authority cannot replace original author authority"
                            );
                        }
                    }
                    Ok(None) | Err(_) => break,
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
    let wrong = mint_agent_root(&[73; 32]).expect("unowned root");
    let transport = thread_api::transport::IrohTransport::new(
        browser
            .connect(address, NATIVE_ALPN)
            .await
            .expect("connection"),
        thread_api::credentials::Credentials::Signed {
            signer: Arc::new(Ed25519Signer::from_seed(&[73; 32]).expect("other key")),
            biscuit: wrong.token.into_bytes(),
            grant_envelope: Vec::new(),
        },
        256 * 1024,
        std::time::Duration::from_secs(5),
    )
    .expect("transport");
    let other = thread_api::Remote::discover(transport, key, EndpointKind::Device)
        .await
        .expect("public description");
    assert!(
        other
            .api
            .call::<thread_api::rpc::CheckoutServiceMaterialize>(&MaterializeCheckoutRequest {
                client_operation_id: uuid::Uuid::new_v4().to_string(),
                ..materialize
            })
            .await
            .is_err(),
        "self-minted unrelated authority cannot control this device"
    );
    drop(other);
    drop(remote);
    browser.close().await;
    router.shutdown().await.expect("router shutdown");
}

pub(super) async fn denied_spool_stream_is_typed(
    remote: &thread_api::Remote<
        thread_api::transport::IrohTransport<thread_api::credentials::Credentials>,
    >,
    spool: uuid::Uuid,
) {
    let mut denied = remote
        .observe::<thread_api::rpc::CheckoutServiceObserveCheckouts>(
            ObserveCheckoutsRequest {
                spool: Some(SpoolRef {
                    id: spool.to_string(),
                }),
                observe: Some(ObserveOptions {
                    mode: ObservationMode::Once as i32,
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("denied stream opens its framed response");
    let error = denied
        .next_commit()
        .await
        .err()
        .expect("unauthorized stream is denied");
    match error {
        thread_api::observation::Error::Client(api::v2::client::ClientError::Transport(
            thread_api::transport::Error::Remote(failure),
        )) => assert_eq!(
            failure.code,
            CallFailureCode::Unauthenticated as i32,
            "denied exact-Spool stream preserves typed authorization failure"
        ),
        other => {
            panic!("denied exact-Spool stream must preserve typed authorization failure: {other}")
        }
    }
}
