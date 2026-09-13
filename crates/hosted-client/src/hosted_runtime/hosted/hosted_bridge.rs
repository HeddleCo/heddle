// SPDX-License-Identifier: Apache-2.0
//! Cross-process warm hosted-connection holder (heddle netd).
//!
//! Each CLI process used to bind a fresh Iroh endpoint, dial the
//! signed relay, and open a new QUIC session to weft. `heddle netd
//! serve` already keeps one persistent endpoint homed on the build's
//! Heddle relay. This bridge lets whoami / push / pull / clone reuse
//! that endpoint and a cached weft QUIC connection across invocations.
//!
//! The daemon never sees credentials: the CLI still signs every call.
//! The socket carries an Ensure/OpenBi handshake, then raw QUIC-stream
//! bytes. Same-uid, mode-0600, fail-closed — the claim-bridge pattern.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use api::heddle::api::v1alpha1::ProviderSource;
use config::ClientConfig;
use iroh::{Endpoint, EndpointId};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::Mutex,
};

use super::{
    HostedError, VerifiedEndpointDescriptor, provider_transport::ProviderWebSocketTransport,
    resolver::resolve_and_verify_endpoint_descriptor,
};

const MAX_BRIDGE_FRAME: usize = 64 * 1024;

/// Box-scoped path of the hosted-connection bridge socket:
/// `<heddle_home>/state/heddle-netd-hosted.sock`.
pub fn hosted_bridge_socket_path(heddle_home: &Path) -> PathBuf {
    repo::daemon::box_state_dir_in(heddle_home).join("heddle-netd-hosted.sock")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum HostedBridgeRequest {
    Ensure {
        server: String,
        allow_insecure: bool,
    },
    OpenBi {
        server: String,
        allow_insecure: bool,
        #[serde(default)]
        provider: Option<ProviderTarget>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProviderTarget {
    provider_id: String,
    endpoint_id: String,
    direct_url: String,
    opaque_ticket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum HostedBridgeResponse {
    Ready { reused: bool, node_id: String },
    Opened { reused: bool },
    Error { message: String },
}

struct CachedWeft {
    connection: iroh::endpoint::Connection,
    expires_at_unix_millis: i64,
}

/// Warm weft/provider connections on the daemon's persistent endpoint.
pub struct HostedBridge {
    endpoint: Endpoint,
    provider_transport: Option<ProviderWebSocketTransport>,
    weft: Mutex<HashMap<String, CachedWeft>>,
    providers: Mutex<HashMap<EndpointId, iroh::endpoint::Connection>>,
}

impl HostedBridge {
    #[must_use]
    pub(crate) fn new(
        endpoint: Endpoint,
        provider_transport: Option<ProviderWebSocketTransport>,
    ) -> Self {
        Self {
            endpoint,
            provider_transport,
            weft: Mutex::new(HashMap::new()),
            providers: Mutex::new(HashMap::new()),
        }
    }

    /// Test helper: insert an already-open weft connection so Ensure/OpenBi
    /// can exercise reuse without an HTTPS descriptor fetch.
    #[cfg(test)]
    pub async fn insert_weft_for_test(
        &self,
        server: impl Into<String>,
        connection: iroh::endpoint::Connection,
    ) {
        self.insert_weft_with_expiry_for_test(server, connection, i64::MAX)
            .await;
    }

    #[cfg(test)]
    pub async fn insert_weft_with_expiry_for_test(
        &self,
        server: impl Into<String>,
        connection: iroh::endpoint::Connection,
        expires_at_unix_millis: i64,
    ) {
        self.weft.lock().await.insert(
            server.into(),
            CachedWeft {
                connection,
                expires_at_unix_millis,
            },
        );
    }

    pub async fn serve(self, socket_path: PathBuf) -> Result<()> {
        let listener = bind_bridge_listener(&socket_path).with_context(|| {
            format!(
                "binding hosted connection bridge socket {}",
                socket_path.display()
            )
        })?;
        let bridge = Arc::new(self);
        loop {
            match listener.accept().await {
                Ok((stream, _)) if peer_is_same_uid(&stream) => {
                    let bridge = Arc::clone(&bridge);
                    tokio::spawn(async move {
                        if let Err(error) = handle_client(bridge, stream).await {
                            tracing::debug!(%error, "hosted bridge session ended");
                        }
                    });
                }
                Ok((stream, _)) => {
                    tracing::warn!("rejecting hosted-bridge client: peer uid mismatch");
                    drop(stream);
                }
                Err(error) => {
                    tracing::warn!(%error, "hosted bridge accept failed");
                }
            }
        }
    }
}

async fn handle_client(bridge: Arc<HostedBridge>, mut stream: UnixStream) -> Result<()> {
    let request = read_frame(&mut stream)
        .await?
        .context("hosted bridge client closed before sending a request")?;
    let request: HostedBridgeRequest =
        serde_json::from_slice(&request).context("decoding hosted bridge request")?;
    match request {
        HostedBridgeRequest::Ensure {
            server,
            allow_insecure,
        } => {
            let outcome = ensure_weft(&bridge, &server, allow_insecure).await;
            let response = match outcome {
                Ok(reused) => HostedBridgeResponse::Ready {
                    reused,
                    node_id: bridge.endpoint.id().to_string(),
                },
                Err(error) => HostedBridgeResponse::Error {
                    message: error.to_string(),
                },
            };
            write_frame(&mut stream, &serde_json::to_vec(&response)?).await?;
        }
        HostedBridgeRequest::OpenBi {
            server,
            allow_insecure,
            provider,
        } => {
            let opened = match provider {
                Some(ref target) => open_provider_bi(&bridge, target).await.map(|()| false),
                None => ensure_weft(&bridge, &server, allow_insecure).await,
            };
            match opened {
                Ok(reused) => {
                    write_frame(
                        &mut stream,
                        &serde_json::to_vec(&HostedBridgeResponse::Opened { reused })?,
                    )
                    .await?;
                    if let Some(target) = provider {
                        splice_provider(&bridge, &target, stream).await?;
                    } else {
                        splice_weft(&bridge, &server, stream).await?;
                    }
                }
                Err(error) => {
                    write_frame(
                        &mut stream,
                        &serde_json::to_vec(&HostedBridgeResponse::Error {
                            message: error.to_string(),
                        })?,
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
}

async fn ensure_weft(bridge: &HostedBridge, server: &str, allow_insecure: bool) -> Result<bool> {
    if live_weft(bridge, server).await.is_some() {
        return Ok(true);
    }
    let mut config = ClientConfig::default();
    if allow_insecure {
        config.allow_insecure = true;
    }
    let descriptor = resolve_and_verify_endpoint_descriptor(server, &config)
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let connection = connect_weft(&bridge.endpoint, &descriptor).await?;
    let expires_at_unix_millis = descriptor.document().expires_at_unix_millis;
    bridge.weft.lock().await.insert(
        server.to_string(),
        CachedWeft {
            connection,
            expires_at_unix_millis,
        },
    );
    Ok(false)
}

async fn live_weft(bridge: &HostedBridge, server: &str) -> Option<iroh::endpoint::Connection> {
    let now = now_unix_millis().ok()?;
    let mut sessions = bridge.weft.lock().await;
    let cached = sessions.get(server)?;
    if cached.expires_at_unix_millis <= now || cached.connection.close_reason().is_some() {
        sessions.remove(server);
        return None;
    }
    Some(cached.connection.clone())
}

async fn connect_weft(
    endpoint: &Endpoint,
    descriptor: &VerifiedEndpointDescriptor,
) -> Result<iroh::endpoint::Connection> {
    let relays = descriptor
        .relay_urls()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let address = descriptor
        .endpoint_addr()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let direct_address = descriptor
        .direct_endpoint_addr()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    if direct_address.ip_addrs().next().is_some() {
        let direct = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            endpoint.connect(direct_address, api::HOSTED_ALPN_V1),
        )
        .await;
        if let Ok(Ok(connection)) = direct {
            return Ok(connection);
        }
        if relays.is_empty() {
            anyhow::bail!("signed direct addresses unavailable and the descriptor has no relays");
        }
    }
    endpoint
        .connect(address, api::HOSTED_ALPN_V1)
        .await
        .context("connecting to weft through the netd endpoint")
}

async fn open_provider_bi(bridge: &HostedBridge, target: &ProviderTarget) -> Result<()> {
    let _ = provider_connection(bridge, target).await?;
    Ok(())
}

async fn provider_connection(
    bridge: &HostedBridge,
    target: &ProviderTarget,
) -> Result<iroh::endpoint::Connection> {
    let endpoint_id: EndpointId = target.endpoint_id.parse().context("provider endpoint id")?;
    {
        let providers = bridge.providers.lock().await;
        if let Some(connection) = providers.get(&endpoint_id)
            && connection.close_reason().is_none()
        {
            return Ok(connection.clone());
        }
    }
    let transport = bridge
        .provider_transport
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("the network daemon endpoint has no provider transport"))?;
    let address = transport
        .register_source(
            &target.provider_id,
            &target.endpoint_id,
            &target.direct_url,
            &target.opaque_ticket,
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let connection = bridge
        .endpoint
        .connect(address, api::PROVIDER_ALPN_V1)
        .await
        .context("connecting to a provider through the netd endpoint")?;
    bridge
        .providers
        .lock()
        .await
        .insert(endpoint_id, connection.clone());
    Ok(connection)
}

async fn splice_weft(bridge: &HostedBridge, server: &str, stream: UnixStream) -> Result<()> {
    let connection = live_weft(bridge, server)
        .await
        .context("weft session vanished after OpenBi")?;
    splice_connection(connection, stream).await
}

async fn splice_provider(
    bridge: &HostedBridge,
    target: &ProviderTarget,
    stream: UnixStream,
) -> Result<()> {
    let connection = provider_connection(bridge, target).await?;
    splice_connection(connection, stream).await
}

async fn splice_connection(
    connection: iroh::endpoint::Connection,
    stream: UnixStream,
) -> Result<()> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .context("opening a hosted stream on the warm weft connection")?;
    let (mut unix_read, mut unix_write) = stream.into_split();
    let to_weft = async {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match unix_read.read(&mut buf).await {
                Ok(0) => {
                    let _ = send.finish();
                    break;
                }
                Ok(n) => {
                    send.write_all(&buf[..n])
                        .await
                        .context("writing hosted stream to weft")?;
                }
                Err(error) => {
                    let _ = send.reset(1u32.into());
                    return Err(error).context("reading hosted-bridge client bytes");
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let from_weft = async {
        loop {
            match recv.read_chunk(64 * 1024).await {
                Ok(Some(chunk)) => {
                    unix_write
                        .write_all(&chunk)
                        .await
                        .context("writing weft bytes to the hosted-bridge client")?;
                }
                Ok(None) => break,
                Err(error) => return Err(error).context("reading weft stream"),
            }
        }
        let _ = unix_write.shutdown().await;
        Ok::<(), anyhow::Error>(())
    };
    let (to_weft, from_weft) = tokio::join!(to_weft, from_weft);
    to_weft?;
    from_weft?;
    Ok(())
}

/// Ask a running netd to hold (or reuse) a weft connection for `server`.
pub async fn ensure_via_netd(
    socket_path: &Path,
    server: &str,
    allow_insecure: bool,
) -> std::result::Result<EnsureOutcome, HostedError> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .map_err(HostedError::transport)?;
    let request = HostedBridgeRequest::Ensure {
        server: server.to_string(),
        allow_insecure,
    };
    write_frame(
        &mut stream,
        &serde_json::to_vec(&request).map_err(HostedError::transport)?,
    )
    .await
    .map_err(HostedError::transport)?;
    let response = read_frame(&mut stream)
        .await
        .map_err(HostedError::transport)?
        .ok_or_else(|| HostedError::transport("netd hosted bridge closed during Ensure"))?;
    match serde_json::from_slice(&response).map_err(HostedError::transport)? {
        HostedBridgeResponse::Ready { reused, node_id } => {
            let node_id = node_id.parse().map_err(HostedError::transport)?;
            Ok(EnsureOutcome { reused, node_id })
        }
        HostedBridgeResponse::Error { message } => Err(HostedError::transport(message)),
        HostedBridgeResponse::Opened { .. } => Err(HostedError::transport(
            "netd hosted bridge returned Opened for Ensure",
        )),
    }
}

#[derive(Debug)]
pub struct EnsureOutcome {
    pub reused: bool,
    pub node_id: EndpointId,
}

/// Open a bidirectional stream on the warm weft (or provider) connection.
pub async fn open_bi_via_netd(
    socket_path: &Path,
    server: &str,
    allow_insecure: bool,
    provider: Option<&ProviderSource>,
) -> std::result::Result<(UnixStream, bool), HostedError> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .map_err(HostedError::transport)?;
    let request = HostedBridgeRequest::OpenBi {
        server: server.to_string(),
        allow_insecure,
        provider: provider.map(|source| ProviderTarget {
            provider_id: source.provider_id.clone(),
            endpoint_id: source.endpoint_id.clone(),
            direct_url: source.direct_url.clone(),
            opaque_ticket: source.opaque_ticket.clone(),
        }),
    };
    write_frame(
        &mut stream,
        &serde_json::to_vec(&request).map_err(HostedError::transport)?,
    )
    .await
    .map_err(HostedError::transport)?;
    let response = read_frame(&mut stream)
        .await
        .map_err(HostedError::transport)?
        .ok_or_else(|| HostedError::transport("netd hosted bridge closed during OpenBi"))?;
    match serde_json::from_slice(&response).map_err(HostedError::transport)? {
        HostedBridgeResponse::Opened { reused } => Ok((stream, reused)),
        HostedBridgeResponse::Error { message } => Err(HostedError::transport(message)),
        HostedBridgeResponse::Ready { .. } => Err(HostedError::transport(
            "netd hosted bridge returned Ready for OpenBi",
        )),
    }
}

fn bind_bridge_listener(socket_path: &Path) -> Result<UnixListener> {
    let listener =
        repo::daemon::bind_unix_socket(socket_path).map_err(|error| anyhow::anyhow!("{error}"))?;
    listener
        .set_nonblocking(true)
        .context("marking hosted bridge socket non-blocking")?;
    UnixListener::from_std(listener).context("adopting hosted bridge socket into the async runtime")
}

fn peer_is_same_uid(stream: &UnixStream) -> bool {
    match stream.peer_cred() {
        Ok(peer) => peer.uid() == unsafe { libc::getuid() },
        Err(error) => {
            tracing::warn!(%error, "could not read hosted-bridge peer credentials");
            false
        }
    }
}

async fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> Result<()> {
    let length = u32::try_from(payload.len()).context("hosted bridge frame is too large")?;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_frame(stream: &mut UnixStream) -> Result<Option<Vec<u8>>> {
    let mut length = [0u8; 4];
    match stream.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("reading hosted bridge frame length"),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_BRIDGE_FRAME {
        anyhow::bail!("hosted bridge frame of {length} bytes exceeds the {MAX_BRIDGE_FRAME} limit");
    }
    let mut payload = vec![0u8; length];
    stream
        .read_exact(&mut payload)
        .await
        .context("reading hosted bridge frame body")?;
    Ok(Some(payload))
}

fn now_unix_millis() -> Result<i64> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_millis();
    i64::try_from(millis).context("timestamp overflow")
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        net::Ipv4Addr,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use iroh::{Endpoint, RelayMode, endpoint::presets};
    use tempfile::TempDir;

    use super::*;

    async fn echo_server(accepts: Arc<AtomicUsize>) -> (Endpoint, tokio::task::JoinHandle<()>) {
        let server = Endpoint::builder(presets::Minimal)
            .alpns(vec![api::HOSTED_ALPN_V1.to_vec()])
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let accept_endpoint = server.clone();
        let task = tokio::spawn(async move {
            while let Some(incoming) = accept_endpoint.accept().await {
                accepts.fetch_add(1, Ordering::SeqCst);
                let Ok(connection) = incoming.await else {
                    continue;
                };
                tokio::spawn(async move {
                    while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                        let bytes = recv.read_to_end(64 * 1024).await.unwrap_or_default();
                        let _ = send.write_all(&bytes).await;
                        let _ = send.finish();
                    }
                });
            }
        });
        (server, task)
    }

    #[tokio::test]
    async fn warm_open_bi_reuses_the_cached_weft_connection() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let (server, server_task) = echo_server(Arc::clone(&accepts)).await;
        let netd = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let first = netd
            .connect(server.addr(), api::HOSTED_ALPN_V1)
            .await
            .unwrap();
        let home = TempDir::new().unwrap();
        let socket = hosted_bridge_socket_path(home.path());
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let bridge = HostedBridge::new(netd, None);
        bridge
            .insert_weft_for_test("https://api.test.heddle.sh", first)
            .await;
        let serve = tokio::spawn(bridge.serve(socket.clone()));

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !socket.exists() {
            if std::time::Instant::now() >= deadline {
                panic!("hosted bridge socket did not appear");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let ensure_first = ensure_via_netd(&socket, "https://api.test.heddle.sh", false)
            .await
            .unwrap();
        assert!(
            ensure_first.reused,
            "pre-seeded weft session must be reported as reused"
        );

        let (mut stream, reused) =
            open_bi_via_netd(&socket, "https://api.test.heddle.sh", false, None)
                .await
                .unwrap();
        assert!(reused, "OpenBi on a warm session must not cold-connect");
        stream.write_all(b"ping-one").await.unwrap();
        stream.shutdown().await.unwrap();
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await.unwrap();
        assert_eq!(reply, b"ping-one");

        let (mut stream, reused) =
            open_bi_via_netd(&socket, "https://api.test.heddle.sh", false, None)
                .await
                .unwrap();
        assert!(reused, "second OpenBi must reuse the same weft connection");
        stream.write_all(b"ping-two").await.unwrap();
        stream.shutdown().await.unwrap();
        reply.clear();
        stream.read_to_end(&mut reply).await.unwrap();
        assert_eq!(reply, b"ping-two");

        assert_eq!(
            accepts.load(Ordering::SeqCst),
            1,
            "warm reuse must not open a second QUIC connection to weft"
        );

        serve.abort();
        server_task.abort();
        let _ = serve.await;
        let _ = server_task.await;
        server.close().await;
    }

    pub(crate) const TEST_WEFT_SERVER: &str = "https://api.test.heddle.sh";

    pub(crate) struct WarmBridgeFixture {
        pub home: TempDir,
        pub socket: PathBuf,
        pub node_id: iroh::EndpointId,
        accepts: Arc<AtomicUsize>,
        serve: Option<tokio::task::JoinHandle<Result<()>>>,
        server_task: Option<tokio::task::JoinHandle<()>>,
        server: Option<Endpoint>,
    }

    impl WarmBridgeFixture {
        pub async fn start() -> Self {
            let accepts = Arc::new(AtomicUsize::new(0));
            let (server, server_task) = echo_server(Arc::clone(&accepts)).await;
            let netd = Endpoint::builder(presets::Minimal)
                .relay_mode(RelayMode::Disabled)
                .bind_addr((Ipv4Addr::LOCALHOST, 0))
                .unwrap()
                .bind()
                .await
                .unwrap();
            let node_id = netd.id();
            let first = netd
                .connect(server.addr(), api::HOSTED_ALPN_V1)
                .await
                .unwrap();
            let home = TempDir::new().unwrap();
            let socket = hosted_bridge_socket_path(home.path());
            if let Some(parent) = socket.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            let bridge = HostedBridge::new(netd, None);
            bridge.insert_weft_for_test(TEST_WEFT_SERVER, first).await;
            let serve = tokio::spawn(bridge.serve(socket.clone()));
            wait_for_socket(&socket).await;
            Self {
                home,
                socket,
                node_id,
                accepts,
                serve: Some(serve),
                server_task: Some(server_task),
                server: Some(server),
            }
        }

        pub fn accepts(&self) -> usize {
            self.accepts.load(Ordering::SeqCst)
        }
    }

    impl Drop for WarmBridgeFixture {
        fn drop(&mut self) {
            if let Some(task) = self.serve.take() {
                task.abort();
            }
            if let Some(task) = self.server_task.take() {
                task.abort();
            }
            if let Some(server) = self.server.take() {
                tokio::spawn(async move {
                    server.close().await;
                });
            }
        }
    }

    pub(crate) struct PinHeddleHome {
        previous: Option<std::ffi::OsString>,
    }

    impl PinHeddleHome {
        pub fn new(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("HEDDLE_HOME");
            unsafe {
                std::env::set_var("HEDDLE_HOME", path);
            }
            Self { previous }
        }
    }

    impl Drop for PinHeddleHome {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("HEDDLE_HOME", value) },
                None => unsafe { std::env::remove_var("HEDDLE_HOME") },
            }
        }
    }

    async fn wait_for_socket(socket: &std::path::Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !socket.exists() {
            if std::time::Instant::now() >= deadline {
                panic!("hosted bridge socket did not appear");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn start_empty_bridge() -> (TempDir, PathBuf, tokio::task::JoinHandle<Result<()>>) {
        let netd = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let home = TempDir::new().unwrap();
        let socket = hosted_bridge_socket_path(home.path());
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let serve = tokio::spawn(HostedBridge::new(netd, None).serve(socket.clone()));
        wait_for_socket(&socket).await;
        (home, socket, serve)
    }

    #[tokio::test]
    async fn ensure_without_a_cached_weft_fails_closed() {
        let (_home, socket, serve) = start_empty_bridge().await;
        let error = ensure_via_netd(&socket, TEST_WEFT_SERVER, true)
            .await
            .expect_err("cold Ensure must not invent a weft session");
        assert!(
            !error.to_string().is_empty(),
            "descriptor fetch failure must surface on Ensure"
        );
        serve.abort();
        let _ = serve.await;
    }

    #[tokio::test]
    async fn open_bi_without_a_cached_weft_fails_closed() {
        let (_home, socket, serve) = start_empty_bridge().await;
        let error = open_bi_via_netd(&socket, TEST_WEFT_SERVER, false, None)
            .await
            .expect_err("cold OpenBi must not invent a weft session");
        assert!(!error.to_string().is_empty());
        serve.abort();
        let _ = serve.await;
    }

    #[tokio::test]
    async fn open_bi_provider_without_transport_fails_closed() {
        let fixture = WarmBridgeFixture::start().await;
        let endpoint_id = iroh_base::SecretKey::generate().public();
        let source = ProviderSource {
            provider_id: "provider-a".to_string(),
            endpoint_id: endpoint_id.to_string(),
            direct_url: "wss://127.0.0.1:1/direct?provider=provider-a&ticket=opaque".to_string(),
            opaque_ticket: "opaque".to_string(),
            expires_at_unix_millis: u64::MAX,
        };
        let error = open_bi_via_netd(&fixture.socket, TEST_WEFT_SERVER, false, Some(&source))
            .await
            .expect_err("provider OpenBi on a weft-only bridge must fail closed");
        assert!(
            error.to_string().contains("provider transport"),
            "got {error}"
        );
    }

    #[tokio::test]
    async fn expired_weft_is_evicted_before_ensure() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let (server, server_task) = echo_server(Arc::clone(&accepts)).await;
        let netd = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let first = netd
            .connect(server.addr(), api::HOSTED_ALPN_V1)
            .await
            .unwrap();
        let home = TempDir::new().unwrap();
        let socket = hosted_bridge_socket_path(home.path());
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let bridge = HostedBridge::new(netd, None);
        bridge
            .insert_weft_with_expiry_for_test(TEST_WEFT_SERVER, first, 1)
            .await;
        let serve = tokio::spawn(bridge.serve(socket.clone()));
        wait_for_socket(&socket).await;
        let error = ensure_via_netd(&socket, TEST_WEFT_SERVER, false)
            .await
            .expect_err("expired cache must not report reuse");
        assert!(!error.to_string().is_empty());
        serve.abort();
        server_task.abort();
        let _ = serve.await;
        let _ = server_task.await;
        server.close().await;
    }

    #[tokio::test]
    async fn closed_weft_is_evicted_before_ensure() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let (server, server_task) = echo_server(Arc::clone(&accepts)).await;
        let netd = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let first = netd
            .connect(server.addr(), api::HOSTED_ALPN_V1)
            .await
            .unwrap();
        first.close(0u32.into(), b"test");
        let home = TempDir::new().unwrap();
        let socket = hosted_bridge_socket_path(home.path());
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let bridge = HostedBridge::new(netd, None);
        bridge.insert_weft_for_test(TEST_WEFT_SERVER, first).await;
        let serve = tokio::spawn(bridge.serve(socket.clone()));
        wait_for_socket(&socket).await;
        let error = ensure_via_netd(&socket, TEST_WEFT_SERVER, false)
            .await
            .expect_err("a closed weft session must be evicted");
        assert!(!error.to_string().is_empty());
        serve.abort();
        server_task.abort();
        let _ = serve.await;
        let _ = server_task.await;
        server.close().await;
    }

    #[tokio::test]
    async fn missing_bridge_socket_fails_ensure_and_open_bi() {
        let missing = std::path::Path::new("/tmp/heddle-hosted-missing-e0c3.sock");
        assert!(
            ensure_via_netd(missing, TEST_WEFT_SERVER, false)
                .await
                .is_err()
        );
        assert!(
            open_bi_via_netd(missing, TEST_WEFT_SERVER, false, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn oversized_bridge_frame_is_rejected() {
        let fixture = WarmBridgeFixture::start().await;
        let mut stream = UnixStream::connect(&fixture.socket).await.unwrap();
        let length = (MAX_BRIDGE_FRAME as u32).saturating_add(1);
        stream.write_all(&length.to_be_bytes()).await.unwrap();
        stream.write_all(&[0u8; 8]).await.unwrap();
        stream.flush().await.unwrap();
        let mut reply = Vec::new();
        let _ = stream.read_to_end(&mut reply).await;
        assert!(
            reply.is_empty(),
            "an oversized frame must close the session without a handshake"
        );
    }

    #[tokio::test]
    async fn client_disconnect_before_request_ends_the_session() {
        let fixture = WarmBridgeFixture::start().await;
        let stream = UnixStream::connect(&fixture.socket).await.unwrap();
        drop(stream);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let ensured = ensure_via_netd(&fixture.socket, TEST_WEFT_SERVER, false)
            .await
            .unwrap();
        assert!(ensured.reused);
    }

    #[tokio::test]
    async fn ensure_rejects_an_opened_response() {
        let home = TempDir::new().unwrap();
        let socket = hosted_bridge_socket_path(home.path());
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let serve = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_frame(&mut stream).await;
            let response =
                serde_json::to_vec(&HostedBridgeResponse::Opened { reused: true }).unwrap();
            let _ = write_frame(&mut stream, &response).await;
        });
        let error = ensure_via_netd(&socket, TEST_WEFT_SERVER, false)
            .await
            .expect_err("Opened is not a valid Ensure reply");
        assert!(
            error.to_string().contains("Opened for Ensure"),
            "got {error}"
        );
        serve.abort();
        let _ = serve.await;
    }

    #[tokio::test]
    async fn open_bi_rejects_a_ready_response() {
        let home = TempDir::new().unwrap();
        let socket = hosted_bridge_socket_path(home.path());
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let serve = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_frame(&mut stream).await;
            let response = serde_json::to_vec(&HostedBridgeResponse::Ready {
                reused: true,
                node_id: iroh_base::SecretKey::generate().public().to_string(),
            })
            .unwrap();
            let _ = write_frame(&mut stream, &response).await;
        });
        let error = open_bi_via_netd(&socket, TEST_WEFT_SERVER, false, None)
            .await
            .expect_err("Ready is not a valid OpenBi reply");
        assert!(
            error.to_string().contains("Ready for OpenBi"),
            "got {error}"
        );
        serve.abort();
        let _ = serve.await;
    }

    #[tokio::test]
    async fn connect_weft_uses_signed_direct_addresses() {
        let accepts = Arc::new(AtomicUsize::new(0));
        let (server, server_task) = echo_server(Arc::clone(&accepts)).await;
        let descriptor = crate::hosted_runtime::hosted::connection_path_tests::verified_descriptor(
            server.id(),
            Vec::new(),
            server.addr().ip_addrs().map(ToString::to_string).collect(),
        );
        let client = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let connection = connect_weft(&client, &descriptor).await.unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        send.write_all(b"direct").await.unwrap();
        send.finish().unwrap();
        let reply = recv.read_to_end(64 * 1024).await.unwrap();
        assert_eq!(reply, b"direct");
        assert_eq!(accepts.load(Ordering::SeqCst), 1);
        client.close().await;
        server_task.abort();
        let _ = server_task.await;
        server.close().await;
    }

    #[tokio::test]
    async fn connect_weft_fails_closed_when_direct_and_relays_are_gone() {
        let descriptor = crate::hosted_runtime::hosted::connection_path_tests::verified_descriptor(
            iroh_base::SecretKey::generate().public(),
            Vec::new(),
            vec!["127.0.0.1:9".to_string()],
        );
        let client = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .bind()
            .await
            .unwrap();
        let error = connect_weft(&client, &descriptor)
            .await
            .expect_err("dead direct + no relays must fail closed");
        assert!(error.to_string().contains("no relays"), "got {error}");
        client.close().await;
    }
}
