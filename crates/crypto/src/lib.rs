// SPDX-License-Identifier: Apache-2.0
//! Cryptographic signing for Heddle states.

mod ed25519;
mod error;
mod p256;
mod pem_loader;
mod rsa;
mod state_signature;
mod state_signing;

use std::path::Path;

pub use ed25519::Ed25519Signer;
pub use error::SignerError;
use objects::object::ContentHash;
pub use objects::object::SignatureStatus;
pub use p256::P256Signer;
pub use pem_loader::{PemKind, classify_pem};
pub use rsa::RsaSigner;
pub use state_signature::{
    StateSignatureError, public_key_bytes, signature_bytes, state_signature_from_signer,
    verify_state_signature_bytes,
};
pub use state_signing::StateSigningExt;

/// Trait for cryptographic signers.
pub trait Signer: Send + Sync {
    fn algorithm(&self) -> &'static str;
    fn public_key(&self) -> Vec<u8>;
    fn sign(&self, data: &[u8]) -> Result<Vec<u8>, SignerError>;
    fn verify(&self, data: &[u8], signature: &[u8]) -> Result<(), SignerError>;
}

/// Load a signer from a key file. When `algorithm` is `None`, the PEM
/// header (or raw-seed shape) selects the backend via
/// [`pem_loader::load_signer_from_pem`].
pub fn load_signer(path: &Path, algorithm: Option<&str>) -> Result<Box<dyn Signer>, SignerError> {
    let key_data = std::fs::read(path)?;
    let pem_content = String::from_utf8_lossy(&key_data);

    if let Some(algo) = algorithm {
        return match algo.to_lowercase().as_str() {
            "ed25519" => {
                Ed25519Signer::from_pem(&pem_content).map(|s| Box::new(s) as Box<dyn Signer>)
            }
            "rsa" => RsaSigner::from_pem(&pem_content).map(|s| Box::new(s) as Box<dyn Signer>),
            "p256" | "ecdsa-p256" => {
                P256Signer::from_pem(&pem_content).map(|s| Box::new(s) as Box<dyn Signer>)
            }
            _ => Err(SignerError::UnsupportedAlgorithm(algo.to_string())),
        };
    }

    pem_loader::load_signer_from_pem(&pem_content)
}

/// Verify a state's signature.
pub fn verify_state_signature(
    content_hash: &ContentHash,
    algorithm: &str,
    public_key: &[u8],
    signature: &[u8],
) -> Result<(), SignerError> {
    verify_payload_signature(content_hash.as_bytes(), algorithm, public_key, signature)
}

/// Verify a detached signature over an arbitrary payload. Used by
/// non-state-signature flows (e.g. `ReviewSignature`) that already have a
/// canonical byte payload built upstream.
pub fn verify_payload_signature(
    payload: &[u8],
    algorithm: &str,
    public_key: &[u8],
    signature: &[u8],
) -> Result<(), SignerError> {
    match algorithm.to_lowercase().as_str() {
        "ed25519" => Ed25519Signer::verify_with_public_key(payload, public_key, signature),
        "rsa" => RsaSigner::verify_with_public_key(payload, public_key, signature),
        "p256" | "ecdsa-p256" => P256Signer::verify_with_public_key(payload, public_key, signature),
        _ => Err(SignerError::UnsupportedAlgorithm(algorithm.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_ed25519_sign_verify_roundtrip() {
        let signer = Ed25519Signer::generate().expect("generate key");
        let data = b"test data for signing";

        let signature = signer.sign(data).expect("sign data");
        signer.verify(data, &signature).expect("verify signature");
    }

    #[test]
    fn test_ed25519_sign_verify_invalid_signature_fails_explicitly() {
        let signer = Ed25519Signer::generate().expect("generate key");
        let data = b"test data for signing";

        let signature = signer.sign(data).expect("sign data");
        let error = signer
            .verify(b"wrong data", &signature)
            .expect_err("verify should fail");

        assert!(matches!(error, SignerError::VerificationFailed));
    }

    #[test]
    fn test_load_signer_ed25519() {
        let temp = TempDir::new().expect("create temp dir");
        let key_path = temp.path().join("test_ed25519.pem");

        let signer = Ed25519Signer::generate().expect("generate key");
        let pem = signer.to_pem().expect("export to PEM");
        std::fs::write(&key_path, &pem).expect("write key file");

        let loaded = load_signer(&key_path, Some("ed25519")).expect("load signer");
        assert_eq!(loaded.algorithm(), "ed25519");
        assert_eq!(loaded.public_key(), signer.public_key());
    }
}