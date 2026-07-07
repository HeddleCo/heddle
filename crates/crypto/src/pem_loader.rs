// SPDX-License-Identifier: Apache-2.0
//! PEM-header classification shared across signer backends.
//!
//! Every `Signer` impl in this crate had its own ad-hoc header sniff
//! (Ed25519 and P-256 each parsed the same `-----BEGIN ...-----`
//! lines). Centralizing the classification keeps the "which formats
//! Heddle accepts" question answerable from a single source.
//!
//! The dispatch helper [`load_signer_from_pem`] mirrors what
//! `lib.rs::load_signer` used to do inline, but now reads as a
//! straight match instead of a chain of `contains()` predicates.

use crate::{Ed25519Signer, P256Signer, Signer, SignerError};

/// The wire format inferred from a PEM blob's BEGIN line, or `Raw*`
/// when the input is just hex/base64 seed bytes with no PEM wrapper.
/// Each variant maps to exactly one `Signer` constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PemKind {
    /// `-----BEGIN PRIVATE KEY-----` — RFC 5208 PKCS#8.
    Pkcs8,
    /// `-----BEGIN EC PRIVATE KEY-----` — SEC1.
    Sec1Ec,
    /// `-----BEGIN OPENSSH PRIVATE KEY-----` — not yet supported.
    OpenSsh,
    /// Bare 32 hex bytes (Ed25519 seed).
    Ed25519HexSeed,
    /// Bare base64 — 32 bytes (seed) or 64 bytes (signing-key + public-key pair).
    Ed25519Base64Seed,
    Unknown,
}

/// Classify a PEM/raw-key blob by its header (or shape, for unwrapped
/// seed material). Pure function — no I/O, no allocation beyond what
/// the input trim implies.
pub fn classify_pem(pem: &str) -> PemKind {
    let trimmed = pem.trim();
    if trimmed.contains("-----BEGIN PRIVATE KEY-----") {
        return PemKind::Pkcs8;
    }
    if trimmed.contains("-----BEGIN EC PRIVATE KEY-----") {
        return PemKind::Sec1Ec;
    }
    if trimmed.contains("-----BEGIN OPENSSH PRIVATE KEY-----") {
        return PemKind::OpenSsh;
    }
    if hex::decode(trimmed).is_ok_and(|b| b.len() == 32) {
        return PemKind::Ed25519HexSeed;
    }
    use base64::Engine;
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(trimmed)
        && (bytes.len() == 32 || bytes.len() == 64)
    {
        return PemKind::Ed25519Base64Seed;
    }
    PemKind::Unknown
}

/// Dispatch a PEM blob to the right `Signer` backend.
///
/// Replaces the chain of `if pem_content.contains(...)` blocks that
/// `load_signer` used to inline. PKCS#8 is ambiguous (the same BEGIN
/// line wraps multiple private-key algorithms), so the PKCS#8 case
/// probes the supported backends in order and returns the first one
/// that accepts the key.
pub fn load_signer_from_pem(pem: &str) -> Result<Box<dyn Signer>, SignerError> {
    match classify_pem(pem) {
        PemKind::Pkcs8 => {
            // PKCS#8 doesn't expose the algorithm in the BEGIN line, so try
            // each backend. Ed25519 keys also encode the marker `MC4CAQ` near
            // the start of the base64 body; checking that first avoids
            // needlessly probing P-256 for common Ed25519 input.
            if pem.contains("MC4CAQ")
                && let Ok(s) = Ed25519Signer::from_pem(pem)
            {
                return Ok(Box::new(s) as Box<dyn Signer>);
            }
            if let Ok(s) = P256Signer::from_pem(pem) {
                return Ok(Box::new(s) as Box<dyn Signer>);
            }
            if let Ok(s) = Ed25519Signer::from_pem(pem) {
                return Ok(Box::new(s) as Box<dyn Signer>);
            }
            Err(SignerError::UnknownKeyFormat)
        }
        PemKind::Sec1Ec => P256Signer::from_pem(pem).map(|s| Box::new(s) as Box<dyn Signer>),
        PemKind::Ed25519HexSeed | PemKind::Ed25519Base64Seed => {
            Ed25519Signer::from_pem(pem).map(|s| Box::new(s) as Box<dyn Signer>)
        }
        PemKind::OpenSsh => Err(SignerError::Pem(
            "OpenSSH private keys are not yet supported".to_string(),
        )),
        PemKind::Unknown => Err(SignerError::UnknownKeyFormat),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_pkcs8_header() {
        let pem = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBC...\n-----END PRIVATE KEY-----";
        assert_eq!(classify_pem(pem), PemKind::Pkcs8);
    }

    #[test]
    fn rejects_pkcs1_rsa_header() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIBOg...\n-----END RSA PRIVATE KEY-----";
        assert_eq!(classify_pem(pem), PemKind::Unknown);
    }

    #[test]
    fn classifies_sec1_ec_header() {
        let pem = "-----BEGIN EC PRIVATE KEY-----\nMHc...\n-----END EC PRIVATE KEY-----";
        assert_eq!(classify_pem(pem), PemKind::Sec1Ec);
    }

    #[test]
    fn classifies_openssh_header() {
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Bl...\n-----END OPENSSH PRIVATE KEY-----";
        assert_eq!(classify_pem(pem), PemKind::OpenSsh);
    }

    #[test]
    fn classifies_hex_seed() {
        let pem = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(classify_pem(pem), PemKind::Ed25519HexSeed);
    }

    #[test]
    fn unknown_input_classified_as_such() {
        assert_eq!(classify_pem(""), PemKind::Unknown);
        assert_eq!(classify_pem("not a key"), PemKind::Unknown);
    }
}
