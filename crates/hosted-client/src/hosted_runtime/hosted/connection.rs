use std::{
    collections::HashMap,
    os::fd::AsRawFd,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use api::heddle::api::v1alpha1::ProviderSource;
use bytes::Bytes;
use config::ClientConfig;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMode,
    endpoint::{AckFrequencyConfig, QuicTransportConfig, presets},
    protocol::Router,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};

use super::{
    HostedError, Result, VerifiedEndpointDescriptor,
    claim_protocol::{CLAIM_ALPN_V1, ClaimProtocol},
    hosted_bridge,
    provider_transport::ProviderWebSocketTransport,
};

const DIRECT_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);

/// Foreground wait for `Router::shutdown` / `Endpoint::close`.
///
/// Loopback drain is sub-millisecond in release. On a relay/WAN path
/// `noq wait_all_draining` waits for a close-frame ACK / probe timeout
/// that measures a repeatable ~1000 ms after the RPC is already
/// `LocallyClosed`. One-shot CLI verbs must not sit on that wait:
/// initiate graceful close (so the endpoint is not dropped dirty),
/// then detach the remainder.
const FOREGROUND_ENDPOINT_DRAIN: Duration = Duration::from_millis(20);

#[cfg(test)]
static NEXT_SHUTDOWN_HOLD_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
pub(super) fn hold_next_shutdown_for_test(duration: Duration) {
    NEXT_SHUTDOWN_HOLD_MS.store(duration.as_millis() as u64, Ordering::SeqCst);
}

fn shutdown_hold_for_test() -> Option<Duration> {
    #[cfg(test)]
    {
        let ms = NEXT_SHUTDOWN_HOLD_MS.swap(0, Ordering::SeqCst);
        if ms > 0 {
            return Some(Duration::from_millis(ms));
        }
    }
    None
}

#[derive(Debug)]
pub(super) struct HostedConnection {
    inner: HostedTransport,
    provider_connections:
        Mutex<HashMap<EndpointId, Arc<Mutex<Option<iroh::endpoint::Connection>>>>>,
}

#[derive(Debug)]
enum HostedTransport {
    Local {
        // A transient inbound claim listener on this connection's endpoint.
        // It serves resolve/consent for a browser that reaches this process's
        // endpoint, but it holds no owner-root co-sign consumer: the persisted
        // box network daemon owns that, and the owner-root co-sign is bridged
        // to a foreground signer (heddle#1620). A co-sign dialed here therefore
        // fails closed rather than being performed without a foreground owner.
        router: Router,
        endpoint: Endpoint,
        connection: iroh::endpoint::Connection,
        provider_transport: Option<ProviderWebSocketTransport>,
    },
    #[cfg(unix)]
    Proxied(ProxiedTransport),
}

#[cfg(unix)]
#[derive(Debug)]
struct ProxiedTransport {
    server: String,
    allow_insecure: bool,
    socket_path: PathBuf,
    node_id: EndpointId,
    reused: AtomicBool,
    /// Ephemeral local endpoint for provider (CAS) dials.
    ///
    /// Weft stays in netd. `ProviderBackend` needs a real Iroh
    /// `Connection`, which cannot cross the UDS splice, so the CLI
    /// binds this once per process the first time a provider is used.
    provider_local: Mutex<Option<LocalProviderEndpoint>>,
}

#[cfg(unix)]
#[derive(Debug)]
struct LocalProviderEndpoint {
    endpoint: Endpoint,
    transport: ProviderWebSocketTransport,
}

impl HostedConnection {
    pub(super) async fn connect_verified(
        descriptor: &VerifiedEndpointDescriptor,
        config: &ClientConfig,
    ) -> Result<Arc<Self>> {
        Self::connect_verified_inner(descriptor, config, EndpointIdentity::Device).await
    }

