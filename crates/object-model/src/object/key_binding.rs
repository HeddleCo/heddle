// SPDX-License-Identifier: Apache-2.0
//! Offline-verifiable bindings from signing keys to durable identities.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ContentHash, StateSignature};

/// Domain separator for signatures that authorize a [`KeyBinding`].
pub const KEY_BINDING_SIGNING_PAYLOAD_VERSION_TAG: &[u8] = b"hd-key-binding-v1\x00";

/// Domain separator for authority signatures over registry checkpoints.
pub const KEY_BINDING_REGISTRY_SIGNING_PAYLOAD_VERSION_TAG: &[u8] =
    b"hd-key-binding-registry-v2\x00";

/// A signing key's role within an identity's provenance chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyBinding {
    /// Signature algorithm used by the bound key.
    pub algorithm: String,
    /// Hex-encoded raw public key bytes.
    pub public_key: String,
    /// Durable identity subject resolved by this key.
    pub identity_ref: String,
    /// Repository role granted to this key, such as `author` or `ci-runner`.
    pub role: String,
    /// Identity-key signature authorizing this binding.
    pub added_by_sig: StateSignature,
    /// First instant at which this binding may authenticate authored objects.
    pub valid_from: DateTime<Utc>,
    /// First instant at which this binding no longer authenticates new objects.
    #[serde(default)]
    pub revoked_at: Option<DateTime<Utc>>,
    /// Content hash of the identity-owned root binding that authorized this
    /// key. Only one delegation hop is permitted by repository verification.
    #[serde(default)]
    pub delegated_from: Option<ContentHash>,
}

impl KeyBinding {
    /// Deterministic bytes covered by [`Self::added_by_sig`].
    pub fn canonical_signing_payload(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(256);
        payload.extend_from_slice(KEY_BINDING_SIGNING_PAYLOAD_VERSION_TAG);
        push_field(&mut payload, self.algorithm.as_bytes());
        push_field(&mut payload, self.public_key.as_bytes());
        push_field(&mut payload, self.identity_ref.as_bytes());
        push_field(&mut payload, self.role.as_bytes());
        push_time(&mut payload, self.valid_from);
        push_optional_time(&mut payload, self.revoked_at);
        push_optional_hash(&mut payload, self.delegated_from);
        payload
    }

    /// Stable address of this signed binding.
    pub fn content_hash(&self) -> Result<ContentHash, KeyBindingError> {
        self.validate()?;
        let encoded =
            rmp_serde::to_vec(self).map_err(|error| KeyBindingError::Codec(error.to_string()))?;
        Ok(ContentHash::compute_typed("key-binding", &encoded))
    }

    /// Validate the durable shape. Cryptographic authorization is checked by
    /// the repository resolver, which has access to the signing backends.
    pub fn validate(&self) -> Result<(), KeyBindingError> {
        require_non_empty(&self.algorithm, KeyBindingError::EmptyAlgorithm)?;
        require_hex(&self.public_key, KeyBindingError::InvalidPublicKey)?;
        require_non_empty(&self.identity_ref, KeyBindingError::EmptyIdentityRef)?;
        require_non_empty(&self.role, KeyBindingError::EmptyRole)?;
        require_non_empty(
            &self.added_by_sig.algorithm,
            KeyBindingError::EmptyAddedByAlgorithm,
        )?;
        require_hex(
            &self.added_by_sig.public_key,
            KeyBindingError::InvalidAddedByPublicKey,
        )?;
        require_hex(
            &self.added_by_sig.signature,
            KeyBindingError::InvalidAddedBySignature,
        )?;
        if self
            .revoked_at
            .is_some_and(|revoked| revoked < self.valid_from)
        {
            return Err(KeyBindingError::RevokedBeforeValid);
        }
        Ok(())
    }
}

/// Versioned, authority-signed checkpoint of the key-binding registry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyBindingRegistry {
    pub format_version: u8,
    /// Monotonic checkpoint number. Genesis is epoch zero.
    pub epoch: u64,
    /// Content hash of the immediately preceding checkpoint.
    pub previous_registry: Option<ContentHash>,
    /// Trusted repository/identity-authority signature over this checkpoint.
    pub authority_signature: StateSignature,
    pub bindings: Vec<KeyBinding>,
}

impl KeyBindingRegistry {
    pub const FORMAT_VERSION: u8 = 2;

