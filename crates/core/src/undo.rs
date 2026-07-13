// SPDX-License-Identifier: Apache-2.0
//! Undo list / planning domain (inspection + batch selection + apply preflight + human labels).
//!
//! Owns the **read-side** and **pure apply-path preflight** of `heddle undo`:
//! - listing user-facing oplog batches for the current checkout scope
//! - pure batch summarization for stable machine JSON field names
//! - selecting the next N undo/redo batches
//! - shared domain refusals for mode conflict and empty history
//! - pure redaction / thread-worktree / state-reachability preflights given
//!   caller-supplied batch facts (no FS / store I/O in the decision layer)
//! - apply step order (reverse within batch for undo; forward for redo) and
//!   preview / completed message strings
//!
//! Locks, store lookups, dirty-worktree checks, git-checkpoint simulation, and
//! the apply engine remain CLI-owned (`undo.rs` + `undo_apply/*`).

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use objects::{
    HeddleError, RecoveryDetails,
    object::{ContentHash, StateId},
};
use oplog::{OpBatch, RedactionUndoClass};
use repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    ExecutionContext, HeddleReport, MachineOutputKind, OutputDiscriminator, ReportContract,
    schema_for_report,
};

/// Soften git-checkpoint batch descriptions for human undo listing.
pub fn human_operation_description(description: &str) -> String {
    if description.starts_with("git checkpoint ") {
        return "Git commit written".to_string();
    }
    description.to_string()
}

/// Soften post-undo verification status for operators.
pub fn human_post_undo_trust_status(status: &str) -> String {
    if matches!(status, "dirty_worktree" | "uncaptured") {
        "changes to save".to_string()
    } else {
        status.to_string()
    }
}

/// Machine JSON for `heddle undo --list` (stable field names).
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct UndoListReport {
    pub output_kind: &'static str,
    pub batches: Vec<UndoBatchSummary>,
}

impl UndoListReport {
    pub const CONTRACT: ReportContract = ReportContract {
        schema_name: "undo_list",
        machine_output_kind: MachineOutputKind::Json,
        output_discriminator: Some(OutputDiscriminator {
            field: "output_kind",
            value: "undo_list",
        }),
        schema: schema_for_report::<UndoListReport>,
    };
}

impl HeddleReport for UndoListReport {
    const CONTRACT: ReportContract = UndoListReport::CONTRACT;
}

/// One oplog batch as surfaced by undo list / preview / completed payloads.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct UndoBatchSummary {
    pub batch_id: u64,
    pub timestamp: String,
    pub undone: bool,
    pub partial: bool,
    pub operations: Vec<UndoOperationSummary>,
}

/// One operation inside an undo batch summary.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct UndoOperationSummary {
    pub id: u64,
    pub description: String,
    pub timestamp: String,
    pub undone: bool,
}

/// Whether empty-history advice refers to undo or redo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoHistoryAction {
    Undo,
    Redo,
}

impl UndoHistoryAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Undo => "undo",
            Self::Redo => "redo",
        }
    }

    pub fn empty_kind(self) -> &'static str {
        match self {
            Self::Undo => "nothing_to_undo",
            Self::Redo => "nothing_to_redo",
        }
    }
}

/// Result of selecting batches for a multi-step undo/redo plan.
///
/// Pure relative to apply: holds the scoped batches that would be rewound or
/// replayed. Callers run safety preflights and the apply engine separately.
#[derive(Debug, Clone)]
pub struct UndoPlan {
    pub action: UndoHistoryAction,
    pub steps_requested: usize,
    pub batches: Vec<OpBatch>,
}

impl UndoPlan {
    pub fn batch_summaries(&self) -> Vec<UndoBatchSummary> {
        self.batches.iter().map(summarize_batch).collect()
    }
}

