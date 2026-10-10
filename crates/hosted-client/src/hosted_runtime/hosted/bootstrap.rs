use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use api::{
    HOSTED_ALPN_V1,
    descriptor_trust::{
        DescriptorSetError, EndpointDescriptorSetDocument, VerifiedEndpoint,
        parse_endpoint_descriptor_set,
    },
    heddle::api::common::{EndpointDescriptor, SignedEndpointDescriptor},
    signing::endpoint_descriptor_bytes,
};
use config::ClientConfig;
use crypto::Ed25519Signer;
use iroh::{EndpointAddr, EndpointId, RelayUrl};
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
        Ok(VerifiedEndpointDescriptor(descriptor.clone(), None, None))
    }
}

/// Endpoint descriptor after signature, expiry, ALPN, and address validation.
#[derive(Debug, Clone)]
pub struct VerifiedEndpointDescriptor(
    EndpointDescriptor,
    Option<super::descriptor_trust::HostedRootSelection>,
    Option<BootstrapHttp>,
);

impl VerifiedEndpointDescriptor {
    pub(super) fn with_http(mut self, http: BootstrapHttp) -> Self {
        self.2 = Some(http);
        self
    }
    pub(super) fn http(&self, config: &ClientConfig) -> BootstrapHttp {
        #[cfg(feature = "gateway-fixture")]
        if self.2.as_ref().is_some_and(|http| {
            http.config.gateway_fixture_address != config.gateway_fixture_address
        }) {
            return BootstrapHttp::new(config);
        }
        match &self.2 {
            Some(http)
                if http.config.tls_ca_certificate_pem == config.tls_ca_certificate_pem
                    && http.config.tls_domain_name == config.tls_domain_name
                    && http.config.timeout_secs == config.timeout_secs =>
            {
                http.clone()
            }
            _ => BootstrapHttp::new(config),
        }
    }

    pub fn hosted_root(&self) -> Option<&super::descriptor_trust::HostedRootSelection> {
        self.1.as_ref()
    }
    pub(super) fn with_hosted_root(
        mut self,
        root: super::descriptor_trust::HostedRootSelection,
    ) -> Self {
        self.1 = Some(root);
        self
    }
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

    /// Construct a verified descriptor from a two-layer-verified endpoint.
    ///
    /// Dial addresses come from the attested ephemeral key's signed
    /// `EndpointDescriptor`. Unsigned well-known hints are never used.
    pub(super) fn from_verified_endpoint(
        endpoint: &VerifiedEndpoint,
        now_unix_millis: i64,
    ) -> Result<Self> {
        if now_unix_millis < endpoint.not_before_unix_millis
            || now_unix_millis >= endpoint.not_after_unix_millis
        {
            return Err(HostedError::DescriptorOutsideValidityWindow);
        }
        validate_descriptor(&endpoint.endpoint_descriptor, now_unix_millis)?;
        Ok(Self(endpoint.endpoint_descriptor.clone(), None, None))
    }
}

pub async fn fetch_ephemeral_descriptor_set(
    url: &str,
    http: &BootstrapHttp,
) -> Result<EndpointDescriptorSetDocument> {
    if !url.starts_with("https://") {
        return Err(HostedError::InvalidDescriptor(
            "endpoint descriptor URL must use HTTPS".to_string(),
        ));
    }
    let (client, request_url, host_header) = http.client(url).await?;
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
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json")) {
        return Err(HostedError::InvalidDescriptor(
            "ephemeral descriptor set must use application/json".to_string(),
        ));
    }
    let body = bounded_response_body(response, MAX_DESCRIPTOR_BYTES, "endpoint descriptor").await?;
    parse_endpoint_descriptor_set(&body).map_err(|error| match error {
        DescriptorSetError::Malformed(message) => HostedError::InvalidDescriptor(format!(
            "ephemeral descriptor set is malformed: {message}"
        )),
        DescriptorSetError::UnsupportedVersion(version) => HostedError::InvalidDescriptor(format!(
            "unsupported endpoint descriptor set version {version}"
        )),
    })
}

