// SPDX-License-Identifier: Apache-2.0
//! Apply undo/redo operations to the repository.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use gix::{
    ObjectId,
    refs::{
        Target,
        transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog},
    },
};
use objects::error::{HeddleError, Result as HeddleResult};
use objects::object::{ChangeId, MarkerName, ThreadName};
use oplog::{OpBatch, OpEntry, OpRecord};
use refs::Head;
use repo::{
    CommitGraphIndex, Repository, ThreadFreshness, ThreadIntegrationPolicy, ThreadManager,
    ThreadState, refresh_thread_freshness,
    atomic::{AtomicMutation, SavepointMutation, StagedCommit, Tx},
};

use super::{advice::RecoveryAdvice, thread_cmd::thread_not_found_advice};
use crate::bridge::git_core::{open_repo as open_git_repo, set_reference};

pub(super) fn preflight_undo_batches(repo: &Repository, batches: &[OpBatch]) -> Result<()> {
    if !batches_have_git_checkpoint(batches) {
        return Ok(());
    }
    let mut simulated_git_head = current_git_head(repo)?;
    for batch in batches {
        for entry in batch.entries.iter().rev() {
            if let OpRecord::GitCheckpoint {
                new_git_oid,
                previous_git_oid,
                ..
            } = &entry.operation
            {
                ensure_simulated_git_head_is(
                    repo,
                    &simulated_git_head,
                    new_git_oid,
                    "undo git checkpoint",
                )?;
                if let Some(previous) = previous_git_oid {
                    simulated_git_head = previous.clone();
                }
            }
        }
    }
    ensure_git_worktree_clean(repo, "undo git checkpoint")?;
    Ok(())
}

pub(super) fn preflight_redo_batches(repo: &Repository, batches: &[OpBatch]) -> Result<()> {
    if !batches_have_git_checkpoint(batches) {
        return Ok(());
    }
    let mut simulated_git_head = current_git_head(repo)?;
    for batch in batches {
        for entry in &batch.entries {
            if let OpRecord::GitCheckpoint {
                previous_git_oid,
                new_git_oid,
                ..
            } = &entry.operation
            {
                if let Some(previous) = previous_git_oid {
                    ensure_simulated_git_head_is(
                        repo,
                        &simulated_git_head,
                        previous,
                        "redo git checkpoint",
                    )?;
                }
                simulated_git_head = new_git_oid.clone();
            }
        }
    }
    ensure_git_worktree_clean(repo, "redo git checkpoint")?;
    Ok(())
}

fn batches_have_git_checkpoint(batches: &[OpBatch]) -> bool {
    batches.iter().any(|batch| {
        batch
            .entries
            .iter()
            .any(|entry| matches!(&entry.operation, OpRecord::GitCheckpoint { .. }))
    })
}

fn current_git_head(repo: &Repository) -> Result<String> {
    let git = git_checkout_repo(repo)?;
    git.head_id()
        .map(|id| id.detach().to_string())
        .map_err(|error| anyhow!("failed to inspect Git HEAD: {error}"))
}

fn ensure_simulated_git_head_is(
    repo: &Repository,
    actual: &str,
    expected: &str,
    action: &str,
) -> Result<()> {
    if actual == expected {
        return Ok(());
    }
    Err(anyhow!(RecoveryAdvice::git_head_mismatch(
        action,
        actual,
        expected,
        repo.git_overlay_current_branch()?
            .unwrap_or_else(|| "HEAD".to_string()),
        git_dirty_paths(repo),
    )))
}

