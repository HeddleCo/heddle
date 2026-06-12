// SPDX-License-Identifier: Apache-2.0
//! Rebase operation execution — applying commits onto a new base.

use objects::store::ObjectStore;
use std::fs;

use anyhow::{Result, anyhow};
use objects::object::{Blob, ChangeId, ContentHash, EntryType, State};
use oplog::OpRecord;
use refs::Head;
use repo::Repository;

use super::{
    super::{advice::RecoveryAdvice, ff_record::ff_advance_deferred},
    rebase_state::{load_rebase_state, save_rebase_state},
};
use crate::cli::{Cli, should_output_json};

/// Synthetic `source_thread` for `OpRecord::FastForward` entries
/// emitted during a rebase replay. Each replayed commit becomes its
/// own FF op (advancing the attached thread's tip), so listing them
/// under `<rebase>` keeps `heddle watch` / `heddle log` honest about
/// the operation's provenance without requiring the rebase target
/// thread name be threaded through the replay loop. Forensic-only —
/// neither undo nor redo reads it.
pub(crate) const REBASE_REPLAY_SOURCE: &str = "<rebase>";

/// Transaction-id prefix stamped on the `OpRecord::TransactionCommit`
/// envelope marker that closes a rebase batch (heddle#198). The full
/// id appends a nanosecond timestamp so distinct rebases in the same
/// oplog don't collide on the forensic id.
const REBASE_TRANSACTION_ID_PREFIX: &str = "rebase-";

pub(super) fn replay_commits(
    repo: &Repository,
    rebase_state_path: &std::path::Path,
    cli: &Cli,
    discard_local_changes: bool,
) -> Result<()> {
    replay_commits_internal(repo, rebase_state_path, Some(cli), discard_local_changes)
}

pub(super) fn replay_commits_silent(
    repo: &Repository,
    rebase_state_path: &std::path::Path,
) -> Result<()> {
    replay_commits_internal(repo, rebase_state_path, None, false)
}

fn replay_commits_internal(
    repo: &Repository,
    rebase_state_path: &std::path::Path,
    cli: Option<&Cli>,
    discard_local_changes: bool,
) -> Result<()> {
    let mut state = load_rebase_state(rebase_state_path)?;
    resume_manual_resolution_if_present(repo, &mut state, rebase_state_path, cli)?;

    let mut current_head = if state.current_index == 0 {
        state.onto
    } else {
        repo.current_state()?
            .ok_or_else(|| {
                anyhow!(RecoveryAdvice::rebase_referenced_state_missing(
                    "<current>",
                    "current state",
                ))
            })?
            .change_id
    };

    while state.current_index < state.commits_to_replay.len() {
        let commit_id = state.commits_to_replay[state.current_index];
        let commit_state = repo.store().get_state(&commit_id)?.ok_or_else(|| {
            anyhow!(RecoveryAdvice::rebase_referenced_state_missing(
                &commit_id.to_string(),
                "commit",
            ))
        })?;

        if let Some(cli) = cli
            && should_output_json(cli, Some(repo.config()))
        {
            println!(
                "{{\"status\": \"applying\", \"commit\": \"{}\"}}",
                commit_id.short()
            );
        } else if cli.is_some() {
            println!("Applying {}...", commit_id.short());
        }

        let result = apply_commit(repo, &commit_state, &current_head, discard_local_changes)?;

        match result {
            ApplyResult::Success { new_head, advance } => {
                current_head = new_head;
                state.current_index += 1;
                state.pending_manual_resolution = None;
                state.pre_conflict_head = None;
                state.pending_advances.push(advance);
                save_rebase_state(rebase_state_path, &state)?;
            }
            ApplyResult::Conflict => {
                state.pending_manual_resolution = Some(commit_id);
                state.pre_conflict_head = Some(current_head);
                save_rebase_state(rebase_state_path, &state)?;
                if let Some(cli) = cli
                    && should_output_json(cli, Some(repo.config()))
                {
                    println!(
                        "{{\"status\": \"conflict\", \"commit\": \"{}\"}}",
                        commit_id.short()
                    );
                } else if cli.is_some() {
                    println!(
                        "Conflict applying {}. Fix conflicts and run 'heddle rebase --continue'",
                        commit_id.short()
                    );
                }
                return Ok(());
            }
        }
    }

    // heddle#198: flush every per-commit FF record accumulated by the
    // replay loop into a single oplog batch so `heddle undo` rewinds
    // the whole rebase atomically. Without grouping, undo would need
    // N invocations for N replayed commits and an undo chain that
    // stopped mid-way would strand the thread tip on a synthetic
    // intermediate state. The `TransactionCommit` marker closing the
    // batch carries the op count for forensic clarity in `undo --list`
    // and `heddle log` — its inverse is a no-op (same as elsewhere in
    // `undo_apply.rs`), so it doesn't interfere with the reverse-walk
    // that undoes the FF entries.
    flush_rebase_batch(repo, &state.pending_advances, &state.transaction_id)?;

    fs::remove_file(rebase_state_path)?;

    if let Some(cli) = cli
        && should_output_json(cli, Some(repo.config()))
    {
        println!(
            "{{\"status\": \"completed\", \"onto\": \"{}\"}}",
            state.onto
        );
    } else if cli.is_some() {
        println!("Rebase completed");
    }

    Ok(())
}

