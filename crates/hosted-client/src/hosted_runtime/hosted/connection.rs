use std::{
    cell::RefCell, collections::HashMap, future::Future, net::Ipv4Addr, path::PathBuf, sync::Arc,
    time::Duration,
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

const ENDPOINT_CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

tokio::task_local! {
    static COMMAND_CONNECTIONS: RefCell<Vec<Arc<HostedConnection>>>;
}

/// The CLI owns this scope. It drains every opened connection even when a verb
/// returns early with an error, before its current-thread runtime is dropped.
pub async fn with_command_shutdown<T>(command: impl Future<Output = T>) -> T {
    COMMAND_CONNECTIONS
        .scope(RefCell::new(Vec::new()), async {
            let result = command.await;
            let connections = COMMAND_CONNECTIONS
                .with(|connections| std::mem::take(&mut *connections.borrow_mut()));
            for connection in connections {
                connection.close().await;
            }
            result
        })
        .await
}

fn track(connection: Arc<HostedConnection>) -> Arc<HostedConnection> {
    let _ = COMMAND_CONNECTIONS
        .try_with(|connections| connections.borrow_mut().push(connection.clone()));
    connection
}

async fn close_endpoint(endpoint: &Endpoint) {
    if tokio::time::timeout(ENDPOINT_CLOSE_TIMEOUT, endpoint.close())
        .await
        .is_err()
    {
        tracing::warn!("timed out closing Heddle Iroh endpoint");
    }
}

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
        track(Arc::new(Self {
            native_description,
            router,
            endpoint,
            connection,
            provider_transport: Some(ProviderWebSocketTransport::new(config.clone())),
            provider_connections: Mutex::new(HashMap::new()),
            proxy_endpoint: None,
            reused_warm: false,
        }))
    }

    async fn connect_inner(
        endpoint: Endpoint,
        address: EndpointAddr,
        provider_transport: Option<ProviderWebSocketTransport>,
    ) -> Result<Arc<Self>> {
        let connection = match endpoint.connect(address, api::HOSTED_ALPN_V1).await {
            Ok(connection) => connection,
            Err(error) => {
                close_endpoint(&endpoint).await;
                return Err(HostedError::transport(error));
            }
        };
        let router = claim_router(endpoint.clone());
        Ok(track(Arc::new(Self {
            native_description: tokio::sync::OnceCell::new(),
            router,
            endpoint,
            connection,
            provider_transport,
            provider_connections: Mutex::new(HashMap::new()),
            proxy_endpoint: None,
            reused_warm: false,
        })))
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
        let provider_transport = ProviderWebSocketTransport::new(config.clone());
        let endpoint_builder = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .map_err(HostedError::transport);
        let endpoint_result = match endpoint_builder {
            Ok(builder) => builder
                .add_custom_transport(Arc::new(provider_transport.clone()))
                .bind()
                .await
                .map_err(HostedError::transport),
            Err(error) => Err(error),
        };
        let endpoint = match endpoint_result {
            Ok(endpoint) => endpoint,
            Err(error) => {
                close_endpoint(&proxy_endpoint).await;
                return Err(error);
            }
        };
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
        heddle_perf_contract::record_network_client_initialization();
        let connection = match endpoint.connect(proxy_address, api::HOSTED_ALPN_V1).await {
            Ok(connection) => connection,
            Err(error) => {
                close_endpoint(&endpoint).await;
                close_endpoint(&proxy_endpoint).await;
                return Err(HostedError::transport(error));
            }
        };
        let router = claim_router(endpoint.clone());
        tracing::debug!(
            reused = ensured.reused,
            netd_node_id = %ensured.node_id,
            local_node_id = %endpoint.id(),
            "hosted connect using netd warm bridge"
        );
        Ok(track(Arc::new(Self {
            native_description: tokio::sync::OnceCell::new(),
            router,
            endpoint,
            connection,
            provider_transport: Some(provider_transport),
            provider_connections: Mutex::new(HashMap::new()),
            proxy_endpoint: Some(proxy_endpoint),
            reused_warm: ensured.reused,
        })))
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
        if !self.router.is_shutdown() {
            match tokio::time::timeout(ENDPOINT_CLOSE_TIMEOUT, self.router.shutdown()).await {
                Ok(Err(error)) => {
                    tracing::warn!(%error, "failed to shut down Heddle Iroh router");
                }
                Err(_) => tracing::warn!("timed out shutting down Heddle Iroh router"),
                Ok(Ok(())) => {}
            }
        }
        close_endpoint(&self.endpoint).await;
        if let Some(endpoint) = &self.proxy_endpoint {
            close_endpoint(endpoint).await;
        }
    }
}

#[cfg(unix)]
async fn serve_netd_proxy(
    endpoint: Endpoint,
    socket_path: PathBuf,
    server: String,
    allow_insecure: bool,
) -> Result<()> {
    let result = async {
        let incoming = endpoint.accept().await.ok_or_else(|| {
            HostedError::transport("local netd proxy endpoint closed before accept")
        })?;
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
        Ok(())
    }
    .await;
    close_endpoint(&endpoint).await;
    result
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
                tokio::time::timeout(ENDPOINT_CLOSE_TIMEOUT, send.stopped())
                    .await
                    .map_err(HostedError::transport)?
                    .map_err(HostedError::transport)?;
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
        io::Write,
        net::Ipv4Addr,
        sync::{Arc, Mutex as StdMutex},
        time::Duration,
    };

    use api::heddle::api::v1alpha2::{EndpointKind, EndpointRef};
    use iroh::{Endpoint, RelayMode, endpoint::presets};
    use tokio::sync::Mutex;

    use super::{HostedConnection, with_command_shutdown};

    struct TraceWriter(Arc<StdMutex<Vec<u8>>>);

    impl Write for TraceWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("trace lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn command_scope_closes_hosted_round_trip_on_error() {
        let _process_env_guard = crate::test_process_env::shared_blocking();
        let output = Arc::new(StdMutex::new(Vec::new()));
        let trace_output = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || TraceWriter(trace_output.clone()))
            .with_max_level(tracing::Level::ERROR)
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            let (server_task, endpoint, result) = with_command_shutdown(async {
                let (client, server_task, _) = super::super::native_exchange_test_server::start(
                    uuid::Uuid::new_v4(),
                    "acme/widgets",
                    [7; 32],
                )
                .await;
                client.native().await.expect("hosted discovery round trip");
                let endpoint = client.connection.endpoint.clone();
                drop(client);
                (
                    server_task,
                    endpoint,
                    Err::<(), _>("simulated command error"),
                )
            })
            .await;
            assert!(result.is_err());
            assert!(
                endpoint.is_closed(),
                "command returned with an open endpoint"
            );
            tokio::time::timeout(Duration::from_secs(5), server_task)
                .await
                .expect("server should finish after client endpoint closes")
                .expect("server task");
        });
        drop(runtime);
        let output = String::from_utf8(output.lock().expect("trace lock").clone())
            .expect("UTF-8 trace output");
        assert!(
            !output.contains("Endpoint dropped without calling Endpoint::close"),
            "iroh reported an unclosed endpoint: {output}"
        );
    }

    #[tokio::test]
    async fn close_drains_endpoint_after_connection_is_locally_closed() {
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

        connection.close().await;
        assert!(connection.endpoint.is_closed());
        server_task.await.expect("server closes its endpoint");
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