    /// Connect for outbound calls only, on an *ephemeral* endpoint node id.
    ///
    /// The persistent box network daemon owns the device node id and the
    /// inbound `heddle-claim/1` router (heddle#1620). A foreground
    /// `heddle claim` that also bound the device node id would fight the
    /// daemon for the relay's home registration and strand browsers
    /// dialing the advertised node id. Its weft auth is carried entirely
    /// by the credential proof key (bearer + PoP + request proof), which
    /// is independent of the endpoint node id, so an ephemeral endpoint
    /// makes the same authenticated calls without the collision.
    pub(super) async fn connect_verified_outbound(
        descriptor: &VerifiedEndpointDescriptor,
        config: &ClientConfig,
    ) -> Result<Arc<Self>> {
        Self::connect_verified_inner(descriptor, config, EndpointIdentity::Ephemeral).await
    }

    async fn connect_verified_inner(
        descriptor: &VerifiedEndpointDescriptor,
        config: &ClientConfig,
        identity: EndpointIdentity,
    ) -> Result<Arc<Self>> {
        heddle_perf_contract::record_network_client_initialization();
        let relays = descriptor.relay_urls()?;
        let address = descriptor.endpoint_addr()?;
        let direct_address = descriptor.direct_endpoint_addr()?;

        if direct_address.ip_addrs().next().is_some() {
            let provider_transport = ProviderWebSocketTransport::new(config.clone());
            // The endpoint is now also an inbound claim listener. Keep its
            // signed relays online even when the hosted connection itself can
            // use a direct path, or a browser holding only the claim link
            // cannot reach the advertised node id.
            let relay_mode = if relays.is_empty() {
                RelayMode::Disabled
            } else {
                RelayMode::custom(relays.clone())
            };
            let endpoint =
                bind_endpoint(relay_mode, Some(provider_transport.clone()), identity).await?;
            if relays.is_empty() {
                return Self::connect_inner(endpoint, direct_address, Some(provider_transport))
                    .await;
            }
            let direct = tokio::time::timeout(
                DIRECT_CONNECT_TIMEOUT,
                Self::connect_inner(endpoint, direct_address, Some(provider_transport)),
            )
            .await;
            match direct {
                Ok(Ok(connection)) => return Ok(connection),
                Ok(Err(error)) => {
                    tracing::debug!(%error, "signed direct addresses unavailable; enabling relays")
                }
                Err(_) => tracing::debug!(
                    timeout_ms = DIRECT_CONNECT_TIMEOUT.as_millis(),
                    "signed direct-address attempt timed out; enabling relays"
                ),
            }
        }

        let relay_mode = if relays.is_empty() {
            RelayMode::Disabled
        } else {
            RelayMode::custom(relays)
        };
        let provider_transport = ProviderWebSocketTransport::new(config.clone());
        let endpoint =
            bind_endpoint(relay_mode, Some(provider_transport.clone()), identity).await?;
        Self::connect_inner(endpoint, address, Some(provider_transport)).await
    }

