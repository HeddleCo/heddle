// SPDX-License-Identifier: MIT OR Apache-2.0

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use crate::{
    Error, Result,
    canonical::{digest, key_id},
    wire::{AuthorizationKeyAlgorithm, AuthorizationSignature, AuthorizationVerificationKey},
};

pub(crate) fn validate_key(key: &AuthorizationVerificationKey) -> Result<()> {
    if key.algorithm != AuthorizationKeyAlgorithm::Ed25519 as i32 || key.public_key.len() != 32 {
        return Err(Error::Invalid(
            "authorization key is not 32-byte Ed25519".to_owned(),
        ));
    }
    let bytes: &[u8; 32] = key
        .public_key
        .as_slice()
        .try_into()
        .map_err(|_| Error::InvalidSignature)?;
    VerifyingKey::from_bytes(bytes).map_err(|_| Error::InvalidSignature)?;
    Ok(())
}

pub(crate) fn verify_signature(
    key: &AuthorizationVerificationKey,
    signature: &AuthorizationSignature,
    domain: &[u8],
    body: &[u8],
) -> Result<()> {
    validate_key(key)?;
    if signature.signer_key_id.as_slice() != key_id(key) || signature.signature.len() != 64 {
        return Err(Error::InvalidSignature);
    }
    let key_bytes: &[u8; 32] = key
        .public_key
        .as_slice()
        .try_into()
        .map_err(|_| Error::InvalidSignature)?;
    let signature_bytes: &[u8; 64] = signature
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| Error::InvalidSignature)?;
    let verifying_key = VerifyingKey::from_bytes(key_bytes).map_err(|_| Error::InvalidSignature)?;
    verifying_key
        .verify(
            &digest(domain, body),
            &Signature::from_bytes(signature_bytes),
        )
        .map_err(|_| Error::InvalidSignature)
}

pub(crate) fn verify_digest_signature(
    key: &AuthorizationVerificationKey,
    signature: &AuthorizationSignature,
    signed_digest: &[u8; 32],
) -> Result<()> {
    validate_key(key)?;
    if signature.signer_key_id.as_slice() != key_id(key) || signature.signature.len() != 64 {
        return Err(Error::InvalidSignature);
    }
    let key_bytes: &[u8; 32] = key
        .public_key
        .as_slice()
        .try_into()
        .map_err(|_| Error::InvalidSignature)?;
    let signature_bytes: &[u8; 64] = signature
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| Error::InvalidSignature)?;
    let verifying_key = VerifyingKey::from_bytes(key_bytes).map_err(|_| Error::InvalidSignature)?;
    verifying_key
        .verify(signed_digest, &Signature::from_bytes(signature_bytes))
        .map_err(|_| Error::InvalidSignature)
}
