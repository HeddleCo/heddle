#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use api::{
    HOSTED_ALPN_V1,
    heddle::api::v1alpha1::{EndpointDescriptor, SignedEndpointDescriptor},
    signing::endpoint_descriptor_bytes,
};
use config::ClientConfig;
use crypto::Ed25519Signer;
use iroh::{EndpointAddr, EndpointId, RelayUrl};
use prost::Message;
use reqwest::{
    Client, StatusCode,
    header::{CONTENT_TYPE, HOST, HeaderValue},
    redirect::Policy,
};
use serde::Deserialize;

use super::{HostedError, Result};

const MAX_DESCRIPTOR_BYTES: usize = 64 * 1024;
const MAX_DESCRIPTOR_KEY_DOCUMENT_BYTES: usize = 4 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptorKeyDocument {
    pub version: u32,
    pub key_id: String,
    pub public_key: String,
}

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
}

/// Endpoint descriptor after signature, expiry, ALPN, and address validation.
#[derive(Debug, Clone)]
pub struct VerifiedEndpointDescriptor(EndpointDescriptor);

impl VerifiedEndpointDescriptor {
    pub fn endpoint_addr(&self) -> Result<EndpointAddr> {
        self.endpoint_addr_with_relays(true)
    }

    pub(super) fn direct_endpoint_addr(&self) -> Result<EndpointAddr> {
        self.endpoint_addr_with_relays(false)
    }

    fn endpoint_addr_with_relays(&self, include_relays: bool) -> Result<EndpointAddr> {
        let endpoint_id: EndpointId = self
            .0
            .endpoint_id
            .parse()
            .map_err(|error| HostedError::InvalidDescriptor(format!("endpoint id: {error}")))?;
        let mut address = EndpointAddr::new(endpoint_id);
        if include_relays {
            for relay in &self.0.relay_urls {
                let relay: RelayUrl = relay.parse().map_err(|error| {
                    HostedError::InvalidDescriptor(format!("relay URL: {error}"))
                })?;
                address = address.with_relay_url(relay);
            }
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

#[cfg(test)]
pub async fn fetch_endpoint_descriptor(
    url: &str,
    keys: &DescriptorKeyring,
    config: &ClientConfig,
) -> Result<VerifiedEndpointDescriptor> {
    let signed = fetch_signed_endpoint_descriptor(url, config).await?;
    keys.verify(&signed, now_unix_millis()?)
}

pub async fn fetch_signed_endpoint_descriptor(
    url: &str,
    config: &ClientConfig,
) -> Result<SignedEndpointDescriptor> {
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
    let response = request.send().await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Err(HostedError::EndpointDescriptorUnavailable);
    }
    if response.status() != StatusCode::OK {
        return Err(HostedError::InvalidDescriptor(format!(
            "endpoint descriptor request returned HTTP {}",
            response.status()
        )));
    }
    let body = bounded_response_body(response, MAX_DESCRIPTOR_BYTES, "endpoint descriptor").await?;
    Ok(SignedEndpointDescriptor::decode(body.as_slice())?)
}

pub async fn fetch_descriptor_key_document(
    url: &str,
    config: &ClientConfig,
) -> Result<DescriptorKeyDocument> {
    if !url.starts_with("https://") {
        return Err(HostedError::InvalidDescriptor(
            "descriptor trust URL must use HTTPS".to_string(),
        ));
    }
    let (client, request_url, host_header) = bootstrap_http_client(url, config).await?;
    let mut request = client.get(request_url);
    if let Some(host_header) = host_header {
        request = request.header(HOST, host_header);
    }
    let response = request.send().await?;
    if response.status() == StatusCode::NOT_FOUND {
        return Err(HostedError::DescriptorTrustUnavailable);
    }
    if response.status() != StatusCode::OK {
        return Err(HostedError::InvalidDescriptor(format!(
            "descriptor trust request returned HTTP {}",
            response.status()
        )));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json")) {
        return Err(HostedError::InvalidDescriptor(
            "descriptor trust response must use application/json".to_string(),
        ));
    }
    let body = bounded_response_body(
        response,
        MAX_DESCRIPTOR_KEY_DOCUMENT_BYTES,
        "descriptor trust response",
    )
    .await?;
    serde_json::from_slice(&body).map_err(|error| {
        HostedError::InvalidDescriptor(format!("descriptor trust response is malformed: {error}"))
    })
}

async fn bootstrap_http_client(
    url: &str,
    config: &ClientConfig,
) -> Result<(Client, reqwest::Url, Option<HeaderValue>)> {
    heddle_perf_contract::record_network_client_initialization();
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(config.timeout_secs.max(1)))
        .redirect(Policy::none());
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

async fn bounded_response_body(
    mut response: reqwest::Response,
    limit: usize,
    label: &str,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(HostedError::InvalidDescriptor(format!(
            "{label} is oversized"
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(HostedError::InvalidDescriptor(format!(
                "{label} is oversized"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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

#[cfg(test)]
fn now_unix_millis() -> Result<i64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(HostedError::transport)?
        .as_millis();
    i64::try_from(millis).map_err(HostedError::transport)
}

#[cfg(test)]
mod tests {
    use config::ClientConfig;

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