    pub(super) async fn connect(endpoint: Endpoint, address: EndpointAddr) -> Result<Arc<Self>> {
        heddle_perf_contract::record_network_client_initialization();
        Self::connect_inner(endpoint, address, None).await
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
            inner: HostedTransport::Local {
                router,
                endpoint,
                connection,
                provider_transport,
            },
            provider_connections: Mutex::new(HashMap::new()),
        }))
    }

    #[cfg(unix)]
    pub(super) async fn connect_via_netd(server: &str, config: &ClientConfig) -> Result<Arc<Self>> {
        let socket = hosted_bridge::hosted_bridge_socket_path(&repo::identity::heddle_home_dir());
        if !socket.exists() {
            return Err(HostedError::transport("netd hosted bridge is not running"));
        }
        let ensured =
            hosted_bridge::ensure_via_netd(&socket, server, config.allow_insecure).await?;
        tracing::debug!(
            reused = ensured.reused,
            node_id = %ensured.node_id,
            "hosted connect using netd warm bridge"
        );
        Ok(Arc::new(Self {
            inner: HostedTransport::Proxied(ProxiedTransport {
                server: server.to_string(),
                allow_insecure: config.allow_insecure,
                socket_path: socket,
                node_id: ensured.node_id,
                reused: AtomicBool::new(ensured.reused),
                provider_local: Mutex::new(None),
            }),
            provider_connections: Mutex::new(HashMap::new()),
        }))
    }

    pub(super) fn endpoint_id(&self) -> EndpointId {
        match &self.inner {
            HostedTransport::Local { endpoint, .. } => endpoint.id(),
            #[cfg(unix)]
            HostedTransport::Proxied(proxied) => proxied.node_id,
        }
    }

    pub(super) fn supports_provider_transport(&self) -> bool {
        match &self.inner {
            HostedTransport::Local {
                provider_transport, ..
            } => provider_transport.is_some(),
            #[cfg(unix)]
            HostedTransport::Proxied(_) => true,
        }
    }

    #[cfg(test)]
    pub(super) fn local_endpoint(&self) -> Option<&Endpoint> {
        match &self.inner {
            HostedTransport::Local { endpoint, .. } => Some(endpoint),
            #[cfg(unix)]
            HostedTransport::Proxied(_) => None,
        }
    }

    #[cfg(test)]
    pub(super) fn quic_connection(&self) -> Option<&iroh::endpoint::Connection> {
        match &self.inner {
            HostedTransport::Local { connection, .. } => Some(connection),
            #[cfg(unix)]
            HostedTransport::Proxied(_) => None,
        }
    }

    pub(super) async fn provider_connection(
        &self,
        source: &ProviderSource,
    ) -> Result<iroh::endpoint::Connection> {
        let endpoint_id: EndpointId = source.endpoint_id.parse().map_err(|error| {
            HostedError::InvalidDescriptor(format!("provider endpoint id: {error}"))
        })?;
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

        match &self.inner {
            HostedTransport::Local {
                endpoint,
                provider_transport,
                ..
            } => {
                let transport = provider_transport.as_ref().ok_or_else(|| {
                    HostedError::InvalidDescriptor(
                        "the active Iroh endpoint has no provider transport".to_string(),
                    )
                })?;
                let connection = dial_provider(endpoint, transport, source).await?;
                *cached = Some(connection.clone());
                Ok(connection)
            }
            #[cfg(unix)]
            HostedTransport::Proxied(proxied) => {
                let (endpoint, transport) = proxied.ensure_local_provider().await?;
                let connection = dial_provider(&endpoint, &transport, source).await?;
                *cached = Some(connection.clone());
                Ok(connection)
            }
        }
    }

    pub(super) async fn close(&self) {
        match &self.inner {
            HostedTransport::Local {
                router, connection, ..
            } => {
                connection.close(0u32.into(), b"Heddle client closed");
                let router = router.clone();
                bounded_foreground_shutdown(async move { router.shutdown().await }).await;
            }
            #[cfg(unix)]
            HostedTransport::Proxied(proxied) => {
                // The weft QUIC session lives in netd; dropping this handle
                // must not drain it. A lazy local provider endpoint is
                // this process's, so start a bounded close if we bound one.
                let local = {
                    let mut guard = proxied.provider_local.lock().await;
                    guard.take()
                };
                if let Some(local) = local {
                    let endpoint = local.endpoint;
                    bounded_foreground_shutdown(async move {
                        endpoint.close().await;
                        Ok::<(), &'static str>(())
                    })
                    .await;
                }
            }
        }
    }

    pub(super) fn reused_warm(&self) -> bool {
        match &self.inner {
            HostedTransport::Local { .. } => false,
            #[cfg(unix)]
            HostedTransport::Proxied(proxied) => proxied.reused.load(Ordering::Relaxed),
        }
    }

    #[cfg(all(test, unix))]
    fn local_provider_endpoint_id_for_test(&self) -> Option<EndpointId> {
        match &self.inner {
            HostedTransport::Local { .. } => None,
            HostedTransport::Proxied(proxied) => proxied
                .provider_local
                .try_lock()
                .ok()
                .and_then(|guard| guard.as_ref().map(|local| local.endpoint.id())),
        }
    }

    #[cfg(all(test, unix))]
    async fn ensure_local_provider_for_test(&self) -> Result<EndpointId> {
        match &self.inner {
            HostedTransport::Local { .. } => Err(HostedError::transport(
                "local sessions already have an endpoint",
            )),
            HostedTransport::Proxied(proxied) => Ok(proxied.ensure_local_provider().await?.0.id()),
        }
    }

    pub(super) async fn open_bi(&self) -> Result<(HostedSendStream, HostedRecvStream)> {
        match &self.inner {
            HostedTransport::Local { connection, .. } => {
                let (send, recv) = connection.open_bi().await.map_err(HostedError::transport)?;
                Ok((
                    HostedSendStream::Direct(send),
                    HostedRecvStream::Direct(recv),
                ))
            }
            #[cfg(unix)]
            HostedTransport::Proxied(proxied) => {
                let (stream, reused) = hosted_bridge::open_bi_via_netd(
                    &proxied.socket_path,
                    &proxied.server,
                    proxied.allow_insecure,
                    None,
                )
                .await?;
                proxied.reused.store(reused, Ordering::Relaxed);
                let (read, write) = stream.into_split();
                Ok((
                    HostedSendStream::Proxied(write),
                    HostedRecvStream::Proxied(read),
                ))
            }
        }
    }
}

