use std::{
    collections::HashMap, future::Future, net::Ipv4Addr, path::PathBuf, sync::Arc, time::Duration,
};

use api::heddle::api::v1alpha2::{
    DescribeEndpointResponse, EndpointKind, EndpointRef, ProviderDialRoute,
};
use config::ClientConfig;
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, endpoint::presets, protocol::Router};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};

use super::{
    HostedError, Result,
    claim_protocol::{ClaimProtocol, NATIVE_ALPN},
    hosted_bridge,
    provider_transport::ProviderWebSocketTransport,
};

/// Foreground budget for graceful endpoint drain.
///
/// Relay paths can spend about one second waiting for a close-frame ACK after
/// the operation is already locally closed. Start the graceful drain, but let
/// one-shot commands return after this bound while the spawned task finishes.
const FOREGROUND_ENDPOINT_DRAIN: Duration = Duration::from_millis(20);

#[cfg(test)]
static NEXT_SHUTDOWN_HOLD_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug)]
pub(super) struct HostedConnection {
    // A transient inbound claim listener on this connection's endpoint.
    // It serves resolve/consent for a browser that reaches this process's
    // endpoint, but it holds no owner-root co-sign consumer: the persisted
    // box network daemon owns that, and the owner-root co-sign is bridged
    // to a foreground signer (heddle#1620). A co-sign dialed here therefore
    // fails closed rather than being performed without a foreground owner.
    router: Router,
    pub(super) endpoint: Endpoint,
    pub(super) connection: iroh::endpoint::Connection,
    pub(super) native_description:
        tokio::sync::OnceCell<api::heddle::api::v1alpha2::DescribeEndpointResponse>,
    provider_transport: Option<ProviderWebSocketTransport>,
    provider_connections:
        Mutex<HashMap<EndpointId, Arc<Mutex<Option<iroh::endpoint::Connection>>>>>,
    /// Local loopback endpoint that adapts the v2 Iroh transport to netd's UDS
    /// stream bridge. `None` for an ordinary direct connection.
    proxy_endpoint: Option<Endpoint>,
    /// Weft's Iroh-authenticated endpoint id when this connection is proxied
    /// through netd. The local adapter's `connection.remote_id()` is the proxy,
    /// so v2 Discover must use this key instead (heddle#1794).
    weft_endpoint_id: Option<EndpointId>,
    reused_warm: bool,
}

impl HostedConnection {
    pub(super) async fn connect(endpoint: Endpoint, address: EndpointAddr) -> Result<Arc<Self>> {
        heddle_perf_contract::record_network_client_initialization();
        Self::connect_inner(endpoint, address, None).await
    }

    /// Wrap a connection already discovered by [`weft_client::HostedClient`].
    pub(super) fn from_discovered(
        endpoint: Endpoint,
        connection: iroh::endpoint::Connection,
        config: &ClientConfig,
        description: DescribeEndpointResponse,
    ) -> Arc<Self> {
        let native_description = tokio::sync::OnceCell::new();
        let _ = native_description.set(description);
        let router = claim_router(endpoint.clone());
        Arc::new(Self {
            native_description,
            router,
            endpoint,
            connection,
            provider_transport: Some(ProviderWebSocketTransport::new(config.clone())),
            provider_connections: Mutex::new(HashMap::new()),
            proxy_endpoint: None,
            weft_endpoint_id: None,
            reused_warm: false,
        })
    }

    async fn connect_inner(
        endpoint: Endpoint,
        address: EndpointAddr,
        provider_transport: Option<ProviderWebSocketTransport>,
    ) -> Result<Arc<Self>> {
        let connection = match endpoint.connect(address, api::HOSTED_ALPN_V1).await {
            Ok(connection) => connection,
            Err(error) => {
                endpoint.close().await;
                return Err(HostedError::transport(error));
            }
        };
        let router = claim_router(endpoint.clone());
        Ok(Arc::new(Self {
            native_description: tokio::sync::OnceCell::new(),
            router,
            endpoint,
            connection,
            provider_transport,
            provider_connections: Mutex::new(HashMap::new()),
            proxy_endpoint: None,
            weft_endpoint_id: None,
            reused_warm: false,
        }))
    }