/// List user-facing undo history for the current checkout scope.
///
/// Counts only user-facing batches toward `depth`, dropping record-less
/// transaction markers (undo/redo commit sentinels) **before** the limit
/// applies — matching `OpLog::recent_user_batches_scoped` (heddle#355).
pub fn list_undo_history(repo: &Repository, depth: usize) -> Result<UndoListReport> {
    let scope = repo.op_scope();
    let batches = repo
        .oplog()
        .recent_user_batches_scoped(depth, Some(&scope))?;
    Ok(UndoListReport {
        output_kind: "undo_list",
        batches: batches.iter().map(summarize_batch).collect(),
    })
}

/// List undo history via an [`ExecutionContext`] (embeddable facade entry).
pub fn list_undo_history_ctx(ctx: &ExecutionContext, depth: usize) -> Result<UndoListReport> {
    let repo = ctx.require_repo()?;
    list_undo_history(repo, depth)
}

/// Select the next `steps` undoable batches for the current checkout scope.
///
/// Returns [`HeddleError`] (kind `nothing_to_undo`) when no eligible batch
/// exists. Does not run worktree / redaction / reachability preflights.
pub fn plan_undo_batches(repo: &Repository, steps: usize) -> Result<UndoPlan> {
    let scope = repo.op_scope();
    let batches = repo.oplog().undo_batches_scoped(steps, Some(&scope))?;
    require_nonempty_history(UndoHistoryAction::Undo, &batches).map_err(|e| anyhow!(e))?;
    Ok(UndoPlan {
        action: UndoHistoryAction::Undo,
        steps_requested: steps,
        batches,
    })
}

/// Select the next `steps` redoable batches for the current checkout scope.
///
/// Returns [`HeddleError`] (kind `nothing_to_redo`) when no eligible batch
/// exists. Does not run worktree / redaction / reachability preflights.
pub fn plan_redo_batches(repo: &Repository, steps: usize) -> Result<UndoPlan> {
    let scope = repo.op_scope();
    let batches = repo.oplog().redo_batches_scoped(steps, Some(&scope))?;
    require_nonempty_history(UndoHistoryAction::Redo, &batches).map_err(|e| anyhow!(e))?;
    Ok(UndoPlan {
        action: UndoHistoryAction::Redo,
        steps_requested: steps,
        batches,
    })
}

/// Pure mode preflight: `--list` and `--preview` are mutually exclusive.
pub fn validate_undo_list_preview_modes(list: bool, preview: bool) -> Result<(), HeddleError> {
    if list && preview {
        Err(undo_mode_conflict())
    } else {
        Ok(())
    }
}

/// Refuse when a plan selected zero batches.
pub fn require_nonempty_history(
    action: UndoHistoryAction,
    batches: &[OpBatch],
) -> Result<(), HeddleError> {
    if batches.is_empty() {
        Err(empty_history_refusal(action))
    } else {
        Ok(())
    }
}

/// Shared advice: `undo --list` combined with `--preview`.
pub fn undo_mode_conflict() -> HeddleError {
    HeddleError::recovery(
        RecoveryDetails::safety_refusal(
            "undo_mode_conflict",
            "Use either --list or --preview, not both",
            "Run `heddle undo --list` to inspect history, or `heddle undo --preview` to preview the next undo.",
            "--list and --preview are mutually exclusive undo modes",
            "combining them would make the command output ambiguous between history listing and undo preview",
            "repository state was left unchanged",
        )
        .with_recovery_commands(vec![
            "heddle undo --list".to_string(),
            "heddle undo --preview".to_string(),
        ]),
    )
}

/// Shared advice: no undo/redo-eligible batch in the current checkout lane.
pub fn empty_history_refusal(action: UndoHistoryAction) -> HeddleError {
    let noun = action.as_str();
    HeddleError::recovery(
        RecoveryDetails::safety_refusal(
            action.empty_kind(),
            format!("Nothing to {noun}"),
            "Inspect recent undo history with `heddle undo --list`.",
            format!("there are no {noun} entries in the current checkout lane"),
            format!("{noun} would need to move Heddle and Git state, but no eligible batch exists"),
            "repository state was left unchanged",
        )
        .with_recovery_commands(vec!["heddle undo --list".to_string()]),
    )
}

