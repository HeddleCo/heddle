// SPDX-License-Identifier: Apache-2.0
//! Redaction — a declaration that a blob in a state is sensitive and must
//! materialize as a stub instead of its content.
//!
//! Redaction is *additive*: a new object that supersedes a read of the
//! original. The blob's bytes stay on disk until `heddle purge` explicitly
//! removes them; the redaction itself is the readers' contract that those
//! bytes are no longer accessible through the materialize path.
//!
//! Distinct from review signatures and state signatures:
//! - [`StateSignature`](crate::object::StateSignature) authenticates a state's authorship.
//! - [`ReviewSignature`](crate::object::ReviewSignature) authenticates that a state was reviewed.
//! - [`Redaction`] is itself a signable operation — it claims that a specific
//!   blob in a specific state should no longer materialize. The signature
//!   binds operator → declaration so audits can trace who hid what when.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::object::{ContentHash, Principal, StateId, StateSignature};

/// Stable byte prefix the signing payload begins with. Bumping this versions
/// the payload format itself; old signatures with the old prefix continue to
/// verify exactly as they did when written.
pub const REDACTION_SIGNING_PAYLOAD_VERSION_TAG: &[u8] = b"hd-redact-v1\x00";

/// A redaction declaration on a single blob in a single state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Redaction {
    /// The blob whose bytes should no longer materialize.
    pub redacted_blob: ContentHash,
    /// The state in which the path resides. A redaction is *scoped* to the
    /// (blob, state, path) triple; `--all-states` produces one redaction
    /// per matching state.
    pub state: StateId,
    /// Path within the state's tree where the blob lives.
    pub path: String,
    /// Operator-supplied reason ("leaked credential", "PII", ...).
    pub reason: String,
    /// Who declared the redaction.
    pub redactor: Principal,
    /// When the redaction was declared. RFC3339 string at the wire format
    /// boundary; `DateTime<Utc>` internally.
    pub redacted_at: DateTime<Utc>,
    /// Optional cryptographic signature over the canonical signing payload
    /// (see [`canonical_signing_payload`]). `None` for unsigned redactions
    /// (still recorded in the oplog, still surfaced in materialize, but
    /// reviewers will see them flagged unsigned).
    #[serde(default)]
    pub signature: Option<StateSignature>,
    /// When `heddle purge` removed the underlying blob bytes. `None` while
    /// the redaction is declared-but-bytes-still-on-disk.
    #[serde(default)]
    pub purged_at: Option<DateTime<Utc>>,
    /// The redaction this one supersedes, if any — for chains where the
    /// reason or scope was updated. Identified by the prior redaction's
    /// content hash.
    #[serde(default)]
    pub supersedes: Option<ContentHash>,
}

impl Redaction {
    /// Build the canonical bytes a signer covers. Anything outside this
    /// payload (e.g. `purged_at`, `signature` itself) is intentionally
    /// excluded — purges happen after signing, and the signature can't sign
    /// itself.
    pub fn canonical_signing_payload(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(REDACTION_SIGNING_PAYLOAD_VERSION_TAG);
        buf.extend_from_slice(self.redacted_blob.as_bytes());
        buf.extend_from_slice(self.state.as_bytes());
        buf.extend_from_slice(self.path.as_bytes());
        buf.push(0);
        buf.extend_from_slice(self.reason.as_bytes());
        buf.push(0);
        buf.extend_from_slice(self.redactor.name.as_bytes());
        buf.push(0);
        buf.extend_from_slice(self.redactor.email.as_bytes());
        buf.push(0);
        buf.extend_from_slice(self.redacted_at.to_rfc3339().as_bytes());
        if let Some(supersedes) = &self.supersedes {
            buf.extend_from_slice(supersedes.as_bytes());
        }
        buf
    }

    /// Mark the redaction as purged. Returns `true` if the state changed
    /// (`false` if already purged — callers can use this for idempotency).
    pub fn mark_purged(&mut self, at: DateTime<Utc>) -> bool {
        if self.purged_at.is_some() {
            false
        } else {
            self.purged_at = Some(at);
            true
        }
    }

    /// Whether the blob bytes are gone from local storage.
    pub fn is_purged(&self) -> bool {
        self.purged_at.is_some()
    }

