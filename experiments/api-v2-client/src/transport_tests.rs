// SPDX-License-Identifier: Apache-2.0
use api::v2::{
    client::{Client, Rpc},
    rpc,
};
use iroh::{Endpoint, RelayMode, endpoint::presets};

use super::*;
use crate::contract::*;

async fn endpoints() -> (Endpoint, Endpoint, Connection, Connection) {
    let server = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind address")
        .bind()
        .await
        .expect("server endpoint");
    let local = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind address")
        .bind()
        .await
        .expect("client endpoint");
    let (outgoing, incoming) =
        tokio::join!(local.connect(server.addr(), api::HOSTED_ALPN_V1), async {
            server
                .accept()
                .await
                .expect("incoming")
                .await
                .expect("connection")
        });
    (server, local, outgoing.expect("outgoing"), incoming)
}

#[tokio::test]
async fn cancelling_next_preserves_a_partially_consumed_frame() {
    let (server, local, outgoing, incoming) = endpoints().await;
    let (mut send, _) = outgoing.open_bi().await.expect("stream");
    send.write_all(&[0, 0]).await.expect("partial header");
    let (_, recv) = incoming.accept_bi().await.expect("receive");
    let mut reader = Reader {
        recv,
        frame_limit: 1024,
        timeout: Duration::from_secs(5),
        done: false,
        buffer: vec![],
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(50), reader.next())
            .await
            .is_err()
    );
    assert_eq!(
        reader.buffer,
        [0, 0],
        "test must cancel after consuming the prefix"
    );
    send.write_all(&[0, 0, 3, 1, 2, 3])
        .await
        .expect("remaining header/body");
    send.finish().expect("FIN");
    assert_eq!(
        reader.next().await.expect("complete frame"),
        Some(vec![1, 2, 3])
    );
    assert!(reader.next().await.expect("FIN").is_none());
    local.close().await;
    server.close().await;
}

struct FixtureContext;
impl Authorize for FixtureContext {
    async fn context(&self, _: &'static MethodDescriptor, _: &[u8]) -> Result<CallContext, Error> {
        Ok(CallContext::default())
    }
}

#[tokio::test]
async fn exchange_receives_before_request_fin_and_half_close_keeps_responses_alive() {
    let (server, local, outgoing, incoming) = endpoints().await;
    let service = tokio::spawn(async move {
        let (mut send, mut recv) = incoming.accept_bi().await.expect("exchange");
        let mut bytes = vec![0; 6];
        recv.read_exact(&mut bytes).await.expect("prelude header");
        let size = u16::from_be_bytes([bytes[0], bytes[1]]) as usize
            + u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]) as usize;
        bytes.resize(6 + size, 0);
        recv.read_exact(&mut bytes[6..])
            .await
            .expect("prelude body");
        let (prelude, _) = framing::decode_request_prelude(&bytes)
            .expect("decode prelude")
            .expect("complete prelude");
        assert_eq!(prelude.method, rpc::SyncServicePublish::METHOD.path);
        assert_eq!(prelude.context.client_operation_id, "transfer-1");
        let mut requests = Reader {
            recv,
            frame_limit: 4096,
            timeout: Duration::from_secs(5),
            done: false,
            buffer: vec![],
        };
        let open = requests
            .next()
            .await
            .expect("opening frame")
            .expect("opening");
        let opening = PublishClientFrame::decode(open.as_slice()).expect("opening protobuf");
        assert!(matches!(
            opening.body,
            Some(publish_client_frame::Body::Open(_))
        ));
        send.write_all(
            &framing::encode_stream_message(
                &PublishServerFrame {
                    body: Some(publish_server_frame::Body::Ready(TransferReady::default())),
                }
                .encode_to_vec(),
            )
            .expect("ready frame"),
        )
        .await
        .expect("ready before client FIN");
        let commit = requests
            .next()
            .await
            .expect("commit frame")
            .expect("commit");
        assert!(matches!(
            PublishClientFrame::decode(commit.as_slice())
                .expect("commit protobuf")
                .body,
            Some(publish_client_frame::Body::Commit(_))
        ));
        assert!(requests.next().await.expect("half-close").is_none());
        send.write_all(
            &framing::encode_stream_message(
                &PublishServerFrame {
                    body: Some(publish_server_frame::Body::Receipt(PublicationReceipt {
                        client_operation_id: "transfer-1".into(),
                        ..Default::default()
                    })),
                }
                .encode_to_vec(),
            )
            .expect("receipt frame"),
        )
        .await
        .expect("response after request FIN");
        send.finish().expect("server FIN");
        // Hold the connection until the client consumes its final response.
        send.stopped().await.expect("response acknowledged");
    });
    let transport = IrohTransport::new(outgoing, FixtureContext, 4096, Duration::from_secs(5))
        .expect("transport");
    let client = Client::new(transport, [rpc::SyncServicePublish::METHOD.path.into()]);
    let (mut input, mut output) = client
        .exchange::<rpc::SyncServicePublish>(&PublishClientFrame {
            client_operation_id: "transfer-1".into(),
            body: Some(publish_client_frame::Body::Open(PublishOpen::default())),
        })
        .await
        .expect("exchange");
    assert!(matches!(
        output.next().await.expect("ready").expect("message").body,
        Some(publish_server_frame::Body::Ready(_))
    ));
    input
        .send(&PublishClientFrame {
            client_operation_id: String::new(),
            body: Some(publish_client_frame::Body::Commit(TransferCommit::default())),
        })
        .await
        .expect("commit");
    input.finish().await.expect("request FIN");
    assert!(
        matches!(output.next().await.expect("receipt").expect("message").body, Some(publish_server_frame::Body::Receipt(receipt)) if receipt.client_operation_id == "transfer-1")
    );
    assert!(output.next().await.expect("server FIN").is_none());
    service.await.expect("contract peer task");
    local.close().await;
    server.close().await;
}

#[test]
fn remote_failure_retains_typed_details_without_boxing_the_error_path() {
    use api::heddle::api::v1alpha1::{ErrorDetail, ErrorReason};
    let detail = ErrorDetail {
        reason: ErrorReason::PolicyDenied as i32,
        resource: "thread".into(),
        ..Default::default()
    };
    let failure = RemoteFailure::from(CallFailure {
        code: 7,
        message: "review required".into(),
        error: Some(detail.clone()),
    });
    assert_eq!(failure.detail().expect("typed detail"), Some(detail));
    assert_eq!(failure.message, "review required");
}
