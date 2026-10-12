//! Disposable endpoint metadata. Signed sets are reverified on every read;
//! descriptions are bound to the attested endpoint and its validity window.
use std::{fs, io::Write, path::PathBuf};

use api::heddle::api::v1alpha2::{DescribeEndpointResponse, EndpointKind};
use config::ClientConfig;
use prost::Message;

use super::VerifiedEndpointDescriptor;

pub(super) fn path(
    authority: &str,
    root: &[u8; 32],
    config: &ClientConfig,
    suffix: &str,
) -> PathBuf {
    let mut key = blake3::Hasher::new();
    for part in [
        authority.as_bytes(),
        root.as_slice(),
        config
            .tls_ca_certificate_pem
            .as_deref()
            .unwrap_or_default()
            .as_bytes(),
        config
            .tls_domain_name
            .as_deref()
            .unwrap_or_default()
            .as_bytes(),
        suffix.as_bytes(),
    ] {
        key.update(&(part.len() as u64).to_be_bytes());
        key.update(part);
    }
    config::heddle_home_dir()
        .join("cache/endpoints")
        .join(key.finalize().to_hex().as_str())
}

pub(super) fn save(path: &std::path::Path, bytes: &[u8]) {
    // A cache failure must never turn a verified connection into a failure.
    let result = (|| -> std::io::Result<()> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(bytes)?;
        file.persist(path).map_err(|error| error.error)?;
        Ok(())
    })();
    if let Err(error) = result {
        tracing::debug!(%error, "endpoint cache write failed");
    }
}

fn description_path(
    descriptor: &VerifiedEndpointDescriptor,
    config: &ClientConfig,
) -> Option<PathBuf> {
    let root = descriptor.hosted_root()?;
    // Changing the attested routing document or its validity window expires
    // the inventory, even when the endpoint key is unchanged.
    let suffix = format!(
        "description:{}:{}",
        blake3::hash(&descriptor.document().encode_to_vec()),
        descriptor.valid_until_unix_millis()
    );
    Some(path(root.authority(), root.public_key(), config, &suffix))
}

pub(super) fn load_description(
    descriptor: &VerifiedEndpointDescriptor,
    config: &ClientConfig,
) -> Option<DescribeEndpointResponse> {
    if chrono::Utc::now().timestamp_millis() >= descriptor.valid_until_unix_millis() {
        return None;
    }
    let bytes = fs::read(description_path(descriptor, config)?).ok()?;
    let description = DescribeEndpointResponse::decode(bytes.as_slice()).ok()?;
    let key = descriptor.endpoint_addr().ok()?.id;
    if description.endpoint.as_ref().is_none_or(|source| {
        source.public_key != key.as_bytes() || source.kind != EndpointKind::Weft as i32
    }) || !description
        .supported_packages
        .iter()
        .any(|package| package == "heddle.api.v1alpha2")
    {
        return None;
    }
    Some(description)
}

pub(super) fn save_description(
    descriptor: &VerifiedEndpointDescriptor,
    config: &ClientConfig,
    description: &DescribeEndpointResponse,
) {
    if let Some(path) = description_path(descriptor, config) {
        save(&path, &description.encode_to_vec());
    }
}
