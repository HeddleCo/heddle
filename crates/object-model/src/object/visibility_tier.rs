// SPDX-License-Identifier: Apache-2.0
//! Shared audience-tier vocabulary.
//!
//! [`VisibilityTier`] is the single content-side visibility vocabulary used
//! across annotations, discussions, and per-state commit visibility. The
//! *reader's* tier (who is asking) is [`AudienceTier`]; this enum is the
//! *content's* tier (who the content is for). The who-sees-what mapping
//! between the two lives in [`visible`](super::visible), beside both
//! vocabularies.
//!
//! `Public` is the default — it matches the pre-unification behavior where
//! every annotation was effectively public, so legacy data on disk decodes
//! unchanged.

use serde::{Deserialize, Serialize};

/// Content-side visibility tier. Shared by annotations, discussions, and
/// states so the per-commit visibility tiers and annotation/discussion
/// visibility draw from one vocabulary rather than parallel enums.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VisibilityTier {
    #[default]
    Public,
    Internal,
    TeamScoped {
        team_id: String,
    },
    Restricted {
        scope_label: String,
    },
    /// The strictest tier: withheld from **every** audience — including the
    /// otherwise all-seeing `Internal` audience — except the one holder of
    /// the matching `Restricted(scope_label)`. Used for embargoed per-state
    /// commit visibility, where even internal callers must not see the
    /// content. The who-sees-what arm lives in `visible`, placed above the
    /// `(_, Internal) => true` arm so the embargo holds.
    Private {
        scope_label: String,
    },
}

impl VisibilityTier {
    /// Whether every reader of this tier could also read the source tier.
    /// Labels are separate audiences, so a higher rank alone cannot prove
    /// narrowing. This follows the audience sets defined by `visible`.
    pub fn is_no_more_visible_than(&self, source: &Self) -> bool {
        self == source
            || matches!(source, Self::Public)
            || matches!((self, source), (Self::TeamScoped { .. }, Self::Internal))
            || matches!(
                (self, source),
                (Self::Private { scope_label: target }, Self::Restricted { scope_label: source })
                    if target == source
            )
    }

    /// A hidden embargo blocks descendants, even when a descendant's own tier
    /// is visible. Internal and TeamScoped restrict only their own state.
    pub fn is_embargo(&self) -> bool {
        matches!(self, Self::Private { .. } | Self::Restricted { .. })
    }

    /// Stable wire/storage token for the tier discriminant. The labelled
    /// variants collapse to their kind name here; the label travels in a
    /// separate field. Shared by the discussion RPC vocabulary and the
    /// state-visibility signing payload, so it must stay stable.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::TeamScoped { .. } => "team_scoped",
            Self::Restricted { .. } => "restricted",
            Self::Private { .. } => "private",
        }
    }

    /// Restrictiveness ordering used by the `visibility promote` monotonicity
    /// check (heddle#317). **Lower rank = LESS restrictive** (the tier reaches a
    /// broader audience):
    ///
    /// | tier         | rank | audience reach                              |
    /// |--------------|------|---------------------------------------------|
    /// | `Public`     | 0    | every audience (least restrictive)          |
    /// | `Internal`   | 1    | the workspace-internal set (+ every team)   |
    /// | `TeamScoped` | 2    | one named team                              |
    /// | `Restricted` | 3    | one named scope label                        |
    /// | `Private`    | 4    | only the matching scope holder (most restrictive, even `Internal` is excluded) |
    ///
    /// This is the *defined* total order for "less restrictive", consistent with
    /// spike #266 §5.2 (`Internal` content is one of the least-restrictive
    /// values; `Private` the most — it is the embargo tier that withholds from
    /// every audience including `Internal`). The labelled variants compare by
    /// rank only — a lateral move between two teams / two scope labels is the
    /// **same** rank, hence not *strictly* less restrictive, and must go through
    /// `set` rather than `promote`.
    pub fn restrictiveness_rank(&self) -> u8 {
        match self {
            Self::Public => 0,
            Self::Internal => 1,
            Self::TeamScoped { .. } => 2,
            Self::Restricted { .. } => 3,
            Self::Private { .. } => 4,
        }
    }

    /// `true` iff `self` is **strictly** less restrictive than `other` — i.e. a
    /// `promote` from `other` to `self` is a valid opening transition. A
    /// narrowing (`self` more restrictive) or lateral (equal rank, including a
    /// different team/scope label at the same rank) change returns `false` and
    /// must be expressed with `set`. See [`restrictiveness_rank`](Self::restrictiveness_rank).
    pub fn is_strictly_less_restrictive_than(&self, other: &Self) -> bool {
        self.restrictiveness_rank() < other.restrictiveness_rank()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrowing_matches_reader_sets_including_distinct_labels() {
        use crate::object::{AudienceTier, visible};
        let tiers = [
            VisibilityTier::Public,
            VisibilityTier::Internal,
            team("a"),
            team("b"),
            restricted("a"),
            restricted("b"),
            VisibilityTier::Private {
                scope_label: "a".into(),
            },
            VisibilityTier::Private {
                scope_label: "b".into(),
            },
        ];
        let readers = [
            AudienceTier::Public,
            AudienceTier::Internal,
            AudienceTier::Team("a".into()),
            AudienceTier::Team("b".into()),
            AudienceTier::Team("other".into()),
            AudienceTier::Restricted("a".into()),
            AudienceTier::Restricted("b".into()),
            AudienceTier::Restricted("other".into()),
        ];
        for source in &tiers {
            for target in &tiers {
                let subset = readers
                    .iter()
                    .all(|reader| !visible(target, reader) || visible(source, reader));
                assert_eq!(
                    target.is_no_more_visible_than(source),
                    subset,
                    "{source:?} -> {target:?}"
                );
            }
        }
    }

    fn team(id: &str) -> VisibilityTier {
        VisibilityTier::TeamScoped { team_id: id.into() }
    }
    fn restricted(label: &str) -> VisibilityTier {
        VisibilityTier::Restricted {
            scope_label: label.into(),
        }
    }

    #[test]
    fn restrictiveness_rank_orders_public_least_restricted_most() {
        assert!(
            VisibilityTier::Public.restrictiveness_rank()
                < VisibilityTier::Internal.restrictiveness_rank()
        );
        assert!(VisibilityTier::Internal.restrictiveness_rank() < team("a").restrictiveness_rank());
        assert!(team("a").restrictiveness_rank() < restricted("legal").restrictiveness_rank());
    }

    #[test]
    fn strictly_less_restrictive_only_when_rank_drops() {
        // Opening transitions (lower rank) are strictly less restrictive.
        assert!(
            VisibilityTier::Public.is_strictly_less_restrictive_than(&VisibilityTier::Internal)
        );
        assert!(VisibilityTier::Internal.is_strictly_less_restrictive_than(&restricted("legal")));
        assert!(VisibilityTier::Internal.is_strictly_less_restrictive_than(&team("infra")));

        // Narrowing transitions (higher rank) are NOT.
        assert!(!restricted("legal").is_strictly_less_restrictive_than(&VisibilityTier::Internal));
        assert!(
            !VisibilityTier::Internal.is_strictly_less_restrictive_than(&VisibilityTier::Public)
        );

        // Lateral (same rank) is NOT strictly less restrictive — even across
        // different team/scope labels. A re-scope must go through `set`.
        assert!(!team("a").is_strictly_less_restrictive_than(&team("b")));
        assert!(!restricted("legal").is_strictly_less_restrictive_than(&restricted("security")));
        assert!(
            !VisibilityTier::Internal.is_strictly_less_restrictive_than(&VisibilityTier::Internal)
        );
    }
}