fn apply_undo_entry(repo: &Repository, entry: &OpEntry) -> Result<()> {
    match &entry.operation {
        OpRecord::Snapshot {
            prev_head: Some(prev),
            thread,
            new_state,
            ..
        } => {
            repo.goto_without_record(prev)?;
            if let Some(thread) = thread {
                repo.refs().set_thread(&ThreadName::new(thread.as_str()), prev)?;
                repo.refs().write_head(&Head::Attached {
                    thread: ThreadName::new(thread.as_str()),
                })?;
                sync_thread_record_state(repo, thread, *prev)?;
                mark_merged_threads_unintegrated_for_target(repo, thread, new_state, prev)?;
            }
        }
        OpRecord::Goto {
            prev_head: Some(prev),
            ..
        } => {
            repo.goto_without_record(prev)?;
        }
        OpRecord::Snapshot {
            prev_head: None, ..
        }
        | OpRecord::Goto {
            prev_head: None, ..
        } => {}
        OpRecord::ThreadCreate { name, .. } | OpRecord::ThreadCreateV2 { name, .. } => {
            delete_thread_safely(repo, &ThreadName::new(name.as_str()))?;
            // Cross-thread contract rule 4 (docs/design/cross-thread-undo.md):
            // the inverse of `ThreadCreate` must also remove the matching
            // ThreadManager record so `heddle thread show` and the record-
            // store readers don't surface a phantom entry for a thread
            // whose ref no longer exists. The worktree-attached refusal in
            // `ensure_thread_worktree_undo_safe` already gated us, so any
            // record we hit here has `materialized_path = None` or a path
            // that no longer exists — either way, dropping the record is
            // safe. Missing record is fine: not every `ThreadCreate` path
            // writes one (legacy oplog entries may predate the record
            // store).
            //
            // Both V1 and V2 use the same undo: V2's `manager_snapshot`
            // is recorded for redo, so undo can still destroy the live
            // record without losing the data needed to put it back.
            // Matches the FastForward/V2 shared-undo shape.
            remove_thread_manager_record(repo, name)?;
        }
        OpRecord::ThreadDelete { name, state } => {
            repo.refs().set_thread(&ThreadName::new(name.as_str()), state)?;
        }
        OpRecord::ThreadUpdate {
            name, old_state, ..
        } => {
            repo.refs().set_thread(&ThreadName::new(name.as_str()), old_state)?;
        }
        OpRecord::MarkerCreate { name, .. } => {
            repo.refs().delete_marker(&MarkerName::new(name.as_str()))?;
        }
        OpRecord::MarkerDelete { name, state } => {
            repo.refs().create_marker(&MarkerName::new(name.as_str()), state)?;
        }
        // Redaction inverse: drop the specific redaction record so
        // subsequent materialize calls restore the original blob
        // bytes. The opt-in flag + purged-bytes check are enforced in
        // `cmd_undo::ensure_redaction_undo_safe` before this point;
        // `remove_redaction` re-checks `purged_at` defensively so a
        // future caller that bypasses the CLI gate can't lose the
        // audit trail of destroyed bytes.
        //
        // Pass the oplog-recorded `redaction_id` through so a
        // refinement pass (multiple records sharing the same
        // `(blob, state, path)` with different reasons or signatures)
        // undoes the exact record this op references rather than the
        // first match in sidecar order. `remove_redaction` falls
        // back to `(state, path)` only for the purge-id-shift case
        // and refuses in that branch.
        OpRecord::Redact {
            redaction_id,
            blob,
            state,
            path,
        } => {
            repo.remove_redaction(blob, state, path, redaction_id)?;
        }
        // Fast-forward merge inverse: restore both HEAD and the target
        // thread ref to the pre-FF tip. The source thread never moved
        // during the FF, so it's untouched. Closes heddle#99 r1 — the
        // bug where recording an FF as `OpRecord::Goto` left the target
        // thread ref stranded at the FF target after undo.
        //
        // V1 and V2 share the same undo: both carry `pre_target_id`.
        OpRecord::FastForward {
            source_thread,
            target_thread,
            pre_target_id,
            ..
        }
        | OpRecord::FastForwardV2 {
            source_thread,
            target_thread,
            pre_target_id,
            ..
        } => {
            apply_ff_undo(repo, source_thread, target_thread, pre_target_id)?;
        }
        OpRecord::GitCheckpoint {
            branch,
            previous_git_oid,
            new_git_oid,
            ..
        } => {
            apply_git_checkpoint_undo(repo, branch, previous_git_oid.as_deref(), new_git_oid)?;
        }
        // No undo inverse: these records don't move a ref the undo chain
        // restores, or their reversal is irreversible / handled outside the
        // oplog replay. Enumerated explicitly (no wildcard) so a new
        // `OpRecord` variant is a COMPILE error here until its undo behavior
        // is decided (heddle#354 r9):
        //   - Fork / Collapse: structural ops; HEAD/thread restoration is
        //     driven by the surrounding records in the same batch.
        //   - Checkpoint: addressable save, goto-reachable; nothing to invert.
        //   - TransactionAbort / TransactionCommit / ConflictResolved: forensic
        //     / audit records, no ref to restore.
        //   - EphemeralThreadCollapse: TTL retirement of a thread pointer; the
        //     states stay addressable and the pointer is not resurrected here.
        //   - Purge: irreversible by design (bytes physically removed) — the
        //     undo preflight (`ensure_redaction_undo_safe`) refuses earlier.
        //   - RemoteThreadUpdate / RemoteThreadDelete / UndoRecoveryUpdate:
        //     reconcile-class bookkeeping refs, outside the user undo chain.
        OpRecord::Fork { .. }
        | OpRecord::Collapse { .. }
        | OpRecord::Checkpoint { .. }
        | OpRecord::TransactionAbort { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::EphemeralThreadCollapse { .. }
        | OpRecord::Purge { .. }
        | OpRecord::RemoteThreadUpdate { .. }
        | OpRecord::RemoteThreadDelete { .. }
        | OpRecord::UndoRecoveryUpdate { .. } => {}
    }

    Ok(())
}

fn apply_ff_undo(
    repo: &Repository,
    source_thread: &str,
    target_thread: &str,
    pre_target_id: &ChangeId,
) -> Result<()> {
    repo.goto_without_record(pre_target_id)?;
    repo.refs().set_thread(&ThreadName::new(target_thread), pre_target_id)?;
    repo.refs().write_head(&Head::Attached {
        thread: ThreadName::new(target_thread),
    })?;
    sync_thread_record_state(repo, target_thread, *pre_target_id)?;
    mark_source_thread_unintegrated(repo, source_thread, pre_target_id)
}

fn apply_redo_entry(repo: &Repository, entry: &OpEntry) -> Result<()> {
    match &entry.operation {
        OpRecord::Snapshot {
            new_state,
            prev_head,
            thread,
        } => {
            repo.goto_without_record(new_state)?;
            if let Some(thread) = thread {
                repo.refs().set_thread(&ThreadName::new(thread.as_str()), new_state)?;
                repo.refs().write_head(&Head::Attached {
                    thread: ThreadName::new(thread.as_str()),
                })?;
                sync_thread_record_state(repo, thread, *new_state)?;
                mark_ready_threads_integrated_for_target(repo, thread, new_state, prev_head)?;
            }
        }
        OpRecord::Goto { target, .. } => {
            repo.goto_without_record(target)?;
        }
        // V1 ThreadCreate redo: ref-only, with a stderr note that the
        // ThreadManager record is not being restored. V1 records were
        // written by code pre-heddle#23 r2 that didn't carry a record
        // snapshot in the OpRecord; redo can't reconstruct the body
        // (mode, base_state, base_root, …) from `(name, state)` alone.
        // Record-backed commands (`thread cd`, delegate, integration
        // policy) will silently degrade on this thread — pointed out
        // on stderr so the operator can rerun `heddle thread start
        // <name>` if they want the record back. V1 ages out as the
        // live oplog window slides forward.
        OpRecord::ThreadCreate { name, state } => {
            repo.refs().set_thread(&ThreadName::new(name.as_str()), state)?;
            eprintln!(
                "warning: redo of legacy V1 `ThreadCreate` for '{}' restores the ref only — \
                 the matching ThreadManager record body was not snapshotted by this oplog entry. \
                 Run `heddle thread start {}` to re-establish the record if record-backed \
                 commands (`thread cd`, delegate, integration policy) misbehave.",
                name, name
            );
        }
        // V2 ThreadCreate redo (heddle#23 r2): restore both the thread
        // ref and the ThreadManager record body from the snapshot
        // captured at recording time. Mirrors the FastForwardV2 redo
        // pattern (record what redo needs).
        //
        // `manager_snapshot = None` means the forward path didn't have
        // a record to snapshot (cmd_start before materialization, the
        // rename batch's new-name arm, ingest, harness/agent stubs).
        // Restore the ref only in that case — no record to put back.
        OpRecord::ThreadCreateV2 {
            name,
            state,
            manager_snapshot,
        } => {
            repo.refs().set_thread(&ThreadName::new(name.as_str()), state)?;
            if let Some(bytes) = manager_snapshot {
                let manager = ThreadManager::new(repo.heddle_dir());
                match manager.restore_thread_record_from_snapshot(bytes) {
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!(
                            "warning: redo of `ThreadCreate` for '{}' restored the ref but \
                             failed to decode the ThreadManager record snapshot ({}). \
                             Record-backed commands (`thread cd`, delegate) may degrade \
                             on this thread — run `heddle thread start {}` to recreate \
                             the record.",
                            name, e, name
                        );
                    }
                }
            }
        }
        OpRecord::ThreadDelete { name, .. } => {
            delete_thread_safely(repo, &ThreadName::new(name.as_str()))?;
        }
        OpRecord::ThreadUpdate {
            name, new_state, ..
        } => {
            repo.refs().set_thread(&ThreadName::new(name.as_str()), new_state)?;
        }
        OpRecord::MarkerCreate { name, state } => {
            repo.refs().create_marker(&MarkerName::new(name.as_str()), state)?;
        }
        OpRecord::MarkerDelete { name, .. } => {
            repo.refs().delete_marker(&MarkerName::new(name.as_str()))?;
        }
        // FF merge redo (V2): replay the *recorded* FF target. We do
        // not re-read `source_thread` — the recorded `post_target_id`
        // is the exact state the target advanced to at the original
        // FF, so redo is deterministic regardless of what the source
        // thread did between undo and redo (advanced, was deleted,
        // etc.). Closes heddle#99 r2 — Codex's non-determinism finding
        // on the r1 implementation.
        OpRecord::FastForwardV2 {
            source_thread,
            target_thread,
            post_target_id,
            ..
        } => {
            apply_ff_redo(repo, source_thread, target_thread, post_target_id)?;
        }
        OpRecord::GitCheckpoint {
            branch,
            previous_git_oid,
            new_git_oid,
            ..
        } => {
            apply_git_checkpoint_redo(repo, branch, previous_git_oid.as_deref(), new_git_oid)?;
        }
        // FF merge redo (V1, legacy): the r1 implementation didn't
        // record `post_target_id`, so we have to re-resolve
        // `source_thread → tip`. This is the non-deterministic redo
        // Codex flagged; V1 records can only be written by the r1
        // implementation and age out as the undo window slides
        // forward. New ops are recorded as `FastForwardV2` so this
        // path stops accumulating.
        OpRecord::FastForward {
            source_thread,
            target_thread,
            ..
        } => {
            let source_tip = repo.refs().get_thread(&ThreadName::new(source_thread.as_str()))?.ok_or_else(|| {
                anyhow!(
                    "cannot redo fast-forward: source thread '{}' no longer exists \
                     (legacy V1 oplog record; re-run the merge or `heddle gc oplog` to prune)",
                    source_thread
                )
            })?;
            apply_ff_redo(repo, source_thread, target_thread, &source_tip)?;
        }
        // No redo replay: these records don't re-advance a ref redo touches, or
        // they are refused upstream. Enumerated explicitly (no wildcard) so a
        // new `OpRecord` variant is a COMPILE error here until its redo
        // behavior is decided (heddle#354 r9):
        //   - Fork / Collapse: structural ops; redo is driven by the
        //     surrounding records in the same batch.
        //   - Checkpoint: addressable save, goto-reachable; nothing to replay.
        //   - Redact: redo is refused upstream by `ensure_redaction_redo_supported`
        //     (the OpRecord doesn't carry the full Redaction needed to recreate
        //     it); reaching here is a no-op.
        //   - Purge: irreversible by design; also refused upstream.
        //   - TransactionAbort / TransactionCommit / ConflictResolved /
        //     EphemeralThreadCollapse: forensic / TTL records, no ref to replay.
        //   - RemoteThreadUpdate / RemoteThreadDelete / UndoRecoveryUpdate:
        //     reconcile-class bookkeeping refs, outside the user redo chain.
        OpRecord::Fork { .. }
        | OpRecord::Collapse { .. }
        | OpRecord::Checkpoint { .. }
        | OpRecord::TransactionAbort { .. }
        | OpRecord::TransactionCommit { .. }
        | OpRecord::ConflictResolved { .. }
        | OpRecord::EphemeralThreadCollapse { .. }
        | OpRecord::Redact { .. }
        | OpRecord::Purge { .. }
        | OpRecord::RemoteThreadUpdate { .. }
        | OpRecord::RemoteThreadDelete { .. }
        | OpRecord::UndoRecoveryUpdate { .. } => {}
    }

    Ok(())
}

