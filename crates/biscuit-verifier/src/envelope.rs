//! Transport-free grant envelopes for client-minted Biscuits.
//!
//! # Why this module exists
//!
//! Clients mint every biscuit, including anonymous ones. Weft verifies
//! against registered device public keys (and any extra public pins from
//! `BISCUIT_PUBLIC_KEYS`). It does not hold a minting key
//! and does not generalise a server-rooted authority block.
//!
//! For client-minted tokens the load-bearing question is no longer
//! "did the root sign this?" — by construction it didn't — but "is
//! this client key authorised to mint a token with this scope?" The
//! threat model spells out the answer (see
//! `docs/E2_THREAT_MODEL.md` §3 Tampering row and §6 item 2): the
//! verifier accepts a client authority block only if **(i)** the
//! signing pubkey is on the user's registered list, and **(ii)** the
//! asserted `right(...)` facts are a subset of the user's effective
//! grant envelope.
//!
//! A **grant envelope** is the cryptographic carrier for that
//! authorisation. It is a small structure, signed by the root trust
//! list, that binds
//!
//!   (device public key, subject, authorised rights, lifetime)
//!
//! and is shipped alongside the client-minted Biscuit on every
//! authenticated request. The verifier checks the envelope's
//! signature against the same trust list it uses for legacy tokens,
//! confirms the envelope binds the actual signer of the token, and
//! then enforces that the token's asserted rights are a subset of the
//! envelope's authorised rights.
//!
//! That is the CURRENT bridge. TARGET authorization chains from a
//! spool owner's key instead of treating weft as a trust root; whether
//! weft retains exactly one narrowly scoped envelope key or zero
//! signing keys remains open. See
//! `docs/IDENTITY_RESOURCE_AUTHORIZATION_MODEL.md` and
//! HeddleCo/weft#836.
//!
//! This module owns the envelope shape, its canonical signing
//! payload, and the sign / verify primitives. The dual-path verifier
//! that consumes it lives in [`crate::biscuit::verify_client_minted`].
//!
//! # Wire format
//!
//! An envelope on the wire is base64url-NO-PAD of the byte sequence
//!
//! ```text
//!   <canonical-payload> || <64-byte Ed25519 signature>
//! ```
//!
//! where `<canonical-payload>` is a versioned, length-prefixed binary
//! encoding produced by [`GrantEnvelope::canonical_payload`]. Keeping
//! signing and wire encoding distinct (payload + appended sig rather
//! than a structured wrapper) means we never sign anything we did not
//! also commit to verbatim — a precaution against signature-malleability
//! and "forgot to include field X in the digest" footguns. The version
//! byte is the *only* extension point; bumping it forces every verifier
//! to reject anything it does not know how to parse rather than silently
//! ignoring unknown trailing bytes.
//!
//! # Why a separate module (not a Biscuit attenuation block)
//!
//! Biscuit's own `BlockBuilder` can attach root-signed blocks, but the
//! Datalog those blocks carry is interpreted by the authoriser — which
//! is exactly the surface a client-minted token tries to escape. A
//! flat, hand-rolled envelope with no Datalog content is much easier
//! to audit: the verifier reads four fixed-shape fields and compares
//! them. The trade-off is that the envelope cannot piggy-back on
//! Biscuit's third-party-block machinery, but that machinery is
//! overkill for what is essentially a signed (pubkey, scope, ttl)
//! credential.

use std::convert::TryFrom;

use base64::Engine as _;
use biscuit_auth::PublicKey;
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};

use super::{BiscuitError, BiscuitResultExt, facts::Right};

/// Wire-format version byte for [`GrantEnvelope`]. A future breaking
/// change (e.g., adding a `kid` header to disambiguate which root key
/// signed the envelope when the trust list grew past two members)
/// would bump this; pre-1.0 we keep the format trivial.
const ENVELOPE_VERSION: u8 = 1;

