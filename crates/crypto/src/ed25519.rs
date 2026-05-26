// SPDX-License-Identifier: Apache-2.0
//! Ed25519 signature implementation.

use ed25519_dalek::{Signature, Signer as EdSigner, SigningKey, Verifier, VerifyingKey};
use rsa::rand_core::OsRng;

use crate::{Signer, SignerError};

/// Ed25519 signer.
pub struct Ed25519Signer {
    signing_key: SigningKey,
    cached_public_key: [u8; 32],
}

impl Ed25519Signer {
    pub fn generate() -> Result<Self, SignerError> {
        let signing_key = SigningKey::generate(&mut OsRng);
        let cached_public_key = signing_key.verifying_key().to_bytes();
        Ok(Self {
            signing_key,
            cached_public_key,
        })
    }

    pub fn from_pem(pem: &str) -> Result<Self, SignerError> {
        use crate::pem_loader::{classify_pem, PemKind};

        match classify_pem(pem) {
            PemKind::Pkcs8 => Self::from_pkcs8_pem(pem),
            PemKind::OpenSsh => Self::from_openssh_pem(pem),
            PemKind::Ed25519HexSeed => {
                let bytes = hex::decode(pem.trim()).map_err(|e| SignerError::Pem(e.to_string()))?;
                Self::from_seed(&bytes)
            }
            PemKind::Ed25519Base64Seed => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(pem.trim())
                    .map_err(|e| SignerError::Pem(e.to_string()))?;
                if bytes.len() == 64 {
                    Self::from_seed(&bytes[..32])
                } else {
                    Self::from_seed(&bytes)
                }
            }
            _ => Err(SignerError::UnknownKeyFormat),
        }
    }

    pub fn from_seed(seed: &[u8]) -> Result<Self, SignerError> {
        let seed_bytes: [u8; 32] = seed
            .try_into()
            .map_err(|_| SignerError::InvalidKey("seed must be 32 bytes".to_string()))?;
        let signing_key = SigningKey::from_bytes(&seed_bytes);
        let cached_public_key = signing_key.verifying_key().to_bytes();
        Ok(Self {
            signing_key,
            cached_public_key,
        })
    }

    fn from_pkcs8_pem(pem: &str) -> Result<Self, SignerError> {
        use pkcs8::DecodePrivateKey;

        let signing_key = SigningKey::from_pkcs8_pem(pem)?;
        let cached_public_key = signing_key.verifying_key().to_bytes();
        Ok(Self {
            signing_key,
            cached_public_key,
        })
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

    fn public_key(&self) -> &[u8] {
        &self.cached_public_key
    }

    fn sign(&self, data: &[u8]) -> Result<Vec<u8>, SignerError> {
        let signature = self.signing_key.sign(data);
        Ok(signature.to_bytes().to_vec())
    }

    fn verify(&self, data: &[u8], signature: &[u8]) -> Result<(), SignerError> {
        Self::verify_with_public_key(data, &self.public_key(), signature)
    }
}
