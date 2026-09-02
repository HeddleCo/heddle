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
        encode_failure_response, encode_stream_message, encode_success_response,
    },
    heddle::api::v1alpha1::{
        BlobResponse, CallFailure, CallFailureCode, CreateSpoolRequest, DeleteSpoolRequest,
        Discussion, GetBlobRequest, GetContextHistoryPageEnd, GetContextHistoryResponse,
        GetDiscussionRequest, HostedSpool, ListContextPageEnd, ListContextResponse,
        ListDiscussionsByStateRequest, ListDiscussionsPageEnd, ListDiscussionsResponse,
        ListRefsPageEnd, ListRefsResponse, ListThreadsPageEnd, ListThreadsResponse, PackChunk,
        PackStreamKind, PullComplete, PullReady, PullServerFrame, PushClientFrame, PushComplete,
        PushReady, PushRequest, PushServerFrame, RepoEvent, SignedSpoolOwnerGenesis, StateId,
        SubscribeRepoEventsRequest, TransferCheckpoint, TransportMode, UpdateSpoolRequest,
        get_context_history_response, list_context_response, list_discussions_response,
        list_refs_response, list_threads_response, pull_server_frame, push_client_frame,
        push_server_frame,
    },
    method_descriptor,
};
use base64::Engine as _;
use bytes::Bytes;
use crypto::Ed25519Signer;
use iroh::{Endpoint, RelayMode, endpoint::presets};
use prost::Message;
use tokio::task::JoinHandle;

use super::{CallContextFactory, HostedClient};

const OWNER_GENESIS_FIXTURE_HEX: &str = "0a380a10222222222222222222222222222222221224080112208a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c12640a20def88318e44a809464c1022f22230567bae6805d17b1ccfc2bebe5326232c58a1240bfe677c0b6fec8d28e379f584f36dee7258d834222f9b75f61dc75b7db2d836d76d4fb6eaf9e7f561925b2e6882b51eadaf3ec77c565f5b638ad0febfc8cd304";
const GET_BLOB_METHOD: &str = "/heddle.api.v1alpha1.RepositoryService/GetBlob";
const CREATE_SPOOL_METHOD: &str = "/heddle.api.v1alpha1.RegistryService/CreateSpool";
const DELETE_SPOOL_METHOD: &str = "/heddle.api.v1alpha1.RegistryService/DeleteSpool";
const UPDATE_SPOOL_METHOD: &str = "/heddle.api.v1alpha1.RegistryService/UpdateSpool";
const GET_DISCUSSION_METHOD: &str = "/heddle.api.v1alpha1.CollaborationService/GetDiscussion";
const LIST_BY_STATE_METHOD: &str = "/heddle.api.v1alpha1.CollaborationService/ListByState";
const SUBSCRIBE_REPO_EVENTS_METHOD: &str =
    "/heddle.api.v1alpha1.RepositoryService/SubscribeRepoEvents";

#[derive(Default)]
pub(crate) struct SpoolMutationCapture {
    pub updates: Vec<UpdateSpoolRequest>,
    pub deletes: Vec<DeleteSpoolRequest>,
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
    pub events: Vec<RepoEvent>,
    pub one_event_per_subscribe: bool,
    pub get_requests: Arc<Mutex<Vec<String>>>,
    pub get_request_state_ids: Arc<Mutex<Vec<Option<Vec<u8>>>>>,
    pub list_requests: Arc<Mutex<usize>>,
    pub subscribe_after: Arc<Mutex<Vec<i64>>>,
    pub subscribe_thread: Arc<Mutex<Vec<(String, String)>>>,
}

pub(crate) async fn start() -> (HostedClient, JoinHandle<()>) {
    start_inner(None, BlobFixture::default(), None, None, None, None).await
}

pub(crate) async fn start_with_collaboration(
    fixture: CollaborationFixture,
) -> (HostedClient, JoinHandle<()>, CollaborationFixture) {
    let fixture_clone = fixture.clone();
    let (client, server) = start_inner(
        None,
        BlobFixture::default(),
        None,
        None,
        None,
        Some(fixture),
    )
    .await;
    (client, server, fixture_clone)
}

