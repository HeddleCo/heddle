//! The assembled CLI client must accept the native contract's source frames.
use std::net::Ipv4Addr;

use api::{
    framing::{decode_request_frame, encode_stream_message, encode_success_response},
    heddle::api::v1alpha2 as v2,
};
use iroh::{Endpoint, RelayMode, endpoint::presets};
use prost::Message;

use super::{CallContextFactory, HostedClient};

#[tokio::test]
async fn native_client_accepts_source_frames_above_legacy_control_limit() {
    let _process_env_guard = crate::test_process_env::shared().await;
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("server bind address")
        .bind()
        .await
        .expect("server endpoint");
    let address = server.addr();
    let key = server.id().as_bytes().to_vec();
    let payload = vec![7; 500 * 1024];
    let expected = payload.clone();
    let task = tokio::spawn(async move {
        let connection = server
            .accept()
            .await
            .expect("client")
            .await
            .expect("connection");
        let (mut send, mut recv) = connection.accept_bi().await.expect("discovery");
        let request = recv
            .read_to_end(1024 * 1024)
            .await
            .expect("discovery request");
        assert_eq!(
            decode_request_frame(&request).expect("request").method,
            "/heddle.api.v1alpha2.EndpointService/DescribeEndpoint"
        );
        let description = v2::DescribeEndpointResponse {
            endpoint: Some(v2::EndpointRef {
                kind: v2::EndpointKind::Weft as i32,
                public_key: key,
            }),
            supported_packages: vec!["heddle.api.v1alpha2".into()],
            implemented_methods: vec!["/heddle.api.v1alpha2.ContentService/ReadContent".into()],
            ..Default::default()
        };
        send.write_all(
            &encode_success_response(&description.encode_to_vec()).expect("description frame"),
        )
        .await
        .expect("send discovery");
        send.finish().expect("discovery FIN");
        let (mut send, mut recv) = connection.accept_bi().await.expect("content read");
        let request = recv
            .read_to_end(1024 * 1024)
            .await
            .expect("content request");
        assert_eq!(
            decode_request_frame(&request).expect("request").method,
            "/heddle.api.v1alpha2.ContentService/ReadContent"
        );
        let event = v2::ContentEvent {
            payload: Some(v2::content_event::Payload::Blob(v2::BlobChunk {
                data: payload,
                ..Default::default()
            })),
            ..Default::default()
        };
        send.write_all(&encode_stream_message(&event.encode_to_vec()).expect("content frame"))
            .await
            .expect("send native source frame");
        send.finish().expect("content FIN");
        connection.closed().await;
        server.close().await;
    });
    let local = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("client address")
        .bind()
        .await
        .expect("client endpoint");
    let client =
        HostedClient::connect_addr_with_context(local, address, CallContextFactory::default())
            .await
            .expect("assembled hosted client");
    let remote = client.native().await.expect("native discovery");
    let mut messages = remote
        .api
        .observe::<thread_api::rpc::ContentServiceReadContent>(&v2::ReadContentRequest::default())
        .await
        .expect("content stream");
    let event = messages
        .next()
        .await
        .expect("native frame within supported budget")
        .expect("content");
    let Some(v2::content_event::Payload::Blob(chunk)) = event.payload else {
        panic!("blob chunk");
    };
    assert_eq!(chunk.data, expected);
    drop(messages);
    drop(remote);
    client.close().await;
    task.await.expect("server finished");
}

