// SPDX-License-Identifier: Apache-2.0
//! Minimal v2 Weft fixture for adopt publication round-trip tests.

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
use crypto::Ed25519Signer;
use hosted_client::hosted_runtime::hosted::{CallContextFactory, HostedClient};
use iroh::{Endpoint, RelayMode, endpoint::presets};
use prost::Message;
use tokio::task::JoinHandle;

#[derive(Clone, Debug, Default)]
pub struct PublicationCapture {
    pub revision: Option<v2::RevisionRef>,
    pub thread_genesis: Option<v2::ThreadGenesisRecord>,
    pub operations: Vec<v2::ReplicationOperations>,
    pub pack_data: Vec<u8>,
    pub index_data: Vec<u8>,
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
    let client_signer = Ed25519Signer::generate().expect("hosted test client signer");
    let context = CallContextFactory::default()
        .with_signing_key_pem(
            &client_signer.to_pem().expect("hosted test client key"),
            "principal:test",
        )
        .expect("hosted test call context");
    let client = HostedClient::connect_addr_with_context(endpoint, server_addr, context)
        .await
        .expect("connect hosted test client");
    (client, server_task, captured)
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
                        "/heddle.api.v1alpha2.ThreadService/ObserveThreads".into(),
                        "/heddle.api.v1alpha2.SyncService/PublishContent".into(),
                        "/heddle.api.v1alpha2.SyncService/Fetch".into(),
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
                    },
                )
                .await;
            }
            other => panic!("unexpected hosted unary method: {other}"),
        },
        StreamingShape::ServerStreaming => {
            assert_eq!(method, "/heddle.api.v1alpha2.ThreadService/ObserveThreads");
            serve_observe_threads(&mut send, server_key, &fixture).await;
        }
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
                *fixture
                    .captured
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = accepted;
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
