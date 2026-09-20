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

pub const FORMAT: &str = "heddle.root-attachment.v2";
const DOMAIN: &[u8] = b"heddle.root-attachment.v2\0";
const MAX_CREDENTIAL: usize = 64 * 1024;

/// Evidence verified at the caller's supplied time. Keep the original credential
/// private when retaining this binding, and reverify it when using the evidence
/// again. This value is not a reusable authorization permit.
pub struct VerifiedAttachment {
    binding: RootAttachmentBinding,
    credential_revocation_ids: Vec<String>,
}
impl VerifiedAttachment {
    pub fn credential_revocation_ids(&self) -> &[String] {
        &self.credential_revocation_ids
    }
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

/// Sign a public approval commitment before receiving the private credential.
/// Both physical endpoint and credential subject prove possession; neither
/// signature establishes root authority. Hosts verify the delegated Biscuit too.
pub fn sign_binding(
    subject: &impl Signer,
    endpoint: &impl Signer,
    binding: RootAttachmentBinding,
) -> Result<RootAttachment, Error> {
    validate_binding(&binding)?;
    if binding.subject_public_key != subject.public_key()
        || binding
            .device
            .as_ref()
            .is_none_or(|device| device.public_key != endpoint.public_key())
    {
        return Err(Error::Protocol(
            "attachment signing keys do not match approval",
        ));
    }
    let canonical_record = binding.encode_to_vec();
    let payload = statement(&canonical_record);
    let mut signatures = vec![RecordSignature {
        public_key: subject.public_key().to_vec(),
        signature: subject.sign(&payload).map_err(io)?,
    }];
    if endpoint.public_key() != subject.public_key() {
        signatures.push(RecordSignature {
            public_key: endpoint.public_key().to_vec(),
            signature: endpoint.sign(&payload).map_err(io)?,
        });
    }
    Ok(RootAttachment {
        root_public_key: binding.root_public_key,
        subject_public_key: binding.subject_public_key,
        device: binding.device,
        attachment: Some(SignedRecord {
            format: FORMAT.into(),
            canonical_record,
            signatures,
        }),
    })
}

/// Check possession against the exact stored public approval. This alone is not
/// authority: the host separately verifies the private credential and account.
pub fn verify_possession(
    attachment: &RootAttachment,
    expected: &RootAttachmentBinding,
) -> Result<(), Error> {
    validate_binding(expected)?;
    let record = attachment
        .attachment
        .as_ref()
        .ok_or(Error::Protocol("missing endpoint attachment proof"))?;
    if record.format != FORMAT
        || record.canonical_record != expected.encode_to_vec()
        || attachment.root_public_key != expected.root_public_key
        || attachment.subject_public_key != expected.subject_public_key
        || attachment.device != expected.device
    {
        return Err(Error::Protocol("attachment differs from approved binding"));
    }
    let endpoint = expected
        .device
        .as_ref()
        .ok_or(Error::Protocol("missing endpoint"))?;
    let keys = if endpoint.public_key == expected.subject_public_key {
        vec![expected.subject_public_key.as_slice()]
    } else {
        vec![
            expected.subject_public_key.as_slice(),
            endpoint.public_key.as_slice(),
        ]
    };
    if record.signatures.len() != keys.len() {
        return Err(Error::Protocol(
            "attachment requires exact endpoint and subject signatures",
        ));
    }
    for (signature, key) in record.signatures.iter().zip(keys) {
        if signature.public_key != key {
            return Err(Error::Protocol(
                "attachment signer differs from expected key",
            ));
        }
        Ed25519Signer::verify_with_public_key(
            &statement(&record.canonical_record),
            key,
            &signature.signature,
        )
        .map_err(|_| Error::Protocol("invalid endpoint attachment possession signature"))?;
    }
    Ok(())
}

/// `trusted_roots` must be independently attached to `expected_account_id`.
/// The binding never supplies its own trust list or authorizes a future call.
pub fn verify(
    attachment: &RootAttachment,
    original_credential: &[u8],
    trusted_roots: &[PublicKey],
    expected_account_id: &str,
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
        || binding.account_id != expected_account_id
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
    if facts
        .subject_user_id()
        .is_some_and(|account| account.to_string() != expected_account_id)
        || facts.cnf.as_deref() != Some(hex::encode(&binding.subject_public_key).as_str())
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
    verify_possession(attachment, &binding)?;
    Ok(VerifiedAttachment {
        binding,
        credential_revocation_ids: facts.revocation_identities().map(str::to_owned).collect(),
    })
}

fn validate_binding(binding: &RootAttachmentBinding) -> Result<(), Error> {
    if binding.format_version != 2
        || uuid::Uuid::parse_str(&binding.account_id).map_or(true, |id| id.is_nil())
        || binding.pairing_challenge.len() != 32
        || binding.root_public_key.len() != 32
        || binding.subject_public_key.len() != 32
        || binding.device.as_ref().is_none_or(|device| {
            device.kind != EndpointKind::Device as i32 || device.public_key.len() != 32
        })
        || binding.credential_digest.len() != 32
        || binding.not_before_unix_seconds <= 0
        || binding.expires_at_unix_seconds <= binding.not_before_unix_seconds
    {
        return Err(Error::Protocol("invalid endpoint attachment binding"));
    }
    Ok(())
}
fn validate(binding: &RootAttachmentBinding, credential: &[u8]) -> Result<(), Error> {
    validate_binding(binding)?;
    if credential.is_empty()
        || credential.len() > MAX_CREDENTIAL
        || binding.credential_digest != blake3::hash(credential).as_bytes()
    {
        return Err(Error::Protocol("attachment credential digest mismatch"));
    }
    Ok(())
}
fn statement(canonical: &[u8]) -> Vec<u8> {
    [DOMAIN, canonical].concat()
}
fn io(error: impl std::fmt::Display) -> Error {
    Error::Io(error.to_string())
}