/// Mint a fresh rebase-batch transaction id. Carries the
/// `REBASE_TRANSACTION_ID_PREFIX` so [`is_rebase_batch`] in `undo.rs`
/// recognises the batch envelope. The nanosecond segment stays for
/// forensic chronology (`heddle undo --list` ordering); the v4 UUID
/// suffix is what guarantees uniqueness — `chrono::Utc::now()` alone
/// can return the same nanos value across cores or on coarse-clock
/// hosts, and `flush_rebase_batch`'s dedup keys on this id verbatim
/// so a collision would silently drop the later rebase's batch
/// (heddle#198 r3 / Codex PR #218 P2).
pub(super) fn mint_rebase_transaction_id() -> String {
    format!(
        "{}{}-{}",
        REBASE_TRANSACTION_ID_PREFIX,
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
        uuid::Uuid::new_v4().simple()
    )
}

pub(super) fn flush_rebase_batch(
    repo: &Repository,
    advances: &[OpRecord],
    transaction_id: &str,
) -> Result<()> {
    if advances.is_empty() {
        return Ok(());
    }
    let mut batch: Vec<OpRecord> = advances.to_vec();
    batch.push(OpRecord::TransactionCommit {
        transaction_id: transaction_id.to_string(),
        op_count: advances.len() as u32,
    });
    // heddle#198 r4 (Codex PR #218 P2): the dedup check and the append
    // run under the same oplog write lock via
    // `record_batch_scoped_if_no_transaction`. Pre-r4 these were two
    // separate calls with no shared lock, so two concurrent
    // `rebase --continue` invocations with the same persisted
    // `transaction_id` (the crash-recovery retry window from r2) could
    // both observe "not committed" and both append, doubling the
    // rebase's undo history. 64 batches is more than enough headroom
    // for the realistic recovery window — immediate retry — and ageing
    // past it is acceptable because the worst-case outcome is a
    // duplicate batch the operator can collapse with a second
    // `heddle undo`.
    //
    // heddle#382 boundary: rebase still uses the older exact-once-windowed
    // append and is explicitly outside same-thread AtomicMutation isolation
    // until this flow is migrated to an AtomicMutation root.
    repo.oplog().record_batch_scoped_if_no_transaction(
        batch,
        Some(&repo.op_scope()),
        transaction_id,
        64,
    )?;
    Ok(())
}