pub(crate) async fn start_recording_push()
-> (HostedClient, JoinHandle<()>, Arc<Mutex<Vec<PushRequest>>>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (client, server) = start_inner(
        None,
        BlobFixture::default(),
        None,
        None,
        Some(Arc::clone(&captured)),
        None,
    )
    .await;
    (client, server, captured)
}

pub(crate) async fn start_recording_create_spool() -> (
    HostedClient,
    JoinHandle<()>,
    Arc<Mutex<Vec<CreateSpoolRequest>>>,
) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (client, server) = start_inner(
        None,
        BlobFixture::default(),
        Some(Arc::clone(&captured)),
        None,
        None,
        None,
    )
    .await;
    (client, server, captured)
}

pub(crate) async fn start_recording_spool_mutations() -> (
    HostedClient,
    JoinHandle<()>,
    Arc<Mutex<SpoolMutationCapture>>,
) {
    let captured = Arc::new(Mutex::new(SpoolMutationCapture::default()));
    let (client, server) = start_inner(
        None,
        BlobFixture::default(),
        None,
        Some(Arc::clone(&captured)),
        None,
        None,
    )
    .await;
    (client, server, captured)
}

pub(crate) async fn start_with_remote_state(
    remote_state: StateId,
) -> (HostedClient, JoinHandle<()>) {
    start_inner(
        Some(PullFixture {
            remote_state,
            pack: None,
        }),
        BlobFixture::default(),
        None,
        None,
        None,
        None,
    )
    .await
}

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
        BlobFixture::default(),
        None,
        None,
        None,
        None,
    )
    .await
}

pub(crate) async fn start_with_get_blob_contents(
    blobs: impl IntoIterator<Item = (String, Vec<u8>)>,
) -> (HostedClient, JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
    start_with_get_blob_contents_and_pull(blobs, None).await
}

pub(crate) async fn start_with_get_blob_contents_and_pull_pack(
    blobs: impl IntoIterator<Item = (String, Vec<u8>)>,
    remote_state: StateId,
    pack_data: Vec<u8>,
    index_data: Vec<u8>,
) -> (HostedClient, JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
    start_with_get_blob_contents_and_pull(
        blobs,
        Some(PullFixture {
            remote_state,
            pack: Some((pack_data, index_data)),
        }),
    )
    .await
}

async fn start_with_get_blob_contents_and_pull(
    blobs: impl IntoIterator<Item = (String, Vec<u8>)>,
    pull: Option<PullFixture>,
) -> (HostedClient, JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
    let requested = Arc::new(Mutex::new(Vec::new()));
    let fixture = BlobFixture {
        contents: blobs.into_iter().collect(),
        requested: Arc::clone(&requested),
    };
    let (client, server) = start_inner(pull, fixture, None, None, None, None).await;
    (client, server, requested)
}

#[derive(Clone)]
struct PullFixture {
    remote_state: StateId,
    pack: Option<(Vec<u8>, Vec<u8>)>,
}

#[derive(Clone, Default)]
struct BlobFixture {
    contents: HashMap<String, Vec<u8>>,
    requested: Arc<Mutex<Vec<String>>>,
}

