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
use objects::object::ChangeId;
use oplog::{OpBatch, OpEntry, OpRecord};
use refs::Head;
use repo::{
    CommitGraphIndex, Repository, ThreadFreshness, ThreadIntegrationPolicy, ThreadManager,
    ThreadState, refresh_thread_freshness,
};

use super::{advice::RecoveryAdvice, thread_cmd::thread_not_found_advice};
use crate::bridge::git_core::{open_repo as open_git_repo, set_reference};

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
                repo.refs().set_thread(thread, prev)?;
                repo.refs().write_head(&Head::Attached {
                    thread: thread.clone(),
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
        _ => {}
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
    repo.refs().set_thread(target_thread, pre_target_id)?;
    repo.refs().write_head(&Head::Attached {
        thread: target_thread.to_string(),
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
                repo.refs().set_thread(thread, new_state)?;
                repo.refs().write_head(&Head::Attached {
                    thread: thread.clone(),
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
            let source_tip = repo.refs().get_thread(source_thread)?.ok_or_else(|| {
                anyhow!(
                    "cannot redo fast-forward: source thread '{}' no longer exists \
                     (legacy V1 oplog record; re-run the merge or `heddle gc oplog` to prune)",
                    source_thread
                )
            })?;
            apply_ff_redo(repo, source_thread, target_thread, &source_tip)?;
        }
        _ => {}
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
    repo.refs().set_thread(target_thread, post_target_id)?;
    repo.refs().write_head(&Head::Attached {
        thread: target_thread.to_string(),
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

fn delete_thread_safely(repo: &Repository, name: &str) -> Result<()> {
    if let Head::Attached { thread } = repo.head_ref()?
        && thread == name
    {
        let state = repo
            .refs()
            .get_thread(name)?
            .ok_or_else(|| anyhow!(thread_not_found_advice(name, "delete thread")))?;
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
    let source_tip = repo.refs().get_thread(source_thread)?;
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
    let source_tip = repo.refs().get_thread(source_thread)?;
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
        let Some(source_tip) = repo.refs().get_thread(&thread.thread)? else {
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
