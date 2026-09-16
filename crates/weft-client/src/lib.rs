// SPDX-License-Identifier: Apache-2.0
//! Native v2 Weft client: descriptor-trusted Iroh transport and
//! [`thread_api::Remote::discover`].
//!
//! The application supplies the HTTP client, descriptor trust keys, and
//! credential. This crate does not read `HEDDLE_HOME` or mint credentials.
//! Admin RPCs that still live only on Weft (`weftctl`) are not shipped here.

mod hosted;

use std::{
    collections::HashMap,
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use api::{
    descriptor_trust::{
        EndpointDescriptorSetDocument, parse_endpoint_descriptor_set, trusted_live_entries,
    },
    heddle::api::v1alpha1::EndpointDescriptor,
};
pub use hosted::HostedClient;
use iroh::{EndpointAddr, EndpointId, RelayUrl};
use reqwest::header::CONTENT_TYPE;
pub use thread_api::{contract, credentials::Credentials, rpc};

const DESCRIPTOR_PATH: &str = "/.well-known/heddle/iroh-endpoint";
const MAX_DESCRIPTOR_BYTES: usize = 64 * 1024;

/// Explicit inputs for a hosted connection. Files, environment, TLS policy,
/// credential storage and trust-anchor selection belong to the application.
pub struct ConnectionOptions<C = Credentials> {
    pub http_client: reqwest::Client,
    pub trusted_descriptors: DescriptorKeyring,
    pub credential: C,
    pub timeout: Duration,
}

/// Trust anchors come from the application, including key rotation policy.
pub struct DescriptorKeyring {
    keys: HashMap<String, [u8; 32]>,
}

impl DescriptorKeyring {
    pub fn new(keys: impl IntoIterator<Item = (String, [u8; 32])>) -> Result<Self> {
        let mut trusted = HashMap::new();
        for (id, key) in keys {
            if id.is_empty() || trusted.insert(id, key).is_some() {
                bail!("descriptor trust requires nonempty, distinct key IDs");
            }
        }
        if trusted.is_empty() {
            bail!("at least one trusted descriptor key is required");
        }
        Ok(Self { keys: trusted })
    }

    pub fn verify_set(
        &self,
        document: &EndpointDescriptorSetDocument,
        now_unix_millis: i64,
        preferred_region: Option<&str>,
    ) -> Result<VerifiedEndpointDescriptor> {
        let root_key = self
            .keys
            .get(&document.root_key_id)
            .context("endpoint descriptor root signing key is not trusted")?;
        let (live, _rejects) = trusted_live_entries(document, root_key, now_unix_millis);
        let mut preferred = None;
        let mut fallback = None;
        for verified in live {
            if validate_descriptor(&verified.endpoint_descriptor, now_unix_millis).is_err() {
                continue;
            }
            let mapped = VerifiedEndpointDescriptor(verified.endpoint_descriptor);
            if preferred_region.is_some_and(|region| verified.region == region) {
                preferred = Some(mapped);
                break;
            }
            if fallback.is_none() {
                fallback = Some(mapped);
            }
        }
        preferred
            .or(fallback)
            .context("no live root-attested Iroh descriptor is trusted")
    }
}

/// Endpoint descriptor after two-layer root attestation, expiry, ALPN, and
/// address validation.
#[derive(Clone, Debug)]
pub struct VerifiedEndpointDescriptor(EndpointDescriptor);

impl VerifiedEndpointDescriptor {
    pub fn endpoint_addr(&self) -> Result<EndpointAddr> {
        let endpoint_id: EndpointId = self
            .0
            .endpoint_id
            .parse()
            .context("parse endpoint descriptor Iroh endpoint id")?;
        let mut address = EndpointAddr::new(endpoint_id);
        for relay in &self.0.relay_urls {
            address = address.with_relay_url(
                relay
                    .parse()
                    .with_context(|| format!("parse endpoint descriptor relay URL {relay}"))?,
            );
        }
        for direct in &self.0.direct_addresses {
            let direct: SocketAddr = direct
                .parse()
                .with_context(|| format!("parse endpoint descriptor direct address {direct}"))?;
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
                    .with_context(|| format!("parse endpoint descriptor relay URL {relay}"))
            })
            .collect()
    }

    pub fn document(&self) -> &EndpointDescriptor {
        &self.0
    }
}

