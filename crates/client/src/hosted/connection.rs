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
    let mut acknowledgements = AckFrequencyConfig::default();
    acknowledgements.ack_eliciting_threshold(50u32.into());
    QuicTransportConfig::builder()
        .ack_frequency_config(Some(acknowledgements))
        .build()
}
