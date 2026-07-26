use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use api::{
    HOSTED_ALPN_V1,
    heddle::api::v1alpha1::{EndpointDescriptor, SignedEndpointDescriptor},
    signing::endpoint_descriptor_bytes,
};
use cli_shared::ClientConfig;
use crypto::Ed25519Signer;
use iroh::{EndpointAddr, EndpointId, RelayUrl};
use prost::Message;
use reqwest::{
    Client,
    header::{HOST, HeaderValue},
};

use super::{HostedError, Result};

const MAX_DESCRIPTOR_BYTES: usize = 64 * 1024;

/// Trusted descriptor-signing keys, keyed independently from Iroh endpoint and
/// hosted capability identities.
#[derive(Debug, Clone, Default)]
pub struct DescriptorKeyring {
    keys: HashMap<String, TrustedKey>,
}

#[derive(Debug, Clone)]
struct TrustedKey {
    public_key: [u8; 32],
    not_before_unix_millis: i64,
    not_after_unix_millis: i64,
}

impl DescriptorKeyring {
    pub fn insert(
        &mut self,
        key_id: impl Into<String>,
        public_key: [u8; 32],
        not_before_unix_millis: i64,
        not_after_unix_millis: i64,
    ) -> Result<()> {
        let key_id = key_id.into();
        if key_id.is_empty() || not_before_unix_millis >= not_after_unix_millis {
            return Err(HostedError::InvalidDescriptor(
                "descriptor trust key has an invalid id or validity window".to_string(),
            ));
        }
        self.keys.insert(
            key_id,
            TrustedKey {
                public_key,
                not_before_unix_millis,
                not_after_unix_millis,
            },
        );
        Ok(())
    }

    pub fn verify(
        &self,
        signed: &SignedEndpointDescriptor,
        now_unix_millis: i64,
    ) -> Result<VerifiedEndpointDescriptor> {
        let descriptor = signed.descriptor.as_ref().ok_or_else(|| {
            HostedError::InvalidDescriptor("signed descriptor has no document".to_string())
        })?;
        validate_descriptor(descriptor, now_unix_millis)?;
        let key = self
            .keys
            .get(&signed.key_id)
            .filter(|key| {
                now_unix_millis >= key.not_before_unix_millis
                    && now_unix_millis < key.not_after_unix_millis
            })
            .ok_or_else(|| {
                HostedError::InvalidDescriptor("descriptor signing key is not trusted".to_string())
            })?;
        Ed25519Signer::verify_with_public_key(
            &endpoint_descriptor_bytes(descriptor),
            &key.public_key,
            &signed.signature,
        )
        .map_err(|_| HostedError::InvalidDescriptorSignature)?;
        Ok(VerifiedEndpointDescriptor(descriptor.clone()))
    }

    pub fn install_rotation(&mut self, verified: &VerifiedEndpointDescriptor) -> Result<()> {
        let Some(rotation) = verified.0.rotation.as_ref() else {
            return Ok(());
        };
        let public_key = rotation
            .next_public_key
            .as_slice()
            .try_into()
            .map_err(|_| {
                HostedError::InvalidDescriptor(
                    "rotated descriptor key is not 32-byte Ed25519".to_string(),
                )
            })?;
        self.insert(
            rotation.next_key_id.clone(),
            public_key,
            rotation.activates_at_unix_millis,
            i64::MAX,
        )
    }
}

/// Endpoint descriptor after signature, expiry, ALPN, and address validation.
#[derive(Debug, Clone)]
pub struct VerifiedEndpointDescriptor(EndpointDescriptor);

