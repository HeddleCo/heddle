// SPDX-License-Identifier: Apache-2.0
//! Pure cherry-pick planning: resolve gate + message assembly.
//!
//! Owns decision logic for `heddle cherry-pick` that can be decided from facts
//! alone:
//! - whether the requested commit/state resolved
//! - default commit message and human success strings
//! - stable recovery-advice kind / JSON status tokens
//!
//! Tree materialization, dirty-worktree guards, RecoveryAdvice construction,
//! and snapshot I/O stay CLI-owned.

// ---------------------------------------------------------------------------
// Resolve preflight
// ---------------------------------------------------------------------------

/// Pure preflight for cherry-pick after state resolution I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CherryPickResolvePlan {
    /// No state matched the user-supplied commit/spec.
    NotFound,
    /// State resolved; proceed to apply tree (and optionally snapshot).
    Proceed,
}

/// Plan cherry-pick resolve from whether a state id / state object was found.
///
/// Call after `resolve_state` / `get_state` I/O that yields `Option`.
pub fn plan_cherry_pick_resolve(state_found: bool) -> CherryPickResolvePlan {
    if state_found {
        CherryPickResolvePlan::Proceed
    } else {
        CherryPickResolvePlan::NotFound
    }
}

/// True when cherry-pick must refuse because the commit/state was not found.
pub fn cherry_pick_should_refuse_not_found(state_found: bool) -> bool {
    matches!(
        plan_cherry_pick_resolve(state_found),
        CherryPickResolvePlan::NotFound
    )
}

/// Stable recovery-advice `kind` for missing commit/state.
pub fn cherry_pick_commit_not_found_kind() -> &'static str {
    "cherry_pick_commit_not_found"
}

/// Summary line for the not-found RecoveryAdvice body (CLI wraps RecoveryAdvice).
pub fn cherry_pick_commit_not_found_summary(commit: &str) -> String {
    format!("Refusing to cherry-pick: commit '{commit}' not found")
}

// ---------------------------------------------------------------------------
// Message assembly / status tokens
// ---------------------------------------------------------------------------

/// Default commit message when the user did not pass `--message`.
pub fn default_cherry_pick_commit_message(commit: &str) -> String {
    format!("Cherry-pick {commit}")
}

/// JSON `status` after a successful no-commit apply.
pub fn cherry_pick_status_applied() -> &'static str {
    "applied"
}

/// JSON `status` after a successful committed cherry-pick.
pub fn cherry_pick_status_committed() -> &'static str {
    "committed"
}

/// Outcome after tree apply (with or without snapshot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CherryPickOutcome {
    /// `--no-commit`: tree applied to worktree only.
    AppliedNotCommitted,
    /// Snapshot created from the cherry-picked tree.
    Committed,
}

/// Facts for assembling a human success line after cherry-pick I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CherryPickSuccessFacts<'a> {
    pub outcome: CherryPickOutcome,
    pub commit: &'a str,
    /// New change id short form when [`CherryPickOutcome::Committed`].
    pub new_change_id_short: Option<&'a str>,
}

/// Human success message for a completed cherry-pick.
///
/// Matches historical CLI strings:
/// - no-commit: `Applied {commit} (not committed)`
/// - committed: `Cherry-picked {commit} as {new}`
pub fn cherry_pick_human_message(facts: &CherryPickSuccessFacts<'_>) -> String {
    match facts.outcome {
        CherryPickOutcome::AppliedNotCommitted => {
            format!("Applied {} (not committed)", facts.commit)
        }
        CherryPickOutcome::Committed => {
            let new_id = facts.new_change_id_short.unwrap_or("");
            format!("Cherry-picked {} as {}", facts.commit, new_id)
        }
    }
}

/// JSON `status` token for the outcome.
pub fn cherry_pick_json_status(outcome: CherryPickOutcome) -> &'static str {
    match outcome {
        CherryPickOutcome::AppliedNotCommitted => cherry_pick_status_applied(),
        CherryPickOutcome::Committed => cherry_pick_status_committed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_gate_and_kind() {
        assert_eq!(
            plan_cherry_pick_resolve(false),
            CherryPickResolvePlan::NotFound
        );
        assert_eq!(
            plan_cherry_pick_resolve(true),
            CherryPickResolvePlan::Proceed
        );
        assert!(cherry_pick_should_refuse_not_found(false));
        assert!(!cherry_pick_should_refuse_not_found(true));
        assert_eq!(
            cherry_pick_commit_not_found_kind(),
            "cherry_pick_commit_not_found"
        );
        assert!(
            cherry_pick_commit_not_found_summary("missing").contains("commit 'missing' not found")
        );
    }

    #[test]
    fn messages_and_status_tokens() {
        assert_eq!(
            default_cherry_pick_commit_message("hd-source"),
            "Cherry-pick hd-source"
        );

        let applied = CherryPickSuccessFacts {
            outcome: CherryPickOutcome::AppliedNotCommitted,
            commit: "hd-source",
            new_change_id_short: None,
        };
        assert_eq!(
            cherry_pick_human_message(&applied),
            "Applied hd-source (not committed)"
        );
        assert_eq!(
            cherry_pick_json_status(CherryPickOutcome::AppliedNotCommitted),
            "applied"
        );

        let committed = CherryPickSuccessFacts {
            outcome: CherryPickOutcome::Committed,
            commit: "hd-source",
            new_change_id_short: Some("hd-result"),
        };
        assert_eq!(
            cherry_pick_human_message(&committed),
            "Cherry-picked hd-source as hd-result"
        );
        assert_eq!(
            cherry_pick_json_status(CherryPickOutcome::Committed),
            "committed"
        );
    }
}
