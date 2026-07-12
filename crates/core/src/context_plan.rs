// SPDX-License-Identifier: Apache-2.0
//! Pure context query/mutate planning helpers (no store/repo I/O).
//!
//! RecoveryAdvice, ObjectStore, and worktree reads stay in the CLI.

use objects::object::{
    Annotation, AnnotationScope, AnnotationStatus, ContextSuggestionTier, ContextTarget,
};

// ---------------------------------------------------------------------------
// Status / suggestion labels
// ---------------------------------------------------------------------------

/// Machine/human status token for an annotation lifecycle state.
pub fn annotation_status_label(status: AnnotationStatus) -> &'static str {
    match status {
        AnnotationStatus::Active => "active",
        AnnotationStatus::Superseded => "superseded",
    }
}

/// Stable machine token for a suggestion tier (`medium` / `high`).
pub fn suggestion_tier_token(tier: &ContextSuggestionTier) -> &'static str {
    match tier {
        ContextSuggestionTier::Medium => "medium",
        ContextSuggestionTier::High => "high",
    }
}

/// Human-facing suggestion tier phrase for text output.
pub fn suggestion_tier_human_label(tier: &ContextSuggestionTier) -> &'static str {
    match tier {
        ContextSuggestionTier::Medium => "may benefit",
        ContextSuggestionTier::High => "recommended",
    }
}

// ---------------------------------------------------------------------------
// Annotation list filters
// ---------------------------------------------------------------------------

/// Whether a single annotation passes list/get filters.
///
/// Scope must already be parsed by the caller (CLI maps parse errors to advice).
pub fn annotation_passes_filters(
    annotation: &Annotation,
    scope_filter: Option<&AnnotationScope>,
    tag_filter: Option<&str>,
    include_superseded: bool,
) -> bool {
    if !include_superseded && annotation.status == AnnotationStatus::Superseded {
        return false;
    }
    if let Some(scope) = scope_filter
        && !annotation.scope.matches(scope)
    {
        return false;
    }
    if let Some(tag) = tag_filter {
        let Some(current) = annotation.current_revision() else {
            return false;
        };
        if !current.tags.iter().any(|candidate| candidate == tag) {
            return false;
        }
    }
    true
}

/// Filter annotations by optional scope/tag and superseded inclusion.
pub fn filter_annotations<'a>(
    annotations: &'a [Annotation],
    scope_filter: Option<&AnnotationScope>,
    tag_filter: Option<&str>,
    include_superseded: bool,
) -> Vec<&'a Annotation> {
    annotations
        .iter()
        .filter(|annotation| {
            annotation_passes_filters(annotation, scope_filter, tag_filter, include_superseded)
        })
        .collect()
}

/// Count annotations still in [`AnnotationStatus::Active`].
pub fn count_active_annotations(annotations: &[Annotation]) -> usize {
    annotations
        .iter()
        .filter(|annotation| annotation.status == AnnotationStatus::Active)
        .count()
}

// ---------------------------------------------------------------------------
// Target / audit pure keys
// ---------------------------------------------------------------------------

/// `(kind, label)` pair for a context target (stable machine kind tokens).
pub fn context_target_kind_and_label(target: &ContextTarget) -> (&'static str, String) {
    match target {
        ContextTarget::File { path } => ("file", path.clone()),
        ContextTarget::State { change_id } => ("state", change_id.to_string_full()),
    }
}

/// Target key used when grouping audit signatures (path or full change id).
pub fn audit_target_key(target: &ContextTarget) -> String {
    match target {
        ContextTarget::File { path } => path.clone(),
        ContextTarget::State { change_id } => change_id.to_string_full(),
    }
}

/// Staleness-map key matching `repo::staleness::check_context_staleness`.
pub fn audit_staleness_key(target: &ContextTarget, annotation: &Annotation) -> String {
    match target {
        ContextTarget::File { path } => format!("{path}:{}", annotation.scope),
        ContextTarget::State { change_id } => {
            format!(
                "state:{}:{}",
                change_id.to_string_full(),
                annotation.annotation_id
            )
        }
    }
}

/// Count signature groups that appear more than once (duplicate annotations).
pub fn audit_duplicate_count(signature_counts: impl IntoIterator<Item = u32>) -> u32 {
    signature_counts
        .into_iter()
        .filter(|count| *count > 1)
        .count() as u32
}

// ---------------------------------------------------------------------------
// Mutate validation (empty body, rm selector, supersede rules)
// ---------------------------------------------------------------------------

/// Missing annotation body source (`-m` / `--file`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextContentPlanError {
    /// Neither message nor file was supplied.
    Required,
}

impl ContextContentPlanError {
    pub fn kind(self) -> &'static str {
        match self {
            Self::Required => "context_content_required",
        }
    }
}

/// Require a content source for set/edit/supersede.
///
/// Does not inspect body text emptiness — only whether a source flag was provided.
pub fn plan_annotation_content_source(
    has_message: bool,
    has_file: bool,
) -> Result<(), ContextContentPlanError> {
    if has_message || has_file {
        Ok(())
    } else {
        Err(ContextContentPlanError::Required)
    }
}

/// Invalid `context rm` selector combinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextRmPlanError {
    /// Neither `--all` nor `--scope` was supplied.
    ScopeRequired,
}

impl ContextRmPlanError {
    pub fn kind(self) -> &'static str {
        match self {
            Self::ScopeRequired => "context_remove_scope_required",
        }
    }
}

