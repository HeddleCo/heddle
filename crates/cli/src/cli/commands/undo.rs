// SPDX-License-Identifier: Apache-2.0
//! Undo and redo commands.
//!
//! Domain list/planning lives in [`heddle_core::undo`]; this module owns lock
//! acquisition, apply, text/json render, and mutation-side RecoveryAdvice.

use anyhow::{Result, anyhow};
use heddle_core::{
    LiveThreadWorktree, UndoApplyPreflightError, UndoBatchSummary, UndoHistoryAction,
    UndoListReport, check_redaction_redo_supported, check_redaction_undo_safe,
    check_states_reachable, check_thread_worktree_undo_safe, collect_redaction_undo_facts,
    collect_redo_required_states, collect_thread_worktree_hazards, collect_undo_required_states,
    human_operation_description as core_human_operation_description,
    human_post_undo_trust_status as core_human_post_undo_trust_status, human_undo_redo_message,
    list_undo_history, live_materialized_path_blocks_undo, plan_redo_batches, plan_undo_apply,
    plan_undo_batches, summarize_batch, validate_undo_list_preview_modes,
};
use objects::store::ObjectStore;
use oplog::OpBatch;
use refs::UNDO_RECOVERY_HANDLE;
use repo::{Repository, ThreadManager};
use serde::Serialize;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    command_catalog::{ActionFields, ActionTemplate, heddle_action},
    undo_apply::{
        RedoOp, UndoOp, acquire_undo_redo_lock, preflight_redo_batches, preflight_undo_batches,
        undo_redo_transaction_id,
    },
    verification_health::{RepositoryVerificationState, build_repository_verification_state},
    worktree_safety::ensure_worktree_clean,
};
use crate::cli::{Cli, should_output_json, style};

/// Well-known handle that `undo` records the pre-undo state under, so the
/// worktree content the undone batch absorbed stays a first-class, addressable
/// recovery point in heddle's thread history. A single rolling ref
/// (ORIG_HEAD-style): each undo overwrites it with its own pre-undo tip.
///
/// Invariant: this is a heddle-INTERNAL ref (`refs::RefManager::set_undo_recovery`,
/// stored as `UNDO_RECOVERY` beside the per-checkout `HEAD`), NOT a user marker.
/// It is scoped to the same checkout as the undo/redo history it recovers
/// (`op_scope`): in objectstore-pointer worktrees the ref root is shared, so a
/// shared-root recovery pointer would let a `heddle undo` in one checkout
/// clobber a sibling's — keying it to the local `HEAD` keeps each checkout's
/// recovery state its own. No heddle-internal bookkeeping ref may live in a
/// user-writable namespace (`refs/markers/`, `refs/threads/`, `refs/remotes/`):
/// doing so coupled recovery to a user-writable name and let the `MarkerDelete`
/// undo inverse collide with it.
/// `apply_undo_batch` replays only user-marker/thread inverses, so it can never
/// see or clobber this internal pointer. `heddle undo --recover` reads it
/// directly, without routing the internal handle through any user-ref resolver.
const UNDO_RECOVERY_MARKER: &str = UNDO_RECOVERY_HANDLE;

#[derive(Serialize)]
struct UndoRedoOutput {
    output_kind: &'static str,
    status: &'static str,
    action: String,
    message: String,
    batches: Vec<UndoBatchSummary>,
    next_action: Option<String>,
    next_action_template: Option<ActionTemplate>,
    recommended_action: Option<String>,
    recommended_action_template: Option<ActionTemplate>,
    /// heddle#305: the pre-undo state preserved for recovery, and the marker
    /// pointing at it. Present only on a completed `undo`; omitted from the
    /// wire when absent (preview / redo).
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_marker: Option<String>,
    #[serde(skip_serializing)]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "verification")]
    trust: Option<RepositoryVerificationState>,
}

