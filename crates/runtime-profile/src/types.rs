// SPDX-License-Identifier: Apache-2.0

use heddle_object_model::object::{Attribution, FacetKind};
use serde::{Deserialize, Serialize};

use crate::ids::{CiphertextId, RecipientId, RuntimeProfileId, RuntimeProfileStateId};

pub const RUNTIME_PROFILE_SCHEMA_VERSION: u16 = 1;
pub const LIFECYCLE_SIGNING_DOMAIN: &[u8] = b"hd-runtime-lifecycle-v1";
pub const RECIPIENT_ENDORSE_DOMAIN: &[u8] = b"hd-runtime-recipient-v1";
pub const SLOT_AAD_DOMAIN: &[u8] = b"heddle-runtime-slot-v1";

/// Paths source capture must refuse to ingest as ordinary files once a
/// materialization path exists. `.heddleignore` is defense in depth only.
pub const RESERVED_MATERIALIZATION_PATHS: &[&str] = &[".env", ".env.local", ".env.rc"];

/// `staged → active → superseded → revoked → purge-eligible → purged`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
    Staged,
    Active,
    Superseded,
    Revoked,
    PurgeEligible,
    Purged,
}

impl LifecycleStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Revoked => "revoked",
            Self::PurgeEligible => "purge-eligible",
            Self::Purged => "purged",
        }
    }

    pub const fn decrypt_allowed(self) -> bool {
        matches!(self, Self::Staged | Self::Active | Self::Superseded)
    }

    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Staged, Self::Active)
                | (Self::Active, Self::Superseded)
                | (Self::Active, Self::Revoked)
                | (Self::Superseded, Self::Revoked)
                | (Self::Revoked, Self::PurgeEligible)
                | (Self::PurgeEligible, Self::Purged)
        )
    }
}

impl std::fmt::Display for LifecycleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapability {
    SoftwareExportable,
    Tpm,
    SecureEnclave,
    OsProvider,
    Pkcs11,
    RemoteHsm,
    Kms,
}

impl ProviderCapability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SoftwareExportable => "software-exportable",
            Self::Tpm => "tpm",
            Self::SecureEnclave => "secure-enclave",
            Self::OsProvider => "os-provider",
            Self::Pkcs11 => "pkcs11",
            Self::RemoteHsm => "remote-hsm",
            Self::Kms => "kms",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureBlock {
    pub algorithm: String,
    #[serde(with = "serde_bytes")]
    pub public_key: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub signature: Vec<u8>,
}

/// Public recipient descriptor endorsed by the principal's signing identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipientDescriptor {
    pub schema_version: u16,
    pub recipient_id: RecipientId,
    pub capability: ProviderCapability,
    pub wrap_alg: String,
    pub key_version: u32,
    #[serde(with = "serde_bytes")]
    pub public_key: Vec<u8>,
    pub endorsement: SignatureBlock,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrappedDekRecord {
    pub recipient_id: RecipientId,
    #[serde(with = "serde_bytes")]
    pub ephemeral_public: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub nonce: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub ciphertext: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotRecord {
    pub name: String,
    pub aead_alg: String,
    pub pad_bucket: u32,
    pub ciphertext_id: CiphertextId,
    pub dek_wraps: Vec<WrappedDekRecord>,
}

/// Mutable typed root. Atomically replaced; versions stay immutable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeProfileRef {
    pub schema_version: u16,
    pub profile_id: RuntimeProfileId,
    pub name: String,
    pub facet: FacetKindWire,
    pub head: RuntimeProfileStateId,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub attribution: Attribution,
}

/// Wire form of [`FacetKind`] so a profile file is self-describing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FacetKindWire {
    ConfidentialRuntime,
}

impl FacetKindWire {
    pub const fn facet_kind(self) -> FacetKind {
        FacetKind::ConfidentialRuntime
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeProfileState {
    pub schema_version: u16,
    pub state_id: RuntimeProfileStateId,
    pub profile_id: RuntimeProfileId,
    pub parent: Option<RuntimeProfileStateId>,
    pub version: u64,
    pub lifecycle: LifecycleStatus,
    pub slots: Vec<SlotRecord>,
    pub recipient_ids: Vec<RecipientId>,
    pub policy_ref: Option<String>,
    pub created_at_ms: i64,
    pub attribution: Attribution,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRecord {
    pub schema_version: u16,
    pub record_id: crate::ids::LifecycleRecordId,
    pub profile_id: RuntimeProfileId,
    pub state_id: RuntimeProfileStateId,
    pub from: Option<LifecycleStatus>,
    pub to: LifecycleStatus,
    pub occurred_at_ms: i64,
    pub attribution: Attribution,
    pub signature: SignatureBlock,
}

/// Slot metadata returned without decrypting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotMetadata {
    pub name: String,
    pub aead_alg: String,
    pub pad_bucket: u32,
    pub ciphertext_id: CiphertextId,
    pub recipient_ids: Vec<RecipientId>,
}

/// Profile listing row. No slot values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileMetadata {
    pub profile_id: RuntimeProfileId,
    pub name: String,
    pub facet: FacetKind,
    pub head: RuntimeProfileStateId,
    pub version: u64,
    pub lifecycle: LifecycleStatus,
    pub slot_names: Vec<String>,
}

pub fn slot_aad(profile_id: RuntimeProfileId, slot: &str) -> Vec<u8> {
    let mut aad = SLOT_AAD_DOMAIN.to_vec();
    aad.extend_from_slice(profile_id.as_bytes());
    aad.extend_from_slice(slot.as_bytes());
    aad
}

pub fn validate_profile_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err("profile name must be 1..=64 characters".to_string());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err("profile name must be [A-Za-z0-9._-]".to_string());
    }
    Ok(())
}

pub fn validate_slot_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 128 {
        return Err("slot name must be 1..=128 characters".to_string());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err("slot name must be [A-Z0-9_]".to_string());
    }
    Ok(())
}
