// SPDX-License-Identifier: Apache-2.0
//! Canonical manifest bindings — what ties a content root to an identity.
//!
//! A binding names `(spool, facet, owner, content_root)`. The owner descriptor
//! is deliberately **outside** the content root: every state and attachment has
//! a distinct owner hash, while a context-only state must reuse its parent's
//! content root byte-for-byte. Keeping owner identity in the binding is what
//! makes that reuse possible.
//!
//! Facet identity follows the ratified weft #358 decision: every spool carries
//! all four facets (content, governance, membership, children) uniformly, with
//! no content-bearing discriminant, so a binding always names its facet
//! explicitly rather than defaulting. The tokens match
//! `heddle_refs::SpoolFacet::token()` exactly — this type exists only because
//! the object model sits below the refs crate, not to introduce a second
//! spelling.
//!
//! Mutable facts stay out. Audience and visibility, the current head, and pack
//! locations are control-plane data resolved after authorization; changing who
//! may see a state must not rewrite a single content byte.
//!
//! ```text
//! binding: "WPMB" | u8(version=1) | u16(spool_len) | spool
//!                 | u16(facet_len) | facet | u8(owner_kind)
//!                 | [u8;32](owner_hash) | u64(owner_decoded_size)
//!                 | [u8;32](content_root)
//! ```

use std::fmt;

use crate::object::{ContentHash, SpoolId};

/// Magic prefix on every canonical binding.
pub const MANIFEST_BINDING_MAGIC: [u8; 4] = *b"WPMB";
/// The only binding format version this binary reads or writes.
pub const MANIFEST_BINDING_VERSION: u8 = 1;

/// The four uniform spool facets from weft #358, plus an open named tail.
///
/// `Named` keeps the set genuinely open — the substrate treats a facet as a
/// token. A `Named` token that spells a well-known facet normalizes to it, so
/// the two spellings can never diverge in a content hash.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ManifestFacet {
    Content,
    Governance,
    Membership,
    Children,
    Named(String),
}

impl ManifestFacet {
    /// The facet token as it appears in canonical bytes, scope tokens, and ref
    /// names.
    pub fn token(&self) -> &str {
        match self {
            Self::Content => "content",
            Self::Governance => "governance",
            Self::Membership => "membership",
            Self::Children => "children",
            Self::Named(token) => token.as_str(),
        }
    }

    /// Parse a token, normalizing the four well-known spellings.
    pub fn parse(token: impl AsRef<str>) -> Result<Self, ManifestFacetParseError> {
        let token = token.as_ref();
        match token {
            "content" => return Ok(Self::Content),
            "governance" => return Ok(Self::Governance),
            "membership" => return Ok(Self::Membership),
            "children" => return Ok(Self::Children),
            _ => {}
        }
        if !valid_facet_token(token) {
            return Err(ManifestFacetParseError(token.to_string()));
        }
        Ok(Self::Named(token.to_string()))
    }

    /// The four facets every spool carries, per weft #358.
    pub fn well_known() -> [Self; 4] {
        [
            Self::Content,
            Self::Governance,
            Self::Membership,
            Self::Children,
        ]
    }
}

fn valid_facet_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 64
        && token
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
        && token.as_bytes()[0].is_ascii_alphanumeric()
        && token.as_bytes()[token.len() - 1].is_ascii_alphanumeric()
}

impl fmt::Display for ManifestFacet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid facet token '{0}'")]
pub struct ManifestFacetParseError(String);

/// What a content root is bound to.
///
/// Distinct from [`super::node::ManifestObjectKind`] on purpose: leaves name
/// content (blobs and trees), while owners name the publication unit.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ManifestOwnerKind {
    State = 0,
    StateAttachment = 1,
}

impl ManifestOwnerKind {
    pub fn to_byte(self) -> u8 {
        self as u8
    }

    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::State),
            1 => Some(Self::StateAttachment),
            _ => None,
        }
    }
}

/// An immutable `(spool, facet, owner) -> content_root` binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestBinding {
    pub spool: SpoolId,
    pub facet: ManifestFacet,
    pub owner_kind: ManifestOwnerKind,
    pub owner_hash: ContentHash,
    pub owner_decoded_size: u64,
    pub content_root: ContentHash,
}