/// Summarize one [`OpBatch`] into stable list/preview JSON fields.
pub fn summarize_batch(batch: &OpBatch) -> UndoBatchSummary {
    let (undone, partial) = batch_status(batch);
    let timestamp = batch
        .entries
        .iter()
        .map(|entry| entry.timestamp)
        .max()
        .map(format_timestamp)
        .unwrap_or_else(|| "unknown".to_string());

    UndoBatchSummary {
        batch_id: batch.id,
        timestamp,
        undone,
        partial,
        operations: batch
            .entries
            .iter()
            .map(|entry| UndoOperationSummary {
                id: entry.id,
                description: entry.operation.description(),
                timestamp: format_timestamp(entry.timestamp),
                undone: entry.undone,
            })
            .collect(),
    }
}

/// `(all_undone, partial)` for a batch — pure status flags for machine output.
pub fn batch_status(batch: &OpBatch) -> (bool, bool) {
    let any_undone = batch.entries.iter().any(|entry| entry.undone);
    let all_undone = batch.entries.iter().all(|entry| entry.undone);
    (all_undone, any_undone && !all_undone)
}

fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.format("%Y-%m-%d %H:%M:%S").to_string()
}

// ---------------------------------------------------------------------------
// Apply-path pure preflight + step plan
// ---------------------------------------------------------------------------

/// One entry as the apply engine will visit it (batch order fixed; entry order
/// depends on undo vs redo).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoApplyStep {
    pub batch_id: u64,
    pub entry_id: u64,
    pub description: String,
}

/// Pure apply plan after batch selection: step order + stable messages.
///
/// Does not encode FS/store preflight outcomes — callers run
/// [`check_redaction_undo_safe`], [`check_thread_worktree_undo_safe`], etc.
/// before applying or advertising preview.
#[derive(Debug, Clone)]
pub struct UndoApplyPlan {
    pub action: UndoHistoryAction,
    pub preview: bool,
    pub steps_requested: usize,
    pub batches: Vec<OpBatch>,
    /// Visit order for the apply engine (undo: reverse entries per batch).
    pub steps: Vec<UndoApplyStep>,
    /// Machine-oriented status line (`Would undo 2 batches` / `Undone 1 batch`).
    pub message: String,
    /// Human one-liner (`Would undo 2 saved changes` / `Undid 1 saved change`).
    pub human_message: String,
}

impl UndoApplyPlan {
    pub fn batch_summaries(&self) -> Vec<UndoBatchSummary> {
        self.batches.iter().map(summarize_batch).collect()
    }

    pub fn batch_count(&self) -> usize {
        self.batches.len()
    }
}

/// Build an apply plan from a selected [`UndoPlan`] (pure; no preflight).
pub fn plan_undo_apply(plan: UndoPlan, preview: bool) -> UndoApplyPlan {
    let count = plan.batches.len();
    let steps = match plan.action {
        UndoHistoryAction::Undo => plan_undo_apply_steps(&plan.batches),
        UndoHistoryAction::Redo => plan_redo_apply_steps(&plan.batches),
    };
    UndoApplyPlan {
        action: plan.action,
        preview,
        steps_requested: plan.steps_requested,
        batches: plan.batches,
        steps,
        message: machine_undo_redo_message(plan.action, count, preview),
        human_message: human_undo_redo_message(plan.action, count, preview),
    }
}

/// Undo apply order: each batch in selection order, entries reverse within batch.
pub fn plan_undo_apply_steps(batches: &[OpBatch]) -> Vec<UndoApplyStep> {
    let mut steps = Vec::new();
    for batch in batches {
        for entry in batch.entries.iter().rev() {
            steps.push(UndoApplyStep {
                batch_id: batch.id,
                entry_id: entry.id,
                description: entry.operation.description(),
            });
        }
    }
    steps
}

