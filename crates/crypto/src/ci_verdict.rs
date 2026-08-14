// SPDX-License-Identifier: Apache-2.0
//! Signed, provenance-bound CI verdicts.
//!
//! A verdict signature binds the canonical [`CiVerdictBody`] content hash, the
//! rewrite-stable change id, the exact evaluated tree, the signer kind, and the
//! signing time. Trust in the embedded key and timestamp freshness are policy
//! decisions for the caller; this module proves integrity and authenticity.

mod body;
mod body_details;

pub use body::{
    Basis, BasisKind, CI_VERDICT_BODY_SCHEMA_VERSION, CheckClass, CheckDescriptor, CiVerdictBody,
    StateRef,
};
pub use body_details::{
    Conclusion, Execution, FailureClass, FailureDetail, LogRef, Outcome, Repro,
};
use chrono::DateTime;
use objects::object::{ChangeId, ContentHash};
use serde::{Deserialize, Serialize};

use crate::{Signer, SignerError, verify_payload_signature};

/// NUL-terminated domain separator for the v2 CI-verdict signing scheme.
pub const CI_VERDICT_DOMAIN: &[u8; 21] = b"heddle-ci-verdict-v2\0";

/// Current serialized [`SignedVerdict`] format version.
pub const SIGNED_VERDICT_FORMAT_VERSION: u8 = 2;

const FIXED_PAYLOAD_LEN: usize = CI_VERDICT_DOMAIN.len() + 32 + 16 + 32;

/// What kind of principal signed a verdict.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignerKind {
    /// A trusted automation principal. Trust-set membership remains caller policy.
    #[default]
    ServiceAccount,
    /// A human device key. Device verdicts are always advisory-only.
    Device,
}

impl SignerKind {
    /// Stable token used in both JSON and the signature preimage.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ServiceAccount => "service_account",
            Self::Device => "device",
        }
    }

    /// Whether this signer kind is forbidden from satisfying a required gate.
    #[must_use]
    pub const fn is_advisory_only(self) -> bool {
        matches!(self, Self::Device)
    }
}

/// A rich CI verdict body plus its provenance-bound signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVerdict {
    /// Serialized envelope format. Only v2 is accepted.
    pub format_version: u8,
    /// Complete conclusion-bearing content covered by [`Self::content_hash`].
    pub body: CiVerdictBody,
    /// BLAKE3 hash of the body's canonical JSON bytes.
    pub content_hash: ContentHash,
    /// Rewrite-stable change identifier for the checked state.
    pub change_id: ChangeId,
    /// Root tree digest for the exact tree that was checked.
    pub tree_digest: ContentHash,
    /// What kind of principal signed the verdict.
    pub signer_kind: SignerKind,
    /// RFC3339 timestamp, signed to prevent freshness-presentation rewrites.
    pub signed_at: String,
    /// Signature algorithm understood by Heddle's shared signing spine.
    pub algorithm: String,
    /// Hex-encoded public key bytes.
    pub public_key: String,
    /// Hex-encoded signature bytes.
    pub signature: String,
}

impl SignedVerdict {
    /// Verify the body digest and signature over every provenance binding.
    ///
    /// Trust-set membership, freshness windows, and required-gate eligibility
    /// remain caller policy. In particular, [`SignerKind::Device`] is advisory-only.
    pub fn verify(&self) -> Result<(), SignedVerdictError> {
        validate_versions(self.format_version, self.body.schema_version)?;
        validate_signed_at(&self.signed_at)?;

        let recomputed = self.body.content_hash();
        if recomputed != self.content_hash {
            return Err(SignedVerdictError::BodyDigestMismatch {
                signed: self.content_hash,
                recomputed,
            });
        }

        let public_key =
            hex::decode(&self.public_key).map_err(SignedVerdictError::InvalidPublicKeyEncoding)?;
        let signature =
            hex::decode(&self.signature).map_err(SignedVerdictError::InvalidSignatureEncoding)?;
        let payload = ci_verdict_signing_payload(
            &self.content_hash,
            &self.change_id,
            &self.tree_digest,
            self.signer_kind,
            &self.signed_at,
        );

        verify_payload_signature(&payload, &self.algorithm, &public_key, &signature)
            .map_err(SignedVerdictError::from)
    }

    /// Whether policy must treat this verdict as advisory-only.
    #[must_use]
    pub const fn is_advisory_only(&self) -> bool {
        self.signer_kind.is_advisory_only()
    }
}