pub async fn fetch_descriptor_key_document(
    url: &str,
    http: &BootstrapHttp,
) -> Result<DescriptorKeyDocument> {
    if !url.starts_with("https://") {
        return Err(HostedError::InvalidDescriptor(
            "descriptor trust URL must use HTTPS".to_string(),
        ));
    }
    let (client, request_url, host_header) = http.client(url).await?;
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

/// One command's HTTPS pool and immutable TLS policy, shared by descriptor
/// discovery and hosted witness requests. Client construction is single-flight.
#[derive(Debug, Clone)]
pub struct BootstrapHttp {
    config: ClientConfig,
    client: std::sync::Arc<tokio::sync::OnceCell<HttpPool>>,
}

#[derive(Debug)]
struct HttpPool {
    client: Client,
    target: BootstrapTarget,
    origin: reqwest::Url,
}

impl BootstrapHttp {
    pub fn new(config: &ClientConfig) -> Self {
        Self {
            config: config.clone(),
            client: std::sync::Arc::default(),
        }
    }

    pub(super) async fn client(
        &self,
        url: &str,
    ) -> Result<(Client, reqwest::Url, Option<HeaderValue>)> {
        let config = &self.config;
        let mut request_url = reqwest::Url::parse(url).map_err(HostedError::transport)?;
        #[cfg(feature = "gateway-fixture")]
        let fixture_address = gateway_fixture_address(&request_url, config)?;
        let pool = self
            .client
            .get_or_try_init(|| async {
                heddle_perf_contract::record_network_client_initialization();
                // Preserve a caller-installed provider, including library callers.
                let _ = rustls::crypto::ring::default_provider().install_default();
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
                    #[cfg(feature = "gateway-fixture")]
                    {
                        builder = if fixture_address.is_some() {
                            builder.tls_certs_only(certificates)
                        } else {
                            builder.tls_certs_merge(certificates)
                        };
                    }
                    #[cfg(not(feature = "gateway-fixture"))]
                    {
                        builder = builder.tls_certs_merge(certificates);
                    }
                }
                #[cfg(feature = "gateway-fixture")]
                if let Some(address) = fixture_address {
                    // Keep HTTPS identity, SNI and Host canonical. Reqwest retains
                    // this explicit socket port when the URL has no port.
                    builder = builder
                        .no_proxy()
                        .resolve("native-fixture.example", address);
                }
                let target = bootstrap_target(url, config.tls_domain_name.as_deref()).await?;
                if let Some((server_name, addresses)) = &target.resolution {
                    builder = builder.resolve_to_addrs(server_name, addresses);
                }
                Ok::<_, HostedError>(HttpPool {
                    client: builder.build()?,
                    target,
                    origin: request_url.clone(),
                })
            })
            .await?;
        let host_header = if config.tls_domain_name.is_some() {
            // A TLS alias resolves to the first authority's network target.
            // Refuse cross-origin reuse rather than routing it to that target.
            if request_url.origin() != pool.origin.origin() {
                return Err(HostedError::InvalidDescriptor(
                    "TLS server-name override cannot span HTTPS authorities".into(),
                ));
            }
            request_url
                .set_host(pool.target.url.host_str())
                .map_err(HostedError::transport)?;
            pool.target.host_header.clone()
        } else {
            None
        };
        Ok((pool.client.clone(), request_url, host_header))
    }
}

#[cfg(feature = "gateway-fixture")]
fn gateway_fixture_address(
    url: &reqwest::Url,
    config: &ClientConfig,
) -> Result<Option<SocketAddr>> {
    let Some(address) = config.gateway_fixture_address else {
        return Ok(None);
    };
    if !address.ip().is_loopback()
        || address.port() == 0
        || url.scheme() != "https"
        || url.host_str() != Some("native-fixture.example")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || config
            .tls_ca_certificate_pem
            .as_ref()
            .is_none_or(|pem| pem.is_empty())
        || config.tls_domain_name.is_some()
        || config.tls_skip_verify
        || config.allow_insecure
    {
        return Err(HostedError::InvalidDescriptor(
            "gateway fixture requires its canonical HTTPS authority, explicit loopback socket, and supplied CA-only TLS trust".into(),
        ));
    }
    Ok(Some(address))
}

