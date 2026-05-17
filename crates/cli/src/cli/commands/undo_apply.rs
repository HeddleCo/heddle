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
        OpRecord::Redact { redaction_id, .. } => {
            repo.remove_redaction(redaction_id)?;
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