pub fn cmd_undo(
    cli: &Cli,
    steps: usize,
    list: bool,
    depth: usize,
    preview: bool,
    allow_redact_undo: bool,
) -> Result<()> {
    let repo = cli.open_repo()?;

    validate_undo_list_preview_modes(list, preview).map_err(|e| anyhow!(e))?;

    if list {
        // Domain filtering (user batches, checkout scope, depth before markers)
        // lives in heddle_core::list_undo_history (heddle#355 cid 3330867777).
        let output: UndoListReport = list_undo_history(&repo, depth)?;

        if should_output_json(cli, Some(repo.config())) {
            println!("{}", serde_json::to_string(&output)?);
        } else {
            println!("Recent undo history (showing up to {}):", depth);
            if output.batches.is_empty() {
                println!("  No saved changes to undo");
            } else if cli.verbose > 0 {
                print_batches(&output.batches);
            } else {
                print_human_history(&output.batches);
            }
        }

        return Ok(());
    }

    // Serialize the whole select→apply→commit against concurrent undo/redo so two
    // invocations can't collide on the generation-derived transaction id (see
    // `acquire_undo_redo_lock`; heddle#355 cid 3330867776). Held until the
    // command returns, covering the `--preview` short-circuit and the commit.
    let _undo_redo_lock = acquire_undo_redo_lock(&repo)?;

    let selected = plan_undo_batches(&repo, steps)?;
    let scope = repo.op_scope();

    // Run safety pre-flights before the `--preview` short-circuit so
    // preview output is honest about refusals. Preview must not
    // advertise "Would undo …" for a chain the real command would
    // reject. Pure decision logic lives in heddle_core::undo; this
    // layer supplies FS/store facts and maps typed refusals to advice.
    ensure_redaction_undo_safe(&repo, &selected.batches, allow_redact_undo)?;
    ensure_thread_worktree_undo_safe(&repo, &selected.batches)?;
    preflight_undo_execution(&repo, &selected.batches)?;

    let apply_plan = plan_undo_apply(selected, preview);

    if preview {
        let output = UndoRedoOutput {
            output_kind: "undo",
            status: "preview",
            action: "undo".to_string(),
            message: apply_plan.message.clone(),
            batches: apply_plan.batch_summaries(),
            next_action: None,
            next_action_template: None,
            recommended_action: None,
            recommended_action_template: None,
            recovery_state: None,
            recovery_marker: None,
            trust: None,
        };

        if should_output_json(cli, Some(repo.config())) {
            println!("{}", serde_json::to_string(&output)?);
        } else {
            println!("{}", apply_plan.human_message);
            if cli.verbose > 0 {
                print_batches(&output.batches);
            } else {
                print_human_history(&output.batches);
            }
        }

        return Ok(());
    }

    // heddle#305: capture the pre-undo state into thread history BEFORE the
    // reset, so the worktree content the undone batch(es) absorbed is never
    // silently discarded. The reset below hard-resets the Git projection ref and
    // rewinds the heddle thread; recording the current tip in the internal
    // recovery ref keeps it a first-class, addressable recovery point —
    // durable even if a later divergent capture/commit strands the redo path.
    // The preflights above guarantee the worktree is clean, so the tip's tree
    // *is* the pre-undo worktree. Durability lives in heddle's immutable store
    // + refs; undo never records itself as Git history.
    //
    // heddle#305 r2: this is written to a heddle-INTERNAL ref, not a user
    // marker — see UNDO_RECOVERY_MARKER. Keeping it out of `refs/markers/`
    // means the `MarkerDelete` undo inverse can never collide with it.
    //
    // heddle#355: the recovery-ref write and every batch's worktree rewrite +
    // mark-undone now run inside ONE atomic transaction (`UndoOp`), so a
    // failure mid-undo rewinds every applied step back to the pre-undo state
    // instead of leaving the repo half-rewound. The preflights above still run
    // outside the transaction (their structured refusals are unchanged).
    let recovery_state = repo.head()?;
    let generation = repo.oplog().head_id()?;
    let transaction_id = undo_redo_transaction_id("undo", &scope, generation, &apply_plan.batches);
    let updated_batches = repo::atomic::execute(
        &repo,
        UndoOp::new(apply_plan.batches, recovery_state, transaction_id),
    )
    .map_err(|e| anyhow!(e))?;

    let post_undo_repo = Repository::open(repo.root())?;
    let post_undo_trust = build_repository_verification_state(&post_undo_repo);
    let recommended_action = ActionFields::from_action(&post_undo_trust.recommended_action);
    let recovery_action = recovery_state
        .map(|_| ActionFields::from_action(&heddle_action(["undo", "--recover"])))
        .unwrap_or_else(ActionFields::none);
    let count = updated_batches.len();
    let output = UndoRedoOutput {
        output_kind: "undo",
        status: "completed",
        action: "undo".to_string(),
        message: heddle_core::machine_undo_redo_message(UndoHistoryAction::Undo, count, false),
        batches: updated_batches.iter().map(summarize_batch).collect(),
        next_action: recovery_action.action,
        next_action_template: recovery_action.template,
        recommended_action: recommended_action.action,
        recommended_action_template: recommended_action.template,
        recovery_state: recovery_state.map(|state| state.short()),
        recovery_marker: recovery_state.map(|_| UNDO_RECOVERY_MARKER.to_string()),
        trust: Some(post_undo_trust),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!(
            "{}",
            human_undo_redo_message(UndoHistoryAction::Undo, count, false)
        );
        if cli.verbose > 0 {
            print_batches(&output.batches);
        } else {
            print_human_history(&output.batches);
        }
        print_head(&post_undo_repo)?;
        if let Some(state) = &output.recovery_state {
            println!(
                "Preserved pre-undo state {} as `{}`",
                style::state_id(state),
                UNDO_RECOVERY_MARKER,
            );
            print_next("heddle undo --recover");
        }
        if let Some(trust) = &output.trust {
            print_post_undo_trust(trust);
        }
    }

    Ok(())
}

