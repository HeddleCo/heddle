// SPDX-License-Identifier: Apache-2.0
//! Shared audience-tier vocabulary.
//!
//! [`VisibilityTier`] is the single content-side visibility vocabulary used
//! across annotations, discussions, and per-state commit visibility. The
//! *reader's* tier (who is asking) is `repo::AudienceTier`; this enum is the
//! *content's* tier (who the content is for). The who-sees-what mapping
//! between the two lives in `repo::visibility::visible`.
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
    /// content. The who-sees-what arm lives in `repo::visibility::visible`,
    /// placed above the `(_, Internal) => true` arm so the embargo holds.
    Private {
        scope_label: String,
    },
}

impl VisibilityTier {
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
}
