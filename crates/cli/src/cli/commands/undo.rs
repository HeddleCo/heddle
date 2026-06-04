// SPDX-License-Identifier: Apache-2.0
//! Undo and redo commands.

use objects::store::ObjectStore;
use anyhow::{Result, anyhow};
use objects::object::{ChangeId, ContentHash};
use oplog::{OpBatch, RedactionUndoClass};
use refs::UNDO_RECOVERY_HANDLE;
use repo::{Repository, ThreadManager};
use serde::Serialize;

use super::{
    action_line::print_next,
    advice::RecoveryAdvice,
    command_catalog::{ActionFields, ActionTemplate},
    git_overlay_health::{RepositoryVerificationState, build_repository_verification_state},
    undo_apply::{
        RedoOp, UndoOp, acquire_undo_redo_lock, preflight_redo_batches, preflight_undo_batches,
        undo_redo_transaction_id,
    },
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
/// see or clobber this internal pointer. `heddle goto .undo-recovery` resolves
/// it via the reserved [`refs::UNDO_RECOVERY_HANDLE`], which `resolve_refspec`
/// routes to the internal pointer BEFORE any user ref — and whose leading `.`
/// makes it uncreatable as a user ref, so it is unshadowable in both directions.
const UNDO_RECOVERY_MARKER: &str = UNDO_RECOVERY_HANDLE;

#[derive(Serialize)]
struct OpListOutput {
    output_kind: &'static str,
    batches: Vec<OpBatchOutput>,
}

#[derive(Serialize)]
struct OpBatchOutput {
    batch_id: u64,
    timestamp: String,
    undone: bool,
    partial: bool,
    operations: Vec<OpListEntry>,
}

#[derive(Serialize)]
struct OpListEntry {
    id: u64,
    description: String,
    timestamp: String,
    undone: bool,
}

#[derive(Serialize)]
struct UndoRedoOutput {
    output_kind: &'static str,
    status: &'static str,
    action: String,
    message: String,
    batches: Vec<OpBatchOutput>,
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
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    if list && preview {
        return Err(anyhow!(undo_mode_conflict_advice()));
    }

    if list {
        let scope = repo.op_scope();
        // Count only user-facing batches toward `depth`, dropping the record-less
        // commit sentinels (an `undo`/`redo`'s marker-only batch) BEFORE the
        // limit applies. Filtering after a fixed-count fetch made `--depth 1`
        // return empty whenever the latest op was itself an undo/redo, whose
        // marker is the newest batch (heddle#355 cid 3330867777).
        let batches: Vec<OpBatch> = repo
            .oplog()
            .recent_user_batches_scoped(depth, Some(&scope))?;
        let output = OpListOutput {
            output_kind: "undo_list",
            batches: batches.iter().map(build_batch_output).collect(),
        };

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

    let scope = repo.op_scope();
    let batches = repo.oplog().undo_batches_scoped(steps, Some(&scope))?;

    if batches.is_empty() {
        return Err(anyhow!(empty_history_advice("undo", "undo")));
    }

    // Run safety pre-flights before the `--preview` short-circuit so
    // preview output is honest about refusals. Preview must not
    // advertise "Would undo …" for a chain the real command would
    // reject.
    ensure_redaction_undo_safe(&repo, &batches, allow_redact_undo)?;
    ensure_thread_worktree_undo_safe(&repo, &batches)?;
    preflight_undo_execution(&repo, &batches)?;

    if preview {
        let output = UndoRedoOutput {
            output_kind: "undo",
            status: "preview",
            action: "undo".to_string(),
            message: format!(
                "Would undo {} batch{}",
                batches.len(),
                if batches.len() == 1 { "" } else { "es" }
            ),
            batches: batches.iter().map(build_batch_output).collect(),
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
            println!(
                "{}",
                human_undo_redo_message("undo", output.batches.len(), true)
            );
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
    // silently discarded. The reset below hard-resets the Git mirror and
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
    let transaction_id = undo_redo_transaction_id("undo", &scope, generation, &batches);
    let updated_batches = repo::atomic::execute(
        &repo,
        UndoOp::new(batches, recovery_state, transaction_id),
    )
    .map_err(|e| anyhow!(e))?;

    let post_undo_repo = Repository::open(repo.root())?;
    let post_undo_trust = build_repository_verification_state(&post_undo_repo);
    let recommended_action = ActionFields::from_action(&post_undo_trust.recommended_action);
    let output = UndoRedoOutput {
        output_kind: "undo",
        status: "completed",
        action: "undo".to_string(),
        message: format!(
            "Undone {} batch{}",
            updated_batches.len(),
            if updated_batches.len() == 1 { "" } else { "es" }
        ),
        batches: updated_batches.iter().map(build_batch_output).collect(),
        next_action: recommended_action.action.clone(),
        next_action_template: recommended_action.template.clone(),
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
            human_undo_redo_message("undo", output.batches.len(), false)
        );
        if cli.verbose > 0 {
            print_batches(&output.batches);
        } else {
            print_human_history(&output.batches);
        }
        print_head(&post_undo_repo)?;
        if let Some(state) = &output.recovery_state {
            println!(
                "Preserved pre-undo state {} as `{}` (recover with `heddle goto {}`)",
                style::change_id(state),
                UNDO_RECOVERY_MARKER,
                UNDO_RECOVERY_MARKER,
            );
        }
        if let Some(trust) = &output.trust {
            print_post_undo_trust(trust);
        }
    }

    Ok(())
}

pub fn cmd_redo(cli: &Cli, steps: usize, preview: bool) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    // Serialize against concurrent undo/redo (mirror of `cmd_undo`; heddle#355
    // cid 3330867776).
    let _undo_redo_lock = acquire_undo_redo_lock(&repo)?;

    let scope = repo.op_scope();
    let batches = repo.oplog().redo_batches_scoped(steps, Some(&scope))?;

    if batches.is_empty() {
        return Err(anyhow!(empty_history_advice("redo", "redo")));
    }

    // Same preview-honesty rule as `cmd_undo`: run pre-flight refusals
    // before the `--preview` short-circuit so preview surfaces the
    // refusal instead of advertising "Would redo …" for a chain the
    // real command will reject.
    ensure_redaction_redo_supported(&batches)?;
    ensure_redo_states_reachable(&repo, &batches)?;

    if preview {
        let output = UndoRedoOutput {
            output_kind: "redo",
            status: "preview",
            action: "redo".to_string(),
            message: format!(
                "Would redo {} batch{}",
                batches.len(),
                if batches.len() == 1 { "" } else { "es" }
            ),
            batches: batches.iter().map(build_batch_output).collect(),
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
            println!(
                "{}",
                human_undo_redo_message("redo", output.batches.len(), true)
            );
            if cli.verbose > 0 {
                print_batches(&output.batches);
            } else {
                print_human_history(&output.batches);
            }
        }

        return Ok(());
    }

    ensure_worktree_clean(&repo, "redo")?;
    preflight_redo_batches(&repo, &batches)?;

    // heddle#355: replay + mark-redone run as ONE atomic transaction so a
    // failure mid-redo rewinds every applied step (mirror of `cmd_undo`).
    let generation = repo.oplog().head_id()?;
    let transaction_id = undo_redo_transaction_id("redo", &scope, generation, &batches);
    let updated_batches =
        repo::atomic::execute(&repo, RedoOp::new(batches, transaction_id)).map_err(|e| anyhow!(e))?;

    let post_redo_trust = build_repository_verification_state(&repo);
    let recommended_action = ActionFields::from_action(&post_redo_trust.recommended_action);
    let output = UndoRedoOutput {
        output_kind: "redo",
        status: "completed",
        action: "redo".to_string(),
        message: format!(
            "Redone {} batch{}",
            updated_batches.len(),
            if updated_batches.len() == 1 { "" } else { "es" }
        ),
        batches: updated_batches.iter().map(build_batch_output).collect(),
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
            human_undo_redo_message("redo", output.batches.len(), false)
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

fn undo_mode_conflict_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "undo_mode_conflict",
        "Use either --list or --preview, not both",
        "Run `heddle undo --list` to inspect history, or `heddle undo --preview` to preview the next undo.",
        "--list and --preview are mutually exclusive undo modes",
        "combining them would make the command output ambiguous between history listing and undo preview",
        "repository state was left unchanged",
        "heddle undo --list",
        vec![
            "heddle undo --list".to_string(),
            "heddle undo --preview".to_string(),
        ],
    )
}

fn empty_history_advice(action: &str, noun: &str) -> RecoveryAdvice {
    let kind = if action == "undo" {
        "nothing_to_undo"
    } else {
        "nothing_to_redo"
    };
    RecoveryAdvice::safety_refusal(
        kind,
        format!("Nothing to {action}"),
        "Inspect recent undo history with `heddle undo --list`.",
        format!("there are no {noun} entries in the current checkout lane"),
        format!("{action} would need to move Heddle and Git state, but no eligible batch exists"),
        "repository state was left unchanged",
        "heddle undo --list",
        vec!["heddle undo --list".to_string()],
    )
}

fn build_batch_output(batch: &OpBatch) -> OpBatchOutput {
    let (undone, partial) = batch_status(batch);
    let timestamp = batch
        .entries
        .iter()
        .map(|entry| entry.timestamp)
        .max()
        .map(format_timestamp)
        .unwrap_or_else(|| "unknown".to_string());

    OpBatchOutput {
        batch_id: batch.id,
        timestamp,
        undone,
        partial,
        operations: batch
            .entries
            .iter()
            .map(|entry| OpListEntry {
                id: entry.id,
                description: entry.operation.description(),
                timestamp: format_timestamp(entry.timestamp),
                undone: entry.undone,
            })
            .collect(),
    }
}

fn batch_status(batch: &OpBatch) -> (bool, bool) {
    let any_undone = batch.entries.iter().any(|entry| entry.undone);
    let all_undone = batch.entries.iter().all(|entry| entry.undone);
    (all_undone, any_undone && !all_undone)
}

fn format_timestamp(timestamp: chrono::DateTime<chrono::Utc>) -> String {
    timestamp.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn human_undo_redo_message(action: &str, count: usize, preview: bool) -> String {
    let noun = if count == 1 {
        "saved change"
    } else {
        "saved changes"
    };
    let verb = match (action, preview) {
        ("undo", true) => "Would undo",
        ("undo", false) => "Undid",
        ("redo", true) => "Would redo",
        ("redo", false) => "Redid",
        _ if preview => "Would apply",
        _ => "Applied",
    };
    format!("{verb} {count} {noun}")
}

fn print_human_history(batches: &[OpBatchOutput]) {
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
    if description.starts_with("git checkpoint ") {
        return "Git commit written".to_string();
    }
    description.to_string()
}

fn print_batches(batches: &[OpBatchOutput]) {
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
    if matches!(trust.status.as_str(), "dirty_worktree" | "uncaptured") {
        "changes to save".to_string()
    } else {
        trust.status.clone()
    }
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
/// inverse would restore is still present in the object store. If any state
/// is missing we refuse before touching the worktree or marking batches
/// undone — letting the apply path discover the gap mid-flight would leave
/// the repository half-rewound (partial worktree apply, batch unmarked).
///
/// "Missing" here means a destructive boundary has been crossed: typically
/// `gc --prune` reached past the live oplog window, or an oplog backup was
/// restored without its underlying objects. The user gets a single clear
/// message instead of a raw `state not found` from deep in `goto`.
fn ensure_undo_states_reachable(repo: &Repository, batches: &[OpBatch]) -> Result<()> {
    let mut missing: Vec<(u64, ChangeId)> = Vec::new();
    for batch in batches {
        for entry in &batch.entries {
            for needed in entry.operation.states_required_for_undo() {
                if !repo.store().has_state(&needed)? {
                    missing.push((entry.id, needed));
                }
            }
        }
    }
    if missing.is_empty() {
        return Ok(());
    }

    let shorts: Vec<String> = missing
        .iter()
        .map(|(op_id, id)| format!("op {} -> {}", op_id, id.short()))
        .collect();
    Err(anyhow!(RecoveryAdvice::safety_refusal(
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
    )))
}

/// Symmetric to `ensure_undo_states_reachable`: walk every batch we'd
/// redo and verify the post-state it would advance to is still in the
/// object store. Without this check, a `gc --prune` that reached past
/// the FF redo target would surface as a raw `state not found` from
/// inside `goto`, identical in shape to the undo destructive-boundary
/// case. The redo arms that touch the object store at apply time are
/// `Snapshot`, `Goto`, `ThreadCreate`, `ThreadUpdate`, `MarkerCreate`,
/// and `FastForwardV2`; all carry the post-state SHA directly. The
/// legacy V1 `FastForward` redo re-resolves `source_thread → tip` and
/// has its own error path, so we skip it here.
fn ensure_redo_states_reachable(repo: &Repository, batches: &[OpBatch]) -> Result<()> {
    let mut missing: Vec<(u64, ChangeId)> = Vec::new();
    for batch in batches {
        for entry in &batch.entries {
            for needed in entry.operation.states_required_for_redo() {
                if !repo.store().has_state(&needed)? {
                    missing.push((entry.id, needed));
                }
            }
        }
    }
    if missing.is_empty() {
        return Ok(());
    }

    let shorts: Vec<String> = missing
        .iter()
        .map(|(op_id, id)| format!("op {} -> {}", op_id, id.short()))
        .collect();
    Err(anyhow!(RecoveryAdvice::safety_refusal(
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
    )))
}

/// Pre-flight check for redaction-related ops in the batch chain.
///
/// Three refusal cases, all surfaced before any state mutation:
///
/// 1. **Purge.** Purge is irreversible by design — the bytes are gone
///    from local storage. The CLI rejects the whole undo so operators
///    aren't surprised by a half-applied chain.
///
/// 2. **Redact without opt-in.** Undoing a `Redact` removes the
///    redaction record so subsequent materializes substitute the
///    original bytes again — i.e. previously-hidden content becomes
///    readable. Pre-fix this was a silent no-op (heddle#98); the fix
///    actually reverses the redaction. To prevent a casual
///    `heddle undo -n N` from re-exposing redacted content, the
///    inverse runs only when the user passes `--allow-redact-undo`.
///
/// 3. **Redact whose bytes have been purged.** The Redaction record is
///    then the only audit trail for "these bytes were destroyed".
///    Removing it would lie about local storage state and the
///    materialize path would fail with a missing-blob error instead
///    of restoring content. Refused regardless of the opt-in flag.
fn ensure_redaction_undo_safe(
    repo: &Repository,
    batches: &[OpBatch],
    allow_redact_undo: bool,
) -> Result<()> {
    struct RedactSummary {
        op_id: u64,
        blob: ContentHash,
        state: ChangeId,
        path: String,
    }

    let mut purge_ops: Vec<(u64, ContentHash)> = Vec::new();
    let mut redact_ops: Vec<RedactSummary> = Vec::new();

    for batch in batches {
        for entry in &batch.entries {
            match entry.operation.redaction_undo_class() {
                RedactionUndoClass::Purge { redaction_id } => {
                    purge_ops.push((entry.id, *redaction_id))
                }
                RedactionUndoClass::Redact { blob, state, path } => redact_ops.push(RedactSummary {
                    op_id: entry.id,
                    blob: *blob,
                    state: *state,
                    path: path.to_string(),
                }),
                RedactionUndoClass::Other => {}
            }
        }
    }

    if !purge_ops.is_empty() {
        let shorts: Vec<String> = purge_ops
            .iter()
            .map(|(op_id, redaction_id)| {
                format!("op {} (redaction {})", op_id, redaction_id.short())
            })
            .collect();
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
            "irreversible_purge_undo",
            format!(
                "Refusing to undo: `heddle purge` is irreversible by design — the blob bytes have been physically removed from local storage and cannot be reconstructed. Affected op(s): {}",
                shorts.join(", ")
            ),
            "Restore the bytes from a backup if you need them, or run `heddle undo --list` and target an earlier op past the purge.",
            "the undo chain contains purge operation(s) whose blob bytes are gone from local storage",
            "undoing purge would claim to restore bytes Heddle no longer has",
            "no undo mutation was applied",
            "heddle undo --list",
            vec!["heddle undo --list".to_string()],
        )));
    }

    if redact_ops.is_empty() {
        return Ok(());
    }

    // Refuse if any redaction in the chain has its bytes already
    // purged — checked first so the precise "purged audit trail" error
    // wins over the generic opt-in prompt. We match by (blob, state,
    // path) rather than by the oplog-stored `redaction_id` because
    // setting `purged_at` shifts the on-disk record's content hash.
    let mut purged_redacts: Vec<&RedactSummary> = Vec::new();
    for r in &redact_ops {
        if repo.redaction_is_purged(&r.blob, &r.state, &r.path)? {
            purged_redacts.push(r);
        }
    }
    if !purged_redacts.is_empty() {
        let shorts: Vec<String> = purged_redacts
            .iter()
            .map(|r| format!("op {} (blob {} at {})", r.op_id, r.blob.short(), r.path))
            .collect();
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
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
        )));
    }

    if !allow_redact_undo {
        let shorts: Vec<String> = redact_ops
            .iter()
            .map(|r| format!("op {} (blob {} at {})", r.op_id, r.blob.short(), r.path))
            .collect();
        return Err(anyhow!(RecoveryAdvice::safety_refusal(
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
        )));
    }

    Ok(())
}

