// SPDX-License-Identifier: Apache-2.0
//! Apply undo/redo operations to the repository.

use anyhow::{Result, anyhow};
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
        OpRecord::ThreadCreate { name, .. } => {
            delete_thread_safely(repo, name)?;
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
        // during the FF, so it's untouched. Closes heddle#99 — the bug
        // where recording an FF as `OpRecord::Goto` left the target
        // thread ref stranded at the FF target after undo.
        OpRecord::FastForward {
            target_thread,
            pre_target_id,
            ..
        } => {
            repo.goto_without_record(pre_target_id)?;
            repo.refs().set_thread(target_thread, pre_target_id)?;
            repo.refs().write_head(&Head::Attached {
                thread: target_thread.clone(),
            })?;
            sync_thread_record_state(repo, target_thread, *pre_target_id)?;
        }
        _ => {}
    }

    Ok(())
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
        OpRecord::ThreadCreate { name, state } => {
            repo.refs().set_thread(name, state)?;
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
        // FF merge redo: re-advance both HEAD and the target thread ref
        // to the source thread's current tip. We re-read the source
        // thread rather than caching its tip in the OpRecord because the
        // source thread is the authoritative pointer for "what FF means
        // right now" — if it moved between undo and redo, we'd otherwise
        // redo to a stale state.
        OpRecord::FastForward {
            source_thread,
            target_thread,
            ..
        } => {
            let source_tip = repo.refs().get_thread(source_thread)?.ok_or_else(|| {
                anyhow!(
                    "cannot redo fast-forward: source thread '{}' no longer exists",
                    source_thread
                )
            })?;
            repo.goto_without_record(&source_tip)?;
            repo.refs().set_thread(target_thread, &source_tip)?;
            repo.refs().write_head(&Head::Attached {
                thread: target_thread.clone(),
            })?;
            sync_thread_record_state(repo, target_thread, source_tip)?;
        }
        _ => {}
    }

    Ok(())
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
