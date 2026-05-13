// SPDX-License-Identifier: Apache-2.0
//! Sweep expired ephemeral threads.
//!
//! Runs as a side effect of cheap, oplog-touching reads (`heddle status`,
//! `heddle log`, `heddle thread list`). For each thread whose ephemeral
//! TTL has elapsed:
//!
//! 1. Mark its [`ThreadState`] as [`ThreadState::Abandoned`] — the
//!    record is preserved (NOT deleted), so the thread name and its
//!    history remain reachable via `goto <change-id>`.
//! 2. Append an [`OpRecord::EphemeralThreadCollapse`] entry so the
//!    transition is part of the audit trail and shows up in
//!    `heddle log --all`.
//!
//! The sweep is best-effort: any error logging or persisting a single
//! collapse is tracing-warned and the next thread proceeds. A read
//! command never fails because of a sweep glitch — that would be the
//! tail wagging the dog.

use anyhow::Result;
use objects::object::ChangeId;
use oplog::OpRecord;
use repo::Repository;
use repo::ephemeral_thread::{CollapsedThread, collapse_expired_ephemeral_threads};
use repo::{ThreadManager, ThreadState};
use tracing::warn;

/// Walk threads, sweep expired ephemeral markers, persist the
/// transitions, and append the matching `OpRecord::EphemeralThreadCollapse`
/// entries. Returns the threads collapsed on this pass — callers can
/// surface this to the reader (or ignore it; the whole point is "happens
/// quietly").
pub fn run_ephemeral_sweep(repo: &Repository) -> Result<Vec<CollapsedThread>> {
    let manager = ThreadManager::new(repo.heddle_dir());
    let threads = manager.list()?;
    if threads.is_empty() {
        return Ok(Vec::new());
    }

    // Build a `Vec<ThreadRecord>` shadow so the pure collapse function
    // can mutate without touching the live `Thread` objects (which
    // carry workspace state). After the pass, we look up each
    // collapsed entry by `thread_id`, mark the live `Thread` as
    // `Abandoned`, and save it back through the manager so the
    // workspace half stays intact.
    let mut records: Vec<_> = threads.iter().map(|t| t.to_record()).collect();
    let now = chrono::Utc::now();
    let collapsed = collapse_expired_ephemeral_threads(&mut records, now);
    if collapsed.is_empty() {
        return Ok(Vec::new());
    }

    let mut ops: Vec<OpRecord> = Vec::with_capacity(collapsed.len());
    for entry in &collapsed {
        // Find the live thread, mark it Abandoned, save.
        if let Some(mut thread) = manager.load(&entry.thread_id)? {
            thread.state = ThreadState::Abandoned;
            thread.updated_at = now;
            if let Err(err) = manager.save(&thread) {
                warn!(error = %err, thread_id = %entry.thread_id,
                    "failed to persist ephemeral collapse — sweep will retry next read");
                continue;
            }
        }
        // Record the oplog entry. `final_state` may be `None` for
        // never-snapshotted threads; in that rare case we synthesise a
        // zero-valued `ChangeId` so the proto field stays populated —
        // the oplog entry is still meaningful as a "this thread
        // collapsed without producing a state" marker.
        let final_state = entry
            .final_state
            .unwrap_or_else(|| ChangeId::from_bytes([0; 16]));
        ops.push(OpRecord::EphemeralThreadCollapse {
            thread: entry.thread_name.clone(),
            final_state,
        });
    }
    if !ops.is_empty()
        && let Err(err) = repo.oplog().record_batch(ops)
    {
        warn!(error = %err, "failed to append EphemeralThreadCollapse oplog entries");
    }
    Ok(collapsed)
}

/// Convenience wrapper used by read-shaped CLI commands. Treats sweep
/// failures as non-fatal and swallows them (with a tracing warning) so
/// `status`/`log`/`thread list` don't error out on a corner-case
/// thread-record write.
pub fn try_run_ephemeral_sweep(repo: &Repository) {
    if let Err(err) = run_ephemeral_sweep(repo) {
        warn!(error = %err, "ephemeral sweep encountered an error; skipping");
    }
}