/// Redo apply order: each batch in selection order, entries forward within batch.
pub fn plan_redo_apply_steps(batches: &[OpBatch]) -> Vec<UndoApplyStep> {
    let mut steps = Vec::new();
    for batch in batches {
        for entry in &batch.entries {
            steps.push(UndoApplyStep {
                batch_id: batch.id,
                entry_id: entry.id,
                description: entry.operation.description(),
            });
        }
    }
    steps
}

/// Machine JSON `message` field for undo/redo preview or completed payloads.
pub fn machine_undo_redo_message(action: UndoHistoryAction, count: usize, preview: bool) -> String {
    let noun = if count == 1 { "batch" } else { "batches" };
    match (action, preview) {
        (UndoHistoryAction::Undo, true) => format!("Would undo {count} {noun}"),
        (UndoHistoryAction::Undo, false) => format!("Undone {count} {noun}"),
        (UndoHistoryAction::Redo, true) => format!("Would redo {count} {noun}"),
        (UndoHistoryAction::Redo, false) => format!("Redone {count} {noun}"),
    }
}

/// Human text status line for undo/redo preview or completed output.
pub fn human_undo_redo_message(action: UndoHistoryAction, count: usize, preview: bool) -> String {
    let noun = if count == 1 {
        "saved change"
    } else {
        "saved changes"
    };
    let verb = match (action, preview) {
        (UndoHistoryAction::Undo, true) => "Would undo",
        (UndoHistoryAction::Undo, false) => "Undid",
        (UndoHistoryAction::Redo, true) => "Would redo",
        (UndoHistoryAction::Redo, false) => "Redid",
    };
    format!("{verb} {count} {noun}")
}

/// A `Purge` op participating in undo redaction safety.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeOpRef {
    pub op_id: u64,
    pub redaction_id: ContentHash,
}

/// A `Redact` op participating in undo redaction safety.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactOpRef {
    pub op_id: u64,
    pub blob: ContentHash,
    pub state: StateId,
    pub path: String,
}

/// Batch-derived redaction facts (no store I/O).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RedactionUndoBatchFacts {
    pub purges: Vec<PurgeOpRef>,
    pub redacts: Vec<RedactOpRef>,
}

/// Collect purge/redact ops from a planned undo chain (pure scan).
pub fn collect_redaction_undo_facts(batches: &[OpBatch]) -> RedactionUndoBatchFacts {
    let mut facts = RedactionUndoBatchFacts::default();
    for batch in batches {
        for entry in &batch.entries {
            match entry.operation.redaction_undo_class() {
                RedactionUndoClass::Purge { redaction_id } => {
                    facts.purges.push(PurgeOpRef {
                        op_id: entry.id,
                        redaction_id: *redaction_id,
                    });
                }
                RedactionUndoClass::Redact { blob, state, path } => {
                    facts.redacts.push(RedactOpRef {
                        op_id: entry.id,
                        blob: *blob,
                        state: *state,
                        path: path.to_string(),
                    });
                }
                RedactionUndoClass::Other => {}
            }
        }
    }
    facts
}

/// Pure redaction-undo safety given batch facts + caller-resolved purge status.
///
/// Precedence (matches CLI):
/// 1. Any purge op → refuse (`irreversible_purge_undo`)
/// 2. Any redact whose bytes are purged → refuse (`redaction_bytes_purged`)
/// 3. Any remaining redact without `--allow-redact-undo` → refuse
///    (`redaction_undo_requires_confirmation`)
pub fn check_redaction_undo_safe(
    facts: &RedactionUndoBatchFacts,
    // Op ids of redact entries whose blob bytes have already been purged.
    purged_redact_op_ids: &[u64],
    allow_redact_undo: bool,
) -> Result<(), UndoApplyPreflightError> {
    if !facts.purges.is_empty() {
        return Err(UndoApplyPreflightError::IrreversiblePurge {
            ops: facts.purges.clone(),
        });
    }
    if facts.redacts.is_empty() {
        return Ok(());
    }
    let purged: Vec<RedactOpRef> = facts
        .redacts
        .iter()
        .filter(|r| purged_redact_op_ids.contains(&r.op_id))
        .cloned()
        .collect();
    if !purged.is_empty() {
        return Err(UndoApplyPreflightError::RedactionBytesPurged { ops: purged });
    }
    if !allow_redact_undo {
        return Err(UndoApplyPreflightError::RedactionUndoRequiresConfirmation {
            ops: facts.redacts.clone(),
        });
    }
    Ok(())
}