fn apply_ff_redo(
    repo: &Repository,
    source_thread: &str,
    target_thread: &str,
    post_target_id: &ChangeId,
) -> Result<()> {
    repo.goto_without_record(post_target_id)?;
    repo.refs().set_thread(&ThreadName::new(target_thread), post_target_id)?;
    repo.refs().write_head(&Head::Attached {
        thread: ThreadName::new(target_thread),
    })?;
    sync_thread_record_state(repo, target_thread, *post_target_id)?;
    mark_source_thread_integrated(repo, source_thread, post_target_id)
}

fn apply_git_checkpoint_undo(
    repo: &Repository,
    branch: &str,
    previous_git_oid: Option<&str>,
    new_git_oid: &str,
) -> Result<()> {
    ensure_git_head_is(repo, new_git_oid, "undo git checkpoint")?;
    ensure_git_worktree_clean(repo, "undo git checkpoint")?;
    let git = git_checkout_repo(repo)?;
    let new_oid = parse_git_oid(new_git_oid)?;
    match previous_git_oid {
        Some(previous) => {
            let previous_oid = parse_git_oid(previous)?;
            if branch != "HEAD" {
                let ref_name = format!("refs/heads/{branch}");
                if ref_target_oid(&git, &ref_name)? != Some(previous_oid) {
                    attach_git_head_to_branch(&git, branch)?;
                    set_attached_git_head(
                        &git,
                        branch,
                        previous_oid,
                        new_oid,
                        "heddle: undo git checkpoint",
                    )?;
                }
                attach_git_head_to_branch(&git, branch)?;
            }
            reset_git_index_to_commit(&git, previous_oid)?;
            update_mirror_branch_ref(repo, branch, Some(previous), Some(new_git_oid))?;
        }
        None => {
            if branch != "HEAD" {
                delete_reference_matching(&git, &format!("refs/heads/{branch}"), Some(new_oid))?;
            }
            update_mirror_branch_ref(repo, branch, None, Some(new_git_oid))?;
        }
    }
    Ok(())
}

fn apply_git_checkpoint_redo(
    repo: &Repository,
    branch: &str,
    previous_git_oid: Option<&str>,
    new_git_oid: &str,
) -> Result<()> {
    if let Some(previous) = previous_git_oid {
        ensure_git_head_is(repo, previous, "redo git checkpoint")?;
    }
    let git = git_checkout_repo(repo)?;
    let new_oid = parse_git_oid(new_git_oid)?;
    if branch != "HEAD" {
        match previous_git_oid {
            Some(previous) => {
                attach_git_head_to_branch(&git, branch)?;
                set_attached_git_head(
                    &git,
                    branch,
                    new_oid,
                    parse_git_oid(previous)?,
                    "heddle: redo git checkpoint",
                )?;
            }
            None => {
                set_reference(
                    &git,
                    &format!("refs/heads/{branch}"),
                    new_oid,
                    PreviousValue::Any,
                    "heddle: redo git checkpoint",
                )?;
                attach_git_head_to_branch(&git, branch)?;
            }
        }
    }
    reset_git_index_to_commit(&git, new_oid)?;
    update_mirror_branch_ref(repo, branch, Some(new_git_oid), previous_git_oid)?;
    Ok(())
}

