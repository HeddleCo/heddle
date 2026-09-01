//! Signing and verification of a [`CiVerdictBody`].
//!
//! A [`SignedVerdict`] binds an ed25519 signature to the body's *content digest*
//! ([`CiVerdictBody::body_digest`]), not to a bare opaque token. Verification
//! recomputes the digest from the embedded body and rejects any mismatch — so a
//! tampered body (e.g. a flipped conclusion) fails even though the signature over
//! the original digest is still cryptographically valid. This is the mechanism
//! the security review names as mandatory: the gate must verify
//! *signature → body digest → body fields*, keying trust on the public key, never
//! on an actor name.
//!
//! # Signing payload (cross-language)
//!
//! The ed25519 signature is **not** over the bare `b3:<hex>` digest string. It is
//! over a domain-separated, NUL-framed preimage that also binds the envelope
//! metadata ([`SignedVerdict::signed_at`] and [`SignedVerdict::signer_kind`]),
//! built by [`signed_payload`]:
//!
//! ```text
//! preimage =
//!       b"heddle-ci-verdict-v2\x00"      // 21-byte domain tag: 20 ASCII bytes + 1 NUL
//!    || body_digest                      // ASCII of "b3:" + 64 lowercase hex chars
//!    || 0x00                             // NUL separator
//!    || signer_kind_str                  // "service_account" | "delegated" | "device"
//!    || 0x00                             // NUL separator
//!    || signed_at                        // RFC3339 timestamp, ASCII
//!    || 0x00                             // NUL terminator
//! ```
//!
//! Every framed field is ASCII with no embedded NUL (the digest is `b3:`+hex; the
//! signer-kind token is a fixed enum string; `signed_at` is RFC3339), so the NUL
//! framing is unambiguous and a verifier in any language can reproduce the exact
//! bytes from the three JSON fields `body_digest`, `signer_kind`, `signed_at`.
//!
//! ## Why each part
//!
//! - **Domain tag** (`heddle-ci-verdict-v2\x00`): mirrors heddle's other signing
//!   surfaces (`hd-rev-sig-v1\x00`, the redaction/state-visibility tags). Without
//!   it, any other context where a runner key signs a `b3:<hex>` string (the
//!   schema already carries `log.manifest_digest`) would yield a signature
//!   *mutually valid* as a verdict signature — a cross-protocol confusion the
//!   tag makes impossible. The trailing `v2` versions the preimage itself.
//!   Verification selects v1 for schema-v1 verdicts and v2 for schema-v2
//!   verdicts, preserving existing signatures while all new signatures use v2.
//! - **`signer_kind`** is load-bearing: a `device`-signed verdict is advisory-only
//!   and may never satisfy a required gate (DESIGN §7). Folding it into the
//!   preimage means relabeling `device → delegated` breaks the signature.
//! - **`signed_at`** is the freshness/replay-presentation field; binding it stops
//!   silent rewrites. (It is still the caller's responsibility to range-check the
//!   timestamp against policy — the signature only proves it wasn't altered.)
//!
//! The body digest itself is unchanged by this; only the signature *preimage*
//! differs from a naive "sign the digest string" scheme. The golden signing
//! vectors in `tests/fixtures/vectors.json` pin the exact bytes for cross-language
//! reproduction.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::body::{CiVerdictBody, DIGEST_PREFIX, SCHEMA_VERSION};

/// What kind of principal signed a verdict.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignerKind {
    /// A legacy non-human runner service account.
    ServiceAccount,
    /// A CI runner acting with authority delegated by a human principal.
    #[default]
    Delegated,
    /// A human device key (e.g. a local `heddle check run`).
    Device,
}

