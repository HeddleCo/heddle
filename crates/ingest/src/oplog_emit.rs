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
use oplog::oplog::{OpLogBackend, OpRecord};
use tracing::warn;

use crate::{IngestError, git_walk::ReflogEntry, sha_map::ShaMap};

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
/// Generic over the [`OpLogBackend`] so the same emitter drives the local
/// `OpLog` on disk and the server's Postgres-backed backend. Only the
/// synchronous `record_*` methods are used here, so `emit` stays sync even
/// though the trait now has `async` read methods — the type parameter is
/// required because the trait is no longer `&dyn`-dispatchable.
pub struct OplogEmitter<'a, O: OpLogBackend> {
    oplog: &'a O,
    map: &'a ShaMap,
    scope: Option<String>,
}

impl<'a, O: OpLogBackend> OplogEmitter<'a, O> {
    pub fn new(oplog: &'a O, map: &'a ShaMap) -> Self {
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

        // Accumulate one `(records, scope)` group per real event across ALL
        // reflog entries, then append the whole sequence in a single call.
        //
        // The old code called a per-entry `record_*` for every event, and
        // each call rewrote the entire growing oplog (copy + re-encode +
        // fsync + rename — `packed_oplog::append_entries`, TODO #423), so N
        // events cost O(N²). `record_batches_scoped` performs one full-log
        // rewrite for the whole batch: O(N). Crucially it is NOT a single
        // mega-batch — each group keeps its own `batch_id` and scope, so the
        // resulting oplog is byte-for-byte equivalent (ids, batches, scopes,
        // undo granularity) to what the per-append path produced. Only the
        // number of disk writes changes.
        let mut groups: Vec<(Vec<OpRecord>, Option<&str>)> = Vec::new();

        for entry in entries {
            match classify(&entry.ref_name) {
                RefKind::Head => self.emit_head(entry, scope, &mut groups, &mut stats),
                RefKind::Branch(name) => {
                    self.emit_branch(entry, &name, scope, &mut groups, &mut stats)
                }
                RefKind::Tag(name) => self.emit_tag(entry, &name, scope, &mut groups, &mut stats),
                RefKind::Other => {
                    stats.skipped_noop += 1;
                }
            }
        }

        // Single append for the whole import. `record_batches_scoped` is a
        // no-op when `groups` is empty, so an emit that classified everything
        // away (HEAD-only non-checkout, all no-ops, all unmapped) writes
        // nothing — no empty-batch corruption.
        if !groups.is_empty() {
            self.oplog
                .record_batches_scoped(groups)
                .map_err(IngestError::from)?;
        }

        Ok(stats)
    }

    fn emit_head<'g>(
        &self,
        entry: &ReflogEntry,
        scope: Option<&'g str>,
        groups: &mut Vec<(Vec<OpRecord>, Option<&'g str>)>,
        stats: &mut OplogEmitStats,
    ) {
        // Only `checkout:` is a pure navigation event — everything else
        // (commit, reset, merge, pull, rebase, amend) is mirrored in the
        // per-branch reflog, which already yields the richer thread op.
        if !entry.message.starts_with("checkout:") {
            stats.skipped_noop += 1;
            return;
        }
        let Some(new_sha) = &entry.new_sha else {
            stats.skipped_noop += 1;
            return;
        };
        let Some(cid) = self.map.get_commit(new_sha) else {
            warn!(
                ref_name = %entry.ref_name,
                sha = %new_sha,
                "dropping HEAD checkout — target commit not in sha map",
            );
            stats.skipped_unmapped += 1;
            return;
        };
        let prev_cid = entry
            .previous_sha
            .as_deref()
            .and_then(|s| self.map.get_commit(s));
        // Mirrors `OpLogRecorder::record_goto`: the `head` field is the goto
        // target. Pushed as its own group so it stays an independent undo
        // unit (one `record_goto` == one batch).
        groups.push((
            vec![OpRecord::Goto {
                target: cid,
                prev_head: prev_cid,
                head: cid,
            }],
            scope,
        ));
        stats.gotos += 1;
    }

