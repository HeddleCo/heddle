// SPDX-License-Identifier: Apache-2.0
//! Content hashing and change identifiers.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A BLAKE3 content hash (32 bytes / 256 bits).
///
/// Used for content-addressing blobs, trees, and states.
/// The hash changes when content changes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// Create a ContentHash from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Compute the hash of the given content.
    pub fn compute(content: &[u8]) -> Self {
        Self(blake3::hash(content).into())
    }

    /// Compute hash with a type prefix (e.g., "blob", "tree", "state").
    pub fn compute_typed(type_prefix: &str, content: &[u8]) -> Self {
        let mut hasher = Self::typed_hasher(type_prefix, content.len() as u64);
        hasher.update(content);
        Self(hasher.finalize().into())
    }

    /// Create a typed hasher pre-seeded with the prefix and length.
    pub fn typed_hasher(type_prefix: &str, content_len: u64) -> blake3::Hasher {
        let mut hasher = blake3::Hasher::new();
        hasher.update(type_prefix.as_bytes());
        hasher.update(&content_len.to_le_bytes());
        hasher.update(&[0]);
        hasher
    }

    /// Compute hash with a known content length using incremental updates.
    pub fn compute_typed_with_len(
        type_prefix: &str,
        content_len: u64,
        update: impl FnOnce(&mut blake3::Hasher),
    ) -> Self {
        let mut hasher = Self::typed_hasher(type_prefix, content_len);
        update(&mut hasher);
        Self(hasher.finalize().into())
    }

    /// Get the raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Convert to hexadecimal string.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse from hexadecimal string.
    pub fn from_hex(s: &str) -> Result<Self, hex::FromHexError> {
        let bytes = hex::decode(s)?;
        if bytes.len() != 32 {
            return Err(hex::FromHexError::InvalidStringLength);
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }

    /// Get a short prefix for display (default 8 chars).
    pub fn short(&self) -> String {
        self.to_hex()[..8].to_string()
    }

    /// Check if a hex prefix matches this hash.
    pub fn matches_prefix(&self, prefix: &str) -> bool {
        self.to_hex().starts_with(prefix)
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({})", self.short())
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// A stable change identifier (16 bytes / 128 bits).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ChangeId([u8; 16]);

impl ChangeId {
    /// Generate a new random ChangeId.
    pub fn generate() -> Self {
        Self(rand::random())
    }

    /// Create from raw bytes.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// Decode from a 16-byte slice. Used at the proto/wire boundary where
    /// ChangeIds arrive as `bytes` fields (`Vec<u8>`).
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, ChangeIdParseError> {
        if bytes.len() != 16 {
            return Err(ChangeIdParseError::InvalidLength);
        }
        let mut arr = [0u8; 16];
        arr.copy_from_slice(bytes);
        Ok(Self(arr))
    }

    /// Get the raw bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Convert to display string (hd-XXXXXXXXXX...).
    pub fn to_string_full(&self) -> String {
        format!(
            "hd-{}",
            base32::encode(base32::Alphabet::Crockford, &self.0).to_lowercase()
        )
    }

    /// Short form for display (first 12 chars after prefix).
    pub fn short(&self) -> String {
        let full = self.to_string_full();
        full[..15.min(full.len())].to_string()
    }

    /// Parse from string (with or without hd- prefix).
    pub fn parse(s: &str) -> Result<Self, ChangeIdParseError> {
        let s = s.strip_prefix("hd-").unwrap_or(s);
        let bytes = base32::decode(base32::Alphabet::Crockford, &s.to_uppercase())
            .ok_or(ChangeIdParseError::InvalidBase32)?;
        if bytes.len() != 16 {
            return Err(ChangeIdParseError::InvalidLength);
        }
        let mut arr = [0u8; 16];
        arr.copy_from_slice(&bytes);
        Ok(Self(arr))
    }

    /// Check if this ChangeId is all zeros (uninitialized).
    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; 16]
    }
}

impl fmt::Debug for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChangeId({})", self.short())
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.short())
    }
}

/// Error parsing a ChangeId.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ChangeIdParseError {
    #[error("invalid base32 encoding")]
    InvalidBase32,
    #[error("invalid length (expected 16 bytes)")]
    InvalidLength,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_hash_compute() {
        let hash = ContentHash::compute(b"hello world");
        assert_eq!(hash.to_hex().len(), 64);

        let hash2 = ContentHash::compute(b"hello world");
        assert_eq!(hash, hash2);

        let hash3 = ContentHash::compute(b"hello world!");
        assert_ne!(hash, hash3);
    }

    #[test]
    fn test_content_hash_typed() {
        let hash1 = ContentHash::compute_typed("blob", b"hello");
        let hash2 = ContentHash::compute_typed("tree", b"hello");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_hex_roundtrip() {
        let hash = ContentHash::compute(b"test");
        let hex = hash.to_hex();
        let parsed = ContentHash::from_hex(&hex).unwrap();
        assert_eq!(hash, parsed);
    }

    #[test]
    fn test_change_id_generate() {
        let id1 = ChangeId::generate();
        let id2 = ChangeId::generate();
        assert_ne!(id1, id2);
        assert!(!id1.is_zero());
    }

    #[test]
    fn test_change_id_roundtrip() {
        let id = ChangeId::generate();
        let s = id.to_string_full();
        assert!(s.starts_with("hd-"));
        let parsed = ChangeId::parse(&s).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_change_id_short() {
        let id = ChangeId::generate();
        let short = id.short();
        assert!(short.starts_with("hd-"));
        assert!(short.len() <= 15);
    }
}