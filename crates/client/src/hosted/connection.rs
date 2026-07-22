use std::sync::Arc;

use iroh::{
    Endpoint, EndpointAddr, RelayMode,
    endpoint::{AckFrequencyConfig, QuicTransportConfig, presets},
};

use super::{HostedError, Result, VerifiedEndpointDescriptor};

#[derive(Debug)]
pub(super) struct HostedConnection {
    pub(super) _endpoint: Endpoint,
    pub(super) connection: iroh::endpoint::Connection,
}

impl HostedConnection {
    pub(super) async fn connect_verified(
        descriptor: &VerifiedEndpointDescriptor,
    ) -> Result<Arc<Self>> {
        let relays = descriptor.relay_urls()?;
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
        Self::connect(endpoint, descriptor.endpoint_addr()?).await
    }

    pub(super) async fn connect(endpoint: Endpoint, address: EndpointAddr) -> Result<Arc<Self>> {
        let connection = endpoint
            .connect(address, api::HOSTED_ALPN_V1)
            .await
            .map_err(HostedError::transport)?;
        Ok(Arc::new(Self {
            _endpoint: endpoint,
            connection,
        }))
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