/// Fetch `/.well-known/heddle/iroh-endpoint` as a JSON descriptor set and
/// verify it against the application's root keyring.
pub async fn fetch_endpoint_descriptor(
    url: &str,
    keys: &DescriptorKeyring,
    http: &reqwest::Client,
) -> Result<VerifiedEndpointDescriptor> {
    let response = http
        .get(url)
        .send()
        .await
        .context("fetch signed Iroh endpoint descriptor")?
        .error_for_status()
        .context("fetch signed Iroh endpoint descriptor")?;
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json")) {
        bail!("endpoint descriptor response must use application/json");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DESCRIPTOR_BYTES as u64)
    {
        bail!("signed endpoint descriptor is oversized");
    }
    let body = response
        .bytes()
        .await
        .context("read signed Iroh endpoint descriptor")?;
    if body.len() > MAX_DESCRIPTOR_BYTES {
        bail!("signed endpoint descriptor is oversized");
    }
    verify_descriptor_set_body(&body, keys)
}

/// Parse a JSON endpoint-descriptor set and verify it. A protobuf body fails
/// closed at the JSON parser.
pub fn verify_descriptor_set_body(
    body: &[u8],
    keys: &DescriptorKeyring,
) -> Result<VerifiedEndpointDescriptor> {
    let document =
        parse_endpoint_descriptor_set(body).context("decode Iroh endpoint descriptor set")?;
    let preferred_region = std::env::var("HEDDLE_REMOTE_IROH_REGION")
        .ok()
        .filter(|value| !value.is_empty());
    keys.verify_set(
        &document,
        i64::try_from(now()?.as_millis()).context("bootstrap timestamp overflow")?,
        preferred_region.as_deref(),
    )
}

pub fn descriptor_url(server: &str) -> Result<String> {
    let authority = server
        .strip_prefix("https://")
        .unwrap_or(server)
        .trim_end_matches('/');
    if server.starts_with("http://") || authority.is_empty() || authority.contains('/') {
        bail!("native hosted bootstrap requires an HTTPS server authority");
    }
    Ok(format!("https://{authority}{DESCRIPTOR_PATH}"))
}

fn validate_descriptor(descriptor: &EndpointDescriptor, now_unix_millis: i64) -> Result<()> {
    if descriptor.version != 1 || descriptor.endpoint_id.is_empty() {
        bail!("unsupported endpoint descriptor version or empty endpoint id");
    }
    if descriptor.issued_at_unix_millis > now_unix_millis
        || descriptor.expires_at_unix_millis <= now_unix_millis
    {
        bail!("endpoint descriptor is expired or not yet valid");
    }
    if !descriptor
        .supported_alpns
        .iter()
        .any(|alpn| alpn == api::HOSTED_ALPN_V1)
    {
        bail!("endpoint descriptor does not support the hosted Iroh ALPN");
    }
    if descriptor.relay_urls.is_empty() && descriptor.direct_addresses.is_empty() {
        bail!("endpoint descriptor has no relay or direct address");
    }
    Ok(())
}

fn now() -> Result<Duration> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")
}

#[cfg(test)]
mod tests {
    use api::{
        HOSTED_ALPN_V1,
        descriptor_trust::{
            AttestedEndpointDescriptorEntry, EndpointDescriptorSetDocument, SET_VERSION,
            ephemeral_attestation_bytes, parse_endpoint_descriptor_set,
        },
        heddle::api::v1alpha1::{EndpointDescriptor, SignedEndpointDescriptor},
        signing,
    };
    use ed25519_dalek::{Signer, SigningKey};
    use prost::Message;

    use super::*;

    const NOW: i64 = 1_750_000_000_000;

    struct Fixture {
        root_public: [u8; 32],
        document: EndpointDescriptorSetDocument,
    }