pub fn cmd_undo_recover(cli: &Cli) -> Result<()> {
    let repo = cli.open_repo()?;
    let _undo_redo_lock = acquire_undo_redo_lock(&repo)?;
    ensure_no_active_operation(&repo, "recover the pre-undo state")?;

    let recovery_state = repo.refs().get_undo_recovery()?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::safety_refusal(
            "undo_recovery_unavailable",
            "No pre-undo recovery state is available",
            "Inspect undoable history with `heddle undo --list`.",
            "this checkout has not preserved a state through `heddle undo`",
            "recovery has no recorded state to materialize",
            "HEAD and worktree files were left unchanged",
            "heddle undo --list",
            vec!["heddle undo --list".to_string()],
        ))
    })?;
    if !repo.store().has_state(&recovery_state)? {
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "undo_recovery_state_missing",
            format!(
                "The preserved pre-undo state {} is missing",
                recovery_state.short()
            ),
            "Restore the missing state from a trusted backup, then retry `heddle undo --recover`.",
            "the checkout-local recovery ref points to an unavailable state",
            "recovery cannot materialize a state whose object is missing",
            "HEAD and worktree files were left unchanged",
            "heddle fsck",
            vec!["heddle fsck".to_string()],
        )));
    }

    ensure_worktree_clean(&repo, "recover the pre-undo state")?;
    repo.restore_state_tree_to_worktree(&recovery_state)?;

    let recovered_repo = Repository::open(repo.root())?;
    let trust = build_repository_verification_state(&recovered_repo);
    let recommended_action = ActionFields::from_action(&trust.recommended_action);
    let output = UndoRedoOutput {
        output_kind: "undo_recover",
        status: "completed",
        action: "recover".to_string(),
        message: "restored the state preserved by the most recent undo as worktree changes"
            .to_string(),
        batches: Vec::new(),
        next_action: recommended_action.action.clone(),
        next_action_template: recommended_action.template.clone(),
        recommended_action: recommended_action.action,
        recommended_action_template: recommended_action.template,
        recovery_state: Some(recovery_state.short()),
        recovery_marker: Some(UNDO_RECOVERY_MARKER.to_string()),
        trust: Some(trust),
    };

    if should_output_json(cli, Some(recovered_repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!(
            "Recovered pre-undo state {} as worktree changes",
            style::state_id(&recovery_state.short())
        );
        if let Some(trust) = &output.trust {
            print_post_undo_trust(trust);
        }
    }

    Ok(())
}

