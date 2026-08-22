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
//! The audience tier itself and the who-sees-what mapping are leaf concepts
//! that live with the visibility vocabularies in `objects::object`
//! ([`AudienceTier`], [`visible`]); they are re-exported here so existing
//! `repo::AudienceTier` paths keep working.

use objects::object::{Annotation, VisibilityTier};
pub use objects::object::{AudienceParseError, AudienceTier, visible};

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
/// counts. Used by `export git` to populate
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

#[cfg(test)]
mod tests {
    use objects::object::{Annotation, AnnotationScope, AnnotationStatus, VisibilityTier};

    use super::*;

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
