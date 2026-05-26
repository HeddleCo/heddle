// SPDX-License-Identifier: Apache-2.0
//! Translate git reflog entries into Heddle oplog `OpRecord`s.
//!
//! This is the "honest history" half of the importer. A git reflog is git's
//! own record of every ref movement — commits, resets, rebases, merges,
//! checkouts — and Heddle's oplog is the equivalent structure on our side.
//! Reconstructing one from the other lets `heddle undo` reach back past the
//! import boundary instead of treating the entire git past as one opaque
//! prelude.
//!
//! # Avoiding double-counting
//!
//! Every `git commit` on `main` writes *two* reflog lines: one to
//! `.git/logs/HEAD` and one to `.git/logs/refs/heads/main`. If we emitted an
//! op for each, every commit would show up twice in the oplog. To keep one
//! op per real event we split by source:
//!
//! | Source                     | Ops we emit                        |
//! |----------------------------|------------------------------------|
//! | `HEAD` reflog              | [`OpRecord::Goto`] (for `checkout:`) |
//! | `refs/heads/<name>` reflog | [`OpRecord::ThreadCreate/Update/Delete`] |
//! | `refs/tags/<name>` reflog  | [`OpRecord::MarkerCreate/Delete`]  |
//!
//! HEAD lines that aren't pure navigation (`commit:`, `reset:`, `merge`,
//! `pull:`, `rebase …`) are redundant with the per-branch line and dropped.
//! The thread-level op carries enough information to replay the state
//! change; the HEAD line would just duplicate it.
//!
//! # Timestamps
//!
//! The oplog backend stamps each entry with `Utc::now()` — there is no
//! public API to back-date an op. For imports that means the oplog's `id`
//! order is reflog order, but each entry's `timestamp` is "imported on",
//! not "performed on". The historical timestamp is preserved on the State
//! itself (via `created_at` = committer time), which is where replay cares
//! about it.
//!
//! # What we deliberately skip
//!
//! - Reflog entries whose target SHA isn't in the `ShaMap`. Shouldn't
//!   happen in practice — the importer seeds commits from `reflog_commit_shas`
//!   — but if a commit was pruned between walker and emitter we warn and
//!   drop it rather than aborting.
//! - No-op updates (`previous_sha == new_sha`). Git records these for some
//!   client tools; they don't correspond to any Heddle state change.
//! - Tag `Update` (moving a tag). Heddle markers are write-once and there's
//!   no `MarkerUpdate` variant. We emit `MarkerDelete` + `MarkerCreate` in
//!   that case to keep the log faithful.

use objects::object::{MarkerName, ThreadName};
use oplog::oplog::OpLogBackend;
use tracing::warn;

use crate::{git_walk::ReflogEntry, sha_map::ShaMap, IngestError};

/// Rolling tally returned by [`OplogEmitter::emit`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OplogEmitStats {
    pub gotos: usize,
    pub thread_creates: usize,
    pub thread_updates: usize,
    pub thread_deletes: usize,
    pub marker_creates: usize,
    pub marker_deletes: usize,
    /// Reflog lines we classified and chose not to emit (HEAD non-`checkout`
    /// lines, no-op updates, etc.). Counted so callers can sanity-check
    /// the filter isn't throwing the log away.
    pub skipped_noop: usize,
    /// Reflog lines whose target commit had no entry in the [`ShaMap`].
    /// Any non-zero count is a correctness signal.
    pub skipped_unmapped: usize,
}

/// Emits oplog records from a list of reflog entries.
///
/// Takes `&dyn OpLogBackend` so the same emitter drives the local
/// `OpLog` on disk and the server's Postgres-backed backend.
pub struct OplogEmitter<'a> {
    oplog: &'a dyn OpLogBackend,
    map: &'a ShaMap,
    scope: Option<String>,
}

impl<'a> OplogEmitter<'a> {
    pub fn new(oplog: &'a dyn OpLogBackend, map: &'a ShaMap) -> Self {
        Self {
            oplog,
            map,
            scope: None,
        }
    }