    pub fn new(
        epoch: u64,
        previous_registry: Option<ContentHash>,
        authority_signature: StateSignature,
        bindings: Vec<KeyBinding>,
    ) -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            epoch,
            previous_registry,
            authority_signature,
            bindings,
        }
    }

    /// Deterministic bytes covered by [`Self::authority_signature`].
    pub fn canonical_checkpoint_signing_payload(&self) -> Result<Vec<u8>, KeyBindingError> {
        let mut payload = Vec::with_capacity(128 + self.bindings.len() * 32);
        payload.extend_from_slice(KEY_BINDING_REGISTRY_SIGNING_PAYLOAD_VERSION_TAG);
        payload.push(self.format_version);
        payload.extend_from_slice(&self.epoch.to_le_bytes());
        push_optional_hash(&mut payload, self.previous_registry);
        payload.extend_from_slice(&(self.bindings.len() as u64).to_le_bytes());
        for binding in &self.bindings {
            payload.extend_from_slice(binding.content_hash()?.as_bytes());
        }
        Ok(payload)
    }

    pub fn encode(&self) -> Result<Vec<u8>, KeyBindingError> {
        self.validate()?;
        rmp_serde::to_vec(self).map_err(|error| KeyBindingError::Codec(error.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, KeyBindingError> {
        let registry: Self = rmp_serde::from_slice(bytes)
            .map_err(|error| KeyBindingError::Codec(error.to_string()))?;
        registry.validate()?;
        Ok(registry)
    }

    /// Stable address of the registry's validated canonical encoding.
    pub fn content_hash(&self) -> Result<ContentHash, KeyBindingError> {
        Ok(ContentHash::compute_typed(
            "key-binding-registry",
            &self.encode()?,
        ))
    }

    pub fn validate(&self) -> Result<(), KeyBindingError> {
        if self.format_version != Self::FORMAT_VERSION {
            return Err(KeyBindingError::UnsupportedVersion(self.format_version));
        }
        match (self.epoch, self.previous_registry) {
            (0, Some(_)) => return Err(KeyBindingError::GenesisHasPrevious),
            (1.., None) => return Err(KeyBindingError::MissingPreviousRegistry(self.epoch)),
            _ => {}
        }
        require_non_empty(
            &self.authority_signature.algorithm,
            KeyBindingError::EmptyAuthorityAlgorithm,
        )?;
        require_hex(
            &self.authority_signature.public_key,
            KeyBindingError::InvalidAuthorityPublicKey,
        )?;
        require_hex(
            &self.authority_signature.signature,
            KeyBindingError::InvalidAuthoritySignature,
        )?;
        for (index, binding) in self.bindings.iter().enumerate() {
            binding.validate()?;
            if self.bindings[..index].iter().any(|prior| {
                prior.algorithm.eq_ignore_ascii_case(&binding.algorithm)
                    && prior.public_key.eq_ignore_ascii_case(&binding.public_key)
            }) {
                return Err(KeyBindingError::DuplicateKey);
            }
        }
        Ok(())
    }
}

fn require_non_empty(value: &str, error: KeyBindingError) -> Result<(), KeyBindingError> {
    if value.trim().is_empty() {
        Err(error)
    } else {
        Ok(())
    }
}

fn require_hex(value: &str, error: KeyBindingError) -> Result<(), KeyBindingError> {
    if value.is_empty() || hex::decode(value).is_err() {
        Err(error)
    } else {
        Ok(())
    }
}

fn push_field(payload: &mut Vec<u8>, value: &[u8]) {
    payload.extend_from_slice(&(value.len() as u64).to_le_bytes());
    payload.extend_from_slice(value);
}

fn push_optional_hash(payload: &mut Vec<u8>, value: Option<ContentHash>) {
    match value {
        Some(value) => {
            payload.push(1);
            payload.extend_from_slice(value.as_bytes());
        }
        None => payload.push(0),
    }
}

fn push_time(payload: &mut Vec<u8>, value: DateTime<Utc>) {
    payload.extend_from_slice(&value.timestamp().to_le_bytes());
    payload.extend_from_slice(&value.timestamp_subsec_nanos().to_le_bytes());
}

fn push_optional_time(payload: &mut Vec<u8>, value: Option<DateTime<Utc>>) {
    match value {
        Some(value) => {
            payload.push(1);
            push_time(payload, value);
        }
        None => payload.push(0),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyBindingError {
    #[error("unsupported key-binding registry version {0}")]
    UnsupportedVersion(u8),
    #[error("key-binding registry codec error: {0}")]
    Codec(String),
    #[error("genesis key-binding registry must not reference a previous checkpoint")]
    GenesisHasPrevious,
    #[error("key-binding registry epoch {0} must reference its previous checkpoint")]
    MissingPreviousRegistry(u64),
    #[error("key-binding registry authority signature algorithm must not be empty")]
    EmptyAuthorityAlgorithm,
    #[error("key-binding registry authority public key must be non-empty hexadecimal bytes")]
    InvalidAuthorityPublicKey,
    #[error("key-binding registry authority signature must be non-empty hexadecimal bytes")]
    InvalidAuthoritySignature,
    #[error("key binding algorithm must not be empty")]
    EmptyAlgorithm,
    #[error("key binding public key must be non-empty hexadecimal bytes")]
    InvalidPublicKey,
    #[error("key binding identity_ref must not be empty")]
    EmptyIdentityRef,
    #[error("key binding role must not be empty")]
    EmptyRole,
    #[error("key binding authorizing signature algorithm must not be empty")]
    EmptyAddedByAlgorithm,
    #[error("key binding authorizing public key must be non-empty hexadecimal bytes")]
    InvalidAddedByPublicKey,
    #[error("key binding authorizing signature must be non-empty hexadecimal bytes")]
    InvalidAddedBySignature,
    #[error("key binding revoked_at must not precede valid_from")]
    RevokedBeforeValid,
    #[error("key-binding registry contains a duplicate algorithm/public-key pair")]
    DuplicateKey,
}