    fn emit_branch<'g>(
        &self,
        entry: &ReflogEntry,
        short_name: &str,
        scope: Option<&'g str>,
        groups: &mut Vec<(Vec<OpRecord>, Option<&'g str>)>,
        stats: &mut OplogEmitStats,
    ) {
        match (&entry.previous_sha, &entry.new_sha) {
            (None, None) => {
                stats.skipped_noop += 1;
            }
            (None, Some(new_sha)) => {
                let Some(cid) = self.map.get_commit(new_sha) else {
                    stats.skipped_unmapped += 1;
                    return;
                };
                // Git-history ingest does not write a ThreadManager
                // record — those exist for native heddle threads. Pass
                // `None` for the snapshot; the recorded
                // `ThreadCreate` carries no record body to restore
                // on redo. heddle#23 r2. Mirrors `record_thread_create`.
                groups.push((
                    vec![OpRecord::ThreadCreate {
                        name: ThreadName::from(short_name).to_string(),
                        state: cid,
                        manager_snapshot: None,
                    }],
                    scope,
                ));
                stats.thread_creates += 1;
            }
            (Some(prev_sha), None) => {
                let Some(cid) = self.map.get_commit(prev_sha) else {
                    stats.skipped_unmapped += 1;
                    return;
                };
                // Mirrors `record_thread_delete`.
                groups.push((
                    vec![OpRecord::ThreadDelete {
                        name: ThreadName::from(short_name).to_string(),
                        state: cid,
                    }],
                    scope,
                ));
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
                    return;
                };
                groups.push((
                    vec![OpRecord::ThreadUpdate {
                        name: short_name.to_string(),
                        old_state: old_cid,
                        new_state: new_cid,
                        manager_snapshots: None,
                    }],
                    scope,
                ));
                stats.thread_updates += 1;
            }
        }
    }

    fn emit_tag<'g>(
        &self,
        entry: &ReflogEntry,
        short_name: &str,
        scope: Option<&'g str>,
        groups: &mut Vec<(Vec<OpRecord>, Option<&'g str>)>,
        stats: &mut OplogEmitStats,
    ) {
        match (&entry.previous_sha, &entry.new_sha) {
            (None, Some(new)) => {
                let Some(cid) = self.map.get_commit(new) else {
                    stats.skipped_unmapped += 1;
                    return;
                };
                // Mirrors `record_marker_create`, which records with scope
                // `None` (it goes through `record_batch`, not the scoped
                // variant). Preserved here to keep replay equivalence.
                groups.push((
                    vec![OpRecord::MarkerCreate {
                        name: MarkerName::from(short_name).to_string(),
                        state: cid,
                    }],
                    None,
                ));
                stats.marker_creates += 1;
            }
            (Some(prev), None) => {
                let Some(cid) = self.map.get_commit(prev) else {
                    stats.skipped_unmapped += 1;
                    return;
                };
                // Mirrors `record_marker_delete` — scope `None`.
                groups.push((
                    vec![OpRecord::MarkerDelete {
                        name: MarkerName::from(short_name).to_string(),
                        state: cid,
                    }],
                    None,
                ));
                stats.marker_deletes += 1;
            }
            (Some(prev), Some(new)) if prev == new => {
                stats.skipped_noop += 1;
            }
            (Some(prev), Some(new)) => {
                // Tag moved. Markers are write-once; emit delete+create so
                // replay lands in the same state as git. Both ops go in ONE
                // group so a single undo reverses the whole move — matches
                // user intent better than two independently undoable ops.
                // This arm uses the emitter scope (it always did, via the
                // earlier inline `record_batch_scoped(_, self.scope)`).
                let (Some(old_cid), Some(new_cid)) =
                    (self.map.get_commit(prev), self.map.get_commit(new))
                else {
                    stats.skipped_unmapped += 1;
                    return;
                };
                groups.push((
                    vec![
                        OpRecord::MarkerDelete {
                            name: short_name.to_string(),
                            state: old_cid,
                        },
                        OpRecord::MarkerCreate {
                            name: short_name.to_string(),
                            state: new_cid,
                        },
                    ],
                    scope,
                ));
                stats.marker_deletes += 1;
                stats.marker_creates += 1;
            }
            (None, None) => {
                stats.skipped_noop += 1;
            }
        }
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
    use oplog::oplog::{OpLog, OpLogRecorder};
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
            tz_offset: 0,
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
            matches!(ops[0], OpRecord::ThreadCreate { ref name, state, manager_snapshot: None } if name == "main" && state == cid_a)
        );
        assert!(matches!(
            &ops[1],
            OpRecord::ThreadUpdate {
                name,
                old_state,
                new_state,
                ..
            }
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
            OpRecord::ThreadCreate { name, .. } if name == "feature/ingest"
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
        assert!(matches!(&ops[0], OpRecord::Goto { target, prev_head, .. }
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

    /// Read every entry back in chronological order, projecting the fields
    /// that define replay: operation, scope, and per-batch grouping
    /// (`batch_id` relative to the first id, plus `batch_index`).
    fn entry_fingerprints(log: &OpLog) -> Vec<(String, Option<String>, u64, u32)> {
        let mut entries = log.recent(1024).unwrap();
        entries.reverse(); // newest-first -> chronological
        let base = entries.first().map(|e| e.batch_id).unwrap_or(0);
        entries
            .into_iter()
            .map(|e| {
                (
                    // OpRecord doesn't derive PartialEq; its Debug rendering is
                    // a faithful, deterministic projection of every field.
                    format!("{:?}", e.operation),
                    e.scope.clone(),
                    e.batch_id - base, // batch_id normalized to the first batch
                    e.batch_index,
                )
            })
            .collect()
    }

    /// The load-bearing correctness gate: the batched single-append emit must
    /// produce a byte-for-byte equivalent oplog to the old per-event
    /// `record_*` path — same ops, same order, same scopes, and same
    /// *per-event* batch boundaries (so `heddle undo` still steps one ref
    /// event at a time, not one mega-undo of the whole import).
    ///
    /// We reproduce the OLD behavior by driving the public `record_*`
    /// recorder helpers in the exact sequence/scopes the pre-refactor emit
    /// used, into a reference oplog, then assert the fingerprints match the
    /// batched emit's.
    #[test]
    fn batched_emit_matches_per_append_replay() {
        let mut map = ShaMap::new();
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let sha_c = "c".repeat(40);
        let cid_a = ChangeId::generate();
        let cid_b = ChangeId::generate();
        let cid_c = ChangeId::generate();
        map.insert_commit(&sha_a, cid_a).unwrap();
        map.insert_commit(&sha_b, cid_b).unwrap();
        map.insert_commit(&sha_c, cid_c).unwrap();

        // A representative multi-ref reflog touching every emit arm:
        // branch create/update/delete, tag create, tag move, HEAD checkout.
        let entries = vec![
            mk_entry("refs/heads/main", None, Some(&sha_a), "branch: Created"),
            mk_entry("refs/heads/main", Some(&sha_a), Some(&sha_b), "commit: x"),
            mk_entry("refs/tags/v1", None, Some(&sha_a), "tag: created"),
            mk_entry("refs/tags/v1", Some(&sha_a), Some(&sha_b), "tag: moved"),
            mk_entry(
                "HEAD",
                Some(&sha_b),
                Some(&sha_a),
                "checkout: moving from main to topic",
            ),
            mk_entry("refs/heads/main", Some(&sha_b), None, "branch: deleted"),
        ];

        // ---- batched path (the new code) ----
        let (_tmp_batched, batched) = fresh_oplog();
        let stats = OplogEmitter::new(&batched, &map)
            .with_scope("ingest")
            .emit(&entries)
            .unwrap();

        // ---- reference path (old per-append behavior, replayed via the
        //      public recorder helpers in identical order/scope) ----
        let (_tmp_ref, reference) = fresh_oplog();
        let scope = Some("ingest");
        // branch: Created
        reference
            .record_thread_create(&ThreadName::from("main"), &cid_a, None, scope)
            .unwrap();
        // commit: x  (branch update)
        reference
            .record_thread_update(&ThreadName::from("main"), &cid_a, &cid_b, None, scope)
            .unwrap();
        // tag: created  (marker create — scope None, via record_marker_create)
        reference
            .record_marker_create(&MarkerName::from("v1"), &cid_a)
            .unwrap();
        // tag: moved  (delete+create in ONE batch, emitter scope)
        reference
            .record_batch_scoped(
                vec![
                    OpRecord::MarkerDelete {
                        name: "v1".to_string(),
                        state: cid_a,
                    },
                    OpRecord::MarkerCreate {
                        name: "v1".to_string(),
                        state: cid_b,
                    },
                ],
                scope,
            )
            .unwrap();
        // HEAD checkout -> goto
        reference
            .record_goto(&cid_a, Some(&cid_b), scope)
            .unwrap();
        // branch: deleted
        reference
            .record_thread_delete(&ThreadName::from("main"), &cid_b, scope)
            .unwrap();

        // Same ops, same scopes, same per-event batch boundaries, same ids.
        assert_eq!(
            entry_fingerprints(&batched),
            entry_fingerprints(&reference),
            "batched emit must replay identically to the per-append path"
        );

        // And the stat tally is unchanged by the batching.
        assert_eq!(stats.thread_creates, 1);
        assert_eq!(stats.thread_updates, 1);
        assert_eq!(stats.thread_deletes, 1);
        assert_eq!(stats.marker_creates, 2); // create + the create half of the move
        assert_eq!(stats.marker_deletes, 1);
        assert_eq!(stats.gotos, 1);
    }

    /// Each real event stays its OWN batch (distinct `batch_id`) — the
    /// batching collapses disk writes, not undo granularity.
    #[test]
    fn each_event_is_an_independent_batch() {
        let (_tmp, log) = fresh_oplog();
        let mut map = ShaMap::new();
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        map.insert_commit(&sha_a, ChangeId::generate()).unwrap();
        map.insert_commit(&sha_b, ChangeId::generate()).unwrap();

        let entries = vec![
            mk_entry("refs/heads/main", None, Some(&sha_a), "branch: Created"),
            mk_entry("refs/heads/main", Some(&sha_a), Some(&sha_b), "commit: x"),
            mk_entry("refs/heads/main", Some(&sha_b), None, "branch: deleted"),
        ];
        OplogEmitter::new(&log, &map).emit(&entries).unwrap();

        let mut all = log.recent(1024).unwrap();
        all.reverse();
        let distinct: std::collections::BTreeSet<u64> =
            all.iter().map(|e| e.batch_id).collect();
        assert_eq!(all.len(), 3);
        assert_eq!(distinct.len(), 3, "three events => three distinct batches");
        // ids are globally sequential across the batches.
        let ids: Vec<u64> = all.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![ids[0], ids[0] + 1, ids[0] + 2]);
    }

    /// Scaling proof (run with `cargo test -p heddle-ingest --release
    /// -- --ignored --nocapture oplog_emit_scaling`). Times
    /// `OplogEmitter::emit` — the phase the profiling spike pinned as the
    /// O(N²) term — at N=100/200/400/800. Pre-fix it grows ~4× per doubling
    /// (one full-log rewrite per reflog entry); post-fix it is ~linear (one
    /// rewrite for the whole import).
    #[test]
    #[ignore = "timing benchmark; run explicitly with --ignored --nocapture in release"]
    fn oplog_emit_scaling_curve() {
        use std::time::Instant;
        for &n in &[100usize, 200, 400, 800] {
            let (_tmp, log) = fresh_oplog();
            let mut map = ShaMap::new();
            let mut entries = Vec::with_capacity(n);
            for i in 0..n {
                let sha = format!("{:040x}", i + 1);
                map.insert_commit(&sha, ChangeId::generate()).unwrap();
                entries.push(mk_entry(
                    &format!("refs/heads/b{i}"),
                    None,
                    Some(&sha),
                    "branch: Created",
                ));
            }
            let start = Instant::now();
            let stats = OplogEmitter::new(&log, &map)
                .with_scope("ingest")
                .emit(&entries)
                .unwrap();
            let dt = start.elapsed();
            assert_eq!(stats.thread_creates, n);
            println!("oplog_emit N={n:<5} emit={:.3}s", dt.as_secs_f64());
        }
    }

    /// An emit that classifies everything away appends nothing — no
    /// empty-batch corruption.
    #[test]
    fn empty_emit_writes_nothing() {
        let (_tmp, log) = fresh_oplog();
        let map = ShaMap::new();
        // HEAD commit (not a checkout) -> skipped; nothing else.
        let entries = vec![mk_entry(
            "HEAD",
            None,
            Some(&"a".repeat(40)),
            "commit (initial): first",
        )];
        let before = log.last().unwrap();
        let stats = OplogEmitter::new(&log, &map).emit(&entries).unwrap();
        assert_eq!(stats.skipped_noop, 1);
        assert!(all_ops(&log).is_empty());
        assert!(before.is_none() && log.last().unwrap().is_none());
    }
}
