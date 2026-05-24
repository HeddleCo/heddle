// SPDX-License-Identifier: Apache-2.0
//! Undo and redo commands.

use std::collections::HashSet;

use anyhow::{Result, anyhow};
use objects::object::{ChangeId, ContentHash, Tree};
use oplog::{OpBatch, OpRecord};
use repo::{Repository, ThreadManager};
use serde::Serialize;

use super::{
    undo_apply::{apply_redo_batch, apply_undo_batch},
    worktree_safety::ensure_worktree_clean,
};
use crate::cli::{Cli, should_output_json};

#[derive(Serialize)]
struct OpListOutput {
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
    action: String,
    message: String,
    batches: Vec<OpBatchOutput>,
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
        return Err(anyhow!("Use either --list or --preview, not both"));
    }

    if list {
        let scope = repo.op_scope();
        let batches = repo.oplog().recent_batches_scoped(depth, Some(&scope))?;
        let output = OpListOutput {
            batches: batches.iter().map(build_batch_output).collect(),
        };

        if should_output_json(cli, Some(repo.config())) {
            println!("{}", serde_json::to_string(&output)?);
        } else {
            println!("Recent operation batches (showing up to {}):", depth);
            if output.batches.is_empty() {
                println!("  No operations");
            } else {
                print_batches(&output.batches);
            }
        }

        return Ok(());
    }

    let scope = repo.op_scope();
    let batches = repo.oplog().undo_batches_scoped(steps, Some(&scope))?;

    if batches.is_empty() {
        return Err(anyhow!("Nothing to undo"));
    }

    // Run the redaction safety pre-flight before the `--preview`
    // short-circuit so preview output is honest about refusals. A
    // `Redact` without `--allow-redact-undo`, or any batch that crosses
    // a `Purge`, must surface the same error here that the real undo
    // would surface — otherwise `--preview` advertises "Would undo …"
    // for a chain the real command would reject. The other pre-flights
    // (`ensure_worktree_clean`, `ensure_undo_states_reachable`) stay
    // post-preview: dirty worktree and gc-pruned states are conditions
    // operators expect `--preview` to ignore.
    ensure_redaction_undo_safe(&repo, &batches, allow_redact_undo)?;
    // Cross-thread safety: refuse to undo a `ThreadCreate` whose
    // ThreadManager record still has a materialized worktree on disk.
    // The inverse only deletes the ref, so silently proceeding would
    // leave the worktree directory orphaned with a broken `.heddle/HEAD`
    // and a phantom ThreadManager record. Pre-preview for the same
    // honesty reason as the redaction gate.
    ensure_thread_worktree_undo_safe(&repo, &batches)?;
    // heddle#198: a rebase batch's inverse restores the thread tip to
    // its pre-rebase state. If any blob reachable from that tree has
    // been purged since (Redact + Purge), the rewind would land the
    // worktree on a tip whose next `materialize` fails with a
    // missing-blob error. Refuse pre-mutation with a message naming
    // the affected blob — same fail-loud discipline as the Redact
    // path's purge gate in `ensure_redaction_undo_safe`.
    ensure_rebase_undo_safe(&repo, &batches)?;

    if preview {
        let output = UndoRedoOutput {
            action: "undo".to_string(),
            message: format!(
                "Would undo {} batch{}",
                batches.len(),
                if batches.len() == 1 { "" } else { "es" }
            ),
            batches: batches.iter().map(build_batch_output).collect(),
        };

        if should_output_json(cli, Some(repo.config())) {
            println!("{}", serde_json::to_string(&output)?);
        } else {
            println!("{}", output.message);
            print_batches(&output.batches);
        }

        return Ok(());
    }

    ensure_worktree_clean(&repo, "undo")?;
    // Refuse before mutating anything when a state the batch needs to
    // restore is missing from the object store — typically because `gc
    // --prune` or a truncated oplog has reached past the live window.
    // Letting `apply_undo_batch` discover this mid-apply would leave the
    // repo half-undone (worktree partially rewritten, batch not marked).
    ensure_undo_states_reachable(&repo, &batches)?;

    let mut updated_batches = Vec::with_capacity(batches.len());
    for batch in batches {
        apply_undo_batch(&repo, &batch)?;
        updated_batches.push(repo.oplog().mark_batch_undone(&batch)?);
    }

    let output = UndoRedoOutput {
        action: "undo".to_string(),
        message: format!(
            "Undone {} batch{}",
            updated_batches.len(),
            if updated_batches.len() == 1 { "" } else { "es" }
        ),
        batches: updated_batches.iter().map(build_batch_output).collect(),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", output.message);
        print_batches(&output.batches);
        print_head(&repo)?;
    }

    Ok(())
}