pub fn cmd_redo(cli: &Cli, steps: usize, preview: bool) -> Result<()> {
    let repo = cli.open_repo()?;

    // Serialize against concurrent undo/redo (mirror of `cmd_undo`; heddle#355
    // cid 3330867776).
    let _undo_redo_lock = acquire_undo_redo_lock(&repo)?;

    let selected = plan_redo_batches(&repo, steps)?;
    let scope = repo.op_scope();

    // Same preview-honesty rule as `cmd_undo`: run pre-flight refusals
    // before the `--preview` short-circuit so preview surfaces the
    // refusal instead of advertising "Would redo …" for a chain the
    // real command will reject.
    ensure_redaction_redo_supported(&selected.batches)?;
    ensure_redo_states_reachable(&repo, &selected.batches)?;

    let apply_plan = plan_undo_apply(selected, preview);

    if preview {
        let output = UndoRedoOutput {
            output_kind: "redo",
            status: "preview",
            action: "redo".to_string(),
            message: apply_plan.message.clone(),
            batches: apply_plan.batch_summaries(),
            next_action: None,
            next_action_template: None,
            recommended_action: None,
            recommended_action_template: None,
            recovery_state: None,
            recovery_marker: None,
            trust: None,
        };

        if should_output_json(cli, Some(repo.config())) {
            println!("{}", serde_json::to_string(&output)?);
        } else {
            println!("{}", apply_plan.human_message);
            if cli.verbose > 0 {
                print_batches(&output.batches);
            } else {
                print_human_history(&output.batches);
            }
        }

        return Ok(());
    }

    ensure_worktree_clean(&repo, "redo")?;
    preflight_redo_batches(&repo, &apply_plan.batches)?;

    // heddle#355: replay + mark-redone run as ONE atomic transaction so a
    // failure mid-redo rewinds every applied step (mirror of `cmd_undo`).
    let generation = repo.oplog().head_id()?;
    let transaction_id = undo_redo_transaction_id("redo", &scope, generation, &apply_plan.batches);
    let updated_batches =
        repo::atomic::execute(&repo, RedoOp::new(apply_plan.batches, transaction_id))
            .map_err(|e| anyhow!(e))?;

    let post_redo_trust = build_repository_verification_state(&repo);
    let recommended_action = ActionFields::from_action(&post_redo_trust.recommended_action);
    let count = updated_batches.len();
    let output = UndoRedoOutput {
        output_kind: "redo",
        status: "completed",
        action: "redo".to_string(),
        message: heddle_core::machine_undo_redo_message(UndoHistoryAction::Redo, count, false),
        batches: updated_batches.iter().map(summarize_batch).collect(),
        next_action: recommended_action.action.clone(),
        next_action_template: recommended_action.template.clone(),
        recommended_action: recommended_action.action,
        recommended_action_template: recommended_action.template,
        recovery_state: None,
        recovery_marker: None,
        trust: Some(post_redo_trust),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!(
            "{}",
            human_undo_redo_message(UndoHistoryAction::Redo, count, false)
        );
        if cli.verbose > 0 {
            print_batches(&output.batches);
        } else {
            print_human_history(&output.batches);
        }
        print_head(&repo)?;
    }

    Ok(())
}

fn print_human_history(batches: &[UndoBatchSummary]) {
    for batch in batches {
        let status = if batch.undone {
            " undone"
        } else if batch.partial {
            " partial"
        } else {
            ""
        };
        println!("  {}{}", style::dim(&batch.timestamp), style::dim(status));
        for entry in &batch.operations {
            let entry_status = if entry.undone { " (undone)" } else { "" };
            println!(
                "    - {}{}",
                human_operation_description(&entry.description),
                style::dim(entry_status)
            );
        }
    }
}

fn human_operation_description(description: &str) -> String {
    core_human_operation_description(description)
}

fn print_batches(batches: &[UndoBatchSummary]) {
    for batch in batches {
        let status = if batch.undone {
            " (undone)"
        } else if batch.partial {
            " (partial)"
        } else {
            ""
        };
        let op_count = batch.operations.len();
        println!(
            "  Batch {}{} {} op{}",
            batch.batch_id,
            status,
            op_count,
            if op_count == 1 { "" } else { "s" }
        );
        for entry in &batch.operations {
            let entry_status = if entry.undone { " (undone)" } else { "" };
            println!(
                "    {} {} {}{}",
                entry.id, entry.timestamp, entry.description, entry_status
            );
        }
    }
}