    fn fixture() -> Fixture {
        let root = SigningKey::from_bytes(&[7; 32]);
        let ephemeral = SigningKey::from_bytes(&[9; 32]);
        let ephemeral_public_key = ephemeral.verifying_key().to_bytes();
        let attestation = ephemeral_attestation_bytes(
            "root:ephemeral",
            &ephemeral_public_key,
            NOW,
            NOW + 60_000,
            "us-west-2",
        );
        let attestation_signature = root.sign(&attestation);
        let descriptor = EndpointDescriptor {
            version: 1,
            endpoint_id: hex::encode(ephemeral_public_key),
            relay_urls: vec!["https://relay.example.test".to_string()],
            supported_alpns: vec![HOSTED_ALPN_V1.to_vec()],
            direct_addresses: vec!["203.0.113.8:4433".to_string()],
            issued_at_unix_millis: NOW,
            expires_at_unix_millis: NOW + 60_000,
            rotation: None,
        };
        let signed = SignedEndpointDescriptor {
            signature: ephemeral
                .sign(&signing::endpoint_descriptor_bytes(&descriptor))
                .to_bytes()
                .to_vec(),
            key_id: "root:ephemeral".to_string(),
            descriptor: Some(descriptor),
        };
        Fixture {
            root_public: root.verifying_key().to_bytes(),
            document: EndpointDescriptorSetDocument {
                version: SET_VERSION,
                root_key_id: "root".to_string(),
                entries: vec![AttestedEndpointDescriptorEntry {
                    ephemeral_key_id: "root:ephemeral".to_string(),
                    ephemeral_public_key: hex::encode(ephemeral_public_key),
                    not_before_unix_millis: NOW,
                    not_after_unix_millis: NOW + 60_000,
                    region: "us-west-2".to_string(),
                    attestation_signature: hex::encode(attestation_signature.to_bytes()),
                    signed_descriptor: hex::encode(signed.encode_to_vec()),
                }],
            },
        }
    }

    #[test]
    fn explicit_descriptor_trust_rejects_empty_and_ambiguous_key_sets() {
        assert!(DescriptorKeyring::new([]).is_err());
        assert!(DescriptorKeyring::new([(String::new(), [1; 32])]).is_err());
        assert!(
            DescriptorKeyring::new([("key".into(), [1; 32]), ("key".into(), [2; 32])]).is_err()
        );
    }

    #[test]
    fn bootstrap_url_is_https_and_targets_the_descriptor_route() {
        assert_eq!(
            descriptor_url("127.0.0.1:8421").unwrap(),
            "https://127.0.0.1:8421/.well-known/heddle/iroh-endpoint"
        );
        assert!(descriptor_url("http://example.com").is_err());
    }

    #[test]
    fn json_descriptor_set_parses_and_a_bad_protobuf_body_fails_closed() {
        let fixture = fixture();
        let body = serde_json::to_vec(&fixture.document).expect("serialize descriptor set");
        parse_endpoint_descriptor_set(&body).expect("JSON descriptor set must parse");
        let keys = DescriptorKeyring::new([("root".to_string(), fixture.root_public)])
            .expect("trusted descriptor root");
        keys.verify_set(&fixture.document, NOW, Some("us-west-2"))
            .expect("root-attested JSON set must verify");

        let protobuf = SignedEndpointDescriptor {
            signature: vec![1; 64],
            key_id: "root".to_string(),
            descriptor: None,
        }
        .encode_to_vec();
        let parse_error = parse_endpoint_descriptor_set(&protobuf)
            .expect_err("protobuf must not parse as a JSON descriptor set");
        assert!(
            parse_error.to_string().contains("malformed"),
            "unexpected parse error: {parse_error}"
        );
        let verify_error = verify_descriptor_set_body(&protobuf, &keys)
            .expect_err("protobuf body must fail closed");
        assert!(
            verify_error
                .to_string()
                .contains("decode Iroh endpoint descriptor set"),
            "unexpected verify error: {verify_error}"
        );
    }

    #[test]
    fn descriptor_set_rejects_tampering() {
        let fixture = fixture();
        let keys = DescriptorKeyring::new([("root".to_string(), fixture.root_public)])
            .expect("trusted descriptor root");
        let mut tampered = fixture.document;
        tampered.entries[0].region = "eu-central-1".to_string();
        assert!(keys.verify_set(&tampered, NOW, None).is_err());
    }
}