/// Pure: whether a materialized worktree path still on disk blocks ThreadCreate undo.
pub fn live_materialized_path_blocks_undo(path_exists: bool) -> bool {
    path_exists
}

/// ThreadCreate in the undo chain that can orphan a materialized worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadWorktreeHazard {
    pub op_id: u64,
    pub thread_name: String,
}

/// Collect ThreadCreate worktree-orphan hazards from batches (pure; no FS).
pub fn collect_thread_worktree_hazards(batches: &[OpBatch]) -> Vec<ThreadWorktreeHazard> {
    let mut out = Vec::new();
    for batch in batches {
        for entry in &batch.entries {
            if let Some(name) = entry.operation.thread_worktree_undo_hazard_name() {
                out.push(ThreadWorktreeHazard {
                    op_id: entry.id,
                    thread_name: name.to_string(),
                });
            }
        }
    }
    out
}

/// A hazard whose materialized path still exists (caller-resolved FS fact).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveThreadWorktree {
    pub op_id: u64,
    pub thread_name: String,
    pub path: PathBuf,
}

/// Pure worktree-orphan preflight: refuse when any live materialized path remains.
pub fn check_thread_worktree_undo_safe(
    live: &[LiveThreadWorktree],
) -> Result<(), UndoApplyPreflightError> {
    if live.is_empty() {
        Ok(())
    } else {
        Err(UndoApplyPreflightError::ThreadWorktreeUndoUnsafe {
            live: live.to_vec(),
        })
    }
}

/// State the apply inverse/replay must load, tagged with the owning op id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredStateRef {
    pub op_id: u64,
    pub state: StateId,
}

/// Collect states required for undo reachability (pure scan of batches).
pub fn collect_undo_required_states(batches: &[OpBatch]) -> Vec<RequiredStateRef> {
    let mut out = Vec::new();
    for batch in batches {
        for entry in &batch.entries {
            for state in entry.operation.states_required_for_undo() {
                out.push(RequiredStateRef {
                    op_id: entry.id,
                    state,
                });
            }
        }
    }
    out
}

/// Collect states required for redo reachability (pure scan of batches).
pub fn collect_redo_required_states(batches: &[OpBatch]) -> Vec<RequiredStateRef> {
    let mut out = Vec::new();
    for batch in batches {
        for entry in &batch.entries {
            for state in entry.operation.states_required_for_redo() {
                out.push(RequiredStateRef {
                    op_id: entry.id,
                    state,
                });
            }
        }
    }
    out
}

/// Pure: refuse when caller-resolved missing states are non-empty.
pub fn check_states_reachable(
    action: UndoHistoryAction,
    missing: &[RequiredStateRef],
) -> Result<(), UndoApplyPreflightError> {
    if missing.is_empty() {
        return Ok(());
    }
    match action {
        UndoHistoryAction::Undo => Err(UndoApplyPreflightError::UndoStateMissing {
            missing: missing.to_vec(),
        }),
        UndoHistoryAction::Redo => Err(UndoApplyPreflightError::RedoStateMissing {
            missing: missing.to_vec(),
        }),
    }
}

/// A redo-unsupported redaction-adjacent op (`Redact` / `Purge`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedRedoOp {
    pub op_id: u64,
    pub label: &'static str,
}

