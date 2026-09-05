// SPDX-License-Identifier: Apache-2.0

use std::fmt;

use heddle_object_model::object::ContentHash;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identity of a runtime profile (UUIDv7). Distinct from any `StateId`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuntimeProfileId(Uuid);

impl RuntimeProfileId {
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
}

impl fmt::Display for RuntimeProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Content-addressed identity of an immutable runtime-profile version.
///
/// Deliberately not a [`heddle_object_model::object::StateId`]: Git Projection
/// and land take `StateId` only, so this type cannot be selected by those paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuntimeProfileStateId([u8; 32]);

impl RuntimeProfileStateId {
    pub fn for_bytes(bytes: &[u8]) -> Self {
        let hash = ContentHash::compute_typed("runtime-profile-state", bytes);
        Self(*hash.as_bytes())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        ContentHash::from_bytes(self.0).to_hex()
    }
}

impl fmt::Display for RuntimeProfileStateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecipientId([u8; 32]);

impl RecipientId {
    pub fn for_bytes(bytes: &[u8]) -> Self {
        let hash = ContentHash::compute_typed("runtime-profile-recipient", bytes);
        Self(*hash.as_bytes())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        ContentHash::from_bytes(self.0).to_hex()
    }
}

impl fmt::Display for RecipientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LifecycleRecordId([u8; 32]);

impl LifecycleRecordId {
    pub fn for_bytes(bytes: &[u8]) -> Self {
        let hash = ContentHash::compute_typed("runtime-profile-lifecycle", bytes);
        Self(*hash.as_bytes())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        ContentHash::from_bytes(self.0).to_hex()
    }
}

impl fmt::Display for LifecycleRecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CiphertextId([u8; 32]);

impl CiphertextId {
    pub fn for_bytes(bytes: &[u8]) -> Self {
        let hash = ContentHash::compute_typed("runtime-profile-ciphertext", bytes);
        Self(*hash.as_bytes())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        ContentHash::from_bytes(self.0).to_hex()
    }
}

impl fmt::Display for CiphertextId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AuditRecordId([u8; 32]);

impl AuditRecordId {
    pub fn for_bytes(bytes: &[u8]) -> Self {
        let hash = ContentHash::compute_typed("runtime-profile-audit", bytes);
        Self(*hash.as_bytes())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        ContentHash::from_bytes(self.0).to_hex()
    }
}

impl fmt::Display for AuditRecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}
