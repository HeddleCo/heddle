// SPDX-License-Identifier: Apache-2.0
//! P-256 (ECDSA) signature implementation.

use p256::{
    SecretKey,
    ecdsa::{
        Signature, SigningKey, VerifyingKey,
        signature::{Signer as SignatureSigner, Verifier as SignatureVerifier},
    },
};
use pkcs8::DecodePrivateKey;
use rsa::{pkcs1::DecodeRsaPrivateKey, rand_core::OsRng};

use crate::{Signer, SignerError};

/// P-256 (ECDSA) signer.
pub struct P256Signer {
    signing_key: SigningKey,
    cached_public_key: Vec<u8>,
}

impl P256Signer {
    fn from_signing_key(signing_key: SigningKey) -> Self {
        let cached_public_key = signing_key
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();
        Self {
            signing_key,
            cached_public_key,
        }
    }

    pub fn generate() -> Result<Self, SignerError> {
        Ok(Self::from_signing_key(SigningKey::random(&mut OsRng)))
    }

    pub fn from_pem(pem: &str) -> Result<Self, SignerError> {
        if let Ok(signing_key) = SigningKey::from_pkcs8_pem(pem) {
            return Ok(Self::from_signing_key(signing_key));
        }

        if let Ok(signing_key) = SigningKey::from_pkcs1_pem(pem) {
            return Ok(Self::from_signing_key(signing_key));
        }

        if let Ok(ec_sk) = SecretKey::from_sec1_pem(pem) {
            return Ok(Self::from_signing_key(SigningKey::from(ec_sk)));
        }

        Err(SignerError::UnknownKeyFormat)
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
        let verifying_key = VerifyingKey::from_sec1_bytes(public_key)
            .map_err(|e| SignerError::InvalidPublicKey(e.to_string()))?;
        if let Ok(signature) = Signature::from_slice(signature) {
            return verifying_key
                .verify(data, &signature)
                .map_err(|_| SignerError::VerificationFailed);
        }
        let signature = Signature::from_der(signature)
            .map_err(|e| SignerError::InvalidSignature(e.to_string()))?;
        verifying_key
            .verify(data, &signature)
            .map_err(|_| SignerError::VerificationFailed)
    }
}

impl Signer for P256Signer {
    fn algorithm(&self) -> &'static str {
        "p256"
    }

    fn public_key(&self) -> &[u8] {
        &self.cached_public_key
    }

    fn sign(&self, data: &[u8]) -> Result<Vec<u8>, SignerError> {
        let signature: Signature = self.signing_key.sign(data);
        Ok(signature.to_bytes().to_vec())
    }

    fn verify(&self, data: &[u8], signature: &[u8]) -> Result<(), SignerError> {
        Self::verify_with_public_key(data, self.public_key(), signature)
    }
}
