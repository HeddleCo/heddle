// SPDX-License-Identifier: Apache-2.0
//! Compile-time and runtime FacetKind gate for Git Projection (ADR 0051).

use objects::object::{FacetKind, SourceHistoryLaws};

use crate::git_core::GitProjectionError;

const _: () = assert!(FacetKind::SourceHistory.git_projection_visits());
const _: () = assert!(!FacetKind::ConfidentialRuntime.git_projection_visits());

/// Git Projection may only select Source History. Runtime profiles, ciphertext,
/// recipient descriptors, and policy data are a different facet.
pub fn require_source_history_projection(
    kind: FacetKind,
) -> Result<SourceHistoryLaws, GitProjectionError> {
    kind.require_git_projection()
        .map_err(GitProjectionError::NonProjectableFacet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_projection_refuses_every_non_source_history_facet() {
        for kind in FacetKind::ALL {
            if kind == FacetKind::SourceHistory {
                assert!(require_source_history_projection(kind).is_ok());
            } else {
                let err = require_source_history_projection(kind).expect_err("excluded");
                assert!(
                    matches!(err, GitProjectionError::NonProjectableFacet(k) if k == kind),
                    "{err}"
                );
            }
        }
    }
}
