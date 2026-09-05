// SPDX-License-Identifier: Apache-2.0
//! Grant-role → [`AudienceTier`] mapping (O2).
//!
//! This is the heddle-side typed contract Weft gates `ListRefs`/`Pull` against.
//! Unknown or missing roles fail closed to [`AudienceTier::Public`] so an
//! unauthorized puller can never be treated as the internal trusted set.

use objects::object::{visible, VisibilityTier};

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
/// * [`GrantRole::Owner`] is [`AudienceTier::Owner`] so the spool owner can
///   see `Private` heads they declared. A non-empty `audience_label` still
///   narrows them to [`AudienceTier::Restricted`] (one embargo scope).
/// * A non-empty `audience_label` on any authorized role maps to
///   [`AudienceTier::Restricted`]. Private content is visible only to that
///   label, not to Internal.
/// * Developer and above (except Owner without a label) are Internal.
/// * Reader, unspecified, and missing grants are [`AudienceTier::Public`].
///   An unauthorized puller must never receive Internal or Owner.
pub fn audience_tier_for_grant(
    role: Option<GrantRole>,
    audience_label: Option<&str>,
) -> AudienceTier {
    // Role is authoritative. A stale or untrusted label must not promote a
    // missing/unknown grant into Restricted or Owner (which can disclose
    // Private).
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
        Some(GrantRole::Owner) => AudienceTier::Owner,
        Some(GrantRole::Developer) | Some(GrantRole::Maintainer) | Some(GrantRole::Admin) => {
            AudienceTier::Internal
        }
        Some(GrantRole::Reader) | Some(GrantRole::Unspecified) | None => AudienceTier::Public,
    }
}

/// Whether this grant may be served a record at `tier`.
///
/// Weft's ListRefs/Pull pre-pass and this CLI share this helper so an
/// unlabeled owner is not treated as Internal and then denied their own
/// `Private` head (clone exit 78 `state not found`).
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
        ] {
            assert_eq!(
                audience_tier_for_grant(Some(role), None),
                AudienceTier::Internal
            );
        }
    }

    #[test]
    fn unlabeled_owner_sees_private_and_unrelated_principal_does_not() {
        let private = VisibilityTier::Private {
            scope_label: "ax-only".into(),
        };
        assert_eq!(
            audience_tier_for_grant(Some(GrantRole::Owner), None),
            AudienceTier::Owner
        );
        assert!(grant_can_see_tier(Some(GrantRole::Owner), None, &private));
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
    fn embargo_label_is_restricted_even_for_owners() {
        assert_eq!(
            audience_tier_for_grant(Some(GrantRole::Owner), Some("sec-embargo")),
            AudienceTier::Restricted("sec-embargo".into())
        );
        assert_eq!(
            audience_tier_for_grant(Some(GrantRole::Owner), Some("  ")),
            AudienceTier::Owner
        );
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