impl SignerKind {
    /// The stable lowercase token used both in JSON (`snake_case`) and as the
    /// `signer_kind` field of the [signing preimage](signed_payload). Changing
    /// these strings is a signing-payload break (bump the domain tag).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            SignerKind::ServiceAccount => "service_account",
            SignerKind::Delegated => "delegated",
            SignerKind::Device => "device",
        }
    }

    /// Whether this signer kind may satisfy a required gate.
    ///
    /// This is only the schema-level classification. The caller must separately
    /// authorize the public key and apply its trust/policy rules.
    #[must_use]
    pub const fn may_satisfy_required_gate(self) -> bool {
        matches!(self, SignerKind::ServiceAccount | SignerKind::Delegated)
    }
}

/// A [`CiVerdictBody`] together with its signature and signer identity.
///
/// The on-the-wire JSON is the OSS contract any harness can produce and verify.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SignedVerdict {
    /// The signed body.
    pub body: CiVerdictBody,
    /// The `b3:…` digest of [`SignedVerdict::body`], recomputed and checked on verify.
    pub body_digest: String,
    /// The signer's ed25519 public key, hex-encoded.
    pub public_key: String,
    /// The ed25519 signature over the [signing preimage](signed_payload),
    /// hex-encoded. The preimage binds the domain tag, `body_digest`,
    /// `signer_kind`, and `signed_at` — so none of those can be rewritten without
    /// invalidating the signature.
    pub signature: String,
    /// RFC3339 timestamp the verdict was signed. **Part of the signed preimage**
    /// (binding it stops freshness/replay-presentation rewrites); the signature
    /// only proves it was not altered, not that it is within any policy window.
    pub signed_at: String,
    /// What kind of principal signed. **Part of the signed preimage** — a
    /// `device → delegated` relabel breaks the signature.
    pub signer_kind: SignerKind,
}

/// Errors from [`SignedVerdict::verify`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum VerifyError {
    /// The embedded `body_digest` did not match the recomputed digest of `body`
    /// — i.e. the body was tampered with after signing.
    #[error("body digest mismatch: signed {signed}, recomputed {recomputed}")]
    BodyDigestMismatch {
        /// The digest stored in the signed verdict.
        signed: String,
        /// The digest recomputed from the embedded body.
        recomputed: String,
    },
    /// A hex field (`public_key` or `signature`) was not valid hex.
    #[error("malformed hex in {field}: {reason}")]
    MalformedHex {
        /// Which field failed to decode.
        field: &'static str,
        /// Why it failed.
        reason: String,
    },
    /// The public key bytes were not a valid ed25519 verifying key.
    #[error("invalid public key: {0}")]
    InvalidPublicKey(String),
    /// The signature bytes were not a valid ed25519 signature.
    #[error("invalid signature encoding: {0}")]
    InvalidSignatureEncoding(String),
    /// The signature did not verify against the public key over the digest bytes.
    #[error("signature verification failed")]
    SignatureInvalid,
    /// The body's `schema_version` is not one this crate can verify. For a newer
    /// version, canonicalize-by-reserialization cannot reproduce the producer's
    /// exact bytes because this crate would drop any field it doesn't know.
    ///
    /// This is reported as its **own** error — never as
    /// [`VerifyError::BodyDigestMismatch`] — so a forward-compat verdict from a
    /// newer producer is distinguishable from genuine tampering. A consumer that
    /// must verify across schema versions has to do so over the producer's raw
    /// bytes (the proto mandates exactly this server-side: verify over the exact
    /// `signed_verdict_json` bytes, never a reserialized struct).
    #[error("unsupported schema_version {found}: this verifier supports up to {supported}")]
    UnsupportedSchemaVersion {
        /// The `schema_version` carried by the body.
        found: u32,
        /// The maximum `schema_version` this crate can verify by reserialization.
        supported: u32,
    },
    /// The signer kind did not exist in the claimed schema version.
    #[error("signer_kind {signer_kind:?} is not supported by schema_version {schema_version}")]
    UnsupportedSignerKindForSchemaVersion {
        /// The signer kind carried by the envelope.
        signer_kind: SignerKind,
        /// The body schema version that selects the signing domain.
        schema_version: u32,
    },
}