pub fn cmd_redo(cli: &Cli, steps: usize, preview: bool) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;

    let scope = repo.op_scope();
    let batches = repo.oplog().redo_batches_scoped(steps, Some(&scope))?;

    if batches.is_empty() {
        return Err(anyhow!("Nothing to redo"));
    }

    // Same preview-honesty rule as `cmd_undo`: run pre-flight refusals
    // before the `--preview` short-circuit so preview surfaces the
    // refusal instead of advertising "Would redo …" for a chain the
    // real command will reject.
    ensure_redaction_redo_supported(&batches)?;
    ensure_redo_states_reachable(&repo, &batches)?;

    if preview {
        let output = UndoRedoOutput {
            action: "redo".to_string(),
            message: format!(
                "Would redo {} batch{}",
                batches.len(),
                if batches.len() == 1 { "" } else { "es" }
            ),
            batches: batches.iter().map(build_batch_output).collect(),
        };

        if should_output_json(cli, Some(repo.config())) {
            println!("{}", serde_json::to_string(&output)?);
        } else {
            println!("{}", output.message);
            print_batches(&output.batches);
        }

        return Ok(());
    }

    ensure_worktree_clean(&repo, "redo")?;

    let mut updated_batches = Vec::with_capacity(batches.len());
    for batch in batches {
        apply_redo_batch(&repo, &batch)?;
        updated_batches.push(repo.oplog().mark_batch_redone(&batch)?);
    }

    let output = UndoRedoOutput {
        action: "redo".to_string(),
        message: format!(
            "Redone {} batch{}",
            updated_batches.len(),
            if updated_batches.len() == 1 { "" } else { "es" }
        ),
        batches: updated_batches.iter().map(build_batch_output).collect(),
    };

    if should_output_json(cli, Some(repo.config())) {
        println!("{}", serde_json::to_string(&output)?);
    } else {
        println!("{}", output.message);
        print_batches(&output.batches);
        print_head(&repo)?;
    }

    Ok(())
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
            for needed in states_required_for_undo(&entry.operation) {
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
    Err(anyhow!(
        "Refusing to undo: prior state(s) needed to restore have been garbage-collected or are otherwise missing from the object store ({}). \
         A destructive boundary (likely `heddle gc --prune`) has been crossed past the live oplog window — \
         undo cannot rewind here. Restore the missing states from a backup, or run `heddle undo --list` and pick an entry past the boundary.",
        shorts.join(", "),
    ))
}

