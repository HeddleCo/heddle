use std::sync::Arc;

use iroh::{
    Endpoint, EndpointAddr, RelayMode,
    endpoint::{AckFrequencyConfig, QuicTransportConfig, presets},
};

use super::{HostedError, Result, VerifiedEndpointDescriptor};

#[derive(Debug)]
pub(super) struct HostedConnection {
    pub(super) endpoint: Endpoint,
    pub(super) connection: iroh::endpoint::Connection,
}

impl HostedConnection {
    pub(super) async fn connect_verified(
        descriptor: &VerifiedEndpointDescriptor,
    ) -> Result<Arc<Self>> {
        let relays = descriptor.relay_urls()?;
        let address = descriptor.endpoint_addr()?;
        let relay_mode = if relays.is_empty() {
            RelayMode::Disabled
        } else {
            RelayMode::custom(relays)
        };
        let endpoint = Endpoint::builder(presets::Minimal)
            .transport_config(transport_config())
            .relay_mode(relay_mode)
            .bind()
            .await
            .map_err(HostedError::transport)?;
        Self::connect(endpoint, address).await
    }

    pub(super) async fn connect(endpoint: Endpoint, address: EndpointAddr) -> Result<Arc<Self>> {
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
        }))
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
    use std::{net::Ipv4Addr, time::Duration};

    use iroh::{Endpoint, RelayMode, endpoint::presets};

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
}
