//! The assembled CLI client must accept the native contract's source frames.
use std::net::Ipv4Addr;

use api::{
    framing::{decode_request_frame, encode_stream_message, encode_success_response},
    heddle::api::v2alpha1 as v2,
};
use iroh::{Endpoint, RelayMode, endpoint::presets};
use prost::Message;

use super::{CallContextFactory, HostedClient};

#[tokio::test]
async fn native_client_accepts_source_frames_above_legacy_control_limit() {
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
            "/heddle.api.v2alpha1.EndpointService/DescribeEndpoint"
        );
        let description = v2::DescribeEndpointResponse {
            endpoint: Some(v2::EndpointRef {
                kind: v2::EndpointKind::Weft as i32,
                public_key: key,
            }),
            supported_packages: vec!["heddle.api.v2alpha1".into()],
            implemented_methods: vec!["/heddle.api.v2alpha1.ContentService/ReadContent".into()],
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
            "/heddle.api.v2alpha1.ContentService/ReadContent"
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