    /// Reuse netd's persistent endpoint and cached Weft QUIC session while
    /// retaining the v2 client's concrete Iroh transport.
    ///
    /// The local connection terminates at an in-process loopback adapter. Each
    /// v2 RPC stream is spliced to netd over its same-uid UDS bridge. Provider
    /// connections still dial directly from this process's local endpoint.
    #[cfg(unix)]
    pub(super) async fn connect_via_netd(server: &str, config: &ClientConfig) -> Result<Arc<Self>> {
        let socket_path =
            hosted_bridge::hosted_bridge_socket_path(&repo::identity::heddle_home_dir());
        if !socket_path.exists() {
            return Err(HostedError::transport("netd hosted bridge is not running"));
        }
        let ensured =
            hosted_bridge::ensure_via_netd(&socket_path, server, config.allow_insecure).await?;

        let proxy_endpoint = Endpoint::builder(presets::Minimal)
            .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .map_err(HostedError::transport)?
            .bind()
            .await
            .map_err(HostedError::transport)?;
        let proxy_address = proxy_endpoint.addr();
        let proxy_task_endpoint = proxy_endpoint.clone();
        let proxy_socket = socket_path.clone();
        let proxy_server = server.to_string();
        let allow_insecure = config.allow_insecure;
        tokio::spawn(async move {
            if let Err(error) = serve_netd_proxy(
                proxy_task_endpoint,
                proxy_socket,
                proxy_server,
                allow_insecure,
            )
            .await
            {
                tracing::debug!(%error, "local netd hosted proxy stopped");
            }
        });

        let provider_transport = ProviderWebSocketTransport::new(config.clone());
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .map_err(HostedError::transport)?
            .add_custom_transport(Arc::new(provider_transport.clone()))
            .bind()
            .await
            .map_err(HostedError::transport)?;
        heddle_perf_contract::record_network_client_initialization();
        let connection = match endpoint.connect(proxy_address, api::HOSTED_ALPN_V1).await {
            Ok(connection) => connection,
            Err(error) => {
                endpoint.close().await;
                proxy_endpoint.close().await;
                return Err(HostedError::transport(error));
            }
        };
        let router = claim_router(endpoint.clone());
        tracing::debug!(
            reused = ensured.reused,
            netd_node_id = %ensured.node_id,
            weft_endpoint_id = %ensured.weft_endpoint_id,
            local_node_id = %endpoint.id(),
            "hosted connect using netd warm bridge"
        );
        Ok(Arc::new(Self {
            native_description: tokio::sync::OnceCell::new(),
            router,
            endpoint,
            connection,
            provider_transport: Some(provider_transport),
            provider_connections: Mutex::new(HashMap::new()),
            proxy_endpoint: Some(proxy_endpoint),
            weft_endpoint_id: Some(ensured.weft_endpoint_id),
            reused_warm: ensured.reused,
        }))
    }

    /// Endpoint key v2 Discover must prove. Direct sessions use the Iroh peer;
    /// proxied netd sessions use Weft's key from Ensure, not the local adapter.
    pub(super) fn discover_endpoint_key(&self) -> [u8; 32] {
        self.weft_endpoint_id
            .map(|id| *id.as_bytes())
            .unwrap_or_else(|| *self.connection.remote_id().as_bytes())
    }

    pub(super) fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    pub(super) fn reused_warm(&self) -> bool {
        self.reused_warm
    }

    pub(super) async fn native_provider_connection(
        &self,
        provider: &EndpointRef,
        routes: &[ProviderDialRoute],
    ) -> Result<iroh::endpoint::Connection> {
        if provider.kind != EndpointKind::Provider as i32 {
            return Err(HostedError::InvalidDescriptor(
                "native provider endpoint kind is not provider".to_string(),
            ));
        }
        let key: &[u8; 32] = provider.public_key.as_slice().try_into().map_err(|_| {
            HostedError::InvalidDescriptor(
                "native provider endpoint key must be 32 bytes".to_string(),
            )
        })?;
        let endpoint_id = EndpointId::from_bytes(key).map_err(|error| {
            HostedError::InvalidDescriptor(format!("native provider endpoint key: {error}"))
        })?;
        self.connect_provider(endpoint_id, || {
            let transport = self.provider_transport.as_ref().ok_or_else(|| {
                HostedError::InvalidDescriptor(
                    "the active Iroh endpoint has no provider transport".to_string(),
                )
            })?;
            transport.register_routes(provider, routes)
        })
        .await
    }

