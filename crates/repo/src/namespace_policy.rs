// SPDX-License-Identifier: Apache-2.0
//! Namespace-level visibility policy.
//!
//! Three-tier resolution for an annotation's default visibility when the
//! caller doesn't supply one:
//!
//! 1. Explicit `--visibility` on the discussion/annotation creation —
//!    handled by the caller, not this module.
//! 2. Namespace policy (`[namespace.<name>] default_visibility = "..."`).
//! 3. Repo-wide default (`[review.discussion] default_visibility = "..."`).
//! 4. Hard-coded fallback: [`AnnotationVisibility::Internal`] — the safer
//!    choice for the "we have no idea who should see this" case.

use objects::object::AnnotationVisibility;
use serde::{Deserialize, Serialize};

/// Per-namespace overrides for default visibility. Loaded from the
/// `[namespace.<name>]` table in repo config; hosted-server overrides
/// (later) merge on top by namespace name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespacePolicy {
    /// Stable identifier — the table name from `[namespace.<name>]`.
    pub name: String,
    /// Default visibility applied to annotations whose anchor falls
    /// inside this namespace and whose creator didn't pass
    /// `--visibility` explicitly.
    pub default_visibility: AnnotationVisibility,
    /// When `true`, annotations on external references inherit the
    /// parent annotation's visibility instead of falling through to
    /// this policy. Mirrors the plan's
    /// `external_refs_inherit_parent: bool = true` default.
    #[serde(default = "default_external_refs_inherit_parent")]
    pub external_refs_inherit_parent: bool,
}

fn default_external_refs_inherit_parent() -> bool {
    true
}

impl NamespacePolicy {
    pub fn new(name: impl Into<String>, default_visibility: AnnotationVisibility) -> Self {
        Self {
            name: name.into(),
            default_visibility,
            external_refs_inherit_parent: true,
        }
    }
}

/// Inputs to [`resolve_default_visibility`]. Holding them in a struct
/// keeps the resolution call readable at the call site (one positional
/// arg per logical layer).
#[derive(Clone, Debug, Default)]
pub struct VisibilityResolutionContext<'a> {
    /// Repo-wide default from `[review.discussion] default_visibility`.
    pub repo_default: Option<AnnotationVisibility>,
    /// Namespace policy applicable to the anchor, if any.
    pub namespace: Option<&'a NamespacePolicy>,
}

/// Pick the default visibility for an annotation/discussion when the
/// caller didn't pass `--visibility` explicitly. Resolution order:
/// **namespace > repo-default > Internal**.
///
/// Explicit-from-the-caller is layered on top of this by the caller —
/// `resolve_default_visibility` never sees an explicit value because by
/// definition the caller didn't supply one.
pub fn resolve_default_visibility(ctx: &VisibilityResolutionContext<'_>) -> AnnotationVisibility {
    if let Some(policy) = ctx.namespace {
        return policy.default_visibility.clone();
    }
    if let Some(repo_default) = &ctx.repo_default {
        return repo_default.clone();
    }
    AnnotationVisibility::Internal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_internal_when_nothing_set() {
        let ctx = VisibilityResolutionContext::default();
        assert_eq!(
            resolve_default_visibility(&ctx),
            AnnotationVisibility::Internal
        );
    }

    #[test]
    fn repo_default_used_without_namespace() {
        let ctx = VisibilityResolutionContext {
            repo_default: Some(AnnotationVisibility::Public),
            namespace: None,
        };
        assert_eq!(
            resolve_default_visibility(&ctx),
            AnnotationVisibility::Public
        );
    }

    #[test]
    fn namespace_policy_overrides_repo_default() {
        let policy = NamespacePolicy::new(
            "infra",
            AnnotationVisibility::TeamScoped {
                team_id: "infra".into(),
            },
        );
        let ctx = VisibilityResolutionContext {
            repo_default: Some(AnnotationVisibility::Public),
            namespace: Some(&policy),
        };
        assert_eq!(
            resolve_default_visibility(&ctx),
            AnnotationVisibility::TeamScoped {
                team_id: "infra".into()
            }
        );
    }
}