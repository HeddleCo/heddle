// SPDX-License-Identifier: Apache-2.0
//! Annotation visibility filtering.
//!
//! Every annotation read path (CLI rendering, web payload shaping, bridge
//! export) flows through one of [`filter_for_audience`] or
//! [`filter_for_audience_with_drops`]. The latter is the same filter, but
//! tracks how many annotations were excluded per scope so the bridge
//! footer can report a count and the web page can show "N annotations
//! hidden by your audience tier".
//!
//! The mapping from [`VisibilityTier`] to [`AudienceTier`] is the
//! single source of truth for "who sees what":
//!
//! | annotation visibility    | shown to `Internal` | `Public` | `Team(X)`               | `Restricted` |
//! |--------------------------|---------------------|----------|-------------------------|--------------|
//! | `Public`                 | yes                 | yes      | yes                     | yes          |
//! | `Internal`               | yes                 | no       | yes                     | no           |
//! | `TeamScoped { team }`    | yes                 | no       | only if `team == X`     | no           |
//! | `Restricted { ... }`     | yes                 | no       | no                      | only equal label |
//!
//! `Internal` is the most permissive tier (used by the workspace-internal
//! reader); `Public` is the most restrictive (used by anonymous web
//! viewers and by `bridge git export` by default).

use std::str::FromStr;

use objects::object::{Annotation, VisibilityTier};

/// Audience reading the annotation set. Matches the CLI's
/// `--audience <internal|public|team:NAME>` flag and the web's payload-
/// shaping context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudienceTier {
    /// Workspace-internal viewer — sees every annotation regardless of
    /// scope. The `--audience internal` value on bridge export.
    Internal,
    /// Anonymous public viewer — sees only `Public` annotations. Default
    /// for bridge export and the public-PR review surface.
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

/// Per-scope counts of annotations excluded by the filter. Returned
/// alongside the filtered slice so callers can surface "N hidden" in
/// renderings without re-running the filter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeDropCounts {
    pub internal: u32,
    pub team: u32,
    pub restricted: u32,
    pub private: u32,
}

impl ScopeDropCounts {
    /// Total annotations dropped across all scopes. Drives the
    /// `Heddle-Annotations-Omitted` footer line.
    pub fn total(&self) -> u32 {
        self.internal + self.team + self.restricted + self.private
    }
}

/// Return only the annotations visible to `audience`. Borrowing variant —
/// callers that need the original slice (e.g. for re-filtering at a
/// different audience tier) keep ownership.
pub fn filter_for_audience<'a>(
    annotations: &'a [Annotation],
    audience: &AudienceTier,
) -> Vec<&'a Annotation> {
    annotations
        .iter()
        .filter(|a| visible(&a.visibility, audience))
        .collect()
}

/// Same as [`filter_for_audience`] but also reports per-scope drop
/// counts. Used by `bridge git export` to populate
/// `Heddle-Annotations-Omitted` and the optional notes breakdown.
pub fn filter_for_audience_with_drops<'a>(
    annotations: &'a [Annotation],
    audience: &AudienceTier,
) -> (Vec<&'a Annotation>, ScopeDropCounts) {
    let mut kept = Vec::with_capacity(annotations.len());
    let mut drops = ScopeDropCounts::default();
    for ann in annotations {
        if visible(&ann.visibility, audience) {
            kept.push(ann);
        } else {
            match &ann.visibility {
                VisibilityTier::Public => {}
                VisibilityTier::Internal => drops.internal += 1,
                VisibilityTier::TeamScoped { .. } => drops.team += 1,
                VisibilityTier::Restricted { .. } => drops.restricted += 1,
                VisibilityTier::Private { .. } => drops.private += 1,
            }
        }
    }
    (kept, drops)
}

