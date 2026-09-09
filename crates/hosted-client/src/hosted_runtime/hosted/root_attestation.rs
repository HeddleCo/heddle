//! Root-attested ephemeral descriptor keys (heddle#1566 / weft#1835).
//!
//! The pinned deployment descriptor ROOT is the only trust anchor. A served
//! entry is dialable only after its ed25519 root attestation verifies over the
//! bound tuple `{ ephemeral_key_id, ephemeral_public_key, not_before,
//! not_after, region }` using the same length-prefixed canonical framing as
//! `api::signing::endpoint_descriptor_bytes`, with a **distinct domain** so an
//! endpoint-descriptor signature cannot be substituted as an attestation.
//!
//! Assumption (weft#1835 PR not published at implementation time):
//! `GET /.well-known/heddle/iroh-endpoint` serves JSON
//! `{ "version": 2, "entries": [ { ephemeral_key_id, ephemeral_public_key,
//! not_before, not_after, region, signature, relay_urls?, direct_addresses? } ] }`.
//! `signature` is 128 lowercase hex (64-byte ed25519). Times are Unix millis.
//! Optional `relay_urls` / `direct_addresses` are dial hints only; they are
//! **not** covered by the attestation. Iroh authenticates the attested
//! `ephemeral_public_key` as the EndpointId.

use api::{HOSTED_ALPN_V1, heddle::api::v1alpha1::EndpointDescriptor};
use crypto::Ed25519Signer;
use serde::{Deserialize, Serialize};

use super::{HostedError, Result, descriptor_trust::parse_descriptor_public_key};

/// Domain separator for root→ephemeral attestations.
///
/// Distinct from `TRANSPORT_BOOTSTRAP_SIGNING_V1_DOMAIN` (`heddle-req-sig-v1`)
/// so a signature over an `EndpointDescriptor` protobuf cannot verify as a
/// root attestation, and vice versa.
pub const ROOT_ATTESTATION_V1_DOMAIN: &str = "heddle-descriptor-root-attestation-v1";
const ROOT_ATTESTATION_KIND: &str = "root-attestation";
const SERVED_SET_VERSION: u32 = 2;

