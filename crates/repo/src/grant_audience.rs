// SPDX-License-Identifier: Apache-2.0
//! Grant-role → [`AudienceTier`] mapping (O2).
//!
//! This is the heddle-side typed contract Weft gates `ListRefs`/`Pull` against.
//! Unknown or missing roles fail closed to [`AudienceTier::Public`] so an
//! unauthorized puller can never be treated as the internal trusted set.
//! Owners stay label-scoped: unlabeled Owner is Internal; Private is visible
//! only with a matching `audience_label` (`Restricted`).

use objects::object::{VisibilityTier, visible};

use super::visibility::AudienceTier;

/// Hosted grant-role ordinal. Mirrors `HostedRole` without depending on
/// generated proto, so the mapping stays auditable in this workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GrantRole {
    Unspecified = 0,
    Reader = 1,
    Developer = 2,
    Maintainer = 3,
    Admin = 4,
    Owner = 5,
}

impl GrantRole {
    /// Parse a proto `HostedRole` i32. Unknown values fail closed to
    /// [`GrantRole::Unspecified`].
    pub fn from_hosted_role_i32(role: i32) -> Self {
        match role {
            1 => GrantRole::Reader,
            2 => GrantRole::Developer,
            3 => GrantRole::Maintainer,
            4 => GrantRole::Admin,
            5 => GrantRole::Owner,
            _ => GrantRole::Unspecified,
        }
    }
}

/// Map a caller's grant to the audience the visibility gate must use.
///
/// * A non-empty `audience_label` is the authorized embargo scope and maps to
///   [`AudienceTier::Restricted`]. Private content is visible only to that
///   label, not to Internal.
/// * Developer and above are the workspace-internal trusted set.
/// * Reader, unspecified, and missing grants are [`AudienceTier::Public`].
///   An unauthorized puller must never receive Internal.
pub fn audience_tier_for_grant(
    role: Option<GrantRole>,
    audience_label: Option<&str>,
) -> AudienceTier {
    // Role is authoritative. A stale or untrusted label must not promote a
    // missing/unknown grant into Restricted (which can disclose Private).
    match role {
        None | Some(GrantRole::Unspecified) => return AudienceTier::Public,
        Some(GrantRole::Reader)
        | Some(GrantRole::Developer)
        | Some(GrantRole::Maintainer)
        | Some(GrantRole::Admin)
        | Some(GrantRole::Owner) => {}
    }
    if let Some(label) = audience_label
        .map(str::trim)
        .filter(|label| !label.is_empty())
    {
        return AudienceTier::Restricted(label.to_string());
    }
    match role {
        Some(GrantRole::Developer)
        | Some(GrantRole::Maintainer)
        | Some(GrantRole::Admin)
        | Some(GrantRole::Owner) => AudienceTier::Internal,
        Some(GrantRole::Reader) | Some(GrantRole::Unspecified) | None => AudienceTier::Public,
    }
}

/// Whether this grant may be served a record at `tier`.
///
/// Owners stay label-scoped: unlabeled Owner is Internal and cannot see
/// `Private`. A matching `audience_label` is `Restricted(L)` and is the
/// only Owner admit for that embargo. Missing/unspecified grants stay
/// Public (Bob without a grant remains PermissionDenied).
pub fn grant_can_see_tier(
    role: Option<GrantRole>,
    audience_label: Option<&str>,
    tier: &VisibilityTier,
) -> bool {
    visible(tier, &audience_tier_for_grant(role, audience_label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unauthorized_and_unknown_roles_are_public() {
        assert_eq!(audience_tier_for_grant(None, None), AudienceTier::Public);
        assert_eq!(
            audience_tier_for_grant(Some(GrantRole::Unspecified), None),
            AudienceTier::Public
        );
        assert_eq!(
            audience_tier_for_grant(Some(GrantRole::from_hosted_role_i32(99)), None),
            AudienceTier::Public
        );
        assert_eq!(
            audience_tier_for_grant(Some(GrantRole::Reader), None),
            AudienceTier::Public
        );
    }

    #[test]
    fn trusted_writers_are_internal() {
        for role in [
            GrantRole::Developer,
            GrantRole::Maintainer,
            GrantRole::Admin,
            GrantRole::Owner,
        ] {
            assert_eq!(
                audience_tier_for_grant(Some(role), None),
                AudienceTier::Internal
            );
        }
    }

    #[test]
    fn embargo_label_is_restricted_even_for_owners() {
        assert_eq!(
            audience_tier_for_grant(Some(GrantRole::Owner), Some("sec-embargo")),
            AudienceTier::Restricted("sec-embargo".into())
        );
        assert_eq!(
            audience_tier_for_grant(Some(GrantRole::Owner), Some("  ")),
            AudienceTier::Internal
        );
    }

    #[test]
    fn owner_sees_private_only_with_matching_embargo_label() {
        let private = VisibilityTier::Private {
            scope_label: "ax-only".into(),
        };
        let other = VisibilityTier::Private {
            scope_label: "other".into(),
        };
        assert_eq!(
            audience_tier_for_grant(Some(GrantRole::Owner), None),
            AudienceTier::Internal
        );
        assert!(
            !grant_can_see_tier(Some(GrantRole::Owner), None, &private),
            "unlabeled Owner must not see every Private label"
        );
        assert!(grant_can_see_tier(
            Some(GrantRole::Owner),
            Some("ax-only"),
            &private
        ));
        assert!(!grant_can_see_tier(
            Some(GrantRole::Owner),
            Some("other"),
            &private
        ));
        assert!(!grant_can_see_tier(
            Some(GrantRole::Owner),
            Some("ax-only"),
            &other
        ));
        assert!(!grant_can_see_tier(Some(GrantRole::Admin), None, &private));
        assert!(
            !grant_can_see_tier(None, None, &private),
            "a missing grant must stay Public and must not see Private"
        );
        assert!(!grant_can_see_tier(
            Some(GrantRole::Unspecified),
            None,
            &private
        ));
    }

    #[test]
    fn missing_or_unknown_role_with_label_is_public() {
        assert_eq!(
            audience_tier_for_grant(None, Some("sec-embargo")),
            AudienceTier::Public
        );
        assert_eq!(
            audience_tier_for_grant(Some(GrantRole::Unspecified), Some("sec-embargo")),
            AudienceTier::Public
        );
        assert_eq!(
            audience_tier_for_grant(
                Some(GrantRole::from_hosted_role_i32(99)),
                Some("sec-embargo")
            ),
            AudienceTier::Public
        );
    }
}
