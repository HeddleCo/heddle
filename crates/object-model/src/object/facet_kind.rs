// SPDX-License-Identifier: Apache-2.0
//! Typed history-graph facet kinds (ADR 0051).
//!
//! A facet's laws are not inferred from a path, thread name, or object-store
//! reuse. Git Projection, checkout, and land may only act on roots that can
//! produce [`SourceHistoryLaws`].

use std::fmt;

/// Durable facet a repository fact belongs to.
///
/// Closed on purpose: a newly added variant is a compile failure at every
/// `match` until its checkout, land, projection, sync, and purge laws are
/// written down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FacetKind {
    /// Immutable source states and trees. The only facet Git Projection,
    /// checkout, and land may select.
    SourceHistory,
    /// Encrypted runtime profiles (env/secret store). Never a checkout,
    /// land, or Git Projection target.
    ConfidentialRuntime,
    /// Collaboration operations (discussions, context). Adjacent metadata.
    Collaboration,
    /// Agent timeline operations. Adjacent execution provenance.
    AgentTimeline,
}

/// Proof that a root is Source History and may be checked out, landed, or
/// visited by Git Projection.
///
/// The only way to obtain this token is [`FacetKind::source_history_laws`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceHistoryLaws {
    _private: (),
}

impl FacetKind {
    /// Every defined facet. Tests use this to prove exclusion is closed.
    pub const ALL: [Self; 4] = [
        Self::SourceHistory,
        Self::ConfidentialRuntime,
        Self::Collaboration,
        Self::AgentTimeline,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceHistory => "source-history",
            Self::ConfidentialRuntime => "confidential-runtime",
            Self::Collaboration => "collaboration",
            Self::AgentTimeline => "agent-timeline",
        }
    }

    /// Source History laws, or `None` when this facet cannot be checked out,
    /// landed, or selected by Git Projection.
    pub const fn source_history_laws(self) -> Option<SourceHistoryLaws> {
        match self {
            Self::SourceHistory => Some(SourceHistoryLaws { _private: () }),
            Self::ConfidentialRuntime | Self::Collaboration | Self::AgentTimeline => None,
        }
    }

    pub const fn may_checkout(self) -> bool {
        self.source_history_laws().is_some()
    }

    pub const fn may_land(self) -> bool {
        self.source_history_laws().is_some()
    }

    pub const fn git_projection_visits(self) -> bool {
        self.source_history_laws().is_some()
    }

    /// Refuse worktree materialization unless this is Source History.
    pub const fn require_worktree_materialization(self) -> Result<SourceHistoryLaws, Self> {
        match self.source_history_laws() {
            Some(laws) => Ok(laws),
            None => Err(self),
        }
    }

    /// Refuse land/merge-into-HEAD unless this is Source History.
    pub const fn require_land(self) -> Result<SourceHistoryLaws, Self> {
        match self.source_history_laws() {
            Some(laws) => Ok(laws),
            None => Err(self),
        }
    }

    /// Refuse Git Projection unless this is Source History.
    pub const fn require_git_projection(self) -> Result<SourceHistoryLaws, Self> {
        match self.source_history_laws() {
            Some(laws) => Ok(laws),
            None => Err(self),
        }
    }
}

impl fmt::Display for FacetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl SourceHistoryLaws {
    pub const fn may_checkout(self) -> bool {
        true
    }

    pub const fn may_land(self) -> bool {
        true
    }

    pub const fn git_projection_visits(self) -> bool {
        true
    }
}

const _: () = assert!(FacetKind::SourceHistory.git_projection_visits());
const _: () = assert!(FacetKind::SourceHistory.may_checkout());
const _: () = assert!(FacetKind::SourceHistory.may_land());
const _: () = assert!(!FacetKind::ConfidentialRuntime.git_projection_visits());
const _: () = assert!(!FacetKind::ConfidentialRuntime.may_checkout());
const _: () = assert!(!FacetKind::ConfidentialRuntime.may_land());
const _: () = assert!(!FacetKind::Collaboration.git_projection_visits());
const _: () = assert!(!FacetKind::AgentTimeline.git_projection_visits());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_source_history_has_projection_checkout_and_land_laws() {
        for kind in FacetKind::ALL {
            let allowed = kind == FacetKind::SourceHistory;
            assert_eq!(kind.may_checkout(), allowed, "{kind}");
            assert_eq!(kind.may_land(), allowed, "{kind}");
            assert_eq!(kind.git_projection_visits(), allowed, "{kind}");
            assert_eq!(kind.source_history_laws().is_some(), allowed, "{kind}");
        }
    }

    #[test]
    fn confidential_runtime_is_refused_at_every_source_history_chokepoint() {
        let kind = FacetKind::ConfidentialRuntime;
        assert_eq!(kind.require_worktree_materialization(), Err(kind));
        assert_eq!(kind.require_land(), Err(kind));
        assert_eq!(kind.require_git_projection(), Err(kind));
    }

    #[test]
    fn source_history_laws_are_the_only_projectable_token() {
        let laws = FacetKind::SourceHistory
            .source_history_laws()
            .expect("source history yields laws");
        assert!(laws.may_checkout());
        assert!(laws.may_land());
        assert!(laws.git_projection_visits());
    }
}