#[derive(Debug, Deserialize)]
pub struct EphemeralDescriptorSet {
    pub version: u32,
    pub entries: Vec<RawEphemeralEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RawEphemeralEntry {
    pub ephemeral_key_id: String,
    pub ephemeral_public_key: String,
    pub not_before: i64,
    pub not_after: i64,
    pub region: String,
    pub signature: String,
    #[serde(default)]
    pub relay_urls: Vec<String>,
    #[serde(default)]
    pub direct_addresses: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedEphemeralEntry {
    pub ephemeral_key_id: String,
    pub ephemeral_public_key: [u8; 32],
    pub not_before: i64,
    pub not_after: i64,
    pub region: String,
    pub relay_urls: Vec<String>,
    pub direct_addresses: Vec<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum EntryReject {
    MissingSignature,
    InvalidSignature,
    Unattested,
    NotYetValid,
    Expired,
    InvalidWindow,
    InvalidKey,
}

/// Length-prefixed canonical bytes the deployment descriptor ROOT signs.
///
/// Framing matches `api::signing`'s `canonical_with_domain` discipline
/// (`{domain}\nkind={len}:{kind}\n{name}={len}:{bytes}…`) so field values
/// cannot be concatenated across boundaries. Integers are fixed-width
/// big-endian; the public key is raw 32 bytes, never hex.
pub fn root_attestation_bytes(
    ephemeral_key_id: &str,
    ephemeral_public_key: &[u8; 32],
    not_before: i64,
    not_after: i64,
    region: &str,
) -> Vec<u8> {
    canonical_with_domain(
        ROOT_ATTESTATION_V1_DOMAIN,
        ROOT_ATTESTATION_KIND,
        &[
            ("ephemeral_key_id", ephemeral_key_id.as_bytes().to_vec()),
            ("ephemeral_public_key", ephemeral_public_key.to_vec()),
            ("not_before", not_before.to_be_bytes().to_vec()),
            ("not_after", not_after.to_be_bytes().to_vec()),
            ("region", region.as_bytes().to_vec()),
        ],
    )
}

fn canonical_with_domain(domain: &str, kind: &str, fields: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut result = format!("{domain}\nkind={}:{}", kind.len(), kind).into_bytes();
    for (name, value) in fields {
        result.extend_from_slice(format!("\n{name}={}:", value.len()).as_bytes());
        result.extend_from_slice(value);
    }
    result
}

pub fn parse_ephemeral_descriptor_set(body: &[u8]) -> Result<EphemeralDescriptorSet> {
    let set: EphemeralDescriptorSet = serde_json::from_slice(body).map_err(|error| {
        HostedError::InvalidDescriptor(format!("ephemeral descriptor set is malformed: {error}"))
    })?;
    if set.version != SERVED_SET_VERSION {
        return Err(HostedError::InvalidDescriptor(format!(
            "unsupported ephemeral descriptor set version {}",
            set.version
        )));
    }
    Ok(set)
}

/// Verify one entry against the pinned root. Never returns a trusted key
/// unless the root signature verifies over the bound tuple and the window
/// contains `now`.
pub fn verify_entry_against_root(
    entry: &RawEphemeralEntry,
    root_public_key: &[u8; 32],
    now_unix_millis: i64,
) -> std::result::Result<TrustedEphemeralEntry, EntryReject> {
    if entry.ephemeral_key_id.trim().is_empty() {
        return Err(EntryReject::InvalidKey);
    }
    if entry.not_before >= entry.not_after {
        return Err(EntryReject::InvalidWindow);
    }
    if entry.signature.trim().is_empty() {
        return Err(EntryReject::MissingSignature);
    }
    let public_key = parse_descriptor_public_key(&entry.ephemeral_public_key)
        .map_err(|_| EntryReject::InvalidKey)?;
    let signature = parse_attestation_signature(&entry.signature)?;
    let signed = root_attestation_bytes(
        &entry.ephemeral_key_id,
        &public_key,
        entry.not_before,
        entry.not_after,
        &entry.region,
    );
    match Ed25519Signer::verify_with_public_key(&signed, root_public_key, &signature) {
        Ok(()) => {}
        Err(_) => {
            return Err(if signature.iter().all(|byte| *byte == 0) {
                EntryReject::Unattested
            } else {
                EntryReject::InvalidSignature
            });
        }
    }
    if now_unix_millis < entry.not_before {
        return Err(EntryReject::NotYetValid);
    }
    if now_unix_millis >= entry.not_after {
        return Err(EntryReject::Expired);
    }
    Ok(TrustedEphemeralEntry {
        ephemeral_key_id: entry.ephemeral_key_id.clone(),
        ephemeral_public_key: public_key,
        not_before: entry.not_before,
        not_after: entry.not_after,
        region: entry.region.clone(),
        relay_urls: entry.relay_urls.clone(),
        direct_addresses: entry.direct_addresses.clone(),
    })
}

fn parse_attestation_signature(signature: &str) -> std::result::Result<Vec<u8>, EntryReject> {
    if signature.len() != 128
        || !signature
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(EntryReject::InvalidSignature);
    }
    let decoded = hex::decode(signature).map_err(|_| EntryReject::InvalidSignature)?;
    if decoded.len() != 64 {
        return Err(EntryReject::InvalidSignature);
    }
    Ok(decoded)
}

/// Filter a served set down to entries the pinned root currently attests.
///
/// Unattested, tampered, not-yet-valid, and expired entries are dropped
/// (never dialed). If nothing remains, the caller must fail closed.
pub fn trusted_live_entries(
    set: &EphemeralDescriptorSet,
    root_public_key: &[u8; 32],
    now_unix_millis: i64,
) -> (Vec<TrustedEphemeralEntry>, Vec<EntryReject>) {
    let mut trusted = Vec::new();
    let mut rejects = Vec::new();
    for entry in &set.entries {
        match verify_entry_against_root(entry, root_public_key, now_unix_millis) {
            Ok(trusted_entry) => trusted.push(trusted_entry),
            Err(reject) => rejects.push(reject),
        }
    }
    (trusted, rejects)
}

pub fn fail_if_none_trusted(
    trusted: &[TrustedEphemeralEntry],
    rejects: &[EntryReject],
) -> Result<()> {
    if !trusted.is_empty() {
        return Ok(());
    }
    if rejects.is_empty() {
        return Err(HostedError::EndpointDescriptorUnavailable);
    }
    if rejects.iter().all(|reject| {
        matches!(
            reject,
            EntryReject::Expired | EntryReject::NotYetValid | EntryReject::InvalidWindow
        )
    }) {
        return Err(HostedError::DescriptorOutsideValidityWindow);
    }
    if rejects.iter().any(|reject| {
        matches!(
            reject,
            EntryReject::Unattested | EntryReject::InvalidSignature | EntryReject::MissingSignature
        )
    }) {
        return Err(HostedError::InvalidDescriptorSignature);
    }
    Err(HostedError::InvalidDescriptor(
        "no currently valid root-attested endpoint".to_string(),
    ))
}

/// Prefer same-`region` entries, then fall back to the rest in served order.
/// Never drops a trusted remote entry.
pub fn order_for_dial<'a>(
    entries: &'a [TrustedEphemeralEntry],
    preferred_region: Option<&str>,
) -> Vec<&'a TrustedEphemeralEntry> {
    let Some(preferred) = preferred_region.filter(|region| !region.is_empty()) else {
        return entries.iter().collect();
    };
    let mut local = Vec::new();
    let mut remote = Vec::new();
    for entry in entries {
        if entry.region == preferred {
            local.push(entry);
        } else {
            remote.push(entry);
        }
    }
    local.extend(remote);
    local
}