pub(super) async fn bounded_response_body(
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

#[derive(Debug)]
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
mod tests {
    use config::ClientConfig;

    use super::{BootstrapHttp, bootstrap_target, fetch_ephemeral_descriptor_set};

    #[tokio::test]
    async fn bootstrap_server_name_override_preserves_the_network_target_and_http_authority() {
        let _process_env_guard = crate::test_process_env::shared().await;
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
        let _process_env_guard = crate::test_process_env::shared().await;
        let config = ClientConfig::default().with_tls_ca_certificate_pem("not a PEM certificate");
        let error = fetch_ephemeral_descriptor_set(
            "https://127.0.0.1:1/.well-known/heddle/iroh-endpoint",
            &BootstrapHttp::new(&config),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("certificate"));
    }

    #[cfg(feature = "gateway-fixture")]
    mod gateway_fixture {
        use std::collections::{HashMap, VecDeque};

        use super::*;
        use crate::hosted_runtime::hosted::test_https::{TestHttpsServer, TestResponse};

        const AUTHORITY: &str = "https://native-fixture.example";

        fn fixture_config(server: &TestHttpsServer) -> ClientConfig {
            ClientConfig::default()
                .with_tls_ca_certificate_pem(server.certificate_pem())
                .with_gateway_fixture_address(
                    server
                        .authority()
                        .strip_prefix("https://")
                        .unwrap()
                        .parse()
                        .unwrap(),
                )
                .with_timeout(2)
        }

        #[tokio::test]
        async fn gateway_fixture_preserves_canonical_authority_over_authenticated_loopback() {
            let _process_env_guard = crate::test_process_env::shared().await;
            let _ = rustls::crypto::ring::default_provider().install_default();
            let paths = [
                "/.well-known/heddle/iroh-endpoint",
                "/.well-known/heddle/hosted-witnesses",
            ];
            let server = TestHttpsServer::start_with_names(
                paths
                    .iter()
                    .map(|path| {
                        (
                            path.to_string(),
                            VecDeque::from([TestResponse::json(b"authenticated fixture".to_vec())]),
                        )
                    })
                    .collect(),
                vec!["native-fixture.example".into()],
            );
            let http = BootstrapHttp::new(&fixture_config(&server));
            api::import_authority::canonical_https(AUTHORITY, true).unwrap();
            for path in paths {
                let expected = format!("{AUTHORITY}{path}");
                let (client, url, host) = http.client(&expected).await.unwrap();
                assert_eq!(
                    url.as_str(),
                    expected,
                    "transport must not rewrite trust identity"
                );
                assert!(
                    host.is_none(),
                    "TLS SNI and HTTP Host retain the canonical authority"
                );
                assert_eq!(
                    client
                        .get(url)
                        .send()
                        .await
                        .unwrap()
                        .bytes()
                        .await
                        .unwrap()
                        .as_ref(),
                    b"authenticated fixture",
                );
            }
            assert_eq!(server.requests(), paths);
            assert!(http.client("https://other.example/metadata").await.is_err());
            assert_eq!(
                server.requests(),
                paths,
                "pooled fixture clients cannot cross origins"
            );
        }

        #[tokio::test]
        async fn gateway_fixture_rejects_other_origins_routes_and_insecure_configuration() {
            let _process_env_guard = crate::test_process_env::shared().await;
            let config = ClientConfig::default()
                .with_gateway_fixture_address("127.0.0.1:8421".parse().unwrap())
                .with_tls_ca_certificate_pem("invalid test CA; no network may occur");
            for url in [
                "http://native-fixture.example/metadata",
                "https://other.example/metadata",
                "https://127.0.0.1/metadata",
                "https://native-fixture.example:8421/metadata",
                "https://user@native-fixture.example/metadata",
                "https://native-fixture.example/metadata?query=1",
                "https://native-fixture.example/metadata#fragment",
            ] {
                assert!(
                    super::super::gateway_fixture_address(&url.parse().unwrap(), &config).is_err(),
                    "{url}"
                );
            }
            let mut no_ca = config.clone();
            no_ca.tls_ca_certificate_pem = None;
            for invalid in [
                no_ca,
                config
                    .clone()
                    .with_gateway_fixture_address("203.0.113.1:8421".parse().unwrap()),
                config
                    .clone()
                    .with_gateway_fixture_address("127.0.0.1:0".parse().unwrap()),
                config.clone().with_tls(true),
                config.clone().with_tls_domain_name("other.example"),
                config.clone().with_allow_insecure(true),
            ] {
                let error = BootstrapHttp::new(&invalid)
                    .client(&format!("{AUTHORITY}/metadata"))
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("gateway fixture requires"));
            }
        }

        #[tokio::test]
        async fn gateway_fixture_verifies_both_ca_and_logical_hostname() {
            let _process_env_guard = crate::test_process_env::shared().await;
            let _ = rustls::crypto::ring::default_provider().install_default();
            let server = TestHttpsServer::start_with_names(
                HashMap::new(),
                vec!["native-fixture.example".into()],
            );
            let unrelated =
                rcgen::generate_simple_self_signed(vec!["native-fixture.example".into()]).unwrap();
            let wrong_ca =
                fixture_config(&server).with_tls_ca_certificate_pem(unrelated.cert.pem());
            let (client, url, _) = BootstrapHttp::new(&wrong_ca)
                .client(&format!("{AUTHORITY}/metadata"))
                .await
                .unwrap();
            assert!(
                client.get(url).send().await.is_err(),
                "an unrelated CA cannot authenticate the route"
            );
            assert!(server.requests().is_empty());

            let wrong_name = TestHttpsServer::start(HashMap::new());
            let (client, url, _) = BootstrapHttp::new(&fixture_config(&wrong_name))
                .client(&format!("{AUTHORITY}/metadata"))
                .await
                .unwrap();
            assert!(
                client.get(url).send().await.is_err(),
                "the loopback certificate must cover the logical authority"
            );
            assert!(wrong_name.requests().is_empty());
        }
    }
}