/// Maximum length of the `subject` field in bytes. The same upper
/// bound the `pg_registry` store enforces on `users.username`, kept
/// here so a malformed envelope can be rejected without touching the
/// database.
pub const MAX_SUBJECT_LEN: usize = 256;

/// Maximum number of authorised rights an envelope may carry. Bounds
/// the signing-payload size so a hostile or buggy issuer cannot drive
/// the verifier into an unbounded allocation.
pub const MAX_RIGHTS: usize = 64;

/// Maximum byte length of any single `Right` field (kind/path/action).
/// The Biscuit rule pack imposes its own bounds; this one is the
/// envelope-layer fence so we never deserialise a hostile payload.
pub const MAX_RIGHT_FIELD_LEN: usize = 1024;

/// The structured contents of a grant envelope. Pre-signature shape;
/// the on-wire encoding ([`SignedGrantEnvelope`]) appends a 64-byte
/// Ed25519 signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantEnvelope {
    /// Raw 32-byte Ed25519 public key the envelope authorises. The
    /// verifier checks that the client-minted Biscuit's authority
    /// block was signed by *this* key — not by anything else on the
    /// trust list.
    pub device_pubkey: [u8; 32],
    /// Principal subject the envelope authorises. The verifier
    /// rejects if the token's `user(...)` fact disagrees.
    pub subject: String,
    /// The full set of rights the device key is allowed to mint
    /// tokens for. The verifier checks that the token's asserted
    /// `right(...)` facts are a subset of this set.
    pub rights: Vec<Right>,
    /// Wall-clock issuance time. Round-tripped through the signing
    /// payload as RFC3339 seconds-resolution UTC so the on-the-wire
    /// bytes are stable across clock-precision variants.
    pub issued_at: DateTime<Utc>,
    /// Wall-clock expiry. The verifier rejects when `Utc::now()` is
    /// past this. Short lifetimes (hours, not weeks) are the intent
    /// — the envelope is a hot credential the client refreshes via
    /// `RegisterPublicKey` reissuance.
    pub expires_at: DateTime<Utc>,
}

/// A grant envelope plus its Ed25519 signature. The signature covers
/// [`GrantEnvelope::canonical_payload`] verbatim and is generated by a
/// root keypair from the same trust list used to verify legacy tokens.
#[derive(Debug, Clone)]
pub struct SignedGrantEnvelope {
    pub envelope: GrantEnvelope,
    pub signature: [u8; 64],
}

impl GrantEnvelope {
    /// Render the canonical signing payload. The output is the
    /// authoritative byte sequence the signer commits to; verifiers
    /// recompute it from the parsed fields and check the appended
    /// signature against it.
    ///
    /// Format (all integers big-endian unless noted):
    ///
    /// ```text
    ///   u8       version (== ENVELOPE_VERSION)
    ///   32 B     device_pubkey
    ///   u16 + N  subject (length-prefixed UTF-8)
    ///   u16      rights_count
    ///     repeated:
    ///       u16 + N kind
    ///       u16 + N path
    ///       u16 + N action
    ///   i64      issued_at (unix seconds)
    ///   i64      expires_at (unix seconds)
    /// ```
    ///
    /// The DateTime fields are encoded as seconds-resolution unix
    /// timestamps rather than RFC3339 strings so the byte-level
    /// representation is independent of `chrono`'s formatting choices
    /// (no trailing fractional seconds, no `Z` vs `+00:00` ambiguity).
    pub fn canonical_payload(&self) -> Result<Vec<u8>, BiscuitError> {
        if self.subject.len() > MAX_SUBJECT_LEN {
            return Err(BiscuitError::EnvelopeInvalid(format!(
                "subject exceeds {MAX_SUBJECT_LEN} bytes"
            )));
        }
        if self.rights.len() > MAX_RIGHTS {
            return Err(BiscuitError::EnvelopeInvalid(format!(
                "rights count exceeds {MAX_RIGHTS}"
            )));
        }
        let mut out = Vec::with_capacity(64 + self.subject.len() + self.rights.len() * 64);
        out.push(ENVELOPE_VERSION);
        out.extend_from_slice(&self.device_pubkey);
        write_lenprefixed_str(&mut out, &self.subject)?;
        let rights_len = u16::try_from(self.rights.len()).map_err(|_| {
            BiscuitError::EnvelopeInvalid(format!("rights count {} > u16::MAX", self.rights.len()))
        })?;
        out.extend_from_slice(&rights_len.to_be_bytes());
        for right in &self.rights {
            write_lenprefixed_str(&mut out, &right.kind)?;
            write_lenprefixed_str(&mut out, &right.path)?;
            write_lenprefixed_str(&mut out, &right.action)?;
        }
        out.extend_from_slice(&self.issued_at.timestamp().to_be_bytes());
        out.extend_from_slice(&self.expires_at.timestamp().to_be_bytes());
        Ok(out)
    }

