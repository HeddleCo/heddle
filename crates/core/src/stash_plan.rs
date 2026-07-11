// SPDX-License-Identifier: Apache-2.0
//! Pure stash planning: refuse predicates, outcome status, message assembly.
//!
//! Owns decision logic for `heddle stash` that can be decided from facts alone:
//! - whether push should refuse (clean worktree)
//! - whether pop / apply / drop / show should refuse (empty stash stack)
//! - outcome status tokens and human/JSON success messages after mutations
//! - list/show empty-display helpers
//!
//! Stash storage I/O, worktree apply/restore, and RecoveryAdvice construction stay
//! CLI-owned. Callers gather cheap facts (worktree clean?, top entry present?),
//! invoke these helpers, then execute FS / stash-manager work.

// ---------------------------------------------------------------------------
// Refuse predicates / plans
// ---------------------------------------------------------------------------

/// Pure preflight for `heddle stash push` from worktree cleanliness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashPushPlan {
    /// Worktree has no modified/deleted/untracked paths worth stashing.
    RefuseNoChanges,
    /// Proceed: build stash tree, push entry, restore HEAD tree.
    Proceed,
}

/// Plan stash push from a pure worktree-clean fact (after status I/O).
pub fn plan_stash_push(worktree_clean: bool) -> StashPushPlan {
    if worktree_clean {
        StashPushPlan::RefuseNoChanges
    } else {
        StashPushPlan::Proceed
    }
}

/// True when push must refuse because the worktree is clean.
pub fn stash_push_should_refuse(worktree_clean: bool) -> bool {
    matches!(
        plan_stash_push(worktree_clean),
        StashPushPlan::RefuseNoChanges
    )
}

/// Pure preflight for verbs that need a top stash entry (pop/apply/drop/show).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashEntryOpPlan {
    /// Stash stack is empty; refuse with no-stash recovery advice.
    RefuseEmpty,
    /// A stash entry is available for the verb.
    Proceed,
}

/// Plan pop / apply / drop / show from whether a top entry exists.
///
/// Call after stash-manager I/O that yields `Option` (top / pop / drop result).
pub fn plan_stash_entry_op(has_stash: bool) -> StashEntryOpPlan {
    if has_stash {
        StashEntryOpPlan::Proceed
    } else {
        StashEntryOpPlan::RefuseEmpty
    }
}

/// True when pop / apply / drop / show must refuse because the stack is empty.
pub fn stash_entry_op_should_refuse(has_stash: bool) -> bool {
    matches!(
        plan_stash_entry_op(has_stash),
        StashEntryOpPlan::RefuseEmpty
    )
}

/// True when a stash stack count of zero means empty.
pub fn stash_stack_is_empty(count: usize) -> bool {
    count == 0
}

// ---------------------------------------------------------------------------
// Outcome status tokens
// ---------------------------------------------------------------------------

/// Outcome status token for a successful stash mutation.
///
/// Stable short labels suitable for machine consumers / tests. CLI human and
/// JSON message strings are derived via [`stash_mutation_message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashOutcomeStatus {
    /// `stash push` saved a new entry.
    Stashed,
    /// `stash apply` restored without dropping.
    Applied,
    /// `stash pop` applied then dropped.
    AppliedAndDropped,
    /// `stash drop` removed the top entry.
    Dropped,
    /// `stash clear` wiped the stack (including zero cleared).
    Cleared,
}

impl StashOutcomeStatus {
    /// Stable token: `"stashed" | "applied" | "applied_and_dropped" | "dropped" | "cleared"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stashed => "stashed",
            Self::Applied => "applied",
            Self::AppliedAndDropped => "applied_and_dropped",
            Self::Dropped => "dropped",
            Self::Cleared => "cleared",
        }
    }
}

/// Whether output should target JSON message shape vs human text.
///
/// Pop and clear historically differ slightly between JSON and text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashMessageMode {
    /// Human terminal lines.
    Text,
    /// JSON `message` field.
    Json,
}

/// Facts for assembling a success message after stash mutation I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StashMutationReport {
    pub status: StashOutcomeStatus,
    /// Index of the entry involved (push/apply/pop/drop when known).
    pub stash_index: Option<usize>,
    /// Number of entries cleared (`stash clear` only).
    pub cleared_count: Option<usize>,
}

impl StashMutationReport {
    pub fn stashed(index: usize) -> Self {
        Self {
            status: StashOutcomeStatus::Stashed,
            stash_index: Some(index),
            cleared_count: None,
        }
    }

    pub fn applied(index: usize) -> Self {
        Self {
            status: StashOutcomeStatus::Applied,
            stash_index: Some(index),
            cleared_count: None,
        }
    }

