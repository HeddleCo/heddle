// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::HashMap,
    net::Ipv4Addr,
    sync::{Arc, Mutex},
};

use api::{
    StreamingShape,
    framing::{
        StreamFrame, decode_request_frame, decode_request_prelude, decode_stream_frame,
        encode_failure_response, encode_stream_failure, encode_stream_message,
        encode_success_response,
    },
    heddle::api::{
        common::{CallFailure, CallFailureCode, StateId},
        v1alpha2 as v2,
        v1alpha2::SignedSpoolOwnerGenesis,
    },
    method_descriptor,
};
use bytes::Bytes;
use crypto::Ed25519Signer;
use iroh::{Endpoint, RelayMode, endpoint::presets};
use prost::Message;
use tokio::task::JoinHandle;

use super::{CallContextFactory, HostedClient};
use crate::legacy_v1::{
    AnnotatedFile, ContextRevision, Discussion, DiscussionResolution, DiscussionTurn,
    ListRefsPageEnd, ListRefsResponse, PackChunk, PackStreamKind, PathSymbolRef, PullComplete,
    PullReady, PullServerFrame, PushClientFrame, PushComplete, PushReady, PushRequest,
    PushServerFrame, StateContextEntry, TransferCheckpoint, TransportMode, discussion_resolution,
    list_refs_response, pull_server_frame, push_client_frame, push_server_frame,
};

const OWNER_GENESIS_FIXTURE_HEX: &str = "0a380a10222222222222222222222222222222221224080112208a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c12640a20def88318e44a809464c1022f22230567bae6805d17b1ccfc2bebe5326232c58a1240bfe677c0b6fec8d28e379f584f36dee7258d834222f9b75f61dc75b7db2d836d76d4fb6eaf9e7f561925b2e6882b51eadaf3ec77c565f5b638ad0febfc8cd304";
const OBSERVE_COLLABORATION_METHOD: &str =
    "/heddle.api.v1alpha2.CollaborationService/ObserveCollaboration";

#[derive(Default)]
pub(crate) struct SpoolMutationCapture {
    pub native_updates: Vec<v2::ReviseSpoolRequest>,
    pub native_deletes: Vec<v2::DeleteSpoolRequest>,
}

fn owner_genesis_fixture() -> SignedSpoolOwnerGenesis {
    let bytes = hex::decode(OWNER_GENESIS_FIXTURE_HEX).expect("published v2 fixture hex");
    SignedSpoolOwnerGenesis::decode(bytes.as_slice()).expect("published v2 fixture genesis")
}

#[derive(Clone, Default)]
pub(crate) struct CollaborationFixture {
    pub discussions: HashMap<String, Discussion>,
    pub list: Vec<Discussion>,
    pub hidden: HashMap<String, CallFailureCode>,
    pub get_requests: Arc<Mutex<Vec<String>>>,
    pub get_request_state_ids: Arc<Mutex<Vec<Option<Vec<u8>>>>>,
    pub list_requests: Arc<Mutex<usize>>,
}

#[derive(Clone, Default)]
pub(crate) struct ContextFixture {
    pub files: Vec<AnnotatedFile>,
    pub states: Vec<StateContextEntry>,
    pub histories: HashMap<String, Vec<ContextRevision>>,
    pub list_requests: Arc<Mutex<usize>>,
    pub history_requests: Arc<Mutex<Vec<String>>>,
    /// When true, PutContext returns Dedup Conflict for the create nonce.
    pub put_conflict: bool,
    pub put_requests: Arc<Mutex<usize>>,
}

pub async fn start() -> (HostedClient, JoinHandle<()>) {
    start_inner(None, None, None, None, None, None).await
}

#[cfg(test)]
pub(crate) async fn start_with_collaboration(
    fixture: CollaborationFixture,
) -> (HostedClient, JoinHandle<()>, CollaborationFixture) {
    let fixture_clone = fixture.clone();
    let (client, server) = start_inner(None, None, None, None, None, Some(fixture)).await;
    (client, server, fixture_clone)
}

#[cfg(test)]
pub(crate) async fn start_with_context(
    fixture: ContextFixture,
) -> (HostedClient, JoinHandle<()>, ContextFixture) {
    let fixture_clone = fixture.clone();
    let (client, server) = start_inner(None, None, None, None, Some(fixture), None).await;
    (client, server, fixture_clone)
}

#[cfg(test)]
pub(crate) async fn start_recording_push()
-> (HostedClient, JoinHandle<()>, Arc<Mutex<Vec<PushRequest>>>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (client, server) =
        start_inner(None, None, None, Some(Arc::clone(&captured)), None, None).await;
    (client, server, captured)
}

#[cfg(test)]
pub(crate) async fn start_recording_create_spool() -> (
    HostedClient,
    JoinHandle<()>,
    Arc<Mutex<Vec<v2::CreateSpoolRequest>>>,
) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (client, server) =
        start_inner(None, Some(Arc::clone(&captured)), None, None, None, None).await;
    (client, server, captured)
}

#[cfg(test)]
pub(crate) async fn start_recording_spool_mutations() -> (
    HostedClient,
    JoinHandle<()>,
    Arc<Mutex<SpoolMutationCapture>>,
) {
    let captured = Arc::new(Mutex::new(SpoolMutationCapture::default()));
    let (client, server) =
        start_inner(None, None, Some(Arc::clone(&captured)), None, None, None).await;
    (client, server, captured)
}