    /// Parse a canonical payload back into its structured form. The
    /// inverse of [`Self::canonical_payload`]. Rejects anything we
    /// would not have produced ourselves — unknown version byte,
    /// over-long fields, trailing bytes, malformed UTF-8.
    fn from_canonical_payload(payload: &[u8]) -> Result<Self, BiscuitError> {
        let mut cursor = ByteCursor::new(payload);
        let version = cursor.take_u8()?;
        if version != ENVELOPE_VERSION {
            return Err(BiscuitError::EnvelopeInvalid(format!(
                "unsupported envelope version {version}"
            )));
        }
        let device_pubkey: [u8; 32] = cursor
            .take_array::<32>()
            .map_err(|e| BiscuitError::EnvelopeInvalid(format!("device pubkey: {e}")))?;
        let subject = cursor.take_lenprefixed_str(MAX_SUBJECT_LEN)?;
        let rights_count = cursor.take_u16()? as usize;
        if rights_count > MAX_RIGHTS {
            return Err(BiscuitError::EnvelopeInvalid(format!(
                "rights count {rights_count} > {MAX_RIGHTS}"
            )));
        }
        let mut rights = Vec::with_capacity(rights_count);
        for _ in 0..rights_count {
            let kind = cursor.take_lenprefixed_str(MAX_RIGHT_FIELD_LEN)?;
            let path = cursor.take_lenprefixed_str(MAX_RIGHT_FIELD_LEN)?;
            let action = cursor.take_lenprefixed_str(MAX_RIGHT_FIELD_LEN)?;
            rights.push(Right { kind, path, action });
        }
        let issued_at_s = cursor.take_i64()?;
        let expires_at_s = cursor.take_i64()?;
        if !cursor.is_empty() {
            return Err(BiscuitError::EnvelopeInvalid(format!(
                "{} unexpected trailing byte(s)",
                cursor.remaining()
            )));
        }
        let issued_at = DateTime::<Utc>::from_timestamp(issued_at_s, 0).ok_or_else(|| {
            BiscuitError::EnvelopeInvalid(format!("issued_at {issued_at_s} not representable"))
        })?;
        let expires_at = DateTime::<Utc>::from_timestamp(expires_at_s, 0).ok_or_else(|| {
            BiscuitError::EnvelopeInvalid(format!("expires_at {expires_at_s} not representable"))
        })?;
        Ok(GrantEnvelope {
            device_pubkey,
            subject,
            rights,
            issued_at,
            expires_at,
        })
    }

    /// Sign this envelope with the root keypair, producing a
    /// transport-ready [`SignedGrantEnvelope`]. The caller supplies a
    /// Biscuit-style root keypair: this module pulls out its raw
    /// private bytes and signs via `ed25519-dalek` directly so the
    /// signature scheme is identical to what the trust-list verifier
    /// already expects.
    pub fn sign_with_root(
        self,
        root: &biscuit_auth::KeyPair,
    ) -> Result<SignedGrantEnvelope, BiscuitError> {
        let payload = self.canonical_payload()?;
        let signing_key = signing_key_from_biscuit(root)?;
        let sig = signing_key.sign(&payload);
        Ok(SignedGrantEnvelope {
            envelope: self,
            signature: sig.to_bytes(),
        })
    }
}