#[cfg(unix)]
impl ProxiedTransport {
    async fn ensure_local_provider(&self) -> Result<(Endpoint, ProviderWebSocketTransport)> {
        let mut guard = self.provider_local.lock().await;
        if let Some(local) = guard.as_ref() {
            return Ok((local.endpoint.clone(), local.transport.clone()));
        }
        let config = ClientConfig {
            allow_insecure: self.allow_insecure,
            ..ClientConfig::default()
        };
        let transport = ProviderWebSocketTransport::new(config);
        let endpoint = bind_endpoint(
            RelayMode::Disabled,
            Some(transport.clone()),
            EndpointIdentity::Ephemeral,
        )
        .await?;
        tracing::debug!(
            node_id = %endpoint.id(),
            "bound local ephemeral endpoint for provider dials on a netd-proxied session"
        );
        *guard = Some(LocalProviderEndpoint {
            endpoint: endpoint.clone(),
            transport: transport.clone(),
        });
        Ok((endpoint, transport))
    }
}

async fn dial_provider(
    endpoint: &Endpoint,
    transport: &ProviderWebSocketTransport,
    source: &ProviderSource,
) -> Result<iroh::endpoint::Connection> {
    let address = transport.register_source(
        &source.provider_id,
        &source.endpoint_id,
        &source.direct_url,
        &source.opaque_ticket,
    )?;
    endpoint
        .connect(address, api::PROVIDER_ALPN_V1)
        .await
        .map_err(HostedError::transport)
}

pub(super) enum HostedSendStream {
    Direct(iroh::endpoint::SendStream),
    #[cfg(unix)]
    Proxied(tokio::net::unix::OwnedWriteHalf),
}

pub(super) enum HostedRecvStream {
    Direct(iroh::endpoint::RecvStream),
    #[cfg(unix)]
    Proxied(tokio::net::unix::OwnedReadHalf),
}

impl HostedSendStream {
    pub(super) async fn write_chunk(&mut self, chunk: Bytes) -> Result<()> {
        match self {
            Self::Direct(send) => send
                .write_chunk(chunk)
                .await
                .map_err(HostedError::transport),
            #[cfg(unix)]
            Self::Proxied(send) => send.write_all(&chunk).await.map_err(HostedError::transport),
        }
    }

