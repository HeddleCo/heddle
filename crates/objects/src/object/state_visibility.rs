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
            VisibilityTier::Restricted { scope_label } => {
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
            _ => Ok(()),
        }
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

    /// The effective record: the most recent declaration by `declared_at`,
    /// breaking ties by **append order** (the record's position in `records`).
    /// Pure over the persisted records, never wall-clock.
    ///
    /// Records accrete in commit order — each locally-committed declaration is
    /// pushed under the repo write lock, and its `declared_at` is stamped inside
    /// that same critical section (heddle#317 / PR #529 P1) — so the append
    /// index is a commit-consistent ordering key. When two records share an
    /// identical `declared_at` (clock resolution can collide for ops serialized
    /// in the same tick under the lock), the later-appended one wins, i.e. the
    /// **last committed** declaration. This keeps the effective record aligned
    /// with the serialized commit / oplog-append order rather than resolving
    /// ambiguously. `declared_at` stays the primary key, so cross-host records
    /// (which arrive carrying their originating host's timestamp) still order by
    /// `declared_at` exactly as before — the index only decides exact ties.
    pub fn latest(&self) -> Option<&StateVisibility> {
        self.records
            .iter()
            .enumerate()
            .max_by(|(ia, a), (ib, b)| a.declared_at.cmp(&b.declared_at).then_with(|| ia.cmp(ib)))
            .map(|(_, record)| record)
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
    fn latest_picks_the_most_recent() {
        let early = record(VisibilityTier::Internal);
        let late = StateVisibility {
            declared_at: Utc.with_ymd_and_hms(2026, 6, 2, 9, 0, 0).unwrap(),
            tier: VisibilityTier::Public,
            ..record(VisibilityTier::Public)
        };
        let blob = StateVisibilityBlob::new(vec![early, late.clone()]);
        assert_eq!(blob.latest().unwrap(), &late);
    }

    #[test]
    fn equal_timestamp_visibility_records_resolve_deterministically() {
        // Two records stamped in the SAME tick (identical declared_at) must
        // resolve to the LATER-appended one — append order is commit order, so
        // the last-committed declaration wins, never an ambiguous pick. Models
        // two ops serialized under the lock whose clock didn't advance between
        // them.
        let same = Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap();
        let first = StateVisibility {
            declared_at: same,
            ..record(VisibilityTier::Internal)
        };
        let second = StateVisibility {
            declared_at: same,
            ..record(VisibilityTier::TeamScoped {
                team_id: "infra".into(),
            })
        };
        let blob = StateVisibilityBlob::new(vec![first, second.clone()]);
        assert_eq!(
            blob.latest().unwrap(),
            &second,
            "an equal-timestamp tie must resolve to the last-appended (last-committed) record"
        );
    }

    #[test]
    fn empty_blob_has_no_record() {
        assert!(!StateVisibilityBlob::empty().has_record());
    }
}