impl SignedGrantEnvelope {
    /// Verify the signature against `trust_list` (Biscuit root
    /// pubkeys). Returns `Ok(())` if any trusted key validates the
    /// signature; `Err(BiscuitError::EnvelopeInvalid)` otherwise. The
    /// trust list is iterated linearly — the rotation window typically
    /// holds at most two keys, so this is cheap.
    pub fn verify_signature(&self, trust_list: &[PublicKey]) -> Result<(), BiscuitError> {
        if trust_list.is_empty() {
            return Err(BiscuitError::Internal(
                "envelope trust list is empty; verifier is unconfigured".to_string(),
            ));
        }
        let payload = self.envelope.canonical_payload()?;
        let sig = Signature::from_bytes(&self.signature);
        for pk in trust_list {
            let vk = verifying_key_from_biscuit(pk)?;
            if vk.verify(&payload, &sig).is_ok() {
                return Ok(());
            }
        }
        Err(BiscuitError::EnvelopeInvalid(
            "envelope signature did not verify against any trusted root key".to_string(),
        ))
    }

    /// Encode the signed envelope as a base64url-NO-PAD string for
    /// carrying in the `x-heddle-grant-envelope` request metadata.
    pub fn to_base64(&self) -> Result<String, BiscuitError> {
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.to_bytes()?))
    }

    /// Encode the signed envelope as raw bytes (`canonical_payload`
    /// concatenated with the 64-byte Ed25519 signature). This is the
    /// shape proto3 `bytes` fields carry — e.g. transport-neutral Heddle API's
    /// `AccessTokenResponse.grant_envelope`. Clients that need to
    /// carry the same envelope through `x-heddle-grant-envelope`
    /// metadata base64url-encode these bytes before placing them on
    /// the header.
    pub fn to_bytes(&self) -> Result<Vec<u8>, BiscuitError> {
        let mut payload = self.envelope.canonical_payload()?;
        payload.extend_from_slice(&self.signature);
        Ok(payload)
    }

    /// Parse a base64url-encoded signed envelope. Does NOT verify the
    /// signature — call [`Self::verify_signature`] for that.
    pub fn from_base64(s: &str) -> Result<Self, BiscuitError> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s.trim().as_bytes())
            .map_err(|e| BiscuitError::EnvelopeInvalid(format!("base64 decode: {e}")))?;
        if raw.len() < 64 {
            return Err(BiscuitError::EnvelopeInvalid(format!(
                "envelope {} bytes < 64-byte signature footer",
                raw.len()
            )));
        }
        let split = raw.len() - 64;
        let (payload, sig_bytes) = raw.split_at(split);
        let envelope = GrantEnvelope::from_canonical_payload(payload)?;
        let mut signature = [0u8; 64];
        signature.copy_from_slice(sig_bytes);
        Ok(SignedGrantEnvelope {
            envelope,
            signature,
        })
    }
}

/// Pull the raw 32-byte private-key seed out of a Biscuit `KeyPair`
/// and reconstruct an `ed25519-dalek` `SigningKey`. The two crates
/// agree on the Ed25519 byte layout — biscuit-auth uses ed25519-dalek
/// under the hood — so this is a straight re-wrap.
pub(crate) fn signing_key_from_biscuit(
    root: &biscuit_auth::KeyPair,
) -> Result<SigningKey, BiscuitError> {
    let bytes = root.private().to_bytes();
    let seed: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| BiscuitError::Internal("root private key is not 32 bytes".to_string()))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Convert a Biscuit `PublicKey` into an `ed25519-dalek`
/// `VerifyingKey`. Used by the envelope verifier (and by the
/// dual-path verifier when it derives a single-key trust list for the
/// client-minted Biscuit itself).
pub(crate) fn verifying_key_from_biscuit(
    pk: &biscuit_auth::PublicKey,
) -> Result<VerifyingKey, BiscuitError> {
    let bytes = pk.to_bytes();
    let raw: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| BiscuitError::Internal("biscuit pubkey is not 32 bytes".to_string()))?;
    VerifyingKey::from_bytes(&raw)
        .internal_ctx("biscuit pubkey is not a valid Ed25519 verifying key")
}