/// Build the canonical bytes signed by a [`SignedVerdict`].
///
/// Layout: `v2-tag || content-hash || change-id || tree-digest || signer-kind
/// || NUL || signed-at || NUL`. The first four fields have fixed widths; the two
/// trailing UTF-8 fields are framed by their fixed enum vocabulary/final NUL.
#[must_use]
pub fn ci_verdict_signing_payload(
    content_hash: &ContentHash,
    change_id: &ChangeId,
    tree_digest: &ContentHash,
    signer_kind: SignerKind,
    signed_at: &str,
) -> Vec<u8> {
    let mut payload =
        Vec::with_capacity(FIXED_PAYLOAD_LEN + signer_kind.as_str().len() + signed_at.len() + 2);
    payload.extend_from_slice(CI_VERDICT_DOMAIN);
    payload.extend_from_slice(content_hash.as_bytes());
    payload.extend_from_slice(change_id.as_bytes());
    payload.extend_from_slice(tree_digest.as_bytes());
    payload.extend_from_slice(signer_kind.as_str().as_bytes());
    payload.push(0);
    payload.extend_from_slice(signed_at.as_bytes());
    payload.push(0);
    payload
}

/// Sign a rich CI verdict with Heddle's shared [`Signer`] spine.
pub fn signed_verdict_from_signer(
    body: CiVerdictBody,
    change_id: &ChangeId,
    tree_digest: &ContentHash,
    signer_kind: SignerKind,
    signed_at: String,
    signer: &dyn Signer,
) -> Result<SignedVerdict, SignedVerdictError> {
    validate_versions(SIGNED_VERDICT_FORMAT_VERSION, body.schema_version)?;
    validate_signed_at(&signed_at)?;

    let content_hash = body.content_hash();
    let payload = ci_verdict_signing_payload(
        &content_hash,
        change_id,
        tree_digest,
        signer_kind,
        &signed_at,
    );
    let signature = signer.sign(&payload)?;

    Ok(SignedVerdict {
        format_version: SIGNED_VERDICT_FORMAT_VERSION,
        body,
        content_hash,
        change_id: *change_id,
        tree_digest: *tree_digest,
        signer_kind,
        signed_at,
        algorithm: signer.algorithm().to_string(),
        public_key: hex::encode(signer.public_key()),
        signature: hex::encode(signature),
    })
}

fn validate_versions(format_version: u8, schema_version: u32) -> Result<(), SignedVerdictError> {
    if format_version != SIGNED_VERDICT_FORMAT_VERSION {
        return Err(SignedVerdictError::UnsupportedFormatVersion {
            found: format_version,
            supported: SIGNED_VERDICT_FORMAT_VERSION,
        });
    }
    if schema_version != CI_VERDICT_BODY_SCHEMA_VERSION {
        return Err(SignedVerdictError::UnsupportedSchemaVersion {
            found: schema_version,
            supported: CI_VERDICT_BODY_SCHEMA_VERSION,
        });
    }
    Ok(())
}

fn validate_signed_at(signed_at: &str) -> Result<(), SignedVerdictError> {
    DateTime::parse_from_rfc3339(signed_at)
        .map(|_| ())
        .map_err(|error| SignedVerdictError::InvalidSignedAt(error.to_string()))
}

/// Errors returned while creating or verifying a signed CI verdict.
#[derive(Debug, thiserror::Error)]
pub enum SignedVerdictError {
    /// The serialized envelope format is not supported.
    #[error("unsupported signed verdict format version {found}; expected {supported}")]
    UnsupportedFormatVersion {
        /// Version found in the envelope.
        found: u8,
        /// Only version accepted by this implementation.
        supported: u8,
    },
    /// The embedded body schema cannot be verified by this implementation.
    #[error("unsupported CI verdict body schema version {found}; expected {supported}")]
    UnsupportedSchemaVersion {
        /// Version found in the body.
        found: u32,
        /// Only version accepted by this implementation.
        supported: u32,
    },
    /// The embedded body no longer hashes to the signed content hash.
    #[error("CI verdict body digest mismatch: signed {signed}, recomputed {recomputed}")]
    BodyDigestMismatch {
        /// Digest carried by the signed envelope.
        signed: ContentHash,
        /// Digest recomputed from the embedded body.
        recomputed: ContentHash,
    },
    /// The signed timestamp is not RFC3339.
    #[error("CI verdict signed_at is not RFC3339: {0}")]
    InvalidSignedAt(String),
    /// The embedded public key is not valid hexadecimal.
    #[error("signed verdict public key is not hexadecimal: {0}")]
    InvalidPublicKeyEncoding(hex::FromHexError),
    /// The embedded signature is not valid hexadecimal.
    #[error("signed verdict signature is not hexadecimal: {0}")]
    InvalidSignatureEncoding(hex::FromHexError),
    /// The shared signing backend rejected the operation.
    #[error("signed verdict cryptographic error: {0}")]
    Signer(#[from] SignerError),
}