// The peer holds a once observation open at each seam, including the next page.
async fn stalled_collaboration_peer(
    stall: usize,
) -> (
    HostedClient,
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("bind")
        .bind()
        .await
        .expect("server");
    let address = server.addr();
    let key = server.id().as_bytes().to_vec();
    let (release, mut released) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let connection = server
            .accept()
            .await
            .expect("client")
            .await
            .expect("connection");
        let (mut send, mut recv) = connection.accept_bi().await.expect("discovery");
        recv.read_to_end(65536).await.expect("discovery request");
        let description = v2::DescribeEndpointResponse {
            endpoint: Some(v2::EndpointRef {
                kind: v2::EndpointKind::Weft as i32,
                public_key: key.clone(),
            }),
            supported_packages: vec!["heddle.api.v1alpha2".into()],
            implemented_methods: vec![
                "/heddle.api.v1alpha2.CollaborationService/ObserveCollaboration".into(),
            ],
            default_read_budget: Some(v2::ReadBudget {
                max_items: 64,
                max_frame_bytes: 65536,
                max_snapshot_bytes: 1048576,
            }),
            max_pending_batch_bytes: 1048576,
            ..Default::default()
        };
        send.write_all(
            &encode_success_response(&description.encode_to_vec()).expect("description"),
        )
        .await
        .expect("discovery response");
        send.finish().expect("FIN");
        for page in 0..=1 {
            let (mut send, mut recv) = connection.accept_bi().await.expect("observation");
            let bytes = recv.read_to_end(65536).await.expect("request");
            let request = v2::ObserveCollaborationRequest::decode(
                decode_request_frame(&bytes).expect("frame").body,
            )
            .expect("observation request");
            if page == 1 {
                assert_eq!(request.page.expect("page").after_page, vec![7]);
            }
            let bodies = [
                v2::stream_frame::Body::Open(v2::StreamOpen {
                    source: Some(v2::EndpointRef {
                        kind: v2::EndpointKind::Weft as i32,
                        public_key: key.clone(),
                    }),
                    binding_digest: vec![9; 32],
                    accepted_budget: Some(v2::ReadBudget {
                        max_items: 64,
                        max_frame_bytes: 65536,
                        max_snapshot_bytes: 1048576,
                    }),
                    ..Default::default()
                }),
                v2::stream_frame::Body::Checkpoint(v2::StreamCheckpoint {
                    cursor: vec![1],
                    snapshot_complete: true,
                    page: Some(v2::PageInfo {
                        exhausted: stall != 3,
                        next_page: if stall == 3 { vec![7] } else { vec![] },
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                v2::stream_frame::Body::Complete(v2::StreamComplete { cursor: vec![1] }),
            ];
            for (index, body) in bodies.into_iter().enumerate() {
                if (stall < 3 && index == stall) || (stall == 3 && page == 1 && index == 0) {
                    // Keep the connection and response stream alive until the
                    // test releases it; absence of Complete must be bounded.
                    let _ = (&mut released).await;
                }
                let event = v2::CollaborationEvent {
                    frame: Some(v2::StreamFrame {
                        sequence: index as u64 + 1,
                        body: Some(body),
                    }),
                    ..Default::default()
                };
                if send
                    .write_all(&encode_stream_message(&event.encode_to_vec()).expect("frame"))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            let _ = send.finish();
            if stall != 3 {
                break;
            }
        }
        connection.closed().await;
        server.close().await;
    });
    let local = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .expect("bind")
        .bind()
        .await
        .expect("client");
    let client =
        HostedClient::connect_addr_with_context(local, address, CallContextFactory::default())
            .await
            .expect("hosted client");
    client.native().await.expect("discovery");
    (client, task, release)
}

async fn assert_once_observation_stall_is_bounded(stall: usize) {
    let _guard = crate::test_process_env::shared().await;
    let (client, server, _release) = stalled_collaboration_peer(stall).await;
    let request = v2::ObserveCollaborationRequest {
        observe: Some(v2::ObserveOptions {
            mode: v2::ObservationMode::Once as i32,
            ..Default::default()
        }),
        ..Default::default()
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.observe_collaboration_events_with_timeout(
            request,
            std::time::Duration::from_millis(100),
        ),
    )
    .await;
    client.close().await;
    server.abort();
    let error = result
        .expect("once observation exceeded its no-progress bound")
        .expect_err("stalled peer must fail");
    assert!(
        error
            .to_string()
            .contains("collaboration observation timed out"),
        "{error}"
    );
}

#[tokio::test]
async fn once_observation_bounds_opening() {
    assert_once_observation_stall_is_bounded(0).await;
}
#[tokio::test]
async fn once_observation_bounds_open_without_complete() {
    assert_once_observation_stall_is_bounded(1).await;
}
#[tokio::test]
async fn once_observation_bounds_checkpoint_without_complete() {
    assert_once_observation_stall_is_bounded(2).await;
}
#[tokio::test]
async fn once_observation_bounds_next_page() {
    assert_once_observation_stall_is_bounded(3).await;
}

#[tokio::test]
async fn live_observation_keeps_long_lived_semantics() {
    let _guard = crate::test_process_env::shared().await;
    let (client, server, release) = stalled_collaboration_peer(1).await;
    let request = v2::ObserveCollaborationRequest {
        observe: Some(v2::ObserveOptions {
            mode: v2::ObservationMode::Follow as i32,
            ..Default::default()
        }),
        ..Default::default()
    };
    {
        let mut observation = std::pin::pin!(client.observe_collaboration_events_with_timeout(
            request,
            std::time::Duration::from_millis(100)
        ));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut observation)
                .await
                .is_err()
        );
        release.send(()).expect("release peer");
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut observation)
            .await
            .expect("released")
            .expect("live observation completes");
    }
    client.close().await;
    server.abort();
}