    pub(super) async fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        match self {
            Self::Direct(send) => send.write_all(buf).await.map_err(HostedError::transport),
            #[cfg(unix)]
            Self::Proxied(send) => send.write_all(buf).await.map_err(HostedError::transport),
        }
    }

    pub(super) fn finish(&mut self) -> Result<()> {
        match self {
            Self::Direct(send) => send.finish().map_err(HostedError::transport),
            #[cfg(unix)]
            Self::Proxied(send) => {
                let fd = send.as_ref().as_raw_fd();
                let rc = unsafe { libc::shutdown(fd, libc::SHUT_WR) };
                if rc == 0 {
                    Ok(())
                } else {
                    Err(HostedError::transport(std::io::Error::last_os_error()))
                }
            }
        }
    }

    pub(super) fn reset(&mut self, error_code: u32) -> Result<()> {
        match self {
            Self::Direct(send) => send
                .reset(error_code.into())
                .map_err(HostedError::transport),
            #[cfg(unix)]
            Self::Proxied(send) => {
                let fd = send.as_ref().as_raw_fd();
                let _ = unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
                Ok(())
            }
        }
    }
}

impl HostedRecvStream {
    pub(super) async fn read_to_end(&mut self, max: usize) -> Result<Vec<u8>> {
        match self {
            Self::Direct(recv) => recv.read_to_end(max).await.map_err(HostedError::transport),
            #[cfg(unix)]
            Self::Proxied(recv) => {
                let mut buf = Vec::new();
                recv.read_to_end(&mut buf)
                    .await
                    .map_err(HostedError::transport)?;
                if buf.len() > max {
                    buf.truncate(max + 1);
                }
                Ok(buf)
            }
        }
    }

    pub(super) async fn read_chunk(&mut self, max: usize) -> Result<Option<Bytes>> {
        match self {
            Self::Direct(recv) => recv.read_chunk(max).await.map_err(HostedError::transport),
            #[cfg(unix)]
            Self::Proxied(recv) => {
                let mut buf = vec![0u8; max.max(1)];
                match recv.read(&mut buf).await {
                    Ok(0) => Ok(None),
                    Ok(n) => {
                        buf.truncate(n);
                        Ok(Some(Bytes::from(buf)))
                    }
                    Err(error) => Err(HostedError::transport(error)),
                }
            }
        }
    }

    pub(super) fn stop(&mut self, error_code: u32) -> Result<()> {
        match self {
            Self::Direct(recv) => recv.stop(error_code.into()).map_err(HostedError::transport),
            #[cfg(unix)]
            Self::Proxied(recv) => {
                let fd = recv.as_ref().as_raw_fd();
                let _ = unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
                Ok(())
            }
        }
    }
}

/// Initiate graceful router/endpoint shutdown without gating the
/// caller on QUIC drain. Wait up to [`FOREGROUND_ENDPOINT_DRAIN`];
/// if close is still running, detach the task so drain continues.
///
/// Do not wrap the [`tokio::task::JoinHandle`] in
/// [`tokio::time::timeout`]: on `Elapsed` that future is dropped,
/// and a dropped handle must not be what stops close. Race join
/// against the bound and [`std::mem::forget`] the handle so the
/// runtime keeps the drain.
pub(super) async fn bounded_foreground_shutdown<E, F>(shutdown: F)
where
    F: std::future::Future<Output = std::result::Result<(), E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let hold = shutdown_hold_for_test();
    let mut task = tokio::spawn(async move {
        if let Some(hold) = hold {
            tokio::time::sleep(hold).await;
        }
        if let Err(error) = shutdown.await {
            tracing::warn!(%error, "failed to shut down Heddle Iroh router");
        }
    });
    tokio::select! {
        _ = &mut task => {}
        () = tokio::time::sleep(FOREGROUND_ENDPOINT_DRAIN) => {
            tracing::debug!(
                timeout_ms = FOREGROUND_ENDPOINT_DRAIN.as_millis(),
                "detached hosted endpoint drain after foreground bound"
            );
            std::mem::forget(task);
        }
    }
}

/// Which node identity a hosted endpoint binds.
#[derive(Clone, Copy, Debug)]
enum EndpointIdentity {
    /// The persisted device node id — the machine's single stable
    /// address, also served by the box network daemon.
    Device,
    /// A fresh per-process node id, for outbound-only connections that
    /// must not contend with the daemon for the device node id.
    Ephemeral,
}