/// Derive a Biscuit `PublicKey` from a raw 32-byte Ed25519 public
/// key. Used to materialise the single-key trust list a client-minted
/// Biscuit's signature is verified against.
///
/// `pub` (was `pub(crate)`) for the weft#719 phase 3 crate boundary: the
/// device-key path in `weft_server::server::hosted::identity` calls it.
pub fn biscuit_pubkey_from_raw(raw: &[u8; 32]) -> Result<biscuit_auth::PublicKey, BiscuitError> {
    biscuit_auth::PublicKey::from_bytes(raw, biscuit_auth::Algorithm::Ed25519)
        .internal_ctx("device pubkey not a valid Ed25519 key")
}

// ---- byte-cursor helpers --------------------------------------------

struct ByteCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn is_empty(&self) -> bool {
        self.offset >= self.bytes.len()
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], BiscuitError> {
        if self.offset + n > self.bytes.len() {
            return Err(BiscuitError::EnvelopeInvalid(format!(
                "truncated payload: need {n} more byte(s), have {}",
                self.remaining()
            )));
        }
        let out = &self.bytes[self.offset..self.offset + n];
        self.offset += n;
        Ok(out)
    }

    fn take_u8(&mut self) -> Result<u8, BiscuitError> {
        Ok(self.take(1)?[0])
    }

    fn take_u16(&mut self) -> Result<u16, BiscuitError> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn take_i64(&mut self) -> Result<i64, BiscuitError> {
        let bytes = self.take(8)?;
        let arr: [u8; 8] = bytes.try_into().expect("take(8) returns 8 bytes");
        Ok(i64::from_be_bytes(arr))
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        if self.offset + N > self.bytes.len() {
            return Err("array read past end");
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.offset..self.offset + N]);
        self.offset += N;
        Ok(out)
    }

    fn take_lenprefixed_str(&mut self, max: usize) -> Result<String, BiscuitError> {
        let len = self.take_u16()? as usize;
        if len > max {
            return Err(BiscuitError::EnvelopeInvalid(format!(
                "length-prefixed string {len} > limit {max}"
            )));
        }
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(|s| s.to_string())
            .map_err(|e| BiscuitError::EnvelopeInvalid(format!("invalid utf-8: {e}")))
    }
}

fn write_lenprefixed_str(out: &mut Vec<u8>, s: &str) -> Result<(), BiscuitError> {
    let len = u16::try_from(s.len()).map_err(|_| {
        BiscuitError::EnvelopeInvalid(format!("string length {} > u16::MAX", s.len()))
    })?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

/// Builder for a fresh, well-formed envelope. Defaults the issuance
/// timestamp to `Utc::now()` and lifetime to the supplied TTL.
///
/// Timestamps are truncated to whole seconds before being stored —
/// the canonical signing payload encodes them as `i64` unix seconds
/// (see [`GrantEnvelope::canonical_payload`]), so anything finer
/// would be lost on the first round trip and break
/// `signed.to_base64()` → `from_base64()` equality.
/// Rights carried on a bootstrap-scope grant envelope (no repo access).
pub fn grant_envelope_rights_bootstrap_session() -> Vec<Right> {
    Vec::new()
}

/// Rights for a promoted / full browser session envelope. Mirrors
/// `scope_string_to_rights("spool:*")` — hosted grants gate repo access.
pub fn grant_envelope_rights_full_session() -> Vec<Right> {
    Vec::new()
}

/// Legacy scope string for a bootstrap session bearer / envelope.
pub fn access_scope_bootstrap_session() -> &'static str {
    ""
}

/// Legacy scope string for a full session bearer / envelope.
pub fn access_scope_full_session() -> &'static str {
    "spool:*"
}

