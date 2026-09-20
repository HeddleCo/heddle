// SPDX-License-Identifier: Apache-2.0
//! One discovered v2 endpoint, with independent RPC streams on its connection.
use std::time::Duration;

use anyhow::{Context, Result, bail};
use iroh::{
    Endpoint, EndpointAddr, RelayMode,
    endpoint::{Connection, presets},
};
use thread_api::{
    Remote, contract::EndpointKind, credentials::Credentials, transport::IrohTransport,
};

use crate::{ConnectionOptions, descriptor_url, fetch_endpoint_descriptor};

/// A direct Weft connection. The application retains operation IDs, credentials,
/// observed versions and stream checkpoints; reconnect never retries a mutation.
pub struct HostedClient {
    pub remote: Remote<IrohTransport<Credentials>>,
    endpoint: Endpoint,
    connection: Connection,
}

impl HostedClient {
    /// Verify the application's HTTPS descriptor trust, connect once, and
    /// discover the endpoint's actual v2 implementation inventory.
    pub async fn connect_server(server: &str, options: &ConnectionOptions) -> Result<Self> {
        let descriptor = fetch_endpoint_descriptor(
            &descriptor_url(server)?,
            &options.trusted_descriptors,
            &options.http_client,
        )
        .await?;
        Self::connect_endpoint(
            descriptor.endpoint_addr()?,
            options.credential.clone(),
            options.timeout,
            options.tls_ca_certificate_pem.as_deref(),
        )
        .await
    }

    /// Connect to an endpoint whose public key the application already trusts.
    /// Relay addresses are routing hints; Iroh authenticates the supplied key.
    pub async fn connect_endpoint(
        address: EndpointAddr,
        credential: Credentials,
        timeout: Duration,
        tls_ca_certificate_pem: Option<&str>,
    ) -> Result<Self> {
        if timeout.is_zero() {
            bail!("hosted progress timeout must be positive");
        }
        let relays: Vec<_> = address.relay_urls().cloned().collect();
        let relay_mode = if relays.is_empty() {
            RelayMode::Disabled
        } else {
            RelayMode::custom(relays)
        };
        heddle_perf_contract::record_network_client_initialization();
        let mut builder = Endpoint::builder(presets::Minimal).relay_mode(relay_mode);
        if let Some(pem) = tls_ca_certificate_pem {
            builder = builder.ca_tls_config(crate::relay_tls::ca_tls_config_from_pem(pem)?);
        }
        let endpoint = builder
            .bind()
            .await
            .context("bind hosted client endpoint")?;
        let connection =
            tokio::time::timeout(timeout, endpoint.connect(address, api::HOSTED_ALPN_V1))
                .await
                .context("hosted connection deadline elapsed")?
                .context("connect to hosted endpoint")?;
        let result = async {
            let transport = IrohTransport::new(
                connection.clone(),
                credential,
                api::framing::MAX_CONTROL_BODY,
                timeout,
            )?;
            Remote::discover(
                transport,
                *connection.remote_id().as_bytes(),
                EndpointKind::Weft,
            )
            .await
            .context("discover hosted v2 API")
        }
        .await;
        match result {
            Ok(remote) => Ok(Self {
                remote,
                endpoint,
                connection,
            }),
            Err(error) => {
                connection.close(0u32.into(), b"v2 discovery failed");
                endpoint.close().await;
                Err(error)
            }
        }
    }

    pub fn local_endpoint_id(&self) -> iroh::EndpointId {
        self.endpoint.id()
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Release the discovered remote and the live Iroh session without closing.
    ///
    /// The caller becomes responsible for closing `connection` / `endpoint`.
    pub fn into_parts(self) -> (Remote<IrohTransport<Credentials>>, Endpoint, Connection) {
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: `this` is forgotten, so Drop will not close the connection.
        // Each field is read exactly once.
        unsafe {
            (
                std::ptr::read(&this.remote),
                std::ptr::read(&this.endpoint),
                std::ptr::read(&this.connection),
            )
        }
    }

    /// Cancel this connection's streams and await endpoint shutdown.
    pub async fn close(&self) {
        self.connection.close(0u32.into(), b"hosted client closed");
        self.endpoint.close().await;
    }
}

impl Drop for HostedClient {
    fn drop(&mut self) {
        self.connection.close(0u32.into(), b"hosted client dropped");
    }
}