async fn bind_endpoint(
    relay_mode: RelayMode,
    provider_transport: Option<ProviderWebSocketTransport>,
    identity: EndpointIdentity,
) -> Result<Endpoint> {
    let mut builder = Endpoint::builder(presets::Minimal)
        .transport_config(transport_config())
        .relay_mode(relay_mode);
    if let EndpointIdentity::Device = identity {
        let device = crate::hosted_runtime::agent_node_identity::load_or_create()
            .map_err(HostedError::transport)?;
        builder = builder.secret_key(device.secret_key());
    }
    if let Some(provider_transport) = provider_transport {
        builder = builder.add_custom_transport(Arc::new(provider_transport));
    }
    builder.bind().await.map_err(HostedError::transport)
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
    Router::builder(endpoint)
        .accept(
            CLAIM_ALPN_V1,
            ClaimProtocol::new(Arc::clone(&authorization), authorization),
        )
        .spawn()
}

impl Drop for HostedConnection {
    fn drop(&mut self) {
        if let HostedTransport::Local { connection, .. } = &self.inner {
            connection.close(0u32.into(), b"Heddle client closed");
        }
    }
}

fn transport_config() -> QuicTransportConfig {
    // Match Weft's WAN-oriented profile: enough BDP for a 1 Gbit/s, ~32 ms
    // path while keeping per-stream memory well below the 16 MiB experiment.
    const STREAM_RECEIVE_WINDOW: u32 = 4 * 1024 * 1024;
    const CONNECTION_RECEIVE_WINDOW: u32 = 8 * STREAM_RECEIVE_WINDOW;
    let mut acknowledgements = AckFrequencyConfig::default();
    acknowledgements.ack_eliciting_threshold(50u32.into());
    QuicTransportConfig::builder()
        .stream_receive_window(STREAM_RECEIVE_WINDOW.into())
        .receive_window(CONNECTION_RECEIVE_WINDOW.into())
        .ack_frequency_config(Some(acknowledgements))
        .build()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        net::Ipv4Addr,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    use api::heddle::api::v1alpha1::ProviderSource;
    use iroh::{Endpoint, RelayMode, endpoint::presets};
    use tokio::sync::Mutex;

    use super::{HostedConnection, bounded_foreground_shutdown};

    #[tokio::test]
    async fn failed_connect_closes_the_client_endpoint() {
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
            Arc::new(Mutex::new(Some(
                connection.quic_connection().expect("local fixture").clone(),
            ))),
        );

        let reused = connection
            .provider_connection(&ProviderSource {
                provider_id: "provider-a".to_string(),
                endpoint_id: server_id.to_string(),
                direct_url: "wss://unused.invalid/direct?provider=provider-a&ticket=unused"
                    .to_string(),
                opaque_ticket: "unused".to_string(),
                expires_at_unix_millis: u64::MAX,
            })
            .await
            .unwrap();

        assert!(reused.close_reason().is_none());
        assert_eq!(connection.provider_connections.lock().await.len(), 1);
        println!("provider_connection_reuse endpoint={server_id} connection_count=1 reused=true");
        connection.close().await;
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn bounded_shutdown_returns_before_a_one_second_drain() {
        let started = Instant::now();
        bounded_foreground_shutdown(async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok::<(), &'static str>(())
        })
        .await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "foreground close must detach a 1s drain, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn bounded_shutdown_still_completes_after_foreground_detach() {
        let finished = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&finished);
        let started = Instant::now();
        bounded_foreground_shutdown(async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            flag.store(true, Ordering::SeqCst);
            Ok::<(), &'static str>(())
        })
        .await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "foreground close must return at the 20ms bound, took {elapsed:?}"
        );
        assert!(
            !finished.load(Ordering::SeqCst),
            "an 80ms drain must still be in flight when the caller returns"
        );
        tokio::time::timeout(Duration::from_millis(400), async {
            while !finished.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("detached drain must still complete after the caller returns");
    }

    #[tokio::test]
    async fn close_does_not_block_a_second_after_locally_closed() {
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
                .expect("incoming connection")
                .await
                .unwrap();
            // Hold the peer without acknowledging close so drain would
            // otherwise wait for the probe timeout.
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
        let connection = HostedConnection::connect(client, server_addr)
            .await
            .unwrap();
        let quic = connection.quic_connection().expect("local fixture");
        quic.close(0u32.into(), b"Heddle client closed");
        tokio::time::timeout(Duration::from_secs(2), quic.closed())
            .await
            .expect("QUIC close should become LocallyClosed");
        assert!(quic.close_reason().is_some());

        let started = Instant::now();
        connection.close().await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "close after LocallyClosed must not wait for endpoint drain, took {elapsed:?}"
        );
        server_task.abort();
        let _ = server_task.await;
    }

    #[cfg(unix)]
    fn test_proxied() -> std::sync::Arc<HostedConnection> {
        let node_id = iroh_base::SecretKey::generate().public();
        std::sync::Arc::new(HostedConnection {
            inner: super::HostedTransport::Proxied(super::ProxiedTransport {
                server: "https://api.test.heddle.sh".to_string(),
                allow_insecure: true,
                socket_path: PathBuf::from("/tmp/heddle-hosted-unused.sock"),
                node_id,
                reused: AtomicBool::new(true),
                provider_local: Mutex::new(None),
            }),
            provider_connections: Mutex::new(HashMap::new()),
        })
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proxied_provider_connection_binds_local_endpoint_instead_of_failing_closed() {
        let connection = test_proxied();
        let endpoint_id = iroh_base::SecretKey::generate().public();
        let source = ProviderSource {
            provider_id: "provider-a".to_string(),
            endpoint_id: endpoint_id.to_string(),
            direct_url: "wss://127.0.0.1:1/direct?provider=provider-a&ticket=opaque".to_string(),
            opaque_ticket: "opaque".to_string(),
            expires_at_unix_millis: u64::MAX,
        };
        let dial = {
            let connection = Arc::clone(&connection);
            tokio::spawn(async move { connection.provider_connection(&source).await })
        };

        let deadline = Instant::now() + Duration::from_secs(2);
        let first = loop {
            if let Some(id) = connection.local_provider_endpoint_id_for_test() {
                break id;
            }
            if Instant::now() >= deadline {
                panic!(
                    "netd-proxied provider_connection must bind a local endpoint (failing closed with 'opened as streams' would never bind)"
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        dial.abort();
        let _ = dial.await;

        let second = connection
            .ensure_local_provider_for_test()
            .await
            .expect("local provider endpoint must stay bound");
        assert_eq!(
            first, second,
            "a proxied session must reuse one local provider endpoint"
        );

        let started = Instant::now();
        connection.close().await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "closing a proxied session with a local provider endpoint must not drain, took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proxied_provider_connection_reuses_a_live_cached_connection() {
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
        let live = HostedConnection::connect(client, server_addr)
            .await
            .unwrap();
        let connection = test_proxied();
        connection.provider_connections.lock().await.insert(
            server_id,
            Arc::new(Mutex::new(Some(
                live.quic_connection().expect("local fixture").clone(),
            ))),
        );

        let reused = connection
            .provider_connection(&ProviderSource {
                provider_id: "provider-a".to_string(),
                endpoint_id: server_id.to_string(),
                direct_url: "wss://unused.invalid/direct?provider=provider-a&ticket=unused"
                    .to_string(),
                opaque_ticket: "unused".to_string(),
                expires_at_unix_millis: u64::MAX,
            })
            .await
            .unwrap();

        assert!(reused.close_reason().is_none());
        assert!(
            connection.local_provider_endpoint_id_for_test().is_none(),
            "a live cached provider connection must not bind a local endpoint"
        );
        connection.close().await;
        live.close().await;
        server_task.await.unwrap();
    }
}
