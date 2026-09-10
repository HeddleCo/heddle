//! Run with HEDDLE_PROFILE=1. One test owns the process-local counter inventory.
use std::time::Duration;

use api::{
    framing,
    heddle::api::v1alpha1::{CallContext, CallFailure, CallFailureCode},
    v2::{
        MethodDescriptor,
        client::{MessageReader, Rpc, RpcTransport},
    },
};
use heddle_thread_api::{
    rpc,
    transport::{Authorize, Error, IrohTransport},
};
use iroh::{Endpoint, RelayMode, endpoint::presets};

struct Public;
impl Authorize for Public {
    async fn context(&self, _: &'static MethodDescriptor, _: &[u8]) -> Result<CallContext, Error> {
        Ok(CallContext::default())
    }
}

#[tokio::test]
#[ignore = "requires HEDDLE_PROFILE=1; exercised explicitly by thread-api CI"]
async fn one_connection_counts_all_rpc_bytes_including_failures_and_cancelled_reads() {
    assert_eq!(
        std::env::var("HEDDLE_PROFILE").as_deref(),
        Ok("1"),
        "run this counter contract with HEDDLE_PROFILE=1"
    );
    let server = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("server address")
        .bind()
        .await
        .expect("server");
    let local = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("client address")
        .bind()
        .await
        .expect("client");
    let (outgoing, incoming) =
        tokio::join!(local.connect(server.addr(), api::HOSTED_ALPN_V1), async {
            server
                .accept()
                .await
                .expect("incoming")
                .await
                .expect("connection")
        });
    let failure = framing::encode_failure_response(&CallFailure {
        code: CallFailureCode::PermissionDenied as i32,
        message: "denied fixture".into(),
        ..Default::default()
    })
    .expect("failure frame");
    let success = framing::encode_success_response(&[1, 2, 3]).expect("success frame");
    let stream = framing::encode_stream_message(&[4, 5, 6, 7]).expect("stream frame");
    let responses = vec![success.clone(), failure.clone(), success.clone()];
    let (prefix_sent, prefix_received) = tokio::sync::oneshot::channel();
    let (continue_send, continue_receive) = tokio::sync::oneshot::channel();
    let service = tokio::spawn(async move {
        let mut received = 0;
        for response in responses {
            let (mut send, mut recv) = incoming.accept_bi().await.expect("unary");
            let request = recv.read_to_end(4096).await.expect("request");
            let frame = framing::decode_request_frame(&request).expect("decode");
            assert_eq!(
                frame.method,
                rpc::EndpointServiceDescribeEndpoint::METHOD.path
            );
            received += request.len();
            send.write_all(&response).await.expect("response");
            send.finish().expect("FIN");
            send.stopped().await.expect("acknowledged");
        }
        let (mut send, mut recv) = incoming.accept_bi().await.expect("observation");
        received += recv
            .read_to_end(4096)
            .await
            .expect("observation request")
            .len();
        send.write_all(&stream[..2]).await.expect("partial header");
        prefix_sent.send(()).expect("notify prefix");
        continue_receive.await.expect("continue");
        send.write_all(&stream[2..]).await.expect("rest of frame");
        send.finish().expect("stream FIN");
        send.stopped().await.expect("stream acknowledged");
        received
    });
    let before = heddle_perf_contract::snapshot();
    let transport = IrohTransport::new(
        outgoing.expect("outgoing"),
        Public,
        4096,
        Duration::from_secs(5),
    )
    .expect("transport");
    for fail in [false, true, false] {
        let result = transport
            .unary(rpc::EndpointServiceDescribeEndpoint::METHOD, vec![])
            .await;
        if fail {
            assert!(
                matches!(result, Err(Error::Remote(f)) if f.code == CallFailureCode::PermissionDenied as i32)
            );
        } else {
            assert_eq!(result.expect("success"), [1, 2, 3]);
        }
    }
    let after_unary = heddle_perf_contract::snapshot();
    assert_eq!(
        after_unary.network_streams_opened - before.network_streams_opened,
        3
    );
    assert_eq!(
        after_unary.network_bytes_received - before.network_bytes_received,
        (2 * success.len() + failure.len()) as u64
    );
    let mut reader = transport
        .observe(rpc::ThreadServiceObserveThread::METHOD, vec![])
        .await
        .expect("observe on same connection");
    prefix_received.await.expect("prefix sent");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), reader.next())
            .await
            .is_err()
    );
    let partial = heddle_perf_contract::snapshot();
    assert_eq!(
        partial.network_bytes_received - after_unary.network_bytes_received,
        2,
        "cancelled read retains its consumed byte count"
    );
    continue_send.send(()).expect("resume server");
    assert_eq!(
        reader.next().await.expect("frame").expect("message"),
        [4, 5, 6, 7]
    );
    assert!(reader.next().await.expect("FIN").is_none());
    let sent = service.await.expect("service");
    let after = heddle_perf_contract::snapshot();
    assert_eq!(
        after.network_streams_opened - before.network_streams_opened,
        4
    );
    assert_eq!(
        after.network_bytes_sent - before.network_bytes_sent,
        sent as u64
    );
    assert_eq!(
        after.network_bytes_received - before.network_bytes_received,
        (2 * success.len() + failure.len() + 9) as u64
    );
    local.close().await;
    server.close().await;
}