/// The domain-separation tag every CI-verdict signing preimage begins with.
///
/// Mirrors heddle's other signing surfaces (`hd-rev-sig-v1\x00` and friends).
/// Bumping the trailing version versions the *preimage layout*: old signatures
/// with the old tag continue to verify under the old code path.
pub const SIGNING_PAYLOAD_VERSION_TAG: &[u8] = b"heddle-ci-verdict-v2\x00";

const V1_SIGNING_PAYLOAD_VERSION_TAG: &[u8] = b"heddle-ci-verdict-v1\x00";

/// Build the exact bytes the ed25519 key signs. See the module-level
/// "Signing payload (cross-language)" section for the full layout and rationale.
///
/// Layout: `TAG || body_digest || 0x00 || signer_kind.as_str() || 0x00 ||
/// signed_at || 0x00`. Every framed field is NUL-free ASCII, so the framing is
/// unambiguous and reproducible from the JSON fields alone.
#[must_use]
pub fn signed_payload(body_digest: &str, signer_kind: SignerKind, signed_at: &str) -> Vec<u8> {
    signed_payload_with_tag(
        SIGNING_PAYLOAD_VERSION_TAG,
        body_digest,
        signer_kind,
        signed_at,
    )
}

fn signed_payload_with_tag(
    tag: &[u8],
    body_digest: &str,
    signer_kind: SignerKind,
    signed_at: &str,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(tag.len() + body_digest.len() + signed_at.len() + 19);
    buf.extend_from_slice(tag);
    buf.extend_from_slice(body_digest.as_bytes());
    buf.push(0);
    buf.extend_from_slice(signer_kind.as_str().as_bytes());
    buf.push(0);
    buf.extend_from_slice(signed_at.as_bytes());
    buf.push(0);
    buf
}