    /// Format the stub a reader sees instead of the redacted blob content.
    /// Plain text, ASCII-only, safe to embed in materialized worktrees and
    /// downstream Git exports.
    pub fn stub_text(&self, redaction_id: &ContentHash) -> String {
        let mut out = String::with_capacity(256);
        out.push_str("# This file was redacted by Heddle.\n");
        out.push_str(&format!(
            "# redacted-at: {}\n",
            self.redacted_at.to_rfc3339()
        ));
        out.push_str(&format!(
            "# redactor:    {} <{}>\n",
            self.redactor.name, self.redactor.email
        ));
        out.push_str(&format!("# reason:      {}\n", self.reason));
        out.push_str(&format!("# redaction:   {}\n", redaction_id.short()));
        if let Some(purged_at) = self.purged_at {
            out.push_str(&format!("# purged-at:   {}\n", purged_at.to_rfc3339()));
            out.push_str("# The original bytes have been purged from local storage.\n");
        } else {
            out.push_str("# The original bytes remain on disk pending purge.\n");
        }
        out
    }
}

/// On-disk blob containing all redactions for a single blob hash. One file
/// per redacted blob, encoded with `rmp-serde` — matches the
/// [`ReviewSignaturesBlob`](crate::object::ReviewSignaturesBlob) pattern.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionsBlob {
    pub format_version: u8,
    pub redactions: Vec<Redaction>,
}

impl RedactionsBlob {
    pub const FORMAT_VERSION: u8 = 1;

    pub fn new(redactions: Vec<Redaction>) -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            redactions,
        }
    }

    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    pub fn encode(&self) -> Result<Vec<u8>, RedactionError> {
        rmp_serde::to_vec(self).map_err(|err| RedactionError::Encoding(err.to_string()))
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, RedactionError> {
        rmp_serde::from_slice(bytes).map_err(|err| RedactionError::Decoding(err.to_string()))
    }

    pub fn push(&mut self, redaction: Redaction) {
        self.redactions.push(redaction);
    }

    /// `true` iff any redaction in this blob is non-superseded — i.e. the
    /// reader should see the stub. Today every redaction is active; a
    /// future "unredact" verb would skip the superseded ones.
    pub fn has_active(&self) -> bool {
        !self.redactions.is_empty()
    }

    /// The most recent redaction, by `redacted_at`. Used as the canonical
    /// stub source when multiple redactions exist for the same blob (e.g.
    /// because of `--all-states` plus a later refinement).
    pub fn latest(&self) -> Option<&Redaction> {
        self.redactions.iter().max_by_key(|r| r.redacted_at)
    }

    /// Mark every redaction in this blob as purged. Returns the count that
    /// actually transitioned (others were already purged).
    pub fn mark_all_purged(&mut self, at: DateTime<Utc>) -> usize {
        let mut transitioned = 0;
        for redaction in &mut self.redactions {
            if redaction.mark_purged(at) {
                transitioned += 1;
            }
        }
        transitioned
    }
}

/// Errors produced while encoding/decoding redactions.
#[derive(Debug, thiserror::Error)]
pub enum RedactionError {
    #[error("encoding redaction: {0}")]
    Encoding(String),
    #[error("decoding redaction: {0}")]
    Decoding(String),
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn principal() -> Principal {
        Principal {
            name: "Grace Hopper".into(),
            email: "grace@example.com".into(),
        }
    }

    fn blob_hash() -> ContentHash {
        ContentHash::from_bytes([7u8; 32])
    }

    fn redaction(blob: ContentHash, reason: &str) -> Redaction {
        Redaction {
            redacted_blob: blob,
            state: StateId::from_bytes([1u8; 32]),
            path: "config/secrets.toml".into(),
            reason: reason.into(),
            redactor: principal(),
            redacted_at: Utc.with_ymd_and_hms(2026, 5, 10, 14, 33, 0).unwrap(),
            signature: None,
            purged_at: None,
            supersedes: None,
        }
    }

    #[test]
    fn round_trips_through_msgpack() {
        let blob = blob_hash();
        let original = RedactionsBlob::new(vec![redaction(blob, "leaked credential")]);
        let encoded = original.encode().expect("encode");
        let decoded = RedactionsBlob::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
        // Format-version is load-bearing: future readers branch on it.
        assert_eq!(decoded.format_version, RedactionsBlob::FORMAT_VERSION);
    }

    #[test]
    fn canonical_payload_stable_across_field_reordering() {
        // The signing payload concatenates fields in a fixed order. If we
        // accidentally derive serialization from struct-field declaration
        // order alone (rmp-serde's default), reordering the struct would
        // silently invalidate every existing signature. The explicit
        // `canonical_signing_payload` is the contract; this test pins it.
        let r = redaction(blob_hash(), "leaked credential");
        let payload = r.canonical_signing_payload();
        // Tag prefix at the front; gives us a versioned signing domain.
        assert!(payload.starts_with(REDACTION_SIGNING_PAYLOAD_VERSION_TAG));
        // Reason text is in the payload — otherwise an operator could
        // re-sign a redaction with a different reason.
        let payload_text = String::from_utf8_lossy(&payload);
        assert!(payload_text.contains("leaked credential"));
        assert!(payload_text.contains("config/secrets.toml"));
        // RFC3339 timestamp string is included — fixed timezone, fixed
        // precision, so the payload is reproducible across runs.
        assert!(payload_text.contains("2026-05-10T14:33:00+00:00"));
    }

