// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{HashMap, HashSet},
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
    heddle::api::v1alpha1::{
        AnnotatedFile, BlobResponse, CallFailure, CallFailureCode, ContextRevision,
        CreateGrantRequest, CreateSpoolRequest, DeleteGrantRequest, DeleteSpoolRequest, Discussion,
        GetBlobRequest, GetContextHistoryPageEnd, GetContextHistoryRequest,
        GetContextHistoryResponse, GetCurrentUserSpoolRequest, GetDiscussionRequest,
        GetSpoolRequest, GrantTargetRef, HostedGrant, HostedSpool, ListContextPageEnd,
        ListContextRequest, ListContextResponse, ListDiscussionsByStateRequest,
        ListDiscussionsPageEnd, ListDiscussionsResponse, ListGrantsRequest, ListGrantsResponse,
        ListRefsPageEnd, ListRefsResponse, ListThreadsPageEnd, ListThreadsResponse, PackChunk,
        PackStreamKind, PromoteSpoolRequest, PromoteSpoolResponse, PullComplete, PullReady,
        PullServerFrame, PushClientFrame, PushComplete, PushReady, PushRequest, PushServerFrame,
        RepoEvent, SignedSpoolOwnerGenesis, StateContextEntry, StateId, SubscribeRepoEventsRequest,
        TransferCheckpoint, TransportMode, UpdateGrantRequest, UpdateSpoolRequest,
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
const GET_CURRENT_USER_SPOOL_METHOD: &str =
    "/heddle.api.v1alpha1.RegistryService/GetCurrentUserSpool";
const GET_SPOOL_METHOD: &str = "/heddle.api.v1alpha1.RegistryService/GetSpool";
const PROMOTE_SPOOL_METHOD: &str = "/heddle.api.v1alpha1.RegistryService/PromoteSpool";
const CREATE_GRANT_METHOD: &str = "/heddle.api.v1alpha1.RegistryService/CreateGrant";
const LIST_GRANTS_METHOD: &str = "/heddle.api.v1alpha1.RegistryService/ListGrants";
const UPDATE_GRANT_METHOD: &str = "/heddle.api.v1alpha1.RegistryService/UpdateGrant";
const DELETE_GRANT_METHOD: &str = "/heddle.api.v1alpha1.RegistryService/DeleteGrant";
const GET_DISCUSSION_METHOD: &str = "/heddle.api.v1alpha1.CollaborationService/GetDiscussion";
const LIST_BY_STATE_METHOD: &str = "/heddle.api.v1alpha1.CollaborationService/ListByState";
const LIST_CONTEXT_METHOD: &str = "/heddle.api.v1alpha1.RepositoryService/ListContext";
const GET_CONTEXT_HISTORY_METHOD: &str = "/heddle.api.v1alpha1.RepositoryService/GetContextHistory";
const SUBSCRIBE_REPO_EVENTS_METHOD: &str =
    "/heddle.api.v1alpha1.RepositoryService/SubscribeRepoEvents";

#[derive(Default)]
pub(crate) struct SpoolMutationCapture {
    pub updates: Vec<UpdateSpoolRequest>,
    pub deletes: Vec<DeleteSpoolRequest>,
}

#[derive(Clone, Default)]
pub(crate) struct RegistryFixture {
    pub personal_root: Option<HostedSpool>,
    pub spools: HashMap<String, HostedSpool>,
    pub get_spool_requests: Arc<Mutex<Vec<String>>>,
    pub promote_requests: Arc<Mutex<Vec<PromoteSpoolRequest>>>,
    pub promote_denial: Option<(CallFailureCode, String)>,
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
    pub unknown_repo_ids: HashSet<String>,
    pub get_requests: Arc<Mutex<Vec<String>>>,
    pub get_request_state_ids: Arc<Mutex<Vec<Option<Vec<u8>>>>>,
    pub list_requests: Arc<Mutex<usize>>,
    pub subscribe_after: Arc<Mutex<Vec<i64>>>,
    pub subscribe_repo_ids: Arc<Mutex<Vec<String>>>,
    pub subscribe_thread: Arc<Mutex<Vec<(String, String)>>>,
}

#[derive(Clone, Default)]
pub(crate) struct ContextFixture {
    pub files: Vec<AnnotatedFile>,
    pub states: Vec<StateContextEntry>,
    pub histories: HashMap<String, Vec<ContextRevision>>,
    pub list_requests: Arc<Mutex<usize>>,
    pub history_requests: Arc<Mutex<Vec<String>>>,
}

pub async fn start() -> (HostedClient, JoinHandle<()>) {
    start_inner(
        None,
        BlobFixture::default(),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn start_with_registry(
    fixture: RegistryFixture,
) -> (HostedClient, JoinHandle<()>, RegistryFixture) {
    let fixture_clone = fixture.clone();
    let (client, server) = start_inner(
        None,
        BlobFixture::default(),
        None,
        None,
        None,
        None,
        None,
        Some(fixture),
    )
    .await;
    (client, server, fixture_clone)
}

#[cfg(test)]
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
        None,
        Some(fixture),
        None,
    )
    .await;
    (client, server, fixture_clone)
}

#[cfg(test)]
pub(crate) async fn start_with_context(
    fixture: ContextFixture,
) -> (HostedClient, JoinHandle<()>, ContextFixture) {
    let fixture_clone = fixture.clone();
    let (client, server) = start_inner(
        None,
        BlobFixture::default(),
        None,
        None,
        None,
        Some(fixture),
        None,
        None,
    )
    .await;
    (client, server, fixture_clone)
}

#[cfg(test)]
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
        None,
        None,
    )
    .await;
    (client, server, captured)
}

