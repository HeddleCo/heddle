use std::{collections::HashMap, sync::Arc};

use api::heddle::api::v1alpha2::{
    DescribeEndpointResponse, EndpointKind, EndpointRef, ProviderDialRoute,
};
use config::ClientConfig;
use iroh::{Endpoint, EndpointAddr, EndpointId, protocol::Router};
use tokio::sync::Mutex;

use super::{
    HostedError, Result,
    claim_protocol::{ClaimProtocol, NATIVE_ALPN},
    provider_transport::ProviderWebSocketTransport,
};

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
        }))
    }

    pub(super) fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
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
        if let Err(error) = self.router.shutdown().await {
            tracing::warn!(%error, "failed to shut down Heddle Iroh router");
        }
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
    use std::{net::Ipv4Addr, sync::Arc, time::Duration};

    use api::heddle::api::v1alpha2::{EndpointKind, EndpointRef};
    use iroh::{Endpoint, RelayMode, endpoint::presets};
    use tokio::sync::Mutex;

    use super::HostedConnection;

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
