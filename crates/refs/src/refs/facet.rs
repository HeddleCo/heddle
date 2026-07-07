// SPDX-License-Identifier: Apache-2.0
//! Facet lineages — the open generalization of the oplog's scope partition.
//!
//! Heddle's oplog is already scope-partitioned: a per-worktree scope token
//! (`wt-<digest>`) threads through [`record_batch_scoped`] and
//! `IsolationKey::LocalHead { scope }`, keeping distinct local-HEAD lineages as
//! independent undo/redo streams within one repo. The Git-overlay lineage and
//! the Heddle lineage are the two well-known content-side histories that ride
//! this partition today.
//!
//! A [`SpoolFacet`] generalizes that same partition to an **open set** of named
//! lineages. Each facet is a fully independent history:
//!
//! - its own oplog batches — the scope token gains a `/<facet>` suffix, so
//!   `record_batch_scoped`/`recent_batches_scoped`/`undo_batches_scoped` filter
//!   each facet's batches into their own gap-free per-scope sequence;
//! - its own undo/redo isolation — the suffixed scope flows into
//!   `IsolationKey::LocalHead { scope }`, so a `governance` op never conflicts
//!   with (or rewinds) `content` undo state and vice-versa;
//! - its own refs, under the `refs/spool/<facet>/…` prefix (`validate_ref_name`
//!   already accepts these — no ref-schema change);
//! - its own HEAD, modeled as an attached-thread ref under that prefix using the
//!   existing [`Head`](crate::Head) enum unchanged.
//!
//! This is **additive**: [`SpoolFacet::Content`] is the well-known default and
//! composes to the *unchanged* per-worktree scope token, so existing
//! Git/Heddle/content behavior is byte-for-byte preserved. Named facets
//! (`governance`, `membership`, or any other token) get their own suffixed
//! scope + ref prefix.

/// A named facet lineage within one repo/spool.
///
/// The three well-known variants name the content-side lineages the Spool model
/// versions independently. [`SpoolFacet::Named`] carries any other facet token
/// so the set is genuinely open — the substrate treats the facet purely as a
/// string that suffixes the scope and prefixes ref names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SpoolFacet {
    /// The versioned file tree — "a repo" today. The **default** facet: it maps
    /// to the unchanged per-worktree scope token and the `refs/spool/content/…`
    /// prefix, so pre-facet callers keep their exact behavior.
    Content,
    /// Visibility + policy-as-code, versioned as content.
    Governance,
    /// Grants + roles, versioned as content.
    Membership,
    /// Any other named facet lineage. The set of facets is open; callers may
    /// version an arbitrary named lineage on the same substrate.
    Named(String),
}

impl SpoolFacet {
    /// The facet token as it appears in scope tokens and ref names.
    pub fn token(&self) -> &str {
        match self {
            SpoolFacet::Content => "content",
            SpoolFacet::Governance => "governance",
            SpoolFacet::Membership => "membership",
            SpoolFacet::Named(token) => token.as_str(),
        }
    }

    /// True for the default (content) facet, which composes to the unchanged
    /// per-worktree scope token. A `Named("content")` also normalizes here so
    /// the well-known token and its string spelling never diverge.
    pub fn is_default(&self) -> bool {
        matches!(self, SpoolFacet::Content) || self.token() == "content"
    }

    /// Compose a facet-qualified oplog scope token from the per-worktree base
    /// scope (`wt-<digest>`, from `Repository::op_scope`).
    ///
    /// The default (content) facet returns the base scope **unchanged**, so the
    /// scope string — and therefore every existing oplog batch, undo record, and
    /// `IsolationKey::LocalHead` — is byte-identical to the pre-facet world.
    /// Every other facet appends `/<token>`, giving that facet its own scope
    /// partition: its batches, its undo/redo view, its isolation key.
    pub fn scope_token(&self, base_scope: &str) -> String {
        if self.is_default() {
            base_scope.to_string()
        } else {
            format!("{base_scope}/{}", self.token())
        }
    }

    /// The ref-name prefix for this facet: `refs/spool/<facet>`.
    ///
    /// All of a facet's refs live under this prefix; `validate_ref_name` accepts
    /// it as an ordinary hierarchical name.
    pub fn ref_prefix(&self) -> String {
        format!("refs/spool/{}", self.token())
    }

    /// The facet's thread ref for `name` — `refs/spool/<facet>/threads/<name>`.
    /// A facet's HEAD attaches to one of these ([`Head::Attached`]).
    pub fn thread_ref(&self, name: &str) -> String {
        format!("{}/threads/{name}", self.ref_prefix())
    }

    /// The facet's marker ref for `name` — `refs/spool/<facet>/markers/<name>`.
    pub fn marker_ref(&self, name: &str) -> String {
        format!("{}/markers/{name}", self.ref_prefix())
    }
}

impl std::fmt::Display for SpoolFacet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.token())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate_ref_name;

    #[test]
    fn content_facet_scope_token_is_unchanged_base() {
        // The default facet must NOT alter the per-worktree scope token, so all
        // existing content/Git/Heddle oplog + undo behavior is preserved.
        let base = "wt-0123456789abcdef";
        assert_eq!(SpoolFacet::Content.scope_token(base), base);
        // A Named("content") normalizes to the same default behavior.
        assert_eq!(SpoolFacet::Named("content".into()).scope_token(base), base);
    }

    #[test]
    fn named_facets_get_independent_suffixed_scopes() {
        let base = "wt-0123456789abcdef";
        assert_eq!(
            SpoolFacet::Governance.scope_token(base),
            "wt-0123456789abcdef/governance"
        );
        assert_eq!(
            SpoolFacet::Membership.scope_token(base),
            "wt-0123456789abcdef/membership"
        );
        // Two different facets never collide in scope space.
        assert_ne!(
            SpoolFacet::Governance.scope_token(base),
            SpoolFacet::Membership.scope_token(base)
        );
        assert_ne!(
            SpoolFacet::Governance.scope_token(base),
            SpoolFacet::Content.scope_token(base)
        );
    }

    #[test]
    fn facet_ref_names_validate() {
        for facet in [
            SpoolFacet::Content,
            SpoolFacet::Governance,
            SpoolFacet::Membership,
            SpoolFacet::Named("audit".into()),
        ] {
            assert!(validate_ref_name(&facet.ref_prefix()).is_ok());
            assert!(validate_ref_name(&facet.thread_ref("main")).is_ok());
            assert!(validate_ref_name(&facet.marker_ref("v1")).is_ok());
        }
    }

    #[test]
    fn distinct_facets_have_distinct_ref_prefixes() {
        assert_eq!(
            SpoolFacet::Governance.thread_ref("main"),
            "refs/spool/governance/threads/main"
        );
        assert_ne!(
            SpoolFacet::Governance.thread_ref("main"),
            SpoolFacet::Content.thread_ref("main")
        );
    }
}