#[cfg(test)]
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
        None,
        None,
    )
    .await;
    (client, server, captured)
}

#[cfg(test)]
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
        None,
        None,
    )
    .await;
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
        BlobFixture::default(),
        None,
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
        BlobFixture::default(),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn start_with_get_blob_contents(
    blobs: impl IntoIterator<Item = (String, Vec<u8>)>,
) -> (HostedClient, JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
    start_with_get_blob_contents_and_pull(blobs, None).await
}

#[cfg(test)]
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

#[cfg(test)]
async fn start_with_get_blob_contents_and_pull(
    blobs: impl IntoIterator<Item = (String, Vec<u8>)>,
    pull: Option<PullFixture>,
) -> (HostedClient, JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
    let requested = Arc::new(Mutex::new(Vec::new()));
    let fixture = BlobFixture {
        contents: blobs.into_iter().collect(),
        requested: Arc::clone(&requested),
    };
    let (client, server) = start_inner(pull, fixture, None, None, None, None, None, None).await;
    (client, server, requested)
}

#[derive(Clone)]
struct PullFixture {
    remote_state: StateId,
    pack: Option<(Vec<u8>, Vec<u8>)>,
}

/// In-memory weft stand-in: accept a push pack, then serve it on clone.
#[derive(Clone, Default)]
pub(crate) struct DurableSyncStore {
    stored: Arc<Mutex<Option<PullFixture>>>,
}

impl DurableSyncStore {
    fn pull_fixture(&self) -> Option<PullFixture> {
        self.stored
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub(crate) fn install(&self, remote_state: StateId, pack_data: Vec<u8>, index_data: Vec<u8>) {
        *self
            .stored
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(PullFixture {
            remote_state,
            pack: Some((pack_data, index_data)),
        });
    }
}

#[derive(Clone, Default)]
struct BlobFixture {
    contents: HashMap<String, Vec<u8>>,
    requested: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone, Default)]
struct GrantStore {
    grants: Arc<Mutex<Vec<HostedGrant>>>,
}

