// SPDX-License-Identifier: Apache-2.0
//! Review signatures: who reviewed a state, in what role, with what scope.
//!
//! Distinct from [`StateSignature`](crate::object::StateSignature). That signature
//! authenticates *authorship* — "this principal captured this state". A
//! [`ReviewSignature`] authenticates *review* — "this actor read / previewed /
//! co-reviewed this state". A state may carry many review signatures across its
//! lifetime; the kind enum is extensible to admit future kinds without
//! re-encoding old data.
//!
//! The cryptographic primitives live in `crates/crypto/`. This module owns the
//! payload format the signature is computed over, so verifiers in any language
//! can reproduce it.

use serde::{Deserialize, Serialize};

use crate::object::{hash::ChangeId, state_attribution::Principal};

/// Stable byte prefix the signing payload begins with. Bumping this versions
/// the payload format itself; old signatures with the old prefix continue to
/// verify exactly as they did when written.
pub const SIGNING_PAYLOAD_VERSION_TAG: &[u8] = b"hd-rev-sig-v1\x00";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSignaturesBlob {
    pub format_version: u8,
    pub signatures: Vec<ReviewSignature>,
}

versioned_msgpack_blob! {
    blob: ReviewSignaturesBlob,
    item: ReviewSignature,
    field: signatures,
    error: ReviewSignatureError,
    codec_err: Encoding,
    version: 1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSignature {
    pub actor: Principal,
    pub kind: ReviewKind,
    pub scope: ReviewScope,
    /// Reserved for future review kinds (e.g. send-it / fast-track) that need
    /// a written rationale. Read/preview/co-review leave this `None`.
    #[serde(default)]
    pub justification: Option<String>,
    /// Unix epoch seconds.
    pub signed_at: i64,
    pub algorithm: String,
    pub public_key: String,
    /// Hex-encoded signature bytes — same encoding convention as
    /// [`StateSignature::signature`](crate::object::StateSignature).
    pub signature: String,
}

impl ReviewSignature {
    pub fn validate(&self) -> Result<(), ReviewSignatureError> {
        if self.algorithm.is_empty() {
            return Err(ReviewSignatureError::EmptyAlgorithm);
        }
        if self.public_key.is_empty() {
            return Err(ReviewSignatureError::EmptyPublicKey);
        }
        if self.signature.is_empty() {
            return Err(ReviewSignatureError::EmptySignature);
        }
        self.scope.validate()?;
        Ok(())
    }
}

/// Reviewer roles. New variants append at the tail; the wire format stays
/// backwards compatible because `serde` emits the snake-case discriminant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewKind {
    /// Reviewer claims to have read the change.
    Read,
    /// Agent ran a preview pass — listed signals, optionally annotated.
    AgentPreview,
    /// Agent acted as co-reviewer; comments and signatures recorded.
    AgentCoReview,
}

impl ReviewKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::AgentPreview => "agent_preview",
            Self::AgentCoReview => "agent_co_review",
        }
    }
}

/// Reviewer signed off on the whole change, or on a specific list of symbols.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewScope {
    WholeChange,
    Symbols(Vec<SymbolAnchor>),
}

impl ReviewScope {
    pub fn validate(&self) -> Result<(), ReviewSignatureError> {
        match self {
            Self::WholeChange => Ok(()),
            Self::Symbols(symbols) => {
                if symbols.is_empty() {
                    return Err(ReviewSignatureError::EmptySymbolScope);
                }
                for s in symbols {
                    s.validate()?;
                }
                Ok(())
            }
        }
    }
}

/// Durable symbol-level anchor: a file path plus a symbol name. No line range
/// — line numbers move under reformatting; symbols do not.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolAnchor {
    pub file: String,
    pub symbol: String,
}

impl SymbolAnchor {
    pub fn new(file: impl Into<String>, symbol: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            symbol: symbol.into(),
        }
    }

    pub fn validate(&self) -> Result<(), ReviewSignatureError> {
        if self.file.is_empty() {
            return Err(ReviewSignatureError::EmptyAnchorFile);
        }
        if self.symbol.is_empty() {
            return Err(ReviewSignatureError::EmptyAnchorSymbol);
        }
        Ok(())
    }
}

