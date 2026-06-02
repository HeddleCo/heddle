// SPDX-License-Identifier: Apache-2.0
//! Shared helper for fast-forward call sites that need to record an
//! `OpRecord::FastForwardV2` instead of the implicit `OpRecord::Goto`.
//!
//! See heddle#99 (merge FF) and heddle#110 (rebase / pull / ship /
//! merge-abort): recording a thread-advancing fast-forward as a plain
//! `OpRecord::Goto` strands the target thread ref on undo, because the
//! `Goto` inverse only rewinds HEAD. The fix is to perform the FF
//! without recording the implicit `Goto`, then explicitly emit an
//! `OpRecord::FastForwardV2` carrying both `pre_target_id` (undo
//! target) and `post_target_id` (redo target — heddle#99 r2's
//! deterministic-redo contract).
//!
//! Detached-HEAD callers fall back to `OpRecord::Goto`: there's no
//! thread ref to strand, and the legacy `Goto` inverse correctly
//! rewinds HEAD on its own.

use anyhow::{Result, anyhow};
use objects::object::{ChangeId, ThreadName};
use oplog::OpRecord;
use refs::Head;
use repo::Repository;

use super::advice::RecoveryAdvice;

/// Perform a fast-forward of the attached thread to `post_target_id`
/// and record the operation so undo restores the target thread ref to
/// its pre-FF tip *and* redo replays deterministically to
/// `post_target_id`.
///
/// Reads the pre-FF tip from the attached thread's ref (or from HEAD
/// for detached). Use this overload at call sites where the thread
/// ref has *not* been mutated since whatever you want undo to restore
/// — e.g. merge / rebase / ship. Pull pre-sets the thread ref before
/// materializing and must call [`record_ff_advance_explicit`].
///
/// `source_thread` is the thread name surfaced in the OpRecord for
/// forensic context. Neither undo nor redo reads it. For rebase-replay
/// loops where no single source thread is in scope, pass a synthetic
/// placeholder like `"<rebase>"`.
///
/// If HEAD is detached before the FF, falls back to recording an
/// `OpRecord::Goto` (legacy behavior — no thread ref to strand).
pub(super) fn record_ff_advance(
    repo: &Repository,
    source_thread: &str,
    post_target_id: &ChangeId,
) -> Result<()> {
    let head_before = repo.head_ref()?;
    let pre_target_id = match &head_before {
        Head::Attached { thread } => attached_thread_tip(repo, thread)?,
        Head::Detached { state } => *state,
    };
    record_ff_advance_inner(
        repo,
        source_thread,
        &head_before,
        &pre_target_id,
        post_target_id,
        false,
        None,
    )
}

/// Variant of [`record_ff_advance`] that takes an explicit
/// `pre_target_id`. Use when the thread ref was already mutated
/// before the FF (e.g. `cmd_pull` advances the local thread ref
/// directly so a non-materializing pull still advances the ref), so
/// reading it back would return the *post* state.
///
/// Caller must capture `pre_target_id` *before* the mutating
/// operation that precedes the FF. The preceding mutation must also
/// have run its dirty-worktree guard. This helper passes that captured
/// state as the materialized baseline because the pre-published ref
/// makes the old clean worktree look dirty against the new ref.
pub(super) fn record_ff_advance_explicit(
    repo: &Repository,
    source_thread: &str,
    pre_target_id: &ChangeId,
    post_target_id: &ChangeId,
) -> Result<()> {
    let head_before = repo.head_ref()?;
    record_ff_advance_inner(
        repo,
        source_thread,
        &head_before,
        pre_target_id,
        post_target_id,
        false,
        Some(Some(*pre_target_id)),
    )
}

/// Record a fast-forward whose worktree reset is itself the explicit
/// destructive action, such as aborting a merge with conflict markers
/// in the worktree.
pub(super) fn record_ff_advance_discard_local(
    repo: &Repository,
    source_thread: &str,
    post_target_id: &ChangeId,
) -> Result<()> {
    let head_before = repo.head_ref()?;
    let pre_target_id = match &head_before {
        Head::Attached { thread } => attached_thread_tip(repo, thread)?,
        Head::Detached { state } => *state,
    };
    record_ff_advance_inner(
        repo,
        source_thread,
        &head_before,
        &pre_target_id,
        post_target_id,
        true,
        None,
    )
}

