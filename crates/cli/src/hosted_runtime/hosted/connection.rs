use std::{collections::HashMap, sync::Arc};

use api::heddle::api::v1alpha1::ProviderSource;
use cli_shared::ClientConfig;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMode,
    endpoint::{AckFrequencyConfig, QuicTransportConfig, presets},
};
use tokio::sync::Mutex;

use super::{
    HostedError, Result, VerifiedEndpointDescriptor, provider_transport::ProviderWebSocketTransport,
};

#[derive(Debug)]
pub(super) struct HostedConnection {
    pub(super) endpoint: Endpoint,
    pub(super) connection: iroh::endpoint::Connection,
    provider_transport: Option<ProviderWebSocketTransport>,
    provider_connections:
        Mutex<HashMap<EndpointId, Arc<Mutex<Option<iroh::endpoint::Connection>>>>>,
}

impl HostedConnection {
    pub(super) async fn connect_verified(
        descriptor: &VerifiedEndpointDescriptor,
        config: &ClientConfig,
    ) -> Result<Arc<Self>> {
        heddle_perf_contract::record_network_client_initialization();
        let relays = descriptor.relay_urls()?;
        let address = descriptor.endpoint_addr()?;
        let relay_mode = if relays.is_empty() {
            RelayMode::Disabled
        } else {
            RelayMode::custom(relays)
        };
        let provider_transport = ProviderWebSocketTransport::new(config.clone());
        let endpoint = Endpoint::builder(presets::Minimal)
            .transport_config(transport_config())
            .relay_mode(relay_mode)
            .add_custom_transport(Arc::new(provider_transport.clone()))
            .bind()
            .await
            .map_err(HostedError::transport)?;
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
        Ok(Arc::new(Self {
            endpoint,
            connection,
            provider_transport,
            provider_connections: Mutex::new(HashMap::new()),
        }))
    }

    pub(super) fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    pub(super) fn supports_provider_transport(&self) -> bool {
        self.provider_transport.is_some()
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

        let transport = self.provider_transport.as_ref().ok_or_else(|| {
            HostedError::InvalidDescriptor(
                "the active Iroh endpoint has no provider transport".to_string(),
            )
        })?;
        let address = transport.register_source(
            &source.provider_id,
            &source.endpoint_id,
            &source.direct_url,
            &source.opaque_ticket,
        )?;
        let connection = self
            .endpoint
            .connect(address, api::PROVIDER_ALPN_V1)
            .await
            .map_err(HostedError::transport)?;
        *cached = Some(connection.clone());
        Ok(connection)
    }

    pub(super) async fn close(&self) {
        self.connection.close(0u32.into(), b"Heddle client closed");
        self.endpoint.close().await;
    }
}

impl Drop for HostedConnection {
    fn drop(&mut self) {
        self.connection.close(0u32.into(), b"Heddle client closed");
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
    use std::{net::Ipv4Addr, sync::Arc, time::Duration};

    use api::heddle::api::v1alpha1::ProviderSource;
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
}