/// Identify the state IDs that an inverse for `op` would need to load.
/// Variants whose undo is a no-op (e.g. `Fork`, `Collapse`, `Checkpoint`)
/// or which only mutate sidecars (`Redact`) return an empty list —
/// they don't reach into the object store, so a missing state can't
/// trip them. `Purge` is irreversible and handled by
/// `ensure_redaction_undo_safe`; it returns nothing here too.
fn states_required_for_undo(op: &OpRecord) -> Vec<ChangeId> {
    match op {
        OpRecord::Snapshot {
            prev_head: Some(prev),
            ..
        } => vec![*prev],
        OpRecord::Goto {
            prev_head: Some(prev),
            ..
        } => vec![*prev],
        OpRecord::ThreadDelete { state, .. } => vec![*state],
        OpRecord::ThreadUpdate { old_state, .. } => vec![*old_state],
        OpRecord::MarkerDelete { state, .. } => vec![*state],
        OpRecord::FastForward { pre_target_id, .. } => vec![*pre_target_id],
        OpRecord::FastForwardV2 { pre_target_id, .. } => vec![*pre_target_id],
        _ => Vec::new(),
    }
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
            for needed in states_required_for_redo(&entry.operation) {
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
    Err(anyhow!(
        "Refusing to redo: post-state(s) needed to replay have been garbage-collected or are otherwise missing from the object store ({}). \
         A destructive boundary (likely `heddle gc --prune`) has been crossed past the live oplog window — \
         redo cannot replay here. Restore the missing states from a backup, or re-run the original operation manually.",
        shorts.join(", "),
    ))
}

fn states_required_for_redo(op: &OpRecord) -> Vec<ChangeId> {
    match op {
        OpRecord::Snapshot { new_state, .. } => vec![*new_state],
        OpRecord::Goto { target, .. } => vec![*target],
        OpRecord::ThreadCreate { state, .. } => vec![*state],
        OpRecord::ThreadCreateV2 { state, .. } => vec![*state],
        OpRecord::ThreadUpdate { new_state, .. } => vec![*new_state],
        OpRecord::MarkerCreate { state, .. } => vec![*state],
        OpRecord::FastForwardV2 { post_target_id, .. } => vec![*post_target_id],
        _ => Vec::new(),
    }
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
            match &entry.operation {
                OpRecord::Purge { redaction_id, .. } => purge_ops.push((entry.id, *redaction_id)),
                OpRecord::Redact {
                    blob, state, path, ..
                } => redact_ops.push(RedactSummary {
                    op_id: entry.id,
                    blob: *blob,
                    state: *state,
                    path: path.clone(),
                }),
                _ => {}
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
        return Err(anyhow!(
            "Refusing to undo: `heddle purge` is irreversible by design — the blob bytes have been \
             physically removed from local storage and cannot be reconstructed. Affected op(s): {}. \
             Restore the bytes from a backup if you need them, or run `heddle undo --list` and \
             target an earlier op past the purge.",
            shorts.join(", "),
        ));
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
        return Err(anyhow!(
            "Refusing to undo: at least one redaction in this chain has had its bytes purged ({}). \
             The redaction record is now the only audit trail that those bytes were destroyed; \
             removing it would lie about local storage and a subsequent materialize would fail \
             with a missing-blob error rather than restore content. Purge is irreversible.",
            shorts.join(", "),
        ));
    }

    if !allow_redact_undo {
        let shorts: Vec<String> = redact_ops
            .iter()
            .map(|r| format!("op {} (blob {} at {})", r.op_id, r.blob.short(), r.path))
            .collect();
        return Err(anyhow!(
            "Refusing to undo a `heddle redact apply`: the inverse removes the redaction record \
             so subsequent materializes restore the original bytes, which would re-expose \
             previously-hidden content. Pass `--allow-redact-undo` to confirm. Affected op(s): {}.",
            shorts.join(", "),
        ));
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
            match &entry.operation {
                OpRecord::Redact { .. } => blocking.push((entry.id, "Redact")),
                OpRecord::Purge { .. } => blocking.push((entry.id, "Purge")),
                _ => {}
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
    Err(anyhow!(
        "Refusing to redo: `Redact` and `Purge` ops do not have a re-apply path — the oplog \
         entry doesn't preserve the full Redaction record (reason, redactor, signature) needed \
         to recreate it, and Purge is irreversible by design. Re-run `heddle redact apply` \
         (or `heddle purge apply`) to re-establish the operation. Affected op(s): {}.",
        shorts.join(", "),
    ))
}

/// heddle#198 — Rebase-undo safety against a purge that crossed the
/// rebase batch. The rebase inverse rewinds the attached thread to
/// the earliest recorded `pre_target_id`. Materializing that tip
/// requires every blob in its tree; if `Redact apply` + `Purge` have
/// run on any of those blobs since the rebase, the bytes are gone
/// and the rewind would surface a raw `missing blob` from inside
/// `goto`. Refuse pre-mutation with a single message naming the
/// blob and the rebase batch — mirrors the "Refused regardless of
/// the flag when the underlying bytes have since been purged" rule
/// the Redact inverse already enforces (docs/undo.md "Safety
/// contracts").
///
/// Identification: a rebase batch is the one written by
/// `rebase_ops::flush_rebase_batch` — its `TransactionCommit`
/// envelope marker carries an id with prefix `"rebase-"`. Both the
/// replay path and the FF arms (is_ancestor / empty-commits) flush
/// through the same helper so the marker is uniform.
fn ensure_rebase_undo_safe(repo: &Repository, batches: &[OpBatch]) -> Result<()> {
    let mut blocking: Vec<(u64, ChangeId, ContentHash)> = Vec::new();

    for batch in batches {
        if !is_rebase_batch(batch) {
            continue;
        }
        // The earliest FF in the batch carries the pre-rebase tip
        // (batch_index is monotonic; the replay loop appends in
        // order). Materializing that tip needs every blob in its
        // tree — that's the surface a purge can invalidate.
        let Some(pre_target) = earliest_rebase_pre_target_id(batch) else {
            continue;
        };
        let Some(tree) = load_tree_for_state(repo, &pre_target)? else {
            // Pre-tip state already missing — `ensure_undo_states_reachable`
            // surfaces that with the destructive-boundary message;
            // don't double-report here.
            continue;
        };
        let mut blobs: HashSet<ContentHash> = HashSet::new();
        collect_tree_blobs(repo, &tree, &mut blobs)?;
        for blob in &blobs {
            let redactions = repo.get_redactions_for_blob(blob)?;
            if redactions.redactions.iter().any(|r| r.is_purged()) {
                blocking.push((batch.id, pre_target, *blob));
                break; // one purged blob per batch is sufficient to refuse
            }
        }
    }

    if blocking.is_empty() {
        return Ok(());
    }

    let shorts: Vec<String> = blocking
        .iter()
        .map(|(batch_id, pre_target, blob)| {
            format!(
                "rebase batch {} (pre-rebase tip {}, purged blob {})",
                batch_id,
                pre_target.short(),
                blob.short()
            )
        })
        .collect();
    Err(anyhow!(
        "Refusing to undo: at least one rebase in this chain rewinds to a tip whose tree \
         references blobs that have since been purged ({}). The rewind would land the worktree \
         on a state whose next `materialize` would fail with a missing-blob error rather than \
         restore content. Purge is irreversible — restore the bytes from a backup or run \
         `heddle undo --list` and target an op past the rebase.",
        shorts.join(", "),
    ))
}

/// A rebase batch is identified by its `TransactionCommit` marker
/// (id prefix `"rebase-"`) — see `rebase_ops::flush_rebase_batch`.
/// Pre-198 oplog entries did not flush a marker; they age out as the
/// undo window slides forward. The check tolerates batches without
/// the marker (treats them as non-rebase, no rebase-specific
/// refusal applies).
fn is_rebase_batch(batch: &OpBatch) -> bool {
    batch.entries.iter().any(|entry| {
        matches!(
            &entry.operation,
            OpRecord::TransactionCommit { transaction_id, .. }
                if transaction_id.starts_with("rebase-")
        )
    })
}

fn earliest_rebase_pre_target_id(batch: &OpBatch) -> Option<ChangeId> {
    // Entries are sorted by `batch_index` (see
    // `PackedOpLog::collect_batches_scoped`), so the first FF arm
    // we encounter carries the pre-rebase tip.
    batch.entries.iter().find_map(|entry| match &entry.operation {
        OpRecord::FastForwardV2 { pre_target_id, .. } => Some(*pre_target_id),
        OpRecord::FastForward { pre_target_id, .. } => Some(*pre_target_id),
        _ => None,
    })
}

fn load_tree_for_state(repo: &Repository, state: &ChangeId) -> Result<Option<Tree>> {
    let Some(state_obj) = repo.store().get_state(state)? else {
        return Ok(None);
    };
    Ok(repo.store().get_tree(&state_obj.tree)?)
}

fn collect_tree_blobs(
    repo: &Repository,
    tree: &Tree,
    out: &mut HashSet<ContentHash>,
) -> Result<()> {
    for entry in tree.iter() {
        if entry.is_blob() {
            out.insert(entry.hash);
            continue;
        }
        if entry.is_tree()
            && let Some(subtree) = repo.store().get_tree(&entry.hash)?
        {
            collect_tree_blobs(repo, &subtree, out)?;
        }
    }
    Ok(())
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
            // Match V1 and V2 — the rename batch's new-name arm and all
            // post-heddle#23-r2 creates record as V2. Both have the
            // same worktree-orphan hazard on undo.
            let name = match &entry.operation {
                OpRecord::ThreadCreate { name, .. }
                | OpRecord::ThreadCreateV2 { name, .. } => name,
                _ => continue,
            };
            let Some(record) = manager.find_by_thread(name)? else {
                continue;
            };
            let Some(path) = record.materialized_path.as_ref() else {
                continue;
            };
            if path.exists() {
                blocking.push((entry.id, name.clone(), path.clone()));
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
    Err(anyhow!(
        "Refusing to undo: at least one `thread create` in this chain has an attached \
         materialized worktree that would be orphaned by the inverse ({}). The undo only \
         removes the thread ref; the worktree directory and its `.heddle/HEAD` would be left \
         pointing at a thread that no longer exists. Tear the worktree down first with \
         `heddle thread drop <name> --delete-thread`, then re-run `heddle undo`.",
        shorts.join(", "),
    ))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use objects::object::Principal;
    use oplog::OpEntry;

    use super::*;

    fn op_entry(id: u64, operation: OpRecord, batch_index: u32) -> OpEntry {
        OpEntry {
            id,
            timestamp: Utc::now(),
            operation,
            undone: false,
            batch_id: 1,
            batch_index,
            scope: None,
            actor: Principal::new("Test", "test@example.com"),
            operation_id: None,
        }
    }

    fn rebase_txn_commit_entry(index: u32) -> OpEntry {
        op_entry(
            100 + u64::from(index),
            OpRecord::TransactionCommit {
                transaction_id: "rebase-1234567890".to_string(),
                op_count: 1,
            },
            index,
        )
    }

    /// heddle#198 r2 (Codex PR #218 P2): detached-mode rebase batches
    /// carry `OpRecord::Goto` (no thread ref to FF), and the purge
    /// safety scan must lift the pre-rebase tip from `prev_head` for
    /// these entries. Pre-fix, the helper matched only `FastForward*`
    /// arms and returned `None` for detached batches; the gate then
    /// silently skipped them.
    #[test]
    fn earliest_rebase_pre_target_id_extracts_prev_head_from_goto() {
        let pre = ChangeId::generate();
        let post = ChangeId::generate();
        let batch = OpBatch {
            id: 1,
            entries: vec![
                op_entry(
                    1,
                    OpRecord::Goto {
                        target: post,
                        prev_head: Some(pre),
                    },
                    0,
                ),
                rebase_txn_commit_entry(1),
            ],
        };
        assert_eq!(earliest_rebase_pre_target_id(&batch), Some(pre));
    }

    /// The same helper must keep the existing `FastForwardV2` arm — a
    /// regression here would silently break the attached-mode purge
    /// scan that's already covered by the integration tests.
    #[test]
    fn earliest_rebase_pre_target_id_extracts_pre_target_from_fast_forward_v2() {
        let pre = ChangeId::generate();
        let post = ChangeId::generate();
        let batch = OpBatch {
            id: 2,
            entries: vec![
                op_entry(
                    1,
                    OpRecord::FastForwardV2 {
                        source_thread: "<rebase>".to_string(),
                        target_thread: "main".to_string(),
                        pre_target_id: pre,
                        post_target_id: post,
                    },
                    0,
                ),
                rebase_txn_commit_entry(1),
            ],
        };
        assert_eq!(earliest_rebase_pre_target_id(&batch), Some(pre));
    }

    /// When the first replayed-commit advance is a Goto and the second
    /// is an FF, the earliest pre-target must come from the Goto. The
    /// `batch_index` ordering (set by `PackedOpLog::collect_batches_scoped`)
    /// is preserved by `entries`, so `find_map` walks oldest-first.
    #[test]
    fn earliest_rebase_pre_target_id_prefers_first_entry_when_mixed() {
        let pre_goto = ChangeId::generate();
        let mid = ChangeId::generate();
        let post = ChangeId::generate();
        let batch = OpBatch {
            id: 3,
            entries: vec![
                op_entry(
                    1,
                    OpRecord::Goto {
                        target: mid,
                        prev_head: Some(pre_goto),
                    },
                    0,
                ),
                op_entry(
                    2,
                    OpRecord::FastForwardV2 {
                        source_thread: "<rebase>".to_string(),
                        target_thread: "main".to_string(),
                        pre_target_id: mid,
                        post_target_id: post,
                    },
                    1,
                ),
                rebase_txn_commit_entry(2),
            ],
        };
        assert_eq!(earliest_rebase_pre_target_id(&batch), Some(pre_goto));
    }

    /// A `Goto` without `prev_head` (legacy or operator-issued goto)
    /// has nothing to rewind to and must skip past it rather than
    /// surface a placeholder. The helper continues scanning for a
    /// subsequent FF/Goto that does carry a pre-target.
    #[test]
    fn earliest_rebase_pre_target_id_skips_goto_without_prev_head() {
        let pre = ChangeId::generate();
        let post = ChangeId::generate();
        let batch = OpBatch {
            id: 4,
            entries: vec![
                op_entry(
                    1,
                    OpRecord::Goto {
                        target: post,
                        prev_head: None,
                    },
                    0,
                ),
                op_entry(
                    2,
                    OpRecord::FastForwardV2 {
                        source_thread: "<rebase>".to_string(),
                        target_thread: "main".to_string(),
                        pre_target_id: pre,
                        post_target_id: post,
                    },
                    1,
                ),
                rebase_txn_commit_entry(2),
            ],
        };
        assert_eq!(earliest_rebase_pre_target_id(&batch), Some(pre));
    }
}
