//! Shared descriptor-trust resolution for every native hosted entry point.
//!
//! The pin is the deployment descriptor ROOT. The live set of ephemeral
//! endpoints is fetched over web-PKI TLS and accepted only when a root
//! attestation verifies against that pin.

use config::ClientConfig;

use super::{
    HostedError, Result, VerifiedEndpointDescriptor,
    descriptor_trust::{
        PinInsertOutcome, canonical_server_authority, insert_verified_pin, load_automatic_pin,
        validate_descriptor_pair,
    },
    fetch_descriptor_key_document, fetch_ephemeral_descriptor_set,
    root_attestation::{fail_if_none_trusted, order_for_dial, trusted_live_entries},
};

pub(super) async fn resolve_and_verify_endpoint_descriptor(
    server: &str,
    config: &ClientConfig,
) -> Result<VerifiedEndpointDescriptor> {
    let canonical_server = canonical_server_authority(server)
        .map_err(|error| HostedError::DescriptorTrust(error.to_string()))?;
    match (
        config.descriptor_key_id.as_deref(),
        config.descriptor_public_key.as_ref(),
    ) {
        (Some(_key_id), Some(public_key)) => {
            verify_live_set_against_root(&canonical_server, public_key, config).await
        }
        (Some(_), None) | (None, Some(_)) => Err(HostedError::DescriptorTrust(
            "ambiguous security posture: both descriptor trust fields are required".to_string(),
        )),
        (None, None) => resolve_automatic_descriptor_trust(&canonical_server, config).await,
    }
}

async fn resolve_automatic_descriptor_trust(
    canonical_server: &str,
    config: &ClientConfig,
) -> Result<VerifiedEndpointDescriptor> {
    if let Some(pin) = load_automatic_pin(canonical_server)
        .map_err(|error| HostedError::DescriptorTrust(error.to_string()))?
    {
        let root = pin
            .public_key_bytes()
            .map_err(|error| HostedError::DescriptorTrust(error.to_string()))?;
        // The pin is the root. A served set cannot rotate it; unattested
        // entries fail closed without touching the store.
        return verify_live_set_against_root(canonical_server, &root, config).await;
    }

    let document =
        match fetch_descriptor_key_document(&descriptor_key_url(canonical_server), config).await {
            Err(HostedError::DescriptorTrustUnavailable) => {
                return Err(HostedError::DescriptorTrust(format!(
                    "server does not publish descriptor trust; configure both values or upgrade \
                     the server (canonical server: {canonical_server})"
                )));
            }
            result => result?,
        };
    if document.version != 1 {
        return Err(HostedError::InvalidDescriptor(format!(
            "unsupported descriptor trust document version {}",
            document.version
        )));
    }
    let public_key = validate_descriptor_pair(&document.key_id, &document.public_key)
        .map_err(|error| HostedError::InvalidDescriptor(error.to_string()))?;
    let verified = verify_live_set_against_root(canonical_server, &public_key, config).await?;
    let outcome = insert_verified_pin(canonical_server, &document.key_id, &public_key)
        .map_err(|error| HostedError::DescriptorTrust(error.to_string()))?;
    if outcome == PinInsertOutcome::Created {
        tracing::info!(
            server = canonical_server,
            descriptor_key_id = document.key_id,
            descriptor_public_key = document.public_key,
            "pinned descriptor root"
        );
    }
    Ok(verified)
}

async fn verify_live_set_against_root(
    canonical_server: &str,
    root_public_key: &[u8; 32],
    config: &ClientConfig,
) -> Result<VerifiedEndpointDescriptor> {
    let set = fetch_ephemeral_descriptor_set(&descriptor_url(canonical_server), config).await?;
    let now = now_unix_millis()?;
    let (trusted, rejects) = trusted_live_entries(&set, root_public_key, now);
    fail_if_none_trusted(&trusted, &rejects)?;
    let env_region = std::env::var("HEDDLE_PREFERRED_REGION")
        .ok()
        .filter(|value| !value.is_empty());
    let preferred_region = config.preferred_region.as_deref().or(env_region.as_deref());
    let ordered = order_for_dial(&trusted, preferred_region);
    let selected = ordered.first().ok_or_else(|| {
        HostedError::InvalidDescriptor("no currently valid root-attested endpoint".to_string())
    })?;
    VerifiedEndpointDescriptor::from_attested_entry(selected, now)
}

fn descriptor_url(canonical_server: &str) -> String {
    format!("{canonical_server}/.well-known/heddle/iroh-endpoint")
}

fn descriptor_key_url(canonical_server: &str) -> String {
    format!("{canonical_server}/.well-known/heddle/iroh-descriptor-key")
}

fn now_unix_millis() -> Result<i64> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(HostedError::transport)?
        .as_millis();
    i64::try_from(millis).map_err(HostedError::transport)
}

#[cfg(test)]
mod tests {
    use super::{canonical_server_authority, descriptor_key_url, descriptor_url};

    #[test]
    fn descriptor_bootstrap_is_https_and_well_known() {
        assert_eq!(
            descriptor_url("https://weft.example:8421"),
            "https://weft.example:8421/.well-known/heddle/iroh-endpoint"
        );
        assert_eq!(
            descriptor_key_url("https://weft.example:8421"),
            "https://weft.example:8421/.well-known/heddle/iroh-descriptor-key"
        );
    }

    #[test]
    fn descriptor_bootstrap_urls_preserve_hostname_authority() {
        let canonical = canonical_server_authority("api-staging.heddle.sh").unwrap();
        assert_eq!(
            descriptor_url(&canonical),
            "https://api-staging.heddle.sh/.well-known/heddle/iroh-endpoint"
        );
        assert_eq!(
            descriptor_key_url(&canonical),
            "https://api-staging.heddle.sh/.well-known/heddle/iroh-descriptor-key"
        );
        assert!(!descriptor_key_url(&canonical).contains("104.18."));
    }

    #[tokio::test]
    async fn half_config_refuses_before_network_io() {
        let error = super::resolve_and_verify_endpoint_descriptor(
            "weft.example:8421",
            &config::ClientConfig {
                descriptor_key_id: Some("key-1".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("ambiguous security posture"));
    }
}
