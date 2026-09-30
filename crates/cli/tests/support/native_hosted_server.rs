// SPDX-License-Identifier: Apache-2.0
//! Minimal v2 Weft fixture for publication and bounded Thread review tests.
// Shared fixtures expose entry points used by different test binaries.
#![allow(dead_code)]

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
use crypto::{Ed25519Signer, Signer};
use hosted_client::hosted_runtime::hosted::{CallContextFactory, HostedClient};
use iroh::{Endpoint, RelayMode, endpoint::presets};
use prost::Message;
use tokio::task::JoinHandle;

#[derive(Clone, Debug, Default)]
pub struct PublicationCapture {
    pub import_requests: Vec<v2::ImportSourceRequest>,
    pub calls: Vec<String>,
    pub revision: Option<v2::RevisionRef>,
    pub thread_genesis: Option<v2::ThreadGenesisRecord>,
    pub operations: Vec<v2::ReplicationOperations>,
    pub pack_data: Vec<u8>,
    pub index_data: Vec<u8>,
    pub started: Vec<v2::ThreadOverview>,
    pub evidence: Vec<v2::RecordEvidenceRequest>,
    pub contexts: Vec<v2::PutContextRequest>,
    pub discussions: Vec<v2::OpenDiscussionRequest>,
}

#[derive(Clone)]
struct Fixture {
    spool: uuid::Uuid,
    thread_name: String,
    thread_id: Vec<u8>,
    owner_genesis: v2::SignedSpoolOwnerGenesis,
    owner: v2::OwnerState,
    captured: Arc<Mutex<PublicationCapture>>,
}

pub async fn start(
    spool: uuid::Uuid,
    thread_name: impl Into<String>,
    thread_id: [u8; 32],
) -> (HostedClient, JoinHandle<()>, Arc<Mutex<PublicationCapture>>) {
    let (client, task, captured, _, _) = start_inner(spool, thread_name, thread_id, false).await;
    (client, task, captured)
}

pub async fn start_routed(
    spool: uuid::Uuid,
    thread_name: impl Into<String>,
    thread_id: [u8; 32],
) -> (
    HostedClient,
    JoinHandle<()>,
    Arc<Mutex<PublicationCapture>>,
    iroh::EndpointAddr,
    iroh::SecretKey,
) {
    start_inner(spool, thread_name, thread_id, true).await
}