pub fn build(
    device_pubkey: [u8; 32],
    subject: impl Into<String>,
    rights: Vec<Right>,
    ttl: Duration,
) -> GrantEnvelope {
    let issued_at = truncate_to_seconds(Utc::now());
    GrantEnvelope {
        device_pubkey,
        subject: subject.into(),
        rights,
        issued_at,
        expires_at: issued_at + ttl,
    }
}

/// Drop sub-second precision from `dt`. Used so envelopes built via
/// `build()` round-trip cleanly through the seconds-resolution
/// canonical payload; tests constructing envelopes by hand should
/// either pass through this helper or use seconds-aligned literals.
pub(crate) fn truncate_to_seconds(dt: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(dt.timestamp(), 0).unwrap_or(dt)
}

// =====================================================================
// Tests — these are the **red-commit assertions** for the envelope half
// of HeddleCo/weft#102. They drive the API surface above; the green
// commit implements `sign_with_root` / `verify_signature` to make them
// pass.
// =====================================================================

#[cfg(test)]
mod tests {
    use biscuit_auth::KeyPair;

    use super::*;

    fn sample_envelope(device_pubkey: [u8; 32]) -> GrantEnvelope {
        build(
            device_pubkey,
            "alice",
            vec![
                Right::spool_admin("org/acme"),
                Right::spool_write("org/acme/heddle"),
            ],
            Duration::hours(1),
        )
    }

    fn device_pubkey_from(kp: &KeyPair) -> [u8; 32] {
        let bytes = kp.public().to_bytes();
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        out
    }

    /// **AC #1 — happy path.** A well-formed envelope signed by the
    /// root keypair must round-trip through `to_base64` →
    /// `from_base64` and verify against a trust list containing the
    /// matching root pubkey.
    #[test]
    fn signed_envelope_round_trips_and_verifies() {
        let root = KeyPair::new();
        let device = KeyPair::new();
        let envelope = sample_envelope(device_pubkey_from(&device));
        let signed = envelope.clone().sign_with_root(&root).expect("sign");
        let encoded = signed.to_base64().expect("encode");
        let decoded = SignedGrantEnvelope::from_base64(&encoded).expect("decode");
        assert_eq!(decoded.envelope, envelope);
        decoded
            .verify_signature(&[root.public()])
            .expect("verify against trusted root");
    }

    /// **AC #2 — tampered payload.** Flipping a byte in the
    /// canonical payload must invalidate the signature even when the
    /// flipped value still parses (e.g., changing the subject from
    /// "alice" → "alic_"). Catches the "envelope mutated in transit"
    /// shape directly.
    #[test]
    fn tampered_subject_fails_verify() {
        let root = KeyPair::new();
        let device = KeyPair::new();
        let mut envelope = sample_envelope(device_pubkey_from(&device));
        let signed = envelope.clone().sign_with_root(&root).expect("sign");
        // Mutate the subject after signing and rebuild the
        // signed-envelope manually — the signature still references
        // "alice" but the payload says "mallory".
        envelope.subject = "mallory".to_string();
        let tampered = SignedGrantEnvelope {
            envelope,
            signature: signed.signature,
        };
        let err = tampered
            .verify_signature(&[root.public()])
            .expect_err("tampered envelope must not verify");
        assert!(matches!(err, BiscuitError::EnvelopeInvalid(_)));
    }

    /// **AC #3 — wrong signer.** An envelope signed by a key that is
    /// not in the trust list must be rejected. Direct mirror of the
    /// legacy-token "signature mismatch" check.
    #[test]
    fn envelope_signed_by_untrusted_key_is_rejected() {
        let trusted_root = KeyPair::new();
        let rogue_root = KeyPair::new();
        let device = KeyPair::new();
        let envelope = sample_envelope(device_pubkey_from(&device));
        let signed = envelope.sign_with_root(&rogue_root).expect("sign rogue");
        let err = signed
            .verify_signature(&[trusted_root.public()])
            .expect_err("rogue-signed envelope must not verify");
        assert!(matches!(err, BiscuitError::EnvelopeInvalid(_)));
    }

