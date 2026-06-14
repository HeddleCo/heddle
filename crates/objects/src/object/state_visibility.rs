// SPDX-License-Identifier: Apache-2.0
//! StateVisibility — a declaration that a state (commit) carries a
//! non-public audience tier.
//!
//! Modeled on [`Redaction`](crate::object::Redaction): an *additive*
//! sidecar record that lives outside the hashed `State` bytes, so changing a
//! state's tier never mutates the state or invalidates its signature. The
//! record is keyed by `ChangeId` (the state), not by a blob hash — commit
//! visibility is a per-state property, where redaction is per-blob.
//!
//! **Absence ≡ public.** A public resolution stays record-free: the public
//! tier is the default, and a state with no `StateVisibility` record is
//! served to every audience. Only resolutions more restrictive than public
//! are persisted, so the per-state sidecar is empty for the common case.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::object::{ChangeId, ContentHash, Principal, StateSignature, VisibilityTier};

/// Stable byte prefix the signing payload begins with. Bumping this versions
/// the payload format itself; old signatures with the old prefix continue to
/// verify exactly as they did when written.
pub const STATE_VISIBILITY_SIGNING_PAYLOAD_VERSION_TAG: &[u8] = b"hd-statevis-v1\x00";

/// A visibility-tier declaration on a single state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateVisibility {
    /// The state (commit) this tier applies to.
    pub state: ChangeId,
    /// The audience tier the state's content is served at.
    pub tier: VisibilityTier,
    /// When set, the host materializes a superseding public record at this
    /// instant (auto-promote). Advisory schedule only — the *effective*
    /// tier is always read from the persisted records, never recomputed
    /// from wall-clock at read time.
    #[serde(default)]
    pub embargo_until: Option<DateTime<Utc>>,
    /// Who declared the tier.
    pub declarer: Principal,
    /// When the tier was declared. RFC3339 at the wire boundary;
    /// `DateTime<Utc>` internally.
    pub declared_at: DateTime<Utc>,
    /// Optional cryptographic signature over the canonical signing payload
    /// (see [`canonical_signing_payload`](StateVisibility::canonical_signing_payload)).
    /// `None` for unsigned declarations.
    #[serde(default)]
    pub signature: Option<StateSignature>,
    /// The record this one supersedes, if any — promotion appends a
    /// superseding record rather than mutating a prior one. Identified by
    /// the prior record's content hash.
    #[serde(default)]
    pub supersedes: Option<ContentHash>,
}

impl StateVisibility {
    /// Build the canonical bytes a signer covers. The `signature` field is
    /// intentionally excluded (a signature can't sign itself).
    pub fn canonical_signing_payload(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(STATE_VISIBILITY_SIGNING_PAYLOAD_VERSION_TAG);
        buf.extend_from_slice(self.state.as_bytes());
        buf.extend_from_slice(self.tier.as_str().as_bytes());
        buf.push(0);
        match &self.tier {
            VisibilityTier::TeamScoped { team_id } => buf.extend_from_slice(team_id.as_bytes()),
            VisibilityTier::Restricted { scope_label }
            | VisibilityTier::Private { scope_label } => {
                buf.extend_from_slice(scope_label.as_bytes())
            }
            VisibilityTier::Public | VisibilityTier::Internal => {}
        }
        buf.push(0);
        if let Some(embargo_until) = &self.embargo_until {
            buf.extend_from_slice(embargo_until.to_rfc3339().as_bytes());
        }
        buf.push(0);
        buf.extend_from_slice(self.declarer.name.as_bytes());
        buf.push(0);
        buf.extend_from_slice(self.declarer.email.as_bytes());
        buf.push(0);
        buf.extend_from_slice(self.declared_at.to_rfc3339().as_bytes());
        if let Some(supersedes) = &self.supersedes {
            buf.extend_from_slice(supersedes.as_bytes());
        }
        buf
    }