fn update_mirror_branch_ref(
    repo: &Repository,
    branch: &str,
    target_oid: Option<&str>,
    expected_old_oid: Option<&str>,
) -> Result<()> {
    if branch == "HEAD" {
        return Ok(());
    }
    let mirror = repo.heddle_dir().join("git");
    if !mirror.exists() {
        return Ok(());
    }
    let git = open_git_repo(&mirror)?;
    let ref_name = format!("refs/heads/{branch}");
    if let Some(target) = target_oid
        && ref_target_oid(&git, &ref_name)? == Some(parse_git_oid(target)?)
    {
        return Ok(());
    }
    match (target_oid, expected_old_oid) {
        (Some(target), Some(expected)) => set_reference(
            &git,
            &ref_name,
            parse_git_oid(target)?,
            PreviousValue::MustExistAndMatch(Target::Object(parse_git_oid(expected)?)),
            "heddle: update mirror checkpoint ref",
        )
        .map_err(|error| anyhow!(error)),
        (Some(target), None) => set_reference(
            &git,
            &ref_name,
            parse_git_oid(target)?,
            PreviousValue::Any,
            "heddle: update mirror checkpoint ref",
        )
        .map_err(|error| anyhow!(error)),
        (None, Some(expected)) => {
            delete_reference_matching(&git, &ref_name, Some(parse_git_oid(expected)?))
        }
        (None, None) => delete_reference_matching(&git, &ref_name, None),
    }
}

fn ensure_git_head_is(repo: &Repository, expected: &str, action: &str) -> Result<()> {
    let actual = current_git_head(repo)?;
    if actual == expected {
        return Ok(());
    }
    Err(anyhow!(RecoveryAdvice::git_head_mismatch(
        action,
        &actual,
        expected,
        repo.git_overlay_current_branch()?
            .unwrap_or_else(|| "HEAD".to_string()),
        git_dirty_paths(repo),
    )))
}

fn ensure_git_worktree_clean(repo: &Repository, action: &str) -> Result<()> {
    let Some(status) = repo.git_overlay_worktree_status()? else {
        return Ok(());
    };
    if status.is_clean() {
        return Ok(());
    }
    Err(anyhow!(RecoveryAdvice::dirty_worktree(
        action,
        git_status_paths(&status),
        "the Heddle undo batch has not been applied",
    )))
}

fn git_dirty_paths(repo: &Repository) -> Vec<String> {
    repo.git_overlay_worktree_status()
        .ok()
        .flatten()
        .map(|status| git_status_paths(&status))
        .unwrap_or_default()
}

fn git_status_paths(status: &objects::worktree::WorktreeStatus) -> Vec<String> {
    let mut paths = Vec::new();
    paths.extend(format_status_paths("modified", &status.modified));
    paths.extend(format_status_paths("added", &status.added));
    paths.extend(format_status_paths("deleted", &status.deleted));
    paths
}

fn format_status_paths(kind: &str, paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| format!("{kind}: {}", path.display()))
        .collect()
}

fn git_checkout_repo(repo: &Repository) -> Result<gix::Repository> {
    open_git_repo(repo.root()).map_err(|error| anyhow!(error))
}

fn parse_git_oid(oid: &str) -> Result<ObjectId> {
    oid.parse::<ObjectId>()
        .map_err(|error| anyhow!("invalid Git object id '{oid}': {error}"))
}

fn ref_target_oid(repo: &gix::Repository, name: &str) -> Result<Option<ObjectId>> {
    let Some(mut reference) = repo
        .try_find_reference(name)
        .map_err(|error| anyhow!("failed to inspect Git reference '{name}': {error}"))?
    else {
        return Ok(None);
    };
    reference
        .peel_to_id()
        .map(|id| Some(id.detach()))
        .map_err(|error| anyhow!("failed to resolve Git reference '{name}': {error}"))
}

fn attach_git_head_to_branch(repo: &gix::Repository, branch: &str) -> Result<()> {
    if branch == "HEAD" {
        return Ok(());
    }
    let head_path = repo.git_dir().join("HEAD");
    fs::write(&head_path, format!("ref: refs/heads/{branch}\n"))
        .map_err(|error| anyhow!("failed to attach Git HEAD to branch '{branch}': {error}"))?;
    fsync_file_and_parent(&head_path)?;
    Ok(())
}

fn set_attached_git_head(
    repo: &gix::Repository,
    branch: &str,
    target: ObjectId,
    expected: ObjectId,
    log_message: &str,
) -> Result<()> {
    let signature = git_signature();
    let mut time_buf = gix::date::parse::TimeBuf::default();
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: log_message.into(),
            },
            expected: PreviousValue::MustExistAndMatch(Target::Object(expected)),
            new: Target::Object(target),
        },
        name: "HEAD"
            .try_into()
            .map_err(|error| anyhow!("invalid Git HEAD ref: {error}"))?,
        deref: true,
    };
    repo.edit_references_as([edit], Some(signature.to_ref(&mut time_buf)))
        .map_err(|error| anyhow!("failed to update Git HEAD for branch '{branch}': {error}"))?;
    Ok(())
}

fn reset_git_index_to_commit(repo: &gix::Repository, oid: ObjectId) -> Result<()> {
    let commit = repo
        .find_commit(oid)
        .map_err(|error| anyhow!("failed to inspect Git commit {oid}: {error}"))?;
    let tree_id = commit
        .tree_id()
        .map_err(|error| anyhow!("failed to inspect Git commit tree {oid}: {error}"))?;
    let mut index = repo
        .index_from_tree(tree_id.as_ref())
        .map_err(|error| anyhow!("failed to build Git index for commit {oid}: {error}"))?;
    index
        .write(gix_index::write::Options::default())
        .map_err(|error| anyhow!("failed to write Git index for commit {oid}: {error}"))?;
    Ok(())
}

fn delete_reference_matching(
    repo: &gix::Repository,
    name: &str,
    expected: Option<ObjectId>,
) -> Result<()> {
    let signature = git_signature();
    let mut time_buf = gix::date::parse::TimeBuf::default();
    let expected = expected.map_or(PreviousValue::MustExist, |oid| {
        PreviousValue::MustExistAndMatch(Target::Object(oid))
    });
    let edit = RefEdit {
        change: Change::Delete {
            log: RefLog::AndReference,
            expected,
        },
        name: name
            .try_into()
            .map_err(|error| anyhow!("invalid Git reference '{name}': {error}"))?,
        deref: false,
    };
    repo.edit_references_as([edit], Some(signature.to_ref(&mut time_buf)))
        .map_err(|error| anyhow!("failed to delete Git reference '{name}': {error}"))?;
    Ok(())
}

fn git_signature() -> gix::actor::Signature {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    gix::actor::Signature {
        name: "Heddle".into(),
        email: "heddle@local".into(),
        time: gix::date::Time { seconds, offset: 0 },
    }
}

fn fsync_file_and_parent(path: &Path) -> Result<()> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| anyhow!("failed to sync '{}': {error}", path.display()))?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|error| anyhow!("failed to sync '{}': {error}", parent.display()))?;
    }
    Ok(())
}

fn delete_thread_safely(repo: &Repository, name: &ThreadName) -> Result<()> {
    if let Head::Attached { thread } = repo.head_ref()?
        && thread == *name
    {
        let state = repo
            .refs()
            .get_thread(name)?
            .ok_or_else(|| anyhow!(thread_not_found_advice(name.as_str(), "delete thread")))?;
        repo.refs().write_head(&Head::Detached { state })?;
    }

    repo.refs().delete_thread(name)?;
    Ok(())
}

