// SPDX-License-Identifier: Apache-2.0
//! The reader-side audience tier.
//!
//! [`AudienceTier`] is who is *asking*; [`VisibilityTier`](super::VisibilityTier)
//! is who the content is *for*. Both live here so the who-sees-what mapping
//! ([`visible`]) sits beside the two vocabularies it joins instead of a layer
//! above them.
//!
//! The mapping from [`VisibilityTier`](super::VisibilityTier) to [`AudienceTier`]
//! is the single source of truth for "who sees what":
//!
//! | annotation visibility    | shown to `Owner` | `Internal` | `Public` | `Team(X)`               | `Restricted` |
//! |--------------------------|------------------|------------|----------|-------------------------|--------------|
//! | `Public`                 | yes              | yes        | yes      | yes                     | yes          |
//! | `Internal`               | yes              | yes        | no       | yes                     | no           |
//! | `TeamScoped { team }`    | yes              | yes        | no       | only if `team == X`     | no           |
//! | `Restricted { ... }`     | yes              | yes        | no       | no                      | only equal label |
//! | `Private { ... }`        | yes              | no         | no       | no                      | only equal label |
//!
//! `Owner` is the spool owner — grant-derived, not a CLI `--audience` value.
//! `Internal` is the broadest ordinary audience (used by the
//! workspace-internal reader); `Public` is the anonymous/public audience.
//! `Private` is stricter than Internal: only the matching restricted-scope
//! holder **or the spool owner** can read it.

use std::str::FromStr;

use super::VisibilityTier;

/// Audience reading the annotation set. Matches the CLI's
/// `--audience <internal|public|team:NAME>` flag and the web's payload-
/// shaping context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudienceTier {
    /// Spool owner. Grant-derived only (not a CLI `--audience` token).
    /// Sees every tier including `Private`, so an owner can clone and
    /// serve their own embargoed head. Unrelated principals never map here.
    Owner,
    /// Workspace-internal viewer — sees every annotation regardless of
    /// scope except `Private`. The `--audience internal` value on Git
    /// projection export.
    Internal,
    /// Anonymous public viewer — sees only `Public` annotations. Default
    /// for Git projection export and the public-PR review surface.
    Public,
    /// A specific team. Sees Public, Internal (assumed in-network), and
    /// `TeamScoped` annotations whose team matches.
    Team(String),
    /// A restricted scope label (legal, security, etc.). Sees Public and
    /// `Restricted` annotations whose label matches.
    Restricted(String),
}

/// Error from [`AudienceTier::from_str`]. The string form is what the
/// CLI's `--audience` flag accepts; bad input here is a usage error.
#[derive(Debug, thiserror::Error)]
pub enum AudienceParseError {
    #[error("audience must be one of: internal, public, team:<NAME>, restricted:<LABEL>")]
    Unknown,
    #[error("`team:` audience requires a non-empty NAME")]
    MissingTeamName,
    #[error("`restricted:` audience requires a non-empty LABEL")]
    MissingRestrictedLabel,
}

impl FromStr for AudienceTier {
    type Err = AudienceParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("internal") {
            return Ok(AudienceTier::Internal);
        }
        if trimmed.eq_ignore_ascii_case("public") {
            return Ok(AudienceTier::Public);
        }
        if let Some(rest) = trimmed.strip_prefix("team:") {
            let name = rest.trim();
            if name.is_empty() {
                return Err(AudienceParseError::MissingTeamName);
            }
            return Ok(AudienceTier::Team(name.to_string()));
        }
        if let Some(rest) = trimmed.strip_prefix("restricted:") {
            let label = rest.trim();
            if label.is_empty() {
                return Err(AudienceParseError::MissingRestrictedLabel);
            }
            return Ok(AudienceTier::Restricted(label.to_string()));
        }
        Err(AudienceParseError::Unknown)
    }
}