#[allow(clippy::too_many_arguments)]
async fn start_inner(
    pull: Option<PullFixture>,
    blobs: BlobFixture,
    create_spool: Option<Arc<Mutex<Vec<CreateSpoolRequest>>>>,
    spool_mutations: Option<Arc<Mutex<SpoolMutationCapture>>>,
    push_requests: Option<Arc<Mutex<Vec<PushRequest>>>>,
    context: Option<ContextFixture>,
    collaboration: Option<CollaborationFixture>,
    registry: Option<RegistryFixture>,
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
    let grants = GrantStore::default();
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
                context.clone(),
                collaboration.clone(),
                registry.clone(),
                grants.clone(),
                None,
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

/// Multi-accept fixture: first client pushes; after bounded close a
/// fresh client clones the stored pack from the same endpoint address.
#[cfg(test)]
pub(crate) async fn start_durable_push_clone() -> (
    HostedClient,
    iroh::EndpointAddr,
    JoinHandle<()>,
    DurableSyncStore,
) {
    let store = DurableSyncStore::default();
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let server_addr = server.addr();
    let grants = GrantStore::default();
    let store_for_server = store.clone();
    let server_task = tokio::spawn(async move {
        loop {
            let Some(incoming) = server.accept().await else {
                break;
            };
            let Ok(connection) = incoming.await else {
                continue;
            };
            let store = store_for_server.clone();
            let grants = grants.clone();
            tokio::spawn(async move {
                while let Ok((send, recv)) = connection.accept_bi().await {
                    tokio::spawn(serve_call(
                        send,
                        recv,
                        store.pull_fixture(),
                        BlobFixture::default(),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        grants.clone(),
                        Some(store.clone()),
                    ));
                }
            });
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
    let client = HostedClient::connect_addr_with_context(endpoint, server_addr.clone(), context)
        .await
        .unwrap();
    (client, server_addr, server_task, store)
}

#[cfg(test)]
pub(crate) async fn connect_test_client(server_addr: iroh::EndpointAddr) -> HostedClient {
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
    HostedClient::connect_addr_with_context(endpoint, server_addr, context)
        .await
        .unwrap()
}

#[allow(clippy::too_many_arguments)]
async fn serve_call(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    pull: Option<PullFixture>,
    blobs: BlobFixture,
    create_spool: Option<Arc<Mutex<Vec<CreateSpoolRequest>>>>,
    spool_mutations: Option<Arc<Mutex<SpoolMutationCapture>>>,
    push_requests: Option<Arc<Mutex<Vec<PushRequest>>>>,
    context: Option<ContextFixture>,
    collaboration: Option<CollaborationFixture>,
    registry: Option<RegistryFixture>,
    grants: GrantStore,
    durable: Option<DurableSyncStore>,
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
            } else if method == GET_CURRENT_USER_SPOOL_METHOD {
                serve_get_current_user_spool(&mut send, &mut recv, &mut request, registry).await;
            } else if method == GET_SPOOL_METHOD {
                serve_get_spool(&mut send, &mut recv, &mut request, registry).await;
            } else if method == PROMOTE_SPOOL_METHOD {
                serve_promote_spool(&mut send, &mut recv, &mut request, registry).await;
            } else if method == CREATE_GRANT_METHOD {
                serve_create_grant(&mut send, &mut recv, &mut request, &grants).await;
            } else if method == LIST_GRANTS_METHOD {
                serve_list_grants(&mut send, &mut recv, &mut request, &grants).await;
            } else if method == UPDATE_GRANT_METHOD {
                serve_update_grant(&mut send, &mut recv, &mut request, &grants).await;
            } else if method == DELETE_GRANT_METHOD {
                serve_delete_grant(&mut send, &mut recv, &mut request, &grants).await;
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
            if method == LIST_CONTEXT_METHOD {
                if let Some(context) = context {
                    serve_list_context(&mut send, &mut recv, &mut request, context).await;
                } else {
                    let body = terminal_page(&method);
                    send.write_chunk(Bytes::from(encode_stream_message(&body).unwrap()))
                        .await
                        .unwrap();
                }
            } else if method == GET_CONTEXT_HISTORY_METHOD {
                if let Some(context) = context {
                    serve_get_context_history(&mut send, &mut recv, &mut request, context).await;
                } else {
                    let body = terminal_page(&method);
                    send.write_chunk(Bytes::from(encode_stream_message(&body).unwrap()))
                        .await
                        .unwrap();
                }
            } else if method == LIST_BY_STATE_METHOD {
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
                serve_push(
                    send,
                    recv,
                    request.split_off(prelude_len),
                    push_requests,
                    durable,
                )
                .await;
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
    durable: Option<DurableSyncStore>,
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
    let local_state = request.local_state.clone();
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
    let accept = durable.is_some() && local_state.is_some();
    let complete = PushServerFrame {
        frame: Some(push_server_frame::Frame::Complete(PushComplete {
            success: accept,
            new_state: if accept { local_state } else { None },
            error: if accept {
                String::new()
            } else {
                "test rejection".to_string()
            },
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

async fn serve_get_current_user_spool(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    registry: Option<RegistryFixture>,
) {
    read_request_body(recv, request).await;
    let _ = decode_request_frame(request)
        .ok()
        .and_then(|frame| GetCurrentUserSpoolRequest::decode(frame.body).ok());
    let response = registry
        .and_then(|fixture| fixture.personal_root)
        .unwrap_or_default();
    send.write_chunk(Bytes::from(
        encode_success_response(&response.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_get_spool(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    registry: Option<RegistryFixture>,
) {
    read_request_body(recv, request).await;
    let full_path = decode_request_frame(request)
        .ok()
        .and_then(|frame| GetSpoolRequest::decode(frame.body).ok())
        .map(|body| body.full_path)
        .unwrap_or_default();
    let Some(fixture) = registry else {
        send.write_chunk(Bytes::from(encode_success_response(&[]).unwrap()))
            .await
            .unwrap();
        return;
    };
    fixture
        .get_spool_requests
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(full_path.clone());
    let Some(spool) = fixture.spools.get(&full_path) else {
        let failure = CallFailure {
            code: CallFailureCode::NotFound as i32,
            message: format!("{full_path} not found"),
            error: None,
        };
        send.write_chunk(Bytes::from(encode_failure_response(&failure).unwrap()))
            .await
            .unwrap();
        return;
    };
    send.write_chunk(Bytes::from(
        encode_success_response(&spool.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_create_grant(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    grants: &GrantStore,
) {
    read_request_body(recv, request).await;
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| CreateGrantRequest::decode(frame.body).ok());
    let Some(body) = body else {
        send.write_chunk(Bytes::from(encode_success_response(&[]).unwrap()))
            .await
            .unwrap();
        return;
    };
    let grant = HostedGrant {
        subject: body.subject,
        role: body.role,
        target: body.target,
    };
    upsert_grant(grants, grant.clone());
    send.write_chunk(Bytes::from(
        encode_success_response(&grant.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_list_grants(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    grants: &GrantStore,
) {
    read_request_body(recv, request).await;
    let resource = decode_request_frame(request)
        .ok()
        .and_then(|frame| ListGrantsRequest::decode(frame.body).ok())
        .map(|body| body.resource)
        .unwrap_or_default();
    let stored = grants
        .grants
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    let listed: Vec<HostedGrant> = stored
        .into_iter()
        .filter(|grant| grant_matches_resource(grant, &resource))
        .collect();
    let response = ListGrantsResponse { grants: listed };
    send.write_chunk(Bytes::from(
        encode_success_response(&response.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_update_grant(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    grants: &GrantStore,
) {
    read_request_body(recv, request).await;
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| UpdateGrantRequest::decode(frame.body).ok());
    let Some(body) = body else {
        send.write_chunk(Bytes::from(encode_success_response(&[]).unwrap()))
            .await
            .unwrap();
        return;
    };
    let grant = HostedGrant {
        subject: body.subject,
        role: body.role,
        target: body.target,
    };
    upsert_grant(grants, grant.clone());
    send.write_chunk(Bytes::from(
        encode_success_response(&grant.encode_to_vec()).unwrap(),
    ))
    .await
    .unwrap();
}

async fn serve_delete_grant(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    grants: &GrantStore,
) {
    read_request_body(recv, request).await;
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| DeleteGrantRequest::decode(frame.body).ok());
    if let Some(body) = body {
        let key = grant_identity_key(&body.subject, body.target.as_ref());
        grants
            .grants
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .retain(|grant| grant_identity_key(&grant.subject, grant.target.as_ref()) != key);
    }
    send.write_chunk(Bytes::from(encode_success_response(&[]).unwrap()))
        .await
        .unwrap();
}

fn upsert_grant(store: &GrantStore, grant: HostedGrant) {
    let key = grant_identity_key(&grant.subject, grant.target.as_ref());
    let mut grants = store
        .grants
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(existing) = grants
        .iter_mut()
        .find(|row| grant_identity_key(&row.subject, row.target.as_ref()) == key)
    {
        *existing = grant;
    } else {
        grants.push(grant);
    }
}

fn grant_identity_key(subject: &str, target: Option<&GrantTargetRef>) -> (String, String, String) {
    let (namespace, repo) = grant_target_paths(target);
    (
        subject.to_string(),
        namespace.unwrap_or_default(),
        repo.unwrap_or_default(),
    )
}

fn grant_target_paths(target: Option<&GrantTargetRef>) -> (Option<String>, Option<String>) {
    use api::heddle::api::v1alpha1::grant_target_ref::Target;
    match target.and_then(|target| target.target.clone()) {
        Some(Target::NamespacePath(path)) if !path.is_empty() => (Some(path), None),
        Some(Target::RepoPath(repository)) => (
            None,
            super::helpers::repository_ref_path(&repository).map(ToOwned::to_owned),
        ),
        _ => (None, None),
    }
}

fn grant_matches_resource(grant: &HostedGrant, resource: &str) -> bool {
    if resource.is_empty() {
        return true;
    }
    // weft `list_manageable_grants` exact-matches the visible path
    // (`spool/<handle>/<name>`). Stripping `repo:` here hid the CLI sending
    // `repo:{spool}` while create/delete send the bare path (heddle#1744).
    let (namespace, repo) = grant_target_paths(grant.target.as_ref());
    repo.as_deref() == Some(resource) || namespace.as_deref() == Some(resource)
}

async fn serve_promote_spool(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    registry: Option<RegistryFixture>,
) {
    read_request_body(recv, request).await;
    let body = decode_request_frame(request)
        .ok()
        .and_then(|frame| PromoteSpoolRequest::decode(frame.body).ok());
    if let (Some(fixture), Some(body)) = (registry.as_ref(), body.as_ref()) {
        fixture
            .promote_requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(body.clone());
        if let Some((code, message)) = fixture.promote_denial.clone() {
            let failure = CallFailure {
                code: code as i32,
                message,
                error: None,
            };
            send.write_chunk(Bytes::from(encode_failure_response(&failure).unwrap()))
                .await
                .unwrap();
            return;
        }
        let slug = body
            .full_path
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string();
        let promoted = HostedSpool {
            full_path: format!("spool/{slug}"),
            kind: "spool".to_string(),
            is_repo: true,
            ..HostedSpool::default()
        };
        let response = PromoteSpoolResponse {
            spool: Some(promoted),
        };
        send.write_chunk(Bytes::from(
            encode_success_response(&response.encode_to_vec()).unwrap(),
        ))
        .await
        .unwrap();
        return;
    }
    send.write_chunk(Bytes::from(encode_success_response(&[]).unwrap()))
        .await
        .unwrap();
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

async fn serve_list_context(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    fixture: ContextFixture,
) {
    read_request_body(recv, request).await;
    let _ = decode_request_frame(request)
        .ok()
        .and_then(|frame| ListContextRequest::decode(frame.body).ok());
    *fixture
        .list_requests
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) += 1;
    for file in &fixture.files {
        let body = ListContextResponse {
            frame: Some(list_context_response::Frame::Item(file.clone())),
            states: Vec::new(),
        }
        .encode_to_vec();
        send.write_chunk(Bytes::from(encode_stream_message(&body).unwrap()))
            .await
            .unwrap();
    }
    let end = ListContextResponse {
        frame: Some(list_context_response::Frame::PageEnd(ListContextPageEnd {
            next_page_token: String::new(),
            ..ListContextPageEnd::default()
        })),
        states: fixture.states.clone(),
    }
    .encode_to_vec();
    send.write_chunk(Bytes::from(encode_stream_message(&end).unwrap()))
        .await
        .unwrap();
}

async fn serve_get_context_history(
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request: &mut Vec<u8>,
    fixture: ContextFixture,
) {
    read_request_body(recv, request).await;
    let annotation_id = decode_request_frame(request)
        .ok()
        .and_then(|frame| GetContextHistoryRequest::decode(frame.body).ok())
        .map(|body| body.annotation_id)
        .unwrap_or_default();
    fixture
        .history_requests
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(annotation_id.clone());
    if let Some(revisions) = fixture.histories.get(&annotation_id) {
        for revision in revisions {
            let body = GetContextHistoryResponse {
                frame: Some(get_context_history_response::Frame::Item(revision.clone())),
            }
            .encode_to_vec();
            send.write_chunk(Bytes::from(encode_stream_message(&body).unwrap()))
                .await
                .unwrap();
        }
    }
    let end = GetContextHistoryResponse {
        frame: Some(get_context_history_response::Frame::PageEnd(
            GetContextHistoryPageEnd {
                next_page_token: String::new(),
                ..GetContextHistoryPageEnd::default()
            },
        )),
    }
    .encode_to_vec();
    send.write_chunk(Bytes::from(encode_stream_message(&end).unwrap()))
        .await
        .unwrap();
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
    let repo_id = subscribe
        .as_ref()
        .map(|body| body.repo_id.clone())
        .unwrap_or_default();
    let thread_scope = subscribe
        .map(|body| (body.thread, body.thread_id))
        .unwrap_or_default();
    fixture
        .subscribe_after
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(after_event_id);
    fixture
        .subscribe_repo_ids
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(repo_id.clone());
    fixture
        .subscribe_thread
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(thread_scope);
    if fixture.unknown_repo_ids.contains(&repo_id) {
        let failure = CallFailure {
            code: CallFailureCode::NotFound as i32,
            message: format!("repository {repo_id} not found"),
            error: None,
        };
        send.write_chunk(Bytes::from(encode_stream_failure(&failure).unwrap()))
            .await
            .unwrap();
        return;
    }
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

#[cfg(test)]
mod grant_filter_tests {
    use api::heddle::api::v1alpha1::{
        GrantTargetRef, HostedGrant, RepositoryRef, grant_target_ref::Target,
        repository_ref::Reference,
    };

    use super::{grant_matches_resource, grant_target_paths};

    fn repo_grant(path: &str) -> HostedGrant {
        HostedGrant {
            subject: "alice".into(),
            role: 2,
            target: Some(GrantTargetRef {
                target: Some(Target::RepoPath(RepositoryRef {
                    reference: Some(Reference::CanonicalPath(path.to_string())),
                })),
            }),
        }
    }

    #[test]
    fn list_filter_matches_bare_spool_path_only() {
        let grant = repo_grant("spool/willow-ibis-8e7264/notes");
        let (namespace, repo) = grant_target_paths(grant.target.as_ref());
        assert_eq!(namespace, None);
        assert_eq!(repo.as_deref(), Some("spool/willow-ibis-8e7264/notes"));
        assert!(grant_matches_resource(
            &grant,
            "spool/willow-ibis-8e7264/notes"
        ));
        assert!(
            !grant_matches_resource(&grant, "repo:spool/willow-ibis-8e7264/notes"),
            "weft exact-matches the visible path; repo: prefix must not match"
        );
        assert!(grant_matches_resource(&grant, ""));
    }
}