fn resume_manual_resolution_if_present(
    repo: &Repository,
    state: &mut super::rebase_state::RebaseState,
    rebase_state_path: &std::path::Path,
    cli: Option<&Cli>,
) -> Result<()> {
    let Some(pending_commit) = state.pending_manual_resolution else {
        return Ok(());
    };
    let Some(pre_conflict_head) = state.pre_conflict_head else {
        return Ok(());
    };

    let current_state = repo.current_state()?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::rebase_referenced_state_missing(
            "<current>",
            "current state",
        ))
    })?;

    if current_state.change_id == pre_conflict_head || current_state.change_id == pending_commit {
        if let Some(cli) = cli
            && should_output_json(cli, Some(repo.config()))
        {
            println!(
                "{{\"status\": \"conflict\", \"commit\": \"{}\"}}",
                pending_commit.short()
            );
        } else if cli.is_some() {
            println!(
                "Conflict applying {}. Capture a manual resolution, then run 'heddle rebase --continue'",
                pending_commit.short()
            );
        }
        return Ok(());
    }

    // heddle#198 r2 (Codex PR #218 P1): the accepted manual-resolution
    // capture advanced the attached thread (or HEAD, when detached) from
    // `pre_conflict_head` to `current_state.change_id`, but unlike a
    // normal `apply_commit` arm there is no `ff_advance_deferred` call
    // to buffer the corresponding FF (or `Goto`) into the rebase batch.
    // Without folding it in here, the batch's last FF points at the
    // pre-conflict commit's rebased tip; `heddle redo` then replays
    // only the recorded FFs and lands one commit short of the actual
    // post-rebase tip. Append the implicit advance before bumping
    // `current_index` so the batch envelope reflects the full rebase
    // even when it paused for a manual fix-up.
    let resolution_advance = match repo.head_ref()? {
        Head::Attached { thread } => OpRecord::FastForward {
            source_thread: REBASE_REPLAY_SOURCE.to_string(),
            target_thread: thread.to_string(),
            pre_target_id: pre_conflict_head,
            post_target_id: current_state.change_id,
        },
        Head::Detached { .. } => OpRecord::Goto {
            target: current_state.change_id,
            prev_head: Some(pre_conflict_head),
            head: current_state.change_id,
        },
    };
    state.pending_advances.push(resolution_advance);
    state.current_index += 1;
    state.pending_manual_resolution = None;
    state.pre_conflict_head = None;
    save_rebase_state(rebase_state_path, state)?;

    if let Some(cli) = cli
        && should_output_json(cli, Some(repo.config()))
    {
        println!(
            "{{\"status\": \"manual-resolution-accepted\", \"commit\": \"{}\", \"resolved_as\": \"{}\"}}",
            pending_commit.short(),
            current_state.change_id.short()
        );
    } else if cli.is_some() {
        println!(
            "Accepted manual resolution for {} as {}",
            pending_commit.short(),
            current_state.change_id.short()
        );
    }

    Ok(())
}

enum ApplyResult {
    Success {
        new_head: ChangeId,
        /// The deferred FF (or detached-HEAD `Goto`) record that the
        /// caller will fold into the rebase's single oplog batch on
        /// completion. Generated by [`super::super::ff_record::ff_advance_deferred`]
        /// — the worktree/ref mutation has already happened by the
        /// time this is returned.
        advance: OpRecord,
    },
    Conflict,
}