async fn start_inner(
    pull: Option<PullFixture>,
    blobs: BlobFixture,
    create_spool: Option<Arc<Mutex<Vec<CreateSpoolRequest>>>>,
    spool_mutations: Option<Arc<Mutex<SpoolMutationCapture>>>,
    push_requests: Option<Arc<Mutex<Vec<PushRequest>>>>,
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
    let server_task = tokio::spawn(async move {
        let connection = server
            .accept()
            .await
            .expect("hosted test connection")
            .await
            .unwrap();
        while let Ok((send, recv)) = connection.accept_bi().await {
            tokio::spawn(serve_call(
                send,
                recv,
                pull.clone(),
                blobs.clone(),
                create_spool.clone(),
                spool_mutations.clone(),
                push_requests.clone(),
                collaboration.clone(),
            ));
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
    let signer = Ed25519Signer::generate().unwrap();
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
    pull: Option<PullFixture>,
    blobs: BlobFixture,
    create_spool: Option<Arc<Mutex<Vec<CreateSpoolRequest>>>>,
    spool_mutations: Option<Arc<Mutex<SpoolMutationCapture>>>,
    push_requests: Option<Arc<Mutex<Vec<PushRequest>>>>,
    collaboration: Option<CollaborationFixture>,
) {
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
    let descriptor = method_descriptor(&method).expect("registered hosted method");
    match descriptor.streaming {
        StreamingShape::Unary | StreamingShape::ClientStreaming => {
            if method == CREATE_SPOOL_METHOD {
                serve_create_spool(&mut send, &mut recv, &mut request, create_spool).await;
            } else if method == UPDATE_SPOOL_METHOD {
                serve_update_spool(&mut send, &mut recv, &mut request, spool_mutations).await;
            } else if method == DELETE_SPOOL_METHOD {
                serve_delete_spool(&mut send, &mut recv, &mut request, spool_mutations).await;
            } else if method == GET_BLOB_METHOD && !blobs.contents.is_empty() {
                serve_get_blob(&mut send, &mut recv, &mut request, blobs).await;
            } else if method == GET_DISCUSSION_METHOD {
                if let Some(collaboration) = collaboration {
                    serve_get_discussion(&mut send, &mut recv, &mut request, collaboration).await;
                } else {
                    send.write_chunk(Bytes::from(encode_success_response(&[]).unwrap()))
                        .await
                        .unwrap();
                }
            } else {
                send.write_chunk(Bytes::from(encode_success_response(&[]).unwrap()))
                    .await
                    .unwrap();
            }
        }
        StreamingShape::ServerStreaming => {
            if method == LIST_BY_STATE_METHOD {
                if let Some(collaboration) = collaboration {
                    serve_list_by_state(&mut send, &mut recv, &mut request, collaboration).await;
                } else {
                    let body = terminal_page(&method);
                    send.write_chunk(Bytes::from(encode_stream_message(&body).unwrap()))
                        .await
                        .unwrap();
                }
            } else if method == SUBSCRIBE_REPO_EVENTS_METHOD {
                if let Some(collaboration) = collaboration {
                    serve_subscribe_repo_events(&mut send, &mut recv, &mut request, collaboration)
                        .await;
                } else {
                    send.finish().unwrap();
                    return;
                }
            } else {
                let body = terminal_page(&method);
                send.write_chunk(Bytes::from(encode_stream_message(&body).unwrap()))
                    .await
                    .unwrap();
            }
        }
        StreamingShape::Bidirectional => {
            if method == "/heddle.api.v1alpha1.RepoSyncService/Push" {
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
            ..PushReady::default()
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
            ..PushComplete::default()
        })),
    }
    .encode_to_vec();
    send.write_chunk(Bytes::from(encode_stream_message(&complete).unwrap()))
        .await
        .unwrap();
    send.finish().unwrap();
}

fn terminal_page(method: &str) -> Vec<u8> {
    match method {
        "/heddle.api.v1alpha1.RepoSyncService/ListRefs" => ListRefsResponse {
            frame: Some(list_refs_response::Frame::PageEnd(ListRefsPageEnd {
                next_page_token: String::new(),
                ..ListRefsPageEnd::default()
            })),
        }
        .encode_to_vec(),
        "/heddle.api.v1alpha1.RepositoryService/ListContext" => ListContextResponse {
            frame: Some(list_context_response::Frame::PageEnd(ListContextPageEnd {
                next_page_token: String::new(),
                ..ListContextPageEnd::default()
            })),
            states: Vec::new(),
        }
        .encode_to_vec(),
        "/heddle.api.v1alpha1.RepositoryService/GetContextHistory" => GetContextHistoryResponse {
            frame: Some(get_context_history_response::Frame::PageEnd(
                GetContextHistoryPageEnd {
                    next_page_token: String::new(),
                    ..GetContextHistoryPageEnd::default()
                },
            )),
        }
        .encode_to_vec(),
        "/heddle.api.v1alpha1.WorkflowService/ListThreads" => ListThreadsResponse {
            frame: Some(list_threads_response::Frame::PageEnd(ListThreadsPageEnd {
                next_page_token: String::new(),
                ..ListThreadsPageEnd::default()
            })),
        }
        .encode_to_vec(),
        "/heddle.api.v1alpha1.CollaborationService/ListByState" => ListDiscussionsResponse {
            frame: Some(list_discussions_response::Frame::PageEnd(
                ListDiscussionsPageEnd {
                    next_page_token: String::new(),
                },
            )),
        }
        .encode_to_vec(),
        _ => Vec::new(),
    }
}

fn bidi_responses(method: &str, pull: Option<PullFixture>) -> Vec<Vec<u8>> {
    let pull_succeeds = pull.is_some();
    match method {
        "/heddle.api.v1alpha1.RepoSyncService/Push" => vec![
            PushServerFrame {
                frame: Some(push_server_frame::Frame::Ready(PushReady::default())),
            }
            .encode_to_vec(),
            PushServerFrame {
                frame: Some(push_server_frame::Frame::Complete(PushComplete {
                    success: false,
                    error: "test rejection".to_string(),
                    ..PushComplete::default()
                })),
            }
            .encode_to_vec(),
        ],
        "/heddle.api.v1alpha1.RepoSyncService/Pull" => {
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
                        ..PullComplete::default()
                    })),
                }
                .encode_to_vec(),
            );
            responses
        }
        _ => Vec::new(),
    }
}

