// SPDX-License-Identifier: Apache-2.0
//! Content hashing and change identifiers.

use std::fmt;

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{Error as DeError, SeqAccess, Visitor},
    ser::SerializeTuple,
};

/// Fixed-size hash/id bytes shared by domain-specific identifiers.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct SizedHash<const N: usize> {
    bytes: [u8; N],
}

impl<const N: usize> SizedHash<N> {
    /// Create from raw bytes.
    pub const fn from_bytes(bytes: [u8; N]) -> Self {
        Self { bytes }
    }

    /// Decode from an exact-length byte slice.
    pub fn try_from_slice(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != N {
            return None;
        }
        let mut arr = [0u8; N];
        arr.copy_from_slice(bytes);
        Some(Self::from_bytes(arr))
    }

    /// Get the raw bytes.
    pub const fn as_bytes(&self) -> &[u8; N] {
        &self.bytes
    }

    /// Convert to hexadecimal string.
    pub fn to_hex(&self) -> String {
        hex::encode(self.bytes)
    }

    /// Parse from hexadecimal string.
    pub fn from_hex(s: &str) -> Result<Self, hex::FromHexError> {
        let bytes = hex::decode(s)?;
        if bytes.len() != N {
            return Err(hex::FromHexError::InvalidStringLength);
        }
        let mut arr = [0u8; N];
        arr.copy_from_slice(&bytes);
        Ok(Self::from_bytes(arr))
    }

    /// Get a short hex prefix for display.
    pub fn short_hex(&self, chars: usize) -> String {
        let hex = self.to_hex();
        hex[..chars.min(hex.len())].to_string()
    }

    /// Check if a hex prefix matches this hash.
    pub fn matches_hex_prefix(&self, prefix: &str) -> bool {
        self.to_hex().starts_with(prefix)
    }

    /// Check if all bytes are zero.
    pub fn is_zero(&self) -> bool {
        self.bytes == [0u8; N]
    }
}

impl<const N: usize> Serialize for SizedHash<N> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut tuple = serializer.serialize_tuple(N)?;
        for byte in self.bytes {
            tuple.serialize_element(&byte)?;
        }
        tuple.end()
    }
}

impl<'de, const N: usize> Deserialize<'de> for SizedHash<N> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SizedHashVisitor<const N: usize>;

        impl<'de, const N: usize> Visitor<'de> for SizedHashVisitor<N> {
            type Value = SizedHash<N>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "exactly {N} hash bytes")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut bytes = [0u8; N];
                for (idx, slot) in bytes.iter_mut().enumerate() {
                    *slot = seq
                        .next_element()?
                        .ok_or_else(|| DeError::invalid_length(idx, &self))?;
                }
                if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(DeError::invalid_length(N + 1, &self));
                }
                Ok(SizedHash::from_bytes(bytes))
            }
        }

        deserializer.deserialize_tuple(N, SizedHashVisitor::<N>)
    }
}

/// Identifier types with a stable textual prefix.
pub trait PrefixedId {
    const PREFIX: &'static [u8];

    fn strip_prefix(input: &[u8]) -> Option<&[u8]> {
        input.strip_prefix(Self::PREFIX)
    }
}

/// A BLAKE3 content hash (32 bytes / 256 bits).
///
/// Used for content-addressing blobs, trees, and states.
/// The hash changes when content changes.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(SizedHash<32>);

