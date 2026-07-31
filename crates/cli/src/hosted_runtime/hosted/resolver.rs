//! Shared descriptor-trust resolution for every native hosted entry point.

use cli_shared::ClientConfig;

use super::{
    DescriptorKeyring, HostedError, Result, VerifiedEndpointDescriptor,
    descriptor_trust::{
        PinInsertOutcome, canonical_server_authority, insert_verified_pin, load_automatic_pin,
        pin_change_message, validate_descriptor_pair,
    },
    fetch_descriptor_key_document, fetch_signed_endpoint_descriptor,
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
        (Some(key_id), Some(public_key)) => {
            let mut keys = DescriptorKeyring::default();
            keys.insert(key_id, *public_key, i64::MIN, i64::MAX)?;
            let signed =
                fetch_signed_endpoint_descriptor(&descriptor_url(&canonical_server), config)
                    .await?;
            keys.verify(&signed, now_unix_millis()?)
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
        let signed =
            fetch_signed_endpoint_descriptor(&descriptor_url(canonical_server), config).await?;
        if signed.key_id != pin.key_id {
            return Err(HostedError::DescriptorTrust(
                pin_change_message(canonical_server, &pin, &signed.key_id)
                    .map_err(|error| HostedError::DescriptorTrust(error.to_string()))?,
            ));
        }
        let mut keys = DescriptorKeyring::default();
        keys.insert(
            &pin.key_id,
            pin.public_key_bytes()
                .map_err(|error| HostedError::DescriptorTrust(error.to_string()))?,
            i64::MIN,
            i64::MAX,
        )?;
        return match keys.verify(&signed, now_unix_millis()?) {
            Err(HostedError::InvalidDescriptorSignature) => Err(HostedError::DescriptorTrust(
                pin_change_message(canonical_server, &pin, &signed.key_id)
                    .map_err(|error| HostedError::DescriptorTrust(error.to_string()))?,
            )),
            result => result,
        };
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
    let mut keys = DescriptorKeyring::default();
    keys.insert(&document.key_id, public_key, i64::MIN, i64::MAX)?;
    let signed =
        fetch_signed_endpoint_descriptor(&descriptor_url(canonical_server), config).await?;
    let verified = keys.verify(&signed, now_unix_millis()?)?;
    let outcome = insert_verified_pin(canonical_server, &document.key_id, &public_key)
        .map_err(|error| HostedError::DescriptorTrust(error.to_string()))?;
    if outcome == PinInsertOutcome::Created {
        eprintln!("Pinned descriptor trust for {canonical_server}.");
        eprintln!("Descriptor key id: {}", document.key_id);
        eprintln!("Descriptor public key: {}", document.public_key);
    }
    Ok(verified)
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
    use super::{descriptor_key_url, descriptor_url};

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

    #[tokio::test]
    async fn half_config_refuses_before_network_io() {
        let error = super::resolve_and_verify_endpoint_descriptor(
            "weft.example:8421",
            &cli_shared::ClientConfig {
                descriptor_key_id: Some("key-1".to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("ambiguous security posture"));
    }
}
