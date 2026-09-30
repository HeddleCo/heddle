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
    static COMMAND_RESOURCES: RefCell<CommandResources>;
}

#[derive(Default)]
struct CommandResources {
    connections: Vec<Arc<HostedConnection>>,
    endpoints: Vec<Endpoint>,
}

/// The CLI owns this scope. It drains hosted connections and the persistent
/// netd endpoint even when a verb returns an error, before its runtime drops.
pub async fn with_command_shutdown<T>(command: impl Future<Output = T>) -> T {
    COMMAND_RESOURCES
        .scope(RefCell::new(CommandResources::default()), async {
            let result = command.await;
            let resources =
                COMMAND_RESOURCES.with(|resources| std::mem::take(&mut *resources.borrow_mut()));
            for connection in resources.connections {
                connection.close().await;
            }
            for endpoint in resources.endpoints {
                close_endpoint(&endpoint).await;
            }
            result
        })
        .await
}

fn track(connection: Arc<HostedConnection>) -> Arc<HostedConnection> {
    let _ = COMMAND_RESOURCES
        .try_with(|resources| resources.borrow_mut().connections.push(connection.clone()));
    connection
}

pub(crate) fn track_command_endpoint(endpoint: &Endpoint) {
    let _ = COMMAND_RESOURCES
        .try_with(|resources| resources.borrow_mut().endpoints.push(endpoint.clone()));
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
    route: HostedRoute,
    reused_warm: bool,
}

/// A netd route must carry the authenticated Weft identity along with its
/// loopback adapter. The adapter's Iroh peer key is never a Weft identity.
#[derive(Debug)]
enum HostedRoute {
    Direct,
    #[cfg(unix)]
    Netd {
        proxy_endpoint: Endpoint,
        weft_endpoint_id: EndpointId,
    },
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
            route: HostedRoute::Direct,
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
            route: HostedRoute::Direct,
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
    pub(super) async fn connect_via_netd(
        server: &str,
        config: &ClientConfig,
        descriptor: Option<&super::VerifiedEndpointDescriptor>,
    ) -> Result<Arc<Self>> {
        let socket_path =
            hosted_bridge::hosted_bridge_socket_path(&repo::identity::heddle_home_dir());
        if !socket_path.exists() {
            return Err(HostedError::transport("netd hosted bridge is not running"));
        }
        let ensured =
            hosted_bridge::ensure_via_netd(&socket_path, server, config.allow_insecure).await?;
        verify_netd_weft_identity(server, config, descriptor, ensured.weft_endpoint_id).await?;

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
        let weft_endpoint_id = ensured.weft_endpoint_id;
        tokio::spawn(async move {
            if let Err(error) = serve_netd_proxy(
                proxy_task_endpoint,
                proxy_socket,
                proxy_server,
                allow_insecure,
                weft_endpoint_id,
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
            weft_endpoint_id = %ensured.weft_endpoint_id,
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
            route: HostedRoute::Netd {
                proxy_endpoint,
                weft_endpoint_id: ensured.weft_endpoint_id,
            },
            reused_warm: ensured.reused,
        })))
    }

    /// Key that v2 Discover must prove on this route.
    pub(super) fn discover_endpoint_key(&self) -> [u8; 32] {
        match &self.route {
            HostedRoute::Direct => *self.connection.remote_id().as_bytes(),
            #[cfg(unix)]
            HostedRoute::Netd {
                weft_endpoint_id, ..
            } => *weft_endpoint_id.as_bytes(),
        }
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
        #[cfg(unix)]
        if let HostedRoute::Netd { proxy_endpoint, .. } = &self.route {
            close_endpoint(proxy_endpoint).await;
        }
    }
}