    /// Per-item validation hook required by [`versioned_msgpack_blob!`].
    /// A `TeamScoped`/`Restricted` tier must carry a non-empty label —
    /// an empty label is meaningless and would silently widen the
    /// audience to "any team / any restricted scope".
    pub fn validate(&self) -> Result<(), StateVisibilityError> {
        match &self.tier {
            VisibilityTier::TeamScoped { team_id } if team_id.trim().is_empty() => {
                Err(StateVisibilityError::EmptyTierLabel("team_scoped"))
            }
            VisibilityTier::Restricted { scope_label } if scope_label.trim().is_empty() => {
                Err(StateVisibilityError::EmptyTierLabel("restricted"))
            }
            VisibilityTier::Private { scope_label } if scope_label.trim().is_empty() => {
                Err(StateVisibilityError::EmptyTierLabel("private"))
            }
            _ => Ok(()),
        }
    }

    /// Content-addressed id of this record: `blake3` over the canonical
    /// rmp-encoded bytes of a one-element [`StateVisibilityBlob`]. This is the
    /// id a superseding record stores in its [`supersedes`](Self::supersedes)
    /// pointer, and the key [`StateVisibilityBlob::latest`] resolves the
    /// supersede chain by — so the write path (which sets `supersedes` from the
    /// under-lock head) and the read path (which walks the chain) agree by
    /// construction. The id covers the single record's bytes embedded in the
    /// versioned envelope, so it stays stable across schema additions that only
    /// extend the container.
    pub fn content_hash(&self) -> Result<ContentHash, StateVisibilityError> {
        let single = StateVisibilityBlob::new(vec![self.clone()]);
        let bytes = single.encode()?;
        Ok(ContentHash::from_bytes(*blake3::hash(&bytes).as_bytes()))
    }
}

/// On-disk blob containing all visibility records for a single state. One
/// file per state, encoded with `rmp-serde` — mirrors the
/// [`RedactionsBlob`](crate::object::RedactionsBlob) sidecar pattern.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateVisibilityBlob {
    pub format_version: u8,
    pub records: Vec<StateVisibility>,
}

// `new` / `encode` / `decode` / `validate` + `FORMAT_VERSION`. `decode`
// rejects any blob whose `format_version` isn't the current one.
versioned_msgpack_blob! {
    blob: StateVisibilityBlob,
    item: StateVisibility,
    field: records,
    error: StateVisibilityError,
    codec_err: Codec,
    version: 1,
}

impl StateVisibilityBlob {
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    pub fn push(&mut self, record: StateVisibility) {
        self.records.push(record);
    }

    /// `true` iff this state carries any visibility record — i.e. it is not
    /// public-by-absence. Used by the sidecar's `has_visibility_for_state`.
    pub fn has_record(&self) -> bool {
        !self.records.is_empty()
    }

    /// The effective record: the **head of the supersede DAG** — the record in
    /// this blob that no other record supersedes — resolved purely from the
    /// records' content-intrinsic [`supersedes`](StateVisibility::supersedes)
    /// pointers, **never** from wall-clock `declared_at`.
    ///
    /// Each locally-committed declaration links onto the prior head it read
    /// under the repo write lock — its `supersedes` points at that head's
    /// content hash (heddle#317 / PR #529 P1) — so for serialized local writes
    /// the chain is linear and the head is the **last-committed** record,
    /// independent of clock skew or whatever order the timestamps happen to
    /// carry. `declared_at` is an audit/display field only. This also fixes a
    /// latent cross-host bug: wall-clock cannot order records replicated across
    /// hosts whose clocks disagree, but the content-hash chain can.
    ///
    /// **Fork tie-break (concurrent / cross-host).** Two records can supersede
    /// the *same* prior with neither superseding the other — a genuine
    /// concurrent fork, e.g. two hosts that diverged. Both are heads. To make
    /// every replica resolve the SAME effective record without consulting
    /// wall-clock, the tie is broken by the **lexicographically greatest record
    /// content hash** — a content-intrinsic, host-independent key. Cycles are
    /// cryptographically unconstructable (a record's hash covers its
    /// `supersedes` pointer, so no record can supersede one minted after it), so
    /// a non-empty blob always has at least one head.
    pub fn latest(&self) -> Result<Option<&StateVisibility>, StateVisibilityError> {
        // Every content hash referenced by some record's `supersedes` pointer.
        // A record whose own hash appears here has been superseded — it is not
        // the head.
        let superseded: std::collections::HashSet<ContentHash> =
            self.records.iter().filter_map(|r| r.supersedes).collect();

        let mut head: Option<(&StateVisibility, ContentHash)> = None;
        for record in &self.records {
            let hash = record.content_hash()?;
            if superseded.contains(&hash) {
                continue;
            }
            // Among multiple heads (a fork), keep the greatest content hash so
            // the pick is deterministic and host-independent.
            let take = match &head {
                Some((_, best)) => hash > *best,
                None => true,
            };
            if take {
                head = Some((record, hash));
            }
        }
        Ok(head.map(|(record, _)| record))
    }
}

