use std::{
    env,
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

const HOSTED_ENDPOINT_CLOSE_P95_BUDGET: Duration = Duration::from_millis(20);
const DEFAULT_CLOSE_SAMPLE_COUNT: usize = 20;

fn require_release_build() {
    #[cfg(debug_assertions)]
    panic!("hosted endpoint close contract must run with --release");
}

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
#[ignore = "release-only hosted endpoint close performance contract"]
async fn hosted_endpoint_close_release_contract() {
    require_release_build();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let sample_count = env::var("HEDDLE_HOSTED_CLOSE_SAMPLES")
        .map(|value| {
            value
                .parse::<usize>()
                .expect("sample count must be an integer")
        })
        .unwrap_or(DEFAULT_CLOSE_SAMPLE_COUNT);
    assert!(
        sample_count >= 5,
        "close contract requires at least 5 samples"
    );
    let negative_control = match env::var("HEDDLE_HOSTED_CLOSE_NEGATIVE_CONTROL").as_deref() {
        Ok("latency") => true,
        Ok(value) => panic!("unknown HEDDLE_HOSTED_CLOSE_NEGATIVE_CONTROL `{value}`"),
        Err(_) => false,
    };
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
        for _ in 0..sample_count {
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

    let mut close_ms = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        let connection =
            HostedConnection::connect_verified(&descriptor, &cli_shared::ClientConfig::default())
                .await
                .unwrap();
        let endpoint_observer = connection.endpoint.clone();
        let close_started = Instant::now();
        if negative_control {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        connection.close().await;
        close_ms.push(close_started.elapsed().as_secs_f64() * 1_000.0);
        assert!(
            endpoint_observer.is_closed(),
            "successful hosted teardown must close the endpoint before drop"
        );
        drop(connection);
        drop(endpoint_observer);
    }
    server_task.await.unwrap();

    close_ms.sort_by(f64::total_cmp);
    let middle = close_ms.len() / 2;
    let median = if close_ms.len().is_multiple_of(2) {
        (close_ms[middle - 1] + close_ms[middle]) / 2.0
    } else {
        close_ms[middle]
    };
    let p95 = percentile_ms(&close_ms, 95);
    let min = close_ms[0];
    let max = close_ms[close_ms.len() - 1];
    let budget_ms = HOSTED_ENDPOINT_CLOSE_P95_BUDGET.as_secs_f64() * 1_000.0;
    println!(
        "HOSTED_CLOSE samples={sample_count} median_ms={median:.3} p95_ms={p95:.3} min_ms={min:.3} max_ms={max:.3} budget_p95_ms={budget_ms:.3} negative_control={negative_control}"
    );
    assert!(
        p95 <= budget_ms,
        "HOSTED CLOSE GATE RED: p95 {p95:.3} ms > {budget_ms:.3} ms budget"
    );
    println!("HOSTED_CLOSE_GATES green");
}

fn percentile_ms(sorted_values: &[f64], percentile: usize) -> f64 {
    let rank = (sorted_values.len() * percentile).div_ceil(100);
    sorted_values[rank.saturating_sub(1)]
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
async fn direct_only_descriptor_uses_the_normal_connection_path() {
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
        Vec::new(),
        server.addr().ip_addrs().map(ToString::to_string).collect(),
    );
    let server_task = tokio::spawn(async move {
        let connection = server
            .accept()
            .await
            .expect("incoming direct-only connection")
            .await
            .unwrap();
        connection.closed().await;
        server.close().await;
    });

    let connection =
        HostedConnection::connect_verified(&descriptor, &cli_shared::ClientConfig::default())
            .await
            .unwrap();
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
