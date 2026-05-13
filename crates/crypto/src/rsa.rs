// SPDX-License-Identifier: Apache-2.0
//! RSA signature implementation.

use pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePublicKey};
use rsa::{
    Pkcs1v15Sign, RsaPrivateKey, RsaPublicKey, pkcs1::DecodeRsaPrivateKey, rand_core::OsRng,
};
use sha2::{Digest, Sha256};

use crate::{Signer, SignerError};

/// RSA signer.
pub struct RsaSigner {
    private_key: RsaPrivateKey,
    public_key: RsaPublicKey,
}

impl RsaSigner {
    pub fn generate(key_size: usize) -> Result<Self, SignerError> {
        let private_key = RsaPrivateKey::new(&mut OsRng, key_size)
            .map_err(|e| SignerError::Rsa(e.to_string()))?;
        let public_key = private_key.to_public_key();

        Ok(Self {
            private_key,
            public_key,
        })
    }

    pub fn from_pem(pem: &str) -> Result<Self, SignerError> {
        let private_key = if pem.contains("-----BEGIN RSA PRIVATE KEY-----") {
            RsaPrivateKey::from_pkcs1_pem(pem).map_err(|e| SignerError::Rsa(e.to_string()))?
        } else if pem.contains("-----BEGIN PRIVATE KEY-----") {
            RsaPrivateKey::from_pkcs8_pem(pem).map_err(|e| SignerError::Pkcs8(e.to_string()))?
        } else {
            return Err(SignerError::UnknownKeyFormat);
        };

        let public_key = private_key.to_public_key();
        Ok(Self {
            private_key,
            public_key,
        })
    }

    pub fn to_pem(&self) -> Result<String, SignerError> {
        use pkcs8::EncodePrivateKey;

        self.private_key
            .to_pkcs8_pem(pkcs8::LineEnding::LF)
            .map(|pem| pem.to_string())
            .map_err(|e| SignerError::Pkcs8(e.to_string()))
    }

    pub fn public_key_to_pem(&self) -> Result<String, SignerError> {
        self.public_key
            .to_public_key_pem(pkcs8::LineEnding::LF)
            .map_err(|e| SignerError::Pkcs8(e.to_string()))
    }

    pub fn verify_with_public_key(
        data: &[u8],
        public_key_pem: &[u8],
        signature: &[u8],
    ) -> Result<(), SignerError> {
        let public_key_str = std::str::from_utf8(public_key_pem)
            .map_err(|e| SignerError::InvalidPublicKey(e.to_string()))?;
        let public_key = RsaPublicKey::from_public_key_pem(public_key_str)
            .map_err(|e| SignerError::InvalidPublicKey(e.to_string()))?;
        let hash = Sha256::digest(data);
        public_key
            .verify(Pkcs1v15Sign::new::<Sha256>(), &hash, signature)
            .map_err(|_| SignerError::VerificationFailed)
    }
}

impl Signer for RsaSigner {
    fn algorithm(&self) -> &'static str {
        "rsa"
    }

    fn public_key(&self) -> Vec<u8> {
        self.public_key_to_pem()
            .unwrap_or_default()
            .as_bytes()
            .to_vec()
    }

    fn sign(&self, data: &[u8]) -> Result<Vec<u8>, SignerError> {
        let hash = Sha256::digest(data);
        self.private_key
            .sign(Pkcs1v15Sign::new::<Sha256>(), &hash)
            .map_err(|e| SignerError::Rsa(e.to_string()))
    }

    fn verify(&self, data: &[u8], signature: &[u8]) -> Result<(), SignerError> {
        let hash = Sha256::digest(data);
        self.public_key
            .verify(Pkcs1v15Sign::new::<Sha256>(), &hash, signature)
            .map_err(|_| SignerError::VerificationFailed)
    }
}