/// Mutate the worktree to fast-forward the attached thread to
/// `post_target_id` *without* writing to the oplog. Returns the
/// `OpRecord` that [`record_ff_advance`] would have written so the
/// caller can fold it into a larger batch.
///
/// Used by the rebase replay loop (heddle#198): per-commit FF records
/// are accumulated and emitted as a single oplog batch at the end of
/// the rebase, so `heddle undo` treats the whole rebase as one undo
/// unit instead of one undo per replayed commit. The mutation half
/// runs immediately (so the next replay step sees the advanced tip);
/// the recording half is deferred to the batch flush.
///
/// Detached-HEAD fallback matches [`record_ff_advance`]: returns an
/// `OpRecord::Goto` so the legacy `Goto` inverse correctly rewinds
/// HEAD on undo without trying to restore a non-existent thread ref.
pub(super) fn ff_advance_deferred(
    repo: &Repository,
    source_thread: &str,
    post_target_id: &ChangeId,
    discard_local_changes: bool,
) -> Result<OpRecord> {
    let head_before = repo.head_ref()?;
    let pre_target_id = match &head_before {
        Head::Attached { thread } => attached_thread_tip(repo, thread)?,
        Head::Detached { state } => *state,
    };
    if discard_local_changes {
        repo.fast_forward_attached_without_record_discard_local(post_target_id)?;
    } else {
        repo.fast_forward_attached_without_record(post_target_id)?;
    }
    Ok(match head_before {
        Head::Attached { thread } => OpRecord::FastForwardV2 {
            source_thread: source_thread.to_string(),
            target_thread: thread.to_string(),
            pre_target_id,
            post_target_id: *post_target_id,
        },
        Head::Detached { state } => OpRecord::Goto {
            target: *post_target_id,
            prev_head: Some(state),
            head: *post_target_id,
        },
    })
}

fn attached_thread_tip(repo: &Repository, thread: &ThreadName) -> Result<ChangeId> {
    repo.refs()
        .get_thread(thread)?
        .ok_or_else(|| anyhow!(attached_thread_missing_ref_advice(thread)))
}

fn attached_thread_missing_ref_advice(thread: &ThreadName) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "fast_forward_missing_attached_thread",
        format!("Attached thread '{thread}' has no ref before fast-forward"),
        "Inspect repository refs before retrying the operation.",
        format!("HEAD is attached to '{thread}', but that thread ref does not resolve"),
        "fast-forward cannot determine the pre-operation thread tip for undo/redo",
        "repository refs and worktree files were left unchanged",
        "heddle status",
        vec!["heddle status".to_string()],
    )
}

fn record_ff_advance_inner(
    repo: &Repository,
    source_thread: &str,
    head_before: &Head,
    pre_target_id: &ChangeId,
    post_target_id: &ChangeId,
    discard_local_changes: bool,
    materialized_baseline: Option<Option<ChangeId>>,
) -> Result<()> {
    if discard_local_changes {
        repo.fast_forward_attached_without_record_discard_local(post_target_id)?;
    } else if let Some(materialized_baseline) = materialized_baseline {
        repo.fast_forward_attached_from_materialized_state_without_record(
            post_target_id,
            materialized_baseline.as_ref(),
        )?;
    } else {
        repo.fast_forward_attached_without_record(post_target_id)?;
    }
    match head_before {
        Head::Attached { thread } => {
            repo.oplog().record_fast_forward(
                &ThreadName::new(source_thread),
                thread,
                pre_target_id,
                post_target_id,
                Some(&repo.op_scope()),
            )?;
        }
        Head::Detached { state } => {
            repo.oplog()
                .record_goto(post_target_id, Some(state), Some(&repo.op_scope()))?;
        }
    }
    Ok(())
}