/// Plan remove: when not removing all, a scope must be present.
pub fn plan_context_rm(all: bool, scope_present: bool) -> Result<(), ContextRmPlanError> {
    if !all && !scope_present {
        Err(ContextRmPlanError::ScopeRequired)
    } else {
        Ok(())
    }
}

/// Supersede keeps the original target when neither path nor state override is set.
pub fn supersede_reuses_original_target(path: Option<&str>, state: Option<&str>) -> bool {
    path.is_none() && state.is_none()
}

/// Supersede keeps the original scope when `--scope` is omitted.
pub fn supersede_reuses_original_scope(scope: Option<&str>) -> bool {
    scope.is_none()
}

/// Prefer non-empty override tags; otherwise keep the current revision's tags.
pub fn next_annotation_tags(current: &[String], override_tags: Vec<String>) -> Vec<String> {
    if override_tags.is_empty() {
        current.to_vec()
    } else {
        override_tags
    }
}

#[cfg(test)]
mod tests {
    use objects::object::{Annotation, AnnotationKind, AnnotationScope, AnnotationStatus};

    use super::*;

    fn sample_annotation(
        scope: AnnotationScope,
        tags: Vec<String>,
        status: AnnotationStatus,
    ) -> Annotation {
        let mut annotation = Annotation::new(
            scope,
            AnnotationKind::Rationale,
            "body".into(),
            tags,
            "Test <t@example.com>".into(),
            0,
            None,
            None,
        );
        annotation.status = status;
        annotation
    }

    #[test]
    fn status_and_tier_labels() {
        assert_eq!(annotation_status_label(AnnotationStatus::Active), "active");
        assert_eq!(
            annotation_status_label(AnnotationStatus::Superseded),
            "superseded"
        );
        assert_eq!(
            suggestion_tier_token(&ContextSuggestionTier::Medium),
            "medium"
        );
        assert_eq!(suggestion_tier_token(&ContextSuggestionTier::High), "high");
        assert_eq!(
            suggestion_tier_human_label(&ContextSuggestionTier::Medium),
            "may benefit"
        );
        assert_eq!(
            suggestion_tier_human_label(&ContextSuggestionTier::High),
            "recommended"
        );
    }

    #[test]
    fn list_filters_status_scope_and_tag() {
        let active = sample_annotation(
            AnnotationScope::File,
            vec!["a".into()],
            AnnotationStatus::Active,
        );
        let superseded = sample_annotation(
            AnnotationScope::File,
            vec!["a".into()],
            AnnotationStatus::Superseded,
        );
        let tagged = sample_annotation(
            AnnotationScope::Lines(1, 2),
            vec!["hot".into()],
            AnnotationStatus::Active,
        );

        assert!(annotation_passes_filters(&active, None, None, false));
        assert!(!annotation_passes_filters(&superseded, None, None, false));
        assert!(annotation_passes_filters(&superseded, None, None, true));

        assert!(!annotation_passes_filters(
            &active,
            Some(&AnnotationScope::Lines(1, 2)),
            None,
            false
        ));
        assert!(annotation_passes_filters(
            &tagged,
            Some(&AnnotationScope::Lines(1, 2)),
            Some("hot"),
            false
        ));
        assert!(!annotation_passes_filters(
            &tagged,
            None,
            Some("cold"),
            false
        ));

        let pool = [active.clone(), superseded, tagged];
        let filtered = filter_annotations(&pool, None, Some("a"), false);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].annotation_id, active.annotation_id);
        assert_eq!(count_active_annotations(&[active.clone(), active]), 2);
    }

    #[test]
    fn content_and_rm_plans() {
        assert!(plan_annotation_content_source(true, false).is_ok());
        assert!(plan_annotation_content_source(false, true).is_ok());
        assert_eq!(
            plan_annotation_content_source(false, false),
            Err(ContextContentPlanError::Required)
        );
        assert_eq!(
            ContextContentPlanError::Required.kind(),
            "context_content_required"
        );

        assert!(plan_context_rm(true, false).is_ok());
        assert!(plan_context_rm(false, true).is_ok());
        assert_eq!(
            plan_context_rm(false, false),
            Err(ContextRmPlanError::ScopeRequired)
        );
        assert_eq!(
            ContextRmPlanError::ScopeRequired.kind(),
            "context_remove_scope_required"
        );
    }

    #[test]
    fn supersede_and_edit_rules() {
        assert!(supersede_reuses_original_target(None, None));
        assert!(!supersede_reuses_original_target(Some("p"), None));
        assert!(!supersede_reuses_original_target(None, Some("s")));
        assert!(supersede_reuses_original_scope(None));
        assert!(!supersede_reuses_original_scope(Some("file")));

        assert_eq!(
            next_annotation_tags(&["keep".into()], vec![]),
            vec!["keep".to_string()]
        );
        assert_eq!(
            next_annotation_tags(&["keep".into()], vec!["new".into()]),
            vec!["new".to_string()]
        );
    }

    #[test]
    fn audit_duplicate_count_and_keys() {
        assert_eq!(audit_duplicate_count([1, 1, 2, 3]), 2);
        assert_eq!(audit_duplicate_count(std::iter::empty::<u32>()), 0);

        let file = ContextTarget::file("src/a.rs").expect("file target");
        let ann = sample_annotation(AnnotationScope::File, vec![], AnnotationStatus::Active);
        assert_eq!(audit_target_key(&file), "src/a.rs");
        assert_eq!(
            audit_staleness_key(&file, &ann),
            format!("src/a.rs:{}", ann.scope)
        );
        let (kind, label) = context_target_kind_and_label(&file);
        assert_eq!(kind, "file");
        assert_eq!(label, "src/a.rs");
    }
}