#[cfg(test)]
pub(crate) async fn start_with_remote_state(
    remote_state: StateId,
) -> (HostedClient, JoinHandle<()>) {
    start_inner(
        Some(PullFixture {
            remote_state,
            pack: None,
        }),
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn start_with_pull_pack(
    remote_state: StateId,
    pack_data: Vec<u8>,
    index_data: Vec<u8>,
) -> (HostedClient, JoinHandle<()>) {
    start_inner(
        Some(PullFixture {
            remote_state,
            pack: Some((pack_data, index_data)),
        }),
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

#[derive(Clone)]
struct PullFixture {
    remote_state: StateId,
    pack: Option<(Vec<u8>, Vec<u8>)>,
}

#[derive(Clone)]
struct TestServerState {
    pull: Option<PullFixture>,
    create_spool: Option<Arc<Mutex<Vec<v2::CreateSpoolRequest>>>>,
    spool_mutations: Option<Arc<Mutex<SpoolMutationCapture>>>,
    push_requests: Option<Arc<Mutex<Vec<PushRequest>>>>,
    context: Option<ContextFixture>,
    collaboration: Option<CollaborationFixture>,
    server_key: Vec<u8>,
    owner: v2::OwnerState,
    grants: Arc<Mutex<Vec<v2::GrantRecord>>>,
    live_discussions: Arc<Mutex<HashMap<String, Discussion>>>,
    live_operations: Arc<Mutex<HashMap<String, Vec<v2::SignedRecord>>>>,
}

async fn start_inner(
    pull: Option<PullFixture>,
    create_spool: Option<Arc<Mutex<Vec<v2::CreateSpoolRequest>>>>,
    spool_mutations: Option<Arc<Mutex<SpoolMutationCapture>>>,
    push_requests: Option<Arc<Mutex<Vec<PushRequest>>>>,
    context: Option<ContextFixture>,
    collaboration: Option<CollaborationFixture>,
) -> (HostedClient, JoinHandle<()>) {
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let server_addr = server.addr();
    let server_key = server.id().as_bytes().to_vec();
    let signer = Ed25519Signer::generate().unwrap();
    let recovery = Ed25519Signer::from_seed(&[97; 32]).unwrap();
    let root = repo::sign_custodial_owner_root(&signer, &recovery, [9; 16], [98; 32]).unwrap();
    let binding = repo::sign_custodial_owner_binding(&signer, &root, [99; 32]).unwrap();
    let verified = heddleco_capability_verifier::verify_owner_root(&root).unwrap();
    let owner = v2::OwnerState {
        owner: Some(v2::PrincipalRef {
            id: uuid::Uuid::from_bytes([9; 16]).to_string(),
        }),
        root: Some(root),
        binding: Some(binding),
        version: verified.state_hash().to_vec(),
        ..Default::default()
    };
    let state = TestServerState {
        pull,
        create_spool,
        spool_mutations,
        push_requests,
        context,
        collaboration,
        server_key,
        owner,
        grants: Arc::new(Mutex::new(Vec::<v2::GrantRecord>::new())),
        live_discussions: Arc::new(Mutex::new(HashMap::<String, Discussion>::new())),
        live_operations: Arc::new(Mutex::new(HashMap::<String, Vec<v2::SignedRecord>>::new())),
    };
    let server_task = tokio::spawn(async move {
        let connection = server
            .accept()
            .await
            .expect("hosted test connection")
            .await
            .unwrap();
        while let Ok((send, recv)) = connection.accept_bi().await {
            tokio::spawn(serve_call(send, recv, state.clone()));
        }
        server.close().await;
    });
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let context = CallContextFactory::default()
        .with_signing_key_pem(&signer.to_pem().unwrap(), "principal:test")
        .unwrap();
    let client = HostedClient::connect_addr_with_context(endpoint, server_addr, context)
        .await
        .unwrap();
    (client, server_task)
}

async fn serve_call(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    state: TestServerState,
) {
    let TestServerState {
        pull,
        create_spool,
        spool_mutations,
        push_requests,
        context,
        collaboration,
        server_key,
        owner,
        grants,
        live_discussions,
        live_operations,
    } = state;
    let mut request = Vec::new();
    let (method, prelude_len) = loop {
        let chunk = recv
            .read_chunk(api::framing::MAX_CONTROL_BODY + 6)
            .await
            .unwrap()
            .expect("request prelude");
        request.extend_from_slice(&chunk);
        if let Some((prelude, consumed)) = decode_request_prelude(&request).unwrap() {
            break (prelude.method.to_string(), consumed);
        }
    };
    let streaming = method_descriptor(&method)
        .map(|descriptor| descriptor.streaming)
        .or_else(|| api::v2::method_descriptor(&method).map(|descriptor| descriptor.streaming))
        .expect("registered hosted method");
    match streaming {
        StreamingShape::Unary | StreamingShape::ClientStreaming => {
            if method == "/heddle.api.v1alpha2.EndpointService/DescribeEndpoint" {
                let response = v2::DescribeEndpointResponse {
                    endpoint: Some(v2::EndpointRef {
                        kind: v2::EndpointKind::Weft as i32,
                        public_key: server_key.clone(),
                    }),
                    supported_packages: vec!["heddle.api.v1alpha2".into()],
                    implemented_methods: vec![
                        "/heddle.api.v1alpha2.WorkspaceService/ResolveResources".into(),
                        "/heddle.api.v1alpha2.SpoolService/ObserveSpool".into(),
                        "/heddle.api.v1alpha2.SpoolService/ListSpools".into(),
                        "/heddle.api.v1alpha2.SpoolService/DeleteSpool".into(),
                        "/heddle.api.v1alpha2.SpoolService/ReviseSpool".into(),
                        "/heddle.api.v1alpha2.SpoolService/PromoteSpool".into(),
                        "/heddle.api.v1alpha2.SpoolService/PutGrant".into(),
                        "/heddle.api.v1alpha2.SpoolService/RevokeGrant".into(),
                        "/heddle.api.v1alpha2.ThreadService/ObserveThread".into(),
                        "/heddle.api.v1alpha2.ThreadService/ObserveThreads".into(),
                        "/heddle.api.v1alpha2.ThreadService/RecordReview".into(),
                        "/heddle.api.v1alpha2.IdentityService/ObserveIdentity".into(),
                        "/heddle.api.v1alpha2.IdentityService/CreateSignupInvitation".into(),
                        "/heddle.api.v1alpha2.WorkspaceService/ObserveWorkspace".into(),
                        "/heddle.api.v1alpha2.OwnerAuthorizationService/ObserveOwnership".into(),
                        "/heddle.api.v1alpha2.SpoolService/CreateSpool".into(),
                        "/heddle.api.v1alpha2.CollaborationService/ObserveCollaboration".into(),
                        "/heddle.api.v1alpha2.CollaborationService/OpenDiscussion".into(),
                        "/heddle.api.v1alpha2.CollaborationService/AppendTurn".into(),
                        "/heddle.api.v1alpha2.CollaborationService/ResolveDiscussion".into(),
                        "/heddle.api.v1alpha2.CollaborationService/PutContext".into(),
                    ],
                    default_read_budget: Some(v2::ReadBudget {
                        max_items: 64,
                        max_frame_bytes: 65536,
                        max_snapshot_bytes: 1048576,
                    }),
                    max_pending_batch_bytes: 1048576,
                    ..Default::default()
                };
                send.write_chunk(Bytes::from(
                    encode_success_response(&response.encode_to_vec()).unwrap(),
                ))
                .await
                .unwrap();
            } else if method == "/heddle.api.v1alpha2.WorkspaceService/ResolveResources" {
                while let Ok(Some(chunk)) =
                    recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await
                {
                    request.extend_from_slice(&chunk);
                }
                let body = decode_request_frame(&request)
                    .ok()
                    .and_then(|frame| v2::ResolveResourcesRequest::decode(frame.body).ok())
                    .expect("native resource selectors");
                let scoped_handle = body.selectors.len() == 2
                    && matches!(
                        body.selectors[1].selector,
                        Some(v2::resource_selector::Selector::PrincipalHandle(_))
                    );
                let spool = v2::SpoolRef {
                    id: uuid::Uuid::from_bytes([2; 16]).to_string(),
                };
                let thread_name = (body.selectors.len() == 1)
                    .then(|| body.selectors[0].selector.as_ref())
                    .flatten()
                    .and_then(|selector| match selector {
                        v2::resource_selector::Selector::ThreadName(value) => {
                            Some(value.name.as_str())
                        }
                        _ => None,
                    });
                let mut results = vec![v2::ResourceResolution {
                    resource: Some(v2::EntityRef {
                        entity: Some(if let Some(name) = thread_name {
                            v2::entity_ref::Entity::Thread(v2::ThreadRef {
                                spool: Some(spool.clone()),
                                id: Some(v2::ThreadId {
                                    value: vec![if name == "main" { 4 } else { 3 }; 32],
                                }),
                            })
                        } else {
                            v2::entity_ref::Entity::Spool(spool)
                        }),
                    }),
                    coverage: v2::Coverage::Complete as i32,
                    ..Default::default()
                }];
                if scoped_handle {
                    results.push(v2::ResourceResolution {
                        selection_index: 1,
                        principal_id: uuid::Uuid::from_bytes([4; 16]).to_string(),
                        coverage: v2::Coverage::Complete as i32,
                        ..Default::default()
                    });
                }
                let response = v2::ResolveResourcesResponse { results };
                send.write_chunk(Bytes::from(
                    encode_success_response(&response.encode_to_vec()).unwrap(),
                ))
                .await
                .unwrap();
            } else if method == "/heddle.api.v1alpha2.SpoolService/ListSpools" {
                serve_native_list_spools(&mut send, &mut recv, &mut request).await;
            } else if method == "/heddle.api.v1alpha2.SpoolService/DeleteSpool" {
                serve_native_delete_spool(
                    &mut send,
                    &mut recv,
                    &mut request,
                    spool_mutations,
                    server_key,
                )
                .await;
            } else if method == "/heddle.api.v1alpha2.SpoolService/ReviseSpool" {
                serve_native_revise_spool(
                    &mut send,
                    &mut recv,
                    &mut request,
                    spool_mutations,
                    server_key,
                )
                .await;
            } else if method == "/heddle.api.v1alpha2.SpoolService/PromoteSpool" {
                serve_native_promote_spool(&mut send, &mut recv, &mut request, server_key).await;
            } else if method == "/heddle.api.v1alpha2.SpoolService/PutGrant" {
                serve_native_put_grant(&mut send, &mut recv, &mut request, server_key, grants)
                    .await;
            } else if method == "/heddle.api.v1alpha2.SpoolService/RevokeGrant" {
                serve_native_revoke_grant(&mut send, &mut recv, &mut request, server_key, grants)
                    .await;
            } else if method == "/heddle.api.v1alpha2.ThreadService/RecordReview" {
                serve_native_record_review(&mut send, &mut recv, &mut request, server_key).await;
            } else if method == "/heddle.api.v1alpha2.SpoolService/CreateSpool" {
                serve_native_create_spool(
                    &mut send,
                    &mut recv,
                    &mut request,
                    server_key,
                    owner,
                    create_spool,
                )
                .await;
            } else if method == "/heddle.api.v1alpha2.CollaborationService/OpenDiscussion" {
                serve_open_discussion(
                    &mut send,
                    &mut recv,
                    &mut request,
                    server_key,
                    live_discussions,
                    live_operations,
                )
                .await;
            } else if method == "/heddle.api.v1alpha2.CollaborationService/AppendTurn" {
                serve_append_turn(
                    &mut send,
                    &mut recv,
                    &mut request,
                    server_key,
                    live_discussions,
                    live_operations,
                )
                .await;
            } else if method == "/heddle.api.v1alpha2.CollaborationService/ResolveDiscussion" {
                serve_resolve_discussion(
                    &mut send,
                    &mut recv,
                    &mut request,
                    server_key,
                    live_discussions,
                    live_operations,
                )
                .await;
            } else if method == "/heddle.api.v1alpha2.CollaborationService/PutContext" {
                serve_put_context(
                    &mut send,
                    &mut recv,
                    &mut request,
                    server_key,
                    context.clone(),
                )
                .await;
            } else if method == "/heddle.api.v1alpha2.IdentityService/CreateSignupInvitation" {
                serve_native_create_signup_invitation(
                    &mut send,
                    &mut recv,
                    &mut request,
                    server_key,
                )
                .await;
            } else {
                send.write_chunk(Bytes::from(encode_success_response(&[]).unwrap()))
                    .await
                    .unwrap();
            }
        }
        StreamingShape::ServerStreaming => {
            if method == "/heddle.api.v1alpha2.ThreadService/ObserveThread" {
                serve_native_thread_review(&mut send, &mut recv, &mut request, server_key).await;
            } else if method == "/heddle.api.v1alpha2.SpoolService/ObserveSpool" {
                serve_native_spool_observation(
                    &mut send,
                    &mut recv,
                    &mut request,
                    server_key,
                    grants,
                )
                .await;
            } else if method == "/heddle.api.v1alpha2.IdentityService/ObserveIdentity" {
                serve_native_identity_observation(&mut send, &mut recv, &mut request, server_key)
                    .await;
            } else if method == "/heddle.api.v1alpha2.WorkspaceService/ObserveWorkspace" {
                serve_native_workspace_observation(&mut send, server_key).await;
            } else if method == "/heddle.api.v1alpha2.OwnerAuthorizationService/ObserveOwnership" {
                serve_native_owner_observation(&mut send, server_key, owner).await;
            } else if method == "/heddle.api.v1alpha2.ThreadService/ObserveThreads" {
                serve_observe_threads(&mut send, server_key.clone()).await;
            } else if method == OBSERVE_COLLABORATION_METHOD {
                serve_observe_collaboration(
                    &mut send,
                    &mut recv,
                    &mut request,
                    server_key.clone(),
                    ObserveCollaborationLive {
                        collaboration,
                        context,
                        discussions: live_discussions,
                        operations: live_operations,
                    },
                )
                .await;
            } else {
                let body = terminal_page(&method);
                send.write_chunk(Bytes::from(encode_stream_message(&body).unwrap()))
                    .await
                    .unwrap();
            }
        }
        StreamingShape::Bidirectional => {
            if method == "/heddle.api.v1alpha2.SyncService/PublishContent" {
                serve_push(send, recv, request.split_off(prelude_len), push_requests).await;
                return;
            }
            tokio::spawn(async move {
                while recv
                    .read_chunk(api::framing::MAX_CONTROL_BODY + 5)
                    .await
                    .is_ok_and(|chunk| chunk.is_some())
                {}
            });
            for body in bidi_responses(&method, pull) {
                send.write_chunk(Bytes::from(encode_stream_message(&body).unwrap()))
                    .await
                    .unwrap();
            }
        }
    }
    send.finish().unwrap();
}

async fn serve_native_list_spools(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::ListSpoolsRequest::decode(frame.body).ok())
        .expect("native ListSpools request");
    let mut spools = vec![
        v2::ListedSpool {
            r#ref: Some(v2::SpoolRef {
                id: uuid::Uuid::from_bytes([2; 16]).to_string(),
            }),
            path_segments: vec!["spool".into(), "acme".into()],
            is_repo: false,
            ..Default::default()
        },
        v2::ListedSpool {
            r#ref: Some(v2::SpoolRef {
                id: uuid::Uuid::from_bytes([3; 16]).to_string(),
            }),
            path_segments: vec!["spool".into(), "acme".into(), "notes".into()],
            is_repo: true,
            ..Default::default()
        },
    ];
    if body.repos_only {
        spools.retain(|spool| spool.is_repo);
    }
    let response = v2::ListSpoolsResponse { spools };
    send.write_chunk(Bytes::from(
        encode_success_response(&response.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_native_workspace_observation(
    send: &mut iroh::endpoint::SendStream,
    server_key: Vec<u8>,
) {
    let source = v2::EndpointRef {
        kind: v2::EndpointKind::Weft as i32,
        public_key: server_key,
    };
    let budget = v2::ReadBudget {
        max_items: 64,
        max_frame_bytes: 65536,
        max_snapshot_bytes: 1048576,
    };
    let events = [
        v2::WorkspaceEvent {
            frame: Some(v2::StreamFrame {
                sequence: 1,
                body: Some(v2::stream_frame::Body::Open(v2::StreamOpen {
                    source: Some(source),
                    binding_digest: vec![8; 32],
                    accepted_budget: Some(budget),
                    ..Default::default()
                })),
            }),
            ..Default::default()
        },
        v2::WorkspaceEvent {
            frame: Some(v2::StreamFrame {
                sequence: 2,
                body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                    kind: v2::StreamDataKind::Snapshot as i32,
                })),
            }),
            payload: Some(v2::workspace_event::Payload::Status(v2::SectionStatus {
                section: "spools".into(),
                coverage: v2::Coverage::Complete as i32,
                page: Some(v2::PageInfo {
                    exhausted: true,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        },
        v2::WorkspaceEvent {
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
    ];
    for event in events {
        send.write_chunk(Bytes::from(
            encode_stream_message(&event.encode_to_vec()).unwrap(),
        ))
        .await
        .unwrap();
    }
}

async fn serve_native_owner_observation(
    send: &mut iroh::endpoint::SendStream,
    server_key: Vec<u8>,
    owner: v2::OwnerState,
) {
    let source = v2::EndpointRef {
        kind: v2::EndpointKind::Weft as i32,
        public_key: server_key,
    };
    let budget = v2::ReadBudget {
        max_items: 64,
        max_frame_bytes: 65536,
        max_snapshot_bytes: 1048576,
    };
    let events = [
        v2::OwnershipEvent {
            frame: Some(v2::StreamFrame {
                sequence: 1,
                body: Some(v2::stream_frame::Body::Open(v2::StreamOpen {
                    source: Some(source),
                    binding_digest: vec![8; 32],
                    accepted_budget: Some(budget),
                    ..Default::default()
                })),
            }),
            ..Default::default()
        },
        v2::OwnershipEvent {
            frame: Some(v2::StreamFrame {
                sequence: 2,
                body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                    kind: v2::StreamDataKind::Snapshot as i32,
                })),
            }),
            payload: Some(v2::ownership_event::Payload::Owner(owner)),
        },
        v2::OwnershipEvent {
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
    ];
    for event in events {
        send.write_chunk(Bytes::from(
            encode_stream_message(&event.encode_to_vec()).unwrap(),
        ))
        .await
        .unwrap();
    }
}

async fn serve_native_create_spool(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
    owner: v2::OwnerState,
    captured: Option<Arc<Mutex<Vec<v2::CreateSpoolRequest>>>>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::CreateSpoolRequest::decode(frame.body).ok())
        .expect("native create Spool request");
    if let Some(captured) = captured {
        captured
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(body.clone());
    }
    let genesis = match body.ownership.expect("creation ownership") {
        v2::create_spool_request::Ownership::OwnerGenesis(genesis) => genesis,
        v2::create_spool_request::Ownership::CustodialSpool(_) => {
            panic!("native test server expects owner genesis creation")
        }
    };
    let spool_uuid = genesis
        .genesis
        .as_ref()
        .expect("genesis")
        .spool_uuid
        .clone();
    let response = v2::SpoolMutationResponse {
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
        spool: Some(v2::SpoolOverview {
            r#ref: Some(v2::SpoolRef {
                id: uuid::Uuid::from_slice(&spool_uuid)
                    .expect("Spool UUID")
                    .to_string(),
            }),
            parent: body.parent,
            slug: body.slug.clone(),
            name: body.display_name.unwrap_or(body.slug),
            owner_genesis: Some(genesis),
            version: vec![7; 32],
            ..Default::default()
        }),
        ownership: Some(owner),
    };
    send.write_chunk(Bytes::from(
        encode_success_response(&response.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_native_create_signup_invitation(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::CreateSignupInvitationRequest::decode(frame.body).ok())
        .expect("native signup invitation request");
    let response = v2::CreateSignupInvitationResponse {
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
        invitation: Some(v2::SignupInvitation {
            r#ref: Some(v2::RecordRef {
                id: uuid::Uuid::from_bytes([11; 16]).to_string(),
                ..Default::default()
            }),
            bound_email: body
                .invitation
                .map(|invitation| invitation.bound_email)
                .unwrap_or_default(),
            ..Default::default()
        }),
        redemption_secret: b"one-time-invite".to_vec(),
    };
    send.write_chunk(Bytes::from(
        encode_success_response(&response.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_native_identity_observation(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::ObserveIdentityRequest::decode(frame.body).ok())
        .expect("native identity observation request");
    let signup_requested = body.signup_invitations.is_some();
    let credential_requested = body.include_current_credential;
    let source = v2::EndpointRef {
        kind: v2::EndpointKind::Weft as i32,
        public_key: server_key,
    };
    let budget = v2::ReadBudget {
        max_items: 64,
        max_frame_bytes: 65536,
        max_snapshot_bytes: 1048576,
    };
    let mut events = vec![
        v2::IdentityEvent {
            frame: Some(v2::StreamFrame {
                sequence: 1,
                body: Some(v2::stream_frame::Body::Open(v2::StreamOpen {
                    source: Some(source),
                    binding_digest: vec![8; 32],
                    accepted_budget: Some(budget),
                    ..Default::default()
                })),
            }),
            ..Default::default()
        },
        v2::IdentityEvent {
            frame: Some(v2::StreamFrame {
                sequence: 2,
                body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                    kind: v2::StreamDataKind::Snapshot as i32,
                })),
            }),
            payload: Some(v2::identity_event::Payload::Identity(v2::PrincipalRecord {
                id: uuid::Uuid::from_bytes([9; 16]).to_string(),
                account_id: uuid::Uuid::from_bytes([9; 16]).to_string(),
                acting_agent_id: "reviewer-1".into(),
                rooting_tier: v2::RootingTier::SelfRooted as i32,
                personal_spool: Some(v2::SpoolAddress {
                    r#ref: Some(v2::SpoolRef {
                        id: uuid::Uuid::from_bytes([2; 16]).to_string(),
                    }),
                    path_segments: vec!["acme".into()],
                }),
                ..Default::default()
            })),
        },
        v2::IdentityEvent {
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
    ];
    if signup_requested {
        events.insert(
            2,
            v2::IdentityEvent {
                frame: Some(v2::StreamFrame {
                    sequence: 3,
                    body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                        kind: v2::StreamDataKind::Snapshot as i32,
                    })),
                }),
                payload: Some(v2::identity_event::Payload::InvitationQuota(
                    v2::InvitationQuota {
                        remaining: 2,
                        ..Default::default()
                    },
                )),
            },
        );
        events.insert(
            3,
            v2::IdentityEvent {
                frame: Some(v2::StreamFrame {
                    sequence: 4,
                    body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                        kind: v2::StreamDataKind::Snapshot as i32,
                    })),
                }),
                payload: Some(v2::identity_event::Payload::Status(v2::SectionStatus {
                    section: "signup_invitations".into(),
                    coverage: v2::Coverage::Complete as i32,
                    page: Some(v2::PageInfo {
                        exhausted: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                })),
            },
        );
    }
    if credential_requested {
        events.insert(
            2,
            v2::IdentityEvent {
                frame: Some(v2::StreamFrame {
                    sequence: 0,
                    body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                        kind: v2::StreamDataKind::Snapshot as i32,
                    })),
                }),
                payload: Some(v2::identity_event::Payload::CurrentCredential(
                    v2::CurrentCredentialRecord {
                        r#ref: Some(v2::RecordRef {
                            id: uuid::Uuid::from_bytes([12; 16]).to_string(),
                            ..Default::default()
                        }),
                        kind: v2::CredentialKind::Agent as i32,
                        subject: "agent:reviewer-1".into(),
                        acting_agent_id: "reviewer-1".into(),
                        agent_provider: "codex".into(),
                        agent_model: "gpt".into(),
                        session: Some(v2::SessionRecord {
                            r#ref: Some(v2::RecordRef {
                                id: uuid::Uuid::from_bytes([13; 16]).to_string(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
            },
        );
    }
    for (index, event) in events.iter_mut().enumerate() {
        event.frame.as_mut().expect("identity event frame").sequence = (index + 1) as u64;
    }
    for event in events {
        send.write_chunk(Bytes::from(
            encode_stream_message(&event.encode_to_vec()).unwrap(),
        ))
        .await
        .unwrap();
    }
}