/// Collect redo-unsupported ops from batches (pure).
pub fn collect_unsupported_redo_ops(batches: &[OpBatch]) -> Vec<UnsupportedRedoOp> {
    let mut out = Vec::new();
    for batch in batches {
        for entry in &batch.entries {
            if let Some(label) = entry.operation.redo_unsupported_label() {
                out.push(UnsupportedRedoOp {
                    op_id: entry.id,
                    label,
                });
            }
        }
    }
    out
}

/// Pure redo redaction support preflight.
pub fn check_redaction_redo_supported(batches: &[OpBatch]) -> Result<(), UndoApplyPreflightError> {
    let blocking = collect_unsupported_redo_ops(batches);
    if blocking.is_empty() {
        Ok(())
    } else {
        Err(UndoApplyPreflightError::RedactionRedoUnsupported { ops: blocking })
    }
}

/// Typed apply-path preflight refusals (CLI maps to recovery advice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoApplyPreflightError {
    IrreversiblePurge { ops: Vec<PurgeOpRef> },
    RedactionBytesPurged { ops: Vec<RedactOpRef> },
    RedactionUndoRequiresConfirmation { ops: Vec<RedactOpRef> },
    RedactionRedoUnsupported { ops: Vec<UnsupportedRedoOp> },
    ThreadWorktreeUndoUnsafe { live: Vec<LiveThreadWorktree> },
    UndoStateMissing { missing: Vec<RequiredStateRef> },
    RedoStateMissing { missing: Vec<RequiredStateRef> },
}

impl UndoApplyPreflightError {
    /// Stable recovery-advice `kind` string (matches existing CLI refusals).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::IrreversiblePurge { .. } => "irreversible_purge_undo",
            Self::RedactionBytesPurged { .. } => "redaction_bytes_purged",
            Self::RedactionUndoRequiresConfirmation { .. } => {
                "redaction_undo_requires_confirmation"
            }
            Self::RedactionRedoUnsupported { .. } => "redaction_redo_unsupported",
            Self::ThreadWorktreeUndoUnsafe { .. } => "thread_worktree_undo_unsafe",
            Self::UndoStateMissing { .. } => "undo_state_missing",
            Self::RedoStateMissing { .. } => "redo_state_missing",
        }
    }
}

impl std::fmt::Display for UndoApplyPreflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.kind())
    }
}

