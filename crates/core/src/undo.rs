// SPDX-License-Identifier: Apache-2.0
//! Undo list / planning domain (inspection + batch selection).
//!
//! Owns the **read-side** of `heddle undo`:
//! - listing user-facing oplog batches for the current checkout scope
//! - pure batch summarization for stable machine JSON field names
//! - selecting the next N undo/redo batches
//! - shared domain refusals for mode conflict and empty history
//!
//! Apply/mutation remains CLI-owned (`undo_apply/*`) until a later wave.

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use objects::{HeddleError, RecoveryDetails};
use oplog::OpBatch;
use repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    ExecutionContext, HeddleReport, MachineOutputKind, OutputDiscriminator, ReportContract,
    schema_for_report,
};

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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
    }
}
