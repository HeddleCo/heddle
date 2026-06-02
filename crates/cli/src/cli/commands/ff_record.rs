// SPDX-License-Identifier: Apache-2.0
//! Shared helper for fast-forward call sites that need to record an
//! `OpRecord::FastForwardV2` instead of the implicit `OpRecord::Goto`.
//!
//! See heddle#99 (merge FF) and heddle#110 (rebase / ship /
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
/// — e.g. merge / rebase / ship. Callers that also materialize a
/// worktree must not publish the ref first: a dirty refusal must never
/// leave a ref advanced without the matching worktree materialization.
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

#[cfg(test)]
mod tests {
    use std::fs;

    use objects::object::ThreadName;
    use tempfile::TempDir;

    use super::*;

    fn create_repo() -> (TempDir, Repository) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        (temp, repo)
    }

    fn snapshot_file(
        repo: &Repository,
        root: &std::path::Path,
        name: &str,
        content: &str,
    ) -> ChangeId {
        fs::write(root.join(name), content).unwrap();
        repo.snapshot(Some(name.to_string()), None)
            .unwrap()
            .change_id
    }

    #[test]
    fn record_ff_advance_attached_records_fast_forward_v2() {
        let (temp, repo) = create_repo();
        let base = snapshot_file(&repo, temp.path(), "base.txt", "base\n");
        let target = snapshot_file(&repo, temp.path(), "target.txt", "target\n");

        repo.goto(&base).unwrap();
        let thread = ThreadName::new("main");
        repo.refs().set_thread(&thread, &base).unwrap();
        repo.refs()
            .write_head(&Head::Attached {
                thread: thread.clone(),
            })
            .unwrap();

        record_ff_advance(&repo, "source", &target).unwrap();

        assert_eq!(repo.refs().get_thread(&thread).unwrap(), Some(target));
        assert!(matches!(
            repo.refs().read_head().unwrap(),
            Head::Attached { thread: current } if current == thread
        ));
        let batches = repo
            .oplog()
            .recent_batches_scoped(4, Some(&repo.op_scope()))
            .unwrap();
        assert!(
            batches
                .iter()
                .flat_map(|batch| &batch.entries)
                .any(|entry| matches!(
                    &entry.operation,
                    oplog::OpRecord::FastForwardV2 {
                        source_thread,
                        target_thread,
                        pre_target_id,
                        post_target_id,
                    } if source_thread == "source"
                        && target_thread == "main"
                        && *pre_target_id == base
                        && *post_target_id == target
                ))
        );
    }

    #[test]
    fn record_ff_advance_discard_local_overwrites_dirty_checkout() {
        let (temp, repo) = create_repo();
        let tracked = temp.path().join("tracked.txt");
        let base = snapshot_file(&repo, temp.path(), "tracked.txt", "base\n");
        fs::write(&tracked, "target\n").unwrap();
        let target = repo
            .snapshot(Some("target".to_string()), None)
            .unwrap()
            .change_id;

        repo.goto(&base).unwrap();
        repo.refs()
            .set_thread(&ThreadName::new("main"), &base)
            .unwrap();
        repo.refs()
            .write_head(&Head::Attached {
                thread: ThreadName::new("main"),
            })
            .unwrap();
        fs::write(&tracked, "local edit\n").unwrap();

        record_ff_advance_discard_local(&repo, "source", &target).unwrap();

        assert_eq!(fs::read_to_string(&tracked).unwrap(), "target\n");
        assert_eq!(
            repo.refs().get_thread(&ThreadName::new("main")).unwrap(),
            Some(target)
        );
    }

    #[test]
    fn ff_advance_deferred_detached_returns_goto_record() {
        let (temp, repo) = create_repo();
        let base = snapshot_file(&repo, temp.path(), "base.txt", "base\n");
        let target = snapshot_file(&repo, temp.path(), "target.txt", "target\n");
        repo.goto(&base).unwrap();

        let advance = ff_advance_deferred(&repo, "source", &target, false).unwrap();

        assert!(matches!(
            advance,
            oplog::OpRecord::Goto {
                target: recorded_target,
                prev_head: Some(prev),
                head
            } if recorded_target == target && prev == base && head == target
        ));
        assert!(matches!(
            repo.refs().read_head().unwrap(),
            Head::Detached { state } if state == target
        ));
    }

    #[test]
    fn record_ff_advance_refuses_attached_head_without_thread_ref() {
        let (temp, repo) = create_repo();
        let target = snapshot_file(&repo, temp.path(), "target.txt", "target\n");
        repo.refs()
            .write_head(&Head::Attached {
                thread: ThreadName::new("missing"),
            })
            .unwrap();

        let err = record_ff_advance(&repo, "source", &target).unwrap_err();
        let msg = err.to_string();

        assert!(
            msg.contains("fast_forward_missing_attached_thread")
                || msg.contains("Attached thread 'missing' has no ref"),
            "missing attached thread should produce typed recovery advice: {msg}"
        );
        assert!(matches!(
            repo.refs().read_head().unwrap(),
            Head::Attached { thread } if thread == "missing"
        ));
    }
}
