// SPDX-License-Identifier: Apache-2.0
//! State-signature helpers that depend on crypto implementations.

use objects::object::{ContentHash, StateSignature};

use crate::{Signer, SignerError, verify_state_signature};

/// Error type for state signature operations.
#[derive(Debug, thiserror::Error)]
pub enum StateSignatureError {
    #[error("unsupported signature algorithm: {0}")]
    UnsupportedAlgorithm(String),
    #[error("invalid public key: {0}")]
    InvalidPublicKey(String),
    #[error("invalid signature: {0}")]
    InvalidSignature(String),
    #[error("hex decode error: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("signer error: {0}")]
    Signer(#[from] SignerError),
}

pub fn state_signature_from_signer(
    hash: &ContentHash,
    signer: &dyn Signer,
) -> Result<StateSignature, StateSignatureError> {
    let signature = signer.sign(hash.as_bytes())?;

    Ok(StateSignature {
        algorithm: signer.algorithm().to_string(),
        public_key: hex::encode(signer.public_key()),
        signature: hex::encode(&signature),
    })
}

pub fn verify_state_signature_bytes(
    signature: &StateSignature,
    hash: &ContentHash,
) -> Result<(), StateSignatureError> {
    let public_key = hex::decode(&signature.public_key)?;
    let signature_bytes = hex::decode(&signature.signature)?;

    verify_state_signature(hash, &signature.algorithm, &public_key, &signature_bytes)
        .map_err(StateSignatureError::from)
}

pub fn public_key_bytes(signature: &StateSignature) -> Result<Vec<u8>, StateSignatureError> {
    hex::decode(&signature.public_key).map_err(StateSignatureError::from)
}

pub fn signature_bytes(signature: &StateSignature) -> Result<Vec<u8>, StateSignatureError> {
    hex::decode(&signature.signature).map_err(StateSignatureError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Ed25519Signer;

    fn make_test_hash() -> ContentHash {
        ContentHash::from_bytes([1u8; 32])
    }

    #[test]
    fn test_sign_verify_ed25519() {
        let signer = Ed25519Signer::generate().expect("generate key");
        let hash = make_test_hash();

        let sig = state_signature_from_signer(&hash, &signer).expect("sign state");
        assert_eq!(sig.algorithm(), "ed25519");

        verify_state_signature_bytes(&sig, &hash).expect("verify should not error");
    }
}