async fn serve_native_thread_review(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::ObserveThreadRequest::decode(frame.body).ok())
        .expect("native review observation request");
    assert!(body.sections.contains(&(v2::ThreadSection::Review as i32)));
    let landing_target = body.landing_target.clone();
    let thread = body.thread.expect("review Thread identity");
    let revision = v2::RevisionRef {
        spool: thread.spool.clone(),
        revision: Some(v2::revision_ref::Revision::State(
            api::heddle::api::common::StateId { value: vec![5; 32] },
        )),
    };
    let data = |sequence, payload| v2::ThreadEvent {
        frame: Some(v2::StreamFrame {
            sequence,
            body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                kind: v2::StreamDataKind::Snapshot as i32,
            })),
        }),
        payload: Some(payload),
    };
    let events = [
        v2::ThreadEvent {
            frame: Some(v2::StreamFrame {
                sequence: 1,
                body: Some(v2::stream_frame::Body::Open(v2::StreamOpen {
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
                })),
            }),
            ..Default::default()
        },
        data(
            2,
            v2::thread_event::Payload::Comparison(v2::ReviewComparison {
                source: Some(revision.clone()),
                base: Some(revision.clone()),
                policy_version: vec![6; 32],
            }),
        ),
        data(
            3,
            v2::thread_event::Payload::Overview(v2::ThreadOverview {
                r#ref: Some(thread),
                name: "feature".into(),
                version: vec![7; 32],
                review_policy_version: vec![6; 32],
                source_heads: vec![revision.clone()],
                base: Some(revision.clone()),
                readiness: v2::ReviewReadiness::Unknown as i32,
                landing_assessment: landing_target.map(|target| v2::LandingAssessment {
                    target: Some(target),
                    source: Some(revision.clone()),
                    expected_target: Some(revision.clone()),
                    policy_version: vec![6; 32],
                    readiness: v2::ReviewReadiness::Eligible as i32,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        ),
        data(
            4,
            v2::thread_event::Payload::Status(v2::SectionStatus {
                section: "review".into(),
                coverage: v2::Coverage::Complete as i32,
                page: Some(v2::PageInfo {
                    exhausted: true,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        ),
        v2::ThreadEvent {
            frame: Some(v2::StreamFrame {
                sequence: 5,
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
    ];
    for event in events {
        send.write_chunk(Bytes::from(
            encode_stream_message(&event.encode_to_vec()).unwrap(),
        ))
        .await
        .unwrap();
    }
}

async fn serve_native_record_review(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::RecordReviewRequest::decode(frame.body).ok())
        .expect("native signed review request");
    let operation = body.operation.as_ref().expect("original review signature");
    thread_api::thread_control::verify(operation).expect("original signature verifies");
    assert!(body.decision.is_some());
    write_native_grant_receipt(send, server_key, body.client_operation_id).await;
}

async fn serve_native_spool_observation(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
    grants: Arc<Mutex<Vec<v2::GrantRecord>>>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let request_body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::ObserveSpoolRequest::decode(frame.body).ok())
        .expect("native Spool observation request");
    let grants_requested = request_body
        .sections
        .contains(&(v2::SpoolSection::Grants as i32));
    let source = v2::EndpointRef {
        kind: v2::EndpointKind::Weft as i32,
        public_key: server_key,
    };
    let budget = v2::ReadBudget {
        max_items: 64,
        max_frame_bytes: 65536,
        max_snapshot_bytes: 1048576,
    };
    let mut events = vec![
        v2::SpoolEvent {
            frame: Some(v2::StreamFrame {
                sequence: 1,
                body: Some(v2::stream_frame::Body::Open(v2::StreamOpen {
                    source: Some(source),
                    binding_digest: vec![9; 32],
                    accepted_budget: Some(budget),
                    ..Default::default()
                })),
            }),
            ..Default::default()
        },
        v2::SpoolEvent {
            frame: Some(v2::StreamFrame {
                sequence: 2,
                body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                    kind: v2::StreamDataKind::Snapshot as i32,
                })),
            }),
            payload: Some(v2::spool_event::Payload::Spool(v2::SpoolOverview {
                r#ref: Some(v2::SpoolRef {
                    id: uuid::Uuid::from_bytes([2; 16]).to_string(),
                }),
                version: vec![7; 32],
                slug: "acme".into(),
                path_segments: vec!["acme".into()],
                settings: Some(v2::SpoolSettings::default()),
                ..Default::default()
            })),
        },
        v2::SpoolEvent {
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
    ];
    if grants_requested {
        events.truncate(1);
        let records = grants
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let mut sequence = 2;
        for grant in records {
            events.push(v2::SpoolEvent {
                frame: Some(v2::StreamFrame {
                    sequence,
                    body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                        kind: v2::StreamDataKind::Snapshot as i32,
                    })),
                }),
                payload: Some(v2::spool_event::Payload::Grant(grant)),
            });
            sequence += 1;
        }
        events.push(v2::SpoolEvent {
            frame: Some(v2::StreamFrame {
                sequence,
                body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                    kind: v2::StreamDataKind::Snapshot as i32,
                })),
            }),
            payload: Some(v2::spool_event::Payload::Status(v2::SectionStatus {
                section: "grants".into(),
                coverage: v2::Coverage::Complete as i32,
                page: Some(v2::PageInfo {
                    exhausted: true,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        });
        events.push(v2::SpoolEvent {
            frame: Some(v2::StreamFrame {
                sequence: sequence + 1,
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
        });
    }
    for event in events {
        send.write_chunk(Bytes::from(
            encode_stream_message(&event.encode_to_vec()).unwrap(),
        ))
        .await
        .unwrap();
    }
}

async fn serve_native_revise_spool(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    captured: Option<Arc<Mutex<SpoolMutationCapture>>>,
    server_key: Vec<u8>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::ReviseSpoolRequest::decode(frame.body).ok())
        .expect("native Spool revision request");
    if let Some(captured) = captured {
        captured
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .native_updates
            .push(body.clone());
    }
    let slug = body.slug.clone().unwrap_or_else(|| "acme".into());
    let response = v2::SpoolMutationResponse {
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
        spool: Some(v2::SpoolOverview {
            r#ref: body.spool,
            version: vec![8; 32],
            slug: slug.clone(),
            path_segments: vec![slug],
            name: body.name,
            settings: body.settings,
            ..Default::default()
        }),
        ..Default::default()
    };
    send.write_chunk(Bytes::from(
        encode_success_response(&response.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_native_promote_spool(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::PromoteSpoolRequest::decode(frame.body).ok())
        .expect("native Spool promotion request");
    let response = v2::SpoolMutationResponse {
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
        spool: Some(v2::SpoolOverview {
            r#ref: body.spool,
            version: vec![8; 32],
            slug: "acme".into(),
            path_segments: vec!["acme".into()],
            ..Default::default()
        }),
        ..Default::default()
    };
    send.write_chunk(Bytes::from(
        encode_success_response(&response.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_native_put_grant(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
    grants: Arc<Mutex<Vec<v2::GrantRecord>>>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::PutGrantRequest::decode(frame.body).ok())
        .expect("native PutGrant request");
    let mut grant = body.grant.expect("grant record");
    {
        let mut records = grants.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(existing) = records.iter_mut().find(|row| row.r#ref == grant.r#ref) {
            assert_eq!(body.expected_version, existing.version);
            grant.version = vec![2; 32];
            *existing = grant;
        } else {
            assert!(body.expected_version.is_empty());
            grant.version = vec![1; 32];
            records.push(grant);
        }
    }
    write_native_grant_receipt(send, server_key, body.client_operation_id).await;
}

async fn serve_native_revoke_grant(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
    grants: Arc<Mutex<Vec<v2::GrantRecord>>>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::RevokeGrantRequest::decode(frame.body).ok())
        .expect("native RevokeGrant request");
    grants
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .retain(|grant| grant.r#ref != body.grant);
    write_native_grant_receipt(send, server_key, body.client_operation_id).await;
}

async fn write_native_grant_receipt(
    send: &mut iroh::endpoint::SendStream,
    server_key: Vec<u8>,
    operation_id: String,
) {
    let response = v2::MutationResponse {
        receipt: Some(v2::MutationReceipt {
            client_operation_id: operation_id,
            endpoint: Some(v2::EndpointRef {
                kind: v2::EndpointKind::Weft as i32,
                public_key: server_key,
            }),
            outcome: Some(v2::mutation_receipt::Outcome::Applied(
                v2::Applied::default(),
            )),
            ..Default::default()
        }),
    };
    send.write_chunk(Bytes::from(
        encode_success_response(&response.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_native_delete_spool(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    captured: Option<Arc<Mutex<SpoolMutationCapture>>>,
    server_key: Vec<u8>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::DeleteSpoolRequest::decode(frame.body).ok());
    if let (Some(captured), Some(body)) = (captured.as_ref(), body.as_ref()) {
        captured
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .native_deletes
            .push(body.clone());
    }
    let response = v2::MutationResponse {
        receipt: body.map(|body| v2::MutationReceipt {
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
    };
    send.write_chunk(Bytes::from(
        encode_success_response(&response.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_push(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    mut buffered: Vec<u8>,
    captured: Option<Arc<Mutex<Vec<PushRequest>>>>,
) {
    let request = loop {
        if let Some((frame, consumed)) = decode_stream_frame(&buffered).unwrap() {
            let request = match frame {
                StreamFrame::Message(body) => PushClientFrame::decode(body).unwrap(),
                other => panic!("unexpected push request frame before request: {other:?}"),
            };
            buffered.drain(..consumed);
            if let Some(push_client_frame::Frame::Request(request)) = request.frame {
                break *request;
            }
            continue;
        }
        let chunk = recv
            .read_chunk(api::framing::MAX_CONTROL_BODY + 5)
            .await
            .unwrap()
            .expect("push request frame");
        buffered.extend_from_slice(&chunk);
    };
    let advertised = request.objects.clone();
    if let Some(captured) = captured {
        captured
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(request);
    }

    let ready = PushServerFrame {
        frame: Some(push_server_frame::Frame::Ready(PushReady {
            want_objects: advertised,
        })),
    }
    .encode_to_vec();
    send.write_chunk(Bytes::from(encode_stream_message(&ready).unwrap()))
        .await
        .unwrap();

    while recv
        .read_chunk(api::framing::MAX_CONTROL_BODY + 5)
        .await
        .is_ok_and(|chunk| chunk.is_some())
    {}
    let complete = PushServerFrame {
        frame: Some(push_server_frame::Frame::Complete(PushComplete {
            success: false,
            error: "test rejection".to_string(),
        })),
    }
    .encode_to_vec();
    send.write_chunk(Bytes::from(encode_stream_message(&complete).unwrap()))
        .await
        .unwrap();
    send.finish().unwrap();
}

async fn serve_observe_threads(send: &mut iroh::endpoint::SendStream, server_key: Vec<u8>) {
    let spool = v2::SpoolRef {
        id: uuid::Uuid::from_bytes([2; 16]).to_string(),
    };
    let overviews = ["main", "refs/heads/feature/run"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| v2::ThreadOverview {
            name: name.into(),
            r#ref: Some(v2::ThreadRef {
                spool: Some(spool.clone()),
                id: Some(v2::ThreadId {
                    value: vec![if index == 0 { 4 } else { 3 }; 32],
                }),
            }),
            ..Default::default()
        });
    let payloads = overviews
        .map(v2::thread_list_event::Payload::Thread)
        .collect();
    write_thread_list_observation(send, server_key, payloads).await;
}

async fn write_thread_list_observation(
    send: &mut iroh::endpoint::SendStream,
    server_key: Vec<u8>,
    payloads: Vec<v2::thread_list_event::Payload>,
) {
    let source = v2::EndpointRef {
        kind: v2::EndpointKind::Weft as i32,
        public_key: server_key,
    };
    let budget = v2::ReadBudget {
        max_items: 64,
        max_frame_bytes: 65536,
        max_snapshot_bytes: 1048576,
    };
    let mut sequence = 1u64;
    let open = v2::ThreadListEvent {
        frame: Some(v2::StreamFrame {
            sequence,
            body: Some(v2::stream_frame::Body::Open(v2::StreamOpen {
                source: Some(source),
                binding_digest: vec![9; 32],
                accepted_budget: Some(budget),
                ..Default::default()
            })),
        }),
        ..Default::default()
    };
    send.write_chunk(Bytes::from(
        encode_stream_message(&open.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
    for payload in payloads {
        sequence += 1;
        let event = v2::ThreadListEvent {
            frame: Some(v2::StreamFrame {
                sequence,
                body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                    kind: v2::StreamDataKind::Snapshot as i32,
                })),
            }),
            payload: Some(payload),
        };
        send.write_chunk(Bytes::from(
            encode_stream_message(&event.encode_to_vec()).unwrap(),
        ))
        .await
        .unwrap();
    }
    sequence += 1;
    let checkpoint = v2::ThreadListEvent {
        frame: Some(v2::StreamFrame {
            sequence,
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
    };
    send.write_chunk(Bytes::from(
        encode_stream_message(&checkpoint.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
    sequence += 1;
    let complete = v2::ThreadListEvent {
        frame: Some(v2::StreamFrame {
            sequence,
            body: Some(v2::stream_frame::Body::Complete(v2::StreamComplete {
                cursor: vec![1],
            })),
        }),
        ..Default::default()
    };
    send.write_chunk(Bytes::from(
        encode_stream_message(&complete.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

fn remember_signed_operation(
    operations: &Arc<Mutex<HashMap<String, Vec<v2::SignedRecord>>>>,
    discussion_id: &str,
    signed: Option<v2::SignedRecord>,
) {
    let Some(signed) = signed.filter(|record| !record.canonical_record.is_empty()) else {
        return;
    };
    operations
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .entry(discussion_id.to_string())
        .or_default()
        .push(signed);
}

fn signed_operation_id(record: &v2::SignedRecord) -> Option<Vec<u8>> {
    thread_api::collaboration::verify(record)
        .ok()
        .and_then(|operation| operation.id().ok())
        .map(|id| id.as_bytes().to_vec())
}

async fn serve_open_discussion(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
    live: Arc<Mutex<HashMap<String, Discussion>>>,
    operations: Arc<Mutex<HashMap<String, Vec<v2::SignedRecord>>>>,
) {
    read_request_body(recv, request).await;
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::OpenDiscussionRequest::decode(frame.body).ok())
        .unwrap_or_default();
    if let Some(discussion) = discussion_from_open(&body) {
        remember_signed_operation(&operations, &discussion.id, body.signed_operation.clone());
        live.lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(discussion.id.clone(), discussion);
    }
    write_native_grant_receipt(send, server_key, body.client_operation_id).await;
}

async fn serve_append_turn(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
    live: Arc<Mutex<HashMap<String, Discussion>>>,
    operations: Arc<Mutex<HashMap<String, Vec<v2::SignedRecord>>>>,
) {
    read_request_body(recv, request).await;
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::AppendDiscussionRequest::decode(frame.body).ok())
        .unwrap_or_default();
    if let Some(id) = body
        .discussion
        .as_ref()
        .map(|reference| reference.id.clone())
    {
        remember_signed_operation(&operations, &id, body.signed_operation.clone());
        let mut live = live.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(discussion) = live.get_mut(&id) {
            let seq = discussion.turns.len() as u64 + 1;
            discussion.turns.push(DiscussionTurn {
                body: body.body.clone(),
                turn_id: format!("turn-{seq}"),
                turn_seq: seq,
                ..Default::default()
            });
        }
    }
    write_native_grant_receipt(send, server_key, body.client_operation_id).await;
}

async fn serve_resolve_discussion(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
    live: Arc<Mutex<HashMap<String, Discussion>>>,
    operations: Arc<Mutex<HashMap<String, Vec<v2::SignedRecord>>>>,
) {
    read_request_body(recv, request).await;
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::ResolveDiscussionRequest::decode(frame.body).ok())
        .unwrap_or_default();
    if let Some(id) = body
        .discussion
        .as_ref()
        .map(|reference| reference.id.clone())
    {
        let mut live = live.lock().unwrap_or_else(|poison| poison.into_inner());
        remember_signed_operation(&operations, &id, body.signed_operation.clone());
        if let Some(discussion) = live.get_mut(&id) {
            discussion.resolution = Some(DiscussionResolution {
                state: Some(discussion_resolution::State::Dismissed(
                    discussion_resolution::Dismissed {
                        reason: "resolved".into(),
                    },
                )),
            });
        }
    }
    write_native_grant_receipt(send, server_key, body.client_operation_id).await;
}

async fn serve_put_context(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
    context: Option<ContextFixture>,
) {
    read_request_body(recv, request).await;
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::PutContextRequest::decode(frame.body).ok())
        .unwrap_or_default();
    if let Some(fixture) = &context {
        *fixture
            .put_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) += 1;
        if fixture.put_conflict {
            let failure = CallFailure {
                code: CallFailureCode::FailedPrecondition as i32,
                message: "operation ID names another command".to_string(),
                error: None,
            };
            send.write_chunk(Bytes::from(encode_failure_response(&failure).unwrap()))
                .await
                .unwrap();
            return;
        }
    }
    write_native_grant_receipt(send, server_key, body.client_operation_id).await;
}

fn discussion_from_open(request: &v2::OpenDiscussionRequest) -> Option<Discussion> {
    let signed = request.signed_operation.as_ref()?;
    if signed.canonical_record.is_empty() {
        return None;
    }
    let operation = thread_api::collaboration::verify(signed).ok().or_else(|| {
        objects::object::thread_replication::ThreadOperation::decode(&signed.canonical_record).ok()
    })?;
    let objects::object::thread_replication::ThreadOperationBody::Discussion(bytes) =
        operation.body
    else {
        return None;
    };
    let record = objects::object::CollaborationOperationEnvelope::decode(&bytes)
        .ok()?
        .operation;
    let objects::object::CollaborationOperationBodyV1::Open {
        title: _,
        anchor,
        visibility,
        turn,
        thread_ref,
        ..
    } = record.body
    else {
        return None;
    };
    let (file, symbol) = request
        .anchor
        .as_ref()
        .and_then(|anchor| match &anchor.target {
            Some(v2::collaboration_anchor::Target::Source(source)) => {
                Some((source.path.clone(), source.symbol_id.clone()))
            }
            _ => None,
        })
        .unwrap_or_else(|| match anchor {
            objects::object::CollaborationAnchor::Symbol { path, symbol, .. } => (path, symbol),
            objects::object::CollaborationAnchor::Path { path, .. } => (path, String::new()),
            objects::object::CollaborationAnchor::Source { source } => {
                (source.path, source.symbol_id)
            }
            _ => (String::new(), String::new()),
        });
    Some(Discussion {
        id: record.discussion_id.to_string(),
        anchor: Some(PathSymbolRef { file, symbol }),
        visibility: match visibility {
            objects::object::VisibilityTier::Public => "public".into(),
            objects::object::VisibilityTier::Private { .. } => "private".into(),
            _ => "internal".into(),
        },
        thread_ref: thread_ref.unwrap_or_default(),
        turns: vec![DiscussionTurn {
            body: turn.body,
            turn_id: "turn-open".into(),
            turn_seq: 1,
            ..Default::default()
        }],
        ..Default::default()
    })
}

struct ObserveCollaborationLive {
    collaboration: Option<CollaborationFixture>,
    context: Option<ContextFixture>,
    discussions: Arc<Mutex<HashMap<String, Discussion>>>,
    operations: Arc<Mutex<HashMap<String, Vec<v2::SignedRecord>>>>,
}

async fn serve_observe_collaboration(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    server_key: Vec<u8>,
    live: ObserveCollaborationLive,
) {
    read_request_body(recv, request).await;
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| v2::ObserveCollaborationRequest::decode(frame.body).ok())
        .unwrap_or_default();
    if let Some(code) = discussion_failure(&body, live.collaboration.as_ref()) {
        let failure = CallFailure {
            code: code as i32,
            message: "discussion is not visible".to_string(),
            error: None,
        };
        send.write_chunk(Bytes::from(encode_stream_failure(&failure).unwrap()))
            .await
            .unwrap();
        return;
    }
    let payloads = observe_payloads(
        &body,
        live.collaboration.as_ref(),
        live.context.as_ref(),
        &live.discussions,
        &live.operations,
    );
    write_collaboration_observation(send, server_key, payloads).await;
}

fn discussion_failure(
    request: &v2::ObserveCollaborationRequest,
    fixture: Option<&CollaborationFixture>,
) -> Option<CallFailureCode> {
    let fixture = fixture?;
    request.discussions.iter().find_map(|reference| {
        match fixture.hidden.get(&reference.id).copied() {
            Some(
                code @ (CallFailureCode::Unauthenticated
                | CallFailureCode::Internal
                | CallFailureCode::Unavailable),
            ) => Some(code),
            _ => None,
        }
    })
}

fn observe_payloads(
    request: &v2::ObserveCollaborationRequest,
    collaboration: Option<&CollaborationFixture>,
    context: Option<&ContextFixture>,
    live: &Arc<Mutex<HashMap<String, Discussion>>>,
    operations: &Arc<Mutex<HashMap<String, Vec<v2::SignedRecord>>>>,
) -> Vec<v2::collaboration_event::Payload> {
    if !request.discussions.is_empty() {
        if let Some(fixture) = collaboration {
            record_get_requests(fixture, request);
        }
        let live = live.lock().unwrap_or_else(|poison| poison.into_inner());
        let operations = operations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        return request
            .discussions
            .iter()
            .filter_map(|reference| {
                collaboration
                    .and_then(|fixture| fixture.discussions.get(&reference.id))
                    .cloned()
                    .or_else(|| live.get(&reference.id).cloned())
                    .map(|discussion| {
                        (
                            discussion,
                            operations.get(&reference.id).cloned().unwrap_or_default(),
                        )
                    })
            })
            .flat_map(|(discussion, signed)| discussion_payloads(&discussion, &signed))
            .collect();
    }
    if !request.contexts.is_empty() {
        if let Some(fixture) = context {
            fixture
                .history_requests
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .extend(
                    request
                        .contexts
                        .iter()
                        .map(|reference| reference.id.clone()),
                );
            return request
                .contexts
                .iter()
                .flat_map(|reference| context_history_payloads(fixture, &reference.id))
                .collect();
        }
        return Vec::new();
    }
    if request.annotations.is_some() {
        if let Some(fixture) = context {
            *fixture
                .list_requests
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) += 1;
            return context_list_payloads(fixture);
        }
        return Vec::new();
    }
    let live = live.lock().unwrap_or_else(|poison| poison.into_inner());
    let operations = operations
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(fixture) = collaboration {
        *fixture
            .list_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) += 1;
        let mut rows = fixture.list.clone();
        rows.extend(live.values().cloned());
        return rows
            .iter()
            .flat_map(|discussion| {
                let signed = operations.get(&discussion.id).cloned().unwrap_or_default();
                discussion_payloads(discussion, &signed)
            })
            .collect();
    }
    live.iter()
        .flat_map(|(id, discussion)| {
            let signed = operations.get(id).cloned().unwrap_or_default();
            discussion_payloads(discussion, &signed)
        })
        .collect()
}

fn record_get_requests(fixture: &CollaborationFixture, request: &v2::ObserveCollaborationRequest) {
    let mut gets = fixture
        .get_requests
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let mut states = fixture
        .get_request_state_ids
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let state = request.anchors.iter().find_map(anchor_state_bytes);
    for reference in &request.discussions {
        gets.push(reference.id.clone());
        states.push(state.clone());
    }
}

fn anchor_state_bytes(anchor: &v2::CollaborationAnchor) -> Option<Vec<u8>> {
    match anchor.target.as_ref() {
        Some(v2::collaboration_anchor::Target::Source(source)) => match source
            .revision
            .as_ref()
            .and_then(|revision| revision.revision.as_ref())
        {
            Some(v2::revision_ref::Revision::State(id)) => Some(id.value.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn discussion_title(discussion: &Discussion) -> String {
    if !discussion.thread_id.is_empty() {
        format!("{}\x1f{}", discussion.thread_ref, discussion.thread_id)
    } else if !discussion.thread_ref.is_empty() {
        discussion.thread_ref.clone()
    } else {
        dismiss_reason(discussion).unwrap_or_default()
    }
}

fn dismiss_reason(discussion: &Discussion) -> Option<String> {
    match discussion
        .resolution
        .as_ref()
        .and_then(|resolution| resolution.state.as_ref())
    {
        Some(discussion_resolution::State::Dismissed(dismissed)) => Some(dismissed.reason.clone()),
        _ => None,
    }
}

fn discussion_payloads(
    discussion: &Discussion,
    operations: &[v2::SignedRecord],
) -> Vec<v2::collaboration_event::Payload> {
    let id = discussion.id.clone();
    let (path, symbol) = discussion
        .anchor
        .as_ref()
        .map(|anchor| (anchor.file.clone(), anchor.symbol.clone()))
        .unwrap_or_default();
    let status = if discussion.resolution.is_some() {
        v2::discussion_record::Status::Resolved as i32
    } else {
        v2::discussion_record::Status::Open as i32
    };
    let audience = match discussion.visibility.as_str() {
        "public" => v2::Audience::Public as i32,
        "private" => v2::Audience::Private as i32,
        _ => v2::Audience::Unspecified as i32,
    };
    let signed_ids: Vec<Vec<u8>> = operations.iter().filter_map(signed_operation_id).collect();
    let turn_causal_ids: Vec<Vec<u8>> = if signed_ids.len() == discussion.turns.len() {
        signed_ids
    } else {
        discussion
            .turns
            .iter()
            .map(|turn| {
                objects::object::ContentHash::compute(format!("{id}:{}", turn.turn_id).as_bytes())
                    .as_bytes()
                    .to_vec()
            })
            .collect()
    };
    let causal_heads = turn_causal_ids.last().cloned().into_iter().collect();
    let mut payloads = vec![v2::collaboration_event::Payload::Discussion(
        v2::DiscussionRecord {
            r#ref: Some(v2::RecordRef {
                id: id.clone(),
                spool: None,
            }),
            version: objects::object::ContentHash::compute(id.as_bytes())
                .as_bytes()
                .to_vec(),
            anchor: Some(source_anchor(path, symbol)),
            title: discussion_title(discussion),
            status,
            turn_count: discussion.turns.len() as u64,
            audience,
            audience_label: discussion.visibility.clone(),
            causal_heads,
            ..Default::default()
        },
    )];
    for (turn, causal_id) in discussion.turns.iter().zip(turn_causal_ids) {
        payloads.push(v2::collaboration_event::Payload::Turn(v2::DiscussionTurn {
            r#ref: Some(v2::RecordRef {
                id: turn.turn_id.clone(),
                spool: None,
            }),
            discussion: Some(v2::RecordRef {
                id: id.clone(),
                spool: None,
            }),
            body: turn.body.clone(),
            principal_id: turn.author_name.clone(),
            created_at: turn.posted_at,
            causal_id,
            ..Default::default()
        }));
    }
    payloads.extend(
        operations
            .iter()
            .cloned()
            .map(v2::collaboration_event::Payload::Operation),
    );
    payloads
}

fn source_anchor(path: String, symbol: String) -> v2::CollaborationAnchor {
    v2::CollaborationAnchor {
        target: Some(v2::collaboration_anchor::Target::Source(v2::SourceAnchor {
            path,
            symbol_id: symbol,
            ..Default::default()
        })),
    }
}

fn context_list_payloads(fixture: &ContextFixture) -> Vec<v2::collaboration_event::Payload> {
    let mut payloads = Vec::new();
    for file in &fixture.files {
        for annotation in &file.annotations {
            payloads.push(v2::collaboration_event::Payload::Context(
                v2::ContextRecord {
                    r#ref: Some(v2::RecordRef {
                        id: annotation.id.clone(),
                        spool: None,
                    }),
                    content: annotation.content.clone(),
                    principal_id: annotation.attribution.clone(),
                    tags: annotation
                        .tags
                        .iter()
                        .map(|tag| v2::AnnotationTag {
                            tag: Some(v2::annotation_tag::Tag::Text(tag.clone())),
                        })
                        .collect(),
                    anchor: Some(source_anchor(file.path.clone(), String::new())),
                    ..Default::default()
                },
            ));
        }
    }
    for state in &fixture.states {
        let path_state = state.state_id.as_ref().map(|id| id.value.clone());
        for annotation in &state.annotations {
            let mut record = v2::ContextRecord {
                r#ref: Some(v2::RecordRef {
                    id: annotation.id.clone(),
                    spool: None,
                }),
                content: annotation.content.clone(),
                principal_id: annotation.attribution.clone(),
                ..Default::default()
            };
            if let Some(value) = &path_state {
                record.anchor = Some(v2::CollaborationAnchor {
                    target: Some(v2::collaboration_anchor::Target::Source(v2::SourceAnchor {
                        revision: Some(v2::RevisionRef {
                            revision: Some(v2::revision_ref::Revision::State(
                                api::heddle::api::common::StateId {
                                    value: value.clone(),
                                },
                            )),
                            ..Default::default()
                        }),
                        ..Default::default()
                    })),
                });
            }
            payloads.push(v2::collaboration_event::Payload::Context(record));
        }
    }
    payloads
}

fn context_history_payloads(
    fixture: &ContextFixture,
    annotation_id: &str,
) -> Vec<v2::collaboration_event::Payload> {
    fixture
        .histories
        .get(annotation_id)
        .into_iter()
        .flatten()
        .map(|revision| {
            v2::collaboration_event::Payload::Context(v2::ContextRecord {
                r#ref: Some(v2::RecordRef {
                    id: annotation_id.to_string(),
                    spool: None,
                }),
                content: revision.content.clone(),
                principal_id: revision.attribution.clone(),
                causal_id: revision.revision_id.as_bytes().to_vec(),
                tags: revision
                    .tags
                    .iter()
                    .map(|tag| v2::AnnotationTag {
                        tag: Some(v2::annotation_tag::Tag::Text(tag.clone())),
                    })
                    .collect(),
                ..Default::default()
            })
        })
        .collect()
}

async fn write_collaboration_observation(
    send: &mut iroh::endpoint::SendStream,
    server_key: Vec<u8>,
    payloads: Vec<v2::collaboration_event::Payload>,
) {
    let source = v2::EndpointRef {
        kind: v2::EndpointKind::Weft as i32,
        public_key: server_key,
    };
    let budget = v2::ReadBudget {
        max_items: 64,
        max_frame_bytes: 65536,
        max_snapshot_bytes: 1048576,
    };
    let mut sequence = 1u64;
    let open = v2::CollaborationEvent {
        frame: Some(v2::StreamFrame {
            sequence,
            body: Some(v2::stream_frame::Body::Open(v2::StreamOpen {
                source: Some(source),
                binding_digest: vec![9; 32],
                accepted_budget: Some(budget),
                ..Default::default()
            })),
        }),
        ..Default::default()
    };
    send.write_chunk(Bytes::from(
        encode_stream_message(&open.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
    for payload in payloads {
        sequence += 1;
        let event = v2::CollaborationEvent {
            frame: Some(v2::StreamFrame {
                sequence,
                body: Some(v2::stream_frame::Body::Data(v2::StreamData {
                    kind: v2::StreamDataKind::Snapshot as i32,
                })),
            }),
            payload: Some(payload),
        };
        send.write_chunk(Bytes::from(
            encode_stream_message(&event.encode_to_vec()).unwrap(),
        ))
        .await
        .unwrap();
    }
    sequence += 1;
    let checkpoint = v2::CollaborationEvent {
        frame: Some(v2::StreamFrame {
            sequence,
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
    };
    send.write_chunk(Bytes::from(
        encode_stream_message(&checkpoint.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
    sequence += 1;
    let complete = v2::CollaborationEvent {
        frame: Some(v2::StreamFrame {
            sequence,
            body: Some(v2::stream_frame::Body::Complete(v2::StreamComplete {
                cursor: vec![1],
            })),
        }),
        ..Default::default()
    };
    send.write_chunk(Bytes::from(
        encode_stream_message(&complete.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

fn terminal_page(method: &str) -> Vec<u8> {
    match method {
        "/heddle.api.v1alpha2.ThreadService/ObserveThreads" => ListRefsResponse {
            frame: Some(list_refs_response::Frame::PageEnd(ListRefsPageEnd {
                next_page_token: String::new(),
            })),
        }
        .encode_to_vec(),
        _ => Vec::new(),
    }
}

fn bidi_responses(method: &str, pull: Option<PullFixture>) -> Vec<Vec<u8>> {
    let pull_succeeds = pull.is_some();
    match method {
        "/heddle.api.v1alpha2.SyncService/PublishContent" => vec![
            PushServerFrame {
                frame: Some(push_server_frame::Frame::Ready(PushReady::default())),
            }
            .encode_to_vec(),
            PushServerFrame {
                frame: Some(push_server_frame::Frame::Complete(PushComplete {
                    success: false,
                    error: "test rejection".to_string(),
                })),
            }
            .encode_to_vec(),
        ],
        "/heddle.api.v1alpha2.SyncService/Fetch" => {
            let remote_state = pull.as_ref().map(|fixture| fixture.remote_state.clone());
            let has_pack = pull.as_ref().is_some_and(|fixture| fixture.pack.is_some());
            let mut responses = vec![
                PullServerFrame {
                    frame: Some(pull_server_frame::Frame::Ready(PullReady {
                        remote_state: remote_state
                            .clone()
                            .or_else(|| Some(StateId { value: vec![7; 32] })),
                        full_closure_available: has_pack || !pull_succeeds,
                        owner_authorization_protocol_version: 2,
                        owner_genesis: Some(owner_genesis_fixture()),
                        ..PullReady::default()
                    })),
                }
                .encode_to_vec(),
            ];
            if let Some((pack_data, index_data)) = pull.and_then(|fixture| fixture.pack) {
                responses.push(pack_frame(PackStreamKind::Pack, pack_data));
                responses.push(pack_frame(PackStreamKind::Index, index_data));
            }
            responses.push(
                PullServerFrame {
                    frame: Some(pull_server_frame::Frame::Complete(PullComplete {
                        success: pull_succeeds,
                        new_state: remote_state,
                        error: if pull_succeeds {
                            String::new()
                        } else {
                            "test rejection".to_string()
                        },
                    })),
                }
                .encode_to_vec(),
            );
            responses
        }
        _ => Vec::new(),
    }
}

async fn read_request_body(recv: &mut iroh::endpoint::RecvStream, request: &mut Vec<u8>) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
}

fn pack_frame(stream_kind: PackStreamKind, data: Vec<u8>) -> Vec<u8> {
    PullServerFrame {
        frame: Some(pull_server_frame::Frame::Pack(PackChunk {
            stream_kind: stream_kind as i32,
            chunk_length: data.len() as u32,
            data,
            transfer: Some(TransferCheckpoint {
                transfer_id: "pull-pack-test".to_string(),
                transport_mode: TransportMode::NativePack as i32,
                is_complete: true,
                ..TransferCheckpoint::default()
            }),
            is_final_chunk: true,
        })),
    }
    .encode_to_vec()
}