fn sync_thread_record_state(
    repo: &Repository,
    thread_name: &str,
    state: objects::object::ChangeId,
) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    if let Some(mut thread) = manager.find_by_thread(thread_name)? {
        thread.current_state = Some(state.short());
        thread.updated_at = chrono::Utc::now();
        manager.save(&thread)?;
    }
    Ok(())
}

fn mark_source_thread_unintegrated(
    repo: &Repository,
    source_thread: &str,
    target_after_undo: &ChangeId,
) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    let Some(mut thread) = manager.find_by_thread(source_thread)? else {
        return Ok(());
    };
    let source_tip = repo.refs().get_thread(&ThreadName::new(source_thread))?;
    let still_integrated = source_tip
        .as_ref()
        .is_some_and(|source_tip| change_contains(repo, source_tip, target_after_undo));
    if still_integrated {
        return Ok(());
    }

    if matches!(thread.state, ThreadState::Merged) {
        thread.state = ThreadState::Ready;
    }
    if let Some(source_tip) = source_tip {
        thread.current_state = Some(source_tip.short());
    }
    thread.merged_state = None;
    if matches!(
        thread.integration_policy_result.status.as_deref(),
        Some("auto_integrated")
    ) {
        thread.integration_policy_result = ThreadIntegrationPolicy::default();
    }
    refresh_thread_freshness(repo, &mut thread)?;
    if matches!(thread.freshness, ThreadFreshness::Unknown) {
        thread.freshness = ThreadFreshness::Current;
    }
    thread.updated_at = chrono::Utc::now();
    manager.save(&thread)?;
    Ok(())
}

fn mark_merged_threads_unintegrated_for_target(
    repo: &Repository,
    target_thread: &str,
    integrated_state: &ChangeId,
    target_after_undo: &ChangeId,
) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    for thread in manager.list()? {
        if thread.thread == target_thread
            || thread.target_thread.as_deref() != Some(target_thread)
            || thread.state != ThreadState::Merged
        {
            continue;
        }
        let points_at_integrated_state = thread
            .merged_state
            .as_deref()
            .or(thread.current_state.as_deref())
            .and_then(|state| repo.resolve_state(state).ok().flatten())
            .is_some_and(|state| state == *integrated_state);
        if points_at_integrated_state {
            mark_source_thread_unintegrated(repo, &thread.thread, target_after_undo)?;
        }
    }
    Ok(())
}

fn mark_source_thread_integrated(
    repo: &Repository,
    source_thread: &str,
    target_after_redo: &ChangeId,
) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    let Some(mut thread) = manager.find_by_thread(source_thread)? else {
        return Ok(());
    };
    let source_tip = repo.refs().get_thread(&ThreadName::new(source_thread))?;
    let integrated = source_tip
        .as_ref()
        .is_some_and(|source_tip| change_contains(repo, source_tip, target_after_redo));
    if !integrated {
        return Ok(());
    }

    thread.state = ThreadState::Merged;
    thread.merged_state = Some(target_after_redo.short());
    thread.current_state = source_tip
        .map(|source_tip| source_tip.short())
        .or_else(|| Some(target_after_redo.short()));
    thread.integration_policy_result = ThreadIntegrationPolicy {
        status: Some("auto_integrated".to_string()),
        reason: Some("redo restored integrated target state".to_string()),
        manual_resolution_state: thread.integration_policy_result.manual_resolution_state,
    };
    thread.freshness = ThreadFreshness::Current;
    thread.updated_at = chrono::Utc::now();
    manager.save(&thread)?;
    Ok(())
}

fn mark_ready_threads_integrated_for_target(
    repo: &Repository,
    target_thread: &str,
    integrated_state: &ChangeId,
    target_before_redo: &Option<ChangeId>,
) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    for thread in manager.list()? {
        if thread.thread == target_thread
            || thread.target_thread.as_deref() != Some(target_thread)
            || thread.state != ThreadState::Ready
        {
            continue;
        }
        let Some(source_tip) = repo.refs().get_thread(&ThreadName::new(&thread.thread))? else {
            continue;
        };
        let newly_integrated = change_contains(repo, &source_tip, integrated_state)
            && !target_before_redo
                .as_ref()
                .is_some_and(|before| change_contains(repo, &source_tip, before));
        if newly_integrated {
            mark_source_thread_integrated(repo, &thread.thread, integrated_state)?;
        }
    }
    Ok(())
}

fn change_contains(repo: &Repository, ancestor: &ChangeId, descendant: &ChangeId) -> bool {
    let mut graph = CommitGraphIndex::new(repo);
    graph.is_ancestor(ancestor, descendant).unwrap_or(false)
}

/// Remove the ThreadManager record matching `thread_name`. No-op when no
/// record exists. Used by the `ThreadCreate` inverse to keep refs and
/// record-store state in lockstep (cross-thread undo contract rule 4).
fn remove_thread_manager_record(repo: &Repository, thread_name: &str) -> Result<()> {
    let manager = ThreadManager::new(repo.heddle_dir());
    if let Some(thread) = manager.find_by_thread(thread_name)? {
        manager.delete(&thread.id)?;
    }
    Ok(())
}