impl VerifiedEndpointDescriptor {
    pub fn endpoint_addr(&self) -> Result<EndpointAddr> {
        let endpoint_id: EndpointId = self
            .0
            .endpoint_id
            .parse()
            .map_err(|error| HostedError::InvalidDescriptor(format!("endpoint id: {error}")))?;
        let mut address = EndpointAddr::new(endpoint_id);
        for relay in &self.0.relay_urls {
            let relay: RelayUrl = relay
                .parse()
                .map_err(|error| HostedError::InvalidDescriptor(format!("relay URL: {error}")))?;
            address = address.with_relay_url(relay);
        }
        for direct in &self.0.direct_addresses {
            let direct: SocketAddr = direct.parse().map_err(|error| {
                HostedError::InvalidDescriptor(format!("direct address: {error}"))
            })?;
            address = address.with_ip_addr(direct);
        }
        Ok(address)
    }

    pub fn relay_urls(&self) -> Result<Vec<RelayUrl>> {
        self.0
            .relay_urls
            .iter()
            .map(|relay| {
                relay
                    .parse()
                    .map_err(|error| HostedError::InvalidDescriptor(format!("relay URL: {error}")))
            })
            .collect()
    }

    pub fn document(&self) -> &EndpointDescriptor {
        &self.0
    }
}

pub async fn fetch_endpoint_descriptor(
    url: &str,
    keys: &DescriptorKeyring,
    config: &ClientConfig,
) -> Result<VerifiedEndpointDescriptor> {
    if !url.starts_with("https://") {
        return Err(HostedError::InvalidDescriptor(
            "endpoint descriptor URL must use HTTPS".to_string(),
        ));
    }
    let (client, request_url, host_header) = bootstrap_http_client(url, config).await?;
    let mut request = client.get(request_url);
    if let Some(host_header) = host_header {
        request = request.header(HOST, host_header);
    }
    let response = request.send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DESCRIPTOR_BYTES as u64)
    {
        return Err(HostedError::InvalidDescriptor(
            "endpoint descriptor is oversized".to_string(),
        ));
    }
    let body = response.bytes().await?;
    if body.len() > MAX_DESCRIPTOR_BYTES {
        return Err(HostedError::InvalidDescriptor(
            "endpoint descriptor is oversized".to_string(),
        ));
    }
    let signed = SignedEndpointDescriptor::decode(body)?;
    keys.verify(&signed, now_unix_millis()?)
}

async fn bootstrap_http_client(
    url: &str,
    config: &ClientConfig,
) -> Result<(Client, reqwest::Url, Option<HeaderValue>)> {
    let mut builder = Client::builder().timeout(Duration::from_secs(config.timeout_secs.max(1)));
    if let Some(ca_pem) = config.tls_ca_certificate_pem.as_deref() {
        let certificates = reqwest::Certificate::from_pem_bundle(ca_pem.as_bytes())?;
        if certificates.is_empty() {
            return Err(HostedError::InvalidDescriptor(
                "TLS CA certificate bundle contains no certificates".to_string(),
            ));
        }
        builder = builder.tls_certs_merge(certificates);
    }

    let target = bootstrap_target(url, config.tls_domain_name.as_deref()).await?;
    if let Some((server_name, addresses)) = target.resolution {
        builder = builder.resolve_to_addrs(&server_name, &addresses);
    }
    Ok((builder.build()?, target.url, target.host_header))
}

struct BootstrapTarget {
    url: reqwest::Url,
    host_header: Option<HeaderValue>,
    resolution: Option<(String, Vec<SocketAddr>)>,
}

