// SPDX-License-Identifier: Apache-2.0
//! Rebase state persistence and commit-graph traversal.

use objects::store::ObjectStore;
use std::{fs, io::Write};

use anyhow::{anyhow, Result};
use objects::object::ChangeId;
use oplog::OpRecord;
use repo::Repository;

use super::super::advice::RecoveryAdvice;

#[derive(Debug, Clone)]
pub(crate) struct RebaseState {
    pub(crate) onto: ChangeId,
    pub(crate) commits_to_replay: Vec<ChangeId>,
    pub(crate) current_index: usize,
    pub(crate) original_head: ChangeId,
    pub(crate) pending_manual_resolution: Option<ChangeId>,
    pub(crate) pre_conflict_head: Option<ChangeId>,
    /// FastForward (or, on detached HEAD, Goto) records buffered from
    /// the per-commit replay loop. Flushed as a single oplog batch
    /// when the rebase completes so `heddle undo` rewinds the whole
    /// transaction atomically (heddle#198). Persisted across
    /// `--continue` invocations so a conflict pause doesn't drop the
    /// in-flight records.
    pub(crate) pending_advances: Vec<OpRecord>,
    /// Stable id for the rebase batch's `TransactionCommit` envelope.
    /// Persisted at rebase start so that a crash between
    /// `flush_rebase_batch` and `fs::remove_file(REBASE_STATE)` can
    /// retry the flush with the same id and the helper's oplog-dedup
    /// check (heddle#198 r2 / Codex PR #218 P2) recognises the prior
    /// commit and skips, instead of doubling the rebase's undo
    /// history.
    pub(crate) transaction_id: String,
}

pub(crate) fn collect_commits_to_rebase(
    repo: &Repository,
    current_head: &ChangeId,
    onto: &ChangeId,
) -> Result<Vec<ChangeId>> {
    let mut commits = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut current = *current_head;

    while visited.insert(current) {
        if current == *onto {
            break;
        }

        if is_ancestor_of(repo, &current, onto)? {
            break;
        }

        commits.push(current);

        let state = match repo.store().get_state(&current)? {
            Some(s) => s,
            None => break,
        };

        match state.parents.first() {
            Some(parent) => current = *parent,
            None => break,
        }
    }

    commits.reverse();
    Ok(commits)
}

pub(crate) fn is_ancestor_of(
    repo: &Repository,
    potential_ancestor: &ChangeId,
    descendant: &ChangeId,
) -> Result<bool> {
    Ok(proto::is_ancestor(
        repo.store(),
        *potential_ancestor,
        *descendant,
    )?)
}

pub(crate) fn save_rebase_state(path: &std::path::Path, state: &RebaseState) -> Result<()> {
    let mut content = String::new();
    content.push_str(&format!("onto={}\n", state.onto.to_string_full()));
    content.push_str(&format!(
        "original_head={}\n",
        state.original_head.to_string_full()
    ));
    content.push_str(&format!("transaction_id={}\n", state.transaction_id));
    content.push_str(&format!("current_index={}\n", state.current_index));
    if let Some(commit) = state.pending_manual_resolution {
        content.push_str(&format!(
            "pending_manual_resolution={}\n",
            commit.to_string_full()
        ));
    }
    if let Some(head) = state.pre_conflict_head {
        content.push_str(&format!("pre_conflict_head={}\n", head.to_string_full()));
    }
    content.push_str("commits=");
    for (i, commit) in state.commits_to_replay.iter().enumerate() {
        if i > 0 {
            content.push(',');
        }
        content.push_str(&commit.to_string_full());
    }
    content.push('\n');
    // Each pending oplog record is rmp-serde encoded then hex-escaped
    // so it round-trips through the existing line-based REBASE_STATE
    // file without disturbing the key=value shape. Order matters —
    // these get re-emitted into the oplog in the same order at the
    // end of the rebase.
    for advance in &state.pending_advances {
        let bytes = rmp_serde::to_vec(advance).map_err(|e| {
            anyhow!(RecoveryAdvice::rebase_state_corrupted(
                "encode pending_advance",
                e,
            ))
        })?;
        content.push_str(&format!("pending_advance={}\n", hex::encode(&bytes)));
    }

    let mut file = fs::File::create(path)?;
    file.write_all(content.as_bytes())?;

    Ok(())
}