    /// Tag emitted ops with a scope (checkout/lane id) so undo filters pick
    /// them up the same way runtime ops are picked up.
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }

    /// Translate every reflog entry. Entries should be passed in the order
    /// they came out of [`GitSource::collect_reflog`](crate::GitSource) —
    /// within each ref that's oldest → newest, which preserves replay
    /// semantics.
    pub fn emit(&self, entries: &[ReflogEntry]) -> crate::Result<OplogEmitStats> {
        let mut stats = OplogEmitStats::default();
        let scope = self.scope.as_deref();

        for entry in entries {
            match classify(&entry.ref_name) {
                RefKind::Head => self.emit_head(entry, scope, &mut stats)?,
                RefKind::Branch(name) => self.emit_branch(entry, &name, scope, &mut stats)?,
                RefKind::Tag(name) => self.emit_tag(entry, &name, &mut stats)?,
                RefKind::Other => {
                    stats.skipped_noop += 1;
                }
            }
        }
        Ok(stats)
    }

    fn emit_head(
        &self,
        entry: &ReflogEntry,
        scope: Option<&str>,
        stats: &mut OplogEmitStats,
    ) -> crate::Result<()> {
        // Only `checkout:` is a pure navigation event — everything else
        // (commit, reset, merge, pull, rebase, amend) is mirrored in the
        // per-branch reflog, which already yields the richer thread op.
        if !entry.message.starts_with("checkout:") {
            stats.skipped_noop += 1;
            return Ok(());
        }
        let Some(new_sha) = &entry.new_sha else {
            stats.skipped_noop += 1;
            return Ok(());
        };
        let Some(cid) = self.map.get_commit(new_sha) else {
            warn!(
                ref_name = %entry.ref_name,
                sha = %new_sha,
                "dropping HEAD checkout — target commit not in sha map",
            );
            stats.skipped_unmapped += 1;
            return Ok(());
        };
        let prev_cid = entry
            .previous_sha
            .as_deref()
            .and_then(|s| self.map.get_commit(s));
        self.oplog
            .record_goto(&cid, prev_cid.as_ref(), scope)
            .map_err(IngestError::from)?;
        stats.gotos += 1;
        Ok(())
    }

    fn emit_branch(
        &self,
        entry: &ReflogEntry,
        short_name: &str,
        scope: Option<&str>,
        stats: &mut OplogEmitStats,
    ) -> crate::Result<()> {
        match (&entry.previous_sha, &entry.new_sha) {
            (None, None) => {
                stats.skipped_noop += 1;
            }
            (None, Some(new_sha)) => {
                let Some(cid) = self.map.get_commit(new_sha) else {
                    stats.skipped_unmapped += 1;
                    return Ok(());
                };
                // Git-history ingest does not write a ThreadManager
                // record — those exist for native heddle threads. Pass
                // `None` for the snapshot; the recorded
                // `ThreadCreateV2` carries no record body to restore
                // on redo. heddle#23 r2.
                let thread_name = ThreadName::from(short_name);
                self.oplog
                    .record_thread_create(&thread_name, &cid, None, scope)
                    .map_err(IngestError::from)?;
                stats.thread_creates += 1;
            }
            (Some(prev_sha), None) => {
                let Some(cid) = self.map.get_commit(prev_sha) else {
                    stats.skipped_unmapped += 1;
                    return Ok(());
                };
                let thread_name = ThreadName::from(short_name);
                self.oplog
                    .record_thread_delete(&thread_name, &cid, scope)
                    .map_err(IngestError::from)?;
                stats.thread_deletes += 1;
            }
            (Some(prev), Some(new)) if prev == new => {
                // Git sometimes logs a self-transition (e.g. `git
                // update-ref` to the same sha). Nothing to replay.
                stats.skipped_noop += 1;
            }
            (Some(prev), Some(new)) => {
                let (Some(old_cid), Some(new_cid)) =
                    (self.map.get_commit(prev), self.map.get_commit(new))
                else {
                    stats.skipped_unmapped += 1;
                    return Ok(());
                };
                self.oplog
                    .record_batch_scoped(
                        vec![oplog::oplog::OpRecord::ThreadUpdate {
                            name: short_name.to_string(),
                            old_state: old_cid,
                            new_state: new_cid,
                        }],
                        scope,
                    )
                    .map_err(IngestError::from)?;
                stats.thread_updates += 1;
            }
        }
        Ok(())
    }

    fn emit_tag(
        &self,
        entry: &ReflogEntry,
        short_name: &str,
        stats: &mut OplogEmitStats,
    ) -> crate::Result<()> {
        let marker_name = MarkerName::from(short_name);
        match (&entry.previous_sha, &entry.new_sha) {
            (None, Some(new)) => {
                let Some(cid) = self.map.get_commit(new) else {
                    stats.skipped_unmapped += 1;
                    return Ok(());
                };
                self.oplog
                    .record_marker_create(&marker_name, &cid)
                    .map_err(IngestError::from)?;
                stats.marker_creates += 1;
            }
            (Some(prev), None) => {
                let Some(cid) = self.map.get_commit(prev) else {
                    stats.skipped_unmapped += 1;
                    return Ok(());
                };
                self.oplog
                    .record_marker_delete(&marker_name, &cid)
                    .map_err(IngestError::from)?;
                stats.marker_deletes += 1;
            }
            (Some(prev), Some(new)) if prev == new => {
                stats.skipped_noop += 1;
            }
            (Some(prev), Some(new)) => {
                // Tag moved. Markers are write-once; emit delete+create so
                // replay lands in the same state as git. We route both
                // through `record_batch_scoped` so a single undo reverses
                // the whole move — matches user intent better than two
                // independently undoable ops.
                let (Some(old_cid), Some(new_cid)) =
                    (self.map.get_commit(prev), self.map.get_commit(new))
                else {
                    stats.skipped_unmapped += 1;
                    return Ok(());
                };
                self.oplog
                    .record_batch_scoped(
                        vec![
                            oplog::oplog::OpRecord::MarkerDelete {
                                name: short_name.to_string(),
                                state: old_cid,
                            },
                            oplog::oplog::OpRecord::MarkerCreate {
                                name: short_name.to_string(),
                                state: new_cid,
                            },
                        ],
                        self.scope.as_deref(),
                    )
                    .map_err(IngestError::from)?;
                stats.marker_deletes += 1;
                stats.marker_creates += 1;
            }
            (None, None) => {
                stats.skipped_noop += 1;
            }
        }
        Ok(())
    }
}

