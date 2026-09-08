//! Portable evidence that a root-derived credential holder owns an Iroh endpoint.
//! The credential is private verification input, never part of the public record.
//! An attachment establishes a locator binding, not permission for future work:
//! verify each current request's complete Biscuit chain and revocations separately.
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use biscuit_verifier::PublicKey;
use chrono::{DateTime, Utc};
use crypto::{Ed25519Signer, Signer};
use prost::Message;

use crate::{contract::*, transport::Error};

pub const FORMAT: &str = "heddle.root-attachment.v1";
const DOMAIN: &[u8] = b"heddle.root-attachment.v1\0";
const MAX_CREDENTIAL: usize = 64 * 1024;

/// Evidence verified at the caller's supplied time. Keep the original credential
/// private when retaining this binding, and reverify it when using the evidence
/// again. This value is not a reusable authorization permit.
pub struct VerifiedAttachment {
    binding: RootAttachmentBinding,
}
impl VerifiedAttachment {
    pub fn root_public_key(&self) -> &[u8] {
        &self.binding.root_public_key
    }
    pub fn subject_public_key(&self) -> &[u8] {
        &self.binding.subject_public_key
    }
    pub fn expires_at_unix_seconds(&self) -> i64 {
        self.binding.expires_at_unix_seconds
    }
}

/// Sign with the paired subject key after receiving its root-derived credential.
/// Hosts still call `verify` with an independently configured root trust list.
pub fn sign(
    subject: &impl Signer,
    root_public_key: &[u8],
    credential: &[u8],
    device: EndpointRef,
    not_before_unix_seconds: i64,
    expires_at_unix_seconds: i64,
) -> Result<RootAttachment, Error> {
    let binding = RootAttachmentBinding {
        format_version: 1,
        root_public_key: root_public_key.to_vec(),
        subject_public_key: subject.public_key().to_vec(),
        device: Some(device.clone()),
        credential_digest: blake3::hash(credential).as_bytes().to_vec(),
        not_before_unix_seconds,
        expires_at_unix_seconds,
    };
    validate(&binding, credential)?;
    let canonical_record = binding.encode_to_vec();
    let signature = subject.sign(&statement(&canonical_record)).map_err(io)?;
    Ok(RootAttachment {
        root_public_key: root_public_key.to_vec(),
        subject_public_key: subject.public_key().to_vec(),
        device: Some(device),
        attachment: Some(SignedRecord {
            format: FORMAT.into(),
            canonical_record,
            signatures: vec![RecordSignature {
                public_key: subject.public_key().to_vec(),
                signature,
            }],
        }),
    })
}

pub fn verify(
    attachment: &RootAttachment,
    original_credential: &[u8],
    trusted_roots: &[PublicKey],
    expected_device: &EndpointRef,
    now: DateTime<Utc>,
) -> Result<VerifiedAttachment, Error> {
    let record = attachment
        .attachment
        .as_ref()
        .ok_or(Error::Protocol("missing endpoint attachment proof"))?;
    if record.format != FORMAT || record.canonical_record.len() > 1024 {
        return Err(Error::Protocol(
            "unsupported or oversized endpoint attachment",
        ));
    }
    let binding = RootAttachmentBinding::decode(record.canonical_record.as_slice())?;
    validate(&binding, original_credential)?;
    if binding.encode_to_vec() != record.canonical_record
        || binding.root_public_key != attachment.root_public_key
        || binding.subject_public_key != attachment.subject_public_key
        || binding.device != attachment.device
        || binding.device.as_ref() != Some(expected_device)
        || now.timestamp() < binding.not_before_unix_seconds
        || now.timestamp() >= binding.expires_at_unix_seconds
    {
        return Err(Error::Protocol(
            "endpoint attachment binding mismatch or expired",
        ));
    }
    let root = trusted_roots
        .iter()
        .find(|root| root.to_bytes() == binding.root_public_key)
        .ok_or(Error::Protocol("endpoint attachment root is not trusted"))?;
    let facts = biscuit_verifier::verify_any_at_with_resource(
        &URL_SAFE.encode(original_credential),
        None,
        &[*root],
        &[],
        "ObserveIdentity",
        None,
        now,
    )
    .map_err(|_| Error::Protocol("endpoint attachment credential is not valid"))?;
    if facts.cnf.as_deref() != Some(hex::encode(&binding.subject_public_key).as_str())
        || (facts.exp != 0 && binding.expires_at_unix_seconds as u64 > facts.exp)
    {
        return Err(Error::Protocol(
            "endpoint attachment exceeds credential subject or lifetime",
        ));
    }
    // Check the final admitted second against every ancestor time caveat, not
    // only the authority's expiry fact. The evidence is also checked at `now`;
    // this never replaces time/revocation checks on subsequent actual calls.
    let last = DateTime::from_timestamp(binding.expires_at_unix_seconds - 1, 0)
        .ok_or(Error::Protocol("invalid endpoint attachment lifetime"))?;
    biscuit_verifier::verify_any_at_with_resource(
        &URL_SAFE.encode(original_credential),
        None,
        &[*root],
        &[],
        "ObserveIdentity",
        None,
        last,
    )
    .map_err(|_| Error::Protocol("endpoint attachment outlives credential attenuation"))?;
    let [signature] = record.signatures.as_slice() else {
        return Err(Error::Protocol(
            "endpoint attachment requires one subject signature",
        ));
    };
    if signature.public_key != binding.subject_public_key {
        return Err(Error::Protocol(
            "endpoint attachment signer is not the credential subject",
        ));
    }
    Ed25519Signer::verify_with_public_key(
        &statement(&record.canonical_record),
        &signature.public_key,
        &signature.signature,
    )
    .map_err(|_| Error::Protocol("invalid endpoint attachment subject signature"))?;
    Ok(VerifiedAttachment { binding })
}

fn validate(binding: &RootAttachmentBinding, credential: &[u8]) -> Result<(), Error> {
    if binding.format_version != 1
        || binding.root_public_key.len() != 32
        || binding.subject_public_key.len() != 32
        || binding.device.as_ref().is_none_or(|device| {
            device.kind != EndpointKind::Device as i32 || device.public_key.len() != 32
        })
        || credential.is_empty()
        || credential.len() > MAX_CREDENTIAL
        || binding.credential_digest != blake3::hash(credential).as_bytes()
        || binding.not_before_unix_seconds <= 0
        || binding.expires_at_unix_seconds <= binding.not_before_unix_seconds
    {
        return Err(Error::Protocol("invalid endpoint attachment binding"));
    }
    Ok(())
}
fn statement(canonical: &[u8]) -> Vec<u8> {
    [DOMAIN, canonical].concat()
}
fn io(error: impl std::fmt::Display) -> Error {
    Error::Io(error.to_string())
}