    async fn connect_provider(
        &self,
        endpoint_id: EndpointId,
        address: impl FnOnce() -> Result<EndpointAddr>,
    ) -> Result<iroh::endpoint::Connection> {
        let slot = {
            let mut connections = self.provider_connections.lock().await;
            Arc::clone(
                connections
                    .entry(endpoint_id)
                    .or_insert_with(|| Arc::new(Mutex::new(None))),
            )
        };
        let mut cached = slot.lock().await;
        if let Some(connection) = cached.as_ref()
            && connection.close_reason().is_none()
        {
            return Ok(connection.clone());
        }

        let connection = self
            .endpoint
            .connect(address()?, api::PROVIDER_ALPN_V1)
            .await
            .map_err(HostedError::transport)?;
        *cached = Some(connection.clone());
        Ok(connection)
    }

    pub(super) async fn close(&self) {
        self.connection.close(0u32.into(), b"Heddle client closed");
        let router = self.router.clone();
        let proxy_endpoint = self.proxy_endpoint.clone();
        bounded_foreground_shutdown(async move {
            if let Err(error) = router.shutdown().await {
                tracing::warn!(%error, "failed to shut down Heddle Iroh router");
            }
            if let Some(endpoint) = proxy_endpoint {
                endpoint.close().await;
            }
        })
        .await;
    }
}

#[cfg(unix)]
async fn serve_netd_proxy(
    endpoint: Endpoint,
    socket_path: PathBuf,
    server: String,
    allow_insecure: bool,
) -> Result<()> {
    let incoming = endpoint
        .accept()
        .await
        .ok_or_else(|| HostedError::transport("local netd proxy endpoint closed before accept"))?;
    let connection = incoming.await.map_err(HostedError::transport)?;
    while let Ok((send, recv)) = connection.accept_bi().await {
        let socket_path = socket_path.clone();
        let server = server.clone();
        tokio::spawn(async move {
            if let Err(error) =
                splice_netd_stream(send, recv, &socket_path, &server, allow_insecure).await
            {
                tracing::debug!(%error, "local netd stream proxy stopped");
            }
        });
    }
    endpoint.close().await;
    Ok(())
}

#[cfg(unix)]
async fn splice_netd_stream(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    socket_path: &std::path::Path,
    server: &str,
    allow_insecure: bool,
) -> Result<()> {
    let (stream, _reused) =
        hosted_bridge::open_bi_via_netd(socket_path, server, allow_insecure).await?;
    let (mut unix_read, mut unix_write) = stream.into_split();
    let to_netd = async {
        while let Some(chunk) = recv
            .read_chunk(64 * 1024)
            .await
            .map_err(HostedError::transport)?
        {
            unix_write
                .write_all(&chunk)
                .await
                .map_err(HostedError::transport)?;
        }
        unix_write.shutdown().await.map_err(HostedError::transport)
    };
    let from_netd = async {
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let read = unix_read
                .read(&mut buffer)
                .await
                .map_err(HostedError::transport)?;
            if read == 0 {
                send.finish().map_err(HostedError::transport)?;
                return Ok(());
            }
            send.write_all(&buffer[..read])
                .await
                .map_err(HostedError::transport)?;
        }
    };
    let (to_netd, from_netd) = tokio::join!(to_netd, from_netd);
    to_netd?;
    from_netd
}

async fn bounded_foreground_shutdown(shutdown: impl Future<Output = ()> + Send + 'static) {
    let hold = shutdown_hold_for_test();
    let mut drain = tokio::spawn(async move {
        if let Some(hold) = hold {
            tokio::time::sleep(hold).await;
        }
        shutdown.await;
    });
    tokio::select! {
        result = &mut drain => {
            if let Err(error) = result {
                tracing::warn!(%error, "hosted endpoint drain task failed");
            }
        }
        () = tokio::time::sleep(FOREGROUND_ENDPOINT_DRAIN) => {
            // Dropping a JoinHandle detaches the task. Be explicit so a future
            // refactor does not replace this with abort-on-timeout behavior.
            std::mem::forget(drain);
        }
    }
}

#[cfg(test)]
pub(super) fn hold_next_shutdown_for_test(duration: Duration) {
    NEXT_SHUTDOWN_HOLD_MS.store(
        duration.as_millis() as u64,
        std::sync::atomic::Ordering::SeqCst,
    );
}

fn shutdown_hold_for_test() -> Option<Duration> {
    #[cfg(test)]
    {
        let millis = NEXT_SHUTDOWN_HOLD_MS.swap(0, std::sync::atomic::Ordering::SeqCst);
        (millis != 0).then(|| Duration::from_millis(millis))
    }
    #[cfg(not(test))]
    {
        None
    }
}

