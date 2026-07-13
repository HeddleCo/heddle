// SPDX-License-Identifier: Apache-2.0
//! Pure revert planning: empty-diff gate + message assembly.
//!
//! Owns decision logic for `heddle revert` that can be decided from facts alone:
//! - whether the parent→target tree diff is empty (nothing to inverse)
//! - default commit message and human/JSON success strings
//! - stable recovery-advice kind token for the empty-diff refusal
//!
//! Tree materialization, worktree FS, RecoveryAdvice construction, and snapshot
//! I/O stay CLI-owned.

// ---------------------------------------------------------------------------
// Empty-diff preflight
// ---------------------------------------------------------------------------

/// Pure preflight for revert from the parent→target change count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertPlan {
    /// Diff is empty; refuse with no-changes recovery advice.
    NoChanges,
    /// Proceed: apply inverse changes and optionally snapshot.
    Proceed,
}

/// Plan revert from how many paths differ between parent and target trees.
///
/// Call after tree-diff I/O that yields a change set (or its length).
pub fn plan_revert(change_count: usize) -> RevertPlan {
    if revert_has_no_changes(change_count) {
        RevertPlan::NoChanges
    } else {
        RevertPlan::Proceed
    }
}

/// True when the parent→target diff has zero file changes.
pub fn revert_has_no_changes(change_count: usize) -> bool {
    change_count == 0
}

/// Stable recovery-advice `kind` for empty-diff refusal.
pub fn no_changes_to_revert_kind() -> &'static str {
    "no_changes_to_revert"
}

/// Inspect command suggested when revert refuses on an empty diff.
pub fn revert_inspect_command(state_short: &str) -> String {
    format!("heddle show {state_short}")
}

// ---------------------------------------------------------------------------
// Message assembly
// ---------------------------------------------------------------------------

/// Default commit message when the user did not pass `--message`.
pub fn default_revert_commit_message(state_short: &str) -> String {
    format!("Revert {state_short}")
}

/// Whether success output targets JSON message shape vs human text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertMessageMode {
    /// Human terminal lines.
    Text,
    /// JSON `message` field.
    Json,
}

/// Outcome after inverse apply (with or without snapshot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertOutcome {
    /// `--no-commit`: inverse applied to worktree only.
    AppliedNotCommitted,
    /// Snapshot created with the inverse tree.
    Committed,
}

/// Facts for assembling a success message after revert I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevertSuccessFacts<'a> {
    pub outcome: RevertOutcome,
    pub state_short: &'a str,
    /// New change id short form when [`RevertOutcome::Committed`].
    pub new_state_id_short: Option<&'a str>,
}

/// Human/JSON success message for a completed revert.
///
/// Matches historical CLI strings:
/// - no-commit text: `Reverted {state} (not committed)`
/// - no-commit JSON: `Changes applied to worktree (not committed)`
/// - committed text: `Reverted {state} as {new}`
/// - committed JSON: `Created revert state {new}`
pub fn revert_success_message(facts: &RevertSuccessFacts<'_>, mode: RevertMessageMode) -> String {
    match (facts.outcome, mode) {
        (RevertOutcome::AppliedNotCommitted, RevertMessageMode::Text) => {
            format!("Reverted {} (not committed)", facts.state_short)
        }
        (RevertOutcome::AppliedNotCommitted, RevertMessageMode::Json) => {
            "Changes applied to worktree (not committed)".to_string()
        }
        (RevertOutcome::Committed, RevertMessageMode::Text) => {
            let new_id = facts.new_state_id_short.unwrap_or("");
            format!("Reverted {} as {}", facts.state_short, new_id)
        }
        (RevertOutcome::Committed, RevertMessageMode::Json) => {
            let new_id = facts.new_state_id_short.unwrap_or("");
            format!("Created revert state {new_id}")
        }
    }
}

/// Summary line for the empty-diff RecoveryAdvice body (CLI wraps RecoveryAdvice).
pub fn no_changes_to_revert_summary(state_short: &str) -> String {
    format!("No changes to revert in state {state_short}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_diff_gate() {
        assert_eq!(plan_revert(0), RevertPlan::NoChanges);
        assert_eq!(plan_revert(1), RevertPlan::Proceed);
        assert_eq!(plan_revert(3), RevertPlan::Proceed);
        assert!(revert_has_no_changes(0));
        assert!(!revert_has_no_changes(2));
        assert_eq!(no_changes_to_revert_kind(), "no_changes_to_revert");
        assert_eq!(revert_inspect_command("abc1234"), "heddle show abc1234");
        assert!(no_changes_to_revert_summary("abc").contains("abc"));
    }

    #[test]
    fn default_and_success_messages() {
        assert_eq!(
            default_revert_commit_message("hs-deadbee"),
            "Revert hs-deadbee"
        );

        let no_commit = RevertSuccessFacts {
            outcome: RevertOutcome::AppliedNotCommitted,
            state_short: "hs-aaaa",
            new_state_id_short: None,
        };
        assert_eq!(
            revert_success_message(&no_commit, RevertMessageMode::Text),
            "Reverted hs-aaaa (not committed)"
        );
        assert_eq!(
            revert_success_message(&no_commit, RevertMessageMode::Json),
            "Changes applied to worktree (not committed)"
        );

        let committed = RevertSuccessFacts {
            outcome: RevertOutcome::Committed,
            state_short: "hs-aaaa",
            new_state_id_short: Some("hs-bbbb"),
        };
        assert_eq!(
            revert_success_message(&committed, RevertMessageMode::Text),
            "Reverted hs-aaaa as hs-bbbb"
        );
        assert_eq!(
            revert_success_message(&committed, RevertMessageMode::Json),
            "Created revert state hs-bbbb"
        );
    }
}