impl ManifestBinding {
    /// Encode to the single canonical byte string for this binding.
    pub fn encode(&self) -> Vec<u8> {
        let spool = self.spool.as_str();
        let facet = self.facet.token();
        let mut out =
            Vec::with_capacity(4 + 1 + 2 + spool.len() + 2 + facet.len() + 1 + 32 + 8 + 32);
        out.extend_from_slice(&MANIFEST_BINDING_MAGIC);
        out.push(MANIFEST_BINDING_VERSION);
        out.extend_from_slice(&(spool.len() as u16).to_be_bytes());
        out.extend_from_slice(spool.as_bytes());
        out.extend_from_slice(&(facet.len() as u16).to_be_bytes());
        out.extend_from_slice(facet.as_bytes());
        out.push(self.owner_kind.to_byte());
        out.extend_from_slice(self.owner_hash.as_bytes());
        out.extend_from_slice(&self.owner_decoded_size.to_be_bytes());
        out.extend_from_slice(self.content_root.as_bytes());
        out
    }

    /// The binding's content address — `BLAKE3` of its canonical bytes.
    pub fn address(&self) -> ContentHash {
        ContentHash::compute(&self.encode())
    }

    /// Decode strictly, rejecting truncation, trailing bytes, unknown kinds,
    /// invalid spool/facet tokens, and any non-canonical spelling.
    pub fn decode(bytes: &[u8]) -> Result<Self, ManifestBindingDecodeError> {
        let binding = Self::decode_inner(bytes)?;
        if binding.encode() != bytes {
            return Err(ManifestBindingDecodeError::NonCanonicalEncoding);
        }
        Ok(binding)
    }

    fn decode_inner(bytes: &[u8]) -> Result<Self, ManifestBindingDecodeError> {
        let mut reader = BindingReader { bytes, pos: 0 };

        if reader.take(4)? != MANIFEST_BINDING_MAGIC {
            return Err(ManifestBindingDecodeError::BadMagic);
        }
        let version = reader.u8()?;
        if version != MANIFEST_BINDING_VERSION {
            return Err(ManifestBindingDecodeError::UnsupportedVersion(version));
        }

        let spool_token = reader.short_string()?;
        let spool = SpoolId::parse(spool_token.clone())
            .map_err(|_| ManifestBindingDecodeError::InvalidSpoolId(spool_token))?;
        let facet_token = reader.short_string()?;
        let facet = ManifestFacet::parse(&facet_token)
            .map_err(|_| ManifestBindingDecodeError::InvalidFacet(facet_token))?;

        let owner_byte = reader.u8()?;
        let owner_kind = ManifestOwnerKind::from_byte(owner_byte)
            .ok_or(ManifestBindingDecodeError::UnknownOwnerKind(owner_byte))?;
        let owner_hash = ContentHash::from_bytes(reader.hash()?);
        let owner_decoded_size = reader.u64()?;
        let content_root = ContentHash::from_bytes(reader.hash()?);

        if reader.pos != bytes.len() {
            return Err(ManifestBindingDecodeError::TrailingBytes);
        }

        Ok(Self {
            spool,
            facet,
            owner_kind,
            owner_hash,
            owner_decoded_size,
            content_root,
        })
    }
}