impl ContentHash {
    /// Create a ContentHash from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(SizedHash::from_bytes(bytes))
    }

    /// Compute the hash of the given content.
    pub fn compute(content: &[u8]) -> Self {
        Self(SizedHash::from_bytes(blake3::hash(content).into()))
    }

    /// Compute hash with a type prefix (e.g., "blob", "tree", "state").
    pub fn compute_typed(type_prefix: &str, content: &[u8]) -> Self {
        let mut hasher = Self::typed_hasher(type_prefix, content.len() as u64);
        hasher.update(content);
        Self(SizedHash::from_bytes(hasher.finalize().into()))
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
        Self(SizedHash::from_bytes(hasher.finalize().into()))
    }

    /// Get the raw bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    /// Convert to hexadecimal string.
    pub fn to_hex(&self) -> String {
        self.0.to_hex()
    }

    /// Parse from hexadecimal string.
    pub fn from_hex(s: &str) -> Result<Self, hex::FromHexError> {
        SizedHash::from_hex(s).map(Self)
    }

    /// Get a short prefix for display (default 8 chars).
    pub fn short(&self) -> String {
        self.0.short_hex(8)
    }

    /// Check if a hex prefix matches this hash.
    pub fn matches_prefix(&self, prefix: &str) -> bool {
        self.0.matches_hex_prefix(prefix)
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
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChangeId(SizedHash<16>);

impl ChangeId {
    /// Generate a new random ChangeId.
    pub fn generate() -> Self {
        Self(SizedHash::from_bytes(rand::random()))
    }

    /// Create from raw bytes.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(SizedHash::from_bytes(bytes))
    }

    /// Decode from a 16-byte slice. Used at the proto/wire boundary where
    /// ChangeIds arrive as `bytes` fields (`Vec<u8>`).
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, ChangeIdParseError> {
        SizedHash::try_from_slice(bytes)
            .map(Self)
            .ok_or(ChangeIdParseError::InvalidLength)
    }

    /// Get the raw bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    /// Convert to display string (hd-XXXXXXXXXX...).
    pub fn to_string_full(&self) -> String {
        format!(
            "hd-{}",
            base32::encode(base32::Alphabet::Crockford, self.0.as_bytes()).to_lowercase()
        )
    }

    /// Short form for display (first 12 chars after prefix).
    pub fn short(&self) -> String {
        let full = self.to_string_full();
        full[..15.min(full.len())].to_string()
    }

    /// Parse from string (with or without hd- prefix).
    pub fn parse(s: &str) -> Result<Self, ChangeIdParseError> {
        let bytes = Self::strip_prefix(s.as_bytes()).unwrap_or_else(|| s.as_bytes());
        let s = std::str::from_utf8(bytes).map_err(|_| ChangeIdParseError::InvalidBase32)?;
        let bytes = base32::decode(base32::Alphabet::Crockford, &s.to_uppercase())
            .ok_or(ChangeIdParseError::InvalidBase32)?;
        Self::try_from_slice(&bytes)
    }

    /// Check if this ChangeId is all zeros (uninitialized).
    pub fn is_zero(&self) -> bool {
        self.0.is_zero()
    }
}

impl PrefixedId for ChangeId {
    const PREFIX: &'static [u8] = b"hd-";
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

    #[test]
    fn test_sized_hash_roundtrip() {
        let hash = SizedHash::<4>::from_bytes([1, 2, 3, 4]);

        assert_eq!(hash.as_bytes(), &[1, 2, 3, 4]);
        assert_eq!(hash.to_hex(), "01020304");
        assert_eq!(SizedHash::<4>::from_hex("01020304").unwrap(), hash);
        assert_eq!(SizedHash::<4>::try_from_slice(&[1, 2, 3, 4]), Some(hash));
        assert_eq!(hash.short_hex(2), "01");
        assert!(hash.matches_hex_prefix("0102"));
        assert!(!hash.is_zero());
    }

    #[test]
    fn test_prefixed_id_strip_prefix() {
        assert_eq!(ChangeId::strip_prefix(b"hd-abc"), Some(b"abc".as_slice()));
        assert_eq!(ChangeId::strip_prefix(b"abc"), None);
    }

    #[test]
    fn test_hash_wrappers_keep_array_serialization_shape() {
        let content_bytes = [7u8; 32];
        let content_hash = ContentHash::from_bytes(content_bytes);
        assert_eq!(
            serde_json::to_value(content_hash).unwrap(),
            serde_json::to_value(content_bytes).unwrap()
        );
        assert_eq!(
            rmp_serde::to_vec(&content_hash).unwrap(),
            rmp_serde::to_vec(&content_bytes).unwrap()
        );

        let change_bytes = [3u8; 16];
        let change_id = ChangeId::from_bytes(change_bytes);
        assert_eq!(
            serde_json::to_value(change_id).unwrap(),
            serde_json::to_value(change_bytes).unwrap()
        );
        assert_eq!(
            rmp_serde::to_vec(&change_id).unwrap(),
            rmp_serde::to_vec(&change_bytes).unwrap()
        );
    }
}