pub(crate) fn load_rebase_state(path: &std::path::Path) -> Result<RebaseState> {
    load_rebase_state_internal(path, /* for_abort= */ false)
}

/// Lenient loader for the `rebase --abort` path. Forgives two
/// otherwise-fatal forms of partial-write damage that abort doesn't
/// actually care about:
///
/// * malformed `pending_advance=` records — skipped silently; abort
///   discards the buffered FF history when it rewinds to
///   `original_head`.
/// * a missing `transaction_id=` line — defaulted to an empty string;
///   the id only matters for `flush_rebase_batch`'s crash-recovery
///   dedup, which abort never reaches.
///
/// The strict [`load_rebase_state`] is still used by `--continue`,
/// which has to flush the full batch and so cannot afford a
/// silently-truncated vector or a blank transaction id (heddle#198
/// r2 P2 + r3 P1 / Codex PR #218: pre-fix, a single corrupted line
/// or a short crash window before the `transaction_id=` line was
/// written stranded the operator with neither abort nor continue
/// available).
pub(crate) fn load_rebase_state_for_abort(path: &std::path::Path) -> Result<RebaseState> {
    load_rebase_state_internal(path, /* for_abort= */ true)
}

fn load_rebase_state_internal(path: &std::path::Path, for_abort: bool) -> Result<RebaseState> {
    let content = fs::read_to_string(path)?;

    let mut onto = None;
    let mut original_head = None;
    let mut transaction_id = None;
    let mut current_index = 0;
    let mut commits_to_replay = Vec::new();
    let mut pending_manual_resolution = None;
    let mut pre_conflict_head = None;
    let mut pending_advances = Vec::new();

    for line in content.lines() {
        if let Some(value) = line.strip_prefix("onto=") {
            onto = Some(ChangeId::parse(value)?);
        } else if let Some(value) = line.strip_prefix("original_head=") {
            original_head = Some(ChangeId::parse(value)?);
        } else if let Some(value) = line.strip_prefix("transaction_id=") {
            transaction_id = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("current_index=") {
            current_index = value.parse().unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("pending_manual_resolution=") {
            pending_manual_resolution = Some(ChangeId::parse(value)?);
        } else if let Some(value) = line.strip_prefix("pre_conflict_head=") {
            pre_conflict_head = Some(ChangeId::parse(value)?);
        } else if let Some(value) = line.strip_prefix("commits=") {
            for commit_str in value.split(',') {
                if !commit_str.is_empty() {
                    commits_to_replay.push(ChangeId::parse(commit_str)?);
                }
            }
        } else if let Some(value) = line.strip_prefix("pending_advance=") {
            // The two-stage decode (hex then msgpack) can fail on a
            // truncated mid-write or a hand-edit. On the abort path
            // (heddle#198 r2 / Codex PR #218 P2) skip silently — abort
            // only needs `original_head` to rewind; the buffered FF
            // history is discarded. The continue path keeps the
            // hard-fail so a silently-truncated batch never lands in
            // the oplog.
            match hex::decode(value).map_err(|e| {
                anyhow!(RecoveryAdvice::rebase_state_corrupted(
                    "decode pending_advance",
                    e,
                ))
            }) {
                Err(_) if for_abort => continue,
                Err(e) => return Err(e),
                Ok(bytes) => match rmp_serde::from_slice::<OpRecord>(&bytes).map_err(|e| {
                    anyhow!(RecoveryAdvice::rebase_state_corrupted(
                        "decode pending_advance OpRecord",
                        e,
                    ))
                }) {
                    Err(_) if for_abort => continue,
                    Err(e) => return Err(e),
                    Ok(advance) => match advance {
                        // heddle#198 r4 (Codex PR #218 P2): only the
                        // FF-advance variants (FFV2, Goto) plus the
                        // legacy V1 FastForward read-back path can
                        // legitimately appear in a rebase batch.
                        // Anything else — MarkerCreate, ThreadX,
                        // Snapshot — would land in the committed batch
                        // verbatim and pollute undo/redo with records
                        // the rebase never produced. Strict loader
                        // rejects; abort silently drops them since the
                        // buffered FF history is discarded on rewind.
                        OpRecord::FastForward { .. }
                        | OpRecord::FastForwardV2 { .. }
                        | OpRecord::Goto { .. } => pending_advances.push(advance),
                        // Anything else would land in the committed batch
                        // verbatim and pollute undo/redo with records the
                        // rebase never produced. Abort silently drops them (the
                        // buffered FF history is discarded on rewind); the
                        // strict loader rejects. Enumerated explicitly (no
                        // wildcard) so a new FF-style variant must be decided
                        // here rather than silently rejected (heddle#354 r9).
                        OpRecord::Snapshot { .. }
                        | OpRecord::ThreadCreate { .. }
                        | OpRecord::ThreadCreateV2 { .. }
                        | OpRecord::ThreadDelete { .. }
                        | OpRecord::ThreadUpdate { .. }
                        | OpRecord::Fork { .. }
                        | OpRecord::Collapse { .. }
                        | OpRecord::MarkerCreate { .. }
                        | OpRecord::MarkerDelete { .. }
                        | OpRecord::Checkpoint { .. }
                        | OpRecord::TransactionAbort { .. }
                        | OpRecord::EphemeralThreadCollapse { .. }
                        | OpRecord::ConflictResolved { .. }
                        | OpRecord::TransactionCommit { .. }
                        | OpRecord::Redact { .. }
                        | OpRecord::Purge { .. }
                        | OpRecord::GitCheckpoint { .. }
                        | OpRecord::RemoteThreadUpdate { .. }
                        | OpRecord::RemoteThreadDelete { .. }
                        | OpRecord::UndoRecoveryUpdate { .. }
                        | OpRecord::StateVisibilitySet { .. }
                        | OpRecord::StateVisibilityPromote { .. } => {
                            if for_abort {
                                continue;
                            }
                            return Err(anyhow!(RecoveryAdvice::rebase_state_corrupted(
                                "Unexpected pending_advance variant in rebase state",
                                format!(
                                    "{} — only FastForward/FastForwardV2/Goto are accepted",
                                    advance.description()
                                ),
                            )));
                        }
                    },
                },
            }
        }
    }

    let onto = onto.ok_or_else(|| {
        anyhow!(RecoveryAdvice::rebase_state_corrupted(
            "Missing 'onto' in rebase state",
            "",
        ))
    })?;
    let original_head = original_head.ok_or_else(|| {
        anyhow!(RecoveryAdvice::rebase_state_corrupted(
            "Missing 'original_head' in rebase state",
            "",
        ))
    })?;
    // heddle#198 r3 (Codex PR #218 P1): on the abort path a missing
    // `transaction_id=` collapses to an empty string — the abort
    // rewind never reaches `flush_rebase_batch` so the id's
    // crash-recovery dedup is dead code. The strict loader still
    // demands the line because `--continue` does flush, and a blank
    // id would key the dedup against every prior empty-id batch.
    // heddle#198 r4 (Codex PR #218 P2): a torn write can leave the
    // `transaction_id=` line emitted with an empty (or whitespace-only)
    // value; `flush_rebase_batch`'s dedup keys verbatim, so a blank id
    // collides with every prior empty-id batch and silently suppresses
    // the new one. Strict loader rejects blank values (the missing-key
    // arm still rejects absence). Abort tolerates either shape — it
    // never reaches the dedup helper.
    let transaction_id = match (transaction_id, for_abort) {
        (Some(id), false) if id.trim().is_empty() => {
            return Err(anyhow!(RecoveryAdvice::rebase_state_corrupted(
                "Blank 'transaction_id' in rebase state",
                "",
            )));
        }
        (Some(id), _) => id,
        (None, true) => String::new(),
        (None, false) => {
            return Err(anyhow!(RecoveryAdvice::rebase_state_corrupted(
                "Missing 'transaction_id' in rebase state",
                "",
            )));
        }
    };

    // heddle#198 r4 (Codex PR #218 P1): the persisted invariant is
    // `pending_advances.len() == current_index` — `replay_commits_internal`
    // and `resume_manual_resolution_if_present` always bump both in
    // lockstep before the save. A crash-truncated REBASE_STATE that keeps
    // `current_index=` but drops trailing `pending_advance=` lines breaks
    // it; strict `--continue` must hard-fail rather than silently flush
    // an incomplete batch. Abort tolerates because it discards the
    // buffered FF history when rewinding to `original_head`.
    if !for_abort && pending_advances.len() != current_index {
        return Err(anyhow!(RecoveryAdvice::rebase_state_corrupted(
            "Inconsistent rebase state",
            format!(
                "pending_advance lines ({}) do not match current_index ({})",
                pending_advances.len(),
                current_index,
            ),
        )));
    }

    Ok(RebaseState {
        onto,
        original_head,
        transaction_id,
        current_index,
        commits_to_replay,
        pending_manual_resolution,
        pre_conflict_head,
        pending_advances,
    })
}

#[cfg(test)]
mod tests {
    use objects::object::ChangeId;
    use oplog::OpRecord;
    use tempfile::TempDir;

    use super::*;

    fn sample_state(pending: Vec<OpRecord>) -> RebaseState {
        // The persisted invariant (heddle#198 r4 / Codex PR #218 P1) is
        // `pending_advances.len() == current_index`; derive `current_index`
        // from the supplied vec so every fixture round-trips cleanly
        // through the strict loader.
        let current_index = pending.len();
        let commits_to_replay = (0..(current_index + 1))
            .map(|_| ChangeId::generate())
            .collect();
        RebaseState {
            onto: ChangeId::generate(),
            commits_to_replay,
            current_index,
            original_head: ChangeId::generate(),
            pending_manual_resolution: Some(ChangeId::generate()),
            pre_conflict_head: Some(ChangeId::generate()),
            pending_advances: pending,
            transaction_id: "rebase-test-sample".to_string(),
        }
    }

    fn ff_record() -> OpRecord {
        OpRecord::FastForwardV2 {
            source_thread: "<rebase>".to_string(),
            target_thread: "main".to_string(),
            pre_target_id: ChangeId::generate(),
            post_target_id: ChangeId::generate(),
        }
    }

    /// Round-trip cover: the `pending_advances` vec must survive a
    /// save+load through the line-based REBASE_STATE file. This is the
    /// load-bearing guarantee for `heddle rebase --continue` after a
    /// conflict pause — the buffered FFs from before the pause need
    /// to come back byte-identical so the eventual `flush_rebase_batch`
    /// emits the same oplog batch as a no-pause rebase would have.
    #[test]
    fn save_then_load_round_trips_pending_advances() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let advances = vec![ff_record(), ff_record(), ff_record()];
        let original = sample_state(advances.clone());

        save_rebase_state(&path, &original).unwrap();
        let loaded = load_rebase_state(&path).unwrap();

        assert_eq!(loaded.onto, original.onto);
        assert_eq!(loaded.original_head, original.original_head);
        assert_eq!(loaded.current_index, original.current_index);
        assert_eq!(loaded.commits_to_replay, original.commits_to_replay);
        assert_eq!(
            loaded.pending_manual_resolution,
            original.pending_manual_resolution
        );
        assert_eq!(loaded.pre_conflict_head, original.pre_conflict_head);
        assert_eq!(loaded.pending_advances.len(), 3);
        for (got, want) in loaded.pending_advances.iter().zip(advances.iter()) {
            // OpRecord doesn't implement PartialEq across all variants
            // we care about — compare via the canonical serialization.
            let got_bytes = rmp_serde::to_vec(got).unwrap();
            let want_bytes = rmp_serde::to_vec(want).unwrap();
            assert_eq!(got_bytes, want_bytes);
        }
    }

    /// Even with no buffered FFs (the conflict-on-first-commit case),
    /// the round-trip must work — the file simply contains no
    /// `pending_advance=` lines and load returns an empty vec.
    #[test]
    fn save_then_load_round_trips_empty_pending_advances() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let original = sample_state(Vec::new());

        save_rebase_state(&path, &original).unwrap();
        let loaded = load_rebase_state(&path).unwrap();
        assert!(loaded.pending_advances.is_empty());
    }

    /// A corrupt `pending_advance=` line (non-hex) must surface as a
    /// clear `decode pending_advance` error rather than a panic — the
    /// REBASE_STATE file is operator-visible and could be hand-edited.
    #[test]
    fn load_rejects_invalid_hex_pending_advance() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let body = format!(
            "onto={onto}\noriginal_head={oh}\ntransaction_id=rebase-test\ncurrent_index=0\ncommits=\npending_advance=not-hex!!\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
        );
        std::fs::write(&path, body).unwrap();

        let err = load_rebase_state(&path).unwrap_err().to_string();
        assert!(
            err.contains("decode pending_advance"),
            "expected hex-decode error to surface, got: {err}"
        );
    }

    /// Hex-valid but msgpack-garbage `pending_advance=` must surface as
    /// the OpRecord-decode error arm (distinct from the hex arm). Keeps
    /// the two failure modes diagnosable.
    #[test]
    fn load_rejects_invalid_msgpack_pending_advance() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let body = format!(
            "onto={onto}\noriginal_head={oh}\ntransaction_id=rebase-test\ncurrent_index=0\ncommits=\npending_advance=deadbeef\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
        );
        std::fs::write(&path, body).unwrap();

        let err = load_rebase_state(&path).unwrap_err().to_string();
        assert!(
            err.contains("decode pending_advance OpRecord"),
            "expected rmp-decode error to surface, got: {err}"
        );
    }

    /// A garbage `current_index=` must not refuse the whole file —
    /// `unwrap_or(0)` falls back so a continue can still attempt to
    /// resume from the start rather than stranding the operator with
    /// an unrecoverable state file.
    #[test]
    fn load_falls_back_to_index_zero_on_garbage_current_index() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let body = format!(
            "onto={onto}\noriginal_head={oh}\ntransaction_id=rebase-test\ncurrent_index=not-a-number\ncommits=\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
        );
        std::fs::write(&path, body).unwrap();

        let loaded = load_rebase_state(&path).unwrap();
        assert_eq!(loaded.current_index, 0);
    }

    /// Missing `onto=` and `original_head=` are not recoverable —
    /// load must reject them with a clear "Missing 'X'" message so
    /// operators don't end up resuming against an empty rebase target.
    #[test]
    fn load_errors_when_onto_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        std::fs::write(
            &path,
            format!(
                "original_head={oh}\ncurrent_index=0\ncommits=\n",
                oh = ChangeId::generate().to_string_full()
            ),
        )
        .unwrap();
        let err = load_rebase_state(&path).unwrap_err().to_string();
        assert!(err.contains("Missing 'onto'"), "got: {err}");
    }

    #[test]
    fn load_errors_when_original_head_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        std::fs::write(
            &path,
            format!(
                "onto={onto}\ncurrent_index=0\ncommits=\n",
                onto = ChangeId::generate().to_string_full()
            ),
        )
        .unwrap();
        let err = load_rebase_state(&path).unwrap_err().to_string();
        assert!(err.contains("Missing 'original_head'"), "got: {err}");
    }

    /// heddle#198 r2 (Codex PR #218 P2): `load_rebase_state_for_abort`
    /// must skip past a non-hex `pending_advance=` line and return a
    /// usable state — abort only needs `original_head` to rewind, so a
    /// crash mid-write to REBASE_STATE shouldn't lock the operator out
    /// of both abort and continue. The strict loader still rejects the
    /// same file (covered by `load_rejects_invalid_hex_pending_advance`
    /// above), so the continue path remains unambiguous.
    #[test]
    fn load_for_abort_skips_invalid_hex_pending_advance() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let body = format!(
            "onto={onto}\noriginal_head={oh}\ntransaction_id=rebase-test\ncurrent_index=0\ncommits=\npending_advance=not-hex!!\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
        );
        std::fs::write(&path, body).unwrap();

        let loaded = load_rebase_state_for_abort(&path)
            .expect("abort loader must tolerate a malformed pending_advance line");
        assert!(
            loaded.pending_advances.is_empty(),
            "malformed entries must be dropped from the abort-loaded vec"
        );
    }

    /// Same contract for the OpRecord-decode arm: a hex-valid but
    /// msgpack-garbage entry must be skipped on the abort path. Covers
    /// the second failure mode of pending_advance — the strict loader
    /// surfaces this as a distinct `decode pending_advance OpRecord`
    /// error, which would otherwise block the abort.
    #[test]
    fn load_for_abort_skips_invalid_msgpack_pending_advance() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let body = format!(
            "onto={onto}\noriginal_head={oh}\ntransaction_id=rebase-test\ncurrent_index=0\ncommits=\npending_advance=deadbeef\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
        );
        std::fs::write(&path, body).unwrap();

        let loaded = load_rebase_state_for_abort(&path)
            .expect("abort loader must tolerate a msgpack-garbage pending_advance line");
        assert!(loaded.pending_advances.is_empty());
    }

    /// Even on the abort path, missing `onto=` / `original_head=` are
    /// not recoverable — without `original_head` there's nothing to
    /// rewind to. Pins that leniency is narrowly scoped to the
    /// pending-advance vec.
    #[test]
    fn load_for_abort_still_requires_original_head() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        std::fs::write(
            &path,
            format!(
                "onto={onto}\ntransaction_id=rebase-test\ncurrent_index=0\ncommits=\n",
                onto = ChangeId::generate().to_string_full()
            ),
        )
        .unwrap();
        let err = load_rebase_state_for_abort(&path).unwrap_err().to_string();
        assert!(err.contains("Missing 'original_head'"), "got: {err}");
    }

    /// heddle#198 r3 (Codex PR #218 P1): `save_rebase_state` truncates
    /// and rewrites REBASE_STATE on every advance, so a crash mid-write
    /// can leave the file with the first couple lines (`onto=`,
    /// `original_head=`) intact but the `transaction_id=` line (and
    /// everything after) gone. Abort only needs `original_head` to
    /// rewind, so refusing it for a missing `transaction_id` strands the
    /// operator with neither `--abort` nor `--continue` available. The
    /// lenient loader must accept the partial file and produce a usable
    /// state for the rewind.
    #[test]
    fn load_for_abort_tolerates_missing_transaction_id() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let body = format!(
            "onto={onto}\noriginal_head={oh}\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
        );
        std::fs::write(&path, body).unwrap();

        let loaded = load_rebase_state_for_abort(&path)
            .expect("abort loader must tolerate a missing transaction_id");
        assert!(loaded.pending_advances.is_empty());
        assert!(loaded.commits_to_replay.is_empty());
    }

    /// Companion to the above: a partial write that also dropped its
    /// `transaction_id=` line AND has a torn-off `pending_advance=`
    /// record (the realistic crash shape — `pending_advance=` lines are
    /// appended after `transaction_id=` in `save_rebase_state`) must
    /// still load through the abort path. Pins the worst-case partial
    /// file an operator can encounter.
    #[test]
    fn load_for_abort_tolerates_missing_transaction_id_with_garbage_tail() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let body = format!(
            "onto={onto}\noriginal_head={oh}\npending_advance=not-hex!!\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
        );
        std::fs::write(&path, body).unwrap();

        let loaded = load_rebase_state_for_abort(&path)
            .expect("abort loader must tolerate a missing transaction_id + torn tail");
        assert!(loaded.pending_advances.is_empty());
    }

    /// The strict `load_rebase_state` (used by `--continue`) must still
    /// reject a missing `transaction_id=` — the dedup check in
    /// `flush_rebase_batch` keys on this id, and a continue without it
    /// would produce a batch the crash-recovery scan can't recognise.
    /// Pins that the r3 leniency is narrowly scoped to the abort loader.
    #[test]
    fn load_strict_still_requires_transaction_id() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        std::fs::write(
            &path,
            format!(
                "onto={onto}\noriginal_head={oh}\ncurrent_index=0\ncommits=\n",
                onto = ChangeId::generate().to_string_full(),
                oh = ChangeId::generate().to_string_full(),
            ),
        )
        .unwrap();
        let err = load_rebase_state(&path).unwrap_err().to_string();
        assert!(err.contains("Missing 'transaction_id'"), "got: {err}");
    }

    /// heddle#198 r4 (Codex PR #218 P2): `pending_advance=` records
    /// deserialize as arbitrary `OpRecord` and get pushed without any
    /// variant check, then written verbatim into the committed batch.
    /// A hand-edited or partially-corrupted state file that still
    /// decodes can inject non-rebase operations (e.g. `MarkerCreate`,
    /// `ThreadDelete`) into the rebase's undo/redo history — undo would
    /// replay records the rebase never produced. The strict loader
    /// must whitelist only the FF-advance variants (`FastForwardV2`
    /// / `Goto`) plus the legacy V1 `FastForward` read-back path,
    /// and reject anything else.
    #[test]
    fn load_strict_rejects_non_advance_pending_record() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        // MarkerCreate is a real OpRecord variant that decodes cleanly
        // but is never legitimately emitted into a rebase batch.
        let bad = OpRecord::MarkerCreate {
            name: "junk".to_string(),
            state: ChangeId::generate(),
        };
        let bytes = rmp_serde::to_vec(&bad).unwrap();
        let body = format!(
            "onto={onto}\noriginal_head={oh}\ntransaction_id=rebase-test\ncurrent_index=1\ncommits=\npending_advance={pa}\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
            pa = hex::encode(&bytes),
        );
        std::fs::write(&path, body).unwrap();

        let err = load_rebase_state(&path).unwrap_err().to_string();
        assert!(
            err.contains("pending_advance") && err.contains("variant"),
            "expected variant-rejection error, got: {err}"
        );
    }

    /// Multiple non-advance records must each be flagged. Pins that the
    /// reject arm fires on the first bad record rather than silently
    /// accepting later ones.
    #[test]
    fn load_strict_rejects_thread_delete_in_pending_advances() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let bad = OpRecord::ThreadDelete {
            name: "junk".to_string(),
            state: ChangeId::generate(),
        };
        let bytes = rmp_serde::to_vec(&bad).unwrap();
        let body = format!(
            "onto={onto}\noriginal_head={oh}\ntransaction_id=rebase-test\ncurrent_index=1\ncommits=\npending_advance={pa}\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
            pa = hex::encode(&bytes),
        );
        std::fs::write(&path, body).unwrap();

        let err = load_rebase_state(&path).unwrap_err().to_string();
        assert!(
            err.contains("pending_advance") && err.contains("variant"),
            "expected variant-rejection error, got: {err}"
        );
    }

    /// heddle#198 r4 (Codex PR #218 P2): the strict loader checks only
    /// key presence of `transaction_id=`, not the value. A torn write
    /// can leave the line emitted with an empty value
    /// (`transaction_id=\n`); `flush_rebase_batch`'s dedup keys on the
    /// supplied id verbatim, so an empty id collides against any prior
    /// rebase whose state file was similarly truncated and silently
    /// suppresses the new batch. Pin that strict load rejects the
    /// blank-value form too — the existing "Missing" message only
    /// fires when the line is absent entirely.
    #[test]
    fn load_strict_rejects_blank_transaction_id() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let body = format!(
            "onto={onto}\noriginal_head={oh}\ntransaction_id=\ncurrent_index=0\ncommits=\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
        );
        std::fs::write(&path, body).unwrap();

        let err = load_rebase_state(&path).unwrap_err().to_string();
        assert!(
            err.contains("Blank 'transaction_id'"),
            "expected blank-transaction_id error, got: {err}"
        );
    }

    /// Whitespace-only is the same hazard as the empty form: `trim()`
    /// would have made the value usable for the dedup key but Codex's
    /// concern is that any non-content id keys collisions across
    /// otherwise-unrelated corrupted rebases. Pin that the strict
    /// loader rejects whitespace too — there's no legitimate way for
    /// `mint_rebase_transaction_id` to emit one.
    #[test]
    fn load_strict_rejects_whitespace_only_transaction_id() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let body = format!(
            "onto={onto}\noriginal_head={oh}\ntransaction_id=   \t\ncurrent_index=0\ncommits=\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
        );
        std::fs::write(&path, body).unwrap();

        let err = load_rebase_state(&path).unwrap_err().to_string();
        assert!(
            err.contains("Blank 'transaction_id'"),
            "expected blank-transaction_id error, got: {err}"
        );
    }

    /// Companion to the variant-rejection pin (heddle#198 r4 / Codex
    /// PR #218 P2): abort discards the buffered FF history when
    /// rewinding to `original_head`, so a non-rebase variant in the
    /// pending vec is harmless. Refusing to load would strand the
    /// operator the same way the missing-transaction_id case did
    /// before r3 — keep abort lenient.
    #[test]
    fn load_for_abort_tolerates_non_advance_pending_record() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let bad = OpRecord::MarkerCreate {
            name: "junk".to_string(),
            state: ChangeId::generate(),
        };
        let bytes = rmp_serde::to_vec(&bad).unwrap();
        let body = format!(
            "onto={onto}\noriginal_head={oh}\ntransaction_id=rebase-test\ncurrent_index=0\ncommits=\npending_advance={pa}\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
            pa = hex::encode(&bytes),
        );
        std::fs::write(&path, body).unwrap();

        let loaded = load_rebase_state_for_abort(&path)
            .expect("abort loader must tolerate a non-rebase pending_advance variant");
        assert!(
            loaded.pending_advances.is_empty(),
            "non-advance variants must be silently dropped on the abort path"
        );
    }

    /// Companion to the blank-transaction_id strict-rejection pin above
    /// (heddle#198 r4 / Codex PR #218 P2): abort never reaches the
    /// dedup helper, so a blank value is harmless on that path and
    /// refusing it would strand the operator the same way the missing-
    /// line case did before r3. Pin that abort still loads.
    #[test]
    fn load_for_abort_tolerates_blank_transaction_id() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let body = format!(
            "onto={onto}\noriginal_head={oh}\ntransaction_id=\ncurrent_index=0\ncommits=\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
        );
        std::fs::write(&path, body).unwrap();

        let loaded = load_rebase_state_for_abort(&path)
            .expect("abort loader must tolerate a blank transaction_id");
        assert!(loaded.transaction_id.is_empty());
    }

    /// Companion to the strict-rejection pin above (heddle#198 r4 /
    /// Codex PR #218 P1): the abort loader must tolerate a
    /// `current_index` / `pending_advances` mismatch — abort only needs
    /// `original_head` to rewind, and discards the buffered FF history
    /// anyway. Refusing to load would strand the operator with neither
    /// `--abort` nor `--continue` available, exactly the trap the
    /// per-loader contract (continue strict, abort tolerant) was set
    /// up to avoid.
    #[test]
    fn load_for_abort_tolerates_truncated_pending_advances() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let advance = ff_record();
        let bytes = rmp_serde::to_vec(&advance).unwrap();
        let body = format!(
            "onto={onto}\noriginal_head={oh}\ntransaction_id=rebase-test\ncurrent_index=3\ncommits=\npending_advance={pa}\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
            pa = hex::encode(&bytes),
        );
        std::fs::write(&path, body).unwrap();

        let loaded = load_rebase_state_for_abort(&path)
            .expect("abort loader must tolerate a count mismatch — the buffered FFs are discarded on rewind");
        // The advance still decodes; we just don't refuse the file for
        // having the wrong number of them.
        assert_eq!(loaded.current_index, 3);
        assert_eq!(loaded.pending_advances.len(), 1);
    }

    /// heddle#198 r4 (Codex PR #218 P1): the persisted invariant in
    /// REBASE_STATE is `pending_advances.len() == current_index` — each
    /// successful per-commit replay bumps both in lockstep (see
    /// `replay_commits_internal` Success arm and
    /// `resume_manual_resolution_if_present`). A crash mid-write to
    /// REBASE_STATE that preserves the `current_index=` line but
    /// truncates one or more trailing `pending_advance=` lines lands the
    /// file in an inconsistent state: the strict `--continue` loader
    /// would treat it as clean, re-enter `replay_commits_internal` with
    /// `current_index == commits_to_replay.len()`, skip the loop body
    /// entirely, and flush an incomplete batch — silently losing part
    /// of the rebase's undo history. Pin that strict load hard-fails.
    #[test]
    fn load_strict_rejects_truncated_pending_advances() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        // current_index=3 claims three advances completed, but only one
        // `pending_advance=` line is present. Encode a real FF record so
        // the line itself decodes cleanly — the inconsistency is purely
        // between the counter and the vec length, not a malformed entry.
        let advance = ff_record();
        let bytes = rmp_serde::to_vec(&advance).unwrap();
        let body = format!(
            "onto={onto}\noriginal_head={oh}\ntransaction_id=rebase-test\ncurrent_index=3\ncommits=\npending_advance={pa}\n",
            onto = ChangeId::generate().to_string_full(),
            oh = ChangeId::generate().to_string_full(),
            pa = hex::encode(&bytes),
        );
        std::fs::write(&path, body).unwrap();

        let err = load_rebase_state(&path).unwrap_err().to_string();
        assert!(
            err.contains("pending_advance") && err.contains("current_index"),
            "expected count-mismatch error naming both counters, got: {err}"
        );
    }

    /// A clean state file (no malformed lines) must load identically
    /// through both paths — leniency is invisible when there's nothing
    /// to forgive.
    #[test]
    fn load_for_abort_matches_strict_on_clean_state() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("REBASE_STATE");
        let original = sample_state(vec![ff_record(), ff_record()]);
        save_rebase_state(&path, &original).unwrap();

        let strict = load_rebase_state(&path).unwrap();
        let lenient = load_rebase_state_for_abort(&path).unwrap();
        assert_eq!(strict.original_head, lenient.original_head);
        assert_eq!(strict.onto, lenient.onto);
        assert_eq!(strict.transaction_id, lenient.transaction_id);
        assert_eq!(
            strict.pending_advances.len(),
            lenient.pending_advances.len()
        );
    }
}