impl TrustedEphemeralEntry {
    pub fn to_endpoint_descriptor(&self) -> EndpointDescriptor {
        EndpointDescriptor {
            version: 1,
            endpoint_id: hex::encode(self.ephemeral_public_key),
            relay_urls: self.relay_urls.clone(),
            direct_addresses: self.direct_addresses.clone(),
            supported_alpns: vec![HOSTED_ALPN_V1.to_vec()],
            issued_at_unix_millis: self.not_before,
            expires_at_unix_millis: self.not_after,
            rotation: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use api::signing::endpoint_descriptor_bytes;
    use crypto::Signer;

    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn generate_root() -> Ed25519Signer {
        Ed25519Signer::generate().expect("root")
    }

    fn sign_entry(
        root: &Ed25519Signer,
        key_id: &str,
        public_key: [u8; 32],
        not_before: i64,
        not_after: i64,
        region: &str,
    ) -> RawEphemeralEntry {
        let signature = root
            .sign(&root_attestation_bytes(
                key_id,
                &public_key,
                not_before,
                not_after,
                region,
            ))
            .expect("sign attestation");
        RawEphemeralEntry {
            ephemeral_key_id: key_id.to_string(),
            ephemeral_public_key: hex::encode(public_key),
            not_before,
            not_after,
            region: region.to_string(),
            signature: hex::encode(signature),
            relay_urls: Vec::new(),
            direct_addresses: vec!["127.0.0.1:9".to_string()],
        }
    }

    #[test]
    fn canonical_fields_are_length_delimited() {
        let key = [0x11; 32];
        let first = root_attestation_bytes("ab", &key, 1, 2, "hel");
        let second = root_attestation_bytes("a", &key, 1, 2, "bhel");
        assert_ne!(first, second);
        assert!(
            first.starts_with(b"heddle-descriptor-root-attestation-v1\nkind=16:root-attestation")
        );
        assert!(!first.starts_with(b"heddle-req-sig-v1"));
    }

    #[test]
    fn verify_against_root_accepts_a_live_attested_entry() {
        let root = generate_root();
        let root_pk: [u8; 32] = root.public_key().try_into().unwrap();
        let entry = sign_entry(
            &root,
            "ephemeral-1",
            [0x42; 32],
            NOW - 1,
            NOW + 60_000,
            "hel",
        );
        let trusted = verify_entry_against_root(&entry, &root_pk, NOW).unwrap();
        assert_eq!(trusted.ephemeral_key_id, "ephemeral-1");
        assert_eq!(trusted.ephemeral_public_key, [0x42; 32]);
        assert_eq!(trusted.region, "hel");
    }

    #[test]
    fn unattested_entry_is_rejected() {
        let root = generate_root();
        let root_pk: [u8; 32] = root.public_key().try_into().unwrap();
        let mut entry = sign_entry(
            &root,
            "ephemeral-1",
            [0x42; 32],
            NOW - 1,
            NOW + 60_000,
            "hel",
        );
        entry.signature.clear();
        assert_eq!(
            verify_entry_against_root(&entry, &root_pk, NOW).unwrap_err(),
            EntryReject::MissingSignature
        );
        entry.signature = "00".repeat(64);
        assert_eq!(
            verify_entry_against_root(&entry, &root_pk, NOW).unwrap_err(),
            EntryReject::Unattested
        );
    }

    #[test]
    fn tampered_binding_is_rejected() {
        let root = generate_root();
        let root_pk: [u8; 32] = root.public_key().try_into().unwrap();
        let mut entry = sign_entry(
            &root,
            "ephemeral-1",
            [0x42; 32],
            NOW - 1,
            NOW + 60_000,
            "hel",
        );
        entry.ephemeral_public_key = hex::encode([0x43; 32]);
        assert_eq!(
            verify_entry_against_root(&entry, &root_pk, NOW).unwrap_err(),
            EntryReject::InvalidSignature
        );
        let mut other_id = sign_entry(
            &root,
            "ephemeral-1",
            [0x42; 32],
            NOW - 1,
            NOW + 60_000,
            "hel",
        );
        other_id.ephemeral_key_id = "ephemeral-2".to_string();
        assert_eq!(
            verify_entry_against_root(&other_id, &root_pk, NOW).unwrap_err(),
            EntryReject::InvalidSignature
        );
        let mut other_region = sign_entry(
            &root,
            "ephemeral-1",
            [0x42; 32],
            NOW - 1,
            NOW + 60_000,
            "hel",
        );
        other_region.region = "sjc".to_string();
        assert_eq!(
            verify_entry_against_root(&other_region, &root_pk, NOW).unwrap_err(),
            EntryReject::InvalidSignature
        );
    }

    #[test]
    fn foreign_root_cannot_attest() {
        let root = generate_root();
        let other = generate_root();
        let other_pk: [u8; 32] = other.public_key().try_into().unwrap();
        let entry = sign_entry(
            &root,
            "ephemeral-1",
            [0x42; 32],
            NOW - 1,
            NOW + 60_000,
            "hel",
        );
        assert_eq!(
            verify_entry_against_root(&entry, &other_pk, NOW).unwrap_err(),
            EntryReject::InvalidSignature
        );
    }

    #[test]
    fn endpoint_descriptor_signature_cannot_substitute_as_attestation() {
        let root = generate_root();
        let root_pk: [u8; 32] = root.public_key().try_into().unwrap();
        let descriptor = EndpointDescriptor {
            version: 1,
            endpoint_id: hex::encode([0x42; 32]),
            relay_urls: Vec::new(),
            direct_addresses: vec!["127.0.0.1:9".to_string()],
            supported_alpns: vec![HOSTED_ALPN_V1.to_vec()],
            issued_at_unix_millis: NOW - 1,
            expires_at_unix_millis: NOW + 60_000,
            rotation: None,
        };
        let signature = root
            .sign(&endpoint_descriptor_bytes(&descriptor))
            .expect("sign descriptor");
        let entry = RawEphemeralEntry {
            ephemeral_key_id: "ephemeral-1".to_string(),
            ephemeral_public_key: hex::encode([0x42; 32]),
            not_before: NOW - 1,
            not_after: NOW + 60_000,
            region: "hel".to_string(),
            signature: hex::encode(signature),
            relay_urls: Vec::new(),
            direct_addresses: vec!["127.0.0.1:9".to_string()],
        };
        assert_eq!(
            verify_entry_against_root(&entry, &root_pk, NOW).unwrap_err(),
            EntryReject::InvalidSignature
        );
    }

    #[test]
    fn expired_and_not_yet_valid_entries_are_excluded() {
        let root = generate_root();
        let root_pk: [u8; 32] = root.public_key().try_into().unwrap();
        let expired = sign_entry(&root, "old", [0x11; 32], NOW - 10_000, NOW, "hel");
        assert_eq!(
            verify_entry_against_root(&expired, &root_pk, NOW).unwrap_err(),
            EntryReject::Expired
        );
        let pending = sign_entry(&root, "next", [0x22; 32], NOW + 1, NOW + 10_000, "hel");
        assert_eq!(
            verify_entry_against_root(&pending, &root_pk, NOW).unwrap_err(),
            EntryReject::NotYetValid
        );
        let inverted = sign_entry(&root, "bad", [0x33; 32], NOW + 10, NOW, "hel");
        assert_eq!(
            verify_entry_against_root(&inverted, &root_pk, NOW).unwrap_err(),
            EntryReject::InvalidWindow
        );
    }

    #[test]
    fn mixed_set_keeps_only_live_root_attested_entries() {
        let root = generate_root();
        let other = generate_root();
        let root_pk: [u8; 32] = root.public_key().try_into().unwrap();
        let live = sign_entry(&root, "live", [0x42; 32], NOW - 1, NOW + 60_000, "sjc");
        let expired = sign_entry(&root, "old", [0x11; 32], NOW - 10_000, NOW, "hel");
        let forged = sign_entry(&other, "evil", [0x99; 32], NOW - 1, NOW + 60_000, "hel");
        let set = EphemeralDescriptorSet {
            version: 2,
            entries: vec![expired, live, forged],
        };
        let (trusted, rejects) = trusted_live_entries(&set, &root_pk, NOW);
        assert_eq!(trusted.len(), 1);
        assert_eq!(trusted[0].ephemeral_key_id, "live");
        assert_eq!(rejects.len(), 2);
        fail_if_none_trusted(&trusted, &rejects).unwrap();
    }

    #[test]
    fn all_unattested_fails_closed_without_constructing_a_key() {
        let root = generate_root();
        let root_pk: [u8; 32] = root.public_key().try_into().unwrap();
        let mut entry = sign_entry(&root, "live", [0x42; 32], NOW - 1, NOW + 60_000, "hel");
        entry.signature = "00".repeat(64);
        let set = EphemeralDescriptorSet {
            version: 2,
            entries: vec![entry],
        };
        let (trusted, rejects) = trusted_live_entries(&set, &root_pk, NOW);
        assert!(trusted.is_empty());
        let error = fail_if_none_trusted(&trusted, &rejects).unwrap_err();
        assert!(matches!(error, HostedError::InvalidDescriptorSignature));
    }

    #[test]
    fn all_expired_fails_as_outside_window() {
        let root = generate_root();
        let root_pk: [u8; 32] = root.public_key().try_into().unwrap();
        let expired = sign_entry(&root, "old", [0x11; 32], NOW - 10_000, NOW, "hel");
        let set = EphemeralDescriptorSet {
            version: 2,
            entries: vec![expired],
        };
        let (trusted, rejects) = trusted_live_entries(&set, &root_pk, NOW);
        let error = fail_if_none_trusted(&trusted, &rejects).unwrap_err();
        assert!(matches!(
            error,
            HostedError::DescriptorOutsideValidityWindow
        ));
    }

    #[test]
    fn region_preference_does_not_drop_remote_entries() {
        let root = generate_root();
        let hel = sign_entry(&root, "hel-1", [0x11; 32], NOW - 1, NOW + 60_000, "hel");
        let sjc = sign_entry(&root, "sjc-1", [0x22; 32], NOW - 1, NOW + 60_000, "sjc");
        let root_pk: [u8; 32] = root.public_key().try_into().unwrap();
        let set = EphemeralDescriptorSet {
            version: 2,
            entries: vec![sjc, hel],
        };
        let (trusted, _) = trusted_live_entries(&set, &root_pk, NOW);
        let ordered = order_for_dial(&trusted, Some("hel"));
        assert_eq!(ordered[0].region, "hel");
        assert_eq!(ordered[1].region, "sjc");
        let remote_only = order_for_dial(&trusted, Some("iad"));
        assert_eq!(remote_only.len(), 2);
    }

    #[test]
    fn served_document_cannot_present_a_replacement_root() {
        let pinned = generate_root();
        let replacement = generate_root();
        let pinned_pk: [u8; 32] = pinned.public_key().try_into().unwrap();
        let body = serde_json::json!({
            "version": 2,
            "root_public_key": hex::encode(replacement.public_key()),
            "entries": [sign_entry(
                &replacement,
                "swap",
                [0x77; 32],
                NOW - 1,
                NOW + 60_000,
                "hel",
            )],
        });
        // Extra top-level fields are ignored; they must not become the pin.
        let set: EphemeralDescriptorSet =
            serde_json::from_value(body).expect("unknown root field is not the pin");
        assert_ne!(
            hex::encode(replacement.public_key()),
            hex::encode(pinned_pk),
            "the served document presented a different root"
        );
        let (trusted, rejects) = trusted_live_entries(&set, &pinned_pk, NOW);
        assert!(trusted.is_empty());
        assert_eq!(rejects, vec![EntryReject::InvalidSignature]);
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let error = parse_ephemeral_descriptor_set(br#"{"version":1,"entries":[]}"#).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported ephemeral descriptor set version 1")
        );
    }
}
