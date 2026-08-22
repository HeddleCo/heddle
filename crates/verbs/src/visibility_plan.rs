// SPDX-License-Identifier: Apache-2.0
//! Pure visibility tier helpers (no store I/O).

use objects::object::VisibilityTier;

/// Team id / scope label carried by a non-public tier, for output.
pub fn visibility_tier_label(tier: &VisibilityTier) -> Option<&str> {
    match tier {
        VisibilityTier::TeamScoped { team_id } => Some(team_id),
        VisibilityTier::Restricted { scope_label } | VisibilityTier::Private { scope_label } => {
            Some(scope_label)
        }
        VisibilityTier::Public | VisibilityTier::Internal => None,
    }
}

/// Stable machine token for a visibility tier kind (not the scope label).
pub fn visibility_tier_kind(tier: &VisibilityTier) -> &'static str {
    match tier {
        VisibilityTier::Public => "public",
        VisibilityTier::Internal => "internal",
        VisibilityTier::TeamScoped { .. } => "team_scoped",
        VisibilityTier::Restricted { .. } => "restricted",
        VisibilityTier::Private { .. } => "private",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_label_and_kind() {
        assert_eq!(visibility_tier_label(&VisibilityTier::Public), None);
        assert_eq!(visibility_tier_kind(&VisibilityTier::Public), "public");
        assert_eq!(
            visibility_tier_label(&VisibilityTier::TeamScoped {
                team_id: "eng".into()
            }),
            Some("eng")
        );
        assert_eq!(
            visibility_tier_kind(&VisibilityTier::TeamScoped {
                team_id: "eng".into()
            }),
            "team_scoped"
        );
        assert_eq!(
            visibility_tier_label(&VisibilityTier::Private {
                scope_label: "me".into()
            }),
            Some("me")
        );
    }
}
