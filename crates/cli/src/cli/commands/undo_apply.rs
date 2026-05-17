// SPDX-License-Identifier: Apache-2.0
//! Apply undo/redo operations to the repository.

use anyhow::{Result, anyhow};
use objects::object::ChangeId;
use oplog::{OpBatch, OpEntry, OpRecord};
use refs::Head;
use repo::{Repository, ThreadManager};

pub(super) fn apply_undo_batch(repo: &Repository, batch: &OpBatch) -> Result<()> {
    for entry in batch.entries.iter().rev() {
        apply_undo_entry(repo, entry)?;
    }
    Ok(())
}

pub(super) fn apply_redo_batch(repo: &Repository, batch: &OpBatch) -> Result<()> {
    for entry in &batch.entries {
        apply_redo_entry(repo, entry)?;
    }
    Ok(())
}

fn apply_undo_entry(repo: &Repository, entry: &OpEntry) -> Result<()> {
    match &entry.operation {
        OpRecord::Snapshot {
            prev_head: Some(prev),
            thread,
            ..
        } => {
            repo.goto_without_record(prev)?;
            if let Some(thread) = thread {
                repo.refs().set_thread(thread, prev)?;
                repo.refs().write_head(&Head::Attached {
                    thread: thread.clone(),
                })?;
                sync_thread_record_state(repo, thread, *prev)?;
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
            delete_thread_safely(repo, name)?;
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
            repo.refs().set_thread(name, state)?;
        }
        OpRecord::ThreadUpdate {
            name, old_state, ..
        } => {
            repo.refs().set_thread(name, old_state)?;
        }
        OpRecord::MarkerCreate { name, .. } => {
            repo.refs().delete_marker(name)?;
        }
        OpRecord::MarkerDelete { name, state } => {
            repo.refs().create_marker(name, state)?;
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
            target_thread,
            pre_target_id,
            ..
        }
        | OpRecord::FastForwardV2 {
            target_thread,
            pre_target_id,
            ..
        } => {
            apply_ff_undo(repo, target_thread, pre_target_id)?;
        }
        _ => {}
    }

    Ok(())
}

fn apply_ff_undo(repo: &Repository, target_thread: &str, pre_target_id: &ChangeId) -> Result<()> {
    repo.goto_without_record(pre_target_id)?;
    repo.refs().set_thread(target_thread, pre_target_id)?;
    repo.refs().write_head(&Head::Attached {
        thread: target_thread.to_string(),
    })?;
    sync_thread_record_state(repo, target_thread, *pre_target_id)
}

fn apply_redo_entry(repo: &Repository, entry: &OpEntry) -> Result<()> {
    match &entry.operation {
        OpRecord::Snapshot {
            new_state, thread, ..
        } => {
            repo.goto_without_record(new_state)?;
            if let Some(thread) = thread {
                repo.refs().set_thread(thread, new_state)?;
                repo.refs().write_head(&Head::Attached {
                    thread: thread.clone(),
                })?;
                sync_thread_record_state(repo, thread, *new_state)?;
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
            repo.refs().set_thread(name, state)?;
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
            repo.refs().set_thread(name, state)?;
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
            delete_thread_safely(repo, name)?;
        }
        OpRecord::ThreadUpdate {
            name, new_state, ..
        } => {
            repo.refs().set_thread(name, new_state)?;
        }
        OpRecord::MarkerCreate { name, state } => {
            repo.refs().create_marker(name, state)?;
        }
        OpRecord::MarkerDelete { name, .. } => {
            repo.refs().delete_marker(name)?;
        }
        // FF merge redo (V2): replay the *recorded* FF target. We do
        // not re-read `source_thread` — the recorded `post_target_id`
        // is the exact state the target advanced to at the original
        // FF, so redo is deterministic regardless of what the source
        // thread did between undo and redo (advanced, was deleted,
        // etc.). Closes heddle#99 r2 — Codex's non-determinism finding
        // on the r1 implementation.
        OpRecord::FastForwardV2 {
            target_thread,
            post_target_id,
            ..
        } => {
            apply_ff_redo(repo, target_thread, post_target_id)?;
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
            let source_tip = repo.refs().get_thread(source_thread)?.ok_or_else(|| {
                anyhow!(
                    "cannot redo fast-forward: source thread '{}' no longer exists \
                     (legacy V1 oplog record; re-run the merge or `heddle gc oplog` to prune)",
                    source_thread
                )
            })?;
            apply_ff_redo(repo, target_thread, &source_tip)?;
        }
        _ => {}
    }

    Ok(())
}

fn apply_ff_redo(repo: &Repository, target_thread: &str, post_target_id: &ChangeId) -> Result<()> {
    repo.goto_without_record(post_target_id)?;
    repo.refs().set_thread(target_thread, post_target_id)?;
    repo.refs().write_head(&Head::Attached {
        thread: target_thread.to_string(),
    })?;
    sync_thread_record_state(repo, target_thread, *post_target_id)
}

fn delete_thread_safely(repo: &Repository, name: &str) -> Result<()> {
    if let Head::Attached { thread } = repo.head_ref()?
        && thread == name
    {
        let state = repo
            .refs()
            .get_thread(name)?
            .ok_or_else(|| anyhow!("Thread not found: {}", name))?;
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
