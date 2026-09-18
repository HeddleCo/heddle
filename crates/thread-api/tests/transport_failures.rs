//! Accepted native streams preserve typed terminal errors before or after data.
#![cfg(feature = "iroh")]

use std::time::Duration;

use api::{
    heddle::api::common::{CallFailure, CallFailureCode},
    v2::client::{MessageReader, MessageWriter, Rpc},
};
use heddle_thread_api::{
    rpc,
    transport::{Error, accepted_stream},
};
use iroh::{Endpoint, RelayMode, endpoint::presets};

#[tokio::test]
async fn accepted_failure_is_bounded_typed_and_terminal_before_or_after_data() {
    let server = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("server address")
        .bind()
        .await
        .expect("server endpoint");
    let client = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("client address")
        .bind()
        .await
        .expect("client endpoint");
    let (outgoing, incoming) =
        tokio::join!(client.connect(server.addr(), api::HOSTED_ALPN_V1), async {
            server
                .accept()
                .await
                .expect("incoming")
                .await
                .expect("connection")
        });
    let outgoing = outgoing.expect("outgoing");
    for after_data in [false, true] {
        let (send, recv) = outgoing.open_bi().await.expect("request stream");
        let (mut request, mut response) = accepted_stream(
            send,
            recv,
            1024,
            Duration::from_secs(5),
            rpc::SyncServiceFetch::METHOD,
        )
        .expect("client framing");
        request.send(vec![]).await.expect("opening frame");
        request.finish().await.expect("request FIN");
        let (send, recv) = incoming.accept_bi().await.expect("accepted request");
        let (mut writer, mut reader) = accepted_stream(
            send,
            recv,
            1024,
            Duration::from_secs(5),
            rpc::SyncServiceFetch::METHOD,
        )
        .expect("server framing");
        assert_eq!(reader.next().await.expect("opening"), Some(vec![]));
        assert!(reader.next().await.expect("request end").is_none());
        let mut failure = CallFailure {
            code: CallFailureCode::NotFound as i32,
            message: "source unavailable".into(),
            ..Default::default()
        };
        let message = failure.message.clone();
        failure.message = "x".repeat(1025);
        assert!(matches!(
            writer.fail(&failure).await,
            Err(Error::Protocol("failure exceeds frame budget"))
        ));
        failure.message = message;
        if after_data {
            writer
                .send(vec![1, 2, 3])
                .await
                .expect("data before failure");
            assert_eq!(response.next().await.expect("data"), Some(vec![1, 2, 3]));
        }
        writer.fail(&failure).await.expect("terminal typed failure");
        assert!(
            writer.send(vec![4]).await.is_err(),
            "closed response rejects later data"
        );
        let error = response.next().await.expect_err("typed terminal failure");
        assert!(
            matches!(error, Error::Remote(ref value)
            if value.code == CallFailureCode::NotFound as i32 && value.message == failure.message),
            "failure was replaced or lost: {error:?}"
        );
        assert!(
            response
                .next()
                .await
                .expect("fused terminal reader")
                .is_none()
        );
    }
    client.close().await;
    server.close().await;
}
