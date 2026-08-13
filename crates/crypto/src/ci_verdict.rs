// SPDX-License-Identifier: Apache-2.0
//! Signed, provenance-bound CI verdicts.

use objects::object::{ChangeId, ContentHash};
use serde::{Deserialize, Serialize};

use crate::{Signer, SignerError, verify_payload_signature};

/// NUL-terminated domain separator for CI verdict signatures.
pub const CI_VERDICT_DOMAIN: &[u8; 21] = b"heddle-ci-verdict-v1\0";

/// Current serialized [`SignedVerdict`] format version.
pub const SIGNED_VERDICT_FORMAT_VERSION: u8 = 1;

const SIGNING_PAYLOAD_LEN: usize = CI_VERDICT_DOMAIN.len() + 32 + 16 + 32;

/// A CI verdict signature bound to immutable verdict content and source state.
///
/// The signature covers [`ci_verdict_signing_payload`]. The embedded public key
/// proves integrity; callers must separately resolve that key against their
/// trusted `ci-runner` key set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedVerdict {
    /// Serialized verdict format. Only version 1 is accepted.
    pub format_version: u8,
    /// Hash of the complete CI verdict content.
    pub content_hash: ContentHash,
    /// Rewrite-stable change identifier for the checked state.
    pub change_id: ChangeId,
    /// Root tree digest for the exact source tree that was checked.
    pub tree_digest: ContentHash,
    /// Signature algorithm understood by Heddle's shared signing spine.
    pub algorithm: String,
    /// Hex-encoded public key bytes.
    pub public_key: String,
    /// Hex-encoded signature bytes.
    pub signature: String,
}

impl SignedVerdict {
    /// Verify the signature over every binding in this verdict.
    ///
    /// Trust in the embedded key is deliberately outside this primitive and is
    /// established by the repository's `ci-runner` trust-set resolver.
    pub fn verify(&self) -> Result<(), SignedVerdictError> {
        if self.format_version != SIGNED_VERDICT_FORMAT_VERSION {
            return Err(SignedVerdictError::UnsupportedVersion(self.format_version));
        }

        let public_key =
            hex::decode(&self.public_key).map_err(SignedVerdictError::InvalidPublicKeyEncoding)?;
        let signature =
            hex::decode(&self.signature).map_err(SignedVerdictError::InvalidSignatureEncoding)?;
        let payload =
            ci_verdict_signing_payload(&self.content_hash, &self.change_id, &self.tree_digest);

        verify_payload_signature(&payload, &self.algorithm, &public_key, &signature)
            .map_err(SignedVerdictError::from)
    }
}

/// Build the canonical bytes signed by a [`SignedVerdict`].
///
/// Layout: NUL-terminated domain tag, 32-byte verdict content hash, 16-byte
/// change id, then 32-byte root tree digest. All fields are fixed-width and
/// retain their native byte order.
pub fn ci_verdict_signing_payload(
    content_hash: &ContentHash,
    change_id: &ChangeId,
    tree_digest: &ContentHash,
) -> [u8; SIGNING_PAYLOAD_LEN] {
    let mut payload = [0; SIGNING_PAYLOAD_LEN];
    let content_hash_start = CI_VERDICT_DOMAIN.len();
    let change_id_start = content_hash_start + content_hash.as_bytes().len();
    let tree_digest_start = change_id_start + change_id.as_bytes().len();

    payload[..content_hash_start].copy_from_slice(CI_VERDICT_DOMAIN);
    payload[content_hash_start..change_id_start].copy_from_slice(content_hash.as_bytes());
    payload[change_id_start..tree_digest_start].copy_from_slice(change_id.as_bytes());
    payload[tree_digest_start..].copy_from_slice(tree_digest.as_bytes());
    payload
}

/// Sign the canonical CI verdict payload with Heddle's shared [`Signer`].
pub fn signed_verdict_from_signer(
    content_hash: &ContentHash,
    change_id: &ChangeId,
    tree_digest: &ContentHash,
    signer: &dyn Signer,
) -> Result<SignedVerdict, SignedVerdictError> {
    let payload = ci_verdict_signing_payload(content_hash, change_id, tree_digest);
    let signature = signer.sign(&payload)?;

    Ok(SignedVerdict {
        format_version: SIGNED_VERDICT_FORMAT_VERSION,
        content_hash: *content_hash,
        change_id: *change_id,
        tree_digest: *tree_digest,
        algorithm: signer.algorithm().to_string(),
        public_key: hex::encode(signer.public_key()),
        signature: hex::encode(signature),
    })
}

/// Errors returned while creating or verifying a signed CI verdict.
#[derive(Debug, thiserror::Error)]
pub enum SignedVerdictError {
    /// The serialized verdict format is not supported by this reader.
    #[error("unsupported signed verdict format version {0}")]
    UnsupportedVersion(u8),
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

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::Ed25519Signer;