/// Single source-of-truth for the visibility×audience mapping. Pure over
/// the two tier enums, so every consumer (annotation filtering, bridge
/// export gating) shares the exact same rules — drift between consumers
/// would be invisible at the call site and catastrophic for the Git
/// projection export footer.
pub fn visible(visibility: &VisibilityTier, audience: &AudienceTier) -> bool {
    match (visibility, audience) {
        // Public is universally visible.
        (VisibilityTier::Public, _) => true,
        // The spool owner sees every remaining tier, including Private.
        // Must sit above the Private embargo arms so an unlabeled Owner
        // grant is not treated as Internal and then denied the head it
        // just embargoed.
        (_, AudienceTier::Owner) => true,
        // Private is the strictest ordinary tier: visible to the holder of
        // the exact matching Restricted scope label, and withheld from
        // everyone else — *including* the otherwise all-seeing Internal
        // audience. These two arms MUST stay above
        // `(_, AudienceTier::Internal) => true`: match arms evaluate
        // top-to-bottom, so a Private arm below it would never be reached
        // for an Internal audience and the embargo would silently leak
        // to internal callers.
        (VisibilityTier::Private { scope_label }, AudienceTier::Restricted(viewer)) => {
            scope_label == viewer
        }
        (VisibilityTier::Private { .. }, _) => false,
        // Internal sees everything else (internal viewers are the trusted set).
        (_, AudienceTier::Internal) => true,
        // Internal annotations to a public/restricted viewer are hidden.
        (VisibilityTier::Internal, AudienceTier::Public)
        | (VisibilityTier::Internal, AudienceTier::Restricted(_)) => false,
        // Internal annotations to a team viewer are visible — the team
        // is part of the workspace-internal trusted set. (Public-only
        // export still hides them via the previous arm.)
        (VisibilityTier::Internal, AudienceTier::Team(_)) => true,
        // Team-scoped: visible only to that exact team.
        (VisibilityTier::TeamScoped { team_id }, AudienceTier::Team(name)) => team_id == name,
        (VisibilityTier::TeamScoped { .. }, _) => false,
        // Restricted: visible only to a viewer holding the matching label.
        (VisibilityTier::Restricted { scope_label }, AudienceTier::Restricted(viewer_label)) => {
            scope_label == viewer_label
        }
        (VisibilityTier::Restricted { .. }, _) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_is_universally_visible_and_team_matches_exact_id() {
        assert!(visible(&VisibilityTier::Public, &AudienceTier::Public));
        assert!(visible(
            &VisibilityTier::TeamScoped {
                team_id: "infra".into()
            },
            &AudienceTier::Team("infra".into())
        ));
        assert!(!visible(
            &VisibilityTier::TeamScoped {
                team_id: "infra".into()
            },
            &AudienceTier::Team("design".into())
        ));
        assert!(visible(
            &VisibilityTier::Restricted {
                scope_label: "legal".into()
            },
            &AudienceTier::Internal
        ));
        assert!(!visible(
            &VisibilityTier::Internal,
            &AudienceTier::Restricted("legal".into())
        ));
    }

    #[test]
    fn private_visible_only_to_matching_restricted_audience() {
        let vis = VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        };
        // The one authorized scope sees it.
        assert!(visible(
            &vis,
            &AudienceTier::Restricted("sec-embargo".into())
        ));
        // A non-matching restricted label does not.
        assert!(!visible(&vis, &AudienceTier::Restricted("legal".into())));
    }

    #[test]
    fn private_is_hidden_even_from_the_all_seeing_internal_audience() {
        // The whole point of Private over Restricted: the otherwise
        // all-seeing Internal audience is denied. The Private arm MUST sit
        // above the `(_, Internal) => true` arm.
        let vis = VisibilityTier::Private {
            scope_label: "sec-embargo".into(),
        };
        assert!(!visible(&vis, &AudienceTier::Internal));
        assert!(!visible(&vis, &AudienceTier::Public));
        assert!(!visible(&vis, &AudienceTier::Team("infra".into())));
        // The spool owner is the other admit — unlabeled Owner must not
        // collapse into Internal or clone of a private-tier head fails.
        assert!(visible(&vis, &AudienceTier::Owner));
    }

    #[test]
    fn parse_audience_strings() {
        assert_eq!(
            "internal".parse::<AudienceTier>().unwrap(),
            AudienceTier::Internal
        );
        assert_eq!(
            "public".parse::<AudienceTier>().unwrap(),
            AudienceTier::Public
        );
        assert_eq!(
            "team:infra".parse::<AudienceTier>().unwrap(),
            AudienceTier::Team("infra".into())
        );
        assert_eq!(
            "restricted:legal".parse::<AudienceTier>().unwrap(),
            AudienceTier::Restricted("legal".into())
        );
        assert!("team:".parse::<AudienceTier>().is_err());
        assert!("nonsense".parse::<AudienceTier>().is_err());
    }
}