enum RefKind {
    Head,
    Branch(String),
    Tag(String),
    Other,
}

/// Classify a reflog ref name into the three buckets the emitter cares
/// about. Preserves the slashed-name suffix for branches (`feature/x`
/// stays `feature/x`).
fn classify(ref_name: &str) -> RefKind {
    if ref_name == "HEAD" {
        return RefKind::Head;
    }
    if let Some(rest) = ref_name.strip_prefix("refs/heads/") {
        return RefKind::Branch(rest.to_string());
    }
    if let Some(rest) = ref_name.strip_prefix("refs/tags/") {
        return RefKind::Tag(rest.to_string());
    }
    RefKind::Other
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use objects::object::ChangeId;
    use oplog::oplog::{OpLog, OpRecord};
    use tempfile::TempDir;

    use super::*;
    use crate::git_walk::GitSignature;

    fn fresh_oplog() -> (TempDir, OpLog) {
        let tmp = TempDir::new().unwrap();
        let log = OpLog::new_unattributed(tmp.path());
        log.init().unwrap();
        (tmp, log)
    }

    fn sig() -> GitSignature {
        GitSignature {
            name: "Test".into(),
            email: "t@example.com".into(),
            time: Utc::now(),
        }
    }

    fn mk_entry(ref_name: &str, prev: Option<&str>, new: Option<&str>, msg: &str) -> ReflogEntry {
        ReflogEntry {
            ref_name: ref_name.to_string(),
            previous_sha: prev.map(str::to_string),
            new_sha: new.map(str::to_string),
            signature: sig(),
            message: msg.to_string(),
        }
    }

    /// Convenience: read back every op record from disk in order.
    fn all_ops(log: &OpLog) -> Vec<OpRecord> {
        // Pull the whole history — tests seed at most a handful of entries.
        log.recent(1024)
            .unwrap()
            .into_iter()
            .rev() // `recent` returns newest-first; we want chronological.
            .map(|e| e.operation)
            .collect()
    }

    #[test]
    fn branch_create_update_delete() {
        let (_tmp, log) = fresh_oplog();
        let mut map = ShaMap::new();
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let cid_a = ChangeId::generate();
        let cid_b = ChangeId::generate();
        map.insert_commit(&sha_a, cid_a).unwrap();
        map.insert_commit(&sha_b, cid_b).unwrap();

        let entries = vec![
            mk_entry("refs/heads/main", None, Some(&sha_a), "branch: Created"),
            mk_entry("refs/heads/main", Some(&sha_a), Some(&sha_b), "commit: x"),
            mk_entry("refs/heads/main", Some(&sha_b), None, "branch: deleted"),
        ];

        let stats = OplogEmitter::new(&log, &map).emit(&entries).unwrap();

        assert_eq!(stats.thread_creates, 1);
        assert_eq!(stats.thread_updates, 1);
        assert_eq!(stats.thread_deletes, 1);
        assert_eq!(stats.skipped_unmapped, 0);

        let ops = all_ops(&log);
        assert_eq!(ops.len(), 3);
        assert!(
            matches!(ops[0], OpRecord::ThreadCreateV2 { ref name, state, manager_snapshot: None } if name == "main" && state == cid_a)
        );
        assert!(matches!(
            &ops[1],
            OpRecord::ThreadUpdate { name, old_state, new_state }
                if name == "main" && *old_state == cid_a && *new_state == cid_b
        ));
        assert!(
            matches!(&ops[2], OpRecord::ThreadDelete { name, state } if name == "main" && *state == cid_b)
        );
    }

    #[test]
    fn slashed_branch_name_is_preserved() {
        let (_tmp, log) = fresh_oplog();
        let mut map = ShaMap::new();
        let sha = "c".repeat(40);
        let cid = ChangeId::generate();
        map.insert_commit(&sha, cid).unwrap();

        let entries = vec![mk_entry(
            "refs/heads/feature/ingest",
            None,
            Some(&sha),
            "branch: Created",
        )];
        OplogEmitter::new(&log, &map).emit(&entries).unwrap();

        let ops = all_ops(&log);
        assert!(matches!(
            &ops[0],
            OpRecord::ThreadCreateV2 { name, .. } if name == "feature/ingest"
        ));
    }

    #[test]
    fn head_checkout_becomes_goto_everything_else_is_skipped() {
        let (_tmp, log) = fresh_oplog();
        let mut map = ShaMap::new();
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let cid_a = ChangeId::generate();
        let cid_b = ChangeId::generate();
        map.insert_commit(&sha_a, cid_a).unwrap();
        map.insert_commit(&sha_b, cid_b).unwrap();

        // A realistic HEAD reflog: initial commit, a second commit, then a
        // checkout. Only the checkout should turn into an oplog entry —
        // the commits are represented on the branch reflog we're not
        // passing here.
        let entries = vec![
            mk_entry("HEAD", None, Some(&sha_a), "commit (initial): first"),
            mk_entry("HEAD", Some(&sha_a), Some(&sha_b), "commit: second"),
            mk_entry(
                "HEAD",
                Some(&sha_b),
                Some(&sha_a),
                "checkout: moving from main to topic",
            ),
        ];
        let stats = OplogEmitter::new(&log, &map).emit(&entries).unwrap();

        assert_eq!(stats.gotos, 1);
        assert_eq!(stats.skipped_noop, 2);

        let ops = all_ops(&log);
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], OpRecord::Goto { target, prev_head }
            if *target == cid_a && *prev_head == Some(cid_b)));
    }

    #[test]
    fn noop_update_is_dropped() {
        let (_tmp, log) = fresh_oplog();
        let mut map = ShaMap::new();
        let sha = "d".repeat(40);
        map.insert_commit(&sha, ChangeId::generate()).unwrap();

        let entries = vec![mk_entry(
            "refs/heads/main",
            Some(&sha),
            Some(&sha),
            "update-ref: no-op",
        )];
        let stats = OplogEmitter::new(&log, &map).emit(&entries).unwrap();

        assert_eq!(stats.skipped_noop, 1);
        assert_eq!(stats.thread_updates, 0);
        assert!(all_ops(&log).is_empty());
    }

    #[test]
    fn unmapped_target_is_reported_not_fatal() {
        let (_tmp, log) = fresh_oplog();
        let map = ShaMap::new(); // empty — every SHA is unmapped
        let entries = vec![
            mk_entry(
                "refs/heads/main",
                None,
                Some(&"a".repeat(40)),
                "branch: Created",
            ),
            mk_entry("HEAD", None, Some(&"b".repeat(40)), "checkout: x"),
        ];
        let stats = OplogEmitter::new(&log, &map).emit(&entries).unwrap();
        assert_eq!(stats.skipped_unmapped, 2);
        assert_eq!(stats.thread_creates, 0);
        assert_eq!(stats.gotos, 0);
        assert!(all_ops(&log).is_empty());
    }

    #[test]
    fn tag_move_becomes_delete_then_create() {
        let (_tmp, log) = fresh_oplog();
        let mut map = ShaMap::new();
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let cid_a = ChangeId::generate();
        let cid_b = ChangeId::generate();
        map.insert_commit(&sha_a, cid_a).unwrap();
        map.insert_commit(&sha_b, cid_b).unwrap();

        let entries = vec![mk_entry(
            "refs/tags/v0.1",
            Some(&sha_a),
            Some(&sha_b),
            "tag: moved",
        )];
        let stats = OplogEmitter::new(&log, &map).emit(&entries).unwrap();
        assert_eq!(stats.marker_deletes, 1);
        assert_eq!(stats.marker_creates, 1);

        let ops = all_ops(&log);
        assert_eq!(ops.len(), 2);
        assert!(matches!(&ops[0], OpRecord::MarkerDelete { name, state }
            if name == "v0.1" && *state == cid_a));
        assert!(matches!(&ops[1], OpRecord::MarkerCreate { name, state }
            if name == "v0.1" && *state == cid_b));
    }

    #[test]
    fn scope_is_propagated() {
        let (_tmp, log) = fresh_oplog();
        let mut map = ShaMap::new();
        let sha = "e".repeat(40);
        map.insert_commit(&sha, ChangeId::generate()).unwrap();

        let entries = vec![mk_entry(
            "refs/heads/main",
            None,
            Some(&sha),
            "branch: Created",
        )];
        OplogEmitter::new(&log, &map)
            .with_scope("import")
            .emit(&entries)
            .unwrap();

        let last = log.last().unwrap().unwrap();
        assert_eq!(last.scope.as_deref(), Some("import"));
    }
}
