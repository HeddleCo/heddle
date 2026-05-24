// SPDX-License-Identifier: Apache-2.0
//! Rebase state persistence and commit-graph traversal.

use std::{fs, io::Write};

use anyhow::{Result, anyhow};
use objects::object::ChangeId;
use oplog::OpRecord;
use repo::Repository;

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
        let bytes = rmp_serde::to_vec(advance)
            .map_err(|e| anyhow!("encode pending_advance: {}", e))?;
        content.push_str(&format!("pending_advance={}\n", hex::encode(&bytes)));
    }

    let mut file = fs::File::create(path)?;
    file.write_all(content.as_bytes())?;

    Ok(())
}

pub(crate) fn load_rebase_state(path: &std::path::Path) -> Result<RebaseState> {
    let content = fs::read_to_string(path)?;

    let mut onto = None;
    let mut original_head = None;
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
            let bytes =
                hex::decode(value).map_err(|e| anyhow!("decode pending_advance: {}", e))?;
            let advance: OpRecord = rmp_serde::from_slice(&bytes)
                .map_err(|e| anyhow!("decode pending_advance OpRecord: {}", e))?;
            pending_advances.push(advance);
        }
    }

    Ok(RebaseState {
        onto: onto.ok_or_else(|| anyhow!("Missing 'onto' in rebase state"))?,
        original_head: original_head
            .ok_or_else(|| anyhow!("Missing 'original_head' in rebase state"))?,
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
        RebaseState {
            onto: ChangeId::generate(),
            commits_to_replay: vec![ChangeId::generate(), ChangeId::generate()],
            current_index: 1,
            original_head: ChangeId::generate(),
            pending_manual_resolution: Some(ChangeId::generate()),
            pre_conflict_head: Some(ChangeId::generate()),
            pending_advances: pending,
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
        assert_eq!(loaded.current_index, 1);
        assert_eq!(loaded.commits_to_replay, original.commits_to_replay);
        assert_eq!(loaded.pending_manual_resolution, original.pending_manual_resolution);
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
            "onto={onto}\noriginal_head={oh}\ncurrent_index=0\ncommits=\npending_advance=not-hex!!\n",
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
            "onto={onto}\noriginal_head={oh}\ncurrent_index=0\ncommits=\npending_advance=deadbeef\n",
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
            "onto={onto}\noriginal_head={oh}\ncurrent_index=not-a-number\ncommits=\n",
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
}