async fn start_inner(
    spool: uuid::Uuid,
    thread_name: impl Into<String>,
    thread_id: [u8; 32],
    routed: bool,
) -> (
    HostedClient,
    JoinHandle<()>,
    Arc<Mutex<PublicationCapture>>,
    iroh::EndpointAddr,
    iroh::SecretKey,
) {
    let captured = Arc::new(Mutex::new(PublicationCapture::default()));
    let owner_signer = Ed25519Signer::generate().expect("hosted test owner");
    let recovery = Ed25519Signer::generate().expect("hosted test recovery owner");
    let account = uuid::Uuid::from_bytes([9; 16]);
    let root =
        repo::sign_custodial_owner_root(&owner_signer, &recovery, *account.as_bytes(), [98; 32])
            .expect("hosted test owner root");
    let binding = repo::sign_custodial_owner_binding(&owner_signer, &root, [99; 32])
        .expect("hosted test owner binding");
    let state_hash = binding.root_state_hash.clone();
    let owner_id = root
        .root
        .as_ref()
        .expect("hosted test owner root body")
        .owner_id
        .clone();
    let owner_genesis = repo::sign_spool_owner_genesis(&owner_signer, *spool.as_bytes())
        .expect("hosted test owner genesis");
    let owner = v2::OwnerState {
        owner: Some(v2::PrincipalRef {
            id: account.to_string(),
        }),
        root: Some(root.clone()),
        binding: Some(binding),
        version: state_hash.clone(),
        resource_keyring: Some(v2::CloneAuthorizationKeyring {
            format_version: 1,
            spool_uuid: spool.as_bytes().to_vec(),
            canonical_spool_path_segments: vec!["acme".into(), "widgets".into()],
            pin: Some(v2::CloneOwnerPin {
                kind: v2::CloneOwnerPinKind::CloneTofu as i32,
                expected_owner_id: owner_id,
                first_seen_unix_seconds: 1,
            }),
            owner_root: Some(root),
            accepted_state_hash: state_hash,
            owner_genesis: Some(owner_genesis.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let fixture = Fixture {
        spool,
        thread_name: thread_name.into(),
        thread_id: thread_id.to_vec(),
        owner_genesis,
        owner,
        captured: Arc::clone(&captured),
    };

    let mut secret_bytes = [0; 32];
    getrandom::fill(&mut secret_bytes).expect("fixture endpoint key");
    let secret = iroh::SecretKey::from_bytes(&secret_bytes);
    let server = Endpoint::builder(presets::Minimal)
        .secret_key(secret.clone())
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
        while let Some(incoming) = server.accept().await {
            let connection = incoming.await.expect("hosted test handshake");
            let fixture = fixture.clone();
            let server_key = server_key.clone();
            let connection_task = async move {
                while let Ok((send, recv)) = connection.accept_bi().await {
                    tokio::spawn(serve_call(send, recv, fixture.clone(), server_key.clone()));
                }
            };
            if routed {
                tokio::spawn(connection_task);
            } else {
                connection_task.await;
                server.close().await;
                break;
            }
        }
    });
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("hosted client bind address")
        .bind()
        .await
        .expect("hosted client endpoint");
    let client_signer = Ed25519Signer::generate().expect("hosted test client signer");
    let context = CallContextFactory::default()
        .with_signing_key_pem(
            &client_signer.to_pem().expect("hosted test client key"),
            "principal:test",
        )
        .expect("hosted test call context");
    let client = HostedClient::connect_addr_with_context(endpoint, server_addr.clone(), context)
        .await
        .expect("connect hosted test client");
    (client, server_task, captured, server_addr, secret)
}

async fn serve_call(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    fixture: Fixture,
    server_key: Vec<u8>,
) {
    let mut request = Vec::new();
    let (method, prelude_len) = loop {
        let chunk = recv
            .read_chunk(api::framing::MAX_CONTROL_BODY + 6)
            .await
            .expect("read request prelude")
            .expect("request prelude");
        request.extend_from_slice(&chunk);
        if let Some((prelude, consumed)) =
            decode_request_prelude(&request).expect("decode request prelude")
        {
            break (prelude.method.to_string(), consumed);
        }
    };
    fixture
        .captured
        .lock()
        .expect("capture calls")
        .calls
        .push(method.clone());
    let streaming = method_descriptor(&method)
        .map(|descriptor| descriptor.streaming)
        .or_else(|| api::v2::method_descriptor(&method).map(|descriptor| descriptor.streaming))
        .expect("registered hosted method");
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
                        "/heddle.api.v1alpha2.SpoolService/ObserveSpool".into(),
                        "/heddle.api.v1alpha2.OwnerAuthorizationService/ObserveOwnership".into(),
                        "/heddle.api.v1alpha2.IdentityService/ObserveIdentity".into(),
                        "/heddle.api.v1alpha2.IntegrationService/ImportSource".into(),
                        "/heddle.api.v1alpha2.ThreadService/ObserveThreads".into(),
                        "/heddle.api.v1alpha2.ThreadService/ObserveThread".into(),
                        "/heddle.api.v1alpha2.SyncService/PublishContent".into(),
                        "/heddle.api.v1alpha2.SyncService/Fetch".into(),
                        "/heddle.api.v1alpha2.ThreadService/StartThread".into(),
                        "/heddle.api.v1alpha2.IdentityService/GetIdentity".into(),
                        "/heddle.api.v1alpha2.EvidenceService/RecordEvidence".into(),
                        "/heddle.api.v1alpha2.CollaborationService/ObserveCollaboration".into(),
                        "/heddle.api.v1alpha2.CollaborationService/OpenDiscussion".into(),
                        "/heddle.api.v1alpha2.CollaborationService/PutContext".into(),
                    ],
                    default_read_budget: Some(v2::ReadBudget {
                        max_items: 64,
                        max_frame_bytes: 64 * 1024,
                        max_snapshot_bytes: 1024 * 1024,
                    }),
                    max_pending_batch_bytes: 1024 * 1024,
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
                    let thread_id = if name == fixture.thread_name {
                        fixture.thread_id.clone()
                    } else {
                        assert_eq!(name, "main", "fixture's landing target");
                        vec![24; 32]
                    };
                    v2::entity_ref::Entity::Thread(v2::ThreadRef {
                        spool: Some(spool),
                        id: Some(v2::ThreadId { value: thread_id }),
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
                    },
                )
                .await;
            }
            "/heddle.api.v1alpha2.IntegrationService/ImportSource" => {
                read_request_body(&mut recv, &mut request).await;
                let body = v2::ImportSourceRequest::decode(
                    decode_request_frame(&request).expect("import frame").body,
                )
                .expect("import request");
                fixture
                    .captured
                    .lock()
                    .expect("capture import")
                    .import_requests
                    .push(body.clone());
                write_unary(
                    &mut send,
                    &v2::MutationResponse {
                        receipt: Some(v2::MutationReceipt {
                            client_operation_id: body.client_operation_id.clone(),
                            endpoint: Some(v2::EndpointRef {
                                kind: v2::EndpointKind::Weft as i32,
                                public_key: server_key,
                            }),
                            outcome: Some(v2::mutation_receipt::Outcome::PendingOperation(
                                v2::RecordRef {
                                    spool: body.destination,
                                    id: body.client_operation_id,
                                },
                            )),
                            ..Default::default()
                        }),
                    },
                )
                .await;
            }
            "/heddle.api.v1alpha2.ThreadService/StartThread" => {
                read_request_body(&mut recv, &mut request).await;
                let body = v2::StartThreadRequest::decode(
                    decode_request_frame(&request).expect("start frame").body,
                )
                .expect("start request");
                let record = body.thread_genesis.expect("original genesis");
                let original = record;
                let signed = crypto::thread_operation::SignedGenesis {
                    canonical: original.canonical_record,
                    signature: original.signatures[0].signature.clone(),
                };
                let genesis = signed.verify().expect("verify original genesis");
                assert_eq!(genesis.spool, fixture.spool.to_string());
                let overview = v2::ThreadOverview {
                    name: genesis.name.clone(),
                    r#ref: Some(v2::ThreadRef {
                        spool: Some(v2::SpoolRef {
                            id: genesis.spool.clone(),
                        }),
                        id: Some(v2::ThreadId {
                            value: genesis.id().expect("Thread id").as_bytes().to_vec(),
                        }),
                    }),
                    ..Default::default()
                };
                fixture
                    .captured
                    .lock()
                    .expect("started Threads")
                    .started
                    .push(overview.clone());
                write_unary(
                    &mut send,
                    &v2::ThreadMutationResponse {
                        thread: Some(overview),
                        ..Default::default()
                    },
                )
                .await;
            }
            "/heddle.api.v1alpha2.IdentityService/GetIdentity" => {
                read_request_body(&mut recv, &mut request).await;
                let frame = decode_request_frame(&request).expect("identity frame");
                let _context = frame.context;
                write_unary(
                    &mut send,
                    &v2::GetIdentityResponse {
                        identity: Some(v2::PrincipalRecord {
                            id: "principal-test".into(),
                            account_id: uuid::Uuid::from_bytes([9; 16]).to_string(),
                            ..Default::default()
                        }),
                        current_credential: Some(v2::CurrentCredentialRecord {
                            kind: v2::CredentialKind::Device as i32,
                            subject: "clone-test".into(),
                            proof_public_key: crypto::Ed25519Signer::from_seed(&[71; 32])
                                .expect("fixture credential signer")
                                .public_key()
                                .to_vec(),
                            thread_control_authority: vec![6; 32],
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )
                .await;
            }
            "/heddle.api.v1alpha2.EvidenceService/RecordEvidence" => {
                read_request_body(&mut recv, &mut request).await;
                let body = v2::RecordEvidenceRequest::decode(
                    decode_request_frame(&request).expect("evidence frame").body,
                )
                .expect("evidence request");
                let projection = body.evidence.as_ref().expect("signed evidence");
                let original = thread_api::evidence::verify_evidence(
                    projection.evidence.as_ref().expect("evidence original"),
                )
                .expect("verify evidence");
                assert_eq!(original.spool, fixture.spool);
                assert_eq!(original.thread.as_bytes().as_slice(), fixture.thread_id);
                let reference = projection.r#ref.clone().expect("evidence ref");
                fixture
                    .captured
                    .lock()
                    .expect("record evidence")
                    .evidence
                    .push(body.clone());
                write_unary(
                    &mut send,
                    &v2::MutationResponse {
                        receipt: Some(v2::MutationReceipt {
                            client_operation_id: body.client_operation_id,
                            outcome: Some(v2::mutation_receipt::Outcome::Applied(v2::Applied {
                                resulting_versions: vec![v2::ExpectedVersion {
                                    resource: Some(v2::EntityRef {
                                        entity: Some(v2::entity_ref::Entity::Evidence(reference)),
                                    }),
                                    ..Default::default()
                                }],
                            })),
                            ..Default::default()
                        }),
                    },
                )
                .await;
            }
            "/heddle.api.v1alpha2.CollaborationService/OpenDiscussion" => {
                read_request_body(&mut recv, &mut request).await;
                let body = v2::OpenDiscussionRequest::decode(
                    decode_request_frame(&request)
                        .expect("discussion frame")
                        .body,
                )
                .expect("discussion request");
                let operation = thread_api::collaboration::verify(
                    body.signed_operation.as_ref().expect("signed discussion"),
                )
                .expect("verify discussion");
                let objects::object::thread_replication::ThreadOperationBody::Discussion(bytes) =
                    operation.body
                else {
                    panic!("discussion original")
                };
                let envelope = objects::object::CollaborationOperationEnvelope::decode(&bytes)
                    .expect("original discussion command");
                fixture
                    .captured
                    .lock()
                    .expect("capture discussion")
                    .discussions
                    .push(body.clone());
                // Match weft's command admission. #1900 remains visible here;
                // source success must not conceal a mismatched signed command.
                assert_ne!(
                    body.client_operation_id,
                    envelope.operation.idempotency_key.as_str(),
                    "remove the #1900 expectation when that separate bug is fixed"
                );
                let failure =
                    api::framing::encode_failure_response(&api::heddle::api::common::CallFailure {
                        code: api::heddle::api::common::CallFailureCode::InvalidArgument as i32,
                        message: "command ID differs from signed operation".into(),
                        ..Default::default()
                    })
                    .expect("command failure");
                send.write_all(&failure)
                    .await
                    .expect("discussion rejection");
            }
            "/heddle.api.v1alpha2.CollaborationService/PutContext" => {
                read_request_body(&mut recv, &mut request).await;
                let body = v2::PutContextRequest::decode(
                    decode_request_frame(&request).expect("context frame").body,
                )
                .expect("context request");
                thread_api::collaboration::verify(
                    body.signed_operation.as_ref().expect("signed context"),
                )
                .expect("verify context");
                fixture
                    .captured
                    .lock()
                    .expect("capture context")
                    .contexts
                    .push(body.clone());
                write_unary(
                    &mut send,
                    &v2::MutationResponse {
                        receipt: Some(v2::MutationReceipt {
                            client_operation_id: body.client_operation_id,
                            endpoint: Some(v2::EndpointRef {
                                kind: v2::EndpointKind::Weft as i32,
                                public_key: server_key,
                            }),
                            outcome: Some(v2::mutation_receipt::Outcome::Applied(
                                v2::Applied::default(),
                            )),
                            ..Default::default()
                        }),
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
            "/heddle.api.v1alpha2.ThreadService/ObserveThread" => {
                // Share the review fixture's weft admission check and pagination.
                hosted_client::hosted_runtime::hosted::test_server::serve_native_thread_review(
                    &mut send,
                    &mut recv,
                    &mut request,
                    server_key,
                )
                .await;
            }
            "/heddle.api.v1alpha2.SpoolService/ObserveSpool" => {
                for frame in snapshot_frames(server_key) {
                    let payload =
                        matches!(frame.body, Some(v2::stream_frame::Body::Data(_))).then(|| {
                            v2::spool_event::Payload::Spool(v2::SpoolOverview {
                                r#ref: Some(v2::SpoolRef {
                                    id: fixture.spool.to_string(),
                                }),
                                version: vec![7; 32],
                                ..Default::default()
                            })
                        });
                    write_message(
                        &mut send,
                        &v2::SpoolEvent {
                            frame: Some(frame),
                            payload,
                        },
                    )
                    .await;
                }
            }
            "/heddle.api.v1alpha2.OwnerAuthorizationService/ObserveOwnership" => {
                for frame in snapshot_frames(server_key) {
                    let payload = matches!(frame.body, Some(v2::stream_frame::Body::Data(_)))
                        .then(|| v2::ownership_event::Payload::Owner(fixture.owner.clone()));
                    write_message(
                        &mut send,
                        &v2::OwnershipEvent {
                            frame: Some(frame),
                            payload,
                        },
                    )
                    .await;
                }
            }
            "/heddle.api.v1alpha2.IdentityService/ObserveIdentity" => {
                let mut sequence = 1;
                for mut frame in snapshot_frames(server_key) {
                    let data = matches!(frame.body, Some(v2::stream_frame::Body::Data(_)));
                    if data {
                        write_message(
                            &mut send,
                            &v2::IdentityEvent {
                                frame: Some(v2::StreamFrame {
                                    sequence,
                                    body: frame.body.clone(),
                                }),
                                payload: Some(v2::identity_event::Payload::Identity(
                                    v2::PrincipalRecord {
                                        id: "principal-test".into(),
                                        account_id: uuid::Uuid::from_bytes([9; 16]).to_string(),
                                        personal_spool: Some(v2::SpoolAddress {
                                            r#ref: Some(v2::SpoolRef {
                                                id: fixture.spool.to_string(),
                                            }),
                                            path_segments: vec!["personal".into()],
                                        }),
                                        ..Default::default()
                                    },
                                )),
                            },
                        )
                        .await;
                        sequence += 1;
                    }
                    frame.sequence = sequence;
                    sequence += 1;
                    let payload = data.then(|| {
                        v2::identity_event::Payload::CurrentCredential(
                            v2::CurrentCredentialRecord {
                                kind: v2::CredentialKind::Device as i32,
                                subject: "clone-test".into(),
                                thread_control_authority: vec![6; 32],
                                ..Default::default()
                            },
                        )
                    });
                    write_message(
                        &mut send,
                        &v2::IdentityEvent {
                            frame: Some(frame),
                            payload,
                        },
                    )
                    .await;
                }
            }
            "/heddle.api.v1alpha2.CollaborationService/ObserveCollaboration" => {
                for (index, mut frame) in snapshot_frames(server_key)
                    .into_iter()
                    .filter(|frame| !matches!(frame.body, Some(v2::stream_frame::Body::Data(_))))
                    .enumerate()
                {
                    frame.sequence = (index + 1) as u64;
                    write_message(
                        &mut send,
                        &v2::CollaborationEvent {
                            frame: Some(frame),
                            ..Default::default()
                        },
                    )
                    .await;
                }
            }
            other => panic!("unexpected hosted observation: {other}"),
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

fn snapshot_frames(server_key: Vec<u8>) -> Vec<v2::StreamFrame> {
    vec![
        v2::stream_frame::Body::Open(v2::StreamOpen {
            source: Some(v2::EndpointRef {
                kind: v2::EndpointKind::Weft as i32,
                public_key: server_key,
            }),
            binding_digest: vec![8; 32],
            accepted_budget: Some(v2::ReadBudget {
                max_items: 64,
                max_frame_bytes: 65536,
                max_snapshot_bytes: 1048576,
            }),
            ..Default::default()
        }),
        v2::stream_frame::Body::Data(v2::StreamData {
            kind: v2::StreamDataKind::Snapshot as i32,
        }),
        v2::stream_frame::Body::Checkpoint(v2::StreamCheckpoint {
            cursor: vec![1],
            snapshot_complete: true,
            page: Some(v2::PageInfo {
                exhausted: true,
                ..Default::default()
            }),
            ..Default::default()
        }),
        v2::stream_frame::Body::Complete(v2::StreamComplete { cursor: vec![1] }),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, body)| v2::StreamFrame {
        sequence: (index + 1) as u64,
        body: Some(body),
    })
    .collect()
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
    let mut overviews = vec![overview];
    overviews.extend(
        fixture
            .captured
            .lock()
            .expect("started listing")
            .started
            .clone(),
    );
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
            payload: Some(v2::thread_list_event::Payload::Thread(overviews.remove(0))),
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
    let mut sequence = 1;
    for mut event in events {
        if matches!(
            event.frame.as_ref().and_then(|f| f.body.as_ref()),
            Some(v2::stream_frame::Body::Checkpoint(_))
        ) {
            for overview in overviews.drain(..) {
                write_message(
                    send,
                    &v2::ThreadListEvent {
                        frame: Some(v2::StreamFrame {
                            sequence,
                            body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                                kind: v2::StreamDataKind::Snapshot as i32,
                            })),
                        }),
                        payload: Some(v2::thread_list_event::Payload::Thread(overview)),
                    },
                )
                .await;
                sequence += 1;
            }
        }
        event.frame.as_mut().expect("listing frame").sequence = sequence;
        sequence += 1;
        write_message(send, &event).await;
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
    assert!(
        open.thread == Some(thread_ref(&fixture))
            || fixture
                .captured
                .lock()
                .expect("started publication")
                .started
                .iter()
                .any(|t| t.r#ref == open.thread)
    );
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
                {
                    let mut capture = fixture.captured.lock().expect("accepted publication");
                    accepted.calls = std::mem::take(&mut capture.calls);
                    accepted.started = std::mem::take(&mut capture.started);
                    accepted.evidence = std::mem::take(&mut capture.evidence);
                    accepted.contexts = std::mem::take(&mut capture.contexts);
                    accepted.discussions = std::mem::take(&mut capture.discussions);
                    *capture = accepted;
                }
                write_message(
                    &mut send,
                    &v2::PublishContentServerFrame {
                        body: Some(v2::publish_content_server_frame::Body::Receipt(
                            v2::PublicationReceipt {
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
    assert_eq!(open.thread, Some(thread_ref(&fixture)));
    let accepted = fixture
        .captured
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
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
                thread: Some(thread_ref(&fixture)),
                current: Some(revision.clone()),
                owner_genesis: Some(fixture.owner_genesis),
                ownership: Some(fixture.owner),
                thread_genesis: Some(genesis),
                packs: packs.clone(),
                checkpoint: Some(checkpoint.clone()),
                budget: Some(v2::ReadBudget {
                    max_items: 10_000,
                    max_frame_bytes: 64 * 1024,
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
