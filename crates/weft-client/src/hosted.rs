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

const ENDPOINT_CLOSE_TIMEOUT: Duration = Duration::from_secs(3);

async fn close_endpoint(endpoint: &Endpoint) {
    let _ = tokio::time::timeout(ENDPOINT_CLOSE_TIMEOUT, endpoint.close()).await;
}

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
        Self::connect_endpoint_with_description(
            address,
            credential,
            timeout,
            tls_ca_certificate_pem,
            None,
        )
        .await
    }

    /// Connect with a description authenticated and retained by the application.
    /// A mismatched description is discarded and rediscovered on this connection.
    pub async fn connect_endpoint_with_description(
        address: EndpointAddr,
        credential: Credentials,
        timeout: Duration,
        tls_ca_certificate_pem: Option<&str>,
        description: Option<thread_api::contract::DescribeEndpointResponse>,
    ) -> Result<Self> {
        if timeout.is_zero() {
            bail!("hosted progress timeout must be positive");
        }
        let endpoint = Self::bind_endpoint(tls_ca_certificate_pem).await?;
        Self::connect_bound_endpoint(
            endpoint,
            address,
            credential,
            timeout,
            description,
            tls_ca_certificate_pem,
        )
        .await
    }

    /// Bind without routing hints so HTTPS trust discovery can run concurrently.
    /// Only verified relays are installed before connecting.
    pub async fn bind_endpoint(tls_ca_certificate_pem: Option<&str>) -> Result<Endpoint> {
        Self::bind_with_relays(RelayMode::Disabled, tls_ca_certificate_pem).await
    }

    async fn bind_with_relays(
        relay_mode: RelayMode,
        tls_ca_certificate_pem: Option<&str>,
    ) -> Result<Endpoint> {
        let mut builder = Endpoint::builder(presets::Minimal).relay_mode(relay_mode);
        if let Some(pem) = tls_ca_certificate_pem {
            builder = builder.ca_tls_config(crate::relay_tls::ca_tls_config_from_pem(pem)?);
        }
        builder.bind().await.context("bind hosted client endpoint")
    }

    /// Finish connecting an independently bound endpoint to a verified address.
    pub async fn connect_bound_endpoint(
        mut endpoint: Endpoint,
        address: EndpointAddr,
        credential: Credentials,
        timeout: Duration,
        description: Option<thread_api::contract::DescribeEndpointResponse>,
        tls_ca_certificate_pem: Option<&str>,
    ) -> Result<Self> {
        if timeout.is_zero() {
            close_endpoint(&endpoint).await;
            bail!("hosted progress timeout must be positive");
        }
        let relays: Vec<_> = address.relay_urls().cloned().collect();
        if !relays.is_empty() {
            // Relay transport configuration depends on the verified addresses.
            // Do not keep an empty relay actor: it emits shutdown errors on
            // direct-only connections and contaminates JSON diagnostics.
            close_endpoint(&endpoint).await;
            endpoint =
                Self::bind_with_relays(RelayMode::custom(relays), tls_ca_certificate_pem).await?;
        }
        heddle_perf_contract::record_network_client_initialization();
        let connection =
            match tokio::time::timeout(timeout, endpoint.connect(address, api::HOSTED_ALPN_V1))
                .await
                .context("hosted connection deadline elapsed")
                .and_then(|connection| connection.context("connect to hosted endpoint"))
            {
                Ok(connection) => connection,
                Err(error) => {
                    close_endpoint(&endpoint).await;
                    return Err(error);
                }
            };
        let result = async {
            let transport = IrohTransport::new(
                connection.clone(),
                credential,
                api::framing::MAX_CONTROL_BODY,
                timeout,
            )?;
            if let Some(description) = description {
                // Validate before consuming the transport so a mismatch can
                // still fall through to discovery over the same connection.
                let key = connection.remote_id();
                if description.endpoint.as_ref().is_some_and(|source| {
                    source.public_key == key.as_bytes() && source.kind == EndpointKind::Weft as i32
                }) && description
                    .supported_packages
                    .iter()
                    .any(|package| package == "heddle.api.v1alpha2")
                {
                    return Remote::from_description(
                        transport,
                        *key.as_bytes(),
                        EndpointKind::Weft,
                        description,
                    )
                    .context("reuse hosted v2 API");
                }
            }
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
                close_endpoint(&endpoint).await;
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
        close_endpoint(&self.endpoint).await;
    }
}

impl Drop for HostedClient {
    fn drop(&mut self) {
        self.connection.close(0u32.into(), b"hosted client dropped");
    }
}