/// Mount a transient claim listener on this connection's endpoint.
///
/// The completion watcher and owner-root call receiver are dropped here
/// on purpose: this endpoint has no foreground signer arming it, so an
/// owner-root co-sign dialed against it fails closed at
/// `StoredClaimAuthorization` (the send finds no receiver) rather than
/// being co-signed without a foreground owner. Persistent, foreground-
/// bridged co-sign is the box network daemon's job (heddle#1620).
fn claim_router(endpoint: Endpoint) -> Router {
    let (authorization, _completion, _owner_root_calls) =
        crate::hosted_runtime::claim_authorization::StoredClaimAuthorization::new();
    let authorization = Arc::new(authorization);
    let endpoint_key = *endpoint.id().as_bytes();
    Router::builder(endpoint)
        .accept(
            NATIVE_ALPN,
            ClaimProtocol::new(Arc::clone(&authorization), authorization, endpoint_key),
        )
        .spawn()
}

impl Drop for HostedConnection {
    fn drop(&mut self) {
        self.connection.close(0u32.into(), b"Heddle client closed");
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::Ipv4Addr,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    use api::heddle::api::v1alpha2::{EndpointKind, EndpointRef};
    use iroh::{Endpoint, RelayMode, endpoint::presets};
    use tokio::sync::Mutex;

    use super::{HostedConnection, bounded_foreground_shutdown, hold_next_shutdown_for_test};

    #[tokio::test]
    async fn bounded_shutdown_detaches_and_keeps_draining() {
        let _process_env_guard = crate::test_process_env::exclusive().await;
        let completed = Arc::new(AtomicBool::new(false));
        let completed_after_drain = Arc::clone(&completed);
        hold_next_shutdown_for_test(Duration::from_millis(80));
        let started = Instant::now();
        bounded_foreground_shutdown(async move {
            completed_after_drain.store(true, Ordering::SeqCst);
        })
        .await;
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "foreground close must detach an 80ms drain"
        );
        assert!(!completed.load(Ordering::SeqCst));
        tokio::time::timeout(Duration::from_millis(200), async {
            while !completed.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached graceful drain must keep running");
    }

    #[tokio::test]
    async fn close_does_not_wait_for_peer_after_locally_closed() {
        let server = Endpoint::builder(presets::Minimal)
            .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let address = server.addr();
        let server_task = tokio::spawn(async move {
            let connection = server
                .accept()
                .await
                .expect("incoming connection")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(3)).await;
            connection.close(0u32.into(), b"test");
            server.close().await;
        });
        let client = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let connection = HostedConnection::connect(client, address).await.unwrap();
        connection
            .connection
            .close(0u32.into(), b"Heddle client closed");
        tokio::time::timeout(Duration::from_secs(2), connection.connection.closed())
            .await
            .expect("QUIC close should become locally closed");

        let started = Instant::now();
        connection.close().await;
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "close after locally closed must not wait for endpoint drain"
        );
        server_task.abort();
        let _ = server_task.await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn netd_proxy_reuses_weft_and_carries_iroh_streams() {
        let _process_env_guard = crate::test_process_env::exclusive().await;
        let fixture =
            crate::hosted_runtime::hosted::hosted_bridge::tests::WarmBridgeFixture::start().await;
        let _home = crate::hosted_runtime::hosted::hosted_bridge::tests::PinHeddleHome::new(
            fixture.home.path(),
        );
        let connection = HostedConnection::connect_via_netd(
            crate::hosted_runtime::hosted::hosted_bridge::tests::TEST_WEFT_SERVER,
            &config::ClientConfig::default(),
        )
        .await
        .expect("connect through warm netd bridge");
        assert!(connection.reused_warm());
        assert_ne!(
            connection.endpoint_id(),
            fixture.node_id,
            "provider-capable endpoint must remain local when Weft is proxied"
        );

        let (mut send, mut recv) = connection.connection.open_bi().await.unwrap();
        send.write_all(b"v2-over-netd").await.unwrap();
        send.finish().unwrap();
        assert_eq!(recv.read_to_end(64 * 1024).await.unwrap(), b"v2-over-netd");
        assert_eq!(
            fixture.accepts(),
            1,
            "netd must reuse its cached QUIC session"
        );
        connection.close().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proxied_discover_rejects_adapter_id_and_accepts_weft_id() {
        let _process_env_guard = crate::test_process_env::exclusive().await;
        let fixture =
            crate::hosted_runtime::hosted::hosted_bridge::tests::WarmBridgeFixture::start_describe()
                .await;
        let _home = crate::hosted_runtime::hosted::hosted_bridge::tests::PinHeddleHome::new(
            fixture.home.path(),
        );
        let connection = HostedConnection::connect_via_netd(
            crate::hosted_runtime::hosted::hosted_bridge::tests::TEST_WEFT_SERVER,
            &config::ClientConfig::default(),
        )
        .await
        .expect("connect through warm netd bridge");
        let adapter_key = *connection.connection.remote_id().as_bytes();
        let weft_key = *fixture.weft_id.as_bytes();
        assert_ne!(
            adapter_key, weft_key,
            "local Iroh↔UDS adapter peer must not be Weft"
        );
        assert_eq!(
            connection.discover_endpoint_key(),
            weft_key,
            "proxied Discover must use Weft's Ensure key, not the adapter"
        );

        let transport = || {
            thread_api::transport::IrohTransport::new(
                connection.connection.clone(),
                thread_api::credentials::Credentials::Public,
                api::framing::MAX_CONTROL_BODY,
                Duration::from_secs(5),
            )
        };
        let adapter = thread_api::Remote::discover(
            transport().expect("adapter transport"),
            adapter_key,
            EndpointKind::Weft,
        )
        .await;
        let adapter_error = match adapter {
            Ok(_) => panic!("Discover must not treat the local adapter as Weft"),
            Err(error) => error.to_string(),
        };
        assert!(
            adapter_error.contains("endpoint identity/package mismatch"),
            "adapter Discover must fail closed, got {adapter_error}"
        );

        let remote = thread_api::Remote::discover(
            transport().expect("weft transport"),
            weft_key,
            EndpointKind::Weft,
        )
        .await
        .expect("Discover must accept Weft's endpoint key over the proxy");
        assert_eq!(
            remote.description.endpoint.expect("described endpoint").public_key,
            weft_key
        );
        connection.close().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn netd_warm_client_discovers_weft_identity() {
        let _process_env_guard = crate::test_process_env::exclusive().await;
        let fixture =
            crate::hosted_runtime::hosted::hosted_bridge::tests::WarmBridgeFixture::start_describe()
                .await;
        let _home = crate::hosted_runtime::hosted::hosted_bridge::tests::PinHeddleHome::new(
            fixture.home.path(),
        );
        let client = super::HostedClient::connect_via_netd(
            crate::hosted_runtime::hosted::hosted_bridge::tests::TEST_WEFT_SERVER,
            &config::ClientConfig::default(),
        )
        .await
        .expect("warm Discover must succeed with Weft's endpoint key");
        assert!(client.reused_warm_connection());
        let remote = client
            .native()
            .await
            .expect("cached native description after warm Discover");
        assert_eq!(
            remote
                .description
                .endpoint
                .expect("described endpoint")
                .public_key,
            fixture.weft_id.as_bytes()
        );
        client.close().await;
    }

    #[tokio::test]
    async fn failed_connect_closes_the_client_endpoint() {
        let _process_env_guard = crate::test_process_env::shared().await;
        let server = Endpoint::builder(presets::Minimal)
            .alpns(vec![b"not-heddle".to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let server_addr = server.addr();
        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.expect("incoming connection");
            assert!(
                incoming.await.is_err(),
                "ALPN mismatch must reject the dial"
            );
            server.close().await;
        });

        let client = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let client_observer = client.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            HostedConnection::connect(client, server_addr),
        )
        .await
        .expect("ALPN mismatch must fail promptly");

        assert!(result.is_err());
        assert!(client_observer.is_closed());
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn provider_connection_is_reused_by_cryptographic_endpoint_id() {
        let _process_env_guard = crate::test_process_env::shared().await;
        let server = Endpoint::builder(presets::Minimal)
            .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let server_id = server.id();
        let server_addr = server.addr();
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
        let client = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let connection = HostedConnection::connect(client, server_addr)
            .await
            .unwrap();
        connection.provider_connections.lock().await.insert(
            server_id,
            Arc::new(Mutex::new(Some(connection.connection.clone()))),
        );

        let reused = connection
            .native_provider_connection(
                &EndpointRef {
                    kind: EndpointKind::Provider as i32,
                    public_key: server_id.as_bytes().to_vec(),
                },
                &[],
            )
            .await
            .unwrap();

        assert!(reused.close_reason().is_none());
        assert_eq!(connection.provider_connections.lock().await.len(), 1);
        println!("provider_connection_reuse endpoint={server_id} connection_count=1 reused=true");
        connection.close().await;
        server_task.await.unwrap();
    }
}