/// Errors produced while encoding/decoding/validating state visibility.
#[derive(Debug, thiserror::Error)]
pub enum StateVisibilityError {
    #[error("unsupported state-visibility format version {0}")]
    UnsupportedVersion(u8),
    #[error("state-visibility codec error: {0}")]
    Codec(String),
    #[error("{0} tier requires a non-empty label")]
    EmptyTierLabel(&'static str),
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

    fn record(tier: VisibilityTier) -> StateVisibility {
        StateVisibility {
            state: ChangeId::from_bytes([3u8; 16]),
            tier,
            embargo_until: None,
            declarer: principal(),
            declared_at: Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap(),
            signature: None,
            supersedes: None,
        }
    }

    #[test]
    fn round_trips_through_msgpack() {
        let original = StateVisibilityBlob::new(vec![record(VisibilityTier::Restricted {
            scope_label: "security-embargo".into(),
        })]);
        let encoded = original.encode().expect("encode");
        let decoded = StateVisibilityBlob::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
        // Format-version is load-bearing: future readers branch on it.
        assert_eq!(decoded.format_version, StateVisibilityBlob::FORMAT_VERSION);
    }

    #[test]
    fn round_trips_with_embargo_and_supersedes() {
        let mut r = record(VisibilityTier::TeamScoped {
            team_id: "infra".into(),
        });
        r.embargo_until = Some(Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap());
        r.supersedes = Some(ContentHash::from_bytes([9u8; 32]));
        let original = StateVisibilityBlob::new(vec![r]);
        let encoded = original.encode().expect("encode");
        let decoded = StateVisibilityBlob::decode(&encoded).expect("decode");
        assert_eq!(decoded, original);
    }

    #[test]
    fn decode_rejects_wrong_version() {
        // Hand-encode a blob whose format_version is not the current one;
        // decode must reject it via the macro's version-check prologue.
        let bad = StateVisibilityBlob {
            format_version: StateVisibilityBlob::FORMAT_VERSION + 7,
            records: vec![record(VisibilityTier::Internal)],
        };
        let bytes = rmp_serde::to_vec(&bad).expect("raw encode");
        let err = StateVisibilityBlob::decode(&bytes).expect_err("must reject wrong version");
        assert!(matches!(
            err,
            StateVisibilityError::UnsupportedVersion(v) if v == StateVisibilityBlob::FORMAT_VERSION + 7
        ));
    }

    #[test]
    fn validate_rejects_empty_label() {
        let blob = StateVisibilityBlob::new(vec![record(VisibilityTier::Restricted {
            scope_label: "  ".into(),
        })]);
        assert!(matches!(
            blob.validate(),
            Err(StateVisibilityError::EmptyTierLabel("restricted"))
        ));
    }

    #[test]
    fn canonical_payload_is_versioned_and_carries_tier() {
        let r = record(VisibilityTier::Restricted {
            scope_label: "security-embargo".into(),
        });
        let payload = r.canonical_signing_payload();
        assert!(payload.starts_with(STATE_VISIBILITY_SIGNING_PAYLOAD_VERSION_TAG));
        let text = String::from_utf8_lossy(&payload);
        assert!(text.contains("restricted"));
        assert!(text.contains("security-embargo"));
        assert!(text.contains("grace@example.com"));
    }

    #[test]
    fn latest_resolves_the_supersede_chain_head() {
        // The effective record is the head of the supersede chain — the record
        // no other record supersedes — not the most recent by wall-clock.
        let early = record(VisibilityTier::Internal);
        let early_id = early.content_hash().unwrap();
        let late = StateVisibility {
            declared_at: Utc.with_ymd_and_hms(2026, 6, 2, 9, 0, 0).unwrap(),
            supersedes: Some(early_id),
            ..record(VisibilityTier::Public)
        };
        let blob = StateVisibilityBlob::new(vec![early, late.clone()]);
        assert_eq!(blob.latest().unwrap().unwrap(), &late);
    }

    #[test]
    fn latest_ignores_wall_clock_declared_at() {
        // A record with a strictly LATER declared_at but EARLIER in the
        // supersede chain must NOT be selected — the chain head wins regardless
        // of wall-clock. This is the bug class the redesign closes: selection is
        // content-intrinsic, so it can't be skewed by timestamps (or clock
        // disagreement across hosts).
        let head_tier = VisibilityTier::TeamScoped {
            team_id: "infra".into(),
        };
        // The genesis carries the LATEST timestamp...
        let early_in_chain = StateVisibility {
            declared_at: Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap(),
            ..record(VisibilityTier::Internal)
        };
        let early_id = early_in_chain.content_hash().unwrap();
        // ...the chain head supersedes it but carries an EARLIER timestamp.
        let head = StateVisibility {
            declared_at: Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap(),
            supersedes: Some(early_id),
            ..record(head_tier.clone())
        };
        let blob = StateVisibilityBlob::new(vec![early_in_chain, head.clone()]);
        let latest = blob.latest().unwrap().unwrap();
        assert_eq!(latest, &head);
        assert_eq!(
            latest.tier, head_tier,
            "the chain head wins even though its declared_at is the earlier of the two"
        );
    }

    #[test]
    fn concurrent_fork_resolves_deterministically() {
        // Two records supersede the SAME prior with neither superseding the
        // other — a genuine concurrent fork (e.g. two hosts). latest() must pick
        // the SAME head on every replica via the content-intrinsic tie-break
        // (greatest content hash), never wall-clock and never input order.
        let genesis = record(VisibilityTier::Internal);
        let genesis_id = genesis.content_hash().unwrap();
        let fork_a = StateVisibility {
            declared_at: Utc.with_ymd_and_hms(2026, 6, 2, 9, 0, 0).unwrap(),
            supersedes: Some(genesis_id),
            ..record(VisibilityTier::TeamScoped {
                team_id: "host-a".into(),
            })
        };
        let fork_b = StateVisibility {
            declared_at: Utc.with_ymd_and_hms(2026, 6, 3, 9, 0, 0).unwrap(),
            supersedes: Some(genesis_id),
            ..record(VisibilityTier::TeamScoped {
                team_id: "host-b".into(),
            })
        };
        // The deterministic winner is the head with the greater content hash.
        let expected = if fork_a.content_hash().unwrap() > fork_b.content_hash().unwrap() {
            fork_a.clone()
        } else {
            fork_b.clone()
        };
        // Resolved identically regardless of the order the records appear in.
        let blob1 = StateVisibilityBlob::new(vec![genesis.clone(), fork_a.clone(), fork_b.clone()]);
        let blob2 = StateVisibilityBlob::new(vec![genesis, fork_b, fork_a]);
        assert_eq!(blob1.latest().unwrap().unwrap(), &expected);
        assert_eq!(
            blob2.latest().unwrap().unwrap(),
            &expected,
            "the fork must resolve to the same head independent of record order"
        );
    }

    #[test]
    fn empty_blob_has_no_record() {
        assert!(!StateVisibilityBlob::empty().has_record());
    }
}
