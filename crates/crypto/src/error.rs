// SPDX-License-Identifier: Apache-2.0
//! Error types for cryptographic signing.

use std::path::PathBuf;

/// Error type for signer operations.
#[derive(Debug)]
pub enum SignerError {
    UnsupportedAlgorithm(String),
    UnknownKeyFormat,
    InvalidKey(String),
    InvalidSignature(String),
    InvalidPublicKey(String),
    Io(std::io::Error),
    Pem(String),
    Ed25519(String),
    Rsa(String),
    P256(String),
    Pkcs8(String),
    KeyNotFound(PathBuf),
    VerificationFailed,
}

impl std::fmt::Display for SignerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignerError::UnsupportedAlgorithm(algo) => {
                write!(f, "unsupported signature algorithm: {}", algo)
            }
            SignerError::UnknownKeyFormat => write!(f, "unknown or unsupported key format"),
            SignerError::InvalidKey(msg) => write!(f, "invalid key: {}", msg),
            SignerError::InvalidSignature(msg) => write!(f, "invalid signature: {}", msg),
            SignerError::InvalidPublicKey(msg) => write!(f, "invalid public key: {}", msg),
            SignerError::Io(e) => write!(f, "I/O error: {}", e),
            SignerError::Pem(msg) => write!(f, "PEM error: {}", msg),
            SignerError::Ed25519(msg) => write!(f, "Ed25519 error: {}", msg),
            SignerError::Rsa(msg) => write!(f, "RSA error: {}", msg),
            SignerError::P256(msg) => write!(f, "P256 error: {}", msg),
            SignerError::Pkcs8(msg) => write!(f, "PKCS8 error: {}", msg),
            SignerError::KeyNotFound(path) => write!(f, "key file not found: {}", path.display()),
            SignerError::VerificationFailed => write!(f, "signature verification failed"),
        }
    }
}

impl std::error::Error for SignerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SignerError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SignerError {
    fn from(e: std::io::Error) -> Self {
        SignerError::Io(e)
    }
}

impl From<ed25519_dalek::SignatureError> for SignerError {
    fn from(e: ed25519_dalek::SignatureError) -> Self {
        SignerError::Ed25519(e.to_string())
    }
}

impl From<rsa::Error> for SignerError {
    fn from(e: rsa::Error) -> Self {
        SignerError::Rsa(e.to_string())
    }
}

impl From<pkcs8::Error> for SignerError {
    fn from(e: pkcs8::Error) -> Self {
        SignerError::Pkcs8(e.to_string())
    }
}

impl From<pkcs8::spki::Error> for SignerError {
    fn from(e: pkcs8::spki::Error) -> Self {
        SignerError::Pkcs8(e.to_string())
    }
}

impl From<sec1::Error> for SignerError {
    fn from(e: sec1::Error) -> Self {
        SignerError::Pem(e.to_string())
    }
}