    fn bindings() -> (ContentHash, ChangeId, ContentHash) {
        (
            ContentHash::from_bytes([1; 32]),
            ChangeId::from_bytes([2; 16]),
            ContentHash::from_bytes([3; 32]),
        )
    }

    fn signed_verdict() -> SignedVerdict {
        let signer = Ed25519Signer::from_seed(&[7; 32]).expect("create signer");
        let (content_hash, change_id, tree_digest) = bindings();
        signed_verdict_from_signer(&content_hash, &change_id, &tree_digest, &signer)
            .expect("sign verdict")
    }

    #[test]
    fn canonical_payload_pins_domain_and_binding_order() {
        let (content_hash, change_id, tree_digest) = bindings();
        let payload = ci_verdict_signing_payload(&content_hash, &change_id, &tree_digest);

        assert_eq!(&payload[..21], b"heddle-ci-verdict-v1\0");
        assert_eq!(&payload[21..53], &[1; 32]);
        assert_eq!(&payload[53..69], &[2; 16]);
        assert_eq!(&payload[69..101], &[3; 32]);
    }

    #[test]
    fn signed_verdict_round_trips_losslessly_and_verifies() {
        let verdict = signed_verdict();
        let encoded = serde_json::to_vec(&verdict).expect("encode signed verdict");
        let decoded: SignedVerdict =
            serde_json::from_slice(&encoded).expect("decode signed verdict");

        assert_eq!(decoded, verdict);
        decoded.verify().expect("verify decoded verdict");
    }

    #[test]
    fn verify_rejects_each_tampered_binding() {
        let verdict = signed_verdict();
        let mut tampered_content = verdict.clone();
        tampered_content.content_hash = ContentHash::from_bytes([9; 32]);
        let mut tampered_change = verdict.clone();
        tampered_change.change_id = ChangeId::from_bytes([9; 16]);
        let mut tampered_tree = verdict;
        tampered_tree.tree_digest = ContentHash::from_bytes([9; 32]);

        for tampered in [tampered_content, tampered_change, tampered_tree] {
            assert!(matches!(
                tampered.verify(),
                Err(SignedVerdictError::Signer(SignerError::VerificationFailed))
            ));
        }
    }

    #[test]
    fn verify_rejects_wrong_key_and_tampered_signature() {
        let verdict = signed_verdict();
        let wrong_signer = Ed25519Signer::from_seed(&[8; 32]).expect("create wrong signer");
        let mut wrong_key = verdict.clone();
        wrong_key.public_key = hex::encode(wrong_signer.public_key());
        let mut tampered_signature = verdict;
        let mut signature = hex::decode(&tampered_signature.signature).expect("decode signature");
        signature[0] ^= 1;
        tampered_signature.signature = hex::encode(signature);

        for tampered in [wrong_key, tampered_signature] {
            assert!(matches!(
                tampered.verify(),
                Err(SignedVerdictError::Signer(SignerError::VerificationFailed))
            ));
        }
    }

    #[test]
    fn verify_rejects_untagged_signature_and_unknown_format() {
        let signer = Ed25519Signer::from_seed(&[7; 32]).expect("create signer");
        let (content_hash, change_id, tree_digest) = bindings();
        let mut untagged_payload = Vec::with_capacity(80);
        untagged_payload.extend_from_slice(content_hash.as_bytes());
        untagged_payload.extend_from_slice(change_id.as_bytes());
        untagged_payload.extend_from_slice(tree_digest.as_bytes());
        let mut verdict = SignedVerdict {
            format_version: SIGNED_VERDICT_FORMAT_VERSION,
            content_hash,
            change_id,
            tree_digest,
            algorithm: signer.algorithm().to_string(),
            public_key: hex::encode(signer.public_key()),
            signature: hex::encode(
                signer
                    .sign(&untagged_payload)
                    .expect("sign untagged payload"),
            ),
        };

        assert!(matches!(
            verdict.verify(),
            Err(SignedVerdictError::Signer(SignerError::VerificationFailed))
        ));
        verdict.format_version = 2;
        assert!(matches!(
            verdict.verify(),
            Err(SignedVerdictError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn deserialize_rejects_every_missing_binding() {
        let encoded = serde_json::to_value(signed_verdict()).expect("encode signed verdict");

        for field in ["content_hash", "change_id", "tree_digest"] {
            let mut incomplete = encoded.clone();
            let Value::Object(ref mut object) = incomplete else {
                panic!("signed verdict must encode as an object");
            };
            object.remove(field);

            serde_json::from_value::<SignedVerdict>(incomplete)
                .expect_err("missing binding must fail closed");
        }
    }
}