impl std::error::Error for UndoApplyPreflightError {}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use objects::object::{ContentHash, StateId};
    use oplog::OpRecord;
    use tempfile::TempDir;

    use super::*;

    fn sample_entry(id: u64, undone: bool) -> oplog::OpEntry {
        use objects::object::Principal;
        oplog::OpEntry {
            id,
            timestamp: Utc::now(),
            operation: OpRecord::TransactionCommit {
                transaction_id: format!("t{id}"),
                op_count: 0,
            },
            undone,
            batch_id: 1,
            batch_index: id as u32,
            scope: None,
            actor: Arc::new(Principal::new("tester", "tester@example.com")),
            operation_id: None,
        }
    }

    #[test]
    fn list_preview_modes_are_mutually_exclusive() {
        assert!(validate_undo_list_preview_modes(false, false).is_ok());
        assert!(validate_undo_list_preview_modes(true, false).is_ok());
        assert!(validate_undo_list_preview_modes(false, true).is_ok());
        let err = validate_undo_list_preview_modes(true, true).unwrap_err();
        match err {
            HeddleError::Recovery(details) => {
                assert_eq!(details.kind, "undo_mode_conflict");
                assert!(details.error.contains("--list") || details.error.contains("preview"));
            }
            other => panic!("expected recovery error, got {other:?}"),
        }
    }

    #[test]
    fn empty_history_kinds_match_action() {
        let undo = empty_history_refusal(UndoHistoryAction::Undo);
        let redo = empty_history_refusal(UndoHistoryAction::Redo);
        match undo {
            HeddleError::Recovery(d) => {
                assert_eq!(d.kind, "nothing_to_undo");
                assert!(d.error.contains("Nothing to undo"));
            }
            other => panic!("unexpected {other:?}"),
        }
        match redo {
            HeddleError::Recovery(d) => {
                assert_eq!(d.kind, "nothing_to_redo");
                assert!(d.error.contains("Nothing to redo"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn batch_status_flags_partial_and_full() {
        let mixed = OpBatch {
            id: 7,
            entries: vec![sample_entry(1, true), sample_entry(2, false)],
        };
        assert_eq!(batch_status(&mixed), (false, true));

        let all = OpBatch {
            id: 8,
            entries: vec![sample_entry(3, true), sample_entry(4, true)],
        };
        assert_eq!(batch_status(&all), (true, false));

        let none = OpBatch {
            id: 9,
            entries: vec![sample_entry(5, false)],
        };
        assert_eq!(batch_status(&none), (false, false));
    }

    #[test]
    fn summarize_batch_preserves_stable_json_field_names() {
        let batch = OpBatch {
            id: 42,
            entries: vec![sample_entry(10, false)],
        };
        let summary = summarize_batch(&batch);
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(value["batch_id"], 42);
        assert!(value["timestamp"].is_string());
        assert_eq!(value["undone"], false);
        assert_eq!(value["partial"], false);
        assert!(value["operations"].is_array());
        assert_eq!(value["operations"][0]["id"], 10);
        assert!(value["operations"][0]["description"].is_string());
        assert!(value["operations"][0]["timestamp"].is_string());
        assert_eq!(value["operations"][0]["undone"], false);
    }

    #[test]
    fn list_undo_history_empty_repo_returns_empty_batches() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let report = list_undo_history(&repo, 10).unwrap();
        assert_eq!(report.output_kind, "undo_list");
        assert!(report.batches.is_empty());
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["output_kind"], "undo_list");
        assert_eq!(value["batches"], serde_json::json!([]));
    }

    #[test]
    fn plan_undo_empty_repo_refuses_with_nothing_to_undo() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let err = plan_undo_batches(&repo, 1).unwrap_err();
        let heddle = err
            .downcast_ref::<HeddleError>()
            .expect("domain refusal should be HeddleError");
        match heddle {
            HeddleError::Recovery(d) => assert_eq!(d.kind, "nothing_to_undo"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn plan_redo_empty_repo_refuses_with_nothing_to_redo() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let err = plan_redo_batches(&repo, 1).unwrap_err();
        let heddle = err
            .downcast_ref::<HeddleError>()
            .expect("domain refusal should be HeddleError");
        match heddle {
            HeddleError::Recovery(d) => assert_eq!(d.kind, "nothing_to_redo"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn list_and_plan_see_recorded_user_batch() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("f.txt"), "x").unwrap();
        let _ = repo
            .snapshot(Some("s".to_string()), None)
            .expect("snapshot");

        let list = list_undo_history(&repo, 5).unwrap();
        assert!(
            !list.batches.is_empty(),
            "snapshot should produce listable history"
        );

        let plan = plan_undo_batches(&repo, 1).unwrap();
        assert_eq!(plan.action, UndoHistoryAction::Undo);
        assert_eq!(plan.batches.len(), 1);
        assert_eq!(plan.batch_summaries().len(), 1);

        let apply = plan_undo_apply(plan, true);
        assert!(apply.preview);
        assert_eq!(apply.action, UndoHistoryAction::Undo);
        assert!(apply.message.starts_with("Would undo"));
        assert!(apply.human_message.starts_with("Would undo"));
        assert_eq!(apply.batch_count(), 1);
        assert!(!apply.steps.is_empty());
    }

    fn batch_with_entries(id: u64, entry_ids: &[u64]) -> OpBatch {
        OpBatch {
            id,
            entries: entry_ids
                .iter()
                .map(|&eid| sample_entry(eid, false))
                .collect(),
        }
    }

    #[test]
    fn undo_apply_steps_reverse_entries_within_batch() {
        let batches = vec![batch_with_entries(1, &[10, 11, 12])];
        let steps = plan_undo_apply_steps(&batches);
        assert_eq!(
            steps.iter().map(|s| s.entry_id).collect::<Vec<_>>(),
            vec![12, 11, 10]
        );
        let redo = plan_redo_apply_steps(&batches);
        assert_eq!(
            redo.iter().map(|s| s.entry_id).collect::<Vec<_>>(),
            vec![10, 11, 12]
        );
    }

    #[test]
    fn redaction_undo_preflight_precedence() {
        let blob = ContentHash::from_bytes([1u8; 32]);
        let redaction_id = ContentHash::from_bytes([2u8; 32]);
        let state = StateId::from_bytes([3u8; 32]);
        let facts = RedactionUndoBatchFacts {
            purges: vec![PurgeOpRef {
                op_id: 1,
                redaction_id,
            }],
            redacts: vec![RedactOpRef {
                op_id: 2,
                blob,
                state,
                path: "secret.txt".into(),
            }],
        };
        // Purge wins even if allow + purged list would also fire.
        let err = check_redaction_undo_safe(&facts, &[2], true).unwrap_err();
        assert_eq!(err.kind(), "irreversible_purge_undo");

        let facts_redact_only = RedactionUndoBatchFacts {
            purges: vec![],
            redacts: facts.redacts.clone(),
        };
        let err = check_redaction_undo_safe(&facts_redact_only, &[2], true).unwrap_err();
        assert_eq!(err.kind(), "redaction_bytes_purged");

        let err = check_redaction_undo_safe(&facts_redact_only, &[], false).unwrap_err();
        assert_eq!(err.kind(), "redaction_undo_requires_confirmation");

        assert!(check_redaction_undo_safe(&facts_redact_only, &[], true).is_ok());
        assert!(check_redaction_undo_safe(&RedactionUndoBatchFacts::default(), &[], false).is_ok());
    }

    #[test]
    fn thread_worktree_and_state_reachability_predicates() {
        assert!(!live_materialized_path_blocks_undo(false));
        assert!(live_materialized_path_blocks_undo(true));

        assert!(check_thread_worktree_undo_safe(&[]).is_ok());
        let live = vec![LiveThreadWorktree {
            op_id: 9,
            thread_name: "feature/x".into(),
            path: PathBuf::from("/tmp/wt"),
        }];
        let err = check_thread_worktree_undo_safe(&live).unwrap_err();
        assert_eq!(err.kind(), "thread_worktree_undo_unsafe");

        assert!(check_states_reachable(UndoHistoryAction::Undo, &[]).is_ok());
        let missing = vec![RequiredStateRef {
            op_id: 3,
            state: StateId::from_bytes([4u8; 32]),
        }];
        assert_eq!(
            check_states_reachable(UndoHistoryAction::Undo, &missing)
                .unwrap_err()
                .kind(),
            "undo_state_missing"
        );
        assert_eq!(
            check_states_reachable(UndoHistoryAction::Redo, &missing)
                .unwrap_err()
                .kind(),
            "redo_state_missing"
        );
    }

    #[test]
    fn machine_and_human_messages_match_cli_shapes() {
        assert_eq!(
            machine_undo_redo_message(UndoHistoryAction::Undo, 1, true),
            "Would undo 1 batch"
        );
        assert_eq!(
            machine_undo_redo_message(UndoHistoryAction::Undo, 2, false),
            "Undone 2 batches"
        );
        assert_eq!(
            human_undo_redo_message(UndoHistoryAction::Redo, 1, true),
            "Would redo 1 saved change"
        );
        assert_eq!(
            human_undo_redo_message(UndoHistoryAction::Redo, 3, false),
            "Redid 3 saved changes"
        );
    }
}