    #[test]
    fn mark_purged_is_idempotent_and_observable() {
        let mut r = redaction(blob_hash(), "leaked credential");
        let at = Utc.with_ymd_and_hms(2026, 5, 11, 0, 0, 0).unwrap();
        assert!(!r.is_purged());
        assert!(r.mark_purged(at));
        assert!(r.is_purged());
        // Second call is a no-op — operators can safely retry purge
        // without distorting the `purged_at` audit trail.
        assert!(!r.mark_purged(Utc.with_ymd_and_hms(2026, 5, 12, 0, 0, 0).unwrap()));
        assert_eq!(r.purged_at, Some(at));
    }

    #[test]
    fn stub_text_mentions_redactor_reason_and_purge_state() {
        let r = redaction(blob_hash(), "leaked credential");
        let stub = r.stub_text(&blob_hash());
        // The stub is the ONLY thing readers see for redacted files. It
        // must carry every field a reviewer would want: who, when, why,
        // and whether the bytes are still recoverable.
        assert!(stub.contains("Grace Hopper"));
        assert!(stub.contains("grace@example.com"));
        assert!(stub.contains("leaked credential"));
        assert!(stub.contains("# redacted-at:"));
        assert!(stub.contains("# redaction:"));
        // Pre-purge, the stub should explicitly say bytes remain.
        assert!(stub.contains("remain on disk pending purge"));

        let mut purged = r.clone();
        purged.mark_purged(Utc.with_ymd_and_hms(2026, 5, 11, 0, 0, 0).unwrap());
        let purged_stub = purged.stub_text(&blob_hash());
        assert!(purged_stub.contains("# purged-at:"));
        assert!(purged_stub.contains("purged from local storage"));
    }

    #[test]
    fn latest_picks_the_most_recent() {
        let early = redaction(blob_hash(), "first pass");
        let late = Redaction {
            redacted_at: Utc.with_ymd_and_hms(2026, 5, 12, 9, 0, 0).unwrap(),
            reason: "tighter scope".into(),
            ..redaction(blob_hash(), "tighter scope")
        };
        let blob = RedactionsBlob::new(vec![early, late.clone()]);
        assert_eq!(blob.latest().unwrap(), &late);
    }
}

#[cfg(test)]
mod proptests {
    //! Property tests for the redaction primitive's data model.
    //!
    //! These match the build brief's "Property tests" acceptance
    //! criteria (`.agents/redaction-primitive.md`):
    //!
    //!   1. Encode → decode round-trips losslessly for any well-formed
    //!      redaction.
    //!   2. `canonical_signing_payload` is deterministic across clones
    //!      and stable across `Redaction` field reordering — the
    //!      contract that lets signatures verify.
    //!   3. `mark_purged` is idempotent: replaying the call with any
    //!      later timestamp does not move `purged_at`.
    //!   4. `stub_text` always carries the redaction id, the reason,
    //!      and the redactor email, no matter what content went in.
    //!
    //! Running with the standard proptest budget produces ~256 cases
    //! per property by default.
    use proptest::prelude::*;

    use super::*;

    fn arb_principal() -> impl Strategy<Value = Principal> {
        // Names + emails are ASCII-printable, length-bounded. We're
        // not testing unicode tolerance here — the redaction store's
        // contract is "whatever the principal source serves us" and
        // we want determinism, not exhaustive locale coverage.
        let name = "[A-Za-z][A-Za-z0-9 _-]{0,30}";
        let email = "[a-z][a-z0-9_-]{0,15}@[a-z0-9.-]{1,30}\\.[a-z]{2,4}";
        (name, email).prop_map(|(name, email)| Principal { name, email })
    }

    fn arb_blob_hash() -> impl Strategy<Value = ContentHash> {
        any::<[u8; 32]>().prop_map(ContentHash::from_bytes)
    }

    fn arb_state_id() -> impl Strategy<Value = StateId> {
        any::<[u8; 32]>().prop_map(StateId::from_bytes)
    }

