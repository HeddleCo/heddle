// SPDX-License-Identifier: Apache-2.0
//! Integration test for `heddle presence publish`.
//!
//! Spins up a throwaway TCP listener that speaks the WebSocket protocol via
//! `tokio-tungstenite::accept_async`, captures the first few client frames,
//! then drives the publisher via `run_publisher` directly. The publisher's
//! `ws_url` is pointed at the ephemeral listener.
//!
//! This deliberately avoids spawning the full binary — the connection loop
//! is a pure async fn, so exercising it in-process is faster and more
//! informative on failure. If we later need a black-box test (verifying
//! the clap wiring end-to-end) we can add one; for now this gives us a
//! sharp assertion that the hello + first publish frames match the on-wire
//! schema the server expects.

#![cfg(all(feature = "weft-client", feature = "local"))]

use std::{net::SocketAddr, time::Duration};

use cli::cli::commands::{PublisherConfig, run_publisher};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::{net::TcpListener, sync::mpsc, task::JoinHandle};
use tokio_tungstenite::tungstenite::Message;

async fn spawn_stub_server() -> (SocketAddr, mpsc::UnboundedReceiver<Value>, JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        // Accept a single connection.
        let (stream, _peer) = listener.accept().await.expect("accept");
        let mut ws = tokio_tungstenite::accept_async(stream)
            .await
            .expect("ws handshake");

        // Send a benign ready so the publisher proceeds.
        ws.send(Message::Text(
            serde_json::json!({"type":"ready","subscribed":[]}).to_string(),
        ))
        .await
        .ok();

        while let Some(msg) = ws.next().await {
            let Ok(msg) = msg else { break };
            if let Message::Text(t) = msg
                && let Ok(value) = serde_json::from_str::<Value>(&t)
                && tx.send(value).is_err()
            {
                break;
            }
        }
    });

    // Give the listener a beat.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, rx, handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn publishes_hello_then_agent_start_frame() {
    let (addr, mut rx, _server) = spawn_stub_server().await;

    let config = PublisherConfig {
        session_id: "agent-test".into(),
        subject: "alice".into(),
        namespace: "heddle/core".into(),
        model: Some("claude-sonnet-4-6".into()),
        provider: Some("anthropic".into()),
        // Opaque token — the stub server ignores auth entirely.
        token: "test-token".into(),
        ws_url: format!("ws://{addr}/presence/ws"),
        interval: Duration::from_secs(60),
    };

    let task = tokio::spawn(run_publisher(config));

    // First frame should be `hello`.
    let hello = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timeout waiting for hello")
        .expect("channel closed before hello");
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["role"], "cli");

    // Second frame should be `publish` with `agent_start`.
    let publish = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timeout waiting for publish")
        .expect("channel closed before publish");
    assert_eq!(publish["type"], "publish");
    assert_eq!(publish["event"]["kind"], "agent_start");
    assert_eq!(publish["event"]["session_id"], "agent-test");
    assert_eq!(publish["event"]["subject"], "alice");
    assert_eq!(publish["event"]["namespace"], "heddle/core");
    assert_eq!(publish["event"]["model"], "claude-sonnet-4-6");
    assert_eq!(publish["event"]["provider"], "anthropic");
    assert!(publish["event"]["started_at_ms"].is_u64());

    // Shutdown — abort the publisher; the stub server closes on its own.
    task.abort();
}