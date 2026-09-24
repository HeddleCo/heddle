//! Owner-authorized WebAuthn delegation to temporary Ed25519 mint keys.
//!
//! Verification is pure and uses independently established owner authority.
//! WebAuthn assertions follow https://www.w3.org/TR/webauthn-3/#sctn-verifying-assertion.
//! A passkey certificate never substitutes for resource permissions or revocation.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use heddle_api::passkey_mint_grant::{
    MAX_PASSKEY_SESSION_TTL_SECONDS, passkey_mint_grant_signing_digest,
    verify_passkey_mint_grant_window,
};
use p256::pkcs8::DecodePublicKey;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    Error, Result, VerifiedOwnerState,
    canonical::{Encoder, digest},
    crypto::{validate_key, verify_signature},
    wire::{
        AuthorizationVerificationKey, PasskeyAuthority, SignedMintRootAttachment,
        SignedPasskeyAuthority,
    },
};

/// Domain for the owner's canonical passkey authorization.
pub const PASSKEY_AUTHORITY_DOMAIN: &[u8] = b"heddle-passkey-authority-v1";
const ED25519_SPKI_PREFIX: &[u8] = &[
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

fn owner_key(value: &PasskeyAuthority) -> Result<&AuthorizationVerificationKey> {
    value
        .owner_key
        .as_ref()
        .ok_or_else(|| invalid("passkey owner key missing"))
}

/// Canonical owner certificate fields in protocol tag order.
pub fn canonical_passkey_authority(value: &PasskeyAuthority) -> Result<Vec<u8>> {
    if value.format_version != 1
        || value.account_uuid.len() != 16
        || value.account_uuid.iter().all(|byte| *byte == 0)
        || value.owner_state_hash.len() != 32
        || value.credential_id.is_empty()
        || value.credential_id.len() > 1024
        || value.nonce.len() != 32
        || value.max_session_ttl_seconds == 0
        || value.max_session_ttl_seconds > MAX_PASSKEY_SESSION_TTL_SECONDS
        || value.relying_party_id.is_empty()
        || value.relying_party_id.len() > 253
        || value
            .relying_party_id
            .bytes()
            .any(|byte| !byte.is_ascii_alphanumeric() && byte != b'.' && byte != b'-')
        || value.allowed_origins.is_empty()
        || value.allowed_origins.len() > 8
        || value
            .allowed_origins
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        || value.allowed_origins.iter().any(|origin| {
            origin.is_empty()
                || origin.len() > 2048
                || origin.contains('*')
                || origin.chars().any(char::is_whitespace)
        })
    {
        return Err(invalid("invalid passkey authority fields or bounds"));
    }
    let owner = owner_key(value)?;
    validate_key(owner)?;
    validate_passkey_key(value.cose_algorithm, &value.public_key_spki)?;
    let mut body = Encoder::new();
    body.u32(value.format_version);
    body.bytes(&value.account_uuid)?;
    body.bytes(&value.owner_state_hash)?;
    body.u64(value.owner_sequence);
    body.i32(owner.algorithm);
    body.bytes(&owner.public_key)?;
    body.bytes(&value.credential_id)?;
    body.i32(value.cose_algorithm);
    body.bytes(&value.public_key_spki)?;
    body.string(&value.relying_party_id)?;
    body.count(value.allowed_origins.len())?;
    for origin in &value.allowed_origins {
        body.string(origin)?;
    }
    body.u32(value.max_session_ttl_seconds);
    body.bytes(&value.nonce)?;
    Ok(body.finish())
}

/// Digest signed by the account's current owner authority.
pub fn passkey_authority_signing_digest(value: &PasskeyAuthority) -> Result<[u8; 32]> {
    Ok(digest(
        PASSKEY_AUTHORITY_DOMAIN,
        &canonical_passkey_authority(value)?,
    ))
}

/// Validate a newly registered or reauthorized passkey against the current owner.
/// The host independently compares the credential ID, SPKI and RP to its verified
/// WebAuthn registration and applies current account authorization/revocation.
pub fn verify_passkey_authority(
    signed: &SignedPasskeyAuthority,
    current: &VerifiedOwnerState,
    expected_account_uuid: &[u8],
) -> Result<()> {
    let value = signed
        .authority
        .as_ref()
        .ok_or_else(|| invalid("passkey authority missing"))?;
    let root = current
        .signed_root()
        .root
        .as_ref()
        .ok_or_else(|| invalid("owner root missing"))?;
    if value.account_uuid != expected_account_uuid
        || value.account_uuid != root.account_uuid
        || value.owner_state_hash.as_slice() != current.state_hash()
        || value.owner_sequence != current.sequence()
        || owner_key(value)? != current.authority_key()
    {
        return Err(invalid(
            "passkey authority differs from current account owner",
        ));
    }
    verify_certificate(signed, current.authority_key())
}

fn verify_certificate(
    signed: &SignedPasskeyAuthority,
    issuer: &AuthorizationVerificationKey,
) -> Result<()> {
    let value = signed
        .authority
        .as_ref()
        .ok_or_else(|| invalid("passkey authority missing"))?;
    if owner_key(value)? != issuer {
        return Err(invalid(
            "passkey certificate issuer differs from attachment owner",
        ));
    }
    verify_signature(
        issuer,
        signed
            .owner_signature
            .as_ref()
            .ok_or_else(|| invalid("passkey owner signature missing"))?,
        PASSKEY_AUTHORITY_DOMAIN,
        &canonical_passkey_authority(value)?,
    )
}

fn ed25519_key(spki: &[u8]) -> Result<VerifyingKey> {
    let raw = spki
        .strip_prefix(ED25519_SPKI_PREFIX)
        .ok_or_else(|| invalid("invalid Ed25519 SPKI"))?;
    let bytes: &[u8; 32] = raw
        .try_into()
        .map_err(|_| invalid("invalid Ed25519 SPKI length"))?;
    VerifyingKey::from_bytes(bytes).map_err(|_| Error::InvalidSignature)
}

/// Validate the certificate's supported algorithm and public SPKI encoding.
pub fn validate_passkey_key(algorithm: i32, spki: &[u8]) -> Result<()> {
    if spki.len() > 512 {
        return Err(invalid("passkey SPKI exceeds bound"));
    }
    match algorithm {
        -8 => {
            ed25519_key(spki)?;
        }
        -7 => {
            p256::ecdsa::VerifyingKey::from_public_key_der(spki)
                .map_err(|_| invalid("invalid ES256 SPKI"))?;
        }
        _ => return Err(invalid("passkey delegation requires ES256 or Ed25519")),
    }
    Ok(())
}

#[derive(Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    ceremony_type: String,
    challenge: String,
    origin: String,
    #[serde(rename = "crossOrigin", default)]
    cross_origin: bool,
    #[serde(rename = "topOrigin", default)]
    top_origin: Option<String>,
}