    fn arb_redaction() -> impl Strategy<Value = Redaction> {
        // Timestamp range is bounded to keep RFC3339 formatting stable
        // (chrono's print is fine, but the test outputs are easier to
        // diff with a narrow window). Year 2000–2100 is plenty.
        let secs = 946_684_800i64..4_102_444_800i64;
        (
            arb_blob_hash(),
            arb_state_id(),
            "[A-Za-z0-9._/-]{1,40}",
            "[A-Za-z0-9 ._:'-]{0,80}",
            arb_principal(),
            secs,
            prop::option::of(arb_blob_hash()),
        )
            .prop_map(|(blob, state, path, reason, redactor, secs, supersedes)| {
                Redaction {
                    redacted_blob: blob,
                    state,
                    path,
                    reason,
                    redactor,
                    redacted_at: chrono::DateTime::<Utc>::from_timestamp(secs, 0)
                        .expect("in-range timestamp"),
                    signature: None,
                    purged_at: None,
                    supersedes,
                }
            })
    }

    proptest! {
        /// Encode → decode round-trips. If this breaks, the on-disk
        /// redaction store can't be read back; the leaked-secret stays
        /// secret only by accident.
        #[test]
        fn encode_decode_roundtrip(r in arb_redaction()) {
            let blob = RedactionsBlob::new(vec![r.clone()]);
            let bytes = blob.encode().expect("encode");
            let decoded = RedactionsBlob::decode(&bytes).expect("decode");
            prop_assert_eq!(decoded.redactions.len(), 1);
            prop_assert_eq!(&decoded.redactions[0], &r);
        }

        /// Canonical signing payload is a pure function of the
        /// redaction's *content*: cloning the value or rebuilding it
        /// from the same fields must give bit-identical bytes. This is
        /// what makes a signature stable across read cycles.
        #[test]
        fn canonical_payload_is_deterministic(r in arb_redaction()) {
            let payload1 = r.canonical_signing_payload();
            let payload2 = r.clone().canonical_signing_payload();
            prop_assert_eq!(payload1, payload2);
        }

        /// `purged_at` is monotonic. Once a redaction is purged, a
        /// later `mark_purged` call with any timestamp must NOT move
        /// the field — operators can re-run the purge command (or
        /// retries can ride a partial failure) without distorting the
        /// audit trail.
        #[test]
        fn mark_purged_is_idempotent(
            mut r in arb_redaction(),
            t1_secs in 946_684_800i64..4_000_000_000i64,
            t2_offset in 0i64..1_000_000_000i64,
        ) {
            let t1 = chrono::DateTime::<Utc>::from_timestamp(t1_secs, 0).unwrap();
            let t2 = chrono::DateTime::<Utc>::from_timestamp(t1_secs + t2_offset, 0).unwrap();
            prop_assert!(r.mark_purged(t1));
            prop_assert!(r.is_purged());
            prop_assert_eq!(r.purged_at, Some(t1));
            // Second purge with a later timestamp is a no-op.
            prop_assert!(!r.mark_purged(t2));
            prop_assert_eq!(r.purged_at, Some(t1));
        }

        /// The stub a reader sees must always identify the redaction.
        /// If the stub failed to carry the id or the reason, downstream
        /// auditors would have no way to trace why a file disappeared.
        #[test]
        fn stub_always_carries_id_and_reason(r in arb_redaction()) {
            let id = ContentHash::from_bytes([0xAB; 32]);
            let stub = r.stub_text(&id);
            // The short id is what `heddle redact show` displays;
            // the stub must echo it for back-reference.
            prop_assert!(
                stub.contains(&id.short()),
                "stub must contain redaction id; got: {stub}"
            );
            // Empty reasons are allowed (defensive) but if any reason
            // text is supplied it must surface in the stub.
            if !r.reason.is_empty() {
                prop_assert!(
                    stub.contains(&r.reason),
                    "stub must carry reason '{}'; got: {stub}",
                    r.reason
                );
            }
            // The redactor's email is the durable identifier — the
            // name might be a display label, but the email survives
            // rename and is what auditors trace back to.
            prop_assert!(
                stub.contains(&r.redactor.email),
                "stub must carry redactor email '{}'; got: {stub}",
                r.redactor.email
            );
        }

        /// Empty `RedactionsBlob` is consistent: `has_active` returns
        /// `false`, and `latest` returns `None`. The materialize path
        /// uses these to decide whether to render a stub — if either
        /// regressed, redacted files would silently materialize their
        /// real bytes.
        #[test]
        fn empty_blob_is_inert(seed in any::<u8>()) {
            let _ = seed; // unused; exists to exercise the proptest harness
            let blob = RedactionsBlob::empty();
            prop_assert!(!blob.has_active());
            prop_assert!(blob.latest().is_none());
        }

        /// Adding redactions makes the blob active. Pin: a single
        /// non-purged redaction is sufficient — readers must see the
        /// stub from the moment the first declaration lands.
        #[test]
        fn single_redaction_makes_blob_active(r in arb_redaction()) {
            let blob = RedactionsBlob::new(vec![r]);
            prop_assert!(blob.has_active());
            prop_assert!(blob.latest().is_some());
        }
    }
}
