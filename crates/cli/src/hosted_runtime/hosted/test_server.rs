// SPDX-License-Identifier: Apache-2.0

use std::net::Ipv4Addr;

use api::{
    StreamingShape,
    framing::{
        StreamFrame, decode_request_prelude, decode_stream_frame, encode_stream_message,
        encode_success_response,
    },
    heddle::api::v1alpha1::{
        GetContextHistoryPageEnd, GetContextHistoryResponse, ListContextPageEnd,
        ListContextResponse, ListDiscussionsPageEnd, ListDiscussionsResponse, ListRefsPageEnd,
        ListRefsResponse, ListThreadsPageEnd, ListThreadsResponse, PackChunk, PackStreamKind,
        PullComplete, PullReady, PullServerFrame, PushClientFrame, PushComplete, PushReady,
        PushServerFrame, SignedSpoolOwnerGenesis, StateId, TransferCheckpoint, TransportMode,
        get_context_history_response, list_context_response, list_discussions_response,
        list_refs_response, list_threads_response, pull_server_frame, push_client_frame,
        push_server_frame,
    },
    method_descriptor,
};
use bytes::Bytes;
use crypto::Ed25519Signer;
use iroh::{Endpoint, RelayMode, endpoint::presets};
use prost::Message;
use tokio::task::JoinHandle;

use super::{CallContextFactory, HostedClient};

const OWNER_GENESIS_FIXTURE_HEX: &str = "0a380a10222222222222222222222222222222221224080112208a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c12640a20def88318e44a809464c1022f22230567bae6805d17b1ccfc2bebe5326232c58a1240bfe677c0b6fec8d28e379f584f36dee7258d834222f9b75f61dc75b7db2d836d76d4fb6eaf9e7f561925b2e6882b51eadaf3ec77c565f5b638ad0febfc8cd304";

fn owner_genesis_fixture() -> SignedSpoolOwnerGenesis {
    let bytes = hex::decode(OWNER_GENESIS_FIXTURE_HEX).expect("published v2 fixture hex");
    SignedSpoolOwnerGenesis::decode(bytes.as_slice()).expect("published v2 fixture genesis")
}

pub(crate) async fn start() -> (HostedClient, JoinHandle<()>) {
    start_inner(None).await
}

pub(crate) async fn start_with_remote_state(
    remote_state: StateId,
) -> (HostedClient, JoinHandle<()>) {
    start_inner(Some(PullFixture {
        remote_state,
        pack: None,
    }))
    .await
}

pub(crate) async fn start_with_pull_pack(
    remote_state: StateId,
    pack_data: Vec<u8>,
    index_data: Vec<u8>,
) -> (HostedClient, JoinHandle<()>) {
    start_inner(Some(PullFixture {
        remote_state,
        pack: Some((pack_data, index_data)),
    }))
    .await
}

#[derive(Clone)]
struct PullFixture {
    remote_state: StateId,
    pack: Option<(Vec<u8>, Vec<u8>)>,
}

async fn start_inner(pull: Option<PullFixture>) -> (HostedClient, JoinHandle<()>) {
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
            tokio::spawn(serve_call(send, recv, pull.clone()));
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
            send.write_chunk(Bytes::from(encode_success_response(&[]).unwrap()))
                .await
                .unwrap();
        }
        StreamingShape::ServerStreaming => {
            let body = terminal_page(&method);
            send.write_chunk(Bytes::from(encode_stream_message(&body).unwrap()))
                .await
                .unwrap();
        }
        StreamingShape::Bidirectional => {
            if method == "/heddle.api.v1alpha1.RepoSyncService/Push" {
                serve_push(send, recv, request.split_off(prelude_len)).await;
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
) {
    let advertised = loop {
        if let Some((frame, consumed)) = decode_stream_frame(&buffered).unwrap() {
            let request = match frame {
                StreamFrame::Message(body) => PushClientFrame::decode(body).unwrap(),
                other => panic!("unexpected push request frame before request: {other:?}"),
            };
            buffered.drain(..consumed);
            if let Some(push_client_frame::Frame::Request(request)) = request.frame {
                break request.objects;
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