/// Decode a lowercase/uppercase ASCII-hex string to bytes, **panic-free on
/// arbitrary (attacker-controlled) input**.
///
/// `verify()` is the security boundary the gate calls on runner-submitted
/// verdicts, and `public_key`/`signature` are attacker-controlled strings off the
/// wire. This decoder therefore operates on raw bytes and never slices the `&str`
/// at a fixed offset (`&s[i..i+2]`): a multi-byte UTF-8 codepoint with even byte
/// length (e.g. `"€€"`, 6 bytes) passes a naive `len % 2` check yet a mid-codepoint
/// slice panics with "byte index N is not a char boundary". We reject any non-ASCII
/// input up front and then decode nibble-by-nibble over `as_bytes()`, so every
/// malformed input returns [`VerifyError::MalformedHex`] — never a panic (a
/// remotely-triggerable DoS).
#[allow(clippy::chunks_exact_to_as_chunks, clippy::manual_is_multiple_of)]
fn decode_hex(field: &'static str, s: &str) -> Result<Vec<u8>, VerifyError> {
    // Reject non-ASCII before touching byte offsets: every hex char is ASCII, and
    // this guarantees `as_bytes()` indices are also char boundaries (no panic path).
    if !s.is_ascii() {
        return Err(VerifyError::MalformedHex {
            field,
            reason: "non-ASCII input".to_string(),
        });
    }
    let bytes = s.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(VerifyError::MalformedHex {
            field,
            reason: "odd length".to_string(),
        });
    }
    let nibble = |b: u8| -> Result<u8, VerifyError> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => Err(VerifyError::MalformedHex {
                field,
                reason: format!("invalid hex digit {:?}", b as char),
            }),
        }
    };
    bytes
        .chunks_exact(2)
        .map(|pair| Ok((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Sign a verdict body with the given ed25519 key.
///
/// `signed_at` must be an RFC3339 timestamp; this crate does not depend on a clock
/// so the caller supplies it (the runner uses the pick's finish time). The body is
/// stamped with [`SCHEMA_VERSION`] so new signatures always use the v2 domain.
#[must_use]
pub fn sign(
    mut body: CiVerdictBody,
    key: &SigningKey,
    signed_at: String,
    signer_kind: SignerKind,
) -> SignedVerdict {
    // The body schema version is also the authenticated signing-format
    // discriminator. Current producers always mint v2; only verification retains
    // the legacy v1 path.
    body.schema_version = SCHEMA_VERSION;
    let body_digest = body.body_digest();
    let signature = key.sign(&signed_payload(&body_digest, signer_kind, &signed_at));
    let public_key = key.verifying_key();
    SignedVerdict {
        body,
        public_key: encode_hex(public_key.as_bytes()),
        signature: encode_hex(&signature.to_bytes()),
        body_digest,
        signed_at,
        signer_kind,
    }
}

impl SignedVerdict {
    /// Sign a body, producing a [`SignedVerdict`]. Convenience over [`sign`].
    #[must_use]
    pub fn sign(
        body: CiVerdictBody,
        key: &SigningKey,
        signed_at: String,
        signer_kind: SignerKind,
    ) -> Self {
        sign(body, key, signed_at, signer_kind)
    }

    /// Verify this verdict end-to-end.
    ///
    /// 0. Reject a `schema_version` other than 1 or [`SCHEMA_VERSION`] with
    ///    [`VerifyError::UnsupportedSchemaVersion`] — *before* recomputing the
    ///    digest. This crate canonicalizes by reserializing the parsed struct, and
    ///    serde drops unknown fields on parse, so a newer producer's verdict would
    ///    otherwise recompute to a different digest and be misreported as
    ///    [`VerifyError::BodyDigestMismatch`] (indistinguishable from tampering).
    ///    Forward-compat verdicts are *parseable* (unknown fields are tolerated,
    ///    per the body's lack of `deny_unknown_fields`) but only *verifiable* here
    ///    up to this crate's known schema; cross-version verification must run over
    ///    the producer's raw bytes.
    /// 1. Recompute the body digest from the embedded body and reject any mismatch
    ///    against `body_digest` (tamper detection).
    /// 2. Select the v1 domain tag for a schema-v1 body or the v2 tag for a
    ///    schema-v2 body, then verify the ed25519 signature against `public_key`
    ///    over `tag ‖ digest ‖ signer_kind ‖ signed_at`. Thus old v1 verdicts
    ///    remain verifiable, while [`sign`] mints only v2 verdicts.
    ///
    /// Trust (is `public_key` in the repo's runner trust set?) is the *caller's*
    /// responsibility — this function proves authenticity, not authorization.
    pub fn verify(&self) -> Result<(), VerifyError> {
        let signing_tag = match self.body.schema_version {
            1 => V1_SIGNING_PAYLOAD_VERSION_TAG,
            SCHEMA_VERSION => SIGNING_PAYLOAD_VERSION_TAG,
            found => {
                return Err(VerifyError::UnsupportedSchemaVersion {
                    found,
                    supported: SCHEMA_VERSION,
                });
            }
        };
        if self.body.schema_version == 1 && self.signer_kind == SignerKind::Delegated {
            return Err(VerifyError::UnsupportedSignerKindForSchemaVersion {
                signer_kind: self.signer_kind,
                schema_version: self.body.schema_version,
            });
        }

        let recomputed = self.body.body_digest();
        if recomputed != self.body_digest {
            return Err(VerifyError::BodyDigestMismatch {
                signed: self.body_digest.clone(),
                recomputed,
            });
        }

        let key_bytes = decode_hex("public_key", &self.public_key)?;
        let key_arr: [u8; 32] = key_bytes
            .as_slice()
            .try_into()
            .map_err(|_| VerifyError::InvalidPublicKey("expected 32 bytes".to_string()))?;
        let verifying_key = VerifyingKey::from_bytes(&key_arr)
            .map_err(|e| VerifyError::InvalidPublicKey(e.to_string()))?;

        let sig_bytes = decode_hex("signature", &self.signature)?;
        let sig_arr: [u8; 64] = sig_bytes
            .as_slice()
            .try_into()
            .map_err(|_| VerifyError::InvalidSignatureEncoding("expected 64 bytes".to_string()))?;
        let signature = Signature::from_bytes(&sig_arr);

        verifying_key
            .verify(
                &signed_payload_with_tag(
                    signing_tag,
                    &self.body_digest,
                    self.signer_kind,
                    &self.signed_at,
                ),
                &signature,
            )
            .map_err(|_| VerifyError::SignatureInvalid)
    }

    /// The signer's public key, hex-encoded — the value a caller matches against a
    /// trust set. (Stable accessor so callers don't reach into the field directly.)
    #[must_use]
    pub fn signer_public_key(&self) -> &str {
        &self.public_key
    }
}

/// Whether `s` looks like one of this crate's content digests (`b3:<hex>`).
#[must_use]
pub fn is_content_digest(s: &str) -> bool {
    s.strip_prefix(DIGEST_PREFIX)
        .is_some_and(|hex| !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::*;
    use crate::fixture;

    #[test]
    fn delegated_v2_roundtrip_rejects_tampering_and_forgery() {
        let key = SigningKey::from_bytes(&[7_u8; 32]);
        let signed = sign(
            fixture::passing_body(),
            &key,
            "2026-09-01T12:00:00Z".into(),
            SignerKind::Delegated,
        );

        assert_eq!(signed.body.schema_version, 2);
        assert_eq!(SignerKind::default(), SignerKind::Delegated);
        assert!(signed.signer_kind.may_satisfy_required_gate());
        assert!(SignerKind::ServiceAccount.may_satisfy_required_gate());
        assert!(!SignerKind::Device.may_satisfy_required_gate());

        let json = serde_json::to_string(&signed).expect("serialize delegated verdict");
        let roundtrip: SignedVerdict =
            serde_json::from_str(&json).expect("deserialize delegated verdict");
        assert_eq!(roundtrip, signed);
        assert_eq!(roundtrip.verify(), Ok(()));

        let mut body_tampered = signed.clone();
        body_tampered.body.outcome.conclusion = crate::Conclusion::Failure;
        assert!(matches!(
            body_tampered.verify(),
            Err(VerifyError::BodyDigestMismatch { .. })
        ));

        let mut wrong_signer_kind = signed.clone();
        wrong_signer_kind.signer_kind = SignerKind::ServiceAccount;
        assert_eq!(
            wrong_signer_kind.verify(),
            Err(VerifyError::SignatureInvalid)
        );

        let attacker_key = SigningKey::from_bytes(&[8_u8; 32]);
        let mut forged = signed.clone();
        forged.signature = encode_hex(
            &attacker_key
                .sign(&signed_payload(
                    &forged.body_digest,
                    forged.signer_kind,
                    &forged.signed_at,
                ))
                .to_bytes(),
        );
        assert_eq!(forged.verify(), Err(VerifyError::SignatureInvalid));
    }

    #[test]
    fn schema_version_selects_v1_or_v2_domain_tag_and_rejects_cross_tag_signatures() {
        let key = SigningKey::from_bytes(&[9_u8; 32]);
        let signed_at = "2026-09-01T12:00:00Z";

        let mut v1_body = fixture::passing_body();
        v1_body.schema_version = 1;
        let v1_digest = v1_body.body_digest();
        let v1_signature = key.sign(&signed_payload_with_tag(
            V1_SIGNING_PAYLOAD_VERSION_TAG,
            &v1_digest,
            SignerKind::ServiceAccount,
            signed_at,
        ));
        let v1 = SignedVerdict {
            body: v1_body,
            body_digest: v1_digest,
            public_key: encode_hex(key.verifying_key().as_bytes()),
            signature: encode_hex(&v1_signature.to_bytes()),
            signed_at: signed_at.into(),
            signer_kind: SignerKind::ServiceAccount,
        };
        assert_eq!(v1.verify(), Ok(()), "v1 verdict must remain verifiable");

        let mut impossible_v1_delegated = v1.clone();
        impossible_v1_delegated.signer_kind = SignerKind::Delegated;
        impossible_v1_delegated.signature = encode_hex(
            &key.sign(&signed_payload_with_tag(
                V1_SIGNING_PAYLOAD_VERSION_TAG,
                &impossible_v1_delegated.body_digest,
                impossible_v1_delegated.signer_kind,
                signed_at,
            ))
            .to_bytes(),
        );
        assert_eq!(
            impossible_v1_delegated.verify(),
            Err(VerifyError::UnsupportedSignerKindForSchemaVersion {
                signer_kind: SignerKind::Delegated,
                schema_version: 1,
            })
        );

        let v2 = sign(
            fixture::passing_body(),
            &key,
            signed_at.into(),
            SignerKind::Delegated,
        );
        assert_eq!(v2.verify(), Ok(()));
        assert!(
            signed_payload(&v2.body_digest, v2.signer_kind, signed_at)
                .starts_with(SIGNING_PAYLOAD_VERSION_TAG)
        );

        let mut v1_with_v2_tag = v1.clone();
        v1_with_v2_tag.signature = encode_hex(
            &key.sign(&signed_payload_with_tag(
                SIGNING_PAYLOAD_VERSION_TAG,
                &v1_with_v2_tag.body_digest,
                v1_with_v2_tag.signer_kind,
                signed_at,
            ))
            .to_bytes(),
        );
        assert_eq!(v1_with_v2_tag.verify(), Err(VerifyError::SignatureInvalid));

        let mut v2_with_v1_tag = v2;
        v2_with_v1_tag.signature = encode_hex(
            &key.sign(&signed_payload_with_tag(
                V1_SIGNING_PAYLOAD_VERSION_TAG,
                &v2_with_v1_tag.body_digest,
                v2_with_v1_tag.signer_kind,
                signed_at,
            ))
            .to_bytes(),
        );
        assert_eq!(v2_with_v1_tag.verify(), Err(VerifyError::SignatureInvalid));
    }

    // ── decode_hex: panic-free on arbitrary input ─────────────────────────────

    #[test]
    fn decode_hex_decodes_valid_lowercase_and_uppercase() {
        assert_eq!(decode_hex("f", "00ff").unwrap(), vec![0x00, 0xff]);
        // Mixed case decodes to the same bytes (callers emit lowercase, but a
        // cross-language producer might uppercase).
        assert_eq!(
            decode_hex("f", "DeAdBeEf").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(decode_hex("f", "").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn decode_hex_rejects_multibyte_utf8_without_panicking() {
        // "€€" is 6 bytes (EVEN — passes a naive `len % 2` check) but every `€` is a
        // 3-byte codepoint, so the old `&s[i..i+2]` slice panicked mid-codepoint
        // ("byte index 2 is not a char boundary"). Must now return MalformedHex.
        let err = decode_hex("public_key", "\u{20AC}\u{20AC}").expect_err("must reject, not panic");
        assert!(
            matches!(
                err,
                VerifyError::MalformedHex {
                    field: "public_key",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn decode_hex_rejects_odd_length() {
        let err = decode_hex("signature", "abc").expect_err("odd length must error");
        assert!(matches!(err, VerifyError::MalformedHex { reason, .. } if reason == "odd length"));
    }

    #[test]
    fn decode_hex_rejects_non_hex_ascii() {
        let err = decode_hex("public_key", "zz").expect_err("non-hex ascii must error");
        assert!(matches!(
            err,
            VerifyError::MalformedHex {
                field: "public_key",
                ..
            }
        ));
        // Whitespace / punctuation are not hex either.
        assert!(decode_hex("public_key", "  ").is_err());
        assert!(decode_hex("public_key", "0x").is_err());
    }

    #[test]
    fn decode_hex_rejects_a_single_multibyte_char() {
        // A lone 2-byte codepoint (`¢` = U+00A2, bytes 0xC2 0xA2) is even-length too.
        let err = decode_hex("public_key", "\u{00A2}").expect_err("must reject, not panic");
        assert!(
            matches!(err, VerifyError::MalformedHex { .. }),
            "got {err:?}"
        );
    }

    // ── verify(): the actual security boundary — never panics on hostile JSON ──

    /// A `SignedVerdict` that verifies, as the base for hostile mutations.
    fn good_signed() -> SignedVerdict {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        sign(
            fixture::passing_body(),
            &key,
            "2026-06-11T18:05:54Z".to_string(),
            SignerKind::ServiceAccount,
        )
    }

    #[test]
    fn verify_rejects_multibyte_public_key_via_json_roundtrip() {
        // The threat model exactly: a SignedVerdict deserialized from attacker JSON
        // whose public_key is multi-byte UTF-8 of even byte length. Must return a
        // VerifyError (MalformedHex), never panic at the slice.
        let mut signed = good_signed();
        signed.public_key = "\u{20AC}\u{20AC}".to_string(); // 6 bytes, even
        let json = serde_json::to_string(&signed).unwrap();
        let parsed: SignedVerdict = serde_json::from_str(&json).unwrap();
        let err = parsed
            .verify()
            .expect_err("hostile public_key must fail, not panic");
        assert!(
            matches!(
                err,
                VerifyError::MalformedHex {
                    field: "public_key",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn verify_rejects_multibyte_signature_via_json_roundtrip() {
        let mut signed = good_signed();
        signed.signature = "\u{20AC}\u{20AC}".to_string();
        let json = serde_json::to_string(&signed).unwrap();
        let parsed: SignedVerdict = serde_json::from_str(&json).unwrap();
        // public_key is still valid here, so decode_hex on `signature` is the path.
        let err = parsed
            .verify()
            .expect_err("hostile signature must fail, not panic");
        assert!(
            matches!(
                err,
                VerifyError::MalformedHex {
                    field: "signature",
                    ..
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn verify_rejects_odd_length_public_key() {
        let mut signed = good_signed();
        signed.public_key.pop(); // make it odd-length valid-hex-charset
        let err = signed.verify().expect_err("odd-length must fail");
        assert!(matches!(
            err,
            VerifyError::MalformedHex {
                field: "public_key",
                ..
            }
        ));
    }

    #[test]
    fn verify_rejects_non_hex_ascii_public_key() {
        let mut signed = good_signed();
        // 64 ascii chars but not hex (right length, wrong charset).
        signed.public_key = "z".repeat(64);
        let err = signed.verify().expect_err("non-hex must fail");
        assert!(matches!(
            err,
            VerifyError::MalformedHex {
                field: "public_key",
                ..
            }
        ));
    }

    #[test]
    fn verify_rejects_wrong_length_valid_hex_public_key() {
        // Valid hex, even length, but not 32 bytes ⇒ InvalidPublicKey (decode
        // succeeds, the try_into to [u8;32] fails — still no panic).
        let mut signed = good_signed();
        signed.public_key = "00".repeat(31); // 31 bytes, valid hex
        let err = signed.verify().expect_err("wrong-length key must fail");
        assert!(
            matches!(err, VerifyError::InvalidPublicKey(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn verify_rejects_wrong_length_valid_hex_signature() {
        let mut signed = good_signed();
        signed.signature = "00".repeat(63); // 63 bytes, valid hex (sig is 64)
        let err = signed.verify().expect_err("wrong-length sig must fail");
        assert!(
            matches!(err, VerifyError::InvalidSignatureEncoding(_)),
            "got {err:?}"
        );
    }
}
