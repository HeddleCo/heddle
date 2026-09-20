use std::{
    env,
    net::Ipv4Addr,
    time::{Duration, Instant},
};

use api::{
    framing::{ResponseFrame, decode_response_frame, encode_request_frame},
    heddle::api::common::{CallContext, CallFailureCode},
};
use iroh::{Endpoint, RelayMode, endpoint::presets};

use super::{
    claim_protocol::{CLAIM_PREPARE_METHOD, NATIVE_ALPN},
    connection::HostedConnection,
};

const HOSTED_ENDPOINT_CLOSE_P95_BUDGET: Duration = Duration::from_millis(20);
const DEFAULT_CLOSE_SAMPLE_COUNT: usize = 20;

struct HeddleHomeEnvGuard {
    previous: Option<std::ffi::OsString>,
    _home: tempfile::TempDir,
}

impl HeddleHomeEnvGuard {
    fn isolated() -> Self {
        let home = tempfile::TempDir::new().expect("temp Heddle home");
        let previous = std::env::var_os("HEDDLE_HOME");
        unsafe {
            std::env::set_var("HEDDLE_HOME", home.path());
        }
        Self {
            previous,
            _home: home,
        }
    }
}

impl Drop for HeddleHomeEnvGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var("HEDDLE_HOME", value) },
            None => unsafe { std::env::remove_var("HEDDLE_HOME") },
        }
    }
}

fn require_release_build() {
    #[cfg(debug_assertions)]
    panic!("hosted endpoint close contract must run with --release");
}

async fn connect_loopback(address: iroh::EndpointAddr) -> std::sync::Arc<HostedConnection> {
    let client = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    HostedConnection::connect(client, address).await.unwrap()
}

#[tokio::test]
#[ignore = "release-only hosted endpoint close performance contract"]
// reason: `lock_test_env` is a process-global serialization mutex (payload
// `()`) held across the whole async scenario so no other test mutates
// HEDDLE_HOME/credentials concurrently. Each `#[tokio::test]` runs on its own
// runtime, so nothing else contends for the guard and it cannot deadlock.
#[allow(clippy::await_holding_lock)]
async fn hosted_endpoint_close_release_contract() {
    let _process_env_guard = crate::test_process_env::exclusive().await;
    let _env_guard = config::credentials::lock_test_env();
    let _home = HeddleHomeEnvGuard::isolated();
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
    let server_addr = server.addr();
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
        let connection = connect_loopback(server_addr.clone()).await;
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
// reason: `lock_test_env` is a process-global serialization mutex (payload
// `()`) held across the whole async scenario so no other test mutates
// HEDDLE_HOME/credentials concurrently. Each `#[tokio::test]` runs on its own
// runtime, so nothing else contends for the guard and it cannot deadlock.
#[allow(clippy::await_holding_lock)]
async fn direct_only_descriptor_uses_the_normal_connection_path() {
    let _process_env_guard = crate::test_process_env::exclusive().await;
    let _env_guard = config::credentials::lock_test_env();
    let _home = HeddleHomeEnvGuard::isolated();
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
        let connection = server
            .accept()
            .await
            .expect("incoming direct-only connection")
            .await
            .unwrap();
        connection.closed().await;
        server.close().await;
    });

    let connection = connect_loopback(server_addr).await;
    connection.close().await;
    server_task.await.unwrap();
}

#[tokio::test]
// reason: `lock_test_env` is a process-global serialization mutex (payload
// `()`) held across the whole async scenario so no other test mutates
// HEDDLE_HOME/credentials concurrently. Each `#[tokio::test]` runs on its own
// runtime, so nothing else contends for the guard and it cannot deadlock.
#[allow(clippy::await_holding_lock)]
async fn hosted_connection_accepts_claim_alpn() {
    let _process_env_guard = crate::test_process_env::exclusive().await;
    let _env_guard = config::credentials::lock_test_env();
    let _home = HeddleHomeEnvGuard::isolated();
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
        let connection = server
            .accept()
            .await
            .expect("incoming hosted connection")
            .await
            .unwrap();
        connection.closed().await;
        server.close().await;
    });

    let connection = connect_loopback(server_addr).await;

    let claim_client = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind_addr((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let claim_connection = claim_client
        .connect(connection.endpoint.addr(), NATIVE_ALPN)
        .await
        .expect("claim ALPN connection");
    let (mut send, mut recv) = claim_connection.open_bi().await.unwrap();
    let frame = encode_request_frame(CLAIM_PREPARE_METHOD, &CallContext::default(), b"resolve")
        .expect("claim request frame");
    send.write_all(&frame).await.unwrap();
    send.finish().unwrap();
    let response = recv.read_to_end(64 * 1024).await.unwrap();
    let ResponseFrame::Failure(failure) = decode_response_frame(&response).unwrap() else {
        panic!("missing Biscuit authentication must be refused");
    };
    assert_eq!(failure.code, CallFailureCode::Unauthenticated as i32);

    claim_client.close().await;
    connection.close().await;
    server_task.await.unwrap();
}
