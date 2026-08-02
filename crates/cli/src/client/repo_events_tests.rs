// SPDX-License-Identifier: Apache-2.0

use std::{net::Ipv4Addr, time::Duration};

use api::{
    framing::{encode_stream_failure, encode_stream_message},
    heddle::api::v1alpha1::{CallFailure, CallFailureCode},
};
use iroh::{Endpoint, RelayMode, endpoint::presets};
use prost::Message as _;
use tokio::sync::oneshot;

use super::{RepoEvent, RepoEventClient, RepoEventError, SubscribeRepoEventsRequest};
use crate::hosted_runtime::hosted::{CallContextFactory, HostedClient};

fn request() -> SubscribeRepoEventsRequest {
    SubscribeRepoEventsRequest {
        repo_id: "00000000-0000-0000-0000-000000000001".to_string(),
        thread: "main".to_string(),
        after_event_id: 40,
        event_types: vec!["ref.updated".to_string()],
    }
}

async fn client_for(server: iroh::EndpointAddr) -> RepoEventClient {
    let endpoint = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let hosted =
        HostedClient::connect_addr_with_context(endpoint, server, CallContextFactory::default())
            .await
            .unwrap();
    RepoEventClient::from_hosted_client(hosted)
}

async fn receive_request(connection: &iroh::endpoint::Connection) -> iroh::endpoint::SendStream {
    let (send, mut recv) = connection.accept_bi().await.unwrap();
    recv.read_to_end(64 * 1024).await.unwrap();
    send
}

#[tokio::test]
async fn killing_connection_mid_stream_returns_resumable_error_instead_of_hanging() {
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let (event_received_tx, event_received_rx) = oneshot::channel();
    let server_addr = server.addr();
    let server_task = tokio::spawn(async move {
        let connection = server.accept().await.unwrap().await.unwrap();
        let mut send = receive_request(&connection).await;
        let event = RepoEvent {
            event_id: 41,
            repo_id: "00000000-0000-0000-0000-000000000001".to_string(),
            event_type: "ref.updated".to_string(),
            thread: "main".to_string(),
            ..RepoEvent::default()
        };
        send.write_all(&encode_stream_message(&event.encode_to_vec()).unwrap())
            .await
            .unwrap();
        event_received_rx.await.unwrap();
        connection.close(42u32.into(), b"connection killed by test");
        server.close().await;
    });

    let client = client_for(server_addr).await;
    let mut subscription = client.subscribe(request()).await.unwrap();
    let event = tokio::time::timeout(Duration::from_secs(2), subscription.next())
        .await
        .expect("live event must not hang")
        .unwrap();
    assert_eq!(event.event_id, 41);
    event_received_tx.send(()).unwrap();

    let error = tokio::time::timeout(Duration::from_secs(2), subscription.next())
        .await
        .expect("a killed connection must not silently hang")
        .expect_err("a killed connection must be an explicit error");
    assert!(matches!(
        &error,
        RepoEventError::Disconnected {
            last_event_id: 41,
            ..
        }
    ));
    assert_eq!(error.resume_after_event_id(), Some(41));
    assert_eq!(subscription.resume_request().after_event_id, 41);
    server_task.await.unwrap();
    client.close().await;
}

#[tokio::test]
async fn unreadable_repository_subscription_is_refused() {
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
        let connection = server.accept().await.unwrap().await.unwrap();
        let mut send = receive_request(&connection).await;
        let failure = CallFailure {
            code: CallFailureCode::PermissionDenied as i32,
            message: "access denied".to_string(),
            error: None,
        };
        send.write_all(&encode_stream_failure(&failure).unwrap())
            .await
            .unwrap();
        send.finish().unwrap();
        connection.closed().await;
        server.close().await;
    });

    let client = client_for(server_addr).await;
    let mut subscription = client.subscribe(request()).await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), subscription.next())
        .await
        .expect("permission denial must not look like an empty stream")
        .expect_err("an unreadable repository must be refused");
    assert!(matches!(
        &error,
        RepoEventError::Refused {
            source: super::HostedError::Call {
                code: CallFailureCode::PermissionDenied,
                ..
            }
        }
    ));
    assert!(error.to_string().contains("subscription was refused"));
    client.close().await;
    server_task.await.unwrap();
}