fn apply_commit(
    repo: &Repository,
    commit_state: &objects::object::State,
    current_head: &ChangeId,
    discard_local_changes: bool,
) -> Result<ApplyResult> {
    let current_tree_hash = get_tree_for_state(repo, current_head)?;
    let commit_tree_hash = commit_state.tree;

    let current_tree = repo.store().get_tree(&current_tree_hash)?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::rebase_referenced_state_missing(
            &current_tree_hash.to_string(),
            "current tree",
        ))
    })?;

    let commit_tree = repo.store().get_tree(&commit_tree_hash)?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::rebase_referenced_state_missing(
            &commit_tree_hash.to_string(),
            "commit tree",
        ))
    })?;

    let parent_tree_hash = if let Some(parent_id) = commit_state.parents.first() {
        get_tree_for_state(repo, parent_id)?
    } else {
        return apply_tree_to_worktree(
            repo,
            commit_state,
            &commit_tree,
            current_head,
            discard_local_changes,
        );
    };

    let parent_tree = repo.store().get_tree(&parent_tree_hash)?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::rebase_referenced_state_missing(
            &parent_tree_hash.to_string(),
            "parent tree",
        ))
    })?;

    let changes = compute_tree_diff(&parent_tree, &commit_tree);

    let mut has_conflicts = false;
    let mut updated_entries: Vec<(String, objects::object::EntryType, ContentHash)> = Vec::new();
    let mut deleted_paths: Vec<String> = Vec::new();

    for (path, change) in changes {
        match change {
            TreeChange::Added(entry_type, hash) => {
                if let Some(existing) = find_entry_in_tree(&current_tree, &path) {
                    if existing.1 != hash {
                        has_conflicts = true;
                    }
                } else {
                    updated_entries.push((path, entry_type, hash));
                }
            }
            TreeChange::Modified(entry_type, hash) => {
                if let Some(existing) = find_entry_in_tree(&current_tree, &path)
                    && existing.1 == hash
                {
                    continue;
                }
                if let Some(parent) = find_entry_in_tree(&parent_tree, &path)
                    && let Some(existing) = find_entry_in_tree(&current_tree, &path)
                    && existing.1 != parent.1
                    && existing.1 != hash
                {
                    if let Some(merged_hash) =
                        try_auto_merge_textual_change(repo, &parent.1, &existing.1, &hash)?
                    {
                        updated_entries.push((path, EntryType::Blob, merged_hash));
                        continue;
                    }
                    if blob_contains_both(repo, &hash, &existing.1, &parent.1)? {
                        updated_entries.push((path, entry_type, hash));
                        continue;
                    }
                    has_conflicts = true;
                    continue;
                }
                updated_entries.push((path, entry_type, hash));
            }
            TreeChange::Deleted => {
                if let Some(existing) = find_entry_in_tree(&current_tree, &path) {
                    if let Some(parent) = find_entry_in_tree(&parent_tree, &path)
                        && existing.1 != parent.1
                    {
                        has_conflicts = true;
                        continue;
                    }
                    deleted_paths.push(path);
                }
            }
        }
    }

    if has_conflicts {
        return Ok(ApplyResult::Conflict);
    }

    let new_tree = apply_changes_to_tree(&current_tree, &updated_entries, &deleted_paths)?;
    let new_tree_hash = repo.store().put_tree(&new_tree)?;

    let new_state = State::new_refresh_of(
        new_tree_hash,
        vec![*current_head],
        commit_state.attribution.clone(),
        commit_state.logical_change_id(),
    )
    .with_intent(
        commit_state
            .intent
            .clone()
            .unwrap_or_else(|| "rebase".to_string()),
    )
    .with_status(commit_state.status);

    let mut new_state = copy_state_metadata(new_state, commit_state);

    let new_state_id = new_state.change_id;
    // Authored-state chokepoint (heddle#482): a rebase replay re-authors the
    // commit onto a new base (new tree + new parent) — a new author-created
    // state — so it is auto-signed rather than carrying the pre-rebase
    // signature forward.
    repo.put_authored_state(&mut new_state)?;
    let advance = ff_advance_deferred(
        repo,
        REBASE_REPLAY_SOURCE,
        &new_state_id,
        discard_local_changes,
    )?;

    Ok(ApplyResult::Success {
        new_head: new_state_id,
        advance,
    })
}

fn try_auto_merge_textual_change(
    repo: &Repository,
    base: &ContentHash,
    current: &ContentHash,
    incoming: &ContentHash,
) -> Result<Option<ContentHash>> {
    let Some(base_blob) = repo.store().get_blob(base)? else {
        return Ok(None);
    };
    let Some(current_blob) = repo.store().get_blob(current)? else {
        return Ok(None);
    };
    let Some(incoming_blob) = repo.store().get_blob(incoming)? else {
        return Ok(None);
    };

    let Some(merged) = auto_merge_text_lines(
        base_blob.content(),
        current_blob.content(),
        incoming_blob.content(),
    )?
    else {
        return Ok(None);
    };

    let hash = repo.store().put_blob(&Blob::new(merged))?;
    Ok(Some(hash))
}

/// Replay-time auto-merge of three line-level byte slices.
///
/// Returns `Some(bytes)` only when the merge resolves *cleanly* — any
/// conflict (including binary, delete/modify, or overlapping hunks) returns
/// `None`, leaving the rebase caller to either invoke `blob_contains_both`
/// or stop with `ApplyResult::Conflict`. Routes through the native
/// hunk-level merger added in heddle#79 — always available, no feature
/// gate — so multi-hunk-per-side edits auto-resolve when disjoint even on
/// `--no-default-features` builds, addressing the heddle#54 trip report's
/// rebase failure mode.
fn auto_merge_text_lines(base: &[u8], current: &[u8], incoming: &[u8]) -> Result<Option<Vec<u8>>> {
    use merge::{MergeOutcome, text_hunk_merge};
    match text_hunk_merge(base, current, incoming) {
        MergeOutcome::Clean(bytes) => Ok(Some(bytes)),
        MergeOutcome::Conflicts { .. } | MergeOutcome::Binary | MergeOutcome::DeleteVsModify => {
            Ok(None)
        }
    }
}