fn print_head(repo: &Repository) -> Result<()> {
    if let Some(id) = repo.head()? {
        println!("Now at: {}", id.short());
    }
    Ok(())
}

fn print_post_undo_trust(trust: &RepositoryVerificationState) {
    println!("Verification: {}", human_post_undo_trust_status(trust));
    if !trust.recommended_action.trim().is_empty() {
        print_next(&trust.recommended_action);
    }
}

fn human_post_undo_trust_status(trust: &RepositoryVerificationState) -> String {
    core_human_post_undo_trust_status(&trust.status)
}

fn preflight_undo_execution(repo: &Repository, batches: &[OpBatch]) -> Result<()> {
    ensure_no_active_operation(repo, "undo")?;
    ensure_worktree_clean(repo, "undo")?;
    // Refuse before mutating anything when a state the batch needs to
    // restore is missing from the object store — typically because `gc
    // --prune` or a truncated oplog has reached past the live window.
    // Letting `apply_undo_batch` discover this mid-apply would leave the
    // repo half-undone (worktree partially rewritten, batch not marked).
    ensure_undo_states_reachable(repo, batches)?;
    preflight_undo_batches(repo, batches)
}

fn ensure_no_active_operation(repo: &Repository, action: &str) -> Result<()> {
    let Some(operation) = repo.operation_status()? else {
        return Ok(());
    };
    let primary_command = operation.next_action.clone();
    let mut recovery_commands = vec![primary_command.clone()];
    if !recovery_commands
        .iter()
        .any(|command| command == "heddle abort")
    {
        recovery_commands.push("heddle abort".to_string());
    }
    if !recovery_commands
        .iter()
        .any(|command| command == "heddle verify")
    {
        recovery_commands.push("heddle verify".to_string());
    }
    Err(anyhow!(RecoveryAdvice::safety_refusal(
        "operation_in_progress",
        format!("Refusing to {action}: {}", operation.message),
        format!("Finish or abort the active operation with `{primary_command}` before retrying."),
        format!(
            "{} {} is {}",
            operation.scope, operation.kind, operation.state
        ),
        format!("{action} would move repository state while an operation still owns the checkout"),
        "no undo mutation was applied",
        primary_command,
        recovery_commands,
    )))
}

/// Walk every batch we're about to undo and verify that each state the
/// inverse would restore is still present in the object store. Domain
/// collection + refusal kind live in heddle-core; store lookup stays here.
fn ensure_undo_states_reachable(repo: &Repository, batches: &[OpBatch]) -> Result<()> {
    let mut missing = Vec::new();
    for required in collect_undo_required_states(batches) {
        if !repo.store().has_state(&required.state)? {
            missing.push(required);
        }
    }
    check_states_reachable(UndoHistoryAction::Undo, &missing)
        .map_err(map_undo_apply_preflight_error)
}

/// Symmetric to `ensure_undo_states_reachable` for redo post-states.
fn ensure_redo_states_reachable(repo: &Repository, batches: &[OpBatch]) -> Result<()> {
    let mut missing = Vec::new();
    for required in collect_redo_required_states(batches) {
        if !repo.store().has_state(&required.state)? {
            missing.push(required);
        }
    }
    check_states_reachable(UndoHistoryAction::Redo, &missing)
        .map_err(map_undo_apply_preflight_error)
}

/// Pre-flight check for redaction-related ops in the batch chain.
///
/// Pure decision precedence (purge → purged-redact → opt-in) lives in
/// [`check_redaction_undo_safe`]; this wrapper supplies `redaction_is_purged`
/// store facts and maps typed refusals to recovery advice.
fn ensure_redaction_undo_safe(
    repo: &Repository,
    batches: &[OpBatch],
    allow_redact_undo: bool,
) -> Result<()> {
    let facts = collect_redaction_undo_facts(batches);
    let mut purged_redact_op_ids = Vec::new();
    for r in &facts.redacts {
        // Match by (blob, state, path) rather than oplog-stored redaction_id
        // because setting purged_at shifts the on-disk record's content hash.
        if repo.redaction_is_purged(&r.blob, &r.state, &r.path)? {
            purged_redact_op_ids.push(r.op_id);
        }
    }
    check_redaction_undo_safe(&facts, &purged_redact_op_ids, allow_redact_undo)
        .map_err(map_undo_apply_preflight_error)
}