// ---- Atomic undo/redo (heddle#355 impl-b) ----
//
// `undo`/`redo` are migrated to the `AtomicMutation` primitive so the whole
// operation is all-or-nothing: a failure anywhere mid-apply rewinds every
// already-applied step back to the exact pre-operation state instead of
// leaving the repo half-rewound (the spike §5.1 hazard — batch N fails after
// batches `0..N` were applied AND marked undone, with no rollback).
//
// SHAPE. `undo`/`redo` perform direct, immediately-visible, **idempotent**
// canonical mutations (ref writes, `goto` worktree material, thread-record and
// git-mirror state, and the in-place `mark_batch_undone` flag flip) and append
// NO new domain oplog record — they navigate states that already exist. So:
//   * Each sub-op stages its effect and registers its inverse via the granular
//     `Tx::on_rewind` ledger (the inverse of "undo entry E" is "redo entry E",
//     and vice-versa; both are absolute SET operations, so the inverse restores
//     the pre-step state regardless of how far a failed step got).
//   * The parent NESTS the sub-ops via `Tx::enroll` (savepoint enrollment) —
//     the recovery-ref child then one child per batch — so a child that stages
//     then fails rewinds the child AND unwinds the parent through the shared
//     ledger. This is the nesting path the migration exists to validate.
//   * The commit point is the executor's lone `TransactionCommit` marker over
//     an EMPTY domain batch (`StagedCommit::pure`). `OpBatch::is_transaction_
//     marker_only` keeps that record-less commit sentinel out of the undo/redo
//     eligibility scans and the `undo --list` view.
//
// TWO VALIDATION NOTES (see the PR description) — neither blocks the migration,
// both are properties of mapping a self-mutating, immediately-visible op onto
// the primitive:
//   1. IDEMPOTENCY KEY. `undo`/`redo` have no unique content identity (they
//      revisit existing states), so a key derived from "operation identity"
//      (batch ids + head) COLLIDES on the legitimate `undo → redo → undo`
//      toggle, and the primitive's dedup-then-`rewind_all` on a hit would
//      silently REVERT the second undo. The key is therefore derived from the
//      oplog GENERATION (`head_id`) at command start — unique per committed
//      transaction (every undo/redo appends a marker that bumps the
//      generation), so the dedup branch is never taken. The crash-retry dedup
//      the trait optimizes for is both unreachable (a committed undo marks its
//      batches undone, so a retry re-derives a different batch set) and
//      unnecessary (re-applying an undo is idempotent) for this op.
//   2. SAVEPOINT SEMANTICS. The children use the savepoint ENROLLMENT mechanism
//      (`enroll` + `on_rewind`) for its apply+ledger-rewind shape, but their
//      effects are direct canonical writes (visible immediately), not the
//      invisible-until-commit staging `SavepointMutation` describes. This adds
//      FAILURE atomicity (rewind), not concurrent-reader isolation — matching
//      the pre-migration concurrency semantics, where undo already published
//      refs batch-by-batch.
//
// EXACTNESS SCOPE. The rewind restores the exact pre-operation state for `undo`
// of any batch count and for single-batch `redo` (the `-n 1` default). A
// MULTI-batch `redo -n N>1` replays batches newest-first with absolute `goto`s
// (pre-existing forward behavior this migration preserves), so the per-entry
// inverses do not compose back to the exact pre-redo head — a mid-redo fault
// there rewinds to a consistent intermediate state, not the precise pre-redo
// tip. Still strictly safer than the pre-migration path, which had no rollback
// at all. Fixing it would mean reordering redo replay (a forward-behavior
// change) — out of scope for the atomicity migration.

/// Convert an `anyhow` error raised by an undo/redo apply helper into the
/// `HeddleError` the primitive's `Result` requires. The structured
/// `RecoveryAdvice` refusals are produced by the command-level preflights
/// (which run BEFORE `execute`), so a wrapped message here only ever surfaces a
/// genuinely-unexpected mid-apply failure — one the preflights could not
/// foresee — whose rewind has already restored the pre-operation state.
fn apply_error(err: anyhow::Error) -> HeddleError {
    HeddleError::Conflict(format!("{err:#}"))
}

/// Build the stable-per-transaction idempotency key. Derived from the oplog
/// `generation` (read at command start) rather than the batch contents — see
/// the "IDEMPOTENCY KEY" note above for why a content-derived key is unsafe for
/// a self-mutating op.
pub(super) fn undo_redo_transaction_id(
    action: &str,
    scope: &str,
    generation: u64,
    batches: &[OpBatch],
) -> String {
    let ids: Vec<String> = batches.iter().map(|batch| batch.id.to_string()).collect();
    format!("{action}:{scope}:gen{generation}:[{}]", ids.join(","))
}

/// Savepoint child: preserve the pre-undo HEAD into the heddle-internal
/// recovery pointer (the heddle#305 `ORIG_HEAD`-style ref), registering its
/// restore as the inverse so an outer failure puts the prior pointer back
/// (or clears it, on the first-ever undo).
struct StageUndoRecovery {
    head: Option<ChangeId>,
}

impl AtomicMutation for StageUndoRecovery {
    type Output = ();

    fn transaction_id(&self) -> String {
        // Enrolled children never reach the commit point; only the root's id is
        // used. A constant is sufficient and never minted fresh.
        "undo:stage-recovery".to_string()
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
        let Some(state) = self.head else {
            return Ok(StagedCommit::pure(()));
        };
        let repo = tx.repo();
        // Reconciled read of the prior pointer so the inverse restores exactly
        // what was there (never a raw-ref bypass).
        let prior = repo.refs().get_undo_recovery()?;
        tx.on_rewind(move || match prior {
            Some(prior) => repo.refs().set_undo_recovery(&prior),
            None => repo.refs().clear_undo_recovery(),
        });
        repo.refs().set_undo_recovery(&state)?;
        Ok(StagedCommit::pure(()))
    }
}

impl SavepointMutation for StageUndoRecovery {}

/// Savepoint child: undo one batch. Each entry's inverse (`apply_redo_entry`)
/// is registered on the shared ledger BEFORE its `apply_undo_entry` runs, so a
/// mid-batch failure rewinds exactly the entries already touched; the
/// `mark_batch_undone` flip is paired with its `mark_batch_redone` inverse.
struct ApplyUndoBatch {
    batch: OpBatch,
    /// Test seam: when `Some(n)`, fail immediately after undoing `n` entries,
    /// to exercise the mid-batch rewind path. Always `None` in production.
    #[cfg(test)]
    fail_after_entries: Option<usize>,
}

impl ApplyUndoBatch {
    fn new(batch: OpBatch) -> Self {
        Self {
            batch,
            #[cfg(test)]
            fail_after_entries: None,
        }
    }

    #[cfg(test)]
    fn failing_after(batch: OpBatch, entries: usize) -> Self {
        Self {
            batch,
            fail_after_entries: Some(entries),
        }
    }
}

impl AtomicMutation for ApplyUndoBatch {
    type Output = OpBatch;

    fn transaction_id(&self) -> String {
        format!("undo:batch:{}", self.batch.id)
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<OpBatch>> {
        let repo = tx.repo();
        for (applied, entry) in self.batch.entries.iter().rev().enumerate() {
            let redo_entry = entry.clone();
            tx.on_rewind(move || apply_redo_entry(repo, &redo_entry).map_err(apply_error));
            apply_undo_entry(repo, entry).map_err(apply_error)?;
            #[cfg(test)]
            if self.fail_after_entries == Some(applied + 1) {
                return Err(HeddleError::Conflict(
                    "injected mid-undo fault".to_string(),
                ));
            }
            #[cfg(not(test))]
            let _ = applied;
        }
        let batch_for_redo = self.batch.clone();
        tx.on_rewind(move || repo.oplog().mark_batch_redone(&batch_for_redo).map(|_| ()));
        let updated = repo.oplog().mark_batch_undone(&self.batch)?;
        Ok(StagedCommit::pure(updated))
    }
}

impl SavepointMutation for ApplyUndoBatch {}

/// Savepoint child: redo one batch — the mirror of [`ApplyUndoBatch`]. Entries
/// replay in forward order; each inverse is `apply_undo_entry`, and the
/// `mark_batch_redone` flip pairs with `mark_batch_undone`.
struct ApplyRedoBatch {
    batch: OpBatch,
    #[cfg(test)]
    fail_after_entries: Option<usize>,
}

impl ApplyRedoBatch {
    fn new(batch: OpBatch) -> Self {
        Self {
            batch,
            #[cfg(test)]
            fail_after_entries: None,
        }
    }

