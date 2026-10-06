// SPDX-License-Identifier: Apache-2.0
//! Minimal v2 Weft fixture for hosted-client publication and fetch tests.

use std::{
    net::Ipv4Addr,
    sync::{Arc, Mutex},
};

use api::{
    StreamingShape,
    framing::{
        StreamFrame, decode_request_frame, decode_request_prelude, decode_stream_frame,
        encode_stream_message, encode_success_response,
    },
    heddle::api::v1alpha2 as v2,
    method_descriptor,
};
use base64::Engine as _;
use crypto::Ed25519Signer;
use iroh::{Endpoint, RelayMode, endpoint::presets};
use prost::Message;
use tokio::task::JoinHandle;

use super::{CallContextFactory, HostedClient};
#[path = "../../../../thread-api/tests/support/native_witness.rs"]
pub(crate) mod witnessing;

#[derive(Clone, Debug, Default)]
pub(crate) struct PublicationCapture {
    pub start_failures: usize,
    pub revision: Option<v2::RevisionRef>,
    pub thread_genesis: Option<v2::ThreadGenesisRecord>,
    pub operations: Vec<v2::ReplicationOperations>,
    pub pack_data: Vec<u8>,
    pub index_data: Vec<u8>,
    pub native_authority: Option<v2::NativePublicProofBundleV1>,
    pub import_authority: Option<v2::ImportPublicProofBundleV1>,
    pub owner_genesis: Option<v2::SignedSpoolOwnerGenesis>,
    pub ownership: Option<v2::OwnerState>,
    pub prefixes: Vec<PublicationCapture>,
}

#[derive(Clone)]
struct Fixture {
    spool: uuid::Uuid,
    thread_name: String,
    thread_id: Vec<u8>,
    owner_genesis: v2::SignedSpoolOwnerGenesis,
    owner: v2::OwnerState,
    witness_set: api::heddle::api::common::SignedHostedWitnessSetV1,
    captured: Arc<Mutex<PublicationCapture>>,
    scope_path: Option<String>,
}

pub(crate) async fn start(
    spool: uuid::Uuid,
    thread_name: impl Into<String>,
    thread_id: [u8; 32],
) -> (HostedClient, JoinHandle<()>, Arc<Mutex<PublicationCapture>>) {
    start_with_scope(spool, thread_name, thread_id, None).await
}

/// Scope admission uses the same shared verifier and canonical resource fact as Weft.
/// Each fixture represents one resolved spool; account ownership has no resource.
pub(crate) async fn start_with_scope(
    spool: uuid::Uuid,
    thread_name: impl Into<String>,
    thread_id: [u8; 32],
    scoped: Option<(String, CallContextFactory)>,
) -> (HostedClient, JoinHandle<()>, Arc<Mutex<PublicationCapture>>) {
    let (scope_path, client_context) = match scoped {
        Some((path, context)) => (Some(path), Some(context)),
        None => (None, None),
    };
    let captured = Arc::new(Mutex::new(PublicationCapture::default()));
    let (owner_genesis, owner) = witnessing::ownership(spool);
    let witness_set = witnessing::witness_set();
    let fixture = Fixture {
        spool,
        thread_name: thread_name.into(),
        thread_id: thread_id.to_vec(),
        owner_genesis,
        owner,
        witness_set: witness_set.clone(),
        captured: Arc::clone(&captured),
        scope_path,
    };

    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("hosted test bind address")
        .bind()
        .await
        .expect("hosted test endpoint");
    let server_addr = server.addr();
    let server_key = server.id().as_bytes().to_vec();
    let server_task = tokio::spawn(async move {
        let connection = server
            .accept()
            .await
            .expect("hosted test connection")
            .await
            .expect("hosted test handshake");
        while let Ok((send, recv)) = connection.accept_bi().await {
            tokio::spawn(serve_call(send, recv, fixture.clone(), server_key.clone()));
        }
        server.close().await;
    });
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("hosted client bind address")
        .bind()
        .await
        .expect("hosted client endpoint");
    let client_signer = Ed25519Signer::from_seed(&[71; 32]).expect("hosted test client signer");
    let token = crate::hosted_runtime::root_mint::mint_agent_root(&[71; 32])
        .expect("current owner credential");
    let context = client_context.unwrap_or(
        CallContextFactory::default()
            .with_bearer_capability(token.token.into_bytes())
            .with_signing_key_pem(
                &client_signer.to_pem().expect("hosted test client key"),
                "principal:test",
            )
            .expect("hosted test call context"),
    );
    let mut client = HostedClient::connect_addr_with_context(endpoint, server_addr, context)
        .await
        .expect("connect hosted test client");
    let root = Ed25519Signer::from_seed(&[7; 32]).expect("deployment root");
    client.hosted_root = Some(super::descriptor_trust::HostedRootSelection {
        authority: "https://weft.example.test".into(),
        root_id: "descriptor-root-1".into(),
        public_key: crypto::Signer::public_key(&root)
            .try_into()
            .expect("root key"),
        automatic_store: None,
    });
    let mut lookup = super::descriptor_trust::HostedWitnessLookup::new(
        "https://weft.example.test",
        &config::UserConfig::default()
            .hosted_runtime_config(None)
            .expect("configuration"),
    )
    .expect("witness lookup");
    lookup.test_responses = Some(super::descriptor_trust::TestWitnessResponses {
        set: Some(witness_set),
        proofs: vec![],
    });
    client.witness_lookup = Some(Arc::new(lookup));
    (client, server_task, captured)
}