/// Pre-flight for `heddle undo --redo`: pure redaction-redo support check.
fn ensure_redaction_redo_supported(batches: &[OpBatch]) -> Result<()> {
    check_redaction_redo_supported(batches).map_err(map_undo_apply_preflight_error)
}

/// Cross-thread undo safety: refuse to undo any `ThreadCreate` whose
/// ThreadManager record carries a materialized worktree path that still
/// exists on disk. Hazard names come from the pure domain scan; path
/// existence is resolved here (full same-name set, not winner-only).
fn ensure_thread_worktree_undo_safe(repo: &Repository, batches: &[OpBatch]) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    let mut live = Vec::new();

    for hazard in collect_thread_worktree_hazards(batches) {
        // The ThreadCreate inverse converges the name to EMPTY — it removes
        // EVERY record under the name, not just the find_by_thread winner.
        for record in manager.snapshot_records(&hazard.thread_name)? {
            let Some(path) = record.materialized_path.as_ref() else {
                continue;
            };
            if live_materialized_path_blocks_undo(path.exists()) {
                live.push(LiveThreadWorktree {
                    op_id: hazard.op_id,
                    thread_name: hazard.thread_name.clone(),
                    path: path.clone(),
                });
            }
        }
    }

    check_thread_worktree_undo_safe(&live).map_err(map_undo_apply_preflight_error)
}

