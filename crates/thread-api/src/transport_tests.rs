// SPDX-License-Identifier: Apache-2.0
use api::{
    heddle::api::v1alpha1::CallContext,
    v2::{
        client::{Client, Rpc},
        rpc,
    },
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
    let mut reader = Reader::new(recv, 1024, Duration::from_secs(5));
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
        assert_eq!(prelude.method, rpc::SyncServiceReplicateThread::METHOD.path);
        assert!(prelude.context.client_operation_id.is_empty());
        let mut requests = Reader::new(recv, 4096, Duration::from_secs(5));
        let open = requests
            .next()
            .await
            .expect("opening frame")
            .expect("opening");
        let opening = ReplicateThreadRequest::decode(open.as_slice()).expect("opening protobuf");
        assert!(matches!(
            opening.body,
            Some(replicate_thread_request::Body::Open(_))
        ));
        send.write_all(
            &framing::encode_stream_message(
                &ReplicateThreadResponse {
                    body: Some(replicate_thread_response::Body::Ready(
                        ReplicationReady::default(),
                    )),
                }
                .encode_to_vec(),
            )
            .expect("ready frame"),
        )
        .await
        .expect("ready before client FIN");
        let have = requests.next().await.expect("have frame").expect("have");
        assert!(matches!(
            ReplicateThreadRequest::decode(have.as_slice())
                .expect("have protobuf")
                .body,
            Some(replicate_thread_request::Body::Have(_))
        ));
        assert!(requests.next().await.expect("half-close").is_none());
        send.write_all(
            &framing::encode_stream_message(
                &ReplicateThreadResponse {
                    body: Some(replicate_thread_response::Body::Receipt(
                        ReplicationReceipt {
                            accepted_operation_ids: vec![vec![1; 32]],
                            ..Default::default()
                        },
                    )),
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
    let client = Client::new(
        transport,
        [rpc::SyncServiceReplicateThread::METHOD.path.into()],
    );
    let (mut input, mut output) = client
        .exchange::<rpc::SyncServiceReplicateThread>(&ReplicateThreadRequest {
            body: Some(replicate_thread_request::Body::Open(
                ReplicationOpen::default(),
            )),
        })
        .await
        .expect("exchange");
    assert!(matches!(
        output.next().await.expect("ready").expect("message").body,
        Some(replicate_thread_response::Body::Ready(_))
    ));
    input
        .send(&ReplicateThreadRequest {
            body: Some(replicate_thread_request::Body::Have(
                ReplicationHave::default(),
            )),
        })
        .await
        .expect("have");
    input.finish().await.expect("request FIN");
    assert!(
        matches!(output.next().await.expect("receipt").expect("message").body, Some(replicate_thread_response::Body::Receipt(receipt)) if receipt.accepted_operation_ids == vec![vec![1; 32]])
    );
    assert!(output.next().await.expect("server FIN").is_none());
    service.await.expect("contract peer task");
    local.close().await;
    server.close().await;
}

#[tokio::test]
async fn a_quiet_live_stream_stays_open_past_the_frame_progress_timeout() {
    let (server, local, outgoing, incoming) = endpoints().await;
    let (mut send, _) = outgoing.open_bi().await.expect("stream");
    send.write_all(&framing::encode_stream_message(&[1]).expect("frame"))
        .await
        .expect("opening response");
    let (_, recv) = incoming.accept_bi().await.expect("receive");
    let mut reader = Reader::for_method(
        recv,
        1024,
        Duration::from_millis(80),
        rpc::ThreadServiceObserveThread::METHOD,
    );
    assert_eq!(reader.next().await.expect("initial frame"), Some(vec![1]));
    assert!(
        tokio::time::timeout(Duration::from_millis(240), reader.next())
            .await
            .is_err(),
        "waiting for the next live event is not stalled frame progress"
    );
    send.write_all(&framing::encode_stream_message(&[2]).expect("frame"))
        .await
        .expect("later event");
    assert_eq!(
        reader.next().await.expect("live stream survived idle"),
        Some(vec![2])
    );
    reader.cancel();
    local.close().await;
    server.close().await;
}

#[tokio::test]
async fn partial_frame_deadline_survives_canceling_and_resuming_next() {
    let (server, local, outgoing, incoming) = endpoints().await;
    let (mut send, _) = outgoing.open_bi().await.expect("stream");
    send.write_all(&framing::encode_stream_message(&[1]).expect("frame"))
        .await
        .expect("opening response");
    let (_, recv) = incoming.accept_bi().await.expect("receive");
    let mut reader = Reader::for_method(
        recv,
        1024,
        Duration::from_millis(250),
        rpc::ThreadServiceObserveThread::METHOD,
    );
    assert_eq!(reader.next().await.expect("first frame"), Some(vec![1]));
    send.write_all(&[0, 0]).await.expect("start next frame");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), reader.next())
            .await
            .is_err()
    );
    assert_eq!(
        reader.buffer,
        [0, 0],
        "cancel after consuming the frame prefix"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    send.write_all(&[0, 0, 1, 2])
        .await
        .expect("late completion");
    assert!(
        matches!(reader.next().await, Err(Error::Timeout)),
        "a new next() call cannot restart a partially consumed frame's deadline"
    );
    local.close().await;
    server.close().await;
}

#[tokio::test]
async fn the_initial_stream_response_still_requires_timely_progress() {
    let (server, local, outgoing, incoming) = endpoints().await;
    let (mut send, recv) = outgoing.open_bi().await.expect("stream");
    send.write_all(&[1]).await.expect("request opens stream");
    let (_response, _request) = incoming
        .accept_bi()
        .await
        .expect("server holds response open");
    let mut reader = Reader::for_method(
        recv,
        1024,
        Duration::from_millis(80),
        rpc::ThreadServiceObserveThread::METHOD,
    );
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), reader.next())
            .await
            .expect("initial response wait is bounded"),
        Err(Error::Timeout)
    ));
    local.close().await;
    server.close().await;
}

#[tokio::test]
async fn finite_content_streams_keep_progress_deadlines_between_frames() {
    let (server, local, outgoing, incoming) = endpoints().await;
    let (mut send, _) = outgoing.open_bi().await.expect("stream");
    send.write_all(&framing::encode_stream_message(&[1]).expect("frame"))
        .await
        .expect("content frame");
    let (_, recv) = incoming.accept_bi().await.expect("receive");
    let mut reader = Reader::for_method(
        recv,
        1024,
        Duration::from_millis(80),
        rpc::ContentServiceReadContent::METHOD,
    );
    assert_eq!(
        reader.next().await.expect("first content frame"),
        Some(vec![1])
    );
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), reader.next())
            .await
            .expect("finite content stream deadline"),
        Err(Error::Timeout)
    ));
    local.close().await;
    server.close().await;
}