async fn serve_call(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    fixture: Fixture,
    server_key: Vec<u8>,
) {
    let mut request = Vec::new();
    let (method, context, prelude_len) = loop {
        let chunk = recv
            .read_chunk(api::framing::MAX_CONTROL_BODY + 6)
            .await
            .expect("read request prelude")
            .expect("request prelude");
        request.extend_from_slice(&chunk);
        if let Some((prelude, consumed)) =
            decode_request_prelude(&request).expect("decode request prelude")
        {
            break (prelude.method.to_string(), prelude.context, consumed);
        }
    };
    let streaming = method_descriptor(&method)
        .map(|descriptor| descriptor.streaming)
        .or_else(|| api::v2::method_descriptor(&method).map(|descriptor| descriptor.streaming))
        .expect("registered hosted method");
    if let Some(path) = fixture.scope_path.as_deref() {
        let root = biscuit_auth::KeyPair::from(
            &biscuit_auth::PrivateKey::from_bytes(&[71; 32], biscuit_auth::Algorithm::Ed25519)
                .expect("fixture root"),
        );
        let operation = method.rsplit('/').next().expect("method name");
        let resource = match operation {
            "DescribeEndpoint" | "GetIdentity" | "ObserveIdentity" => None,
            "ObserveOwnership" => {
                read_request_body(&mut recv, &mut request).await;
                let observed = v2::ObserveOwnershipRequest::decode(
                    decode_request_frame(&request).expect("owner frame").body,
                )
                .expect("owner request");
                observed.spool.map(|spool| {
                    assert_eq!(spool.id, fixture.spool.to_string());
                    ("spool", path)
                })
            }
            _ => Some(("spool", path)),
        };
        let token = base64::engine::general_purpose::URL_SAFE.encode(&context.bearer_capability);
        if let Err(error) = biscuit_verifier::verify_at_with_resource(
            &token,
            &[root.public()],
            &[],
            operation,
            resource,
            chrono::Utc::now(),
        ) {
            println!("scope admission refused {operation} on {resource:?}: {error}");
            let failure = api::heddle::api::common::CallFailure {
                code: api::heddle::api::common::CallFailureCode::Unauthenticated as i32,
                message: "invalid or revoked bearer capability".into(),
                ..Default::default()
            };
            let bytes = if streaming == StreamingShape::Unary {
                api::framing::encode_failure_response(&failure)
            } else {
                api::framing::encode_stream_failure(&failure)
            }
            .expect("scope failure");
            send.write_all(&bytes).await.expect("scope rejection");
            send.finish().expect("scope response");
            return;
        }
    }
    match streaming {
        StreamingShape::Unary | StreamingShape::ClientStreaming => match method.as_str() {
            "/heddle.api.v1alpha2.EndpointService/DescribeEndpoint" => {
                let response = v2::DescribeEndpointResponse {
                    endpoint: Some(v2::EndpointRef {
                        kind: v2::EndpointKind::Weft as i32,
                        public_key: server_key,
                    }),
                    supported_packages: vec!["heddle.api.v1alpha2".into()],
                    implemented_methods: vec![
                        "/heddle.api.v1alpha2.WorkspaceService/ResolveResources".into(),
                        "/heddle.api.v1alpha2.ThreadService/ObserveThreads".into(),
                        "/heddle.api.v1alpha2.IdentityService/GetIdentity".into(),
                        "/heddle.api.v1alpha2.OwnerAuthorizationService/ObserveOwnership".into(),
                        "/heddle.api.v1alpha2.IdentityService/ObserveIdentity".into(),
                        "/heddle.api.v1alpha2.SyncService/PublishContent".into(),
                        "/heddle.api.v1alpha2.SyncService/Fetch".into(),
                        "/heddle.api.v1alpha2.SpoolService/ObserveSpool".into(),
                        "/heddle.api.v1alpha2.ThreadService/ObserveThread".into(),
                        "/heddle.api.v1alpha2.ThreadService/StartThread".into(),
                    ],
                    default_read_budget: Some(v2::ReadBudget {
                        max_items: 64,
                        max_frame_bytes: 64 * 1024,
                        max_snapshot_bytes: 1024 * 1024,
                    }),
                    max_pending_batch_bytes: 1024 * 1024,
                    understood_signed_record_formats: vec![
                        objects::object::thread_replication::GENESIS_FORMAT.into(),
                        objects::object::thread_replication::OPERATION_FORMAT.into(),
                        objects::object::thread_replication::ownership_claim::FORMAT.into(),
                    ],
                    protocol: Some(thread_api::hybrid::protocol()),
                    ..Default::default()
                };
                write_unary(&mut send, &response).await;
            }
            "/heddle.api.v1alpha2.WorkspaceService/ResolveResources" => {
                read_request_body(&mut recv, &mut request).await;
                let body = decode_request_frame(&request)
                    .expect("decode ResolveResources frame")
                    .body;
                let request = v2::ResolveResourcesRequest::decode(body)
                    .expect("decode ResolveResources request");
                let thread_name = (request.selectors.len() == 1)
                    .then(|| request.selectors[0].selector.as_ref())
                    .flatten()
                    .and_then(|selector| match selector {
                        v2::resource_selector::Selector::ThreadName(value) => {
                            Some(value.name.as_str())
                        }
                        _ => None,
                    });
                let spool = v2::SpoolRef {
                    id: fixture.spool.to_string(),
                };
                let entity = if let Some(name) = thread_name {
                    assert_eq!(name, fixture.thread_name);
                    v2::entity_ref::Entity::Thread(v2::ThreadRef {
                        spool: Some(spool),
                        id: Some(v2::ThreadId {
                            value: fixture.thread_id.clone(),
                        }),
                    })
                } else {
                    v2::entity_ref::Entity::Spool(spool)
                };
                write_unary(
                    &mut send,
                    &v2::ResolveResourcesResponse {
                        results: vec![v2::ResourceResolution {
                            resource: Some(v2::EntityRef {
                                entity: Some(entity),
                            }),
                            coverage: v2::Coverage::Complete as i32,
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                )
                .await;
            }
            "/heddle.api.v1alpha2.IdentityService/GetIdentity" => {
                let key = biscuit_verifier::PublicKey::from_bytes(
                    crypto::Signer::public_key(
                        &Ed25519Signer::from_seed(&[71; 32]).expect("owner"),
                    ),
                    biscuit_auth::Algorithm::Ed25519,
                )
                .expect("mint root");
                let token = biscuit_verifier::parse_token(
                    &base64::engine::general_purpose::URL_SAFE.encode(&context.bearer_capability),
                    &[key],
                )
                .expect("current native credential");
                let envelope = witnessing::envelope(&fixture.owner, &token);
                write_unary(
                    &mut send,
                    &v2::GetIdentityResponse {
                        identity: Some(v2::PrincipalRecord {
                            id: "principal-test".into(),
                            account_id: uuid::Uuid::from_u128(2).to_string(),
                            ..Default::default()
                        }),
                        current_credential: Some(v2::CurrentCredentialRecord {
                            thread_control_authority: envelope,
                            acting_agent_id: if fixture.scope_path.is_some() {
                                "scoped-writer".into()
                            } else {
                                String::new()
                            },
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )
                .await;
            }
            "/heddle.api.v1alpha2.ThreadService/StartThread" => {
                read_request_body(&mut recv, &mut request).await;
                let body = decode_request_frame(&request)
                    .expect("StartThread frame")
                    .body;
                let request = v2::StartThreadRequest::decode(body).expect("StartThread request");
                api::native_witness::verify_genesis_authority(
                    request.native_genesis_authority.as_ref().expect("binding"),
                    request.thread_genesis.as_ref().expect("genesis"),
                    &request.creator_authority,
                )
                .expect("creator signature before request PoP");
                let reference = thread_ref(&fixture);
                let observed = thread_api::replication::opening::verify_genesis(
                    request.thread_genesis.as_ref().expect("genesis"),
                    &reference,
                )
                .expect("exact started Thread");
                fixture.captured.lock().expect("capture").thread_genesis =
                    Some(v2::ThreadGenesisRecord {
                        genesis: request.thread_genesis,
                        creator_authority: request.creator_authority,
                        native_genesis_authority: request.native_genesis_authority,
                        ..Default::default()
                    });
                let refuse = {
                    let mut captured = fixture.captured.lock().expect("capture");
                    if captured.start_failures > 0 {
                        captured.start_failures -= 1;
                        true
                    } else {
                        false
                    }
                };
                if refuse {
                    let failure = api::heddle::api::common::CallFailure {
                        code: api::heddle::api::common::CallFailureCode::Unavailable as i32,
                        message: "StartThread failed before admission".into(),
                        ..Default::default()
                    };
                    let frame =
                        api::framing::encode_failure_response(&failure).expect("failure frame");
                    send.write_chunk(bytes::Bytes::from(frame))
                        .await
                        .expect("failure response");
                    send.finish().expect("finish");
                    return;
                }
                write_unary(
                    &mut send,
                    &v2::ThreadMutationResponse {
                        thread: Some(v2::ThreadOverview {
                            r#ref: Some(reference),
                            name: observed.name,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )
                .await;
            }
            other => panic!("unexpected hosted unary method: {other}"),
        },
        StreamingShape::ServerStreaming => match method.as_str() {
            "/heddle.api.v1alpha2.ThreadService/ObserveThreads" => {
                serve_observe_threads(&mut send, server_key, &fixture).await
            }
            _ => serve_owner_views(&mut send, &method, server_key, &fixture).await,
        },
        StreamingShape::Bidirectional => {
            let buffered = request.split_off(prelude_len);
            match method.as_str() {
                "/heddle.api.v1alpha2.SyncService/PublishContent" => {
                    serve_publication(send, recv, buffered, fixture).await;
                    return;
                }
                "/heddle.api.v1alpha2.SyncService/Fetch" => {
                    serve_fetch(send, recv, buffered, fixture, server_key).await;
                    return;
                }
                other => panic!("unexpected hosted bidirectional method: {other}"),
            }
        }
    }
    send.finish().expect("finish hosted response");
}

async fn read_request_body(recv: &mut iroh::endpoint::RecvStream, request: &mut Vec<u8>) {
    while let Some(chunk) = recv
        .read_chunk(api::framing::MAX_CONTROL_BODY + 6)
        .await
        .expect("read request body")
    {
        request.extend_from_slice(&chunk);
    }
}

async fn write_unary<M: Message>(send: &mut iroh::endpoint::SendStream, response: &M) {
    send.write_chunk(
        encode_success_response(&response.encode_to_vec())
            .expect("encode unary response")
            .into(),
    )
    .await
    .expect("write unary response");
}

async fn serve_observe_threads(
    send: &mut iroh::endpoint::SendStream,
    server_key: Vec<u8>,
    fixture: &Fixture,
) {
    let source = v2::EndpointRef {
        kind: v2::EndpointKind::Weft as i32,
        public_key: server_key,
    };
    let overview = v2::ThreadOverview {
        name: fixture.thread_name.clone(),
        r#ref: Some(thread_ref(fixture)),
        source_heads: fixture
            .captured
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .revision
            .clone()
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let events = [
        v2::ThreadListEvent {
            frame: Some(v2::StreamFrame {
                sequence: 1,
                body: Some(v2::stream_frame::Body::Open(v2::StreamOpen {
                    source: Some(source),
                    binding_digest: vec![9; 32],
                    accepted_budget: Some(v2::ReadBudget {
                        max_items: 64,
                        max_frame_bytes: 64 * 1024,
                        max_snapshot_bytes: 1024 * 1024,
                    }),
                    ..Default::default()
                })),
            }),
            ..Default::default()
        },
        v2::ThreadListEvent {
            frame: Some(v2::StreamFrame {
                sequence: 2,
                body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                    kind: v2::StreamDataKind::Snapshot as i32,
                })),
            }),
            payload: Some(v2::thread_list_event::Payload::Thread(overview)),
        },
        v2::ThreadListEvent {
            frame: Some(v2::StreamFrame {
                sequence: 3,
                body: Some(v2::stream_frame::Body::Checkpoint(v2::StreamCheckpoint {
                    cursor: vec![1],
                    snapshot_complete: true,
                    page: Some(v2::PageInfo {
                        exhausted: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                })),
            }),
            ..Default::default()
        },
        v2::ThreadListEvent {
            frame: Some(v2::StreamFrame {
                sequence: 4,
                body: Some(v2::stream_frame::Body::Complete(v2::StreamComplete {
                    cursor: vec![1],
                })),
            }),
            ..Default::default()
        },
    ];
    for event in events {
        if event.payload.is_some()
            && fixture
                .captured
                .lock()
                .expect("capture")
                .thread_genesis
                .is_none()
        {
            continue;
        }
        let mut event = event;
        if event.frame.as_ref().is_some_and(|f| f.sequence > 2)
            && fixture
                .captured
                .lock()
                .expect("capture")
                .thread_genesis
                .is_none()
        {
            event.frame.as_mut().expect("frame").sequence -= 1;
        }
        write_message(send, &event).await;
    }
}

async fn serve_owner_views(
    send: &mut iroh::endpoint::SendStream,
    method: &str,
    server_key: Vec<u8>,
    fixture: &Fixture,
) {
    let now = chrono::Utc::now().timestamp();
    let frames = [
        v2::StreamFrame {
            sequence: 1,
            body: Some(v2::stream_frame::Body::Open(v2::StreamOpen {
                source: Some(v2::EndpointRef {
                    kind: v2::EndpointKind::Weft as i32,
                    public_key: server_key,
                }),
                binding_digest: vec![9; 32],
                accepted_budget: Some(v2::ReadBudget {
                    max_items: 64,
                    max_frame_bytes: 64 * 1024,
                    max_snapshot_bytes: 1024 * 1024,
                }),
                authority_valid_until: Some(prost_types::Timestamp {
                    seconds: now + 240,
                    nanos: 0,
                }),
                ..Default::default()
            })),
        },
        v2::StreamFrame {
            sequence: 2,
            body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                kind: v2::StreamDataKind::Snapshot as i32,
            })),
        },
        v2::StreamFrame {
            sequence: 3,
            body: Some(v2::stream_frame::Body::Checkpoint(v2::StreamCheckpoint {
                cursor: vec![1],
                snapshot_complete: true,
                ..Default::default()
            })),
        },
        v2::StreamFrame {
            sequence: 4,
            body: Some(v2::stream_frame::Body::Complete(v2::StreamComplete {
                cursor: vec![1],
            })),
        },
    ];
    for (index, frame) in frames.into_iter().enumerate() {
        match method {
            "/heddle.api.v1alpha2.SpoolService/ObserveSpool" => {
                let event = v2::SpoolEvent {
                    frame: Some(frame),
                    payload: (index == 1).then(|| {
                        v2::spool_event::Payload::Spool(v2::SpoolOverview {
                            r#ref: Some(v2::SpoolRef {
                                id: fixture.spool.to_string(),
                            }),
                            version: vec![1; 32],
                            path_segments: vec!["acme".into(), "widgets".into()],
                            owner_genesis: Some(fixture.owner_genesis.clone()),
                            ..Default::default()
                        })
                    }),
                };
                write_message(send, &event).await;
            }
            "/heddle.api.v1alpha2.OwnerAuthorizationService/ObserveOwnership" => {
                write_message(
                    send,
                    &v2::OwnershipEvent {
                        frame: Some(frame),
                        payload: (index == 1)
                            .then(|| v2::ownership_event::Payload::Owner(fixture.owner.clone())),
                    },
                )
                .await;
            }
            "/heddle.api.v1alpha2.ThreadService/ObserveThread" => {
                write_message(
                    send,
                    &v2::ThreadEvent {
                        frame: Some(frame),
                        payload: (index == 1).then(|| {
                            v2::thread_event::Payload::Overview(v2::ThreadOverview {
                                r#ref: Some(thread_ref(fixture)),
                                name: fixture.thread_name.clone(),
                                ..Default::default()
                            })
                        }),
                    },
                )
                .await;
            }
            _ => panic!("unexpected observation {method}"),
        }
    }
}

fn thread_ref(fixture: &Fixture) -> v2::ThreadRef {
    v2::ThreadRef {
        spool: Some(v2::SpoolRef {
            id: fixture.spool.to_string(),
        }),
        id: Some(v2::ThreadId {
            value: fixture.thread_id.clone(),
        }),
    }
}

async fn serve_publication(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    mut buffered: Vec<u8>,
    fixture: Fixture,
) {
    let opening: v2::PublishContentClientFrame = read_message(&mut recv, &mut buffered).await;
    let Some(v2::publish_content_client_frame::Body::Open(open)) = opening.body.clone() else {
        panic!("native publication must start with Open");
    };
    assert_eq!(open.thread, Some(thread_ref(&fixture)));
    let mut logical = opening.clone();
    if let Some(v2::publish_content_client_frame::Body::Open(open)) = logical.body.as_mut() {
        open.checkpoint = None;
    }
    let digest = typed_digest("thread-source-transfer-v1", &logical.encode_to_vec());
    let checkpoint = v2::TransferCheckpoint {
        transfer_id: digest[..16].to_vec(),
        plan_digest: digest.to_vec(),
        resume_token: Vec::new(),
        committed_bytes: 0,
    };
    write_message(
        &mut send,
        &v2::PublishContentServerFrame {
            body: Some(v2::publish_content_server_frame::Body::Ready(
                v2::TransferReady {
                    endpoint: open.destination.clone(),
                    protocol: open.protocol.clone(),
                    thread: open.thread.clone(),
                    current: open.revision.clone(),
                    checkpoint: Some(checkpoint.clone()),
                    budget: Some(v2::ReadBudget {
                        max_items: 10_000,
                        max_frame_bytes: 64 * 1024,
                        max_snapshot_bytes: 16 * 1024 * 1024,
                    }),
                    ..Default::default()
                },
            )),
        },
    )
    .await;

    let mut accepted = PublicationCapture {
        revision: open.revision.clone(),
        ..Default::default()
    };
    loop {
        let frame: v2::PublishContentClientFrame = read_message(&mut recv, &mut buffered).await;
        assert_eq!(frame.client_operation_id, opening.client_operation_id);
        match frame.body {
            Some(v2::publish_content_client_frame::Body::ThreadGenesis(genesis)) => {
                assert!(accepted.thread_genesis.replace(genesis).is_none());
                write_checkpoint(&mut send, &checkpoint).await;
            }
            Some(v2::publish_content_client_frame::Body::Operations(operations)) => {
                accepted.operations.push(operations);
                write_checkpoint(&mut send, &checkpoint).await;
            }
            Some(v2::publish_content_client_frame::Body::Pack(chunk)) => {
                let extent = chunk.extent.as_ref().expect("native pack chunk extent");
                let destination = if extent.kind == v2::pack_extent::Kind::NativePack as i32 {
                    &mut accepted.pack_data
                } else {
                    assert_eq!(extent.kind, v2::pack_extent::Kind::NativeIndex as i32);
                    &mut accepted.index_data
                };
                assert_eq!(extent.offset, destination.len() as u64);
                assert_eq!(extent.length, chunk.data.len() as u64);
                destination.extend_from_slice(&chunk.data);
                write_checkpoint(&mut send, &checkpoint).await;
            }
            Some(v2::publish_content_client_frame::Body::Finish(finish)) => {
                assert_eq!(finish.checkpoint, Some(checkpoint));
                assert_eq!(accepted.pack_data.len() as u64, open.packs[0].length);
                assert_eq!(accepted.index_data.len() as u64, open.packs[1].length);
                assert!(accepted.thread_genesis.is_some());
                assert!(!accepted.operations.is_empty());
                let inventory = inventory_digest(&open.packs);
                let prior = fixture
                    .captured
                    .lock()
                    .expect("capture")
                    .native_authority
                    .clone();
                let bundle = witnessing::bundle(
                    &fixture.owner_genesis,
                    &fixture.owner,
                    accepted.thread_genesis.as_ref().expect("original genesis"),
                    &accepted.operations,
                    &fixture.witness_set,
                    prior,
                );
                for batch in &mut accepted.operations {
                    batch.native_authority = Some(bundle.clone());
                }
                accepted.native_authority = Some(bundle.clone());
                *fixture
                    .captured
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = accepted;
                write_message(
                    &mut send,
                    &v2::PublishContentServerFrame {
                        body: Some(v2::publish_content_server_frame::Body::Receipt(
                            v2::PublicationReceipt {
                                native_authority: Some(bundle),
                                import_authority: None,
                                client_operation_id: opening.client_operation_id,
                                destination: open.destination,
                                thread: open.thread,
                                revision: open.revision,
                                sharing_policy_version: if open.sharing_policy_version.is_empty() {
                                    vec![3; 32]
                                } else {
                                    open.sharing_policy_version
                                },
                                accepted_inventory: Some(v2::ObjectAddress {
                                    algorithm: "blake3".into(),
                                    digest: inventory.to_vec(),
                                }),
                                outcome: Some(v2::publication_receipt::Outcome::Accepted(
                                    v2::Applied::default(),
                                )),
                            },
                        )),
                    },
                )
                .await;
                send.finish().expect("finish native publication response");
                return;
            }
            other => panic!("unexpected native publication frame: {other:?}"),
        }
    }
}

async fn serve_fetch(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    mut buffered: Vec<u8>,
    fixture: Fixture,
    server_key: Vec<u8>,
) {
    let opening: v2::FetchClientFrame = read_message(&mut recv, &mut buffered).await;
    let Some(v2::fetch_client_frame::Body::Open(open)) = opening.body else {
        panic!("native fetch must start with Open");
    };
    let mut accepted = fixture
        .captured
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    let prefix_fixture = !accepted.prefixes.is_empty();
    if prefix_fixture {
        accepted = accepted
            .prefixes
            .iter()
            .find(|p| {
                let record = p
                    .thread_genesis
                    .as_ref()
                    .expect("wrapper")
                    .genesis
                    .as_ref()
                    .expect("genesis");
                let (_, genesis) =
                    crypto::import_authority::verify_native_genesis(record).expect("signature");
                open.thread
                    .as_ref()
                    .and_then(|r| r.id.as_ref())
                    .is_some_and(|id| id.value == genesis.id().expect("id").as_bytes())
                    && open
                        .revision
                        .as_ref()
                        .is_none_or(|r| Some(r) == p.revision.as_ref())
            })
            .expect("exact requested prefix")
            .clone();
    } else {
        assert_eq!(open.thread, Some(thread_ref(&fixture)));
    }
    let reference = open.thread.clone();
    let genesis = accepted
        .thread_genesis
        .clone()
        .expect("published Thread genesis");
    let revision = accepted.revision.clone().expect("published revision");
    assert!(
        open.revision
            .as_ref()
            .is_none_or(|value| value == &revision)
    );
    let artifacts = [&accepted.pack_data, &accepted.index_data];
    let packs = pack_extents(artifacts);
    let checkpoint = v2::TransferCheckpoint {
        transfer_id: vec![7; 16],
        plan_digest: inventory_digest(&packs).to_vec(),
        resume_token: Vec::new(),
        committed_bytes: 0,
    };
    write_message(
        &mut send,
        &v2::FetchServerFrame {
            body: Some(v2::fetch_server_frame::Body::Ready(v2::TransferReady {
                endpoint: Some(v2::EndpointRef {
                    kind: v2::EndpointKind::Weft as i32,
                    public_key: server_key,
                }),
                thread: reference,
                current: Some(revision.clone()),
                protocol: open.protocol,
                native_authority: accepted.native_authority.clone(),
                import_authority: accepted.import_authority.clone(),
                owner_genesis: accepted
                    .owner_genesis
                    .clone()
                    .or(Some(fixture.owner_genesis)),
                ownership: accepted.ownership.clone().or(Some(fixture.owner)),
                thread_genesis: Some(genesis),
                packs: packs.clone(),
                checkpoint: Some(checkpoint.clone()),
                budget: Some(v2::ReadBudget {
                    max_items: 10_000,
                    max_frame_bytes: if prefix_fixture {
                        512 * 1024
                    } else {
                        64 * 1024
                    },
                    max_snapshot_bytes: 16 * 1024 * 1024,
                }),
                full_closure_available: true,
                ..Default::default()
            })),
        },
    )
    .await;
    for operations in accepted.operations {
        write_message(
            &mut send,
            &v2::FetchServerFrame {
                body: Some(v2::fetch_server_frame::Body::Operations(operations)),
            },
        )
        .await;
    }
    for (planned, data) in packs.iter().zip(artifacts) {
        for (offset, chunk) in data.chunks(32 * 1024).enumerate() {
            let mut extent = planned.clone();
            extent.offset = (offset * 32 * 1024) as u64;
            extent.length = chunk.len() as u64;
            extent.extent_digest = Some(v2::ObjectAddress {
                algorithm: "blake3".into(),
                digest: blake3::hash(chunk).as_bytes().to_vec(),
            });
            write_message(
                &mut send,
                &v2::FetchServerFrame {
                    body: Some(v2::fetch_server_frame::Body::Pack(v2::PackChunk {
                        extent: Some(extent),
                        data: chunk.to_vec(),
                    })),
                },
            )
            .await;
        }
    }
    let mut complete_checkpoint = checkpoint;
    complete_checkpoint.committed_bytes = artifacts.iter().map(|data| data.len() as u64).sum();
    write_message(
        &mut send,
        &v2::FetchServerFrame {
            body: Some(v2::fetch_server_frame::Body::Complete(v2::FetchComplete {
                revision: Some(revision),
                checkpoint: Some(complete_checkpoint),
                closure: v2::Coverage::Complete as i32,
                missing: Vec::new(),
            })),
        },
    )
    .await;
    send.finish().expect("finish native fetch response");
}

async fn read_message<M: Message + Default>(
    recv: &mut iroh::endpoint::RecvStream,
    buffered: &mut Vec<u8>,
) -> M {
    loop {
        if let Some((frame, consumed)) = decode_stream_frame(buffered).expect("native frame") {
            let StreamFrame::Message(body) = frame else {
                panic!("expected native message frame, got {frame:?}");
            };
            let body = body.to_vec();
            buffered.drain(..consumed);
            return M::decode(body.as_slice()).expect("native protobuf message");
        }
        let chunk = recv
            .read_chunk(api::framing::MAX_CONTROL_BODY + 5)
            .await
            .expect("read native frame")
            .expect("native message frame");
        buffered.extend_from_slice(&chunk);
    }
}

async fn write_message<M: Message>(send: &mut iroh::endpoint::SendStream, message: &M) {
    send.write_chunk(
        encode_stream_message(&message.encode_to_vec())
            .expect("encode native frame")
            .into(),
    )
    .await
    .expect("write native frame");
}

async fn write_checkpoint(
    send: &mut iroh::endpoint::SendStream,
    checkpoint: &v2::TransferCheckpoint,
) {
    write_message(
        send,
        &v2::PublishContentServerFrame {
            body: Some(v2::publish_content_server_frame::Body::Checkpoint(
                checkpoint.clone(),
            )),
        },
    )
    .await;
}

fn pack_extents(artifacts: [&Vec<u8>; 2]) -> Vec<v2::PackExtent> {
    artifacts
        .into_iter()
        .enumerate()
        .map(|(index, bytes)| {
            assert!(!bytes.is_empty());
            let address = v2::ObjectAddress {
                algorithm: "blake3".into(),
                digest: blake3::hash(bytes).as_bytes().to_vec(),
            };
            v2::PackExtent {
                pack: Some(address.clone()),
                kind: if index == 0 {
                    v2::pack_extent::Kind::NativePack
                } else {
                    v2::pack_extent::Kind::NativeIndex
                } as i32,
                offset: 0,
                length: bytes.len() as u64,
                extent_digest: Some(address),
            }
        })
        .collect()
}

fn inventory_digest(packs: &[v2::PackExtent]) -> [u8; 32] {
    let mut inventory = Vec::new();
    for extent in packs {
        extent
            .encode_length_delimited(&mut inventory)
            .expect("encode native inventory");
    }
    typed_digest("thread-source-inventory-v1", &inventory)
}

fn typed_digest(kind: &str, bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(kind.as_bytes());
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(&[0]);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}