    /// Rotation window: an envelope signed by either of two trusted
    /// roots must verify. Mirrors `parse_token`'s rotation behaviour
    /// so an operator rotating the root keypair does not have to
    /// re-mint every outstanding envelope on flip day.
    #[test]
    fn envelope_verifies_against_either_trusted_root_during_rotation() {
        let old_root = KeyPair::new();
        let new_root = KeyPair::new();
        let device = KeyPair::new();
        let envelope = sample_envelope(device_pubkey_from(&device));
        let signed = envelope.sign_with_root(&old_root).expect("sign with old");
        signed
            .verify_signature(&[new_root.public(), old_root.public()])
            .expect("either key in rotation window verifies");
    }

    /// Empty trust list surfaces as `Internal` (a configuration
    /// error), not `EnvelopeInvalid` (a bad input). Matches the
    /// legacy `parse_token` behaviour — the verifier never silently
    /// accepts an envelope just because nothing was configured.
    #[test]
    fn empty_trust_list_is_internal_error_not_invalid() {
        let root = KeyPair::new();
        let device = KeyPair::new();
        let envelope = sample_envelope(device_pubkey_from(&device));
        let signed = envelope.sign_with_root(&root).expect("sign");
        let err = signed
            .verify_signature(&[])
            .expect_err("empty trust list must error");
        assert!(matches!(err, BiscuitError::Internal(_)));
    }

    /// Garbage base64 surfaces a clean `EnvelopeInvalid` error rather
    /// than panicking. Pins the parser's failure mode for fuzz-driven
    /// inputs.
    #[test]
    fn malformed_base64_is_envelope_invalid() {
        let err = SignedGrantEnvelope::from_base64("!!!not-base64!!!")
            .expect_err("garbage must be rejected");
        assert!(matches!(err, BiscuitError::EnvelopeInvalid(_)));
    }

    /// A decoded payload shorter than a 64-byte signature footer is
    /// rejected before the parser ever looks at the body.
    #[test]
    fn payload_shorter_than_signature_is_envelope_invalid() {
        // 10 bytes of base64 round-trip to 7 raw bytes — comfortably
        // below the 64-byte signature footer threshold.
        let short = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"too-short");
        let err = SignedGrantEnvelope::from_base64(&short)
            .expect_err("under-length payload must be rejected");
        assert!(matches!(err, BiscuitError::EnvelopeInvalid(_)));
    }

    /// An unknown version byte is rejected. Future format extensions
    /// MUST bump the version byte and roll the verifiers forward
    /// first; silently accepting unknown trailing bytes would be a
    /// downgrade-attack vector.
    #[test]
    fn unknown_version_byte_is_envelope_invalid() {
        let root = KeyPair::new();
        let device = KeyPair::new();
        let envelope = sample_envelope(device_pubkey_from(&device));
        let mut payload = envelope.canonical_payload().expect("payload");
        payload[0] = 0xFF; // wrong version
        let signing_key = signing_key_from_biscuit(&root).expect("signing key");
        let sig = signing_key.sign(&payload);
        let mut wire = payload;
        wire.extend_from_slice(&sig.to_bytes());
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(wire);
        let err =
            SignedGrantEnvelope::from_base64(&encoded).expect_err("unknown version must reject");
        assert!(matches!(err, BiscuitError::EnvelopeInvalid(_)));
    }

    /// The Biscuit-to-dalek public-key bridge round-trips a key
    /// produced by `KeyPair::new`. Pins the assumption the two crates
    /// share Ed25519 byte layout.
    #[test]
    fn biscuit_pubkey_bridge_round_trips() {
        let kp = KeyPair::new();
        let raw = device_pubkey_from(&kp);
        let pk = biscuit_pubkey_from_raw(&raw).expect("biscuit pubkey");
        assert_eq!(pk.to_bytes(), kp.public().to_bytes());
    }
}