async fn serve_create_spool(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    captured: Option<Arc<Mutex<Vec<CreateSpoolRequest>>>>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| CreateSpoolRequest::decode(frame.body).ok());
    if let (Some(captured), Some(body)) = (captured.as_ref(), body.as_ref()) {
        captured
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(body.clone());
    }
    let response = match body {
        Some(request) => HostedSpool {
            full_path: format!("{}/{}", request.parent_path, request.slug),
            kind: if request.is_repo {
                "project".to_string()
            } else {
                "namespace".to_string()
            },
            is_repo: request.is_repo,
            display_name: request.display_name.unwrap_or_default(),
            ..HostedSpool::default()
        },
        None => HostedSpool::default(),
    };
    send.write_chunk(Bytes::from(
        encode_success_response(&response.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_update_spool(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    captured: Option<Arc<Mutex<SpoolMutationCapture>>>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| UpdateSpoolRequest::decode(frame.body).ok());
    if let (Some(captured), Some(body)) = (captured.as_ref(), body.as_ref()) {
        captured
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .updates
            .push(body.clone());
    }
    let response = HostedSpool {
        full_path: body.map_or_else(String::new, |request| request.full_path),
        ..HostedSpool::default()
    };
    send.write_chunk(Bytes::from(
        encode_success_response(&response.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_delete_spool(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    captured: Option<Arc<Mutex<SpoolMutationCapture>>>,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| DeleteSpoolRequest::decode(frame.body).ok());
    if let (Some(captured), Some(body)) = (captured.as_ref(), body) {
        captured
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .deletes
            .push(body);
    }
    send.write_chunk(Bytes::from(encode_success_response(&[]).unwrap()))
        .await
        .unwrap();
}

async fn serve_get_blob(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    blobs: BlobFixture,
) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
    let path = decode_request_frame(request)
        .ok()
        .and_then(|frame| GetBlobRequest::decode(frame.body).ok())
        .map(|body| body.path)
        .unwrap_or_default();
    blobs
        .requested
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(path.clone());
    let content = blobs.contents.get(&path).cloned().unwrap_or_default();
    let is_binary = std::str::from_utf8(&content).is_err();
    let encoded = if is_binary {
        base64::engine::general_purpose::STANDARD.encode(&content)
    } else {
        String::from_utf8(content).unwrap_or_default()
    };
    let response = BlobResponse {
        content: encoded,
        is_binary,
        ..Default::default()
    };
    send.write_chunk(Bytes::from(
        encode_success_response(&response.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn read_request_body(recv: &mut iroh::endpoint::RecvStream, request: &mut Vec<u8>) {
    while let Ok(Some(chunk)) = recv.read_chunk(api::framing::MAX_CONTROL_BODY + 6).await {
        request.extend_from_slice(&chunk);
    }
}

async fn serve_get_discussion(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    fixture: CollaborationFixture,
) {
    read_request_body(recv, request).await;
    let request = decode_request_frame(request)
        .ok()
        .and_then(|frame| GetDiscussionRequest::decode(frame.body).ok());
    let discussion_id = request
        .as_ref()
        .map(|body| body.discussion_id.clone())
        .unwrap_or_default();
    let state_id = request.and_then(|body| body.state_id.map(|state| state.value));
    fixture
        .get_requests
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(discussion_id.clone());
    fixture
        .get_request_state_ids
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(state_id);
    if let Some(code) = fixture.hidden.get(&discussion_id).copied() {
        let failure = CallFailure {
            code: code as i32,
            message: "discussion is not visible".to_string(),
            error: None,
        };
        send.write_chunk(Bytes::from(encode_failure_response(&failure).unwrap()))
            .await
            .unwrap();
        return;
    }
    let Some(discussion) = fixture.discussions.get(&discussion_id) else {
        let failure = CallFailure {
            code: CallFailureCode::NotFound as i32,
            message: format!("discussion {discussion_id} not found"),
            error: None,
        };
        send.write_chunk(Bytes::from(encode_failure_response(&failure).unwrap()))
            .await
            .unwrap();
        return;
    };
    send.write_chunk(Bytes::from(
        encode_success_response(&discussion.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_list_by_state(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    fixture: CollaborationFixture,
) {
    read_request_body(recv, request).await;
    let _ = decode_request_frame(request)
        .ok()
        .and_then(|frame| ListDiscussionsByStateRequest::decode(frame.body).ok());
    *fixture
        .list_requests
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) += 1;
    for discussion in &fixture.list {
        let body = ListDiscussionsResponse {
            frame: Some(list_discussions_response::Frame::Item(Box::new(
                discussion.clone(),
            ))),
        }
        .encode_to_vec();
        send.write_chunk(Bytes::from(encode_stream_message(&body).unwrap()))
            .await
            .unwrap();
    }
    let end = ListDiscussionsResponse {
        frame: Some(list_discussions_response::Frame::PageEnd(
            ListDiscussionsPageEnd {
                next_page_token: String::new(),
            },
        )),
    }
    .encode_to_vec();
    send.write_chunk(Bytes::from(encode_stream_message(&end).unwrap()))
        .await
        .unwrap();
}

async fn serve_subscribe_repo_events(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    fixture: CollaborationFixture,
) {
    read_request_body(recv, request).await;
    let subscribe = decode_request_frame(request)
        .ok()
        .and_then(|frame| SubscribeRepoEventsRequest::decode(frame.body).ok());
    let after_event_id = subscribe
        .as_ref()
        .map(|body| body.after_event_id)
        .unwrap_or(0);
    let thread_scope = subscribe
        .map(|body| (body.thread, body.thread_id))
        .unwrap_or_default();
    fixture
        .subscribe_after
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(after_event_id);
    fixture
        .subscribe_thread
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(thread_scope);
    let mut matching = fixture
        .events
        .into_iter()
        .filter(|event| event.event_id > after_event_id);
    if fixture.one_event_per_subscribe {
        if let Some(event) = matching.next() {
            send.write_chunk(Bytes::from(
                encode_stream_message(&event.encode_to_vec()).unwrap(),
            ))
            .await
            .unwrap();
        }
    } else {
        for event in matching {
            send.write_chunk(Bytes::from(
                encode_stream_message(&event.encode_to_vec()).unwrap(),
            ))
            .await
            .unwrap();
        }
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