/// Verify one owner-certified, WebAuthn-signed, time-bounded temporary mint root.
/// The ceremony caller must atomically consume `(account, grant nonce)` when it
/// first admits the mint root. Later operations may reverify this portable proof;
/// verification here neither persists nor substitutes for that replay gate.
pub fn verify_mint_delegation(
    signed: &SignedMintRootAttachment,
    current: &VerifiedOwnerState,
    expected_account_uuid: &[u8],
    expected_mint_root_key: &[u8],
    now: i64,
) -> Result<()> {
    let grant = signed
        .grant
        .as_ref()
        .ok_or_else(|| invalid("passkey mint grant missing"))?;
    let proof = signed
        .passkey_delegation
        .as_ref()
        .ok_or_else(|| invalid("passkey delegation missing"))?;
    let certificate = proof
        .authority
        .as_ref()
        .ok_or_else(|| invalid("passkey certificate missing"))?;
    verify_passkey_authority(certificate, current, expected_account_uuid)?;
    let authority = certificate
        .authority
        .as_ref()
        .ok_or_else(|| invalid("passkey authority missing"))?;
    // The grant deliberately contains no account or owner fields. The verified
    // owner-signed authority is their only source. Callers must atomically mark
    // (account, grant nonce) as used at admission; this pure verifier cannot
    // enforce single use or persist replay state.
    verify_passkey_mint_grant_window(grant, authority.max_session_ttl_seconds, now)
        .map_err(|error| invalid(error.to_string()))?;
    if grant.relying_party_id != authority.relying_party_id
        || grant
            .mint_root_key
            .as_ref()
            .is_none_or(|key| key.public_key != expected_mint_root_key)
        || proof.client_data_json.len() > 8192
        || !(37..=4096).contains(&proof.authenticator_data.len())
        || proof.signature.is_empty()
        || proof.signature.len() > 80
    {
        return Err(invalid(
            "passkey delegation differs from attachment or exceeds bounds",
        ));
    }
    let client: ClientData = serde_json::from_slice(&proof.client_data_json)
        .map_err(|_| invalid("invalid passkey client data"))?;
    if client.ceremony_type != "webauthn.get"
        || client.challenge
            != URL_SAFE_NO_PAD.encode(
                passkey_mint_grant_signing_digest(grant)
                    .map_err(|error| invalid(error.to_string()))?,
            )
        || !authority.allowed_origins.contains(&client.origin)
        || client.cross_origin
        || client.top_origin.is_some()
    {
        return Err(invalid("passkey assertion challenge or origin mismatch"));
    }
    let rp_hash = Sha256::digest(authority.relying_party_id.as_bytes());
    let flags = proof.authenticator_data[32];
    if proof.authenticator_data[..32] != rp_hash[..]
        || flags & 0x05 != 0x05
        || flags & 0x40 != 0
        || (flags & 0x10 != 0 && flags & 0x08 == 0)
    {
        return Err(invalid(
            "passkey RP, presence, verification or backup flags invalid",
        ));
    }
    let mut signed = proof.authenticator_data.clone();
    signed.extend_from_slice(&Sha256::digest(&proof.client_data_json));
    match authority.cose_algorithm {
        -8 => ed25519_key(&authority.public_key_spki)?
            .verify_strict(
                &signed,
                &Signature::from_slice(&proof.signature).map_err(|_| Error::InvalidSignature)?,
            )
            .map_err(|_| Error::InvalidSignature),
        -7 => {
            let signature = p256::ecdsa::Signature::from_der(&proof.signature)
                .map_err(|_| Error::InvalidSignature)?;
            if signature != signature.normalize_s() {
                return Err(Error::InvalidSignature);
            }
            p256::ecdsa::VerifyingKey::from_public_key_der(&authority.public_key_spki)
                .map_err(|_| Error::InvalidSignature)?
                .verify(&signed, &signature)
                .map_err(|_| Error::InvalidSignature)
        }
        _ => Err(invalid("unsupported passkey signature algorithm")),
    }
}