/// Pre-flight for `heddle redo`: refuse when the batch chain contains a
/// `Redact` or `Purge` op. Neither has a faithful re-apply path today —
/// `OpRecord::Redact` doesn't carry the full `Redaction` (reason,
/// redactor, signature, etc.), so a re-application would invent fields,
/// and `Purge` is irreversible. Refusing pre-mutation keeps multi-batch
/// chains from being half-redone.
fn ensure_redaction_redo_supported(batches: &[OpBatch]) -> Result<()> {
    let mut blocking: Vec<(u64, &'static str)> = Vec::new();
    for batch in batches {
        for entry in &batch.entries {
            if let Some(label) = entry.operation.redo_unsupported_label() {
                blocking.push((entry.id, label));
            }
        }
    }
    if blocking.is_empty() {
        return Ok(());
    }
    let shorts: Vec<String> = blocking
        .iter()
        .map(|(op_id, kind)| format!("op {} ({})", op_id, kind))
        .collect();
    Err(anyhow!(RecoveryAdvice::safety_refusal(
        "redaction_redo_unsupported",
        format!(
            "Refusing to redo: `Redact` and `Purge` ops do not have a re-apply path. Affected op(s): {}",
            shorts.join(", ")
        ),
        "Re-run `heddle redact apply` (or `heddle purge apply`) to re-establish the operation.",
        "the oplog entry doesn't preserve the full Redaction record (reason, redactor, signature) needed to recreate it, and Purge is irreversible by design",
        "redo would invent redaction metadata or claim to recreate purged bytes",
        "no redo mutation was applied",
        "heddle redact apply",
        vec![
            "heddle redact apply".to_string(),
            "heddle purge apply".to_string(),
        ],
    )))
}

/// Cross-thread undo safety: refuse to undo any `ThreadCreate` whose
/// ThreadManager record carries a materialized worktree path that still
/// exists on disk. The undo inverse only deletes the thread ref — without
/// the matching worktree teardown the directory at `materialized_path` is
/// left orphaned with a broken `.heddle/HEAD` and a phantom record. The
/// canonical teardown verb is `heddle thread drop <name> --delete-thread`,
/// which goes through `drop_thread_silent` (thread_cmd.rs:562) to unmount
/// virtualized threads, rm the execution path, mark the record Abandoned,
/// strip agent registry entries, and finally remove the ref. Once that
/// has run the path no longer exists and a subsequent `heddle undo` of
/// the original create proceeds.
///
/// A stale record whose `materialized_path` points at a non-existent
/// directory is *not* a refusal — the user has already torn the worktree
/// down by hand and the undo's record-cleanup pass will sweep the orphan
/// up. See docs/design/cross-thread-undo.md "Contract" rule 5.
fn ensure_thread_worktree_undo_safe(repo: &Repository, batches: &[OpBatch]) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    let mut blocking: Vec<(u64, String, std::path::PathBuf)> = Vec::new();

    for batch in batches {
        for entry in &batch.entries {
            // V1 and V2 thread-creates (the rename batch's new-name arm and all
            // post-heddle#23-r2 creates) share the worktree-orphan hazard on
            // undo; every other record is irrelevant to this preflight.
            let Some(name) = entry.operation.thread_worktree_undo_hazard_name() else {
                continue;
            };
            // The `ThreadCreate` inverse converges the name to EMPTY — it removes
            // EVERY record filed under the name, not just the `find_by_thread`
            // winner. So the preflight must check the FULL same-name set: a
            // non-winner duplicate with a live `materialized_path` would otherwise
            // pass this gate and have its worktree orphaned by the converge.
            for record in manager.snapshot_records(name)? {
                let Some(path) = record.materialized_path.as_ref() else {
                    continue;
                };
                if path.exists() {
                    blocking.push((entry.id, name.to_string(), path.clone()));
                }
            }
        }
    }

    if blocking.is_empty() {
        return Ok(());
    }

    let shorts: Vec<String> = blocking
        .iter()
        .map(|(op_id, name, path)| {
            format!(
                "op {} (thread '{}', worktree {})",
                op_id,
                name,
                path.display()
            )
        })
        .collect();
    let first_drop_command = blocking
        .first()
        .map(|(_, name, _)| format!("heddle thread drop {name} --delete-thread"))
        .unwrap_or_else(|| "heddle undo --list".to_string());
    Err(anyhow!(RecoveryAdvice::safety_refusal(
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
    )))
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

        // Record a `ThreadCreateV2` for the name; its undo converges the name to
        // empty, removing BOTH same-name records.
        std::fs::write(temp.path().join("f.txt"), "x").unwrap();
        let state = repo.snapshot(Some("s".to_string()), None).unwrap().change_id;
        let scope = repo.op_scope();
        repo.oplog()
            .record_batch_scoped(
                vec![OpRecord::ThreadCreateV2 {
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
                OpRecord::ThreadCreateV2 { name, .. } if name == "feature/x"
            ))),
            "the recorded ThreadCreateV2 is the undoable batch"
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