fn blob_contains_both(
    repo: &Repository,
    candidate: &ContentHash,
    current: &ContentHash,
    parent: &ContentHash,
) -> Result<bool> {
    let Some(candidate_blob) = repo.store().get_blob(candidate)? else {
        return Ok(false);
    };
    let Some(current_blob) = repo.store().get_blob(current)? else {
        return Ok(false);
    };
    let Some(parent_blob) = repo.store().get_blob(parent)? else {
        return Ok(false);
    };
    let candidate = candidate_blob.content();
    Ok(!current_blob.content().is_empty()
        && !parent_blob.content().is_empty()
        && contains_bytes(candidate, current_blob.content())
        && contains_bytes(candidate, parent_blob.content()))
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn apply_tree_to_worktree(
    repo: &Repository,
    commit_state: &objects::object::State,
    tree: &objects::object::Tree,
    current_head: &ChangeId,
    discard_local_changes: bool,
) -> Result<ApplyResult> {
    let tree_hash = repo.store().put_tree(tree)?;
    let new_state = State::new_refresh_of(
        tree_hash,
        vec![*current_head],
        commit_state.attribution.clone(),
        commit_state.logical_change_id(),
    )
    .with_intent(
        commit_state
            .intent
            .clone()
            .unwrap_or_else(|| "rebase".to_string()),
    )
    .with_status(commit_state.status);
    let mut new_state = copy_state_metadata(new_state, commit_state);

    let new_state_id = new_state.change_id;
    // Authored-state chokepoint (heddle#482): a rebase replay re-authors the
    // commit onto a new base (new tree + new parent) — a new author-created
    // state — so it is auto-signed rather than carrying the pre-rebase
    // signature forward.
    repo.put_authored_state(&mut new_state)?;
    let advance = ff_advance_deferred(
        repo,
        REBASE_REPLAY_SOURCE,
        &new_state_id,
        discard_local_changes,
    )?;

    Ok(ApplyResult::Success {
        new_head: new_state_id,
        advance,
    })
}

fn copy_state_metadata(
    mut rebased_state: objects::object::State,
    source_state: &objects::object::State,
) -> objects::object::State {
    if let Some(confidence) = source_state.confidence {
        rebased_state = rebased_state.with_confidence(confidence);
    }
    if let Some(verification) = source_state.verification.clone() {
        rebased_state = rebased_state.with_verification(verification);
    }
    if let Some(provenance) = source_state.provenance {
        rebased_state = rebased_state.with_provenance(provenance);
    }
    if let Some(context) = source_state.context {
        rebased_state = rebased_state.with_context(context);
    }
    rebased_state
}

fn get_tree_for_state(repo: &Repository, state_id: &ChangeId) -> Result<ContentHash> {
    let state = repo.store().get_state(state_id)?.ok_or_else(|| {
        anyhow!(RecoveryAdvice::rebase_referenced_state_missing(
            &state_id.to_string(),
            "state",
        ))
    })?;
    Ok(state.tree)
}

#[derive(Clone)]
enum TreeChange {
    Added(objects::object::EntryType, ContentHash),
    Modified(objects::object::EntryType, ContentHash),
    Deleted,
}

fn compute_tree_diff(
    old_tree: &objects::object::Tree,
    new_tree: &objects::object::Tree,
) -> Vec<(String, TreeChange)> {
    let mut changes = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in new_tree.entries() {
        seen.insert(entry.name.clone());
        let old_entry = old_tree.entries().iter().find(|e| e.name == entry.name);
        match old_entry {
            Some(old) => {
                if old.hash != entry.hash {
                    changes.push((
                        entry.name.clone(),
                        TreeChange::Modified(entry.entry_type, entry.hash),
                    ));
                }
            }
            None => {
                changes.push((
                    entry.name.clone(),
                    TreeChange::Added(entry.entry_type, entry.hash),
                ));
            }
        }
    }

    for entry in old_tree.entries() {
        if !seen.contains(&entry.name) {
            changes.push((entry.name.clone(), TreeChange::Deleted));
        }
    }

    changes
}

fn find_entry_in_tree(
    tree: &objects::object::Tree,
    name: &str,
) -> Option<(objects::object::EntryType, ContentHash)> {
    for entry in tree.entries() {
        if entry.name == name {
            return Some((entry.entry_type, entry.hash));
        }
    }
    None
}

fn apply_changes_to_tree(
    base_tree: &objects::object::Tree,
    updates: &[(String, objects::object::EntryType, ContentHash)],
    deletions: &[String],
) -> Result<objects::object::Tree> {
    let mut entries: Vec<objects::object::TreeEntry> = base_tree.entries().to_vec();

    for name in deletions {
        entries.retain(|e| &e.name != name);
    }

    for (name, entry_type, hash) in updates {
        entries.retain(|e| &e.name != name);
        let new_entry = match entry_type {
            objects::object::EntryType::Blob => {
                objects::object::TreeEntry::file(name.clone(), *hash, false)?
            }
            objects::object::EntryType::Tree => {
                objects::object::TreeEntry::directory(name.clone(), *hash)?
            }
            objects::object::EntryType::Symlink => {
                objects::object::TreeEntry::symlink(name.clone(), *hash)?
            }
        };
        entries.push(new_entry);
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(objects::object::Tree::from_entries(entries))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use objects::object::ThreadName;
    use refs::Head;
    use tempfile::TempDir;

    use super::*;

    /// `flush_rebase_batch(&[])` must short-circuit without writing
    /// anything to the oplog. The is_ancestor / empty-replay arms
    /// always pass at least one advance, but the replay loop can
    /// reach this with zero buffered records if it never entered the
    /// per-commit loop body (defensive — keeps the helper safe to
    /// call unconditionally at the loop tail).
    #[test]
    fn flush_rebase_batch_with_no_advances_is_a_noop() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();

        let before = repo
            .oplog()
            .recent_batches_scoped(10, Some(&repo.op_scope()))
            .unwrap()
            .len();

        flush_rebase_batch(&repo, &[], "rebase-noop-test").unwrap();

        let after = repo
            .oplog()
            .recent_batches_scoped(10, Some(&repo.op_scope()))
            .unwrap()
            .len();
        assert_eq!(before, after, "empty flush must not record a batch");
    }

    fn synthetic_goto_advance() -> OpRecord {
        let target = objects::object::ChangeId::generate();
        OpRecord::Goto {
            target,
            prev_head: Some(objects::object::ChangeId::generate()),
            head: target,
        }
    }

    fn rebase_replay_fixture() -> (TempDir, Repository, ChangeId) {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();

        fs::write(temp.path().join("base.txt"), "base\n").unwrap();
        let base = repo
            .snapshot(Some("base".to_string()), None)
            .unwrap()
            .change_id;

        fs::write(temp.path().join("feature.txt"), "feature\n").unwrap();
        let feature = repo
            .snapshot(Some("feature".to_string()), None)
            .unwrap()
            .change_id;

        repo.goto(&base).unwrap();
        fs::write(temp.path().join("main.txt"), "main\n").unwrap();
        let main = repo
            .snapshot(Some("main".to_string()), None)
            .unwrap()
            .change_id;

        repo.goto(&feature).unwrap();
        let feature_thread = ThreadName::new("feature");
        repo.refs().set_thread(&feature_thread, &feature).unwrap();
        repo.refs()
            .write_head(&Head::Attached {
                thread: feature_thread,
            })
            .unwrap();

        let rebase_state = super::super::rebase_state::RebaseState {
            onto: main,
            commits_to_replay: vec![feature],
            current_index: 0,
            original_head: feature,
            pending_manual_resolution: None,
            pre_conflict_head: None,
            pending_advances: Vec::new(),
            transaction_id: "rebase-dirty-routing-test".to_string(),
        };
        save_rebase_state(&repo.heddle_dir().join("REBASE_STATE"), &rebase_state).unwrap();

        (temp, repo, main)
    }

    #[test]
    fn replay_commits_refuses_dirty_worktree_without_discard_opt_in() {
        let (temp, repo, _main) = rebase_replay_fixture();
        let tracked = temp.path().join("feature.txt");
        fs::write(&tracked, "local edit\n").unwrap();

        let err =
            replay_commits_internal(&repo, &repo.heddle_dir().join("REBASE_STATE"), None, false)
                .unwrap_err();
        let msg = err.to_string();

        assert!(
            msg.contains("dirty worktree") && msg.contains("feature.txt"),
            "dirty replay should refuse and name the at-risk edit: {msg}"
        );
        assert_eq!(fs::read_to_string(&tracked).unwrap(), "local edit\n");
        assert!(
            repo.heddle_dir().join("REBASE_STATE").exists(),
            "failed replay must leave rebase state for retry or abort"
        );
    }

    #[test]
    fn replay_commits_with_discard_opt_in_overwrites_dirty_worktree() {
        let (temp, repo, _main) = rebase_replay_fixture();
        fs::write(temp.path().join("feature.txt"), "local edit\n").unwrap();
        fs::write(temp.path().join("scratch.txt"), "scratch\n").unwrap();

        replay_commits_internal(&repo, &repo.heddle_dir().join("REBASE_STATE"), None, true)
            .unwrap();

        assert!(!temp.path().join("scratch.txt").exists());
        assert_eq!(
            fs::read_to_string(temp.path().join("feature.txt")).unwrap(),
            "feature\n"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("main.txt")).unwrap(),
            "main\n"
        );
        assert!(
            !repo.heddle_dir().join("REBASE_STATE").exists(),
            "successful replay should remove rebase state"
        );
    }

    /// heddle#198 r2 (Codex PR #218 P2): `flush_rebase_batch` must be
    /// idempotent across re-invocations with the same `transaction_id`.
    /// The crash window between a successful flush and the
    /// `fs::remove_file(REBASE_STATE)` call leaves the state file on
    /// disk; the next `rebase --continue` reloads it, reaches the loop
    /// tail with `current_index == len`, and re-enters
    /// `flush_rebase_batch`. Without the dedup check it would append a
    /// second `TransactionCommit`-bracketed batch and the rebase would
    /// then need two `heddle undo` invocations to roll back instead of
    /// one — exactly the duplicated undo/redo history Codex flagged.
    #[test]
    fn flush_rebase_batch_skips_when_transaction_id_already_committed() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let txn_id = "rebase-idempotency-fixture";
        let advance = synthetic_goto_advance();

        let before = repo
            .oplog()
            .recent_batches_scoped(20, Some(&repo.op_scope()))
            .unwrap()
            .len();

        flush_rebase_batch(&repo, std::slice::from_ref(&advance), txn_id).unwrap();
        let after_first = repo
            .oplog()
            .recent_batches_scoped(20, Some(&repo.op_scope()))
            .unwrap()
            .len();
        assert_eq!(
            after_first,
            before + 1,
            "first flush must append exactly one batch"
        );

        flush_rebase_batch(&repo, std::slice::from_ref(&advance), txn_id).unwrap();
        let after_second = repo
            .oplog()
            .recent_batches_scoped(20, Some(&repo.op_scope()))
            .unwrap()
            .len();
        assert_eq!(
            after_second, after_first,
            "second flush with the same transaction_id must be a noop, not double the batch"
        );
    }

    /// heddle#198 r3 (Codex PR #218 P2): `mint_rebase_transaction_id`
    /// must produce a fresh id on every call. The dedup helper
    /// [`rebase_batch_already_committed`] keys on this id verbatim, so a
    /// collision between two rebases (rapid back-to-back, or concurrent
    /// invocations on a coarse-clock host) would silently drop the
    /// later rebase's batch — undo/redo history vanishes for that
    /// rebase. Pre-r3 the id was just a nanosecond timestamp; this test
    /// pins that uniqueness is now an unconditional guarantee, not a
    /// happens-to-work-on-this-clock side-effect.
    #[test]
    fn mint_rebase_transaction_id_is_unique_across_serial_calls() {
        use std::collections::HashSet;
        const N: usize = 1000;
        let mut seen: HashSet<String> = HashSet::with_capacity(N);
        for _ in 0..N {
            let id = mint_rebase_transaction_id();
            assert!(
                seen.insert(id.clone()),
                "duplicate mint id {id} after {} unique",
                seen.len()
            );
        }
    }

    /// Stronger variant: under thread contention each thread's
    /// `chrono::Utc::now()` reads can land in the same nanosecond bucket
    /// on different cores. Pre-r3 these would mint identical ids; with
    /// the UUID-v4 suffix even simultaneous reads diverge. Uses a
    /// barrier to maximise the chance of concurrent clock reads.
    #[test]
    fn mint_rebase_transaction_id_is_unique_under_thread_contention() {
        use std::collections::HashSet;
        use std::sync::{Arc, Barrier};
        use std::thread;

        const N_THREADS: usize = 32;
        const N_PER_THREAD: usize = 64;

        let barrier = Arc::new(Barrier::new(N_THREADS));
        let handles: Vec<_> = (0..N_THREADS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let mut local = Vec::with_capacity(N_PER_THREAD);
                    for _ in 0..N_PER_THREAD {
                        local.push(mint_rebase_transaction_id());
                    }
                    local
                })
            })
            .collect();

        let mut all = Vec::with_capacity(N_THREADS * N_PER_THREAD);
        for h in handles {
            all.extend(h.join().unwrap());
        }
        let unique: HashSet<&String> = all.iter().collect();
        assert_eq!(
            unique.len(),
            all.len(),
            "{}/{} mints collided across {} threads",
            all.len() - unique.len(),
            all.len(),
            N_THREADS,
        );
    }

    /// The id must keep the `rebase-` prefix so [`is_rebase_batch`] in
    /// `undo.rs` continues to recognise the batch envelope. Pins that
    /// adding the uniqueness suffix doesn't break the prefix tag.
    #[test]
    fn mint_rebase_transaction_id_keeps_rebase_prefix() {
        let id = mint_rebase_transaction_id();
        assert!(
            id.starts_with("rebase-"),
            "mint id {id} must keep the 'rebase-' prefix for is_rebase_batch"
        );
    }

    /// heddle#198 r4 (Codex PR #218 P2): `flush_rebase_batch` pre-r4
    /// called `recent_batches_scoped` (read) and `record_batch_scoped`
    /// (write) in two separate oplog calls with no shared lock. Two
    /// concurrent `rebase --continue` invocations with the same
    /// persisted `transaction_id` (the crash-recovery retry path —
    /// state file survives the `fs::remove_file` failure window and a
    /// second operator triggers continue while the first's retry is
    /// still in flight) both observe "not committed" and both append,
    /// reintroducing the duplicate-batch scenario from r2.
    ///
    /// This test maximises the race window with a `Barrier` so every
    /// thread enters the dedup check at the same instant; pre-r4 the
    /// assertion fails with N>1 added batches, post-r4 the check and
    /// append happen atomically under the oplog's existing write lock
    /// so exactly one batch lands regardless of contention.
    #[test]
    fn flush_rebase_batch_is_atomic_across_concurrent_continues() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const N_THREADS: usize = 8;

        let temp = TempDir::new().unwrap();
        let repo = Arc::new(Repository::init_default(temp.path()).unwrap());
        let txn_id = "rebase-concurrent-continue-fixture";

        let before = repo
            .oplog()
            .recent_batches_scoped(64, Some(&repo.op_scope()))
            .unwrap()
            .len();

        let barrier = Arc::new(Barrier::new(N_THREADS));
        let handles: Vec<_> = (0..N_THREADS)
            .map(|_| {
                let repo = Arc::clone(&repo);
                let barrier = Arc::clone(&barrier);
                let txn_id = txn_id.to_string();
                thread::spawn(move || {
                    let advance = synthetic_goto_advance();
                    barrier.wait();
                    flush_rebase_batch(&repo, std::slice::from_ref(&advance), &txn_id).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let after = repo
            .oplog()
            .recent_batches_scoped(64, Some(&repo.op_scope()))
            .unwrap()
            .len();
        let added = after - before;
        assert_eq!(
            added, 1,
            "concurrent flush_rebase_batch calls with the same transaction_id must produce exactly one batch — got {added}",
        );
    }

    /// Distinct transaction ids — separate rebases or a fresh mint —
    /// must each produce their own batch. Pins that the dedup check
    /// keys strictly on the supplied id and doesn't accidentally
    /// suppress legitimate back-to-back rebases.
    #[test]
    fn flush_rebase_batch_records_distinct_transaction_ids_separately() {
        let temp = TempDir::new().unwrap();
        let repo = Repository::init_default(temp.path()).unwrap();
        let advance = synthetic_goto_advance();

        let before = repo
            .oplog()
            .recent_batches_scoped(20, Some(&repo.op_scope()))
            .unwrap()
            .len();

        flush_rebase_batch(&repo, std::slice::from_ref(&advance), "rebase-id-A").unwrap();
        flush_rebase_batch(&repo, std::slice::from_ref(&advance), "rebase-id-B").unwrap();

        let after = repo
            .oplog()
            .recent_batches_scoped(20, Some(&repo.op_scope()))
            .unwrap()
            .len();
        assert_eq!(
            after,
            before + 2,
            "two distinct transaction ids must produce two batches"
        );
    }
}