    #[cfg(test)]
    fn failing_after(batch: OpBatch, entries: usize) -> Self {
        Self {
            batch,
            fail_after_entries: Some(entries),
        }
    }
}

impl AtomicMutation for ApplyRedoBatch {
    type Output = OpBatch;

    fn transaction_id(&self) -> String {
        format!("redo:batch:{}", self.batch.id)
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<OpBatch>> {
        let repo = tx.repo();
        for (applied, entry) in self.batch.entries.iter().enumerate() {
            let undo_entry = entry.clone();
            tx.on_rewind(move || apply_undo_entry(repo, &undo_entry).map_err(apply_error));
            apply_redo_entry(repo, entry).map_err(apply_error)?;
            #[cfg(test)]
            if self.fail_after_entries == Some(applied + 1) {
                return Err(HeddleError::Conflict(
                    "injected mid-redo fault".to_string(),
                ));
            }
            #[cfg(not(test))]
            let _ = applied;
        }
        let batch_for_undo = self.batch.clone();
        tx.on_rewind(move || repo.oplog().mark_batch_undone(&batch_for_undo).map(|_| ()));
        let updated = repo.oplog().mark_batch_redone(&self.batch)?;
        Ok(StagedCommit::pure(updated))
    }
}

impl SavepointMutation for ApplyRedoBatch {}

/// Root composite for `heddle undo`: stage the recovery pointer, then nest one
/// [`ApplyUndoBatch`] per batch. Returns the updated (undone) batches for the
/// command's output. Appends no domain record — the executor's commit marker
/// is the sole commit point.
pub(super) struct UndoOp {
    batches: Vec<OpBatch>,
    recovery_head: Option<ChangeId>,
    transaction_id: String,
}

impl UndoOp {
    pub(super) fn new(
        batches: Vec<OpBatch>,
        recovery_head: Option<ChangeId>,
        transaction_id: String,
    ) -> Self {
        Self {
            batches,
            recovery_head,
            transaction_id,
        }
    }
}

impl AtomicMutation for UndoOp {
    type Output = Vec<OpBatch>;

    fn transaction_id(&self) -> String {
        self.transaction_id.clone()
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<Vec<OpBatch>>> {
        tx.enroll(StageUndoRecovery {
            head: self.recovery_head,
        })?;
        let mut updated = Vec::with_capacity(self.batches.len());
        for batch in &self.batches {
            let staged = tx.enroll(ApplyUndoBatch::new(batch.clone()))?;
            updated.push(staged.output);
        }
        Ok(StagedCommit::pure(updated))
    }
}

/// Root composite for `heddle redo`: nest one [`ApplyRedoBatch`] per batch. No
/// recovery child (redo restores the pre-undo state the recovery pointer was
/// captured against).
pub(super) struct RedoOp {
    batches: Vec<OpBatch>,
    transaction_id: String,
}

impl RedoOp {
    pub(super) fn new(batches: Vec<OpBatch>, transaction_id: String) -> Self {
        Self {
            batches,
            transaction_id,
        }
    }
}

impl AtomicMutation for RedoOp {
    type Output = Vec<OpBatch>;

    fn transaction_id(&self) -> String {
        self.transaction_id.clone()
    }

    fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<Vec<OpBatch>>> {
        let mut updated = Vec::with_capacity(self.batches.len());
        for batch in &self.batches {
            let staged = tx.enroll(ApplyRedoBatch::new(batch.clone()))?;
            updated.push(staged.output);
        }
        Ok(StagedCommit::pure(updated))
    }
}

#[cfg(test)]
mod atomic_tests {
    use super::*;
    use tempfile::TempDir;

    /// Init a repo and create two snapshots on `main`. The worktree at `s2`
    /// holds both `a.txt` (from `s1`) and `b.txt` (from `s2`); `s1` holds only
    /// `a.txt`; the initial state holds neither. Returns the repo + temp dir +
    /// the two states.
    fn repo_with_two_snapshots() -> (TempDir, Repository, ChangeId, ChangeId) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("a.txt"), "a").unwrap();
        let s1 = repo.snapshot(Some("s1".to_string()), None).unwrap();
        std::fs::write(temp.path().join("b.txt"), "b").unwrap();
        let s2 = repo.snapshot(Some("s2".to_string()), None).unwrap();
        (temp, repo, s1.change_id, s2.change_id)
    }

    #[test]
    fn apply_error_wraps_anyhow_into_conflict() {
        let wrapped = apply_error(anyhow!("boom"));
        assert!(
            matches!(&wrapped, HeddleError::Conflict(message) if message.contains("boom")),
            "an apply-helper error must surface as a HeddleError::Conflict carrying the message"
        );
    }

    fn commit_marker_count(repo: &Repository) -> usize {
        repo.oplog()
            .recent(256)
            .unwrap()
            .iter()
            .filter(|entry| matches!(entry.operation, OpRecord::TransactionCommit { .. }))
            .count()
    }

    fn main_thread(repo: &Repository) -> Option<ChangeId> {
        repo.refs().get_thread(&ThreadName::new("main")).unwrap()
    }

    /// Test-only parent mirroring [`UndoOp`] but injecting a fault: the LAST
    /// enrolled batch child fails after undoing `fail_after` of its entries.
    /// Reuses the REAL [`StageUndoRecovery`] + [`ApplyUndoBatch`] children, so
    /// it exercises the real compensators + nesting + rewind path.
    struct FaultyUndo {
        batches: Vec<OpBatch>,
        recovery_head: Option<ChangeId>,
        fail_after: usize,
    }

    impl AtomicMutation for FaultyUndo {
        type Output = ();

        fn transaction_id(&self) -> String {
            "test-undo-fault".to_string()
        }

        fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
            tx.enroll(StageUndoRecovery {
                head: self.recovery_head,
            })?;
            let last = self.batches.len() - 1;
            for (i, batch) in self.batches.iter().enumerate() {
                if i == last {
                    tx.enroll(ApplyUndoBatch::failing_after(batch.clone(), self.fail_after))?;
                } else {
                    tx.enroll(ApplyUndoBatch::new(batch.clone()))?;
                }
            }
            Ok(StagedCommit::pure(()))
        }
    }

    /// Test-only parent mirroring [`RedoOp`] with an injected fault on the last
    /// enrolled batch child.
    struct FaultyRedo {
        batches: Vec<OpBatch>,
        fail_after: usize,
    }

    impl AtomicMutation for FaultyRedo {
        type Output = ();

        fn transaction_id(&self) -> String {
            "test-redo-fault".to_string()
        }

        fn apply(&mut self, tx: &mut Tx<'_>) -> HeddleResult<StagedCommit<()>> {
            let last = self.batches.len() - 1;
            for (i, batch) in self.batches.iter().enumerate() {
                if i == last {
                    tx.enroll(ApplyRedoBatch::failing_after(batch.clone(), self.fail_after))?;
                } else {
                    tx.enroll(ApplyRedoBatch::new(batch.clone()))?;
                }
            }
            Ok(StagedCommit::pure(()))
        }
    }

