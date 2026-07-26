// SPDX-License-Identifier: Apache-2.0

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use uuid::Uuid;

use crate::object::{ContentHash, StateAttachmentId, StateId};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct CollabOpId([u8; 32]);

impl CollabOpId {
    pub fn for_bytes(bytes: &[u8]) -> Self {
        Self(*ContentHash::compute_typed("collaboration-operation", bytes).as_bytes())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn to_string_full(&self) -> String {
        format!(
            "co-{}",
            base32::encode(base32::Alphabet::Crockford, &self.0).to_lowercase()
        )
    }

    pub fn parse(value: &str) -> Result<Self, CollabOpIdParseError> {
        let Some(value) = value.strip_prefix("co-") else {
            return Err(CollabOpIdParseError::MissingPrefix);
        };
        let bytes = base32::decode(base32::Alphabet::Crockford, &value.to_uppercase())
            .ok_or(CollabOpIdParseError::InvalidBase32)?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| CollabOpIdParseError::InvalidLength)?;
        Ok(Self(bytes))
    }
}

impl fmt::Debug for CollabOpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_full())
    }
}

impl fmt::Display for CollabOpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_full())
    }
}

impl FromStr for CollabOpId {
    type Err = CollabOpIdParseError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for CollabOpId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string_full())
    }
}

impl<'de> Deserialize<'de> for CollabOpId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CollabOpIdParseError {
    #[error("collaboration operation id must start with co-")]
    MissingPrefix,
    #[error("invalid collaboration operation base32")]
    InvalidBase32,
    #[error("collaboration operation id must contain 32 bytes")]
    InvalidLength,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct DiscussionRecordId(Uuid);

impl DiscussionRecordId {
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    pub fn from_uuid(value: Uuid) -> Result<Self, DiscussionRecordIdParseError> {
        if value.get_version_num() != 7 {
            return Err(DiscussionRecordIdParseError::WrongVersion);
        }
        Ok(Self(value))
    }

    pub fn for_legacy_source(source: &LegacySourceLocator, opened_at_ms: i64) -> Self {
        let hash = ContentHash::compute_typed("legacy-discussion", &source.identity_bytes());
        let mut bytes = [0; 16];
        bytes[..6].copy_from_slice(&(opened_at_ms.max(0) as u64).to_be_bytes()[2..]);
        bytes[6..].copy_from_slice(&hash.as_bytes()[..10]);
        bytes[6] = (bytes[6] & 0x0f) | 0x70;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(Uuid::from_bytes(bytes))
    }
}

impl fmt::Display for DiscussionRecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "disc-{}", self.0)
    }
}

impl FromStr for DiscussionRecordId {
    type Err = DiscussionRecordIdParseError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value
            .strip_prefix("disc-")
            .ok_or(DiscussionRecordIdParseError::MissingPrefix)?;
        Self::from_uuid(Uuid::parse_str(value).map_err(DiscussionRecordIdParseError::InvalidUuid)?)
    }
}

impl Serialize for DiscussionRecordId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for DiscussionRecordId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiscussionRecordIdParseError {
    #[error("discussion id must start with disc-")]
    MissingPrefix,
    #[error("invalid discussion UUID: {0}")]
    InvalidUuid(uuid::Error),
    #[error("discussion id must be UUIDv7")]
    WrongVersion,
}

macro_rules! nonempty_id {
    ($name:ident, $message:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, String> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err($message.to_string());
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
            }
        }
    };
}

nonempty_id!(
    CollaborationIdempotencyKey,
    "idempotency key must not be empty"
);
nonempty_id!(LegacyDiscussionId, "legacy discussion id must not be empty");

#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct LegacySourceLocator {
    pub state_id: StateId,
    pub attachment_id: StateAttachmentId,
    pub blob_hash: ContentHash,
}

impl LegacySourceLocator {
    pub fn new(
        state_id: StateId,
        attachment_id: StateAttachmentId,
        blob_hash: ContentHash,
    ) -> Self {
        Self {
            state_id,
            attachment_id,
            blob_hash,
        }
    }

    pub fn identity_bytes(&self) -> [u8; 96] {
        let mut bytes = [0; 96];
        bytes[..32].copy_from_slice(self.state_id.as_bytes());
        bytes[32..64].copy_from_slice(self.attachment_id.as_hash().as_bytes());
        bytes[64..].copy_from_slice(self.blob_hash.as_bytes());
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialized_nonempty_ids_cannot_bypass_validation() {
        let bytes = rmp_serde::to_vec_named("").unwrap();
        assert!(rmp_serde::from_slice::<CollaborationIdempotencyKey>(&bytes).is_err());
        assert!(rmp_serde::from_slice::<LegacyDiscussionId>(&bytes).is_err());
    }

    #[test]
    fn legacy_locator_preserves_full_typed_identities() {
        let locator = LegacySourceLocator::new(
            StateId::from_bytes([1; 32]),
            StateAttachmentId::from_hash(ContentHash::from_bytes([2; 32])),
            ContentHash::from_bytes([3; 32]),
        );
        let bytes = locator.identity_bytes();
        assert_eq!(&bytes[..32], &[1; 32]);
        assert_eq!(&bytes[32..64], &[2; 32]);
        assert_eq!(&bytes[64..], &[3; 32]);
    }
}