/// Single source-of-truth for the visibility×audience mapping. Pulled
/// out so the borrowing and dropping variants share the exact same
/// rules — drift between them would be invisible at the call site and
/// catastrophic for the bridge export footer.
pub fn visible(visibility: &VisibilityTier, audience: &AudienceTier) -> bool {
    match (visibility, audience) {
        // Public is universally visible.
        (VisibilityTier::Public, _) => true,
        // Private is the strictest tier: visible ONLY to the holder of the
        // exact matching Restricted scope label, and withheld from everyone
        // else — *including* the otherwise all-seeing Internal audience. These
        // two arms MUST stay above `(_, AudienceTier::Internal) => true`:
        // match arms evaluate top-to-bottom, so a Private arm below it would
        // never be reached for an Internal audience and the embargo would
        // silently leak to internal callers.
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
    use objects::object::{Annotation, AnnotationScope, AnnotationStatus, VisibilityTier};

    fn ann(id: &str, vis: VisibilityTier) -> Annotation {
        Annotation {
            annotation_id: id.into(),
            scope: AnnotationScope::File,
            status: AnnotationStatus::Active,
            revisions: vec![],
            supersedes_annotation_id: None,
            supersedes_rewrite_pct: None,
            visibility: vis,
            resolved_from_discussion: None,
        }
    }

    #[test]
    fn public_audience_sees_only_public() {
        let anns = vec![
            ann("a", VisibilityTier::Public),
            ann("b", VisibilityTier::Internal),
            ann(
                "c",
                VisibilityTier::TeamScoped {
                    team_id: "infra".into(),
                },
            ),
            ann(
                "d",
                VisibilityTier::Restricted {
                    scope_label: "legal".into(),
                },
            ),
        ];
        let (kept, drops) = filter_for_audience_with_drops(&anns, &AudienceTier::Public);
        assert_eq!(
            kept.iter()
                .map(|a| a.annotation_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(drops.internal, 1);
        assert_eq!(drops.team, 1);
        assert_eq!(drops.restricted, 1);
        assert_eq!(drops.total(), 3);
    }

    #[test]
    fn internal_audience_sees_everything() {
        let anns = vec![
            ann("a", VisibilityTier::Public),
            ann("b", VisibilityTier::Internal),
            ann(
                "c",
                VisibilityTier::Restricted {
                    scope_label: "legal".into(),
                },
            ),
        ];
        let (kept, drops) = filter_for_audience_with_drops(&anns, &AudienceTier::Internal);
        assert_eq!(kept.len(), 3);
        assert_eq!(drops.total(), 0);
    }

    #[test]
    fn team_audience_filters_by_team_id() {
        let anns = vec![
            ann(
                "infra",
                VisibilityTier::TeamScoped {
                    team_id: "infra".into(),
                },
            ),
            ann(
                "design",
                VisibilityTier::TeamScoped {
                    team_id: "design".into(),
                },
            ),
            ann("public", VisibilityTier::Public),
            ann("internal", VisibilityTier::Internal),
        ];
        let (kept, drops) =
            filter_for_audience_with_drops(&anns, &AudienceTier::Team("infra".into()));
        let ids: Vec<&str> = kept.iter().map(|a| a.annotation_id.as_str()).collect();
        assert!(ids.contains(&"infra"));
        assert!(ids.contains(&"public"));
        assert!(ids.contains(&"internal"));
        assert!(!ids.contains(&"design"));
        // One drop, the design-team annotation.
        assert_eq!(drops.team, 1);
    }

    #[test]
    fn restricted_audience_matches_label_only() {
        let anns = vec![
            ann(
                "legal",
                VisibilityTier::Restricted {
                    scope_label: "legal".into(),
                },
            ),
            ann(
                "security",
                VisibilityTier::Restricted {
                    scope_label: "security".into(),
                },
            ),
            ann("public", VisibilityTier::Public),
            ann("internal", VisibilityTier::Internal),
        ];
        let (kept, drops) =
            filter_for_audience_with_drops(&anns, &AudienceTier::Restricted("legal".into()));
        let ids: Vec<&str> = kept.iter().map(|a| a.annotation_id.as_str()).collect();
        assert!(ids.contains(&"legal"));
        assert!(ids.contains(&"public"));
        assert!(!ids.contains(&"security"));
        assert!(!ids.contains(&"internal"));
        assert_eq!(drops.restricted, 1);
        assert_eq!(drops.internal, 1);
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
    }

    #[test]
    fn private_drops_are_counted_and_internal_audience_keeps_restricted() {
        let anns = vec![
            ann("public", VisibilityTier::Public),
            ann(
                "private",
                VisibilityTier::Private {
                    scope_label: "sec-embargo".into(),
                },
            ),
        ];
        // Even the all-seeing Internal audience drops the Private annotation.
        let (kept, drops) = filter_for_audience_with_drops(&anns, &AudienceTier::Internal);
        let ids: Vec<&str> = kept.iter().map(|a| a.annotation_id.as_str()).collect();
        assert_eq!(ids, vec!["public"]);
        assert_eq!(drops.private, 1);
        assert_eq!(drops.total(), 1);
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

    #[test]
    fn borrowing_filter_matches_drop_filter_kept_set() {
        let anns = vec![
            ann("a", VisibilityTier::Public),
            ann("b", VisibilityTier::Internal),
        ];
        let kept_only = filter_for_audience(&anns, &AudienceTier::Public);
        let (kept_drops, _) = filter_for_audience_with_drops(&anns, &AudienceTier::Public);
        let ids_only: Vec<&str> = kept_only.iter().map(|a| a.annotation_id.as_str()).collect();
        let ids_drops: Vec<&str> = kept_drops
            .iter()
            .map(|a| a.annotation_id.as_str())
            .collect();
        assert_eq!(ids_only, ids_drops);
    }
}