struct BindingReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BindingReader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], ManifestBindingDecodeError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(ManifestBindingDecodeError::Truncated)?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(ManifestBindingDecodeError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ManifestBindingDecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ManifestBindingDecodeError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u64(&mut self) -> Result<u64, ManifestBindingDecodeError> {
        let bytes = self.take(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(arr))
    }

    fn hash(&mut self) -> Result<[u8; 32], ManifestBindingDecodeError> {
        let bytes = self.take(32)?;
        let mut arr = [0u8; 32];
        arr.copy_from_slice(bytes);
        Ok(arr)
    }

    fn short_string(&mut self) -> Result<String, ManifestBindingDecodeError> {
        let len = usize::from(self.u16()?);
        std::str::from_utf8(self.take(len)?)
            .map(str::to_string)
            .map_err(|_| ManifestBindingDecodeError::InvalidUtf8)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ManifestBindingDecodeError {
    #[error("binding does not start with the WPMB magic")]
    BadMagic,
    #[error("unsupported binding version {0}")]
    UnsupportedVersion(u8),
    #[error("unknown manifest owner kind {0}")]
    UnknownOwnerKind(u8),
    #[error("binding bytes are truncated")]
    Truncated,
    #[error("binding has trailing bytes after its declared content")]
    TrailingBytes,
    #[error("spool id or facet token is not valid UTF-8")]
    InvalidUtf8,
    #[error("invalid spool id: {0}")]
    InvalidSpoolId(String),
    #[error("invalid facet token: {0}")]
    InvalidFacet(String),
    #[error("binding bytes are a non-canonical spelling of their own content")]
    NonCanonicalEncoding,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> ManifestBinding {
        ManifestBinding {
            spool: SpoolId::parse("acme/api-v2").unwrap(),
            facet: ManifestFacet::Content,
            owner_kind: ManifestOwnerKind::State,
            owner_hash: ContentHash::from_bytes([7; 32]),
            owner_decoded_size: 4096,
            content_root: ContentHash::from_bytes([9; 32]),
        }
    }

    #[test]
    fn binding_round_trips_and_is_hash_stable() {
        let binding = binding();
        let encoded = binding.encode();
        let decoded = ManifestBinding::decode(&encoded).unwrap();
        assert_eq!(decoded, binding);
        assert_eq!(decoded.encode(), encoded);
        assert_eq!(decoded.address(), binding.address());
    }

    #[test]
    fn every_spool_carries_all_four_facets_and_they_bind_distinctly() {
        // weft #358: facets are uniform and independent, so the same owner and
        // content root under two facets must never collide.
        let addresses: Vec<ContentHash> = ManifestFacet::well_known()
            .into_iter()
            .map(|facet| ManifestBinding { facet, ..binding() }.address())
            .collect();
        let unique: std::collections::BTreeSet<_> = addresses.iter().collect();
        assert_eq!(unique.len(), 4, "facets must not collide in binding space");
    }

    #[test]
    fn a_named_token_normalizes_to_its_well_known_facet() {
        assert_eq!(
            ManifestFacet::parse("children").unwrap(),
            ManifestFacet::Children
        );
        // Normalization matters for hashing: two spellings must not produce
        // two addresses for one logical binding.
        let named = ManifestBinding {
            facet: ManifestFacet::parse("content").unwrap(),
            ..binding()
        };
        assert_eq!(named.address(), binding().address());
    }

    #[test]
    fn owner_identity_is_outside_the_content_root() {
        // A context-only state reuses its parent content root byte-for-byte;
        // only the owner half of the binding moves.
        let parent = binding();
        let child = ManifestBinding {
            owner_hash: ContentHash::from_bytes([11; 32]),
            ..parent.clone()
        };
        assert_eq!(child.content_root, parent.content_root);
        assert_ne!(child.address(), parent.address());
    }

    #[test]
    fn decode_rejects_each_corruption_class() {
        let encoded = binding().encode();

        let mut bad_magic = encoded.clone();
        bad_magic[0] = b'Q';
        assert_eq!(
            ManifestBinding::decode(&bad_magic).unwrap_err(),
            ManifestBindingDecodeError::BadMagic
        );

        let mut bad_version = encoded.clone();
        bad_version[4] = 4;
        assert_eq!(
            ManifestBinding::decode(&bad_version).unwrap_err(),
            ManifestBindingDecodeError::UnsupportedVersion(4)
        );

        let mut trailing = encoded.clone();
        trailing.push(0);
        assert_eq!(
            ManifestBinding::decode(&trailing).unwrap_err(),
            ManifestBindingDecodeError::TrailingBytes
        );

        assert_eq!(
            ManifestBinding::decode(&encoded[..encoded.len() - 1]).unwrap_err(),
            ManifestBindingDecodeError::Truncated
        );

        // The owner-kind byte sits right after the two length-prefixed tokens.
        let owner_kind_at = 4 + 1 + 2 + "acme/api-v2".len() + 2 + "content".len();
        let mut bad_owner = encoded;
        bad_owner[owner_kind_at] = 5;
        assert_eq!(
            ManifestBinding::decode(&bad_owner).unwrap_err(),
            ManifestBindingDecodeError::UnknownOwnerKind(5)
        );
    }

    #[test]
    fn invalid_facet_tokens_are_rejected() {
        for token in ["", "Content", "-bad", "bad-", "with space"] {
            assert!(
                ManifestFacet::parse(token).is_err(),
                "accepted facet token {token:?}"
            );
        }
    }
}