    pub fn applied_and_dropped(index: usize) -> Self {
        Self {
            status: StashOutcomeStatus::AppliedAndDropped,
            stash_index: Some(index),
            cleared_count: None,
        }
    }

    pub fn dropped(index: usize) -> Self {
        Self {
            status: StashOutcomeStatus::Dropped,
            stash_index: Some(index),
            cleared_count: None,
        }
    }

    pub fn cleared(count: usize) -> Self {
        Self {
            status: StashOutcomeStatus::Cleared,
            stash_index: None,
            cleared_count: Some(count),
        }
    }

    /// Index field for JSON `StashOutput.stash_index` (historical shape).
    ///
    /// Push and apply emit the index; pop/drop/clear emit `None`.
    pub fn json_stash_index(&self) -> Option<usize> {
        match self.status {
            StashOutcomeStatus::Stashed | StashOutcomeStatus::Applied => self.stash_index,
            StashOutcomeStatus::AppliedAndDropped
            | StashOutcomeStatus::Dropped
            | StashOutcomeStatus::Cleared => None,
        }
    }
}

/// Human/JSON success message for a completed stash mutation.
///
/// Matches historical CLI strings:
/// - push: `Saved stash@{N}`
/// - apply: `Applied stash@{N}`
/// - pop text: `Applied and dropped stash@{N}`; JSON: `Applied and dropped stash`
/// - drop: `Dropped stash@{N}`
/// - clear text empty: `No stashes to clear`; else / JSON always: `Cleared N stash(es)`
pub fn stash_mutation_message(report: &StashMutationReport, mode: StashMessageMode) -> String {
    match report.status {
        StashOutcomeStatus::Stashed => {
            format!("Saved stash@{{{}}}", report.stash_index.unwrap_or(0))
        }
        StashOutcomeStatus::Applied => {
            format!("Applied stash@{{{}}}", report.stash_index.unwrap_or(0))
        }
        StashOutcomeStatus::AppliedAndDropped => match mode {
            StashMessageMode::Json => "Applied and dropped stash".to_string(),
            StashMessageMode::Text => format!(
                "Applied and dropped stash@{{{}}}",
                report.stash_index.unwrap_or(0)
            ),
        },
        StashOutcomeStatus::Dropped => {
            format!("Dropped stash@{{{}}}", report.stash_index.unwrap_or(0))
        }
        StashOutcomeStatus::Cleared => {
            let count = report.cleared_count.unwrap_or(0);
            match mode {
                StashMessageMode::Text if stash_stack_is_empty(count) => {
                    "No stashes to clear".to_string()
                }
                _ => format!("Cleared {count} stash(es)"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// List / show display helpers
// ---------------------------------------------------------------------------

/// Default message when a stash entry has no user-supplied message.
pub const STASH_DEFAULT_LIST_MESSAGE: &str = "WIP on main";

/// Message shown for one list row (default when missing).
pub fn stash_list_entry_message(message: Option<&str>) -> &str {
    message.unwrap_or(STASH_DEFAULT_LIST_MESSAGE)
}

/// Format one human list line: `stash@{N}: message`.
pub fn format_stash_list_line(index: usize, message: Option<&str>) -> String {
    format!("stash@{{{index}}}: {}", stash_list_entry_message(message))
}

/// True when list text should print the empty-stack line.
pub fn stash_list_is_empty(count: usize) -> bool {
    stash_stack_is_empty(count)
}

/// True when show text should print `"Empty stash"` (no tree diffs).
pub fn stash_show_is_empty(change_count: usize) -> bool {
    change_count == 0
}

/// Kind of path change for stash show classification (mirrors tree diff kinds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashShowChangeKind {
    Modified,
    Added,
    Deleted,
    Unchanged,
}

/// Single-letter text prefix for a show row (`M`/`A`/`D`), or `None` to skip.
pub fn stash_show_change_prefix(kind: StashShowChangeKind) -> Option<&'static str> {
    match kind {
        StashShowChangeKind::Modified => Some("M"),
        StashShowChangeKind::Added => Some("A"),
        StashShowChangeKind::Deleted => Some("D"),
        StashShowChangeKind::Unchanged => None,
    }
}

/// Bucketed paths for JSON stash show (ignores unchanged).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StashShowBuckets {
    pub modified: Vec<String>,
    pub added: Vec<String>,
    pub deleted: Vec<String>,
}

/// Classify `(kind, path)` pairs into show buckets (pure).
pub fn bucket_stash_show_changes<'a, I>(changes: I) -> StashShowBuckets
where
    I: IntoIterator<Item = (StashShowChangeKind, &'a str)>,
{
    let mut buckets = StashShowBuckets::default();
    for (kind, path) in changes {
        match kind {
            StashShowChangeKind::Modified => buckets.modified.push(path.to_string()),
            StashShowChangeKind::Added => buckets.added.push(path.to_string()),
            StashShowChangeKind::Deleted => buckets.deleted.push(path.to_string()),
            StashShowChangeKind::Unchanged => {}
        }
    }
    buckets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_refuses_only_when_worktree_clean() {
        assert_eq!(plan_stash_push(true), StashPushPlan::RefuseNoChanges);
        assert_eq!(plan_stash_push(false), StashPushPlan::Proceed);
        assert!(stash_push_should_refuse(true));
        assert!(!stash_push_should_refuse(false));
    }

    #[test]
    fn entry_ops_refuse_when_stack_empty() {
        assert_eq!(plan_stash_entry_op(false), StashEntryOpPlan::RefuseEmpty);
        assert_eq!(plan_stash_entry_op(true), StashEntryOpPlan::Proceed);
        assert!(stash_entry_op_should_refuse(false));
        assert!(!stash_entry_op_should_refuse(true));
        assert!(stash_stack_is_empty(0));
        assert!(!stash_stack_is_empty(1));
    }

    #[test]
    fn outcome_status_tokens_are_stable() {
        assert_eq!(StashOutcomeStatus::Stashed.as_str(), "stashed");
        assert_eq!(StashOutcomeStatus::Applied.as_str(), "applied");
        assert_eq!(
            StashOutcomeStatus::AppliedAndDropped.as_str(),
            "applied_and_dropped"
        );
        assert_eq!(StashOutcomeStatus::Dropped.as_str(), "dropped");
        assert_eq!(StashOutcomeStatus::Cleared.as_str(), "cleared");
    }

    #[test]
    fn mutation_messages_match_historical_cli() {
        let stashed = StashMutationReport::stashed(0);
        assert_eq!(
            stash_mutation_message(&stashed, StashMessageMode::Text),
            "Saved stash@{0}"
        );
        assert_eq!(stashed.json_stash_index(), Some(0));

        let applied = StashMutationReport::applied(2);
        assert_eq!(
            stash_mutation_message(&applied, StashMessageMode::Json),
            "Applied stash@{2}"
        );
        assert_eq!(applied.json_stash_index(), Some(2));

        let pop = StashMutationReport::applied_and_dropped(1);
        assert_eq!(
            stash_mutation_message(&pop, StashMessageMode::Text),
            "Applied and dropped stash@{1}"
        );
        assert_eq!(
            stash_mutation_message(&pop, StashMessageMode::Json),
            "Applied and dropped stash"
        );
        assert_eq!(pop.json_stash_index(), None);

        let dropped = StashMutationReport::dropped(3);
        assert_eq!(
            stash_mutation_message(&dropped, StashMessageMode::Text),
            "Dropped stash@{3}"
        );
        assert_eq!(dropped.json_stash_index(), None);

        let cleared_n = StashMutationReport::cleared(2);
        assert_eq!(
            stash_mutation_message(&cleared_n, StashMessageMode::Text),
            "Cleared 2 stash(es)"
        );
        let cleared_0 = StashMutationReport::cleared(0);
        assert_eq!(
            stash_mutation_message(&cleared_0, StashMessageMode::Text),
            "No stashes to clear"
        );
        assert_eq!(
            stash_mutation_message(&cleared_0, StashMessageMode::Json),
            "Cleared 0 stash(es)"
        );
        assert_eq!(cleared_0.json_stash_index(), None);
    }

    #[test]
    fn list_and_show_helpers() {
        assert_eq!(stash_list_entry_message(None), "WIP on main");
        assert_eq!(stash_list_entry_message(Some("wip")), "wip");
        assert_eq!(format_stash_list_line(0, None), "stash@{0}: WIP on main");
        assert!(stash_list_is_empty(0));
        assert!(stash_show_is_empty(0));
        assert!(!stash_show_is_empty(2));
        assert_eq!(
            stash_show_change_prefix(StashShowChangeKind::Modified),
            Some("M")
        );
        assert_eq!(
            stash_show_change_prefix(StashShowChangeKind::Unchanged),
            None
        );

        let buckets = bucket_stash_show_changes([
            (StashShowChangeKind::Modified, "a.rs"),
            (StashShowChangeKind::Added, "b.rs"),
            (StashShowChangeKind::Deleted, "c.rs"),
            (StashShowChangeKind::Unchanged, "d.rs"),
        ]);
        assert_eq!(buckets.modified, vec!["a.rs".to_string()]);
        assert_eq!(buckets.added, vec!["b.rs".to_string()]);
        assert_eq!(buckets.deleted, vec!["c.rs".to_string()]);
    }
}