async fn bootstrap_target(url: &str, tls_domain_name: Option<&str>) -> Result<BootstrapTarget> {
    let mut url = reqwest::Url::parse(url).map_err(|error| {
        HostedError::InvalidDescriptor(format!("endpoint descriptor URL: {error}"))
    })?;
    let Some(tls_domain_name) = tls_domain_name else {
        return Ok(BootstrapTarget {
            url,
            host_header: None,
            resolution: None,
        });
    };
    if tls_domain_name.is_empty() {
        return Err(HostedError::InvalidDescriptor(
            "TLS server-name override is empty".to_string(),
        ));
    }

    let original_host = url
        .host_str()
        .ok_or_else(|| {
            HostedError::InvalidDescriptor("endpoint descriptor URL has no host".to_string())
        })?
        .to_string();
    let port = url.port_or_known_default().ok_or_else(|| {
        HostedError::InvalidDescriptor("endpoint descriptor URL has no usable port".to_string())
    })?;
    let addresses = resolve_host(&original_host, port).await?;
    let host_header =
        HeaderValue::from_str(&http_authority(&url, &original_host)).map_err(|error| {
            HostedError::InvalidDescriptor(format!(
                "endpoint descriptor URL has an invalid authority: {error}"
            ))
        })?;

    url.set_host(Some(tls_domain_name)).map_err(|error| {
        HostedError::InvalidDescriptor(format!("TLS server-name override is invalid: {error}"))
    })?;
    let server_name = url
        .host_str()
        .ok_or_else(|| {
            HostedError::InvalidDescriptor("TLS server-name override is invalid".to_string())
        })?
        .to_string();

    Ok(BootstrapTarget {
        url,
        host_header: Some(host_header),
        resolution: Some((server_name, addresses)),
    })
}

async fn resolve_host(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(HostedError::transport)?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(HostedError::transport(format!(
            "endpoint descriptor host {host} resolved to no addresses"
        )));
    }
    Ok(addresses)
}

fn http_authority(url: &reqwest::Url, host: &str) -> String {
    let host = match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => format!("[{host}]"),
        Ok(IpAddr::V4(_)) | Err(_) => host.to_string(),
    };
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

fn validate_descriptor(descriptor: &EndpointDescriptor, now_unix_millis: i64) -> Result<()> {
    if descriptor.version != 1 || descriptor.endpoint_id.is_empty() {
        return Err(HostedError::InvalidDescriptor(
            "unsupported descriptor version or empty endpoint id".to_string(),
        ));
    }
    if descriptor.issued_at_unix_millis > now_unix_millis
        || descriptor.expires_at_unix_millis <= now_unix_millis
    {
        return Err(HostedError::DescriptorOutsideValidityWindow);
    }
    if !descriptor
        .supported_alpns
        .iter()
        .any(|alpn| alpn == HOSTED_ALPN_V1)
    {
        return Err(HostedError::InvalidDescriptor(
            "descriptor does not support the hosted ALPN".to_string(),
        ));
    }
    if descriptor.relay_urls.is_empty() && descriptor.direct_addresses.is_empty() {
        return Err(HostedError::InvalidDescriptor(
            "descriptor has no relay or direct address".to_string(),
        ));
    }
    Ok(())
}

fn now_unix_millis() -> Result<i64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(HostedError::transport)?
        .as_millis();
    i64::try_from(millis).map_err(HostedError::transport)
}

#[cfg(test)]
mod tests {
    use cli_shared::ClientConfig;

    use super::{DescriptorKeyring, bootstrap_target, fetch_endpoint_descriptor};

    #[tokio::test]
    async fn bootstrap_server_name_override_preserves_the_network_target_and_http_authority() {
        let target = bootstrap_target("https://127.0.0.1:8421/descriptor", Some("localhost"))
            .await
            .unwrap();

        assert_eq!(target.url.as_str(), "https://localhost:8421/descriptor");
        assert_eq!(target.host_header.unwrap(), "127.0.0.1:8421");
        let (server_name, addresses) = target.resolution.unwrap();
        assert_eq!(server_name, "localhost");
        assert_eq!(addresses, ["127.0.0.1:8421".parse().unwrap()]);
    }

    #[tokio::test]
    async fn descriptor_bootstrap_consumes_the_configured_ca_bundle_before_network_io() {
        let config = ClientConfig::default().with_tls_ca_certificate_pem("not a PEM certificate");
        let error = fetch_endpoint_descriptor(
            "https://127.0.0.1:1/.well-known/heddle/iroh-endpoint",
            &DescriptorKeyring::default(),
            &config,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("certificate"));
    }
}