/// Build the deterministic byte payload that a [`ReviewSignature`] is computed
/// over. Re-implementing this in another language (TypeScript, Python) must
/// produce byte-identical output for verification to round-trip.
///
/// Layout: version tag, then NUL-terminated string fields, then fixed-width
/// integers. Uses NUL byte as a field separator, which is safe because
/// `change_id` is hex and other fields are utf-8 strings without embedded NULs.
pub fn signing_payload(
    state_change_id: ChangeId,
    kind: ReviewKind,
    scope: &ReviewScope,
    signed_at: i64,
    justification: Option<&str>,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(SIGNING_PAYLOAD_VERSION_TAG.len() + 256);
    buf.extend_from_slice(SIGNING_PAYLOAD_VERSION_TAG);
    buf.extend_from_slice(state_change_id.to_string_full().as_bytes());
    buf.push(0);
    buf.extend_from_slice(kind.as_str().as_bytes());
    buf.push(0);
    match scope {
        ReviewScope::WholeChange => {
            buf.extend_from_slice(b"whole_change");
            buf.push(0);
        }
        ReviewScope::Symbols(symbols) => {
            buf.extend_from_slice(b"symbols");
            buf.push(0);
            buf.extend_from_slice(&(symbols.len() as u32).to_le_bytes());
            for s in symbols {
                buf.extend_from_slice(s.file.as_bytes());
                buf.push(0);
                buf.extend_from_slice(s.symbol.as_bytes());
                buf.push(0);
            }
        }
    }
    buf.extend_from_slice(&signed_at.to_le_bytes());
    if let Some(j) = justification {
        buf.push(1);
        buf.extend_from_slice(j.as_bytes());
        buf.push(0);
    } else {
        buf.push(0);
    }
    buf
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewSignatureError {
    #[error("unsupported review signatures blob version {0}")]
    UnsupportedVersion(u8),
    #[error("review signature must declare a non-empty algorithm")]
    EmptyAlgorithm,
    #[error("review signature must include a public key")]
    EmptyPublicKey,
    #[error("review signature must include a signature value")]
    EmptySignature,
    #[error("symbol-scope review must include at least one symbol")]
    EmptySymbolScope,
    #[error("symbol anchor must reference a non-empty file")]
    EmptyAnchorFile,
    #[error("symbol anchor must reference a non-empty symbol")]
    EmptyAnchorSymbol,
    #[error("review signatures blob encoding error: {0}")]
    Encoding(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_principal() -> Principal {
        Principal::new("Alice", "alice@example.com")
    }

    fn sample_signature() -> ReviewSignature {
        ReviewSignature {
            actor: sample_principal(),
            kind: ReviewKind::Read,
            scope: ReviewScope::WholeChange,
            justification: None,
            signed_at: 1_700_000_000,
            algorithm: "ed25519".into(),
            public_key: "deadbeef".into(),
            signature: "abad1dea".into(),
        }
    }

    #[test]
    fn read_signature_validates() {
        sample_signature().validate().unwrap();
    }

    #[test]
    fn empty_symbol_scope_rejected() {
        let mut sig = sample_signature();
        sig.scope = ReviewScope::Symbols(vec![]);
        assert!(matches!(
            sig.validate(),
            Err(ReviewSignatureError::EmptySymbolScope)
        ));
    }

    #[test]
    fn unsigned_blob_validates() {
        let blob = ReviewSignaturesBlob::new(vec![]);
        blob.validate().unwrap();
    }

    #[test]
    fn blob_roundtrip() {
        let blob = ReviewSignaturesBlob::new(vec![sample_signature()]);
        let bytes = blob.encode().unwrap();
        let decoded = ReviewSignaturesBlob::decode(&bytes).unwrap();
        assert_eq!(blob, decoded);
    }

    #[test]
    fn signing_payload_distinguishes_scope() {
        let id = ChangeId::from_bytes([1; 16]);
        let whole = signing_payload(id, ReviewKind::Read, &ReviewScope::WholeChange, 0, None);
        let one_symbol = signing_payload(
            id,
            ReviewKind::Read,
            &ReviewScope::Symbols(vec![SymbolAnchor::new("a.rs", "foo")]),
            0,
            None,
        );
        assert_ne!(whole, one_symbol);
    }

    #[test]
    fn signing_payload_starts_with_version_tag() {
        let id = ChangeId::from_bytes([1; 16]);
        let payload = signing_payload(id, ReviewKind::Read, &ReviewScope::WholeChange, 0, None);
        assert!(payload.starts_with(SIGNING_PAYLOAD_VERSION_TAG));
    }

    #[test]
    fn signing_payload_distinguishes_kind() {
        let id = ChangeId::from_bytes([1; 16]);
        let read = signing_payload(id, ReviewKind::Read, &ReviewScope::WholeChange, 0, None);
        let preview = signing_payload(
            id,
            ReviewKind::AgentPreview,
            &ReviewScope::WholeChange,
            0,
            None,
        );
        assert_ne!(read, preview);
    }
}
