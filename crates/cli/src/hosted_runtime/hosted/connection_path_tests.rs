use std::{
    net::Ipv4Addr,
    time::{Duration, Instant},
};

use api::{
    HOSTED_ALPN_V1,
    heddle::api::v1alpha1::{EndpointDescriptor, SignedEndpointDescriptor},
    signing::endpoint_descriptor_bytes,
};
use crypto::{Ed25519Signer, Signer};
use iroh::{Endpoint, RelayMode, endpoint::presets};
use n0_watcher::Watcher;

use super::{DescriptorKeyring, VerifiedEndpointDescriptor, connection::HostedConnection};

fn verified_descriptor(
    endpoint_id: iroh::EndpointId,
    relay_urls: Vec<String>,
    direct_addresses: Vec<String>,
) -> VerifiedEndpointDescriptor {
    let signer = Ed25519Signer::generate().unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let descriptor = EndpointDescriptor {
        version: 1,
        endpoint_id: endpoint_id.to_string(),
        relay_urls,
        direct_addresses,
        supported_alpns: vec![HOSTED_ALPN_V1.to_vec()],
        issued_at_unix_millis: now - 1_000,
        expires_at_unix_millis: now + 60_000,
        rotation: None,
    };
    let signed = SignedEndpointDescriptor {
        signature: signer
            .sign(&endpoint_descriptor_bytes(&descriptor))
            .unwrap(),
        descriptor: Some(descriptor),
        key_id: "test-key".to_string(),
    };
    let mut keys = DescriptorKeyring::default();
    keys.insert(
        "test-key",
        signer.public_key().try_into().unwrap(),
        i64::MIN,
        i64::MAX,
    )
    .unwrap();
    keys.verify(&signed, now).unwrap()
}

#[tokio::test]
#[ignore = "manual five-sample loopback transport measurement"]
async fn measure_loopback_transport_setup_and_close() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let descriptor = verified_descriptor(
        server.id(),
        vec![
            "https://usw1-1.relay.n0.iroh.link.".to_string(),
            "https://aps1-1.relay.n0.iroh.link.".to_string(),
            "https://use1-1.relay.n0.iroh.link.".to_string(),
            "https://euc1-1.relay.n0.iroh.link.".to_string(),
        ],
        server.addr().ip_addrs().map(ToString::to_string).collect(),
    );
    let server_task = tokio::spawn(async move {
        for _ in 0..5 {
            let connection = server
                .accept()
                .await
                .expect("incoming connection")
                .await
                .unwrap();
            connection.closed().await;
        }
        server.close().await;
    });

    for sample in 1..=5 {
        let started = Instant::now();
        let connection =
            HostedConnection::connect_verified(&descriptor, &cli_shared::ClientConfig::default())
                .await
                .unwrap();
        let setup = started.elapsed();
        connection.close().await;
        let total = started.elapsed();
        println!(
            "LOOPBACK sample={sample} setup_ms={:.3} close_ms={:.3} total_ms={:.3}",
            setup.as_secs_f64() * 1_000.0,
            (total - setup).as_secs_f64() * 1_000.0,
            total.as_secs_f64() * 1_000.0,
        );
    }
    server_task.await.unwrap();
}

#[tokio::test]
async fn reachable_direct_address_never_initializes_advertised_relays() {
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let descriptor = verified_descriptor(
        server.id(),
        vec!["https://usw1-1.relay.n0.iroh.link.".to_string()],
        server.addr().ip_addrs().map(ToString::to_string).collect(),
    );
    let server_task = tokio::spawn(async move {
        let connection = server
            .accept()
            .await
            .expect("incoming connection")
            .await
            .unwrap();
        connection.closed().await;
        server.close().await;
    });

    let connection =
        HostedConnection::connect_verified(&descriptor, &cli_shared::ClientConfig::default())
            .await
            .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        connection.endpoint.home_relay_status().get().is_empty(),
        "a reachable signed direct address must not initialize a relay transport"
    );
    connection.close().await;
    server_task.await.unwrap();
}

#[tokio::test]
async fn unreachable_direct_address_falls_back_to_signed_relay() {
    use iroh_relay::server::{RelayConfig as RelayServerConfig, Server, ServerConfig};

    let mut relay_config = ServerConfig::default();
    relay_config.relay = Some(RelayServerConfig::new((Ipv4Addr::LOCALHOST, 0)));
    let relay = Server::spawn(relay_config).await.unwrap();
    let relay_url: iroh::RelayUrl = format!("http://{}", relay.http_addr().unwrap())
        .parse()
        .unwrap();
    let server = Endpoint::builder(presets::Minimal)
        .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
        .relay_mode(RelayMode::custom([relay_url.clone()]))
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), server.online())
        .await
        .expect("server should register with the relay");
    let descriptor = verified_descriptor(
        server.id(),
        vec![relay_url.to_string()],
        vec!["127.0.0.1:9".to_string()],
    );
    let server_task = tokio::spawn(async move {
        let connection = server
            .accept()
            .await
            .expect("incoming relay connection")
            .await
            .unwrap();
        connection.closed().await;
        server.close().await;
    });

    let connection = tokio::time::timeout(
        Duration::from_secs(5),
        HostedConnection::connect_verified(&descriptor, &cli_shared::ClientConfig::default()),
    )
    .await
    .expect("relay fallback should connect")
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), connection.endpoint.online())
        .await
        .expect("client should register with the signed relay");
    assert!(
        !connection.endpoint.home_relay_status().get().is_empty(),
        "relay fallback must initialize the signed relay transport"
    );
    connection.close().await;
    server_task.await.unwrap();
    drop(relay);
}