/// Map typed domain preflight refusals to the existing CLI RecoveryAdvice
/// messages (stable kind strings + recovery commands).
fn map_undo_apply_preflight_error(err: UndoApplyPreflightError) -> anyhow::Error {
    match err {
        UndoApplyPreflightError::IrreversiblePurge { ops } => {
            let shorts: Vec<String> = ops
                .iter()
                .map(|op| format!("op {} (redaction {})", op.op_id, op.redaction_id.short()))
                .collect();
            anyhow!(RecoveryAdvice::safety_refusal(
                "irreversible_purge_undo",
                format!(
                    "Refusing to undo: `heddle redact purge apply` is irreversible by design — the blob bytes have been physically removed from local storage and cannot be reconstructed. Affected op(s): {}",
                    shorts.join(", ")
                ),
                "Restore the bytes from a backup if you need them, or run `heddle undo --list` and target an earlier op past the purge.",
                "the undo chain contains purge operation(s) whose blob bytes are gone from local storage",
                "undoing purge would claim to restore bytes Heddle no longer has",
                "no undo mutation was applied",
                "heddle undo --list",
                vec!["heddle undo --list".to_string()],
            ))
        }
        UndoApplyPreflightError::RedactionBytesPurged { ops } => {
            let shorts: Vec<String> = ops
                .iter()
                .map(|r| format!("op {} (blob {} at {})", r.op_id, r.blob.short(), r.path))
                .collect();
            anyhow!(RecoveryAdvice::safety_refusal(
                "redaction_bytes_purged",
                format!(
                    "Refusing to undo: at least one redaction in this chain has had its bytes purged ({}). Purge is irreversible.",
                    shorts.join(", ")
                ),
                "Inspect redactions with `heddle redact list`; restore the bytes from backup before attempting a different recovery.",
                "the redaction record is now the only audit trail that those bytes were destroyed",
                "removing it would lie about local storage and a subsequent materialize would fail with a missing-blob error rather than restore content",
                "no undo mutation was applied",
                "heddle redact list",
                vec![
                    "heddle redact list".to_string(),
                    "heddle undo --list".to_string()
                ],
            ))
        }
        UndoApplyPreflightError::RedactionUndoRequiresConfirmation { ops } => {
            let shorts: Vec<String> = ops
                .iter()
                .map(|r| format!("op {} (blob {} at {})", r.op_id, r.blob.short(), r.path))
                .collect();
            anyhow!(RecoveryAdvice::safety_refusal(
                "redaction_undo_requires_confirmation",
                format!(
                    "Refusing to undo a `heddle redact apply`: the inverse removes the redaction record so subsequent materializes restore the original bytes, which would re-expose previously-hidden content. Affected op(s): {}",
                    shorts.join(", ")
                ),
                "Pass `--allow-redact-undo` to confirm.",
                "undo would re-expose previously-hidden content",
                "the redaction record would be removed and future materialization would restore the original bytes",
                "no undo mutation was applied",
                "heddle undo --allow-redact-undo",
                vec!["heddle undo --allow-redact-undo".to_string()],
            ))
        }
        UndoApplyPreflightError::RedactionRedoUnsupported { ops } => {
            let shorts: Vec<String> = ops
                .iter()
                .map(|op| format!("op {} ({})", op.op_id, op.label))
                .collect();
            anyhow!(RecoveryAdvice::safety_refusal(
                "redaction_redo_unsupported",
                format!(
                    "Refusing to redo: `Redact` and `Purge` ops do not have a re-apply path. Affected op(s): {}",
                    shorts.join(", ")
                ),
                "Re-run `heddle redact apply` (or `heddle redact purge apply`) to re-establish the operation.",
                "the oplog entry doesn't preserve the full Redaction record (reason, redactor, signature) needed to recreate it, and Purge is irreversible by design",
                "redo would invent redaction metadata or claim to recreate purged bytes",
                "no redo mutation was applied",
                "heddle redact apply",
                vec![
                    "heddle redact apply".to_string(),
                    "heddle redact purge apply".to_string(),
                ],
            ))
        }
        UndoApplyPreflightError::ThreadWorktreeUndoUnsafe { live } => {
            let shorts: Vec<String> = live
                .iter()
                .map(|item| {
                    format!(
                        "op {} (thread '{}', worktree {})",
                        item.op_id,
                        item.thread_name,
                        item.path.display()
                    )
                })
                .collect();
            let first_drop_command = live
                .first()
                .map(|item| format!("heddle thread drop {} --delete-thread", item.thread_name))
                .unwrap_or_else(|| "heddle undo --list".to_string());
            anyhow!(RecoveryAdvice::safety_refusal(
                "thread_worktree_undo_unsafe",
                format!(
                    "Refusing to undo: at least one `thread create` in this chain has an attached materialized worktree that would be orphaned by the inverse ({}).",
                    shorts.join(", ")
                ),
                format!(
                    "Tear the first worktree down with `{first_drop_command}`, then re-run `heddle undo`."
                ),
                "undo chain includes thread create operation(s) whose materialized worktrees still exist",
                "undo would remove thread refs while leaving worktree directories and `.heddle/HEAD` pointing at missing threads",
                "no undo mutation was applied",
                first_drop_command.clone(),
                vec![first_drop_command, "heddle undo --list".to_string()],
            ))
        }
        UndoApplyPreflightError::UndoStateMissing { missing } => {
            let shorts: Vec<String> = missing
                .iter()
                .map(|m| format!("op {} -> {}", m.op_id, m.state.short()))
                .collect();
            anyhow!(RecoveryAdvice::safety_refusal(
                "undo_state_missing",
                format!(
                    "Refusing to undo: prior state(s) needed to restore have been garbage-collected or are otherwise missing from the object store ({})",
                    shorts.join(", ")
                ),
                "Restore the missing states from a backup, or run `heddle undo --list` and pick an entry past the boundary.",
                "a destructive boundary (likely `heddle maintenance gc --prune`) has been crossed past the live oplog window",
                "undo cannot rewind here without the prior states",
                "no undo mutation was applied",
                "heddle undo --list",
                vec!["heddle undo --list".to_string()],
            ))
        }
        UndoApplyPreflightError::RedoStateMissing { missing } => {
            let shorts: Vec<String> = missing
                .iter()
                .map(|m| format!("op {} -> {}", m.op_id, m.state.short()))
                .collect();
            anyhow!(RecoveryAdvice::safety_refusal(
                "redo_state_missing",
                format!(
                    "Refusing to redo: post-state(s) needed to replay have been garbage-collected or are otherwise missing from the object store ({})",
                    shorts.join(", ")
                ),
                "Restore the missing states from a backup, or re-run the original operation manually.",
                "a destructive boundary (likely `heddle maintenance gc --prune`) has been crossed past the live oplog window",
                "redo cannot replay here without the post-states",
                "no redo mutation was applied",
                "heddle log",
                vec!["heddle log".to_string()],
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use oplog::{OpLogBackend, OpRecord};
    use tempfile::TempDir;

    use super::*;

    fn record(
        id: &str,
        thread: &str,
        materialized: Option<std::path::PathBuf>,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> repo::Thread {
        repo::Thread {
            id: id.to_string(),
            thread: thread.to_string(),
            target_thread: None,
            parent_thread: None,
            mode: repo::ThreadMode::Solid,
            state: repo::ThreadState::Active,
            base_state: "base".to_string(),
            base_root: "root".to_string(),
            current_state: Some("base".to_string()),
            merged_state: None,
            task: None,
            execution_path: std::path::PathBuf::from("/work/exec"),
            materialized_path: materialized,
            changed_paths: vec![],
            impact_categories: vec![],
            heavy_impact_paths: vec![],
            promotion_suggested: false,
            freshness: repo::ThreadFreshness::Current,
            verification_summary: repo::ThreadVerificationSummary::default(),
            confidence_summary: repo::ThreadConfidenceSummary::default(),
            integration_policy_result: repo::ThreadIntegrationPolicy::default(),
            created_at: chrono::Utc::now(),
            updated_at,
            ephemeral: None,
            auto: false,
            shared_target_dir: None,
        }
    }

    /// The worktree-safety preflight must check EVERY record the `ThreadCreate`
    /// inverse will remove — the converge empties the WHOLE same-name set — not
    /// just the `find_by_thread` winner (cid 3331603138). A non-winner duplicate
    /// with a LIVE materialized worktree, sitting behind a winner whose path no
    /// longer exists, must still trip the refusal; otherwise the converge orphans
    /// that live worktree. Fails against the winner-only check (the winner's path
    /// is gone, so it passed) and passes against the set-aware check.
    #[test]
    fn worktree_undo_safe_checks_full_same_name_set_not_just_winner() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let manager = ThreadManager::new(repo.heddle_dir());

        // A live worktree directory for the NON-winner duplicate.
        let live_worktree = temp.path().join("live-worktree");
        std::fs::create_dir_all(&live_worktree).unwrap();

        let now = chrono::Utc::now();
        // Winner (newer `updated_at`) — its materialized path does NOT exist, so a
        // winner-only check would not block.
        let winner = record(
            "rec-winner",
            "feature/x",
            Some(temp.path().join("gone-worktree")),
            now,
        );
        manager.save(&winner).unwrap();
        // Non-winner duplicate (older) — its materialized path EXISTS on disk.
        let dup = record(
            "rec-dup",
            "feature/x",
            Some(live_worktree.clone()),
            now - chrono::Duration::seconds(60),
        );
        manager.save(&dup).unwrap();

        // Sanity: the winner-only view would NOT block — the winner is selected and
        // its path is gone.
        let winner_seen = manager.find_by_thread("feature/x").unwrap().unwrap();
        assert_eq!(winner_seen.id, "rec-winner");
        assert!(!winner_seen.materialized_path.unwrap().exists());

        // Record a `ThreadCreate` for the name; its undo converges the name to
        // empty, removing BOTH same-name records.
        std::fs::write(temp.path().join("f.txt"), "x").unwrap();
        let state = repo.snapshot(Some("s".to_string()), None).unwrap().state_id;
        let scope = repo.op_scope();
        repo.oplog()
            .record_batch_scoped(
                vec![OpRecord::ThreadCreate {
                    name: "feature/x".to_string(),
                    state,
                    manager_snapshot: None,
                }],
                Some(&scope),
            )
            .unwrap();
        let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        assert!(
            batches.iter().any(|b| b.entries.iter().any(|e| matches!(
                &e.operation,
                OpRecord::ThreadCreate { name, .. } if name == "feature/x"
            ))),
            "the recorded ThreadCreate is the undoable batch"
        );

        let result = ensure_thread_worktree_undo_safe(&repo, &batches);
        assert!(
            result.is_err(),
            "the live non-winner duplicate worktree must trip the refusal"
        );
        let msg = format!("{:#}", result.unwrap_err());
        assert!(
            msg.contains(&live_worktree.display().to_string()),
            "the refusal names the live non-winner worktree path: {msg}"
        );
    }
}