    /// Behavioral parity: a clean atomic `UndoOp` reverts the worktree, HEAD,
    /// and thread ref, marks the batch undone, captures the recovery pointer,
    /// and commits exactly one marker — same observable result as the
    /// pre-migration sequential path.
    #[test]
    fn atomic_undo_success_reverts_and_records_recovery() {
        let (temp, repo, s1, s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();

        let recovery_head = repo.head().unwrap();
        let batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &batches);
        let updated = repo::atomic::execute(&repo, UndoOp::new(batches, recovery_head, txid)).unwrap();

        assert_eq!(updated.len(), 1);
        assert!(updated[0].entries.iter().all(|e| e.undone));
        assert_eq!(repo.head().unwrap(), Some(s1), "HEAD reverted to s1");
        assert_eq!(main_thread(&repo), Some(s1));
        assert!(temp.path().join("a.txt").exists(), "s1 file kept");
        assert!(!temp.path().join("b.txt").exists(), "s2 file reverted");
        assert_eq!(
            repo.refs().get_undo_recovery().unwrap(),
            Some(s2),
            "recovery pointer pins the pre-undo tip"
        );
        assert_eq!(commit_marker_count(&repo), 1, "exactly one commit marker");
    }

    /// Fault-injection: a failure mid-undo (after the first batch is fully
    /// applied, partway into the second) rewinds EVERY applied step back to the
    /// exact pre-operation state — no partial ref / oplog / worktree leak.
    #[test]
    fn fault_mid_undo_rewinds_to_pre_operation_state() {
        let (temp, repo, _s1, s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();

        let pre_head = repo.head().unwrap();
        assert_eq!(pre_head, Some(s2));
        let pre_main = main_thread(&repo);
        assert_eq!(repo.refs().get_undo_recovery().unwrap(), None);
        let pre_markers = commit_marker_count(&repo);

        let batches = repo.oplog().undo_batches_scoped(2, Some(&scope)).unwrap();
        assert_eq!(batches.len(), 2, "two snapshots are undoable");
        let result = repo::atomic::execute(
            &repo,
            FaultyUndo {
                batches,
                recovery_head: pre_head,
                fail_after: 1,
            },
        );
        assert!(result.is_err(), "the injected fault must fail the undo");

        // Exact pre-operation state restored across every dimension.
        assert_eq!(repo.head().unwrap(), Some(s2), "HEAD rewound to pre-undo tip");
        assert_eq!(main_thread(&repo), pre_main, "main ref rewound");
        assert!(temp.path().join("a.txt").exists(), "s1 file restored");
        assert!(temp.path().join("b.txt").exists(), "s2 file restored");
        assert_eq!(
            repo.oplog()
                .undo_batches_scoped(2, Some(&scope))
                .unwrap()
                .len(),
            2,
            "no batch left marked undone"
        );
        assert_eq!(
            repo.refs().get_undo_recovery().unwrap(),
            None,
            "recovery pointer cleared by rewind (it had no prior value)"
        );
        assert_eq!(
            commit_marker_count(&repo),
            pre_markers,
            "a failed transaction commits no marker"
        );
    }

    /// Fault-injection: a failure mid-redo rewinds the replay back to the
    /// fully-undone pre-redo state — no partial effect leaks.
    #[test]
    fn fault_mid_redo_rewinds_to_pre_operation_state() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        std::fs::write(temp.path().join("a.txt"), "a").unwrap();
        let _s1 = repo.snapshot(Some("s1".to_string()), None).unwrap();
        let scope = repo.op_scope();

        // Cleanly undo the single snapshot through the real atomic UndoOp.
        let recovery_head = repo.head().unwrap();
        let undo_batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &undo_batches);
        repo::atomic::execute(&repo, UndoOp::new(undo_batches, recovery_head, txid)).unwrap();

        // Pre-redo state: the initial (pre-s1) state — a.txt gone, one batch
        // redoable.
        assert!(!temp.path().join("a.txt").exists(), "undone: a.txt gone");
        let pre_redo_head = repo.head().unwrap();
        let pre_redo_main = main_thread(&repo);
        assert_eq!(
            repo.oplog()
                .redo_batches_scoped(1, Some(&scope))
                .unwrap()
                .len(),
            1,
            "one batch is redoable"
        );
        let pre_markers = commit_marker_count(&repo);

        let redo_batches = repo.oplog().redo_batches_scoped(1, Some(&scope)).unwrap();
        let result = repo::atomic::execute(
            &repo,
            FaultyRedo {
                batches: redo_batches,
                fail_after: 1,
            },
        );
        assert!(result.is_err(), "the injected fault must fail the redo");

        // Rewound to the fully-undone pre-redo state.
        assert_eq!(repo.head().unwrap(), pre_redo_head, "HEAD rewound");
        assert_eq!(main_thread(&repo), pre_redo_main, "main ref rewound");
        assert!(!temp.path().join("a.txt").exists(), "s1 file not resurrected");
        assert_eq!(
            repo.oplog()
                .redo_batches_scoped(1, Some(&scope))
                .unwrap()
                .len(),
            1,
            "batch still redoable"
        );
        assert_eq!(
            commit_marker_count(&repo),
            pre_markers,
            "a failed transaction commits no marker"
        );
    }

    /// A successful round trip via the atomic ops: undo then redo restores the
    /// original tip, and the marker-only commit batches are excluded from the
    /// undo/redo eligibility scans (so the round trip terminates instead of
    /// chasing its own commit sentinels).
    #[test]
    fn atomic_undo_redo_round_trip_ignores_commit_markers() {
        let (temp, repo, s1, s2) = repo_with_two_snapshots();
        let scope = repo.op_scope();

        // Undo s2.
        let recovery_head = repo.head().unwrap();
        let undo_batches = repo.oplog().undo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("undo", &scope, generation, &undo_batches);
        repo::atomic::execute(&repo, UndoOp::new(undo_batches, recovery_head, txid)).unwrap();
        assert_eq!(repo.head().unwrap(), Some(s1));

        // The undo's commit marker is a record-less batch — not itself undoable.
        let still_undoable = repo.oplog().undo_batches_scoped(2, Some(&scope)).unwrap();
        assert_eq!(
            still_undoable.len(),
            1,
            "only the s1 snapshot remains undoable; the commit marker is excluded"
        );

        // Redo s2.
        let redo_batches = repo.oplog().redo_batches_scoped(1, Some(&scope)).unwrap();
        let generation = repo.oplog().head_id().unwrap();
        let txid = undo_redo_transaction_id("redo", &scope, generation, &redo_batches);
        repo::atomic::execute(&repo, RedoOp::new(redo_batches, txid)).unwrap();
        assert_eq!(repo.head().unwrap(), Some(s2), "redo restored the s2 tip");
        assert!(temp.path().join("b.txt").exists(), "s2 file restored by redo");
    }
}
