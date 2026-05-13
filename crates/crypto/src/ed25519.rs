// SPDX-License-Identifier: Apache-2.0
//! Ed25519 signature implementation.

use ed25519_dalek::{Signature, Signer as EdSigner, SigningKey, Verifier, VerifyingKey};
use rsa::rand_core::OsRng;

use crate::{Signer, SignerError};

/// Ed25519 signer.
pub struct Ed25519Signer {
    signing_key: SigningKey,
}

impl Ed25519Signer {
    pub fn generate() -> Result<Self, SignerError> {
        Ok(Self {
            signing_key: SigningKey::generate(&mut OsRng),
        })
    }

    pub fn from_pem(pem: &str) -> Result<Self, SignerError> {
        if pem.contains("-----BEGIN PRIVATE KEY-----") {
            return Self::from_pkcs8_pem(pem);
        }
        if pem.contains("-----BEGIN OPENSSH PRIVATE KEY-----") {
            return Self::from_openssh_pem(pem);
        }

        let trimmed = pem.trim();
        if let Ok(bytes) = hex::decode(trimmed)
            && bytes.len() == 32
        {
            return Self::from_seed(&bytes);
        }

        if let Ok(bytes) = base64_decode(trimmed) {
            if bytes.len() == 32 {
                return Self::from_seed(&bytes);
            }
            if bytes.len() == 64 {
                return Self::from_seed(&bytes[..32]);
            }
        }

        Err(SignerError::UnknownKeyFormat)
    }

    pub fn from_seed(seed: &[u8]) -> Result<Self, SignerError> {
        let seed_bytes: [u8; 32] = seed
            .try_into()
            .map_err(|_| SignerError::InvalidKey("seed must be 32 bytes".to_string()))?;
        let signing_key = SigningKey::from_bytes(&seed_bytes);
        Ok(Self { signing_key })
    }

    fn from_pkcs8_pem(pem: &str) -> Result<Self, SignerError> {
        use pkcs8::DecodePrivateKey;

        let signing_key = SigningKey::from_pkcs8_pem(pem)?;
        Ok(Self { signing_key })
    }

    fn from_openssh_pem(_pem: &str) -> Result<Self, SignerError> {
        Err(SignerError::Pem(
            "OpenSSH Ed25519 private keys are not yet supported".to_string(),
        ))
    }

    pub fn to_pem(&self) -> Result<String, SignerError> {
        use pkcs8::EncodePrivateKey;

        self.signing_key
            .to_pkcs8_pem(pkcs8::LineEnding::LF)
            .map(|pem| pem.to_string())
            .map_err(|e| SignerError::Pkcs8(e.to_string()))
    }

    pub fn verify_with_public_key(
        data: &[u8],
        public_key: &[u8],
        signature: &[u8],
    ) -> Result<(), SignerError> {
        let verifying_key = VerifyingKey::from_bytes(public_key.try_into().map_err(|_| {
            SignerError::InvalidPublicKey("public key must be 32 bytes".to_string())
        })?)?;
        let signature = Signature::from_slice(signature)
            .map_err(|_| SignerError::InvalidSignature("signature must be 64 bytes".to_string()))?;
        verifying_key
            .verify(data, &signature)
            .map_err(|_| SignerError::VerificationFailed)
    }
}

impl Signer for Ed25519Signer {
    fn algorithm(&self) -> &'static str {
        "ed25519"
    }

    fn public_key(&self) -> Vec<u8> {
        self.signing_key.verifying_key().to_bytes().to_vec()
    }

    fn sign(&self, data: &[u8]) -> Result<Vec<u8>, SignerError> {
        let signature = self.signing_key.sign(data);
        Ok(signature.to_bytes().to_vec())
    }

    fn verify(&self, data: &[u8], signature: &[u8]) -> Result<(), SignerError> {
        Self::verify_with_public_key(data, &self.public_key(), signature)
    }
}

fn base64_decode(input: &str) -> Result<Vec<u8>, SignerError> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input)
        .map_err(|e| SignerError::Pem(e.to_string()))
}