/// Refuse netd's warm Weft session unless its identity is the one the caller's
/// own trust verified. Nothing has crossed the bridge except `server` and
/// `allow_insecure` at this point, so rejection happens before any credential
/// or proof is sent.
///
/// - An already verified descriptor pins the exact identity; netd holding a
///   session to any other endpoint is not used.
/// - A configured descriptor root, CA, or TLS name override is applied here,
///   in this process: the live descriptor set is fetched and verified with the
///   caller's policy, and netd's identity must be one of its root-attested
///   entries. netd bootstraps with default trust, so its choice alone proves
///   nothing about the caller's pin.
/// - Default trust is what netd itself applies (automatic pins in the same
///   Heddle home), so the warm route is used as-is.
#[cfg(unix)]
async fn verify_netd_weft_identity(
    server: &str,
    config: &ClientConfig,
    descriptor: Option<&super::VerifiedEndpointDescriptor>,
    netd_weft: EndpointId,
) -> Result<()> {
    let verified = match descriptor {
        Some(descriptor) => descriptor.endpoint_addr()?.id,
        None if super::resolver::configures_endpoint_trust(config) => {
            super::resolver::resolve_and_verify_netd_endpoint(server, config, netd_weft)
                .await?
                .endpoint_addr()?
                .id
        }
        None => return Ok(()),
    };
    if verified != netd_weft {
        return Err(HostedError::DescriptorTrust(
            "netd Weft identity does not match the caller's verified endpoint descriptor"
                .to_string(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
async fn serve_netd_proxy(
    endpoint: Endpoint,
    socket_path: PathBuf,
    server: String,
    allow_insecure: bool,
    weft_endpoint_id: EndpointId,
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
                if let Err(error) = splice_netd_stream(
                    send,
                    recv,
                    &socket_path,
                    &server,
                    allow_insecure,
                    weft_endpoint_id,
                )
                .await
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
    weft_endpoint_id: EndpointId,
) -> Result<()> {
    let (stream, _reused, opened_endpoint_id) =
        hosted_bridge::open_bi_via_netd(socket_path, server, allow_insecure).await?;
    // No request bytes (including discovery credentials) cross the UDS until
    // the connection used for this stream proves the same verified identity.
    if opened_endpoint_id != weft_endpoint_id {
        return Err(HostedError::DescriptorTrust(
            "netd Weft identity changed after endpoint verification".to_string(),
        ));
    }
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

    use super::{HostedConnection, track_command_endpoint, with_command_shutdown};

    #[tokio::test]
    async fn command_scope_closes_a_registered_endpoint_on_error() {
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("client address")
            .bind()
            .await
            .expect("client endpoint");
        let result = with_command_shutdown(async {
            track_command_endpoint(&endpoint);
            Err::<(), _>("simulated daemon error")
        })
        .await;
        assert!(result.is_err());
        assert!(endpoint.is_closed());
    }

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
            None,
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
            crate::hosted_runtime::hosted::hosted_bridge::tests::WarmBridgeFixture::start_describe(
            )
            .await;
        let _home = crate::hosted_runtime::hosted::hosted_bridge::tests::PinHeddleHome::new(
            fixture.home.path(),
        );
        let connection = HostedConnection::connect_via_netd(
            crate::hosted_runtime::hosted::hosted_bridge::tests::TEST_WEFT_SERVER,
            &config::ClientConfig::default(),
            None,
        )
        .await
        .expect("connect through warm netd bridge");
        let adapter_key = *connection.connection.remote_id().as_bytes();
        let weft_key = *fixture.weft_id.as_bytes();
        assert_ne!(
            adapter_key, weft_key,
            "local Iroh↔UDS adapter peer must not be Weft"
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
            remote
                .description
                .endpoint
                .expect("described endpoint")
                .public_key,
            weft_key
        );
        let client = super::super::HostedClient {
            connection: connection.clone(),
            context: super::super::CallContextFactory::default(),
            on_human_signature: None,
            warnings: Arc::new(super::super::NoopWarnings),
            server_key: None,
        };
        let discovered = client.native().await;
        connection.close().await;
        assert!(
            discovered.is_ok(),
            "proxied client must discover Weft, got {:?}",
            discovered.err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn netd_warm_client_discovers_weft_identity() {
        let _process_env_guard = crate::test_process_env::exclusive().await;
        let fixture =
            crate::hosted_runtime::hosted::hosted_bridge::tests::WarmBridgeFixture::start_describe(
            )
            .await;
        let _home = crate::hosted_runtime::hosted::hosted_bridge::tests::PinHeddleHome::new(
            fixture.home.path(),
        );
        let client = crate::hosted_runtime::hosted::HostedClient::connect_via_netd(
            crate::hosted_runtime::hosted::hosted_bridge::tests::TEST_WEFT_SERVER,
            &config::ClientConfig::default(),
            None,
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
        assert_eq!(
            fixture.accepts(),
            1,
            "Discover must reuse netd's Weft session"
        );
        client.close().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn netd_stream_rejects_changed_or_missing_identity_before_forwarding_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let _process_env_guard = crate::test_process_env::exclusive().await;
        for changed_identity in [false, true] {
            let home = tempfile::TempDir::new().expect("home");
            let _home = super::hosted_bridge::tests::PinHeddleHome::new(home.path());
            let socket = super::hosted_bridge::hosted_bridge_socket_path(home.path());
            std::fs::create_dir_all(socket.parent().expect("socket parent"))
                .expect("state directory");
            let listener = tokio::net::UnixListener::bind(&socket).expect("bridge socket");
            let weft_id = iroh_base::SecretKey::generate().public();
            let descriptor = super::super::connection_path_tests::verified_descriptor(
                weft_id,
                vec![],
                vec!["127.0.0.1:9".into()],
            );
            let serve = tokio::spawn(async move {
                for op in ["ready", "opened"] {
                    let (mut stream, _) = listener.accept().await.expect("bridge request");
                    let length = stream.read_u32().await.expect("request length");
                    let mut request = vec![0; length as usize];
                    stream
                        .read_exact(&mut request)
                        .await
                        .expect("request frame");
                    let mut response = serde_json::json!({ "op": op, "reused": true });
                    if op == "ready" {
                        response["node_id"] =
                            iroh_base::SecretKey::generate().public().to_string().into();
                        response["weft_endpoint_id"] = weft_id.to_string().into();
                    } else if changed_identity {
                        response["weft_endpoint_id"] =
                            iroh_base::SecretKey::generate().public().to_string().into();
                    }
                    let response = serde_json::to_vec(&response).expect("bridge response");
                    stream
                        .write_u32(response.len() as u32)
                        .await
                        .expect("response length");
                    stream.write_all(&response).await.expect("response frame");
                    if op == "opened" {
                        let mut forwarded = Vec::new();
                        stream
                            .read_to_end(&mut forwarded)
                            .await
                            .expect("closed rejected stream");
                        assert!(
                            forwarded.is_empty(),
                            "credentials forwarded after changed/missing identity"
                        );
                    }
                }
            });
            let connection = HostedConnection::connect_via_netd(
                super::hosted_bridge::tests::TEST_WEFT_SERVER,
                &config::ClientConfig::default(),
                Some(&descriptor),
            )
            .await
            .expect("verified Ready route");
            let context = api::heddle::api::common::CallContext {
                bearer_capability: b"bearer-secret".to_vec(),
                request_proof: Some(Default::default()),
                ..Default::default()
            };
            let response = tokio::time::timeout(
                Duration::from_secs(2),
                super::super::call::unary_encoded::<
                    api::heddle::api::v1alpha2::DescribeEndpointResponse,
                >(
                    &connection,
                    "/heddle.api.v1alpha2.EndpointService/DescribeEndpoint",
                    &context,
                    &[],
                ),
            )
            .await
            .expect("rejected RPC stream");
            assert!(
                response.is_err(),
                "unproved Opened identity must fail the RPC"
            );
            tokio::time::timeout(Duration::from_secs(2), serve)
                .await
                .expect("bridge task finishes")
                .expect("no forwarded bytes");
            connection.close().await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn netd_connection_rejects_ready_without_weft_identity() {
        let _process_env_guard = crate::test_process_env::exclusive().await;
        let home = tempfile::TempDir::new().expect("home");
        let _home = super::hosted_bridge::tests::PinHeddleHome::new(home.path());
        let socket = super::hosted_bridge::hosted_bridge_socket_path(home.path());
        std::fs::create_dir_all(socket.parent().expect("socket parent")).expect("state directory");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bridge socket");
        let serve = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut stream, _) = listener.accept().await.expect("Ensure connection");
            let length = stream.read_u32().await.expect("Ensure length");
            let mut request = vec![0; length as usize];
            stream
                .read_exact(&mut request)
                .await
                .expect("Ensure request");
            let response = serde_json::to_vec(&serde_json::json!({
                "op": "ready",
                "reused": true,
                "node_id": iroh_base::SecretKey::generate().public().to_string(),
            }))
            .expect("old daemon Ready");
            stream
                .write_u32(response.len() as u32)
                .await
                .expect("Ready length");
            stream.write_all(&response).await.expect("Ready frame");
        });
        let error = HostedConnection::connect_via_netd(
            super::hosted_bridge::tests::TEST_WEFT_SERVER,
            &config::ClientConfig::default(),
            None,
        )
        .await
        .expect_err("missing Weft identity must reject the proxy route");
        assert!(
            error.to_string().contains("weft_endpoint_id"),
            "got {error}"
        );
        serve.await.expect("bridge task");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_client_discovers_remote_identity() {
        let _process_env_guard = crate::test_process_env::shared().await;
        let fixture = super::hosted_bridge::tests::WarmBridgeFixture::start_describe().await;
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .expect("client address")
            .bind()
            .await
            .expect("client endpoint");
        let client = super::super::HostedClient::connect_addr(endpoint, fixture.weft_address())
            .await
            .expect("direct client");
        assert!(matches!(
            client.connection.route,
            super::HostedRoute::Direct
        ));
        assert_eq!(
            client.connection.discover_endpoint_key(),
            *client.connection.connection.remote_id().as_bytes()
        );
        let remote = client.native().await.expect("direct Discover");
        assert_eq!(
            remote
                .description
                .endpoint
                .expect("Weft endpoint")
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
